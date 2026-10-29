//! The OV2640 camera payload in `payload-sim`: the real
//! `cam_ov2640_stream` deploy build (built here from `payloads/` with
//! `scripts/build-byte-pages.sh`, so 1-byte pages and the crate's fixed
//! memory) runs in virtual time under the ESP32's 1 000 000 fuel per call
//! against the simulator's OV2640 register model on I2C bus 0. It must read
//! and log the sensor ID, soft-reset the sensor, write its whole register
//! table (JPEG at 240×240), then try the camera route, which the simulator
//! does not have (`Unsupported`), and keep retrying on its tick without
//! trapping.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use middleware::wasm_abi::{AbiError, FUEL_PER_CALL};

/// The deploy build's fixed linear memory (its `[package.metadata.byte-pages]`).
const MEMORY: &str = "6144";
/// Register writes: one bank select before the ID reads, two for the soft
/// reset, then the payload's 219-entry table.
const WRITES: usize = 1 + 2 + 219;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Builds the deploy build once per test binary; returns its path.
fn payload() -> &'static PathBuf {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT.get_or_init(|| {
        let dir = repo().join("payloads");
        let status = Command::new("bash")
            .args(["scripts/build-byte-pages.sh", "-p", "cam_ov2640_stream"])
            .current_dir(&dir)
            // The script sets the wasm rustflags and the target directory
            // itself; the memory and stack come from the crate's manifest.
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env_remove("CARGO_BUILD_TARGET")
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("MEMORY_BYTES")
            .env_remove("STACK_BYTES")
            .status()
            .expect("the build script runs");
        assert!(status.success(), "building cam_ov2640_stream failed");
        dir.join("target/byte-pages/wasm32-unknown-unknown/release/cam_ov2640_stream.wasm")
    })
}

/// The run's stdout, with the `[time] ` prefixes kept.
fn run() -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_payload-sim"))
        .arg(payload())
        .args([
            "--virtual",
            "--fuel",
            &FUEL_PER_CALL.to_string(),
            "--memory-bytes",
            MEMORY,
            "--ticks",
            "3",
            "--until-ms",
            "6000",
        ])
        .output()
        .expect("payload-sim runs");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success() && !stdout.contains("trap"),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
}

/// The event of each line, without its `[time] ` prefix.
fn events(stdout: &str) -> Vec<&str> {
    stdout
        .lines()
        .map(|l| l.split_once("] ").map_or(l, |(_, e)| e))
        .collect()
}

/// The milliseconds of the first line containing `what`.
fn time_of(stdout: &str, what: &str) -> u64 {
    let line = stdout
        .lines()
        .find(|l| l.contains(what))
        .unwrap_or_else(|| panic!("no {what:?} in\n{stdout}"));
    line.trim_start_matches('[')
        .split_whitespace()
        .next()
        .and_then(|t| t.parse().ok())
        .expect("a time")
}

/// The `reg=value` pairs of the closing `ov2640 <bank>` line.
fn bank(stdout: &str, name: &str) -> Vec<(u8, u8)> {
    let tag = format!("ov2640 {name}");
    let line = events(stdout)
        .into_iter()
        .find(|e| e.starts_with(&tag))
        .unwrap_or_else(|| panic!("no {tag:?} line"));
    line[tag.len()..]
        .split_whitespace()
        .map(|kv| {
            let (k, v) = kv.split_once('=').expect("reg=value");
            (
                u8::from_str_radix(k, 16).unwrap(),
                u8::from_str_radix(v, 16).unwrap(),
            )
        })
        .collect()
}

/// `(calls, max)` of the closing fuel line.
fn fuel(stdout: &str) -> (u64, u64) {
    let line = events(stdout)
        .into_iter()
        .find(|e| e.starts_with("fuel calls "))
        .expect("fuel line");
    let w: Vec<&str> = line.split_whitespace().collect();
    (w[2].parse().unwrap(), w[4].parse().unwrap())
}

#[test]
fn ov2640_bring_up_writes_the_whole_table_and_tries_the_camera_route() {
    let stdout = run();
    let ev = events(&stdout);
    // Power and clock first, then the ID read after the power-up delay.
    for expected in [
        "gpio 32 = low",
        "pwm0 open 20000000 Hz",
        "pwm0.0 duty 32768",
        "i2c0 open 100000 Hz",
        "i2c0 0x30 w 0xff01 r -",
        "i2c0 0x30 w 0x0a r -",
        // Printable bytes are shown as text: 0x26 is '&', 0x42 'B'.
        "i2c0 0x30 w - r &",
        "i2c0 0x30 w 0x0b r -",
        "i2c0 0x30 w - r B",
        "log cam_ov2640_stream: sensor PID 0x26 VER 0x42",
        "i2c0 0x30 w 0x1280 r -",
    ] {
        assert!(ev.contains(&expected), "missing {expected:?}\n{stdout}");
    }
    let id = time_of(&stdout, "sensor PID");
    assert!(id >= 10, "ID read at {id} ms, before the power-up delay");
    // Every write was acknowledged and the model counted all of them,
    // with exactly one soft reset.
    assert!(
        !ev.iter()
            .any(|e| e.starts_with("i2c0 0x30") && !e.contains(" r ")),
        "a refused SCCB transfer"
    );
    assert!(
        ev.contains(&format!("ov2640 writes {WRITES} resets 1").as_str()),
        "{stdout}"
    );
    // The final registers: JPEG output, quality, 240×240 output of the
    // centred 1200×1200 window, DSP running.
    let dsp = bank(&stdout, "dsp");
    for (reg, value) in [
        (0xDA, 0x10), // IMAGE_MODE: JPEG
        (0x44, 15),   // QS
        (0xD3, 0x08), // R_DVP_SP: manual divider 8 (ESP32 PCLK)
        (0x5A, 60),   // ZMOW: 240 / 4
        (0x5B, 60),   // ZMOH: 240 / 4
        (0x51, 0x2C), // HSIZE: 1200 / 4, low byte
        (0x52, 0x2C), // VSIZE
        (0x53, 0xC8), // XOFFL: 200
        (0x55, 0x88), // VHYX
        (0x50, 0x80), // CTRLI: LP_DP
    ] {
        assert!(dsp.contains(&(reg, value)), "dsp {reg:#04x} in {dsp:x?}");
    }
    // ZMHH, RESET and R_BYPASS end at 0 (their defaults: not listed).
    for reg in [0x5C, 0xE0, 0x05] {
        assert!(dsp.iter().all(|&(r, _)| r != reg), "dsp {reg:#04x} set");
    }
    let sensor = bank(&stdout, "sensor");
    assert!(sensor.contains(&(0x03, 0x0F)), "COM1 for UXGA");
    assert!(sensor.contains(&(0x32, 0x36)), "REG32 for UXGA");
    assert!(
        sensor.iter().all(|&(r, _)| r != 0x11),
        "CLKRC back at 0x00, no doubler (ESP32 PCLK)"
    );
    // Then the camera route, refused by the simulator, retried on every
    // tick without trapping.
    let refused = format!(
        "log cam_ov2640_stream: route_camera error {}",
        AbiError::Unsupported.code().unsigned_abs()
    );
    let tries = ev.iter().filter(|e| **e == refused).count();
    assert_eq!(tries, 4, "once after the table, then each of 3 ticks");
    assert!(time_of(&stdout, "route_camera") > time_of(&stdout, "0x30 w 0x1280"));
    assert!(ev.contains(&"log cam_ov2640_stream: frames 0 dropped 0 stops 0"));
    // No call came near the ESP32's budget.
    let (calls, max) = fuel(&stdout);
    eprintln!("cam_ov2640_stream: {calls} calls, at most {max} fuel in one");
    assert!(max < FUEL_PER_CALL, "max fuel per call {max}");
    assert!(max < 50_000, "a 32-write slice measured under 6 000: {max}");
}
