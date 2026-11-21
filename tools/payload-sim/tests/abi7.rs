//! ABI 7 channels end to end (BD6): the `level_meter` example (built from
//! `payloads/`) declares `audio` and `level` and names no key. With
//! `--bind` the simulated board starts the fake microphone's levels route
//! on `ch/audio` before `init` and the payload publishes one line per
//! batch on the bound `level` key; rebinding (a new file, the same image)
//! changes the rate and the key; without a binding the required channel
//! fails the load with its name.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use middleware::loader::binding::{BINDING_MAX, Binding, Target};
use middleware::loader::trust::Version;
use middleware::wasm_abi::route::{AudioMode, MicConfig, RouteSource, SampleSource};
use middleware::{Key, SenderId};

fn level_meter() -> &'static PathBuf {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT.get_or_init(|| {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../payloads");
        let status = Command::new(env!("CARGO"))
            .args(["build", "--release", "-q", "-p", "level_meter"])
            .current_dir(&dir)
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env_remove("CARGO_BUILD_TARGET")
            .env_remove("CARGO_TARGET_DIR")
            .status()
            .expect("cargo runs");
        assert!(status.success(), "building level_meter failed");
        dir.join("target/wasm32-unknown-unknown/release/level_meter.wasm")
    })
}

fn binding_file(name: &str, rate: u16, key: &'static str) -> PathBuf {
    let mut b = Binding::new([0; 32], Version(1));
    let mic = MicConfig::new(rate, 20, AudioMode::Levels, 10, rate / 100).unwrap();
    b.bind(
        "audio",
        Target::Local(RouteSource::Samples(SampleSource::Microphone(mic))),
    )
    .unwrap();
    b.bind("level", Target::Key(Key::from_static(key))).unwrap();
    let mut out = [0u8; BINDING_MAX];
    let n = b.encode(&mut out).unwrap();
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::write(&path, &out[..n]).unwrap();
    path
}

fn run(args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_payload-sim"))
        .arg(level_meter())
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("runs");
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    (out.status.success(), text)
}

#[test]
fn bd6_a_binding_wires_the_microphone_to_the_payload_and_rebinding_changes_it() {
    let a = binding_file("bind-a.bin", 16_000, "lab/level");
    let (ok, out) = run(&[
        "--virtual",
        "--until-ms",
        "1000",
        "--bind",
        a.to_str().unwrap(),
    ]);
    assert!(ok, "{out}");
    assert!(out.contains("subscriptions: [ch/audio]"), "{out}");
    // Ten 10 ms blocks per batch: ten batches a second, block indices
    // advancing by ten.
    let lines: Vec<&str> = out
        .lines()
        .filter(|l| l.contains("publish lab/level rms "))
        .collect();
    assert!(lines.len() >= 9, "{out}");
    assert!(lines[0].contains("blocks 10 from 0"), "{}", lines[0]);
    assert!(lines[1].contains("blocks 10 from 10"), "{}", lines[1]);
    // The same image, rebound: another rate and another key.
    let b = binding_file("bind-b.bin", 8_000, "other/level");
    let (ok, out) = run(&[
        "--virtual",
        "--until-ms",
        "500",
        "--bind",
        b.to_str().unwrap(),
    ]);
    assert!(ok, "{out}");
    assert!(out.contains("publish other/level rms "), "{out}");
    assert!(!out.contains("lab/level"), "{out}");
}

#[test]
fn bd6_without_a_binding_the_required_channel_fails_the_load() {
    let (ok, out) = run(&["--virtual", "--until-ms", "100"]);
    assert!(!ok);
    assert!(
        out.contains("channel audio is required and not bound"),
        "{out}"
    );
}

/// The SC6 fixture: fake microphone levels at 8 kHz on `app/mic`.
fn sensor_stream() -> PathBuf {
    let wat = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/test/fixtures/sensor_stream.wat"),
    )
    .unwrap();
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("sensor_stream.wasm");
    std::fs::write(&path, wat::parse_str(&wat).unwrap()).unwrap();
    path
}

fn remote_binding(name: &str, node: SenderId) -> PathBuf {
    let mut b = Binding::new([0; 32], Version(1));
    b.bind(
        "audio",
        Target::Remote {
            node,
            key: Key::from_static("app/mic"),
        },
    )
    .unwrap();
    b.bind("level", Target::Key(Key::from_static("lab/level")))
        .unwrap();
    let mut out = [0u8; BINDING_MAX];
    let n = b.encode(&mut out).unwrap();
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::write(&path, &out[..n]).unwrap();
    path
}

/// PR3: a channel bound to a key on another node takes that node's
/// publications only. The `--peer` board (node 0..02, the default node
/// plus one) runs the microphone levels route on `app/mic`.
#[test]
fn pr3_a_remote_channel_takes_the_named_node_only() {
    let peer = sensor_stream();
    let good = remote_binding("bind-remote.bin", SenderId([0, 0, 0, 0, 0, 0, 0, 2]));
    let args = |b: &PathBuf| {
        vec![
            "--virtual".to_string(),
            "--until-ms".into(),
            "1500".into(),
            "--peer".into(),
            peer.to_string_lossy().into_owned(),
            "--bind".into(),
            b.to_string_lossy().into_owned(),
        ]
    };
    let run_with = |a: Vec<String>| {
        let out = Command::new(env!("CARGO_BIN_EXE_payload-sim"))
            .arg(level_meter())
            .args(a)
            .stdin(Stdio::null())
            .output()
            .expect("runs");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    };
    let (ok, out) = run_with(args(&good));
    assert!(ok, "{out}");
    assert!(out.contains("subscriptions: [app/mic]"), "{out}");
    assert!(out.contains("publish lab/level rms "), "{out}");
    let other = remote_binding("bind-other.bin", SenderId([9; 8]));
    let (ok, out) = run_with(args(&other));
    assert!(ok, "{out}");
    assert!(!out.contains("publish lab/level"), "{out}");
}

/// AD2: the same payload on a local PCM source through the levels adapter
/// (16 kHz PCM, 160-sample blocks): one levels message per ten blocks, the
/// block indices as a levels route would number them.
#[test]
fn ad2_pcm_through_the_levels_adapter_reaches_a_levels_channel() {
    use middleware::loader::binding::{Adapter, Endpoint};
    let mut b = Binding::new([0; 32], Version(1));
    let pcm = MicConfig::new(16_000, 0, AudioMode::Pcm, 160, 0).unwrap();
    b.bind(
        "audio",
        Target::Adapted {
            inner: Endpoint::Local(RouteSource::Samples(SampleSource::Microphone(pcm))),
            adapter: Adapter::PcmToLevels { block: 160 },
        },
    )
    .unwrap();
    b.bind("level", Target::Key(Key::from_static("lab/level")))
        .unwrap();
    let mut out = [0u8; BINDING_MAX];
    let n = b.encode(&mut out).unwrap();
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("bind-adapted.bin");
    std::fs::write(&path, &out[..n]).unwrap();
    let (ok, out) = run(&[
        "--virtual",
        "--until-ms",
        "300",
        "--bind",
        path.to_str().unwrap(),
    ]);
    assert!(ok, "{out}");
    // Each 10 ms PCM batch completes one 160-sample block.
    let lines: Vec<&str> = out
        .lines()
        .filter(|l| l.contains("publish lab/level rms "))
        .collect();
    assert!(lines.len() >= 20, "{out}");
    assert!(lines[0].contains("blocks 1 from 0"), "{}", lines[0]);
    assert!(lines[1].contains("blocks 1 from 1"), "{}", lines[1]);
}
