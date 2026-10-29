//! End-to-end tests of the ABI 3 imports (PWM, ADC, I2C; requirements
//! X3–X5), with WAT payloads and a fake device: PWM calls are recorded, ADC
//! readings are scripted per pin, and I2C talks to a register device at
//! address 0x50 (write `[reg, data...]` stores from `reg`; a read returns
//! registers from the last written `reg`, auto-incrementing).

use std::collections::BTreeMap;

use middleware::Key;
use middleware::wasm_abi::periph::{ADC_MAX, MAX_I2C};
use middleware::wasm_abi::{ABI1_IMPORTS, ABI2_IMPORTS, IMPORTS};
use wasm_host::{AbiError, Device, I2cConfig, Limits, Payload, Pull, PwmSpec};

/// Address of the fake register device.
const REG_DEV: u8 = 0x50;

#[derive(Default)]
struct Fake {
    /// Error every ABI 3 call returns instead, when set.
    fail: Option<AbiError>,
    adc: BTreeMap<u8, u16>,
    regs: Vec<u8>,
    reg_ptr: usize,
    calls: Vec<String>,
    published: Vec<(String, Vec<u8>)>,
}

impl Fake {
    fn new() -> Self {
        Fake {
            regs: vec![0; 256],
            ..Fake::default()
        }
    }

    fn results(&self) -> Vec<i32> {
        self.data("r")
            .iter()
            .map(|d| i32::from_le_bytes(d.as_slice().try_into().expect("4 bytes")))
            .collect()
    }

    fn data(&self, key: &str) -> Vec<Vec<u8>> {
        self.published
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, d)| d.clone())
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
    fn pwm_open(&mut self, s: PwmSpec) -> Result<(), AbiError> {
        self.outcome(format!("pwm_open {} {}", s.timer, s.freq_hz))
    }
    fn pwm_set(&mut self, timer: u8, channel: u8, duty: u16) -> Result<(), AbiError> {
        self.outcome(format!("pwm_set {timer} {channel} {duty}"))
    }
    fn pwm_stop(&mut self, timer: u8) -> Result<(), AbiError> {
        self.outcome(format!("pwm_stop {timer}"))
    }
    fn adc_read(&mut self, pin: u8) -> Result<u16, AbiError> {
        self.outcome(format!("adc_read {pin}"))?;
        self.adc.get(&pin).copied().ok_or(AbiError::Unsupported)
    }
    fn i2c_open(&mut self, c: I2cConfig) -> Result<(), AbiError> {
        self.outcome(format!("i2c_open {} {}", c.bus, c.freq_hz))
    }
    fn i2c_transfer(
        &mut self,
        bus: u8,
        addr: u8,
        tx: &[u8],
        rx: &mut [u8],
    ) -> Result<(), AbiError> {
        self.outcome(format!(
            "i2c_transfer {bus} {addr:#x} {tx:?} rx={}",
            rx.len()
        ))?;
        if addr != REG_DEV {
            return Err(AbiError::Transport);
        }
        if let Some((&reg, data)) = tx.split_first() {
            self.reg_ptr = usize::from(reg);
            for (i, b) in data.iter().enumerate() {
                self.regs[(self.reg_ptr + i) % 256] = *b;
            }
        }
        for (i, b) in rx.iter_mut().enumerate() {
            *b = self.regs[(self.reg_ptr + i) % 256];
        }
        Ok(())
    }
}

/// Implements only the ABI 1 methods, so every ABI 3 method is a default.
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

/// The ABI 3 imports plus `$report` (publishes an i32 on key `r`, stored at
/// offset 0; value scratch at 4). Data: "d" at 8, transmit bytes at 16.
const PRELUDE: &str = r#"
  (import "mw" "publish" (func $publish (param i32 i32 i32 i32) (result i32)))
  (import "mw" "pwm_open" (func $pwm_open (param i32 i32) (result i32)))
  (import "mw" "pwm_set" (func $pwm_set (param i32 i32 i32) (result i32)))
  (import "mw" "pwm_stop" (func $pwm_stop (param i32) (result i32)))
  (import "mw" "adc_read" (func $adc_read (param i32) (result i32)))
  (import "mw" "i2c_open" (func $i2c_open (param i32 i32) (result i32)))
  (import "mw" "i2c_transfer" (func $i2c_transfer (param i32 i32 i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "r")
  (data (i32.const 8) "d")
  (data (i32.const 16) "\10hi!")
  (func $report (param $v i32)
    (i32.store (i32.const 4) (local.get $v))
    (drop (call $publish (i32.const 0) (i32.const 1) (i32.const 4) (i32.const 4))))
  (func $show (param $ptr i32) (param $len i32)
    (drop (call $publish (i32.const 8) (i32.const 1) (local.get $ptr) (local.get $len))))
"#;

fn module(body: &str) -> Vec<u8> {
    wat::parse_str(format!("(module {PRELUDE} {body})")).expect("valid WAT")
}

/// Runs `calls` in `init` against `device`.
fn run_init<D: Device + 'static>(calls: &str, device: D) -> Payload<D> {
    let wasm = module(&format!(r#"(func (export "init") {calls})"#));
    Payload::load(&wasm, device, Limits::default()).expect("payload loads")
}

const BAD: i32 = AbiError::InvalidArgument.code();
const OOB: i32 = AbiError::OutOfBounds.code();
const UNSUPPORTED: i32 = AbiError::Unsupported.code();
const TRANSPORT: i32 = AbiError::Transport.code();
const BUSY: i32 = AbiError::Busy.code();

/// A module importing `imports` (with their table signatures) and nothing
/// else.
fn importing(imports: &[middleware::wasm_abi::Import]) -> Vec<u8> {
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

// --------------------------------------------------------- compatibility

#[test]
fn x1_every_import_links_and_older_revisions_still_load() {
    for (what, imports) in [
        ("ABI 4 (all)", IMPORTS),
        ("ABI 2", &IMPORTS[..ABI2_IMPORTS]),
        ("ABI 1", &IMPORTS[..ABI1_IMPORTS]),
    ] {
        let p = Payload::load(&importing(imports), Fake::new(), Limits::default());
        assert!(p.is_ok(), "{what}: {:?}", p.err());
        let p = Payload::load(
            &importing(imports),
            Abi1Board(Vec::new()),
            Limits::default(),
        );
        assert!(p.is_ok(), "{what} on an ABI 1 board: {:?}", p.err());
    }
}

// ------------------------------------------------------------------- PWM

#[test]
fn x3_pwm_open_set_stop_and_bad_args() {
    let mut p = run_init(
        r#"(call $report (call $pwm_open (i32.const 5) (i32.const 20000)))
           (call $report (call $pwm_set (i32.const 5) (i32.const 3) (i32.const 65535)))
           (call $report (call $pwm_set (i32.const 5) (i32.const 0) (i32.const 0)))
           (call $report (call $pwm_stop (i32.const 5)))
           (call $report (call $pwm_open (i32.const 6) (i32.const 1000)))
           (call $report (call $pwm_open (i32.const 0) (i32.const 0)))
           (call $report (call $pwm_open (i32.const 0) (i32.const 40000001)))
           (call $report (call $pwm_set (i32.const 0) (i32.const 4) (i32.const 1)))
           (call $report (call $pwm_set (i32.const 0) (i32.const 0) (i32.const 65536)))
           (call $report (call $pwm_set (i32.const 0) (i32.const 0) (i32.const -1)))
           (call $report (call $pwm_set (i32.const -1) (i32.const 0) (i32.const 1)))
           (call $report (call $pwm_stop (i32.const 6)))"#,
        Fake::new(),
    );
    assert_eq!(
        p.device().results(),
        vec![0, 0, 0, 0, BAD, BAD, BAD, BAD, BAD, BAD, BAD, BAD]
    );
    assert_eq!(
        p.device().calls,
        vec![
            "pwm_open 5 20000",
            "pwm_set 5 3 65535",
            "pwm_set 5 0 0",
            "pwm_stop 5"
        ]
    );
}

// ------------------------------------------------------------------- ADC

#[test]
fn x4_adc_read_returns_raw_values_clamped_and_errors() {
    let mut fake = Fake::new();
    fake.adc.insert(0, 0);
    fake.adc.insert(3, 1234);
    fake.adc.insert(4, ADC_MAX);
    fake.adc.insert(7, u16::MAX);
    let mut p = run_init(
        r#"(call $report (call $adc_read (i32.const 3)))
           (call $report (call $adc_read (i32.const 0)))
           (call $report (call $adc_read (i32.const 4)))
           (call $report (call $adc_read (i32.const 7)))
           (call $report (call $adc_read (i32.const 9)))
           (call $report (call $adc_read (i32.const 256)))
           (call $report (call $adc_read (i32.const -1)))"#,
        fake,
    );
    assert_eq!(
        p.device().results(),
        vec![1234, 0, 4095, 4095, UNSUPPORTED, BAD, BAD]
    );
    assert_eq!(p.device().calls.len(), 5, "bad pins never reach the device");
}

// ------------------------------------------------------------------- I2C

#[test]
fn x5_i2c_open_and_bad_config() {
    let mut p = run_init(
        r#"(call $report (call $i2c_open (i32.const 0) (i32.const 100000)))
           (call $report (call $i2c_open (i32.const 1) (i32.const 400000)))
           (call $report (call $i2c_open (i32.const 2) (i32.const 100000)))
           (call $report (call $i2c_open (i32.const 0) (i32.const 0)))
           (call $report (call $i2c_open (i32.const 0) (i32.const 1000001)))"#,
        Fake::new(),
    );
    assert_eq!(p.device().results(), vec![0, 0, BAD, BAD, BAD]);
    assert_eq!(
        p.device().calls,
        vec!["i2c_open 0 100000", "i2c_open 1 400000"]
    );
}

#[test]
fn x5_i2c_write_then_write_read_round_trips_data() {
    // Write "\x10hi!" (register 0x10 <- "hi!"), then write register 0x10 and
    // read 3 bytes back into offset 64, publish them on "d"; then a
    // read-only transfer continues from the same register.
    let mut p = run_init(
        r#"(call $report (call $i2c_transfer (i32.const 0) (i32.const 0x50) (i32.const 16) (i32.const 4) (i32.const 0) (i32.const 0)))
           (call $report (call $i2c_transfer (i32.const 0) (i32.const 0x50) (i32.const 16) (i32.const 1) (i32.const 64) (i32.const 3)))
           (call $show (i32.const 64) (i32.const 3))
           (call $report (call $i2c_transfer (i32.const 0) (i32.const 0x50) (i32.const 0) (i32.const 0) (i32.const 80) (i32.const 2)))
           (call $show (i32.const 80) (i32.const 2))"#,
        Fake::new(),
    );
    assert_eq!(p.device().results(), vec![0, 0, 0]);
    assert_eq!(p.device().data("d"), vec![b"hi!".to_vec(), b"hi".to_vec()]);
    assert_eq!(
        p.device().calls,
        vec![
            "i2c_transfer 0 0x50 [16, 104, 105, 33] rx=0",
            "i2c_transfer 0 0x50 [16] rx=3",
            "i2c_transfer 0 0x50 [] rx=2"
        ]
    );
}

#[test]
fn x5_i2c_errors_bounds_and_untouched_memory() {
    // Offset 96 holds "zz"; failing transfers that target it must leave it.
    let mut p = run_init(
        &format!(
            r#"(i32.store16 (i32.const 96) (i32.const 0x7a7a))
               (call $report (call $i2c_transfer (i32.const 0) (i32.const 0x51) (i32.const 16) (i32.const 1) (i32.const 96) (i32.const 2)))
               (call $report (call $i2c_transfer (i32.const 0) (i32.const 0x80) (i32.const 16) (i32.const 1) (i32.const 96) (i32.const 2)))
               (call $report (call $i2c_transfer (i32.const 2) (i32.const 0x50) (i32.const 16) (i32.const 1) (i32.const 96) (i32.const 2)))
               (call $report (call $i2c_transfer (i32.const 0) (i32.const 0x50) (i32.const 16) (i32.const {over}) (i32.const 96) (i32.const 0)))
               (call $report (call $i2c_transfer (i32.const 0) (i32.const 0x50) (i32.const 16) (i32.const 1) (i32.const 96) (i32.const {over})))
               (call $report (call $i2c_transfer (i32.const 0) (i32.const 0x50) (i32.const 16) (i32.const -1) (i32.const 96) (i32.const 0)))
               (call $report (call $i2c_transfer (i32.const 0) (i32.const 0x50) (i32.const 65535) (i32.const 4) (i32.const 96) (i32.const 2)))
               (call $report (call $i2c_transfer (i32.const 0) (i32.const 0x50) (i32.const 16) (i32.const 1) (i32.const 65535) (i32.const 2)))
               (call $report (call $i2c_transfer (i32.const 0) (i32.const 0x50) (i32.const -16) (i32.const 1) (i32.const 96) (i32.const 2)))
               (call $report (call $i2c_transfer (i32.const 0) (i32.const 0x50) (i32.const 16) (i32.const 1) (i32.const 65537) (i32.const 0)))
               (call $report (call $i2c_transfer (i32.const 0) (i32.const 0x50) (i32.const 16) (i32.const 1) (i32.const 65536) (i32.const 0)))
               (call $report (call $i2c_transfer (i32.const 0) (i32.const 0x50) (i32.const 0) (i32.const {max}) (i32.const 128) (i32.const {max})))
               (call $show (i32.const 96) (i32.const 2))"#,
            over = MAX_I2C + 1,
            max = MAX_I2C
        ),
        Fake::new(),
    );
    assert_eq!(
        p.device().results(),
        vec![TRANSPORT, BAD, BAD, BAD, BAD, BAD, OOB, OOB, OOB, OOB, 0, 0]
    );
    assert_eq!(p.device().data("d"), vec![b"zz".to_vec()]);
    let calls = &p.device().calls;
    assert_eq!(
        calls.len(),
        3,
        "the NACKed one, the empty read at the end of memory, the full-size one"
    );
    assert_eq!(calls[1], "i2c_transfer 0 0x50 [16] rx=0");
    assert!(calls[2].ends_with(&format!("rx={MAX_I2C}")));
    assert!(!p.is_poisoned());
}

// ------------------------------------------------------- device outcomes

#[test]
fn x2_device_errors_pass_through_and_defaults_are_unsupported() {
    let calls = r#"(call $report (call $pwm_open (i32.const 0) (i32.const 1000)))
       (call $report (call $pwm_set (i32.const 0) (i32.const 0) (i32.const 1)))
       (call $report (call $pwm_stop (i32.const 0)))
       (call $report (call $adc_read (i32.const 0)))
       (call $report (call $i2c_open (i32.const 0) (i32.const 100000)))
       (call $report (call $i2c_transfer (i32.const 0) (i32.const 0x50) (i32.const 16) (i32.const 1) (i32.const 64) (i32.const 1)))"#;
    let busy = Fake {
        fail: Some(AbiError::Busy),
        ..Fake::new()
    };
    let mut p = run_init(calls, busy);
    assert_eq!(p.device().results(), vec![BUSY; 6]);

    let p = run_init(calls, Abi1Board(Vec::new()));
    assert_eq!(p.into_device().0, vec![UNSUPPORTED; 6]);
}
