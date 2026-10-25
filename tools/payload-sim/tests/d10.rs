//! Runs `payload-sim` on WAT payloads that use the ABI 2 virtual
//! peripherals (requirement D10, desktop part): timers in virtual time,
//! GPIO edges, SPI loopback and the UART terminal.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn write_wasm(name: &str, wat_src: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::write(&path, wat::parse_str(wat_src).expect("valid WAT")).expect("write wasm");
    path
}

/// Runs the simulator in virtual time and returns stdout lines after the
/// `loaded ...` banner, asserting a clean exit.
fn sim_virtual(wasm: &PathBuf, extra: &[&str], stdin: &str) -> Vec<String> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_payload-sim"))
        .arg(wasm)
        .arg("--virtual")
        .args(extra)
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
    let out = child.wait_with_output().expect("payload-sim runs");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
        .lines()
        .skip_while(|l| !l.starts_with("loaded "))
        .skip(1)
        .map(str::to_string)
        .collect()
}

/// Starts periodic timer 0 at 1 ms; `on_irq` publishes the running count on
/// `t` as one ASCII digit and stops the timer at 5.
const TIMER_COUNT: &str = r#"
(module
  (import "mw" "publish" (func $publish (param i32 i32 i32 i32) (result i32)))
  (import "mw" "timer_start" (func $timer_start (param i32 i32 i32) (result i32)))
  (import "mw" "timer_stop" (func $timer_stop (param i32) (result i32)))
  (memory (export "memory") 1)
  (global $count (mut i32) (i32.const 0))
  (data (i32.const 0) "t")
  (func (export "init")
    (drop (call $timer_start (i32.const 0) (i32.const 1000) (i32.const 1))))
  (func (export "on_irq") (param $kind i32) (param $src i32) (param $val i32) (param $t i32)
    (if (i32.ne (local.get $kind) (i32.const 2)) (then (return)))
    (global.set $count (i32.add (global.get $count) (local.get $val)))
    (i32.store8 (i32.const 8) (i32.add (i32.const 48) (global.get $count)))
    (drop (call $publish (i32.const 0) (i32.const 1) (i32.const 8) (i32.const 1)))
    (if (i32.ge_u (global.get $count) (i32.const 5))
      (then (drop (call $timer_stop (i32.const 0)))))))
"#;

#[test]
fn d10_timer_counts_to_n_in_virtual_time() {
    let wasm = write_wasm("d10_timer.wasm", TIMER_COUNT);
    let lines = sim_virtual(&wasm, &[], "");
    assert_eq!(
        lines,
        vec![
            "[     1 ms] publish t 1",
            "[     2 ms] publish t 2",
            "[     3 ms] publish t 3",
            "[     4 ms] publish t 4",
            "[     5 ms] publish t 5",
        ]
    );
}

#[test]
fn d10_until_ms_bounds_a_periodic_timer() {
    // Same payload but never reaching the stop: --until-ms ends the run.
    let wasm = write_wasm("d10_timer_until.wasm", TIMER_COUNT);
    let lines = sim_virtual(&wasm, &["--until-ms", "3"], "");
    assert_eq!(lines.len(), 3, "{lines:?}");
}

/// Pin 4 input with pull-down and a rising-edge interrupt; `on_irq`
/// publishes `<pin digit><level digit>` on `g`.
const GPIO_EDGE: &str = r#"
(module
  (import "mw" "publish" (func $publish (param i32 i32 i32 i32) (result i32)))
  (import "mw" "gpio_input" (func $gpio_input (param i32 i32) (result i32)))
  (import "mw" "gpio_irq" (func $gpio_irq (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "g")
  (func (export "init")
    (drop (call $gpio_input (i32.const 4) (i32.const 2)))
    (drop (call $gpio_irq (i32.const 4) (i32.const 1))))
  (func (export "on_irq") (param $kind i32) (param $src i32) (param $val i32) (param $t i32)
    (if (i32.ne (local.get $kind) (i32.const 1)) (then (return)))
    (i32.store8 (i32.const 8) (i32.add (i32.const 48) (local.get $src)))
    (i32.store8 (i32.const 9) (i32.add (i32.const 48) (local.get $val)))
    (drop (call $publish (i32.const 0) (i32.const 1) (i32.const 8) (i32.const 2)))))
"#;

#[test]
fn d10_gpio_edge_reaches_on_irq() {
    let wasm = write_wasm("d10_gpio.wasm", GPIO_EDGE);
    let lines = sim_virtual(&wasm, &[], "!gpio 4 1\n!gpio 4 0\n!gpio 4 1\n!gpio 5 1\n");
    let published: Vec<&String> = lines.iter().filter(|l| l.contains("publish")).collect();
    assert_eq!(
        published,
        vec!["[     0 ms] publish g 41", "[     0 ms] publish g 41"],
        "{lines:?}"
    );
}

/// Opens SPI bus 0 and sends "ping" in `init`; on DmaDone(Spi0) reads the
/// received bytes into 256.. and publishes them on `s`.
const SPI_LOOPBACK: &str = r#"
(module
  (import "mw" "publish" (func $publish (param i32 i32 i32 i32) (result i32)))
  (import "mw" "spi_open" (func $spi_open (param i32 i32 i32 i32 i32 i32 i32) (result i32)))
  (import "mw" "spi_transfer" (func $spi_transfer (param i32 i32 i32 i32) (result i32)))
  (import "mw" "dma_read" (func $dma_read (param i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "s")
  (data (i32.const 16) "ping")
  (func (export "init")
    (drop (call $spi_open (i32.const 0) (i32.const 6) (i32.const 7) (i32.const 8)
                          (i32.const -1) (i32.const 1000) (i32.const 0)))
    (drop (call $spi_transfer (i32.const 0) (i32.const 16) (i32.const 4) (i32.const 4))))
  (func (export "on_irq") (param $kind i32) (param $src i32) (param $val i32) (param $t i32)
    (local $n i32)
    (if (i32.ne (local.get $kind) (i32.const 3)) (then (return)))
    (local.set $n (call $dma_read (local.get $src) (i32.const 256) (i32.const 64)))
    (drop (call $publish (i32.const 0) (i32.const 1) (i32.const 256) (local.get $n)))))
"#;

#[test]
fn d10_spi_loopback_round_trip_through_dma_read() {
    let wasm = write_wasm("d10_spi.wasm", SPI_LOOPBACK);
    let lines = sim_virtual(&wasm, &[], "");
    assert_eq!(lines, vec!["[     0 ms] publish s ping"]);

    let lines = sim_virtual(&wasm, &["--spi-pattern", "4142"], "");
    assert_eq!(lines, vec!["[     0 ms] publish s ABAB"]);
}

/// Opens UART1; echoes every received chunk back and logs `sent` when the
/// transmit DMA completes.
const UART_ECHO: &str = r#"
(module
  (import "mw" "log" (func $log (param i32 i32)))
  (import "mw" "uart_open" (func $uart_open (param i32 i32 i32 i32) (result i32)))
  (import "mw" "uart_read" (func $uart_read (param i32 i32 i32) (result i32)))
  (import "mw" "uart_write" (func $uart_write (param i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "sent")
  (func (export "init")
    (drop (call $uart_open (i32.const 1) (i32.const 4) (i32.const 5) (i32.const 115200))))
  (func (export "on_irq") (param $kind i32) (param $src i32) (param $val i32) (param $t i32)
    (local $n i32)
    (if (i32.eq (local.get $kind) (i32.const 4))
      (then
        (local.set $n (call $uart_read (i32.const 1) (i32.const 512) (i32.const 128)))
        (drop (call $uart_write (i32.const 1) (i32.const 512) (local.get $n)))))
    (if (i32.and (i32.eq (local.get $kind) (i32.const 3)) (i32.eq (local.get $src) (i32.const 1)))
      (then (call $log (i32.const 0) (i32.const 4))))))
"#;

#[test]
fn d10_uart_echo() {
    let wasm = write_wasm("d10_uart.wasm", UART_ECHO);
    let lines = sim_virtual(&wasm, &[], "!uart hello\n");
    assert_eq!(
        lines,
        vec![
            "[     0 ms] uart1 rx hello",
            "[     0 ms] uart1 tx hello",
            "[     0 ms] log sent",
        ]
    );
}
