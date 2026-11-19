//! `!req` and `--node` (ABI 5): a WAT payload serves `app/echo`, replying
//! with its node id followed by the request body; the simulator prints
//! the served key, each reply, and `NotFound` for a key nothing serves.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

const SERVE_ECHO: &str = r#"
(module
  (import "mw" "serve" (func $serve (param i32 i32) (result i32)))
  (import "mw" "node_id" (func $node_id (param i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 32) "app/echo")
  (func (export "init") (drop (call $serve (i32.const 32) (i32.const 8))))
  (func (export "mw_alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "on_request") (param $slot i32) (param $req i32) (param $len i32)
        (param $meta i32) (param $reply i32) (param $cap i32) (result i32)
    (drop (call $node_id (local.get $reply)))
    (memory.copy (i32.add (local.get $reply) (i32.const 8)) (local.get $req) (local.get $len))
    (i32.add (local.get $len) (i32.const 8))))
"#;

fn run(args: &[&str], stdin: &str) -> String {
    // One file per test: the tests run in parallel, and rewriting a shared
    // file truncates it under another test's simulator.
    let test = std::thread::current()
        .name()
        .unwrap_or("main")
        .replace("::", "-");
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("serve_echo-{test}.wasm"));
    std::fs::write(&path, wat::parse_str(SERVE_ECHO).expect("WAT")).expect("write");
    let mut child = Command::new(env!("CARGO_BIN_EXE_payload-sim"))
        .arg(&path)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin.as_bytes())
        .expect("stdin");
    let out = child.wait_with_output().expect("runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8")
}

#[test]
fn s11_req_reaches_the_serving_payload_and_prints_the_reply() {
    let out = run(
        &["--virtual", "--node", "246f28aa01fe0000"],
        "!req app/echo 0102ff\n!req app/echo\n!req app/none 00\n",
    );
    assert!(out.contains("[     0 ms] serve app/echo\n"), "{out}");
    assert!(
        out.contains("reply app/echo 246f28aa01fe00000102ff\n"),
        "{out}"
    );
    assert!(out.contains("reply app/echo 246f28aa01fe0000\n"), "{out}");
    assert!(out.contains("reply app/none error NotFound\n"), "{out}");
}

#[test]
fn s11_the_default_node_is_one() {
    let out = run(&["--virtual"], "!req app/echo\n");
    assert!(out.contains("reply app/echo 0000000000000001\n"), "{out}");
}

/// MF3: `!req sys/manifest` is answered by the simulated board (with its
/// node id), not by the payload.
#[test]
fn mf3_the_board_answers_sys_manifest() {
    use middleware::loader::manifest::Manifest;
    let out = run(
        &["--virtual", "--node", "246f28aa01fe0000"],
        "!req sys/manifest\n",
    );
    let hex = out
        .lines()
        .find_map(|l| l.split("reply sys/manifest ").nth(1))
        .unwrap_or_else(|| panic!("no reply in {out}"));
    let bytes: Vec<u8> = (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("hex"))
        .collect();
    let m = Manifest::decode(&bytes).expect("manifest");
    assert_eq!(
        m.node.map(|n| n.0),
        Some([0x24, 0x6f, 0x28, 0xaa, 0x01, 0xfe, 0, 0])
    );
    assert!(m.imu.is_some() && m.mic.is_some());
}
