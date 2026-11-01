//! Runs `payload-sim` on a WAT payload that uses the ABI 5 LED strips
//! (requirement V6): it opens a strip, writes a frame, shows that the data
//! pin is refused as GPIO and that a frame longer than the strip is
//! refused, and leaves the strip open so the run ends with its LEDs.

use std::path::PathBuf;
use std::process::Command;

/// In `init`: opens 3 LEDs on pin 5, writes red, green, blue (G, R, B wire
/// bytes), then a one-LED frame of white; publishes the error codes of
/// `gpio_output(5)` on `g` and of a 4-LED frame on `f`.
const STRIP: &str = r#"
(module
  (import "mw" "publish" (func $publish (param i32 i32 i32 i32) (result i32)))
  (import "mw" "gpio_output" (func $gpio_output (param i32) (result i32)))
  (import "mw" "ledstrip_open" (func $open (param i32 i32) (result i32)))
  (import "mw" "ledstrip_write" (func $write (param i32 i32 i32) (result i32)))
  (import "mw" "ledstrip_close" (func $close (param i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "g")
  (data (i32.const 1) "f")
  (data (i32.const 16) "\00\ff\00\ff\00\00\00\00\ff")
  (data (i32.const 32) "\ff\ff\ff")
  (func (export "init")
    (local $h i32)
    (local.set $h (call $open (i32.const 5) (i32.const 3)))
    (drop (call $write (local.get $h) (i32.const 16) (i32.const 9)))
    (drop (call $write (local.get $h) (i32.const 32) (i32.const 3)))
    (i32.store (i32.const 64) (call $gpio_output (i32.const 5)))
    (drop (call $publish (i32.const 0) (i32.const 1) (i32.const 64) (i32.const 4)))
    (i32.store (i32.const 64) (call $write (local.get $h) (i32.const 16) (i32.const 12)))
    (drop (call $publish (i32.const 1) (i32.const 1) (i32.const 64) (i32.const 4)))
    ;; A second strip, opened and closed again.
    (drop (call $close (call $open (i32.const 6) (i32.const 1))))))
"#;

#[test]
fn v6_ledstrip_frames_in_the_simulator() {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("v6.wasm");
    std::fs::write(&path, wat::parse_str(STRIP).expect("valid WAT")).expect("write wasm");
    let out = Command::new(env!("CARGO_BIN_EXE_payload-sim"))
        .arg(&path)
        .arg("--virtual")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("payload-sim runs");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "{stdout}");
    let lines: Vec<&str> = stdout
        .lines()
        .map(|l| l.split_once("] ").map_or(l, |(_, e)| e))
        .collect();
    let expected = [
        "ledstrip0 open pin 5 3 leds",
        "ledstrip0 #ff0000 #00ff00 #0000ff",
        "ledstrip0 #ffffff",
        // InvalidArgument (-1) twice: the pin is the strip's, the frame
        // is one LED too long.
        "publish g 0xffffffff",
        "publish f 0xffffffff",
        "ledstrip1 open pin 6 1 leds",
        "ledstrip1 close",
        "ledstrip0 pin 5: #ffffff #00ff00 #0000ff",
    ];
    let found: Vec<usize> = expected
        .iter()
        .map(|e| {
            lines
                .iter()
                .position(|l| l == e)
                .unwrap_or_else(|| panic!("missing {e:?} in {lines:?}"))
        })
        .collect();
    assert!(found.is_sorted(), "out of order: {lines:?}");
    assert!(
        !lines.iter().any(|l| l.starts_with("ledstrip1 pin")),
        "a closed strip has no summary: {lines:?}"
    );
}
