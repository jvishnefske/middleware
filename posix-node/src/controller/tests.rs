//! The controller with the real interpreter, on the in-memory flash and a
//! clock the test moves: start and autostart, served keys, publications,
//! ticks and timers, traps, stop and upload.

use std::cell::Cell;
use std::rc::Rc;

use super::*;
use crate::flash::SectorFlash;
use crate::flash::mem::{Disk, MemSectors};
use crate::layout::SECTORS;
use crate::service::tests::{NODE, facts};
use middleware::loader::{Caps, DeviceInfo};
use mwctl::plan::upload_plan;

/// A clock the test moves. The controller and the payload's device each
/// hold a handle (the device must be `'static`), so it is shared; tests
/// only.
#[derive(Debug, Clone, Default)]
struct Manual(Rc<Cell<u64>>);

impl Manual {
    fn advance_ms(&self, ms: u64) {
        self.0.set(self.0.get() + ms * 1000);
    }
}

impl Clock for Manual {
    fn now_us(&self) -> u64 {
        self.0.get()
    }
}

type Ctl<'d> = Controller<SectorFlash<MemSectors<'d>>, Manual>;

fn demo() -> Vec<u8> {
    wat::parse_str(include_str!("../../tests/fixtures/pi_demo.wat")).expect("WAT")
}

fn open(disk: &mut Disk, clock: Manual) -> (Ctl<'_>, Boot) {
    let flash = SectorFlash::open(MemSectors::new(disk), SECTORS).expect("flash");
    Controller::open(flash, facts(), crate::limits(1024 * 1024), clock)
}

fn req(c: &mut Ctl<'_>, key: &str, body: &[u8]) -> Result<Vec<u8>, HandlerError> {
    c.request(Key::new(key).expect("key"), body, &RequestMeta::NONE)
}

fn install(c: &mut Ctl<'_>, image: &[u8]) {
    for step in upload_plan(image, None, None).expect("plan") {
        let r = c.request(step.key, &step.body, &RequestMeta::NONE);
        assert!(r.is_ok(), "{}: {r:?}", step.key.as_str());
    }
}

/// The publications on `key` in `outs`.
fn on<'o>(outs: &'o [Out], key: &str) -> Vec<&'o [u8]> {
    outs.iter().filter(|o| o.key == key).map(|o| o.payload.as_slice()).collect()
}

fn logs(outs: &[Out]) -> Vec<String> {
    on(outs, "sys/log").iter().map(|b| String::from_utf8_lossy(b).into_owned()).collect()
}

#[test]
fn pn7_a_started_payload_serves_publishes_ticks_and_fires_timers() {
    let mut disk = Disk::default();
    let clock = Manual::default();
    let (mut c, boot) = open(&mut disk, clock.clone());
    c.boot(&boot);
    let outs = c.drain();
    assert_eq!(on(&outs, "sys/payload/status"), [&[PayloadStatus::Empty as u8][..]]);
    assert!(logs(&outs)[0].starts_with("mw-posix-node "), "{:?}", logs(&outs));
    install(&mut c, &demo());
    assert_eq!(req(&mut c, "sys/payload/start", &[]), Ok(Vec::new()));
    assert!(c.running());
    let outs = c.drain();
    let lines = logs(&outs);
    assert!(lines.iter().any(|l| l == "pi: gpio unsupported"), "{lines:?}");
    assert!(lines.iter().any(|l| l == "pi: started"), "{lines:?}");
    assert_eq!(on(&outs, "sys/payload/status").last(), Some(&&[PayloadStatus::Running as u8][..]));
    // sys/info says requests on served keys reach the payload.
    let info = req(&mut c, "sys/info", &[]).expect("info");
    assert_eq!(DeviceInfo::decode(&info).expect("decode").caps, Caps::SERVE);
    // A served key answers through on_request, with the node id.
    let reply = req(&mut c, "app/pi/echo", b"ping").expect("echo");
    assert_eq!(&reply[..8], &NODE.0);
    assert_eq!(&reply[8..], b"ping");
    assert_eq!(req(&mut c, "app/pi/other", b""), Err(HandlerError::NotFound));
    // A publication on a subscribed key reaches on_message.
    c.publication(Key::from_static("app/pi/in"), b"hello");
    c.publication(Key::from_static("app/pi/nobody"), b"x");
    assert_eq!(on(&c.drain(), "app/pi/out"), [&b"hello"[..]]);
    // The tick (20 ms) and timer 0 (30 ms) run when due, and the
    // controller says when that is.
    assert_eq!(c.next_wake_us(), Some(20_000));
    c.poll();
    assert!(on(&c.drain(), "app/pi/count").is_empty());
    clock.advance_ms(20);
    c.poll();
    assert_eq!(on(&c.drain(), "app/pi/count"), [&1u32.to_le_bytes()[..]]);
    assert_eq!(c.next_wake_us(), Some(30_000));
    clock.advance_ms(10);
    c.poll();
    let outs = c.drain();
    let irq = on(&outs, "app/pi/irq");
    assert_eq!(irq.len(), 1);
    assert_eq!(&irq[0][..4], &0u32.to_le_bytes(), "timer 0");
    assert_eq!(&irq[0][4..], &1u32.to_le_bytes(), "one period");
}

#[test]
fn pn7_traps_stop_and_uploads_end_the_run_and_settle_the_served_keys() {
    let mut disk = Disk::default();
    let (mut c, boot) = open(&mut disk, Manual::default());
    c.boot(&boot);
    install(&mut c, &demo());
    req(&mut c, "sys/payload/start", &[]).expect("start");
    // A trap in on_request answers Internal and ends the run; its keys are
    // then Unavailable, not NotFound.
    assert_eq!(req(&mut c, "app/pi/trap", b""), Err(HandlerError::Internal));
    assert!(!c.running());
    assert_eq!(c.status(), PayloadStatus::Trapped);
    assert!(logs(&c.drain()).iter().any(|l| l.starts_with("payload trapped on request app/pi/trap")));
    assert_eq!(req(&mut c, "app/pi/echo", b""), Err(HandlerError::Unavailable));
    // Started again, then stopped: Unavailable again.
    req(&mut c, "sys/payload/start", &[]).expect("restart");
    assert!(req(&mut c, "app/pi/echo", b"").is_ok());
    req(&mut c, "sys/payload/stop", &[]).expect("stop");
    assert!(!c.running());
    assert_eq!(c.status(), PayloadStatus::Stopped);
    assert_eq!(req(&mut c, "app/pi/echo", b""), Err(HandlerError::Unavailable));
    // A new upload clears them: NotFound, as for any key nothing serves.
    install(&mut c, &demo());
    assert_eq!(req(&mut c, "app/pi/echo", b""), Err(HandlerError::NotFound));
    // A begin while running stops the payload first.
    req(&mut c, "sys/payload/start", &[]).expect("start");
    let steps = upload_plan(&demo(), None, None).expect("plan");
    c.request(steps[0].key, &steps[0].body, &RequestMeta::NONE).expect("begin");
    assert!(!c.running());
    assert_eq!(c.status(), PayloadStatus::Loading);
}

#[test]
fn pn7_an_installed_payload_starts_at_boot_and_bad_images_fail_to_start() {
    let mut disk = Disk::default();
    {
        let (mut c, boot) = open(&mut disk, Manual::default());
        c.boot(&boot);
        install(&mut c, &demo());
        c.shutdown().expect("sync");
    }
    {
        let (mut c, boot) = open(&mut disk, Manual::default());
        c.boot(&boot);
        assert!(c.running(), "autostart");
        assert!(logs(&c.drain()).iter().any(|l| l == "pi: started"));
        assert!(req(&mut c, "app/pi/echo", b"x").is_ok());
        c.shutdown().expect("sync");
        assert!(!c.running());
        // An image that is not WASM installs (the store does not look
        // inside) but fails to start: Internal, Trapped, logged.
        install(&mut c, b"not a wasm module");
        assert_eq!(req(&mut c, "sys/payload/start", &[]), Err(HandlerError::Internal));
        assert_eq!(c.status(), PayloadStatus::Trapped);
        assert!(logs(&c.drain()).iter().any(|l| l.starts_with("payload start failed")));
    }
    // A payload over the memory cap is refused at load.
    let big = wat::parse_str(r#"(module (memory (export "memory") 32))"#).expect("WAT");
    let flash = SectorFlash::open(MemSectors::new(&mut disk), SECTORS).expect("flash");
    let (mut c, boot) = Controller::open(flash, facts(), crate::limits(64 * 1024), Manual::default());
    c.boot(&boot);
    install(&mut c, &big);
    assert_eq!(req(&mut c, "sys/payload/start", &[]), Err(HandlerError::Internal));
}
