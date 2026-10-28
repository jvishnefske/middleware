//! Runs a payload that drives the virtual GC9A01 panel itself over SPI
//! bus 0 (no sink route), with `--panel-dc 13` naming its D/C pin: the
//! payload's CASET / RASET / RAMWR commands (D/C low) and their data (D/C
//! high) draw pixels that `--dump-panel` writes out.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const PANEL: usize = 240;
const PPM_HEADER: &[u8] = b"P6\n240 240\n255\n";

/// Opens SPI bus 0 and makes GPIO 13 an output, then sends the transfers
/// in the table at 64, one at a time: the first from `init`, each next one
/// from `on_irq` on a `DmaDone` (kind 3), since only one transfer may be in
/// flight. A table entry is four LE i32s: D/C level, tx pointer, tx length,
/// rx length. The script sets the window to columns 10..=12 of row 20 and
/// writes three pixels; the last transfer sends 5 bytes but clocks 6, so the
/// third pixel's low byte is the 0x00 padding.
const DRAW: &str = r#"
(module
  (import "mw" "spi_open" (func $spi_open (param i32 i32 i32 i32 i32 i32 i32) (result i32)))
  (import "mw" "spi_transfer" (func $spi_transfer (param i32 i32 i32 i32) (result i32)))
  (import "mw" "gpio_output" (func $gpio_output (param i32) (result i32)))
  (import "mw" "gpio_write" (func $gpio_write (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (global $step (mut i32) (i32.const 0))
  (data (i32.const 16) "\2a\2b\2c")
  (data (i32.const 20) "\00\0a\00\0c")
  (data (i32.const 24) "\00\14\00\14")
  (data (i32.const 28) "\f8\00\07\e0\ff")
  (data (i32.const 64)
    "\00\00\00\00\10\00\00\00\01\00\00\00\00\00\00\00"
    "\01\00\00\00\14\00\00\00\04\00\00\00\00\00\00\00"
    "\00\00\00\00\11\00\00\00\01\00\00\00\00\00\00\00"
    "\01\00\00\00\18\00\00\00\04\00\00\00\00\00\00\00"
    "\00\00\00\00\12\00\00\00\01\00\00\00\00\00\00\00"
    "\01\00\00\00\1c\00\00\00\05\00\00\00\06\00\00\00")
  (func $next
    (local $e i32)
    (if (i32.ge_u (global.get $step) (i32.const 6)) (then (return)))
    (local.set $e (i32.add (i32.const 64) (i32.shl (global.get $step) (i32.const 4))))
    (global.set $step (i32.add (global.get $step) (i32.const 1)))
    (drop (call $gpio_write (i32.const 13) (i32.load (local.get $e))))
    (drop (call $spi_transfer (i32.const 0)
      (i32.load offset=4 (local.get $e))
      (i32.load offset=8 (local.get $e))
      (i32.load offset=12 (local.get $e)))))
  (func (export "init")
    (drop (call $spi_open (i32.const 0) (i32.const 12) (i32.const 11) (i32.const -1)
                          (i32.const 10) (i32.const 40000) (i32.const 0)))
    (drop (call $gpio_output (i32.const 13)))
    (call $next))
  (func (export "on_irq") (param $kind i32) (param i32 i32 i32)
    (if (i32.eq (local.get $kind) (i32.const 3)) (then (call $next)))))
"#;

fn write_wasm(name: &str, wat_src: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::write(&path, wat::parse_str(wat_src).expect("valid WAT")).expect("write wasm");
    path
}

/// Runs the drawing payload in virtual time with `extra` arguments; returns
/// stdout and the dumped panel's pixels.
fn run(name: &str, extra: &[&str]) -> (String, Vec<u8>) {
    let wasm = write_wasm("panel_dc_draw.wasm", DRAW);
    let ppm = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_file(&ppm);
    let out = Command::new(env!("CARGO_BIN_EXE_payload-sim"))
        .arg(&wasm)
        .args(["--virtual", "--dump-panel"])
        .arg(&ppm)
        .args(extra)
        .stdin(Stdio::null())
        .output()
        .expect("payload-sim runs");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    (stdout, read_ppm(&ppm))
}

fn read_ppm(path: &Path) -> Vec<u8> {
    let bytes = std::fs::read(path).expect("panel dumped");
    assert!(bytes.starts_with(PPM_HEADER));
    let pixels = bytes[PPM_HEADER.len()..].to_vec();
    assert_eq!(pixels.len(), PANEL * PANEL * 3);
    pixels
}

fn at(ppm: &[u8], x: usize, y: usize) -> [u8; 3] {
    let i = (y * PANEL + x) * 3;
    [ppm[i], ppm[i + 1], ppm[i + 2]]
}

#[test]
fn panel_dc_payload_spi_draws_pixels() {
    let (stdout, ppm) = run("panel_dc.ppm", &["--panel-dc", "13"]);
    assert!(stdout.contains("spi0 tx 0x000a000c (4 bytes)"), "{stdout}");
    assert!(
        stdout.contains("spi0 tx 0xf80007e0ff (6 bytes)"),
        "{stdout}"
    );
    // RGB565 0xF800 red, 0x07E0 green, 0xFF00 (padded low byte) expanded.
    assert_eq!(at(&ppm, 10, 20), [0xFF, 0x00, 0x00]);
    assert_eq!(at(&ppm, 11, 20), [0x00, 0xFF, 0x00]);
    assert_eq!(at(&ppm, 12, 20), [0xFF, 0xE3, 0x00]);
    let lit = ppm.chunks(3).filter(|p| *p != [0, 0, 0]).count();
    assert_eq!(lit, 3, "only the window was drawn");
}

#[test]
fn without_panel_dc_the_panel_stays_black() {
    let (stdout, ppm) = run("panel_dc_off.ppm", &[]);
    assert!(stdout.contains("spi0 tx 0x000a000c (4 bytes)"), "{stdout}");
    assert!(ppm.iter().all(|&b| b == 0));
}
