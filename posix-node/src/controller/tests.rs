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

// ---- ABI 8: a machine payload bound to the simulator (PN16, PN17) ------

mod machine {
    use super::*;
    use crate::driver::sim::{SimConfig, SimDriver};
    use middleware::loader::control::Page;
    use middleware::loader::manifest::Manifest;
    use middleware::wasm_abi::channels::{Channel, DataKind, Direction, SECTION, encode, encoded_len};
    use middleware::wasm_abi::machine::{Position, QueueState, Segment};

    const DECLS: &[Channel<'static>] = &[
        Channel::machine("z_home", Direction::In, DataKind::Digital, true),
        Channel::machine("z_pos", Direction::In, DataKind::Position, true),
        Channel::machine("motion", Direction::Out, DataKind::Segments, true),
        Channel::machine("queue", Direction::In, DataKind::Segments, true),
    ];
    const LEN: usize = encoded_len(DECLS);
    static SECTION_BYTES: [u8; LEN] = encode::<LEN>(DECLS);

    fn wat_bytes(b: &[u8]) -> String {
        b.iter().map(|x| format!("\\{x:02x}")).collect()
    }

    /// A homing move as a payload: `init` sends Z toward its home end at
    /// 5 mm/s, past the reference switch; when the switch makes,
    /// `on_message` stops the queue with a flush. Positions and queue
    /// states are republished on `app/z` and `app/q` for the test.
    fn homing() -> Vec<u8> {
        let toward = Segment { seq: 1, x_um: 20_000, z_um: 10_000, feed: 5_000, flags: 0 };
        let mut wasm = wat::parse_str(format!(
            r#"(module
  (import "mw" "channel_open" (func $open (param i32 i32) (result i32)))
  (import "mw" "channel_publish" (func $pub (param i32 i32 i32) (result i32)))
  (import "mw" "publish" (func $publish (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 16) "z_home")
  (data (i32.const 24) "z_pos")
  (data (i32.const 32) "motion")
  (data (i32.const 40) "queue")
  (data (i32.const 48) "app/z")
  (data (i32.const 56) "app/q")
  (data (i32.const 200) "{toward}")
  (data (i32.const 300) "{flush}")
  (func (export "init")
    (i32.store (i32.const 100) (call $open (i32.const 16) (i32.const 6)))
    (i32.store (i32.const 104) (call $open (i32.const 24) (i32.const 5)))
    (i32.store (i32.const 108) (call $open (i32.const 32) (i32.const 6)))
    (i32.store (i32.const 112) (call $open (i32.const 40) (i32.const 5)))
    (drop (call $pub (i32.load (i32.const 108)) (i32.const 200) (i32.const 17))))
  (func (export "mw_alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "on_message") (param $slot i32) (param $p i32) (param $n i32)
    (if (i32.and (i32.eq (local.get $slot) (i32.load (i32.const 100))) (i32.eq (i32.load8_u (local.get $p)) (i32.const 1)))
      (then (drop (call $pub (i32.load (i32.const 108)) (i32.const 300) (i32.const 17)))))
    (if (i32.eq (local.get $slot) (i32.load (i32.const 104)))
      (then (drop (call $publish (i32.const 48) (i32.const 5) (local.get $p) (local.get $n)))))
    (if (i32.eq (local.get $slot) (i32.load (i32.const 112)))
      (then (drop (call $publish (i32.const 56) (i32.const 5) (local.get $p) (local.get $n)))))))"#,
            toward = wat_bytes(&toward.encode()),
            flush = wat_bytes(&Segment::flush(2).encode()),
        ))
        .expect("WAT");
        let mut s = vec![SECTION.len() as u8];
        s.extend_from_slice(SECTION.as_bytes());
        s.extend_from_slice(&SECTION_BYTES);
        wasm.push(0);
        wasm.push(s.len() as u8);
        wasm.extend_from_slice(&s);
        wasm
    }

    fn open_sim(disk: &mut Disk, clock: Manual) -> (Ctl<'_>, Boot) {
        let flash = SectorFlash::open(MemSectors::new(disk), SECTORS).expect("flash");
        let driver = SimDriver::new(SimConfig::pc_turn_50());
        let facts = Facts { control: crate::driver::control_summary(crate::driver::Driver::points(&driver)), ..facts() };
        Controller::open_with_driver(flash, facts, crate::limits(1024 * 1024), clock, Box::new(driver))
    }

    fn bind(c: &mut Ctl<'_>, image: &[u8], specs: &[&str]) {
        use sha2::Digest;
        let sha: [u8; 32] = sha2::Sha256::digest(image).into();
        let specs: Vec<String> = specs.iter().map(|s| s.to_string()).collect();
        let record = mwctl::bindspec::record(sha, 1, &specs).expect("binding");
        req(c, "sys/payload/bind", &record).expect("bind");
    }

    #[test]
    fn pn16_the_manifest_and_sys_manifest_cm_list_the_points() {
        let mut disk = Disk::default();
        let (mut c, boot) = open_sim(&mut disk, Manual::default());
        c.boot(&boot);
        let m = Manifest::decode(&req(&mut c, "sys/manifest", &[]).unwrap()).unwrap();
        assert_eq!(m.control.map(|x| (x.points, x.pages)), Some((10, 1)));
        let page = req(&mut c, "sys/manifest/cm", &[]).unwrap();
        let page = Page::decode(&page).unwrap();
        let names: Vec<&str> = page.points().map(|p| p.name).collect();
        assert_eq!(names[..2], ["x_pos", "z_pos"]);
        assert_eq!(req(&mut c, "sys/manifest/cm", &[1]), Err(HandlerError::NotFound));
    }

    #[test]
    fn pn17_a_payload_homes_an_axis_on_the_simulator() {
        let mut disk = Disk::default();
        let clock = Manual::default();
        let (mut c, boot) = open_sim(&mut disk, clock.clone());
        c.boot(&boot);
        let image = homing();
        install(&mut c, &image);
        bind(&mut c, &image, &["z_home=driver:z_home", "z_pos=driver:z_pos", "motion=driver:motion", "queue=driver:motion"]);
        assert_eq!(req(&mut c, "sys/payload/start", &[]), Ok(Vec::new()), "{:?}", logs(&c.drain()));
        let outs = c.drain();
        assert!(outs.iter().all(|o| !o.key.starts_with("drv/")), "driver keys never leave the node");
        // The snapshot reached it: Z at its start, -100 mm.
        let z = on(&outs, "app/z");
        assert_eq!(Position::decode(z[0]).unwrap().counts, -100_000);
        // 110 mm at 5 mm/s: the switch makes 0.2 mm before +0.5 mm, after
        // about 20 s; run 30 s in 1 ms polls.
        let mut zs = Vec::new();
        let mut qs = Vec::new();
        for _ in 0..30_000 {
            clock.advance_ms(1);
            c.poll();
            let outs = c.drain();
            zs.extend(on(&outs, "app/z").iter().filter_map(|b| Position::decode(b)));
            qs.extend(on(&outs, "app/q").iter().filter_map(|b| QueueState::decode(b)));
        }
        let last = zs.last().unwrap().counts;
        assert!((300..=310).contains(&last), "stopped at the reference switch, not the stop: {last}");
        let q = qs.last().unwrap();
        assert_eq!((q.flags & QueueState::IDLE, q.done), (QueueState::IDLE, 0), "flushed, not finished");
        assert_eq!(c.next_wake_us(), None, "nothing moves, nothing to wake for");
    }

    #[test]
    fn pn16_a_binding_to_a_missing_or_wrong_point_fails_the_start() {
        let mut disk = Disk::default();
        let (mut c, boot) = open_sim(&mut disk, Manual::default());
        c.boot(&boot);
        let image = homing();
        install(&mut c, &image);
        for (specs, why) in [
            (["z_home=driver:w_home", "z_pos=driver:z_pos", "motion=driver:motion", "queue=driver:motion"], "no point w_home"),
            (["z_home=driver:z_pos", "z_pos=driver:z_pos", "motion=driver:motion", "queue=driver:motion"], "carries position"),
        ] {
            bind(&mut c, &image, &specs);
            c.drain();
            assert_eq!(req(&mut c, "sys/payload/start", &[]), Err(HandlerError::Internal));
            let lines = logs(&c.drain());
            assert!(lines.iter().any(|l| l.contains("driver:") && l.contains(why)), "{lines:?}");
            assert!(!c.running());
        }
    }

    #[test]
    fn pn16_without_a_driver_a_driver_bound_payload_does_not_start() {
        let mut disk = Disk::default();
        let (mut c, boot) = open(&mut disk, Manual::default());
        c.boot(&boot);
        let image = homing();
        install(&mut c, &image);
        bind(&mut c, &image, &["z_home=driver:z_home", "z_pos=driver:z_pos", "motion=driver:motion", "queue=driver:motion"]);
        assert_eq!(req(&mut c, "sys/payload/start", &[]), Err(HandlerError::Internal));
        assert_eq!(req(&mut c, "sys/manifest/cm", &[]), Err(HandlerError::NotFound));
    }
}

/// Requests over Zenoh (`Controller::mesh_request`): the gate, the
/// requester origins and the persisted control counter.
mod mesh {
    use super::*;
    use crate::driver::sim::{SimConfig, SimDriver};
    use crate::service::tests::test_seed;
    use middleware::SenderId;
    use middleware::loader::trust::{ControlCounter, ControlOp, TARGET_LEN, TrustInfo, TrustState, Version};
    use mwctl::sign::{sign_commit, sign_control};

    const A: Option<SenderId> = Some(SenderId([0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8]));
    const B: Option<SenderId> = Some(SenderId([0xb1, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8]));

    fn mesh(c: &mut Ctl<'_>, from: Option<SenderId>, key: &str, body: &[u8]) -> Result<Vec<u8>, HandlerError> {
        c.mesh_request(Key::new(key).expect("key"), from, body)
    }

    fn claim(c: &mut Ctl<'_>) {
        req(c, "sys/trust/claim", test_seed().public_key().as_bytes()).expect("claim over serial");
    }

    fn trust(c: &mut Ctl<'_>) -> TrustState {
        TrustInfo::decode(&req(c, "sys/trust/info", &[]).expect("trust info")).expect("decode").state
    }

    /// The upload's steps, the commit signed as `version` when given.
    fn steps(image: &[u8], version: Option<u32>) -> Vec<mwctl::plan::Step> {
        let auth = version.map(|v| sign_commit(image, &test_seed(), Version(v)).expect("sign"));
        upload_plan(image, None, auth).expect("plan")
    }

    fn upload(
        c: &mut Ctl<'_>,
        from: Option<SenderId>,
        image: &[u8],
        version: Option<u32>,
    ) -> Result<Vec<u8>, HandlerError> {
        let mut last = Ok(Vec::new());
        for s in steps(image, version) {
            last = c.mesh_request(s.key, from, &s.body);
            last.as_ref().map_err(|e| *e)?;
        }
        last
    }

    fn control(op: ControlOp, target: [u8; TARGET_LEN], counter: u64) -> Vec<u8> {
        sign_control(&test_seed(), op, &target, ControlCounter(counter)).expect("sign").encode().to_vec()
    }

    fn own() -> [u8; TARGET_LEN] {
        let mut t = [0u8; TARGET_LEN];
        t.copy_from_slice(&NODE.0[..TARGET_LEN]);
        t
    }

    #[test]
    fn pn23_an_unclaimed_node_answers_only_its_description_over_zenoh() {
        let mut disk = Disk::default();
        let flash = SectorFlash::open(MemSectors::new(&mut disk), SECTORS).expect("flash");
        let driver = SimDriver::new(SimConfig::pc_turn_50());
        let facts =
            Facts { control: crate::driver::control_summary(crate::driver::Driver::points(&driver)), ..facts() };
        let (mut c, boot) =
            Controller::open_with_driver(flash, facts, crate::limits(1024 * 1024), Manual::default(), Box::new(driver));
        c.boot(&boot);
        let info = DeviceInfo::decode(&mesh(&mut c, A, "sys/info", &[]).expect("info")).map(|i| i.node);
        assert_eq!(info, Some(Some(NODE)));
        assert_eq!(mesh(&mut c, None, "sys/manifest", &[]), req(&mut c, "sys/manifest", &[]));
        assert_eq!(mesh(&mut c, A, "sys/manifest/cm", &[]), req(&mut c, "sys/manifest/cm", &[]));
        assert!(mesh(&mut c, A, "sys/trust/info", &[]).is_ok());
        let image = demo();
        for s in steps(&image, None) {
            assert_eq!(c.mesh_request(s.key, A, &s.body), Err(HandlerError::Denied), "{}", s.key.as_str());
        }
        for key in ["sys/payload/start", "sys/payload/stop", "sys/payload/binding", "sys/payload/bind"] {
            assert_eq!(mesh(&mut c, A, key, &[]), Err(HandlerError::Denied), "{key}");
        }
        let claim = test_seed().public_key();
        assert_eq!(mesh(&mut c, A, "sys/trust/claim", claim.as_bytes()), Err(HandlerError::Denied));
        assert_eq!(trust(&mut c), TrustState::Unclaimed);
        assert_eq!(c.status(), PayloadStatus::Empty);
        assert_eq!(c.service().loader_owner(), None);
    }

    #[test]
    fn pn23_pn24_a_signed_upload_and_start_over_zenoh_and_no_replay_across_restart() {
        let mut disk = Disk::default();
        let image = demo();
        let counter = 1_000;
        {
            let (mut c, boot) = open(&mut disk, Manual::default());
            c.boot(&boot);
            claim(&mut c);
            assert_eq!(upload(&mut c, A, &image, None), Err(HandlerError::Denied));
            upload(&mut c, A, &image, Some(1)).expect("signed upload");
            assert_eq!(upload(&mut c, A, &image, Some(1)), Err(HandlerError::Denied), "a replayed version");
            assert_eq!(mesh(&mut c, A, "sys/payload/start", &[]), Err(HandlerError::Denied));
            // The RT1050's address rule is not this node's.
            let rt = [0x02, NODE.0[3], NODE.0[4], NODE.0[5], NODE.0[6], NODE.0[7]];
            assert_eq!(
                mesh(&mut c, A, "sys/payload/start", &control(ControlOp::Start, rt, counter)),
                Err(HandlerError::Denied)
            );
            assert!(!c.running());
            let start = control(ControlOp::Start, own(), counter);
            assert_eq!(mesh(&mut c, B, "sys/payload/start", &start), Ok(Vec::new()));
            assert!(c.running());
            assert_eq!(c.service().state().control(), ControlCounter(counter));
            assert_eq!(mesh(&mut c, A, "sys/payload/start", &start), Err(HandlerError::Denied), "replayed");
            let stop = control(ControlOp::Stop, own(), counter + 1);
            assert_eq!(mesh(&mut c, A, "sys/payload/stop", &[]), Err(HandlerError::Denied));
            assert!(c.running());
            assert_eq!(mesh(&mut c, A, "sys/payload/stop", &stop), Ok(Vec::new()));
            assert!(!c.running());
            c.shutdown().expect("sync");
        }
        // After a restart the counter holds: the same start is refused.
        let (mut c, boot) = open(&mut disk, Manual::default());
        c.boot(&boot);
        assert_eq!(c.service().state().control(), ControlCounter(counter + 1));
        let start = control(ControlOp::Start, own(), counter);
        assert_eq!(mesh(&mut c, A, "sys/payload/start", &start), Err(HandlerError::Denied));
    }

    #[test]
    fn pn23_pn25_a_staged_upload_belongs_to_its_requester_and_survives_unsigned_commits() {
        let mut disk = Disk::default();
        let (mut c, boot) = open(&mut disk, Manual::default());
        c.boot(&boot);
        claim(&mut c);
        let image = demo();
        upload(&mut c, A, &image, Some(1)).expect("first");
        mesh(&mut c, A, "sys/payload/start", &control(ControlOp::Start, own(), 10)).expect("start");
        assert!(c.running());
        // A second upload by A, staged beside the running payload.
        let s = steps(&image, Some(2));
        let (commit, transfer) = s.split_last().expect("steps");
        let (begin, chunks) = transfer.split_first().expect("begin");
        c.mesh_request(begin.key, A, &begin.body).expect("begin");
        assert!(c.running(), "a staged begin leaves the payload running");
        assert_eq!(c.status(), PayloadStatus::Running);
        // B can neither continue it nor end it.
        assert_eq!(c.mesh_request(chunks[0].key, B, &chunks[0].body), Err(HandlerError::InvalidRequest));
        assert_eq!(mesh(&mut c, B, "sys/loader/commit", &[]), Err(HandlerError::InvalidRequest));
        assert_eq!(c.mesh_request(commit.key, B, &commit.body), Err(HandlerError::InvalidRequest));
        // Nor can the anonymous requester, nor a serial link.
        assert_eq!(c.mesh_request(chunks[0].key, None, &chunks[0].body), Err(HandlerError::InvalidRequest));
        assert_eq!(c.request(chunks[0].key, &chunks[0].body, &RequestMeta::NONE), Err(HandlerError::InvalidRequest));
        for ch in chunks {
            c.mesh_request(ch.key, A, &ch.body).expect("chunk");
        }
        // An unsigned commit, A's own included, is Denied and ends nothing.
        assert_eq!(mesh(&mut c, A, "sys/loader/commit", &[]), Err(HandlerError::Denied));
        assert_eq!(c.service().loader_owner(), Some(crate::mesh_gate::requester(A, crate::mesh_gate::address(NODE))));
        assert!(c.running());
        // A's signed commit still activates it (and replaces the running payload).
        c.mesh_request(commit.key, A, &commit.body).expect("signed commit");
        assert_eq!(c.service().loader_owner(), None);
        assert!(!c.running());
        assert!(matches!(trust(&mut c), TrustState::Claimed { min_version: Version(2), .. }));
    }

    #[test]
    fn pn25_a_serial_begin_takes_a_zenoh_transfer_over() {
        let mut disk = Disk::default();
        let (mut c, boot) = open(&mut disk, Manual::default());
        c.boot(&boot);
        claim(&mut c);
        let s = steps(&demo(), Some(3));
        c.mesh_request(s[0].key, A, &s[0].body).expect("mesh begin");
        c.request(s[0].key, &s[0].body, &RequestMeta::NONE).expect("serial begin");
        assert_eq!(c.service().loader_owner(), Some(esp_base::ota::Origin::Serial));
        assert_eq!(c.mesh_request(s[1].key, A, &s[1].body), Err(HandlerError::InvalidRequest));
        for step in &s[1..] {
            c.request(step.key, &step.body, &RequestMeta::NONE).expect("serial transfer");
        }
    }
}
