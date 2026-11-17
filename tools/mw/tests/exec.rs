//! The shell (`mw` without `--dry-run`) against fake programs (design.md
//! M55, M56).
//!
//! Each test puts small `/bin/sh` stand-ins for nix, kubectl, cargo, just
//! and curl first on the child's `PATH`. They record their argument vector
//! and standard input under `$FAKE_LOG` and print canned output, so the
//! real executor runs end to end with nothing contacted.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

/// Records argv (one line, space separated) to $FAKE_LOG/<prog>.argv and
/// stdin to $FAKE_LOG/<prog>.stdin when the args contain "-f -".
const KUBECTL: &str = r##"#!/bin/sh
echo "$*" >> "$FAKE_LOG/kubectl.argv"
case "$*" in *"-f -"*) cat >> "$FAKE_LOG/kubectl.stdin"; echo "---" >> "$FAKE_LOG/kubectl.stdin";; esac
"##;
/// `nix build -L .#<attr> --out-link <path>`: for a binary attribute,
/// creates <path>/bin/<attr> printing a spec or manifests; exits
/// $FAKE_NIX_EXIT when set.
const NIX: &str = r##"#!/bin/sh
echo "$*" >> "$FAKE_LOG/nix.argv"
[ -n "$FAKE_NIX_EXIT" ] && exit "$FAKE_NIX_EXIT"
attr="${3#.#}"; out="$5"
mkdir -p "$out/bin"
printf '#!/bin/sh\necho "# %s $*"\n' "$attr" > "$out/bin/$attr"
chmod +x "$out/bin/$attr"
"##;
const CARGO: &str = r##"#!/bin/sh
echo "$*" >> "$FAKE_LOG/cargo.argv"
echo "kind: ConfigMap"
"##;
const JUST: &str = r##"#!/bin/sh
echo "$* KUBECONFIG=$KUBECONFIG PWD=$PWD" >> "$FAKE_LOG/just.argv"
cat "$FIXTURES/robot-join.txt"
"##;
const CURL: &str = r##"#!/bin/sh
for a; do url="$a"; done
echo "$url" >> "$FAKE_LOG/curl.argv"
case "$url" in */api/search*) cat "$FIXTURES/tempo-search.json";; *) cat "$FIXTURES/tempo-trace.json";; esac
"##;

struct Lab {
    root: PathBuf,
}

impl Lab {
    fn new(name: &str) -> Lab {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("mw-exec-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["bin", "log", "repo"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        for (prog, text) in [
            ("kubectl", KUBECTL),
            ("nix", NIX),
            ("cargo", CARGO),
            ("just", JUST),
            ("curl", CURL),
        ] {
            let p = root.join("bin").join(prog);
            std::fs::write(&p, text).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Lab { root }
    }

    fn repo(&self) -> PathBuf {
        self.root.join("repo")
    }

    fn log(&self, name: &str) -> String {
        std::fs::read_to_string(self.root.join("log").join(name)).unwrap_or_default()
    }

    fn mw(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let path = format!("{}:/usr/bin:/bin", self.root.join("bin").display());
        let mut c = Command::new(env!("CARGO_BIN_EXE_mw"));
        c.args(args)
            .env_clear()
            .env("PATH", path)
            .env("HOME", "/home/test")
            .env("MW_REPO", self.repo())
            .env("MW_KUBECONFIG", "/k.yaml")
            .env("FAKE_LOG", self.root.join("log"))
            .env("FIXTURES", FIXTURES)
            .current_dir(self.repo());
        for (k, v) in env {
            c.env(k, v);
        }
        c.output().unwrap()
    }
}

fn success(out: &Output) -> String {
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() && !p.is_symlink() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}

#[test]
fn m55_steps_run_as_argv_and_pipe_manifests() {
    let lab = Lab::new("apply");
    success(&lab.mw(&["apply", "demo-echo", "--replicas", "2"], &[]));
    let repo = lab.repo().display().to_string();
    assert_eq!(
        lab.log("nix.argv"),
        format!("build -L .#demo-echo --out-link {repo}/target/mw/bin/demo-echo\n")
    );
    assert_eq!(
        lab.log("kubectl.argv"),
        "--kubeconfig /k.yaml -n apps apply -f -\n--kubeconfig /k.yaml -n apps scale deployment/demo-echo --replicas 2\n"
    );
    // The binary's output reached kubectl's stdin, flags intact.
    assert_eq!(
        lab.log("kubectl.stdin"),
        "# demo-echo --print-manifests --namespace apps --image localhost/mw/demo-echo:latest --pull-policy Never\n---\n"
    );
}

#[test]
fn m55_the_first_failure_stops_the_run() {
    let lab = Lab::new("fail");
    let out = lab.mw(&["apply", "demo-echo"], &[("FAKE_NIX_EXIT", "3")]);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("step 1/2 failed: nix failed"), "{err}");
    assert_eq!(lab.log("kubectl.argv"), "");
}

#[test]
fn m56_zenoh_passwords_reach_only_kubectl() {
    let lab = Lab::new("zenoh");
    success(&lab.mw(&["zenoh", "apply", "demo-echo", "demo-caller"], &[]));
    let repo = lab.repo();
    let spec = std::fs::read_to_string(repo.join("target/mw/specs/demo-echo.yaml")).unwrap();
    assert_eq!(spec, "# demo-echo --print-spec\n");
    assert!(lab.log("cargo.argv").contains(&format!(
        "--example zenoh_router_manifests -- --namespace apps {}/target/mw/specs/demo-echo.yaml",
        repo.display()
    )));
    let stdin = lab.log("kubectl.stdin");
    let secrets = stdin.split("---\n").next().unwrap();
    let v: serde_json::Value = serde_json::from_str(secrets).unwrap();
    let dict = v["items"][0]["stringData"]["credentials.txt"]
        .as_str()
        .unwrap();
    let lines: Vec<&str> = dict.lines().collect();
    assert_eq!(lines.len(), 2);
    let mut passwords = Vec::new();
    for (i, svc) in ["demo-echo", "demo-caller"].iter().enumerate() {
        let (user, pw) = lines[i].split_once(':').unwrap();
        assert_eq!(&user, svc);
        assert_eq!(pw.len(), 48);
        assert!(pw.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(
            v["items"][i + 1]["stringData"]["credentials"],
            format!("{svc}\n{pw}\n")
        );
        passwords.push(pw.to_owned());
    }
    assert_ne!(passwords[0], passwords[1]);
    // Nowhere on disk except the fake kubectl's own stdin log.
    for f in files_under(&repo) {
        let text = std::fs::read(&f).unwrap();
        let text = String::from_utf8_lossy(&text);
        for pw in &passwords {
            assert!(
                !text.contains(pw.as_str()),
                "{} holds a password",
                f.display()
            );
        }
    }
    assert!(lab.log("kubectl.argv").ends_with(
        "rollout restart deployment/zenohd\n--kubeconfig /k.yaml -n apps rollout restart deployment -l app.kubernetes.io/managed-by=mw-service\n"
    ));
}

#[test]
fn m56_robot_enrollment_lands_in_the_robot_dir() {
    let lab = Lab::new("robot");
    let vm = lab.root.join("vm-share");
    let vm_s = vm.display().to_string();
    let out = success(&lab.mw(
        &["robot", "enroll", "r1", "--robot-dir", &vm_s],
        &[("MW_LAB_DIR", "/")],
    ));
    assert!(out.contains("enrolled robot r1"), "{out}");
    assert_eq!(
        lab.log("just.argv"),
        "helm::robot-join r1 unix:uid:1000 KUBECONFIG=/k.yaml PWD=/\n"
    );
    let mode = |n: &str| std::fs::metadata(vm.join(n)).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        std::fs::read_to_string(vm.join("join-token")).unwrap(),
        "7c0b6e1a-4a8d-4f61-9b36-2f1d0c9e8a77\n"
    );
    assert_eq!(mode("join-token"), 0o600);
    assert_eq!(
        std::fs::metadata(&vm).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let bundle = std::fs::read_to_string(vm.join("bundle.spiffe")).unwrap();
    assert!(bundle.contains("\"spiffe_sequence\": 1"));
    let env = std::fs::read_to_string(vm.join("enrollment.env")).unwrap();
    assert!(
        env.contains("MW_SPIRE_SERVER_ADDRESS=192.168.66.11\nMW_SPIRE_SERVER_PORT=30081\n"),
        "{env}"
    );
}

#[test]
fn m56_trace_prints_span_trees_from_tempo() {
    let lab = Lab::new("trace");
    let tempo = [("MW_TEMPO_URL", "http://tempo.test:3200")];
    let by_id = success(&lab.mw(&["trace", "4bf92f3577b34da6a3ce929d0e0e4736"], &tempo));
    assert!(
        by_id.starts_with("trace 4bf92f3577b34da6a3ce929d0e0e4736: 5 spans, 12.500 ms\n"),
        "{by_id}"
    );
    let by_request = success(&lab.mw(&["trace", "6e1b4bd2-35b4-4f3c-9f6e-0c1d2e3f4a5b"], &tempo));
    assert_eq!(by_request, by_id);
    let urls = lab.log("curl.argv");
    let urls: Vec<&str> = urls.lines().collect();
    assert_eq!(urls.len(), 3);
    assert!(
        urls[1].starts_with(
            "http://tempo.test:3200/api/search?tags=guid%3Ax-request-id%3D%226e1b4bd2"
        )
    );
    assert_eq!(
        urls[2],
        "http://tempo.test:3200/api/traces/4bf92f3577b34da6a3ce929d0e0e4736"
    );
}
