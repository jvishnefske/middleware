//! Runs `payload-sim` on a WAT payload that uses the ABI 3 virtual
//! peripherals (requirement X7): an ADC value set with `!adc`, a PWM duty
//! derived from it, and a write / write-read round trip through the I2C
//! register device.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// On each message: reads ADC pin 3, reports it on `a` (4 bytes LE), sets
/// PWM 0.0 to `raw * 16`, then reads back two registers from the I2C device
/// (written in `init`) and publishes them on `i`; a transfer to the absent
/// address 0x51 reports its error code on `e`.
const ANALOG_I2C: &str = r#"
(module
  (import "mw" "publish" (func $publish (param i32 i32 i32 i32) (result i32)))
  (import "mw" "subscribe" (func $subscribe (param i32 i32) (result i32)))
  (import "mw" "pwm_open" (func $pwm_open (param i32 i32) (result i32)))
  (import "mw" "pwm_set" (func $pwm_set (param i32 i32 i32) (result i32)))
  (import "mw" "adc_read" (func $adc_read (param i32) (result i32)))
  (import "mw" "i2c_open" (func $i2c_open (param i32 i32) (result i32)))
  (import "mw" "i2c_transfer" (func $i2c_transfer (param i32 i32 i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "go")
  (data (i32.const 4) "a")
  (data (i32.const 5) "i")
  (data (i32.const 6) "e")
  (data (i32.const 16) "\00hi")
  (func (export "init")
    (drop (call $subscribe (i32.const 0) (i32.const 2)))
    (drop (call $pwm_open (i32.const 0) (i32.const 1000)))
    (drop (call $i2c_open (i32.const 0) (i32.const 100000)))
    (drop (call $i2c_transfer (i32.const 0) (i32.const 0x50) (i32.const 16) (i32.const 3) (i32.const 0) (i32.const 0))))
  (func (export "mw_alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "on_message") (param i32 i32 i32)
    (local $raw i32)
    (local.set $raw (call $adc_read (i32.const 3)))
    (i32.store (i32.const 32) (local.get $raw))
    (drop (call $publish (i32.const 4) (i32.const 1) (i32.const 32) (i32.const 4)))
    (drop (call $pwm_set (i32.const 0) (i32.const 0) (i32.mul (local.get $raw) (i32.const 16))))
    (drop (call $i2c_transfer (i32.const 0) (i32.const 0x50) (i32.const 16) (i32.const 1) (i32.const 64) (i32.const 2)))
    (drop (call $publish (i32.const 5) (i32.const 1) (i32.const 64) (i32.const 2)))
    (i32.store (i32.const 32)
      (call $i2c_transfer (i32.const 0) (i32.const 0x51) (i32.const 16) (i32.const 1) (i32.const 64) (i32.const 2)))
    (drop (call $publish (i32.const 6) (i32.const 1) (i32.const 32) (i32.const 4)))))
"#;

#[test]
fn x7_adc_pwm_and_i2c_in_the_simulator() {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("x7.wasm");
    std::fs::write(&path, wat::parse_str(ANALOG_I2C).expect("valid WAT")).expect("write wasm");
    let mut child = Command::new(env!("CARGO_BIN_EXE_payload-sim"))
        .arg(&path)
        .arg("--virtual")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn payload-sim");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"!adc 3 1234\ngo x\n")
        .expect("write stdin");
    let out = child.wait_with_output().expect("payload-sim runs");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "{stdout}");
    let lines: Vec<&str> = stdout
        .lines()
        .map(|l| l.split_once("] ").map_or(l, |(_, e)| e))
        .collect();
    for expected in [
        "pwm0 open 1000 Hz",
        "i2c0 open 100000 Hz",
        "i2c0 0x50 w 0x006869 r -",
        "adc 3 <- 1234",
        "publish a 0xd2040000",
        "pwm0.0 duty 19744",
        "i2c0 0x50 w 0x00 r hi",
        "publish i hi",
        "i2c0 0x51 w 0x00 Transport",
        "publish e 0xfbffffff",
    ] {
        assert!(
            lines.contains(&expected),
            "missing {expected:?} in {lines:?}"
        );
    }
}
