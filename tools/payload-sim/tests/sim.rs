//! Runs the `payload-sim` binary on WAT-built payloads (requirement W7).

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

/// Blinks pin 2 every 100 ms, echoes `in/msg` to `out`, logs on init.
const BLINK_ECHO: &str = r#"
(module
  (import "mw" "log" (func $log (param i32 i32)))
  (import "mw" "publish" (func $publish (param i32 i32 i32 i32) (result i32)))
  (import "mw" "subscribe" (func $subscribe (param i32 i32) (result i32)))
  (import "mw" "set_tick" (func $set_tick (param i32) (result i32)))
  (import "mw" "gpio_output" (func $gpio_output (param i32) (result i32)))
  (import "mw" "gpio_write" (func $gpio_write (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (global $on (mut i32) (i32.const 0))
  (data (i32.const 16) "hello sim")
  (data (i32.const 32) "in/msg")
  (data (i32.const 48) "out")
  (func (export "init")
    (call $log (i32.const 16) (i32.const 9))
    (drop (call $gpio_output (i32.const 2)))
    (drop (call $subscribe (i32.const 32) (i32.const 6)))
    (drop (call $set_tick (i32.const 100))))
  (func (export "tick") (param i32)
    (global.set $on (i32.xor (global.get $on) (i32.const 1)))
    (drop (call $gpio_write (i32.const 2) (global.get $on))))
  (func (export "mw_alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "on_message") (param i32) (param $p i32) (param $n i32)
    (drop (call $publish (i32.const 48) (i32.const 3) (local.get $p) (local.get $n)))))
"#;

fn write_wasm(name: &str, wat_src: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::write(&path, wat::parse_str(wat_src).expect("valid WAT")).expect("write wasm");
    path
}

fn sim(args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_payload-sim"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn payload-sim");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("payload-sim runs")
}

#[test]
fn w7_runs_payload_against_fake_device_in_virtual_time() {
    let wasm = write_wasm("blink_echo.wasm", BLINK_ECHO);
    let out = sim(
        &[wasm.to_str().unwrap(), "--virtual", "--ticks", "3"],
        "in/msg ping\nother/key x\n",
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines,
        vec![
            "[     0 ms] log hello sim",
            "[     0 ms] gpio 2 output",
            &format!(
                "loaded {} (subscriptions: [in/msg], next tick: Some(100))",
                wasm.display()
            ),
            "[     0 ms] publish out ping",
            "[     0 ms] drop other/key",
            "[   100 ms] gpio 2 = high",
            "[   200 ms] gpio 2 = low",
            "[   300 ms] gpio 2 = high",
        ]
    );
}

#[test]
fn w7_real_time_ticks() {
    let wasm = write_wasm("blink_rt.wasm", BLINK_ECHO);
    let out = sim(&[wasm.to_str().unwrap(), "--ticks", "2"], "");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert_eq!(stdout.matches("gpio 2 = ").count(), 2, "{stdout}");
}

#[test]
fn w7_reports_traps_and_load_errors() {
    let looping = r#"(module
        (import "mw" "set_tick" (func $t (param i32) (result i32)))
        (memory (export "memory") 1)
        (func (export "init") (drop (call $t (i32.const 10))))
        (func (export "tick") (param i32) (loop $l (br $l))))"#;
    let wasm = write_wasm("loop.wasm", looping);
    let out = sim(
        &[
            wasm.to_str().unwrap(),
            "--virtual",
            "--ticks",
            "1",
            "--fuel",
            "5000",
        ],
        "",
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stdout).contains("trap: out of fuel"));

    let bad = write_wasm(
        "bad.wasm",
        r#"(module (import "env" "x" (func)) (memory (export "memory") 1))"#,
    );
    let out = sim(&[bad.to_str().unwrap(), "--virtual"], "");
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown import env.x"));
}
