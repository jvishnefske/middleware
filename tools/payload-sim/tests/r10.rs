//! Runs a camera payload and a display payload together in `payload-sim`
//! (`--peer`), with DMA routes (requirement R10): a 120×120 test-pattern
//! source route on one board feeds a sink route drawing a centred window
//! on the other board's virtual 240×240 panel, dumped with `--dump-panel`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use middleware::stream::pattern;

const SIDE: u16 = 120;
/// Window origin on the panel (centred).
const ORIGIN: u16 = 60;
const PANEL: usize = 240;
const PPM_HEADER: &[u8] = b"P6\n240 240\n255\n";

/// A 120×120 RGB565 test-pattern source route on `app/cam/frame`, a frame
/// every 100 ms in 166-byte chunks (chunk size 0 = default). A camera
/// source with a well-formed config (120×120 JPEG and the AI-Thinker
/// pins, 21 bytes) must be refused, as the simulator has no image sensor:
/// its result is published on `cam` (4 bytes LE).
const CAMERA: &str = r#"
(module
  (import "mw" "publish" (func $publish (param i32 i32 i32 i32) (result i32)))
  (import "mw" "route_source" (func $route_source (param i32 i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "app/cam/frame")
  (data (i32.const 16) "\78\00\78\00\00\00\64\00\00\00")
  (data (i32.const 32) "cam")
  (data (i32.const 80) "\78\00\78\00\01\00\64\00\00\00\05\12\13\15\24\27\22\23\19\17\16")
  (func (export "init")
    (i32.store (i32.const 64)
      (call $route_source (i32.const 1) (i32.const 0) (i32.const 13) (i32.const 80) (i32.const 21)))
    (drop (call $publish (i32.const 32) (i32.const 3) (i32.const 64) (i32.const 4)))
    (drop (call $route_source (i32.const 0) (i32.const 0) (i32.const 13) (i32.const 16) (i32.const 10))))
  (func (export "on_irq") (param i32 i32 i32 i32)))
"#;

/// Opens SPI bus 0 and starts a sink route on `app/cam/frame` with D/C on
/// GPIO 13, 28 800-byte frames and the MIPI window 60..=179 × 60..=179.
/// On every `RouteFrames` event it publishes `route_stat(handle, Frames)`
/// on `frames` (4 bytes LE).
const DISPLAY: &str = r#"
(module
  (import "mw" "publish" (func $publish (param i32 i32 i32 i32) (result i32)))
  (import "mw" "spi_open" (func $spi_open (param i32 i32 i32 i32 i32 i32 i32) (result i32)))
  (import "mw" "route_sink" (func $route_sink (param i32 i32 i32 i32 i32) (result i32)))
  (import "mw" "route_stat" (func $route_stat (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "app/cam/frame")
  (data (i32.const 16) "\0d\00\80\70\00\00\05\00\01\2a\01\04\00\3c\00\b3\00\01\2b\01\04\00\3c\00\b3\00\01\2c")
  (data (i32.const 64) "frames")
  (func (export "init")
    (drop (call $spi_open (i32.const 0) (i32.const 12) (i32.const 11) (i32.const -1)
                          (i32.const 10) (i32.const 40000) (i32.const 0)))
    (drop (call $route_sink (i32.const 0) (i32.const 13) (i32.const 0) (i32.const 16) (i32.const 28))))
  (func (export "on_irq") (param $kind i32) (param $src i32) (param $val i32) (param $t i32)
    (if (i32.ne (local.get $kind) (i32.const 5)) (then (return)))
    (i32.store (i32.const 128) (call $route_stat (local.get $src) (i32.const 0)))
    (drop (call $publish (i32.const 64) (i32.const 6) (i32.const 128) (i32.const 4)))))
"#;

fn write_wasm(name: &str, wat_src: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::write(&path, wat::parse_str(wat_src).expect("valid WAT")).expect("write wasm");
    path
}

fn sim(args: &[&str]) -> Output {
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
        .write_all(b"")
        .expect("stdin");
    child.wait_with_output().expect("payload-sim runs")
}

/// Runs the display with the camera as its peer; returns stdout and the
/// dumped panel.
fn run_pair(name: &str, extra: &[&str]) -> (String, Vec<u8>) {
    let camera = write_wasm("r10_camera.wasm", CAMERA);
    let display = write_wasm("r10_display.wasm", DISPLAY);
    let ppm = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_file(&ppm);
    let mut args = vec![
        display.to_str().unwrap(),
        "--peer",
        camera.to_str().unwrap(),
        "--dump-panel",
        ppm.to_str().unwrap(),
    ];
    args.extend_from_slice(extra);
    let out = sim(&args);
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

/// RGB565 expanded to the PPM's 8-bit channels by bit replication.
fn rgb888(p: u16) -> [u8; 3] {
    let (r, g, b) = (
        (p >> 11) as u8 & 0x1F,
        (p >> 5) as u8 & 0x3F,
        p as u8 & 0x1F,
    );
    [
        (r << 3) | (r >> 2),
        (g << 2) | (g >> 4),
        (b << 3) | (b >> 2),
    ]
}

fn at(ppm: &[u8], x: u16, y: u16) -> [u8; 3] {
    let i = (usize::from(y) * PANEL + usize::from(x)) * 3;
    [ppm[i], ppm[i + 1], ppm[i + 2]]
}

fn in_window(x: u16, y: u16) -> bool {
    (ORIGIN..ORIGIN + SIDE).contains(&x) && (ORIGIN..ORIGIN + SIDE).contains(&y)
}

#[test]
fn r10_camera_and_display_payloads_stream_the_pattern_onto_the_panel() {
    let (stdout, ppm) = run_pair("r10_lossless.ppm", &["--virtual", "--until-ms", "1000"]);
    // Camera::route_source(Camera) is Unsupported (-4 = 0xfcffffff LE).
    assert!(
        stdout.contains("r10_camera: publish cam 0xfcffffff"),
        "{stdout}"
    );
    // Frames 0..=9 at 100..=1000 ms, each 174 chunks of 166 bytes.
    assert_eq!(
        stdout.matches("r10_camera: route0 frame ").count(),
        10,
        "{stdout}"
    );
    assert!(
        stdout.contains("[  1000 ms] r10_camera: route0 frame 9 -> app/cam/frame (174 chunks)")
    );
    assert_eq!(stdout.matches("r10_display: route0 frame ").count(), 10);
    assert!(stdout.contains("[  1000 ms] r10_display: route0 frame 9 drawn"));
    // route_stat through the ABI, after each RouteFrames event.
    assert!(
        stdout.contains("r10_display: publish frames 0x0a000000"),
        "{stdout}"
    );
    assert!(stdout.contains(
        "r10_display: route0 sink app/cam/frame: frames 10 dropped 0 chunks 1740 chunks dropped 0"
    ));
    assert!(stdout.contains(
        "r10_camera: route0 source app/cam/frame: frames 10 dropped 0 chunks 1740 chunks dropped 0"
    ));
    assert!(stdout.contains("bus: delivered 1751 lost 0"), "{stdout}");
    for y in 0..PANEL as u16 {
        for x in 0..PANEL as u16 {
            let want = if in_window(x, y) {
                pattern::pixel(9, SIDE, SIDE, x - ORIGIN, y - ORIGIN)
            } else {
                0
            };
            assert_eq!(at(&ppm, x, y), rgb888(want), "pixel ({x}, {y})");
        }
    }
}

#[test]
fn r10_lossy_bus_never_draws_misplaced_pixels() {
    let frames = 30;
    let (stdout, ppm) = run_pair(
        "r10_lossy.ppm",
        &[
            "--virtual",
            "--until-ms",
            "3000",
            "--loss",
            "3",
            "--seed",
            "7",
        ],
    );
    let summary = stdout
        .lines()
        .find(|l| l.contains("r10_display: route0 sink") && l.contains("chunks dropped"))
        .expect("sink summary");
    let field = |name: &str| -> u32 {
        let rest = &summary[summary.find(name).expect(name) + name.len()..];
        rest.split_whitespace().next().unwrap().parse().unwrap()
    };
    let (drawn, dropped) = (field(": frames "), field(" dropped "));
    assert!(drawn > 0 && dropped > 0, "{summary}");
    assert!(drawn + dropped <= frames, "{summary}");
    assert!(!stdout.contains("bus: delivered 0 lost 0"));
    assert!(stdout.contains("r10_display: route0 frame ") && stdout.contains(" dropped\n"));
    for y in 0..PANEL as u16 {
        for x in 0..PANEL as u16 {
            let got = at(&ppm, x, y);
            if !in_window(x, y) {
                assert_eq!(got, [0, 0, 0], "pixel ({x}, {y}) outside the window");
                continue;
            }
            // Black is also what an unwritten pixel shows.
            let ok = got == [0, 0, 0]
                || (0..frames)
                    .any(|f| rgb888(pattern::pixel(f, SIDE, SIDE, x - ORIGIN, y - ORIGIN)) == got);
            assert!(ok, "pixel ({x}, {y}) = {got:?} belongs to no frame");
        }
    }
}

#[test]
fn r10_real_time_routes_follow_the_wall_clock() {
    let (stdout, ppm) = run_pair("r10_real.ppm", &["--until-ms", "350"]);
    let drawn = stdout.matches("r10_display: route0 frame ").count();
    assert!((2..=3).contains(&drawn), "{stdout}");
    let (x, y) = (ORIGIN + 30, ORIGIN + 70);
    let in_some_frame = (0..3)
        .any(|f| rgb888(pattern::pixel(f, SIDE, SIDE, x - ORIGIN, y - ORIGIN)) == at(&ppm, x, y));
    assert!(in_some_frame);
}
