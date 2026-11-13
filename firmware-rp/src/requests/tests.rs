//! The request service with the real interpreter: the round 8 generated
//! phase server `pico-led` (`tools/payload-sim/tests/fixtures/phase/`,
//! kept by `web/test/phase.gen.test.ts`) installed through the loader,
//! started, and driven over serial frames: requests in as `Frame::Request`
//! bytes, replies out as `Frame::Response`, the payload's state reports
//! out as `[pub]` frames, decoded with `recipe_core::wire`.

extern crate std;

use std::string::{String, ToString};
use std::vec::Vec;

use middleware::crc::Crc32;
use middleware::loader::{Caps, DeviceInfo, PayloadStatus, keys};
use middleware::serial::frame::{self, Frame, MAX_RAW_FRAME, MAX_WIRE_FRAME};
use middleware::transport::{HandlerError, RequestMeta, SenderId};
use middleware::wasm_abi::{AbiError, Pull};
use middleware::Key;
use recipe_core::phase::{Command, PhaseState};
use recipe_core::wire;
use wasm_host::{Device, Limits, Payload};

use super::{Answer, ended, handle};
use crate::fw_update::Absent;
use crate::ram_flash::RamFlash;
use crate::service::tests::{FACTS, MAP, NODE, flash};
use crate::service::{Reply, Service};

type Flash = RamFlash<0x1_4000>;
type Svc<'f> = Service<&'f mut Flash, Absent>;

fn fixture(name: &str) -> Vec<u8> {
    let path = std::format!("{}/../tools/payload-sim/tests/fixtures/phase/{name}.wasm", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// The board as the payload sees it: the clock, the LED, and the serial
/// bytes its publications became (as the link task writes them).
struct Board {
    node: SenderId,
    now: u32,
    led: Vec<bool>,
    wire: Vec<u8>,
    served: Vec<String>,
}

impl Board {
    fn new(node: SenderId) -> Self {
        Board { node, now: 0, led: Vec::new(), wire: Vec::new(), served: Vec::new() }
    }

    /// The `[pub]` frames written since the last call, as (key, body).
    fn take_publications(&mut self) -> Vec<(String, Vec<u8>)> {
        let bytes = core::mem::take(&mut self.wire);
        bytes
            .split(|&b| b == 0)
            .filter(|f| !f.is_empty())
            .map(|f| {
                let mut f = f.to_vec();
                match frame::decode_wire(&mut f).expect("a frame") {
                    Frame::Publish { key, payload } => (key.as_str().to_string(), payload.to_vec()),
                    other => panic!("not a publication: {other:?}"),
                }
            })
            .collect()
    }
}

impl Device for Board {
    fn log(&mut self, _line: &str) {}
    fn publish(&mut self, key: Key<'_>, data: &[u8]) -> Result<(), AbiError> {
        let mut scratch = [0u8; MAX_RAW_FRAME];
        let mut out = [0u8; MAX_WIRE_FRAME];
        let n = frame::encode(&Frame::Publish { key, payload: data }, &mut scratch, &mut out)
            .map_err(|_| AbiError::Transport)?;
        self.wire.extend_from_slice(&out[..n]);
        Ok(())
    }
    fn now_ms(&mut self) -> u32 {
        self.now
    }
    fn gpio_output(&mut self, _: u8) -> Result<(), AbiError> {
        Ok(())
    }
    fn gpio_input(&mut self, _: u8, _: Pull) -> Result<(), AbiError> {
        Ok(())
    }
    fn gpio_write(&mut self, _: u8, high: bool) -> Result<(), AbiError> {
        self.led.push(high);
        Ok(())
    }
    fn gpio_read(&mut self, _: u8) -> Result<bool, AbiError> {
        Ok(false)
    }
    /// What `RpDevice::serve` does: `esp_base::serve::servable`.
    fn serve(&mut self, key: Key<'_>) -> Result<(), AbiError> {
        esp_base::serve::servable(key)?;
        self.served.push(key.as_str().to_string());
        Ok(())
    }
    /// What `RpDevice::node_id` does: the `sys/info` node.
    fn node_id(&mut self) -> Result<SenderId, AbiError> {
        Ok(self.node)
    }
}

/// The host side of the USB link: frames one request, the device decodes
/// it, routes it through [`handle`] and frames the response, which the
/// host decodes.
struct Host<'f> {
    svc: Svc<'f>,
    payload: Option<Payload<Board>>,
    id: u8,
}

impl<'f> Host<'f> {
    fn request(&mut self, key: &str, body: &[u8]) -> (Result<Vec<u8>, HandlerError>, bool) {
        self.id = self.id.wrapping_add(1);
        let mut scratch = [0u8; MAX_RAW_FRAME];
        let mut wire_in = [0u8; MAX_WIRE_FRAME];
        let k = Key::new(key).expect("key");
        let n = frame::encode(&Frame::Request { id: self.id, key: k, payload: body }, &mut scratch, &mut wire_in).unwrap();
        // Device side: decode, route, answer.
        let mut rx = wire_in[..n - 1].to_vec();
        let Frame::Request { id, key, payload } = frame::decode_wire(&mut rx).unwrap() else { panic!("request") };
        let mut out = [0u8; 512];
        // USB frames name no sender (the device's link has no peer) and no
        // deadline.
        let Answer { handled, trapped } = handle(&mut self.svc, self.payload.as_mut(), key, payload, &RequestMeta::NONE, &mut out);
        let result: Result<&[u8], HandlerError> = match handled.reply {
            Reply::Now(r) => r.map(|n| &out[..n]),
            Reply::Start => {
                self.start();
                Ok(&[])
            }
        };
        let stop = handled.stop || trapped.is_some();
        let mut wire_out = [0u8; MAX_WIRE_FRAME];
        let m = frame::encode(&Frame::Response { id, result }, &mut scratch, &mut wire_out).unwrap();
        if stop && let Some(p) = self.payload.take() {
            ended(&mut self.svc, &p);
        }
        // Host side.
        let mut back = wire_out[..m - 1].to_vec();
        match frame::decode_wire(&mut back).unwrap() {
            Frame::Response { id: rid, result } => {
                assert_eq!(rid, self.id);
                (result.map(|b| b.to_vec()), trapped.is_some())
            }
            other => panic!("not a response: {other:?}"),
        }
    }

    /// What the controller's running session does on `Reply::Start`.
    fn start(&mut self) {
        let store = self.svc.store_mut().expect("store");
        let header = store.header().expect("installed");
        let mut image = std::vec![0u8; header.len as usize];
        store.load(header, &mut image).expect("loads");
        let payload = Payload::load(&image, Board::new(self.svc.node()), Limits::default()).expect("instantiates");
        self.svc.started();
        self.payload = Some(payload);
    }

    fn upload(&mut self, image: &[u8]) {
        let mut b = [0u8; 8];
        b[..4].copy_from_slice(&(image.len() as u32).to_le_bytes());
        b[4..].copy_from_slice(&Crc32::checksum(image).to_le_bytes());
        assert_eq!(self.request(keys::BEGIN.as_str(), &b).0, Ok(Vec::new()));
        for (i, part) in image.chunks(512).enumerate() {
            let mut c = ((i * 512) as u32).to_le_bytes().to_vec();
            c.extend_from_slice(part);
            assert_eq!(self.request(keys::CHUNK.as_str(), &c).0, Ok(Vec::new()));
        }
        assert_eq!(self.request(keys::COMMIT.as_str(), &[]).0, Ok(Vec::new()));
    }

    fn board(&mut self) -> &mut Board {
        self.payload.as_mut().expect("running").device()
    }

    /// Advances the clock in 10 ms steps, ticking when due, for `ms`.
    fn run_for(&mut self, ms: u32) {
        for _ in 0..ms / 10 {
            let p = self.payload.as_mut().expect("running");
            p.device().now += 10;
            let now = p.device().now;
            if p.next_tick().is_some_and(|due| due <= now) {
                p.tick(now).expect("no trap");
            }
        }
    }

    /// The states in the `[pub]` reports on `state_key` since the last call.
    fn states(&mut self, state_key: &str) -> Vec<PhaseState> {
        self.board()
            .take_publications()
            .into_iter()
            .filter(|(k, _)| k == state_key)
            .map(|(_, b)| wire::decode_report(&b).expect("report").state)
            .collect()
    }
}

fn phase_keys(node: SenderId) -> (String, String) {
    let mut a = [0u8; wire::MAX_KEY];
    let mut b = [0u8; wire::MAX_KEY];
    let cmd = wire::command_key(node.0, "LED", "Blink", &mut a).unwrap().to_string();
    let state = wire::state_key(node.0, "LED", "Blink", &mut b).unwrap().to_string();
    (cmd, state)
}

fn command(c: Command, id: u32, args: &[i32]) -> Vec<u8> {
    let mut buf = [0u8; wire::MAX_BODY];
    wire::encode_command(c, id, args, &mut buf).unwrap().to_vec()
}

fn ack(state: PhaseState, id: u32, accepted: bool) -> Vec<u8> {
    let mut buf = [0u8; wire::MAX_BODY];
    wire::encode_ack(accepted, state, id, &mut buf).unwrap().to_vec()
}

fn host(flash: &mut Flash) -> Host<'_> {
    let (svc, _) = Service::open(flash, &MAP, FACTS, Absent);
    Host { svc, payload: None, id: 0 }
}

/// Collapses repeats (heartbeats repeat the state).
fn changes(states: &[PhaseState]) -> Vec<PhaseState> {
    let mut out: Vec<PhaseState> = Vec::new();
    for &s in states {
        if out.last() != Some(&s) {
            out.push(s);
        }
    }
    out
}

#[test]
fn m7_the_pico_led_phase_server_runs_blink_to_complete_through_the_request_service() {
    let mut f = flash();
    let mut h = host(&mut f);
    let (cmd, state) = phase_keys(NODE);

    // sys/info says the board routes requests and names its node.
    let info = h.request(keys::INFO.as_str(), &[]).0.unwrap();
    let info = DeviceInfo::decode(&info).unwrap();
    assert_eq!((info.node, info.caps), (Some(NODE), Caps::SERVE));

    // Before any payload: the phase key is nobody's.
    assert_eq!(h.request(&cmd, &command(Command::Start, 1, &[250, 3])).0, Err(HandlerError::NotFound));

    h.upload(&fixture("pico-led"));
    assert_eq!(h.request(keys::START.as_str(), &[]).0, Ok(Vec::new()));
    assert_eq!(h.svc.status(), PayloadStatus::Running);
    // init read node_id and served the key naming this node.
    assert_eq!(h.board().served, std::slice::from_ref(&cmd));
    assert_eq!(h.states(&state), [PhaseState::Idle]);

    // Start(period 250 ms, 3 times): accepted, Running, reported.
    let (r, trapped) = h.request(&cmd, &command(Command::Start, 7, &[250, 3]));
    assert!(!trapped);
    assert_eq!(r, Ok(ack(PhaseState::Running, 7, true)));
    assert_eq!(changes(&h.states(&state)), [PhaseState::Running]);
    // A sys key while the payload serves still reaches the firmware.
    assert!(DeviceInfo::decode(&h.request(keys::INFO.as_str(), &[]).0.unwrap()).is_some());
    // Start again while Running: rejected with the current state.
    assert_eq!(h.request(&cmd, &command(Command::Start, 8, &[250, 3])).0, Ok(ack(PhaseState::Running, 8, false)));
    // Out of range (times 1..=10): InvalidRequest from the payload.
    assert_eq!(h.request(&cmd, &command(Command::Start, 9, &[250, 11])).0, Err(HandlerError::InvalidRequest));

    // Three toggles at 250 ms, then Complete.
    h.run_for(1_000);
    let seen = changes(&h.states(&state));
    assert_eq!(seen.last(), Some(&PhaseState::Complete), "{seen:?}");
    assert!(seen.iter().all(|s| matches!(s, PhaseState::Running | PhaseState::Complete)), "{seen:?}");
    // init drove the LED low; then three toggles.
    assert_eq!(h.board().led, [false, true, false, true]);

    // Reset: Resetting (LED off), then Idle.
    assert_eq!(h.request(&cmd, &command(Command::Reset, 10, &[])).0, Ok(ack(PhaseState::Resetting, 10, true)));
    h.run_for(100);
    let after = changes(&h.states(&state));
    assert_eq!(after.last(), Some(&PhaseState::Idle), "{after:?}");
    assert_eq!(h.board().led.last(), Some(&false));

    // Stop the payload: its key answers Unavailable (resumable), not
    // NotFound; other keys are still nobody's.
    assert_eq!(h.request(keys::STOP.as_str(), &[]).0, Ok(Vec::new()));
    assert!(h.payload.is_none());
    assert_eq!(h.svc.served().iter().collect::<Vec<_>>(), [cmd.as_str()]);
    assert_eq!(h.request(&cmd, &command(Command::Start, 11, &[250, 3])).0, Err(HandlerError::Unavailable));
    assert_eq!(h.request("app/other", &[]).0, Err(HandlerError::NotFound));

    // A new image clears the table: NotFound until a payload serves it.
    h.upload(&fixture("pico-led"));
    assert!(h.svc.served().is_empty());
    assert_eq!(h.request(&cmd, &command(Command::Start, 12, &[250, 3])).0, Err(HandlerError::NotFound));
}

/// A payload whose `on_request` traps (`unreachable`), serving `app/boom`.
fn trapping() -> Vec<u8> {
    wat::parse_str(
        r#"(module
  (import "mw" "serve" (func $serve (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 32) "app/boom")
  (data (i32.const 48) "sys/info")
  (global $slot (export "slot") (mut i32) (i32.const 0))
  (global $sys (export "sys") (mut i32) (i32.const 0))
  (func (export "init")
    (global.set $sys (call $serve (i32.const 48) (i32.const 8)))
    (global.set $slot (call $serve (i32.const 32) (i32.const 8))))
  (func (export "mw_alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "on_request") (param i32 i32 i32 i32 i32 i32) (result i32) unreachable))"#,
    )
    .expect("WAT")
}

#[test]
fn m7_a_trap_answers_internal_stops_the_payload_and_its_key_is_then_unavailable() {
    let mut f = flash();
    let mut h = host(&mut f);
    h.upload(&trapping());
    assert_eq!(h.request(keys::START.as_str(), &[]).0, Ok(Vec::new()));
    // `serve("sys/info")` was refused (wasm-host and the device both
    // refuse sys keys), so only app/boom is served.
    assert_eq!(h.board().served, ["app/boom"]);
    let (r, trapped) = h.request("app/boom", b"x");
    assert_eq!((r, trapped), (Err(HandlerError::Internal), true));
    assert!(h.payload.is_none(), "the shell drops a trapped payload");
    assert_eq!(h.svc.status(), PayloadStatus::Trapped);
    assert_eq!(h.request("app/boom", b"x").0, Err(HandlerError::Unavailable));
    // sys/info is the firmware's, trapped or not.
    let body = h.request(keys::INFO.as_str(), &[]).0.unwrap();
    assert_eq!(DeviceInfo::decode(&body).unwrap().status, PayloadStatus::Trapped);
    // A restart serves again.
    assert_eq!(h.request(keys::START.as_str(), &[]).0, Ok(Vec::new()));
    assert_eq!(h.request("app/boom", b"x").0, Err(HandlerError::Internal));
}

#[test]
fn m7_without_the_runtime_sys_info_reports_no_capability() {
    let mut f = flash();
    let facts = crate::service::Facts { runtime: false, firmware: "0.1.0+no-wasm", chip: "rp2040", ..FACTS };
    let (mut svc, _) = Service::open(&mut *f, &MAP, facts, Absent);
    let svc = &mut svc;
    let mut out = [0u8; 128];
    let handled = svc.handle(keys::INFO, &[], &mut out);
    let Reply::Now(Ok(n)) = handled.reply else { panic!("info") };
    let info = DeviceInfo::decode(&out[..n]).unwrap();
    assert_eq!((info.node, info.caps), (Some(NODE), Caps::NONE));
}
