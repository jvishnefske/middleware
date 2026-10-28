//! Heap use of every example payload under the badge's limits
//! (requirement W10): the bound [`Limits::heap_needed`] must cover the
//! measured peak of loading and of running each one.
//!
//! The examples are built here from `payloads/` with
//! `cargo build --release`, as in `r14.rs`, plus `cam_jpeg_view`'s deploy
//! build with 1-byte pages and exactly its memory
//! (`scripts/build-byte-pages.sh -p cam_jpeg_view`, listed as
//! `cam_jpeg_view/b`), whose bound charges its declared 48 128 bytes
//! rather than the 64 KiB cap. Each is loaded with the
//! ESP32-S3 profile's limits (one 64 KiB page of linear memory, 1 000 000
//! fuel per call, the default stacks) and run in virtual time against the
//! simulator's board ([`sim::SimDevice`]): its subscriptions get a message
//! each, and the two viewers get the frames their camera peer publishes
//! (`cam_view` from `cam_stream`, `cam_jpeg_view` from `cam_jpeg_stream`),
//! recorded before the measurement. A counting global allocator
//! ([`peak_alloc`]) gives the peak:
//!
//! * **compile**: from just before the image is read into its buffer
//!   until [`Compiled::new`] returns (the image buffer included, as the
//!   firmware holds it while compiling);
//! * **instantiate**: [`Payload::instantiate`] and `init`, image freed;
//! * **run**: during every later call into the payload.
//!
//! All are relative to the heap in use before the load (the board, the
//! recorded frames). Simulator allocations made inside a call (event
//! lines, the copies of SPI transfers) are counted too, so the figures
//! over-estimate the runtime's own use slightly.
//!
//! The 32-bit figures in `wasm_host::heap` come from this same test built
//! for a 32-bit host, the closest runnable stand-in for the Xtensa chips:
//! `cargo test -p payload-sim --release --test heap --target
//! i686-unknown-linux-gnu` (not part of the gates; it needs the i686
//! target and a multilib C toolchain).
//!
//! This file is its own test binary without the libtest harness
//! (`harness = false` in `Cargo.toml`): the allocator's counters are
//! process wide, and the harness would run other tests, or its own
//! threads, alongside.

#[global_allocator]
static PEAK: PeakAlloc = PeakAlloc;

/// The simulator's board, compiled from the binary's sources.
#[path = "../src"]
#[allow(dead_code, unused_imports)]
mod src {
    pub mod sim;
}

use std::path::{Path, PathBuf};
use std::process::Command;

use middleware::Key;
use middleware::wasm_abi::events::MAX_EVENTS;
use peak_alloc::PeakAlloc;
use wasm_host::{Compiled, Limits, Payload, Ticked};

use src::sim::SimDevice;

/// The examples `payloads/scripts/check-all.sh` builds, except `bench`:
/// a CPU benchmark that never stops stepping (about 100 000 fuel per
/// millisecond of virtual time), so [`STEPS`] of it would take minutes
/// here; its fuel is measured in `tests/bench.rs`.
const EXAMPLES: [&str; 10] = [
    "blink",
    "echo",
    "button",
    "spi_loopback",
    "uart_echo",
    "dimmer",
    "cam_stream",
    "cam_view",
    "cam_jpeg_stream",
    "cam_jpeg_view",
];

/// The badge's limits: the ESP32-S3 profile's one 64 KiB page, the ABI's
/// fuel per call and the default interpreter stacks.
const LIMITS: Limits = Limits::bytes(64 * 1024);

/// Virtual-time steps per run; enough for `cam_jpeg_view` to decode and
/// draw a whole frame and publish its statistics.
const STEPS: u32 = 20_000;

/// Ticks per run (the stats ticks are 1–2 s apart).
const TICKS: u32 = 3;

/// The panel's D/C pin on the badge (`cam_view`, `cam_jpeg_view`).
const PANEL_DC: u8 = 13;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Builds `cam_jpeg_view`'s deploy build, with 1-byte pages and the
/// memory and stack of its `[package.metadata.byte-pages]`, as
/// `payloads/scripts/check-all.sh` does; returns its path.
fn build_byte_pages() -> PathBuf {
    let dir = repo().join("payloads");
    let status = Command::new(dir.join("scripts/build-byte-pages.sh"))
        .args(["-q", "-p", "cam_jpeg_view"])
        .current_dir(&dir)
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("CARGO_BUILD_TARGET")
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("MEMORY_BYTES")
        .env_remove("STACK_BYTES")
        .status()
        .expect("build-byte-pages.sh runs");
    assert!(status.success(), "building the byte-page payload failed");
    dir.join("target/byte-pages/wasm32-unknown-unknown/release/cam_jpeg_view.wasm")
}

/// Builds the examples; returns the release directory.
fn build() -> PathBuf {
    let dir = repo().join("payloads");
    let mut args = vec!["build", "--release", "-q"];
    for e in EXAMPLES {
        args.extend(["-p", e]);
    }
    let status = Command::new(env!("CARGO"))
        .args(&args)
        .current_dir(&dir)
        // The payload workspace's .cargo/config.toml sets the target and
        // the wasm rustflags; nothing from this build may override them.
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("CARGO_BUILD_TARGET")
        .env_remove("CARGO_TARGET_DIR")
        .status()
        .expect("cargo runs");
    assert!(status.success(), "building the payloads failed");
    dir.join("target/wasm32-unknown-unknown/release")
}

fn board() -> SimDevice {
    SimDevice::new(None).with_panel_dc(PANEL_DC)
}

/// What one run did.
#[derive(Default)]
struct Run {
    calls: u32,
    /// Highest heap in use during a call into the payload (absolute).
    peak: usize,
    published: Vec<(String, Vec<u8>)>,
}

/// Runs `p` in virtual time: delivers `feed` at time 0 (to a sink route
/// first, else to a subscription), then jumps from one due tick or timer to
/// the next for at most [`STEPS`] steps and [`TICKS`] ticks. Keeps what the
/// payload published when `keep` says so. Panics on a trap.
///
/// The simulator's own work between calls (frames of its source routes,
/// sink routes drawing) is kept out of [`Run::peak`]: the peak is reset
/// before each call into the payload and read after it, with the
/// previous step's outbox and event lines already dropped.
fn run(
    p: &mut Payload<SimDevice>,
    feed: &[(String, Vec<u8>)],
    keep: impl Fn(&str) -> bool,
) -> Run {
    let mut out = Run::default();
    let mut ticks = 0;
    let call = |out: &mut Run| {
        out.calls += 1;
        out.peak = out.peak.max(PEAK.peak_usage());
    };
    let collect = |p: &mut Payload<SimDevice>, out: &mut Run| {
        p.device().events.clear();
        for (key, data) in p.device().take_outbox() {
            if keep(&key) {
                out.published.push((key, data));
            }
        }
    };
    for (key, data) in feed {
        let key = Key::new(key).expect("key");
        if !p.device().route_receive(key.as_str(), data) {
            PEAK.reset_peak_usage();
            p.deliver(key, data).expect("delivered without a trap");
            call(&mut out);
        }
        collect(p, &mut out);
    }
    for _ in 0..STEPS {
        p.device().complete_dma();
        for _ in 0..MAX_EVENTS {
            let Some(event) = p.device().pop_event() else {
                break;
            };
            PEAK.reset_peak_usage();
            p.dispatch(event).expect("on_irq without a trap");
            call(&mut out);
        }
        collect(p, &mut out);
        if p.device().busy() {
            continue;
        }
        let tick = (ticks < TICKS)
            .then(|| p.next_tick())
            .flatten()
            .map(|ms| u64::from(ms) * 1000);
        let Some(next) = [tick, p.device().next_due_us()].into_iter().flatten().min() else {
            break;
        };
        p.device().advance_to(next);
        collect(p, &mut out);
        if tick == Some(next) {
            PEAK.reset_peak_usage();
            let ran = p.tick((next / 1000) as u32).expect("tick without a trap");
            if ran == Ticked::Ran {
                ticks += 1;
                call(&mut out);
            }
            collect(p, &mut out);
        }
    }
    out
}

/// The frames a camera payload publishes on `key` in one run.
fn record(dir: &Path, camera: &str, key: &str) -> Vec<(String, Vec<u8>)> {
    let wasm = std::fs::read(dir.join(format!("{camera}.wasm"))).expect("camera payload");
    let mut p = Payload::load(&wasm, board(), LIMITS).expect("camera loads");
    let frames = run(&mut p, &[], |k| k == key).published;
    assert!(!frames.is_empty(), "{camera} published nothing on {key}");
    frames
}

/// One payload's measurement, in bytes above the heap in use before it.
struct Measured {
    image: u32,
    /// Peak while the image is read and compiled.
    compile: usize,
    /// In use once compiled, image freed: the module.
    module: usize,
    /// Peak while the compiled module is instantiated and `init` runs.
    instantiate: usize,
    /// In use once loaded, image freed.
    resident: usize,
    /// Peak while it runs.
    run: usize,
    calls: u32,
    bound: u32,
}

fn measure(path: &Path, name: &str, feed: &[(String, Vec<u8>)]) -> Measured {
    let device = board();
    let base = PEAK.current_usage();
    PEAK.reset_peak_usage();
    let image = std::fs::read(path).expect("payload built");
    let image_len = u32::try_from(image.len()).expect("small image");
    let bound = LIMITS.heap_needed(&image);
    let compiled = Compiled::new(&image, LIMITS).expect("payload compiles");
    let compile = PEAK.peak_usage() - base;
    drop(image);
    let module = PEAK.current_usage() - base;
    PEAK.reset_peak_usage();
    let mut p = Payload::instantiate(compiled, device).expect("payload loads");
    let instantiate = PEAK.peak_usage() - base;
    let resident = PEAK.current_usage() - base;
    let keep = |k: &str| k == "app/cam/view_stats";
    let outcome = if feed.is_empty() {
        // A message on each subscribed key.
        let subs: Vec<(String, Vec<u8>)> = p
            .subscriptions()
            .map(|k| (k.to_string(), b"hello".to_vec()))
            .collect();
        run(&mut p, &subs, keep)
    } else {
        run(&mut p, feed, keep)
    };
    let run_peak = outcome.peak.saturating_sub(base);
    if name == "cam_jpeg_view" {
        // The stats are received, decoded, dropped, failed, decode ms
        // (u32 LE): the run must have decoded a frame.
        let decoded = outcome
            .published
            .iter()
            .filter_map(|(_, d)| d.get(4..8))
            .filter_map(|b| b.try_into().ok())
            .map(u32::from_le_bytes)
            .max();
        assert!(
            decoded.is_some_and(|n| n >= 1),
            "cam_jpeg_view decoded no frame"
        );
    }
    drop(p);
    Measured {
        image: image_len,
        compile,
        module,
        instantiate,
        resident,
        run: run_peak,
        calls: outcome.calls,
        bound,
    }
}

fn main() {
    let dir = build();
    let jpeg = record(&dir, "cam_jpeg_stream", "app/cam/jpeg");
    let rgb = record(&dir, "cam_stream", "app/cam/frame");
    println!(
        "{:<16} {:>6} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}",
        "payload", "image", "compile", "module", "instant", "resident", "run", "calls", "bound"
    );
    let mut failed = Vec::new();
    let deploy = build_byte_pages();
    let runs = EXAMPLES
        .iter()
        .map(|name| (*name, dir.join(format!("{name}.wasm"))))
        .chain([("cam_jpeg_view", deploy)]);
    for (i, (name, path)) in runs.enumerate() {
        let feed: &[(String, Vec<u8>)] = match name {
            "cam_jpeg_view" => &jpeg,
            "cam_view" => &rgb,
            _ => &[],
        };
        let m = measure(&path, name, feed);
        let label = if i < EXAMPLES.len() {
            name.to_string()
        } else {
            format!("{name}/b")
        };
        println!(
            "{label:<16} {:>6} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}",
            m.image, m.compile, m.module, m.instantiate, m.resident, m.run, m.calls, m.bound
        );
        let peak = m.compile.max(m.instantiate).max(m.run);
        if usize::try_from(m.bound).unwrap_or(usize::MAX) < peak {
            failed.push(format!("{label}: bound {} < peak {peak}", m.bound));
        }
    }
    assert!(failed.is_empty(), "heap bound too small: {failed:?}");
    println!("heap bound covers every example payload");
}
