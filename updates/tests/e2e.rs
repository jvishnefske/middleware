//! UP4–UP8 end to end, on loopback: the updates router (router mode, the
//! rendered config with `usrpwd` and the ACL), the `mw-updates` server, a
//! publisher, and the updater reaching the router over its `ws/` listener
//! through the WebSocket bridge and installing into a real posix-node over
//! the node's local serial link.
//!
//! Network conduct: every listener binds `127.0.0.1` on a port reserved by
//! binding port 0; no scouting.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use middleware::loader::release::CertKind;
use middleware::loader::trust::{TrustState, Version};
use middleware::zenoh::wsbridge::WsTarget;
use middleware::{SenderId, loader::PayloadStatus};
use mw_posix_node::service::CHIP;
use mw_updates::client::{Client, Route, session_config};
use mw_updates::publish::{prepare, publish, sign};
use mw_updates::router::{Users, render};
use mw_updates::run::pass;
use mw_updates::store::Store;
use mw_updates::updater::Opener;
use mwctl::device::Device;
use mwctl::error::Error;
use mwctl::link::{Link, Timeouts};
use mwctl::sign::Seed;

mod common;
use common::{Chan, demo};

const NODE: SenderId = SenderId([0x5e, 0x11, 0x0d, 0xe0, 0x2a, 0x9c, 0x41, 0x08]);
const CHANNEL: &str = "test";

fn root() -> Seed {
    Seed::new([0x42; 32])
}

fn tmp(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("updates-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

async fn port() -> u16 {
    tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn creds(user: &str) -> (String, String) {
    (user.to_owned(), format!("{user}-password-0123456789"))
}

/// The router with the rendered config, its listeners moved to loopback.
async fn router(dir: &std::path::Path) -> (zenoh::Session, String, u16) {
    let users = Users {
        server: vec!["mw-updates".into()],
        publishers: vec!["release".into()],
        updaters: vec!["pi-a".into()],
    };
    let dictionary = dir.join("dictionary");
    let lines: String = ["mw-updates", "release", "pi-a"]
        .iter()
        .map(|u| {
            let (u, p) = creds(u);
            format!("{u}:{p}\n")
        })
        .collect();
    std::fs::write(&dictionary, lines).unwrap();
    let text = render(&users, &dictionary.display().to_string());
    let mut config = zenoh::Config::from_json5(&text).expect("config");
    let (tcp, ws) = (port().await, port().await);
    config
        .insert_json5(
            "listen/endpoints",
            &format!(r#"["tcp/127.0.0.1:{tcp}", "ws/127.0.0.1:{ws}"]"#),
        )
        .unwrap();
    let session = zenoh::open(config).await.expect("router");
    (session, format!("tcp/127.0.0.1:{tcp}"), ws)
}

struct Local(Chan);

impl Opener for Local {
    type Link = Link<Chan>;
    fn open(&mut self) -> Result<Device<Self::Link>, Error> {
        Ok(Device::new(Link::new(self.0.clone(), Timeouts::default())))
    }
}

/// A posix-node in this process, its local link as an opener.
async fn node(state: &std::path::Path) -> (mw_posix_node::PosixNode, Local) {
    let (pi, chan) = common::node(state, NODE).await;
    (pi, Local(chan))
}

async fn publisher(tcp: &str) -> Client {
    Client::connect(&Route::Direct(tcp.into()), Some(&creds("release")))
        .await
        .expect("publisher")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn up4_up8_publish_pull_install_refuse_downgrades_and_forgeries() {
    let dir = tmp("e2e");
    let (_router, tcp, ws) = router(&dir).await;

    // The server, as `mw-updates` (credentials of the server role).
    let store = Arc::new(Store::open(dir.join("data"), root().public_key()).unwrap());
    let server = zenoh::open(session_config(&tcp, Some(&creds("mw-updates"))).unwrap())
        .await
        .expect("server session");
    let status = server
        .declare_subscriber("@mw/updates/status/**")
        .await
        .unwrap();
    tokio::spawn(mw_updates::server::serve(server.clone(), store.clone()));

    // The updater's session: ws/ through the bridge (Traefik's place).
    let updater = Client::connect(
        &Route::Bridge(WsTarget {
            url: format!("ws://127.0.0.1:{ws}/zenoh"),
            connect_addr: None,
            roots: None,
        }),
        Some(&creds("pi-a")),
    )
    .await
    .expect("updater over ws");

    // A node claimed by the root over its local link (provisioning).
    let (_pi, mut local) = node(&dir.join("node")).await;
    tokio::task::block_in_place(|| {
        let mut d = local.open().unwrap();
        d.trust_claim(root().public_key()).expect("claim");
    });
    let mut devices = vec![("local".to_owned(), local)];

    // Nothing published yet.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let r = pass(
        &updater,
        CHANNEL,
        &root().public_key(),
        &mut devices,
        "pi-a",
        Duration::from_secs(5),
    )
    .await;
    assert!(r.iter().all(|r| r.result.starts_with("skipped")), "{r:?}");

    // Publish a payload release, signed by the root.
    let p = publisher(&tcp).await;
    let img = demo();
    let v10 = prepare(
        CertKind::Payload,
        &img,
        Some(Version(10)),
        SystemTime::now(),
    )
    .unwrap();
    let release = sign(CertKind::Payload, CHANNEL, CHIP, &v10, &root()).unwrap();
    publish(&p, &release, &v10.image, Duration::from_secs(10))
        .await
        .expect("published");

    // The updater pulls and installs it, and starts it.
    let r = pass(
        &updater,
        CHANNEL,
        &root().public_key(),
        &mut devices,
        "pi-a",
        Duration::from_secs(5),
    )
    .await;
    let payload = r.iter().find(|r| r.kind == "payload").unwrap();
    assert_eq!(
        (payload.result.as_str(), payload.version),
        ("installed", Some(10)),
        "{r:?}"
    );
    let fw = r.iter().find(|r| r.kind == "fw").unwrap();
    assert!(
        fw.result.starts_with("skipped"),
        "posix-node has no firmware updater: {fw:?}"
    );
    tokio::task::block_in_place(|| {
        let mut d = devices[0].1.open().unwrap();
        let t = d.trust_info().unwrap();
        assert!(matches!(
            t.state,
            TrustState::Claimed {
                min_version: Version(10),
                ..
            }
        ));
        let info = d.info().unwrap();
        assert_eq!(info.status, PayloadStatus::Running);
    });
    // Its reports reached the server's status key.
    let s = tokio::time::timeout(Duration::from_secs(5), status.recv_async())
        .await
        .expect("a status report")
        .unwrap();
    assert!(s.key_expr().as_str().ends_with("/status/pi-a"));
    // ... and only under its own name: a report as another node is dropped.
    updater.status("pi-b", "{}".into()).await.ok();
    let until = tokio::time::Instant::now() + Duration::from_secs(1);
    while let Ok(Ok(s)) = tokio::time::timeout_at(until, status.recv_async()).await {
        assert!(
            !s.key_expr().as_str().ends_with("/status/pi-b"),
            "a report as pi-b reached the server"
        );
    }

    // Again: up to date.
    let r = pass(
        &updater,
        CHANNEL,
        &root().public_key(),
        &mut devices,
        "pi-a",
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(
        r.iter().find(|r| r.kind == "payload").unwrap().result,
        "up-to-date"
    );

    // A downgrade and a forged release are refused by the server.
    let v9 = prepare(CertKind::Payload, &img, Some(Version(9)), SystemTime::now()).unwrap();
    let old = sign(CertKind::Payload, CHANNEL, CHIP, &v9, &root()).unwrap();
    assert!(
        publish(&p, &old, &v9.image, Duration::from_secs(2))
            .await
            .is_err()
    );
    let v11 = prepare(
        CertKind::Payload,
        &img,
        Some(Version(11)),
        SystemTime::now(),
    )
    .unwrap();
    let forged = sign(CertKind::Payload, CHANNEL, CHIP, &v11, &Seed::new([9; 32])).unwrap();
    assert!(
        publish(&p, &forged, &v11.image, Duration::from_secs(2))
            .await
            .is_err()
    );

    // The ACL: an updater cannot publish, even a release the root signed.
    let good11 = sign(CertKind::Payload, CHANNEL, CHIP, &v11, &root()).unwrap();
    updater.publish(&good11, &v11.image).await.ok();
    tokio::time::sleep(Duration::from_secs(1)).await;
    let latest = p.latest(CHANNEL, CHIP).await.unwrap();
    assert_eq!(latest, vec![release.clone()], "nothing changed");

    // The publisher's v11 lands, and the updater installs it.
    publish(&p, &good11, &v11.image, Duration::from_secs(10))
        .await
        .expect("v11");
    let r = pass(
        &updater,
        CHANNEL,
        &root().public_key(),
        &mut devices,
        "pi-a",
        Duration::from_secs(5),
    )
    .await;
    let payload = r.iter().find(|r| r.kind == "payload").unwrap();
    assert_eq!(
        (payload.result.as_str(), payload.version),
        ("installed", Some(11)),
        "{r:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn up8_no_credentials_no_session() {
    let dir = tmp("anon");
    let (_router, tcp, _ws) = router(&dir).await;
    assert!(
        zenoh::open(session_config(&tcp, None).unwrap())
            .await
            .is_err()
    );
    let wrong = ("pi-a".to_owned(), "not-the-password-at-all".to_owned());
    assert!(
        zenoh::open(session_config(&tcp, Some(&wrong)).unwrap())
            .await
            .is_err()
    );
}
