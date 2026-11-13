//! The `periph_demo` payload in `payload-sim` (requirement V14): the real
//! byte-page build (built here from `payloads/` with
//! `scripts/build-byte-pages.sh`, so 1-byte pages and 16 KiB of fixed
//! memory, run under a 16 KiB memory cap) runs in virtual time. Its SPI
//! loopback transfer must come back intact, and its I2C read of register
//! 0x00 at 0x50 must return the value preset with `!i2c 0 0x00 0x5a`.
//!
//! The simulator calls `init` before it reads stdin, so the read `init`
//! makes sees the register's initial 0; the preset shows from the first
//! 1 s repeat on.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// The byte-page build's fixed linear memory (build-byte-pages.sh default).
const MEMORY: &str = "16384";

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Builds the byte-page build once per test binary; returns its path.
fn payload() -> &'static PathBuf {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT.get_or_init(|| {
        let dir = repo().join("payloads");
        let status = Command::new("bash")
            .args(["scripts/build-byte-pages.sh", "-q", "-p", "periph_demo"])
            .current_dir(&dir)
            // The script sets the wasm rustflags and the target directory
            // itself; memory and stack are its defaults.
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env_remove("CARGO_BUILD_TARGET")
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("MEMORY_BYTES")
            .env_remove("STACK_BYTES")
            .status()
            .expect("the build script runs");
        assert!(status.success(), "building periph_demo failed");
        dir.join("target/byte-pages/wasm32-unknown-unknown/release/periph_demo.wasm")
    })
}

/// Runs the payload for 1.5 s of virtual time with `stdin`; returns stdout.
fn run(stdin: &[u8]) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_payload-sim"))
        .arg(payload())
        .args(["--virtual", "--memory-bytes", MEMORY, "--until-ms", "1500"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn payload-sim");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin)
        .expect("write stdin");
    let out = child.wait_with_output().expect("payload-sim runs");
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

#[test]
fn v14_periph_demo_runs_spi_loopback_and_i2c_read_in_the_simulator() {
    let stdout = run(b"!i2c 0 0x00 0x5a\n");
    let lines = events(&stdout);
    for expected in [
        "spi0 open 1000 kHz mode 0",
        "i2c0 open 100000 Hz",
        "spi0 tx 0xa55a00ff01803cc3 (8 bytes)",
        "i2c0 0x50 reg 0x00 <- 0x5a",
        "log periph_demo: spi loopback ok (8 bytes)",
        "publish app/periph_demo/spi ok",
        "i2c0 0x50 w 0x00 r Z",
        "log periph_demo: i2c 0x50 reg 0x00 = 0x5a",
        // 0x5a is printable, so the simulator shows the byte as `Z`.
        "publish app/periph_demo/i2c Z",
    ] {
        assert!(lines.contains(&expected), "missing {expected:?} in {stdout}");
    }
    // Two rounds (init and the 1 s repeat), each with one good transfer.
    let ok = lines
        .iter()
        .filter(|l| **l == "publish app/periph_demo/spi ok")
        .count();
    assert_eq!(ok, 2, "{stdout}");
    assert!(!stdout.contains("app/periph_demo/spi err"), "{stdout}");
    // Only the configured device is ever addressed.
    for l in lines.iter().filter(|l| l.starts_with("i2c0 0x")) {
        assert!(l.starts_with("i2c0 0x50 "), "{l}");
    }
}

#[test]
fn v14_periph_demo_logs_a_mismatch_when_miso_disagrees() {
    let out = Command::new(env!("CARGO_BIN_EXE_payload-sim"))
        .arg(payload())
        .args([
            "--virtual",
            "--memory-bytes",
            MEMORY,
            "--until-ms",
            "500",
            "--spi-pattern",
            "a55a00fe",
        ])
        .stdin(Stdio::null())
        .output()
        .expect("payload-sim runs");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "{stdout}");
    let lines = events(&stdout);
    for expected in [
        "log periph_demo: spi loopback mismatch at 3: sent 0xff got 0xfe",
        "publish app/periph_demo/spi err",
        // Nothing preset: the register reads its initial 0.
        "log periph_demo: i2c 0x50 reg 0x00 = 0x00",
        "publish app/periph_demo/i2c 0x00",
    ] {
        assert!(lines.contains(&expected), "missing {expected:?} in {stdout}");
    }
}
