//! End-to-end tests of the ABI 5 LED-strip imports (requirements V1–V3),
//! with WAT payloads and a fake device that records each strip call with
//! its parsed arguments and the frame bytes it received.

use middleware::Key;
use middleware::wasm_abi::{ABI4_IMPORTS, IMPORTS, Import};
use wasm_host::{AbiError, Device, LedStripSpec, Limits, Payload, Pull};

#[derive(Default)]
struct Fake {
    /// Error every strip call returns instead, when set.
    fail: Option<AbiError>,
    /// Handle the next ledstrip_open returns.
    handle: u8,
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
    fn ledstrip_open(&mut self, spec: LedStripSpec) -> Result<u8, AbiError> {
        self.outcome(format!("open pin={} count={}", spec.pin, spec.count.get()))?;
        Ok(self.handle)
    }
    fn ledstrip_write(&mut self, handle: u8, grb: &[u8]) -> Result<(), AbiError> {
        self.outcome(format!("write {handle} {grb:02x?}"))
    }
    fn ledstrip_close(&mut self, handle: u8) -> Result<(), AbiError> {
        self.outcome(format!("close {handle}"))
    }
}

/// Implements only the ABI 1 methods, so every strip method is a default.
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

/// The strip imports plus `$report` (publishes an i32 on key `r`). Data:
/// "r" at 0 and a two-LED GRB frame (red, then blue) at 16.
const PRELUDE: &str = r#"
  (import "mw" "publish" (func $publish (param i32 i32 i32 i32) (result i32)))
  (import "mw" "ledstrip_open" (func $open (param i32 i32) (result i32)))
  (import "mw" "ledstrip_write" (func $write (param i32 i32 i32) (result i32)))
  (import "mw" "ledstrip_close" (func $close (param i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "r")
  (data (i32.const 16) "\00\ff\00\00\00\ff")
  (func $report (param $v i32)
    (i32.store (i32.const 4) (local.get $v))
    (drop (call $publish (i32.const 0) (i32.const 1) (i32.const 4) (i32.const 4))))
"#;

fn run_init<D: Device + 'static>(calls: &str, device: D) -> Payload<D> {
    let wasm = wat::parse_str(format!(
        r#"(module {PRELUDE} (func (export "init") {calls}))"#
    ))
    .expect("valid WAT");
    Payload::load(&wasm, device, Limits::default()).expect("payload loads")
}

const BAD: i32 = AbiError::InvalidArgument.code();
const OOB: i32 = AbiError::OutOfBounds.code();
const UNSUPPORTED: i32 = AbiError::Unsupported.code();
const NO_RES: i32 = AbiError::NoResources.code();
const BUSY: i32 = AbiError::Busy.code();

const OPEN: &str = "(call $report (call $open (i32.const 45) (i32.const 8)))";
const WRITE: &str = "(call $report (call $write (i32.const 0) (i32.const 16) (i32.const 6)))";
const CLOSE: &str = "(call $report (call $close (i32.const 0)))";

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
fn v1_every_import_links_and_abi4_payloads_still_load() {
    assert_eq!(IMPORTS.len(), ABI4_IMPORTS + 3);
    for (what, imports) in [("ABI 5", IMPORTS), ("ABI 4", &IMPORTS[..ABI4_IMPORTS])] {
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
fn v1_wrong_signatures_are_refused() {
    let wasm = wat::parse_str(
        r#"(module (import "mw" "ledstrip_write" (func (param i32 i32) (result i32)))
                   (memory (export "memory") 1))"#,
    )
    .expect("WAT");
    assert!(Payload::load(&wasm, Fake::default(), Limits::default()).is_err());
}

#[test]
fn v2_open_write_close_pass_parsed_arguments_and_frame_bytes() {
    let fake = Fake {
        handle: 1,
        ..Fake::default()
    };
    let mut p = run_init(&format!("{OPEN} {WRITE} {CLOSE}"), fake);
    assert_eq!(p.device().results(), vec![1, 0, 0]);
    assert_eq!(
        p.device().calls,
        vec![
            "open pin=45 count=8",
            "write 0 [00, ff, 00, 00, 00, ff]",
            "close 0",
        ]
    );
}

#[test]
fn v2_bad_arguments_never_reach_the_device() {
    let mut p = run_init(
        r#"(call $report (call $open (i32.const -1) (i32.const 8)))
           (call $report (call $open (i32.const 256) (i32.const 8)))
           (call $report (call $open (i32.const 45) (i32.const 0)))
           (call $report (call $open (i32.const 45) (i32.const 65)))
           (call $report (call $write (i32.const 2) (i32.const 16) (i32.const 6)))
           (call $report (call $write (i32.const 0) (i32.const 16) (i32.const 5)))
           (call $report (call $write (i32.const 0) (i32.const 16) (i32.const 0)))
           (call $report (call $write (i32.const 0) (i32.const 16) (i32.const 195)))
           (call $report (call $write (i32.const 0) (i32.const 65532) (i32.const 6)))
           (call $report (call $write (i32.const 0) (i32.const -1) (i32.const 6)))
           (call $report (call $close (i32.const 2)))
           (call $report (call $close (i32.const -1)))"#,
        Fake::default(),
    );
    assert_eq!(
        p.device().results(),
        vec![BAD, BAD, BAD, BAD, BAD, BAD, BAD, BAD, OOB, OOB, BAD, BAD]
    );
    assert!(p.device().calls.is_empty(), "{:?}", p.device().calls);
}

#[test]
fn v2_device_errors_rogue_handles_and_defaults_reach_the_payload_as_codes() {
    let calls = format!("{OPEN} {WRITE} {CLOSE}");
    let busy = Fake {
        fail: Some(AbiError::Busy),
        ..Fake::default()
    };
    let mut p = run_init(&calls, busy);
    assert_eq!(p.device().results(), vec![BUSY; 3]);

    let rogue = Fake {
        handle: 2,
        ..Fake::default()
    };
    let mut p = run_init(OPEN, rogue);
    assert_eq!(p.device().results(), vec![NO_RES]);

    let wasm = wat::parse_str(format!(
        r#"(module {PRELUDE} (func (export "init") {calls}))"#
    ))
    .expect("valid WAT");
    let mut p = Payload::load(&wasm, Abi1Board(Vec::new()), Limits::default()).expect("loads");
    assert_eq!(p.device().0, vec![UNSUPPORTED; 3]);
}
