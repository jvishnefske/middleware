//! `mw-release sign`: the signed release written to a file, as `mwctl os
//! update <image> <cert>` reads it, verifiable with the root key.

use std::process::Command;

use middleware::loader::release::{CertKind, SignedCert, verify_cert};
use middleware::loader::trust::PublicKey;
use sha2::{Digest, Sha256};

fn scratch(name: &str) -> std::path::PathBuf {
    let d = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("mw-release-sign-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn sign_writes_a_root_signed_os_cert_and_publishes_nothing() {
    let dir = scratch("os");
    let seed = [42u8; 32];
    std::fs::write(dir.join("key"), seed).unwrap();
    let mut image = b"hsqs".to_vec();
    image.resize(5000, 7);
    std::fs::write(dir.join("root.squashfs"), &image).unwrap();
    let cert_path = dir.join("root.cert");

    let out = Command::new(env!("CARGO_BIN_EXE_mw-release"))
        .args(["sign", "--channel", "stable", "--target", "linux-x86_64"])
        .args(["--kind", "os", "--version", "1700000000", "--key"])
        .arg(dir.join("key"))
        .arg("--out")
        .arg(&cert_path)
        .arg(dir.join("root.squashfs"))
        // No updates channel is configured: sign must not need one.
        .env_remove("MW_UPDATES_HOST")
        .env_remove("MW_UPDATES_ENDPOINT")
        .env_remove("MW_UPDATES_ADDR")
        .output()
        .expect("run mw-release");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let body = std::fs::read(&cert_path).unwrap();
    let signed = SignedCert::decode(&body).expect("[cert][sig]");
    let c = signed.cert;
    assert_eq!(c.kind, CertKind::Os);
    assert_eq!((c.target, c.channel), ("linux-x86_64", "stable"));
    assert_eq!(c.version.0, 1_700_000_000);
    assert_eq!(c.len as usize, image.len());
    assert_eq!(c.sha256, <[u8; 32]>::from(Sha256::digest(&image)));
    let root = PublicKey::Ed25519(
        ed25519_dalek::SigningKey::from_bytes(&seed)
            .verifying_key()
            .to_bytes(),
    );
    assert!(verify_cert(&root, &signed), "signed by the root key");
    let other = PublicKey::Ed25519(
        ed25519_dalek::SigningKey::from_bytes(&[1; 32])
            .verifying_key()
            .to_bytes(),
    );
    assert!(!verify_cert(&other, &signed));
}

#[test]
fn sign_refuses_an_os_image_that_is_not_a_squashfs_and_needs_out() {
    let dir = scratch("refuse");
    std::fs::write(dir.join("key"), [42u8; 32]).unwrap();
    std::fs::write(dir.join("not-a-root"), b"ELF....").unwrap();
    let run = |extra: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_mw-release"))
            .args(["sign", "--channel", "stable", "--target", "linux-x86_64"])
            .args(["--kind", "os", "--version", "5", "--key"])
            .arg(dir.join("key"))
            .args(extra)
            .arg(dir.join("not-a-root"))
            .output()
            .expect("run")
    };
    let out_file = dir.join("c").to_string_lossy().into_owned();
    assert!(!run(&["--out", &out_file]).status.success(), "not hsqs");
    assert!(!dir.join("c").exists());
    assert!(!run(&[]).status.success(), "no --out");
}
