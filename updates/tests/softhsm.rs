//! UP3/UP4 with the root in a PKCS#11 token (SoftHSM2 standing in for the
//! TPM's `mw-release` key): a P-256 key generated in the token signs a
//! release; the server's store accepts it against the token's public key,
//! and a posix-node claimed with that key installs it over its local link.
//!
//! The token needs `SOFTHSM2_CONF` set before the module loads, and the
//! crates forbid `unsafe` (so no `set_var`): [`softhsm_root`] runs
//! [`softhsm_root_child`] in a child process with it. Skipped without
//! SoftHSM2 unless `MWCTL_REQUIRE_SOFTHSM` is set (CI).

use std::path::PathBuf;
use std::time::SystemTime;

use middleware::loader::PayloadStatus;
use middleware::loader::release::CertKind;
use middleware::loader::trust::{KeyKind, TrustState, Version};
use mw_posix_node::service::CHIP;
use mw_updates::publish::{prepare, sign};
use mw_updates::store::Store;
use mw_updates::updater::install_payload;
use mwctl::device::Device;
use mwctl::link::{Link, Timeouts};
use mwctl::pkcs11::{self, Location, Pkcs11Signer};
use mwctl::sign::Signer;

mod common;

fn module() -> Option<PathBuf> {
    [
        "/usr/lib/softhsm/libsofthsm2.so",
        "/usr/lib/x86_64-linux-gnu/softhsm/libsofthsm2.so",
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|p| p.exists())
}

#[test]
fn softhsm_root() {
    let Some(_) = module() else {
        assert!(
            std::env::var_os("MWCTL_REQUIRE_SOFTHSM").is_none(),
            "MWCTL_REQUIRE_SOFTHSM is set but SoftHSM2 is not installed"
        );
        eprintln!("SKIPPED: SoftHSM2 is not installed (apt install softhsm2)");
        return;
    };
    let dir = std::env::temp_dir().join(format!("mw-updates-softhsm-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("tokens")).unwrap();
    let conf = dir.join("softhsm2.conf");
    std::fs::write(
        &conf,
        format!(
            "directories.tokendir = {}\nobjectstore.backend = file\nlog.level = ERROR\n",
            dir.join("tokens").display()
        ),
    )
    .unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "softhsm_root_child", "--ignored", "--nocapture"])
        .env("SOFTHSM2_CONF", &conf)
        .env("MW_TEST_SO_PIN", "so-test-0000")
        .env("MW_TEST_USER_PIN", "user-test-1234")
        .env("MW_TEST_DIR", &dir)
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{text}");
    assert!(text.contains("1 passed"), "the child ran the test: {text}");
}

#[test]
#[ignore = "run by softhsm_root with SOFTHSM2_CONF set"]
fn softhsm_root_child() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let dir = PathBuf::from(std::env::var("MW_TEST_DIR").expect("run through softhsm_root"));
    let module = module().unwrap();
    let so = pkcs11::pin("MW_TEST_SO_PIN").unwrap();
    let user = pkcs11::pin("MW_TEST_USER_PIN").unwrap();
    pkcs11::init_token(&module, "mw-test", &so, &user).unwrap();
    let at = Location {
        module,
        token: Some("mw-test".into()),
    };
    let public = pkcs11::generate(&at, pkcs11::DEFAULT_LABEL, KeyKind::P256, &user).unwrap();
    let signer = Pkcs11Signer::open(&at, pkcs11::DEFAULT_LABEL, &user).unwrap();
    assert_eq!(signer.public_key(), public);

    // The token signs a release; the store (holding only the public key)
    // takes it.
    let img = common::demo();
    let prepared = prepare(CertKind::Payload, &img, Some(Version(7)), SystemTime::now()).unwrap();
    let release = sign(CertKind::Payload, "test", CHIP, &prepared, &signer).unwrap();
    let store = Store::open(dir.join("data"), public).unwrap();
    store
        .publish("test", CHIP, &release, &prepared.image)
        .expect("store");
    assert_eq!(store.latest("test", CHIP), vec![release.clone()]);

    // A posix-node claimed with the token's key installs it.
    rt.block_on(async {
        let state = dir.join("node");
        let (_pi, chan) = common::node(&state, middleware::SenderId([3; 8])).await;
        tokio::task::block_in_place(|| {
            let mut d = Device::new(Link::new(chan, Timeouts::default()));
            d.trust_claim(public).unwrap();
            install_payload(&mut d, &release, &prepared.image).expect("install");
            let t = d.trust_info().unwrap();
            assert!(matches!(
                t.state,
                TrustState::Claimed {
                    min_version: Version(7),
                    ..
                }
            ));
            assert_eq!(d.info().unwrap().status, PayloadStatus::Running);
            // The same release again: refused (no replay).
            assert!(install_payload(&mut d, &release, &prepared.image).is_err());
        });
    });
}
