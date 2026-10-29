//! End-to-end tests of the ABI 4 route imports (requirement R8), with WAT
//! payloads and a fake device that records each route call with its parsed
//! arguments and hands out handles.

use middleware::Key;
use middleware::wasm_abi::{ABI1_IMPORTS, ABI2_IMPORTS, ABI3_IMPORTS, IMPORTS, Import};
use wasm_host::{
    AbiError, Device, Event, EventKind, Limits, Payload, Pull, RouteStat, SinkConfig, Source,
    SourceConfig,
};

#[derive(Default)]
struct Fake {
    /// Error every route call returns instead, when set.
    fail: Option<AbiError>,
    /// Handle the next route_source / route_sink returns.
    handle: u8,
    /// Value route_stat returns.
    stat: u32,
    calls: Vec<String>,
    published: Vec<(String, Vec<u8>)>,
}

impl Fake {
    fn results(&self) -> Vec<i32> {
        self.published
            .iter()
            .filter(|(k, _)| k == "r")
            .map(|(_, d)| i32::from_le_bytes(d.as_slice().try_into().expect("4 bytes")))
            .collect()
    }

    fn outcome(&mut self, call: String) -> Result<(), AbiError> {
        self.calls.push(call);
        self.fail.map_or(Ok(()), Err)
    }
}

impl Device for Fake {
    fn log(&mut self, _: &str) {}
    fn publish(&mut self, key: Key<'_>, data: &[u8]) -> Result<(), AbiError> {
        self.published
            .push((key.as_str().to_string(), data.to_vec()));
        Ok(())
    }
    fn now_ms(&mut self) -> u32 {
        0
    }
    fn gpio_output(&mut self, _: u8) -> Result<(), AbiError> {
        Ok(())
    }
    fn gpio_input(&mut self, _: u8, _: Pull) -> Result<(), AbiError> {
        Ok(())
    }
    fn gpio_write(&mut self, _: u8, _: bool) -> Result<(), AbiError> {
        Ok(())
    }
    fn gpio_read(&mut self, _: u8) -> Result<bool, AbiError> {
        Ok(false)
    }
    fn route_source(
        &mut self,
        source: Source,
        key: Key<'_>,
        cfg: SourceConfig,
    ) -> Result<u8, AbiError> {
        let source = match source {
            Source::TestPattern => "TestPattern".to_string(),
            Source::Camera(pins) => format!("Camera{:?}", pins.all()),
        };
        self.outcome(format!(
            "source {source} {} {}x{} {}ms chunk={}",
            key.as_str(),
            cfg.width,
            cfg.height,
            cfg.period_ms,
            cfg.chunk_data
        ))?;
        Ok(self.handle)
    }
    fn route_sink(&mut self, key: Key<'_>, bus: u8, cfg: &SinkConfig) -> Result<u8, AbiError> {
        let segs: Vec<String> = cfg
            .preamble
            .segments()
            .map(|s| format!("{}{:02x?}", if s.dc { "D" } else { "C" }, s.bytes))
            .collect();
        self.outcome(format!(
            "sink {} bus={bus} dc={} frame={} {}",
            key.as_str(),
            cfg.dc_pin,
            cfg.frame_len,
            segs.join(" ")
        ))?;
        Ok(self.handle)
    }
    fn route_stop(&mut self, handle: u8) -> Result<(), AbiError> {
        self.outcome(format!("stop {handle}"))
    }
    fn route_stat(&mut self, handle: u8, stat: RouteStat) -> Result<u32, AbiError> {
        self.outcome(format!("stat {handle} {stat:?}"))?;
        Ok(self.stat)
    }
}

/// Implements only the ABI 1 methods, so every route method is a default.
struct Abi1Board(Vec<i32>);

impl Device for Abi1Board {
    fn log(&mut self, _: &str) {}
    fn publish(&mut self, _: Key<'_>, d: &[u8]) -> Result<(), AbiError> {
        self.0
            .push(i32::from_le_bytes(d.try_into().expect("4 bytes")));
        Ok(())
    }
    fn now_ms(&mut self) -> u32 {
        0
    }
    fn gpio_output(&mut self, _: u8) -> Result<(), AbiError> {
        Ok(())
    }
    fn gpio_input(&mut self, _: u8, _: Pull) -> Result<(), AbiError> {
        Ok(())
    }
    fn gpio_write(&mut self, _: u8, _: bool) -> Result<(), AbiError> {
        Ok(())
    }
    fn gpio_read(&mut self, _: u8) -> Result<bool, AbiError> {
        Ok(false)
    }
}

/// The route imports plus `$report` (publishes an i32 on key `r`). Data:
/// "r" at 0, the stream key "app/cam/frame" (13 bytes) at 32, a 120×120
/// 100 ms source config at 64, a 28 800-byte sink config with D/C 13 and
/// the centred 120×120 MIPI window (28 bytes) at 80, "sys/x" at 128, and
/// a 240×240 JPEG 500 ms camera config with the AI-Thinker pins (21 bytes)
/// at 160.
const PRELUDE: &str = r#"
  (import "mw" "publish" (func $publish (param i32 i32 i32 i32) (result i32)))
  (import "mw" "route_source" (func $route_source (param i32 i32 i32 i32 i32) (result i32)))
  (import "mw" "route_sink" (func $route_sink (param i32 i32 i32 i32 i32) (result i32)))
  (import "mw" "route_stop" (func $route_stop (param i32) (result i32)))
  (import "mw" "route_stat" (func $route_stat (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "r")
  (data (i32.const 32) "app/cam/frame")
  (data (i32.const 64) "\78\00\78\00\00\00\64\00\00\00")
  (data (i32.const 80) "\0d\00\80\70\00\00\05\00\01\2a\01\04\00\3c\00\b3\00\01\2b\01\04\00\3c\00\b3\00\01\2c")
  (data (i32.const 128) "sys/x")
  (data (i32.const 160) "\f0\00\f0\00\01\00\f4\01\00\00\05\12\13\15\24\27\22\23\19\17\16")
  (func $report (param $v i32)
    (i32.store (i32.const 4) (local.get $v))
    (drop (call $publish (i32.const 0) (i32.const 1) (i32.const 4) (i32.const 4))))
"#;

fn run_init<D: Device + 'static>(calls: &str, device: D) -> Payload<D> {
    let wasm = wat::parse_str(format!(
        r#"(module {PRELUDE} (func (export "init") {calls})
             (func (export "on_irq") (param i32 i32 i32 i32)
               (call $report (local.get 0))
               (call $report (local.get 1))
               (call $report (local.get 2))))"#
    ))
    .expect("valid WAT");
    Payload::load(&wasm, device, Limits::default()).expect("payload loads")
}

const BAD: i32 = AbiError::InvalidArgument.code();
const OOB: i32 = AbiError::OutOfBounds.code();
const KEY: i32 = AbiError::InvalidKey.code();
const UNSUPPORTED: i32 = AbiError::Unsupported.code();
const NO_RES: i32 = AbiError::NoResources.code();
const BUSY: i32 = AbiError::Busy.code();

const SOURCE_OK: &str = "(call $report (call $route_source (i32.const 0) (i32.const 32) (i32.const 13) (i32.const 64) (i32.const 10)))";
const SINK_OK: &str = "(call $report (call $route_sink (i32.const 32) (i32.const 13) (i32.const 0) (i32.const 80) (i32.const 28)))";

fn importing(imports: &[Import]) -> Vec<u8> {
    let decls: String = imports
        .iter()
        .map(|i| {
            let params = " i32".repeat(i.params);
            let result = if i.returns { " (result i32)" } else { "" };
            format!(
                r#"(import "mw" "{}" (func (param{params}){result}))"#,
                i.name
            )
        })
        .collect();
    wat::parse_str(format!(r#"(module {decls} (memory (export "memory") 1))"#)).expect("WAT")
}

#[test]
fn r1_every_import_links_and_older_revisions_still_load() {
    assert_eq!(IMPORTS.len(), ABI3_IMPORTS + 4);
    for (what, imports) in [
        ("ABI 4", IMPORTS),
        ("ABI 3", &IMPORTS[..ABI3_IMPORTS]),
        ("ABI 2", &IMPORTS[..ABI2_IMPORTS]),
        ("ABI 1", &IMPORTS[..ABI1_IMPORTS]),
    ] {
        let p = Payload::load(&importing(imports), Fake::default(), Limits::default());
        assert!(p.is_ok(), "{what}: {:?}", p.err());
        let p = Payload::load(
            &importing(imports),
            Abi1Board(Vec::new()),
            Limits::default(),
        );
        assert!(p.is_ok(), "{what} on an ABI 1 board: {:?}", p.err());
    }
}

#[test]
fn r8_route_source_passes_parsed_arguments_and_returns_the_handle() {
    let fake = Fake {
        handle: 2,
        ..Fake::default()
    };
    let mut p = run_init(SOURCE_OK, fake);
    assert_eq!(p.device().results(), vec![2]);
    assert_eq!(
        p.device().calls,
        vec!["source TestPattern app/cam/frame 120x120 100ms chunk=166"]
    );
}

#[test]
fn r15_camera_source_passes_its_pins_and_needs_21_bytes() {
    let mut p = run_init(
        r#"(call $report (call $route_source (i32.const 1) (i32.const 32) (i32.const 13) (i32.const 160) (i32.const 21)))
           (call $report (call $route_source (i32.const 1) (i32.const 32) (i32.const 13) (i32.const 160) (i32.const 10)))
           (call $report (call $route_source (i32.const 0) (i32.const 32) (i32.const 13) (i32.const 160) (i32.const 21)))
           (call $report (call $route_source (i32.const 1) (i32.const 32) (i32.const 13) (i32.const 64) (i32.const 21)))"#,
        Fake::default(),
    );
    // The last one reads the RGB565 pattern config plus the sink bytes as
    // pins: a camera config must be JPEG.
    assert_eq!(p.device().results(), vec![0, BAD, BAD, BAD]);
    assert_eq!(
        p.device().calls,
        vec![
            "source Camera[5, 18, 19, 21, 36, 39, 34, 35, 25, 23, 22] app/cam/frame 240x240 500ms chunk=166"
        ]
    );
}

#[test]
fn r8_route_sink_passes_the_preamble_segments() {
    let mut p = run_init(SINK_OK, Fake::default());
    assert_eq!(p.device().results(), vec![0]);
    assert_eq!(
        p.device().calls,
        vec![
            "sink app/cam/frame bus=0 dc=13 frame=28800 C[2a] D[00, 3c, 00, b3] C[2b] D[00, 3c, 00, b3] C[2c]"
        ]
    );
}

#[test]
fn r8_bad_arguments_never_reach_the_device() {
    let mut p = run_init(
        r#"(call $report (call $route_source (i32.const 2) (i32.const 32) (i32.const 13) (i32.const 64) (i32.const 10)))
           (call $report (call $route_source (i32.const 0) (i32.const 32) (i32.const 13) (i32.const 64) (i32.const 9)))
           (call $report (call $route_source (i32.const 0) (i32.const 65530) (i32.const 13) (i32.const 64) (i32.const 10)))
           (call $report (call $route_source (i32.const 0) (i32.const 128) (i32.const 5) (i32.const 64) (i32.const 10)))
           (call $report (call $route_source (i32.const 0) (i32.const 32) (i32.const 13) (i32.const 80) (i32.const 10)))
           (call $report (call $route_sink (i32.const 32) (i32.const 13) (i32.const 1) (i32.const 80) (i32.const 28)))
           (call $report (call $route_sink (i32.const 32) (i32.const 13) (i32.const 0) (i32.const 80) (i32.const 27)))
           (call $report (call $route_sink (i32.const 32) (i32.const 13) (i32.const 0) (i32.const 65530) (i32.const 28)))
           (call $report (call $route_sink (i32.const 128) (i32.const 5) (i32.const 0) (i32.const 80) (i32.const 28)))
           (call $report (call $route_stop (i32.const 4)))
           (call $report (call $route_stop (i32.const -1)))
           (call $report (call $route_stat (i32.const 4) (i32.const 0)))
           (call $report (call $route_stat (i32.const 0) (i32.const 4)))"#,
        Fake::default(),
    );
    assert_eq!(
        p.device().results(),
        vec![
            BAD, BAD, OOB, KEY, BAD, BAD, BAD, OOB, KEY, BAD, BAD, BAD, BAD
        ]
    );
    assert!(p.device().calls.is_empty(), "{:?}", p.device().calls);
}

#[test]
fn r8_stop_and_stat_pass_through_and_counts_saturate() {
    let fake = Fake {
        stat: u32::MAX,
        ..Fake::default()
    };
    let mut p = run_init(
        r#"(call $report (call $route_stop (i32.const 3)))
           (call $report (call $route_stat (i32.const 1) (i32.const 0)))
           (call $report (call $route_stat (i32.const 1) (i32.const 3)))"#,
        fake,
    );
    assert_eq!(p.device().results(), vec![0, i32::MAX, i32::MAX]);
    assert_eq!(
        p.device().calls,
        vec!["stop 3", "stat 1 Frames", "stat 1 ChunksDropped"]
    );
}

#[test]
fn r8_device_errors_and_bad_handles_reach_the_payload_as_codes() {
    let calls = format!(
        "{SOURCE_OK} {SINK_OK}
         (call $report (call $route_stop (i32.const 0)))
         (call $report (call $route_stat (i32.const 0) (i32.const 1)))"
    );
    let busy = Fake {
        fail: Some(AbiError::Busy),
        ..Fake::default()
    };
    let mut p = run_init(&calls, busy);
    assert_eq!(p.device().results(), vec![BUSY; 4]);

    let rogue = Fake {
        handle: 4,
        ..Fake::default()
    };
    let mut p = run_init(&calls, rogue);
    assert_eq!(p.device().results()[..2], [NO_RES, NO_RES]);

    let p = run_init(&calls, Abi1Board(Vec::new()));
    assert_eq!(p.into_device().0, vec![UNSUPPORTED; 4]);
}

#[test]
fn r8_route_events_reach_on_irq() {
    let mut p = run_init("", Fake::default());
    for (kind, value) in [
        (EventKind::RouteFrames, 3),
        (EventKind::RouteDropped, 1),
        (EventKind::RouteStopped, AbiError::Transport.code()),
    ] {
        let delivered = p.dispatch(Event {
            kind,
            source: 2,
            value,
            time_us: 7,
        });
        assert_eq!(delivered, Ok(true));
    }
    assert_eq!(
        p.device().results(),
        vec![5, 2, 3, 6, 2, 1, 7, 2, AbiError::Transport.code()]
    );
}
