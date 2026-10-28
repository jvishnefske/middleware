//! End-to-end JPEG stream in `payload-sim` (requirement R14): the real
//! `cam_jpeg_stream` and `cam_jpeg_view` payloads (built here from
//! `payloads/` with `cargo build --release`) run as `--peer`s under the
//! badge's limits (1 000 000 fuel per call, one 64 KiB page), the view
//! drawing with its own SPI transfers on the virtual panel (`--panel-dc
//! 13`). The panel must equal `mw-jpeg`'s host-side decode of a fixture
//! frame bit for bit, and no call may trap (running out of fuel traps).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use mw_jpeg::{Decoder, Rgb565Be, Step};

const PANEL: usize = 240;
const PPM_HEADER: &[u8] = b"P6\n240 240\n255\n";
/// Fuel per call the payloads must stay under: well below the budget.
const FUEL_TARGET: u64 = 600_000;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Builds the two payloads once per test binary; returns their paths.
fn payloads() -> &'static (PathBuf, PathBuf) {
    static BUILT: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();
    BUILT.get_or_init(|| {
        let dir = repo().join("payloads");
        let status = Command::new(env!("CARGO"))
            .args([
                "build",
                "--release",
                "-q",
                "-p",
                "cam_jpeg_stream",
                "-p",
                "cam_jpeg_view",
            ])
            .current_dir(&dir)
            // The payload workspace's .cargo/config.toml sets the target
            // and the wasm rustflags; nothing from this build may override
            // them.
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env_remove("CARGO_BUILD_TARGET")
            .env_remove("CARGO_TARGET_DIR")
            .status()
            .expect("cargo runs");
        assert!(status.success(), "building the payloads failed");
        let out = dir.join("target/wasm32-unknown-unknown/release");
        (
            out.join("cam_jpeg_view.wasm"),
            out.join("cam_jpeg_stream.wasm"),
        )
    })
}

/// The fixture frames' RGB565 decodes, row-major.
fn references() -> Vec<Vec<u16>> {
    (0..8)
        .map(|i| {
            let path = repo().join(format!("fixtures/cam/jpeg240/frame_{i:02}.jpg"));
            let jpeg = std::fs::read(path).expect("fixture");
            let mut dec = Decoder::new(&jpeg).expect("header");
            assert_eq!((dec.output_width(), dec.output_height()), (240, 240));
            let mut strip = vec![0u8; dec.strip_bytes::<Rgb565Be>()];
            let mut px = Vec::with_capacity(PANEL * PANEL);
            for _ in 0..100 {
                match dec
                    .decode_mcu_row::<Rgb565Be>(&jpeg, &mut strip)
                    .expect("decodes")
                {
                    Step::Row(s) => px.extend(
                        strip[..s.bytes(2)]
                            .chunks_exact(2)
                            .map(|b| u16::from_be_bytes([b[0], b[1]])),
                    ),
                    Step::Done => break,
                    Step::Partial => unreachable!("a whole row per call"),
                }
            }
            assert_eq!(px.len(), PANEL * PANEL);
            px
        })
        .collect()
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

/// The run's stdout and panel (as PPM pixel bytes).
fn run(name: &str, extra: &[&str]) -> (String, Vec<u8>) {
    let (view, stream) = payloads();
    let ppm = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_file(&ppm);
    let mut args: Vec<&str> = vec![
        view.to_str().unwrap(),
        "--peer",
        stream.to_str().unwrap(),
        "--virtual",
        "--fuel",
        "1000000",
        "--memory-bytes",
        "65536",
        "--panel-dc",
        "13",
        "--ticks",
        "10",
        "--dump-panel",
        ppm.to_str().unwrap(),
    ];
    args.extend_from_slice(extra);
    let out = Command::new(env!("CARGO_BIN_EXE_payload-sim"))
        .args(&args)
        .stdin(Stdio::null())
        .output()
        .expect("payload-sim runs");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success() && !stdout.contains("trap:"),
        "{}\n{}",
        tail(&stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let bytes = std::fs::read(&ppm).expect("panel dumped");
    assert!(bytes.starts_with(PPM_HEADER));
    (stdout, bytes[PPM_HEADER.len()..].to_vec())
}

fn tail(s: &str) -> String {
    let lines: Vec<&str> = s.lines().filter(|l| !l.contains("spi0 tx")).collect();
    lines[lines.len().saturating_sub(40)..].join("\n")
}

/// `(calls, max, total)` of a board's closing fuel line.
fn fuel(stdout: &str, board: &str) -> (u64, u64, u64) {
    let tag = format!("{board}: fuel calls ");
    let line = stdout
        .lines()
        .find(|l| l.contains(&tag))
        .expect("fuel line");
    let rest = &line[line.find(&tag).unwrap() + tag.len()..];
    let w: Vec<&str> = rest.split_whitespace().collect();
    (
        w[0].parse().unwrap(),
        w[2].parse().unwrap(),
        w[4].parse().unwrap(),
    )
}

/// A field of the view's last stats log line.
fn stat(stdout: &str, name: &str) -> u32 {
    let line = stdout
        .lines()
        .rfind(|l| l.contains("log cam_jpeg_view: received "))
        .expect("stats line");
    let tag = format!(" {name} ");
    let rest = &line[line.find(&tag).expect(name) + tag.len()..];
    rest.split_whitespace().next().unwrap().parse().unwrap()
}

/// The 16-row band `b` of the panel equals that band of `frame`.
fn band_is(ppm: &[u8], frame: &[u16], b: usize) -> bool {
    (b * 16 * PANEL..(b + 1) * 16 * PANEL).all(|i| ppm[i * 3..i * 3 + 3] == rgb888(frame[i]))
}

#[test]
fn r14_jpeg_stream_is_decoded_bit_exactly_onto_the_panel_within_the_fuel_budget() {
    let (stdout, ppm) = run("r14_lossless.ppm", &["--until-ms", "3000"]);
    let refs = references();
    // The last frame drawn is on the panel, bit-exact; the fake camera
    // serves fixture `seq % 8` as frame `seq`.
    let last = stdout
        .lines()
        .rfind(|l| l.contains("cam_jpeg_view: frame ") && l.contains(" drawn in "))
        .expect("a frame was drawn");
    let seq: usize = last
        .split("frame ")
        .nth(1)
        .unwrap()
        .split(' ')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let frame = &refs[seq % 8];
    for b in 0..PANEL / 16 {
        assert!(
            band_is(&ppm, frame, b),
            "band {b} differs from fixture {}",
            seq % 8
        );
    }
    // Every frame from the first one after the bring-up is drawn, none lost.
    let drawn = stdout.matches(" drawn in ").count();
    assert!(drawn >= 25, "{drawn} frames drawn\n{}", tail(&stdout));
    assert_eq!(stat(&stdout, "failed"), 0);
    assert!(stat(&stdout, "decoded") >= 15);
    // No call came near the budget (a call over it traps, checked in run).
    let (calls, max, total) = fuel(&stdout, "cam_jpeg_view");
    assert!(max < FUEL_TARGET, "max fuel per call {max}");
    assert!(calls > 1000 && total > 0);
    let (_, max, _) = fuel(&stdout, "cam_jpeg_stream");
    assert!(max < FUEL_TARGET);
}

#[test]
fn r14_lossy_stream_drops_frames_without_trapping_or_misdrawing() {
    let (stdout, ppm) = run(
        "r14_lossy.ppm",
        &["--until-ms", "4000", "--loss", "20", "--seed", "3"],
    );
    let refs = references();
    assert!(stat(&stdout, "dropped") > 0, "{}", tail(&stdout));
    assert!(stat(&stdout, "decoded") > 0);
    assert_eq!(stat(&stdout, "failed"), 0, "a reassembled frame is whole");
    // Strips are drawn whole: each band is the cleared panel or a band of
    // some fixture frame.
    let black = vec![0u16; PANEL * PANEL];
    for b in 0..PANEL / 16 {
        let ok = band_is(&ppm, &black, b) || refs.iter().any(|f| band_is(&ppm, f, b));
        assert!(ok, "band {b} belongs to no frame");
    }
    let (_, max, _) = fuel(&stdout, "cam_jpeg_view");
    assert!(max < FUEL_TARGET, "max fuel per call {max}");
}
