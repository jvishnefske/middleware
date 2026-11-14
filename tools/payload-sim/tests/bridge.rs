//! ABI 6 on the fake two-port board: `payloads/templates/bridge.wat`
//! installs its rules, matching arrivals on link 0 are forwarded to link 1
//! (and back for `back/ack`), others are not, and `ctl/bridge 0` stops the
//! demo rule so the next `demo/...` arrival is dropped.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn run(stdin: &str) -> String {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("bridge.wasm");
    let wat = include_str!("../../../payloads/templates/bridge.wat");
    std::fs::write(&path, wat::parse_str(wat).expect("WAT")).expect("write");
    let mut child = Command::new(env!("CARGO_BIN_EXE_payload-sim"))
        .arg(&path)
        .args(["--virtual", "--until-ms", "1500", "--memory-bytes", "16384"])
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
fn cl2_bridge_rules_forward_between_the_fake_links() {
    let out = run(concat!(
        "demo/a hi\n",
        "demox no\n",
        "!link 1 back/ack ok\n",
        "!link 1 demo/a wrong-way\n",
        "ctl/bridge 0\n",
        "demo/b late\n",
    ));
    let has = |s: &str| assert!(out.contains(s), "missing {s:?} in\n{out}");
    has("bridge16 link0 demo/** -> link1\n");
    has("bridge17 link1 back/ack -> link0\n");
    has("bridge: up");
    has("link1 tx demo/a hi\n");
    has("link0 tx back/ack ok\n");
    has("drop demox\n");
    has("drop demo/a\n");
    has("bridge16 stop\n");
    has("drop demo/b\n");
    assert!(!out.contains("link1 tx demo/b"), "{out}");
    assert!(!out.contains("tx demox"), "{out}");
}
