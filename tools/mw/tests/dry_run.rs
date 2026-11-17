//! `mw --dry-run` for every command, as a process (design.md M54).
//!
//! `PATH` is emptied, so if a dry run started any program (nix, kubectl,
//! ssh, curl, just) it would fail; the cached kubeconfig is absent and
//! `MW_KUBECONFIG` unset, so plans that need one show the fetch step
//! without running it. Nothing is contacted.

use std::path::PathBuf;
use std::process::{Command, Output};

fn scratch(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("mw-dry-run-{name}"));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn mw(name: &str, args: &[&str], extra_env: &[(&str, &str)]) -> (Output, PathBuf) {
    let repo = scratch(name);
    let mut c = Command::new(env!("CARGO_BIN_EXE_mw"));
    c.arg("--dry-run")
        .args(args)
        .env_clear()
        .env("PATH", "")
        .env("HOME", "/home/test")
        .env("MW_REPO", &repo)
        .current_dir(&repo);
    for (k, v) in extra_env {
        c.env(k, v);
    }
    let out = c.output().unwrap();
    (out, repo)
}

fn ok(name: &str, args: &[&str], extra_env: &[(&str, &str)]) -> String {
    let (out, repo) = mw(name, args, extra_env);
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // A dry run writes nothing: the scratch repository stays empty.
    assert_eq!(
        std::fs::read_dir(&repo).unwrap().count(),
        0,
        "{args:?} wrote files"
    );
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn m54_every_command_dry_runs_without_side_effects() {
    let build = ok("build", &["build", "demo-echo"], &[]);
    assert!(build.contains(" 1. (cd "), "{build}");
    assert!(
        build.contains("nix build -L .#image-demo-echo --out-link "),
        "{build}"
    );

    let load = ok(
        "load",
        &["load", "demo-echo"],
        &[("MW_LAB_NODES", "10.9.0.1")],
    );
    assert!(
        load.contains("root@10.9.0.1 k3s ctr -n k8s.io images import /tmp/mw-demo-echo.tar"),
        "{load}"
    );
    assert_eq!(load.lines().count(), 3);

    let apply = ok("apply", &["apply", "demo-echo", "--replicas", "3"], &[]);
    assert!(
        apply.contains(
            "/home/test/src/k8s-logstash-logging/envs/helm/nix/run-local-kvm.sh kubeconfig > "
        ),
        "{apply}"
    );
    assert!(apply.contains("--print-manifests --namespace apps --image localhost/mw/demo-echo:latest --pull-policy Never | kubectl --kubeconfig "), "{apply}");
    assert!(
        apply.contains("scale deployment/demo-echo --replicas 3"),
        "{apply}"
    );

    let zenoh = ok(
        "zenoh",
        &["zenoh", "apply", "demo-echo"],
        &[("MW_KUBECONFIG", "/k.yaml")],
    );
    assert!(
        zenoh.contains("never written to disk> | kubectl --kubeconfig /k.yaml -n apps apply -f -"),
        "{zenoh}"
    );
    assert!(
        zenoh.contains("--example zenoh_router_manifests"),
        "{zenoh}"
    );

    let status = ok(
        "status",
        &["status", "demo-echo"],
        &[("MW_KUBECONFIG", "/k.yaml")],
    );
    assert_eq!(
        status,
        " 1. kubectl --kubeconfig /k.yaml -n apps get deployments,pods,services -l app.kubernetes.io/managed-by=mw-service,app.kubernetes.io/name=demo-echo -o wide\n"
    );

    let logs = ok(
        "logs",
        &["logs", "demo-echo", "--since", "5m"],
        &[("MW_KUBECONFIG", "/k.yaml")],
    );
    assert!(logs.contains("logs -l app.kubernetes.io/managed-by=mw-service,app.kubernetes.io/name=demo-echo --all-containers=true"), "{logs}");

    let down = ok(
        "down",
        &["down", "demo-echo"],
        &[("MW_KUBECONFIG", "/k.yaml")],
    );
    assert!(down.contains(" delete deployments.apps,"), "{down}");

    let trace = ok(
        "trace",
        &["trace", "4bf92f3577b34da6a3ce929d0e0e4736"],
        &[("MW_KUBECONFIG", "/k.yaml")],
    );
    assert!(
        trace.contains("port-forward --address 127.0.0.1 svc/tempo 13200:3200 &"),
        "{trace}"
    );
    assert!(
        trace.contains("http://127.0.0.1:13200/api/traces/4bf92f3577b34da6a3ce929d0e0e4736"),
        "{trace}"
    );

    let search = ok(
        "search",
        &["trace", "req-1"],
        &[("MW_TEMPO_URL", "http://t:3200")],
    );
    assert!(search.starts_with(" 1. curl "), "{search}");
    assert!(
        search.contains("/api/search?tags=guid%3Ax-request-id%3D%22req-1%22"),
        "{search}"
    );

    let robot = ok(
        "robot",
        &["robot", "enroll", "r1", "--robot-host", "10.9.0.20"],
        &[("MW_KUBECONFIG", "/k.yaml")],
    );
    assert!(
        robot.contains("KUBECONFIG=/k.yaml just helm::robot-join r1 unix:uid:1000"),
        "{robot}"
    );
    assert!(
        robot.contains("root@10.9.0.20:/var/lib/mw-robot/join-token"),
        "{robot}"
    );
}

#[test]
fn m54_usage_and_plan_errors_exit_2() {
    let (out, _) = mw("bad-name", &["build", "Bad_Name"], &[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a valid name"));
    let (out, _) = mw("bad-cmd", &["frobnicate"], &[]);
    assert_eq!(out.status.code(), Some(2));
    let (out, _) = mw("help", &["--help"], &[]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("robot enroll <name>"));
}
