//! The `bench` payload's fuel constants (`payloads/examples/bench`): its
//! deploy build (1-byte pages, `scripts/build-byte-pages.sh -p bench`) runs
//! under the badge's limits against the simulator's board, one step per
//! timer event, for one whole JPEG frame. The fuel each step used, read
//! from `wasm_host::FuelStats`, must match the crate's `MIX_FUEL` (per mix
//! step) and `JPEG_FRAME_FUEL` (per frame of JPEG steps) within 5 %, so the
//! rates the payload logs on a board (constant over measured time) stay
//! honest. No step may come near the per-call budget.

/// The simulator's board, compiled from the binary's sources.
#[path = "../src"]
#[allow(dead_code, unused_imports)]
mod src {
    pub mod sim;
}

use std::path::{Path, PathBuf};
use std::process::Command;

use middleware::wasm_abi::events::{Event, EventKind};
use wasm_host::{Limits, Payload};

use src::sim::SimDevice;

/// The badge's limits (the ESP32-S3 profile's 64 KiB cap, the ABI's fuel
/// per call).
const LIMITS: Limits = Limits::bytes(64 * 1024);

/// Fuel per call the steps must stay under, as `cam_jpeg_view`'s (r14.rs).
const FUEL_TARGET: u64 = 600_000;

/// The bench crate's source, for its constants.
const SOURCE: &str = include_str!("../../../payloads/examples/bench/src/lib.rs");

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// `pub const NAME: u32 = N;` from the bench source (underscores allowed).
fn constant(name: &str) -> u64 {
    let prefix = format!("pub const {name}: u32 = ");
    SOURCE
        .lines()
        .find_map(|l| l.strip_prefix(&prefix))
        .and_then(|v| v.trim_end_matches(';').replace('_', "").parse().ok())
        .unwrap_or_else(|| panic!("{name} in bench/src/lib.rs"))
}

/// Builds `package`'s deploy build; returns its path.
fn build(package: &str) -> PathBuf {
    let dir = repo().join("payloads");
    let status = Command::new(dir.join("scripts/build-byte-pages.sh"))
        .args(["-q", "-p", package])
        .current_dir(&dir)
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("CARGO_BUILD_TARGET")
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("MEMORY_BYTES")
        .env_remove("STACK_BYTES")
        .status()
        .expect("build-byte-pages.sh runs");
    assert!(status.success(), "building {package} failed");
    dir.join(format!("target/byte-pages/wasm32-unknown-unknown/release/{package}.wasm"))
}

fn within_5_percent(measured: u64, documented: u64) -> bool {
    measured.abs_diff(documented) * 20 <= documented
}

#[test]
fn bench_fuel_constants_match_the_measured_steps() {
    let wasm = std::fs::read(build("bench")).expect("bench payload");
    let mut p = Payload::load(&wasm, SimDevice::new(None), LIMITS).expect("bench loads");
    let steps = constant("JPEG_STEPS_PER_FRAME");
    let step = Event { kind: EventKind::Timer, source: 0, value: 1, time_us: 0 };
    let (mut mix, mut jpeg) = (Vec::new(), Vec::new());
    for i in 0..2 * steps {
        let before = p.fuel_stats().total;
        assert!(p.dispatch(step).expect("step runs without a trap"), "step {i}: on_irq exported");
        let used = p.fuel_stats().total - before;
        if i % 2 == 0 { mix.push(used) } else { jpeg.push(used) }
    }
    let mix_avg = mix.iter().sum::<u64>() / mix.len() as u64;
    let frame: u64 = jpeg.iter().sum();
    let max = mix.iter().chain(&jpeg).copied().max().unwrap_or(0);
    eprintln!("mix step {mix_avg} fuel (min {:?}, max {:?}); jpeg frame {frame} fuel; max step {max}", mix.iter().min(), mix.iter().max());
    assert!(within_5_percent(mix_avg, constant("MIX_FUEL")), "MIX_FUEL: measured {mix_avg}");
    assert!(within_5_percent(frame, constant("JPEG_FRAME_FUEL")), "JPEG_FRAME_FUEL: measured {frame}");
    assert!(max < FUEL_TARGET, "largest step {max}");
    // The same frame again costs the same: the frame ended exactly after
    // JPEG_STEPS_PER_FRAME steps and the next one starts over.
    let again: u64 = (0..2 * steps)
        .map(|_| {
            let before = p.fuel_stats().total;
            assert!(p.dispatch(step).expect("step runs without a trap"));
            p.fuel_stats().total - before
        })
        .skip(1)
        .step_by(2)
        .sum();
    assert_eq!(again, frame, "every frame costs the same fuel");
}

#[test]
fn bench_starts_wherever_cam_jpeg_view_starts() {
    // The firmware refuses a payload whose heap bound (plus margin) is
    // above the free heap; the benchmark must not need more than the
    // viewer it stands in for.
    let bench = std::fs::read(build("bench")).expect("bench payload");
    let view = std::fs::read(build("cam_jpeg_view")).expect("cam_jpeg_view payload");
    let (b, v) = (LIMITS.heap_needed(&bench), LIMITS.heap_needed(&view));
    eprintln!("heap bound: bench {b} B, cam_jpeg_view {v} B");
    assert!(b <= v, "bench needs {b} B, cam_jpeg_view {v} B");
}
