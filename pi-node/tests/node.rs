//! The node end to end, in one process: the device thread on a state
//! directory under the build's temporary directory, the in-process gateway
//! on the other end of its `ChannelIo` pair, and a "cluster" Zenoh session
//! listening on one explicit 127.0.0.1 port (reserved by binding port 0,
//! never probed) that the node's session connects to. A local link (the
//! second `ChannelIo` pair, what `MW_PI_NODE_LOCAL_SERIAL` opens on a
//! host) carries requests on the payload's served keys, which `gw/sys`
//! does not pass. No serial port is opened.
//!
//! A signed payload (the WAT fixture, signed with the golden vector's test
//! key) is claimed for, uploaded and started through `gw/sys` as the
//! service runner calls it; its tick publications arrive on the cluster
//! session, a cluster sample on a downlink filter reaches it and comes
//! back, and its ABI 5 served key answers with the node id. After a
//! restart on the same state directory it starts by itself and a replayed
//! commit is denied.

use std::path::PathBuf;
use std::time::Duration;

use middleware::loader::{Caps, DeviceInfo, PayloadStatus, keys};
use middleware::serial::link::{INBOX_BYTES, LinkError, SerialLink};
use middleware::transport::{
    HandlerError, Publisher, Request, RequestMeta, RequestOptions, Requester, Subscribe, Subscriber,
};
use middleware::zenoh::{SessionConfig, ZenohSubscriber, ZenohTransport};
use middleware::{Key, KeyFilter, SenderId};
use mw_gateway::bridge::{SysAllow, encode_sys_request};
use mw_gateway::io::{ChannelIo, pair};
use mw_gateway::{GatewayConfig, SysForward, service};
use mw_pi_node::service::CHIP;
use mw_pi_node::shell::Ended;
use mw_pi_node::{PiNode, facts, limits, open_state_flash, start};
use mw_service::{Ctx, Links, Served};
use mwctl::plan::upload_plan;
use mwctl::sign::{Seed, sign_commit};
use middleware::loader::trust::Version;

const NODE: SenderId = SenderId([0x5e, 0x11, 0x0d, 0xe0, 0x2a, 0x9c, 0x41, 0x07]);
const TIMEOUT: Duration = Duration::from_secs(15);

/// The golden vector's key (`web/test/fixtures/trust-vector.json`, TEST
/// KEY ONLY: seed 00..1f).
fn test_seed() -> Seed {
    let mut b = [0u8; 32];
    for (i, x) in b.iter_mut().enumerate() {
        *x = i as u8;
    }
    Seed::new(b)
}

fn demo() -> Vec<u8> {
    wat::parse_str(include_str!("fixtures/pi_demo.wat")).expect("WAT")
}

fn state_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("pi-node-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// The cluster session (listening on a fresh loopback port) and a session
/// for the node connected to it.
async fn zenoh_pair() -> (ZenohTransport, ZenohTransport) {
    let port = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port();
    let endpoint = format!("tcp/127.0.0.1:{port}");
    let cluster = SessionConfig::new().listen(endpoint.clone()).build().expect("config");
    let node = SessionConfig::new().connect(endpoint).build().expect("config");
    let cluster = ZenohTransport::open(cluster).await.expect("open cluster");
    let node = ZenohTransport::open(node).await.expect("open node");
    (cluster, node)
}

/// The gateway's settings for the test: the loader, trust and start keys
/// allowed through `gw/sys` (by default only `sys/info` and `sys/log`
/// are), `app/pi/in` relayed down to the node.
fn gateway_config() -> GatewayConfig {
    let allow = [
        keys::INFO,
        keys::LOG,
        keys::TRUST_INFO,
        keys::TRUST_CLAIM,
        keys::BEGIN,
        keys::CHUNK,
        keys::COMMIT,
        keys::START,
    ];
    GatewayConfig::from_env(&|k| match k {
        "MW_GATEWAY_DOWNLINK" => Some("app/pi/in".into()),
        _ => None,
    })
    .map(|c| GatewayConfig { sys_allow: SysAllow::new(allow).expect("allow"), ..c })
    .expect("config")
}

/// `gw/sys` as the service runner calls it.
struct Sys(Vec<Served>);

impl Sys {
    fn new(sys: SysForward) -> Self {
        let (spec, served) = service(&gateway_config(), sys).expect("service").into_parts();
        assert_eq!(spec.serves, ["gw/sys"]);
        Sys(served)
    }

    async fn call(&mut self, key: Key<'_>, body: &[u8]) -> Result<Vec<u8>, HandlerError> {
        let handler = &mut self.0[0].handler;
        let mut req = vec![0u8; 1 + key.as_str().len() + body.len()];
        let n = encode_sys_request(key, body, &mut req).expect("body");
        let request = Request::new(Key::from_static("gw/sys"), &req[..n]);
        let mut links = Links::new();
        let mut ctx = Ctx::new(RequestMeta::NONE, None, &mut links);
        let mut out = vec![0u8; 2048];
        let len = handler.call(&mut ctx, &request, &mut out).await?;
        out.truncate(len);
        Ok(out)
    }

    /// Uploads `image` (signed as `version`, when given); the commit's reply.
    async fn upload(&mut self, image: &[u8], version: Option<u32>) -> Result<Vec<u8>, HandlerError> {
        let auth = version.and_then(|v| sign_commit(image, &test_seed(), Version(v)));
        let steps = upload_plan(image, None, auth).expect("plan");
        let (commit, transfer) = steps.split_last().expect("steps");
        for step in transfer {
            self.call(step.key, &step.body).await?;
        }
        self.call(commit.key, &commit.body).await
    }

    /// Asks `sys/info` until the gateway's link to the device answers;
    /// the raw reply (decode with `DeviceInfo::decode`).
    async fn info(&mut self) -> Vec<u8> {
        tokio::time::timeout(TIMEOUT, async {
            loop {
                if let Ok(b) = self.call(keys::INFO, &[]).await {
                    return b;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("sys/info")
    }
}

/// Waits for a publication on the subscriber; its payload.
async fn next(sub: &mut ZenohSubscriber) -> Vec<u8> {
    let mut buf = vec![0u8; 2048];
    let s = tokio::time::timeout(TIMEOUT, sub.recv(&mut buf)).await.expect("in time").expect("sample");
    s.payload.to_vec()
}

type Local = SerialLink<ChannelIo, INBOX_BYTES>;

async fn local_request(link: &mut Local, key: &'static str, body: &[u8]) -> Result<Vec<u8>, LinkError<mw_gateway::io::Closed>> {
    let mut out = vec![0u8; 1024];
    let r = tokio::time::timeout(
        TIMEOUT,
        link.request_with(Key::from_static(key), body, &mut out, &RequestOptions::default()),
    )
    .await
    .expect("in time")?;
    Ok(r.to_vec())
}

async fn node(state: &std::path::Path, session: ZenohTransport) -> (PiNode, Sys, Local) {
    let (local_test, local_node) = pair();
    let flash = open_state_flash(state).expect("state");
    let (pi, sys) = start(flash, facts(NODE), limits(1024 * 1024), gateway_config().bridge(), session, Some(local_node))
        .await
        .expect("start");
    (pi, Sys::new(sys), SerialLink::with_inbox(local_test))
}

#[tokio::test(flavor = "multi_thread")]
async fn pn8_a_signed_payload_through_gw_sys_publishes_to_zenoh_and_serves() {
    let state = state_dir("e2e");
    let (mut cluster, session) = zenoh_pair().await;
    let mut count = cluster.subscribe(KeyFilter::Exact(Key::from_static("app/pi/count"))).await.expect("subscribe");
    let mut out = cluster.subscribe(KeyFilter::Exact(Key::from_static("app/pi/out"))).await.expect("subscribe");
    let (pi, mut sys, mut local) = node(&state, session).await;

    // sys/info through gw/sys: the node is a board named after its platform.
    let raw = sys.info().await;
    let info = DeviceInfo::decode(&raw).expect("decode");
    assert_eq!(info.chip, CHIP);
    assert_eq!(info.node, Some(NODE));
    assert_eq!(info.caps, Caps::SERVE);
    assert_eq!(info.status, PayloadStatus::Empty);
    assert_eq!(info.capacity, 252 * 1024);

    // Claim, then: unsigned is denied, signed installs, start runs it.
    sys.call(keys::TRUST_CLAIM, &test_seed().public_key().0).await.expect("claim");
    let image = demo();
    assert_eq!(sys.upload(&image, None).await, Err(HandlerError::Denied));
    sys.upload(&image, Some(1_761_500_000)).await.expect("signed upload");
    sys.call(keys::START, &[]).await.expect("start");

    // The payload's tick publications reach the cluster.
    let first = u32::from_le_bytes(next(&mut count).await.try_into().expect("u32"));
    let second = u32::from_le_bytes(next(&mut count).await.try_into().expect("u32"));
    assert!(second > first, "{first} then {second}");

    // A cluster sample on the downlink filter reaches on_message, and its
    // republication comes back.
    let echoed = tokio::time::timeout(TIMEOUT, async {
        let mut buf = vec![0u8; 256];
        loop {
            cluster.publish(Key::from_static("app/pi/in"), b"from the cluster").await.expect("publish");
            if let Ok(Ok(s)) = tokio::time::timeout(Duration::from_millis(200), out.recv(&mut buf)).await {
                return s.payload.to_vec();
            }
        }
    })
    .await
    .expect("downlink round trip");
    assert_eq!(echoed, b"from the cluster");

    // The ABI 5 served key answers over the local link with the node id.
    let reply = local_request(&mut local, "app/pi/echo", b"hello").await.expect("echo");
    assert_eq!(&reply[..8], &NODE.0);
    assert_eq!(&reply[8..], b"hello");
    assert!(matches!(
        local_request(&mut local, "app/pi/none", b"").await,
        Err(LinkError::Handler(HandlerError::NotFound))
    ));
    // gw/sys still refuses what is not allowlisted.
    assert_eq!(sys.call(keys::STOP, &[]).await, Err(HandlerError::Denied));
    // The log tail holds the payload's lines.
    let log = sys.call(keys::LOG, &[]).await.expect("log");
    assert!(String::from_utf8_lossy(&log).contains("pi: started"));

    let report = pi.stop().await;
    assert_eq!(report.device.ended, Some(Ended::Stopped));
    assert!(report.device.synced);
    assert_eq!(report.gateway.device, Some(NODE));

    // Restart on the same state directory: the payload starts by itself,
    // the pinned key and min_version hold.
    let (mut cluster, session) = zenoh_pair().await;
    let mut count = cluster.subscribe(KeyFilter::Exact(Key::from_static("app/pi/count"))).await.expect("subscribe");
    let (pi, mut sys, _local) = node(&state, session).await;
    let raw = sys.info().await;
    assert_eq!(DeviceInfo::decode(&raw).expect("decode").status, PayloadStatus::Running);
    next(&mut count).await;
    assert_eq!(sys.upload(&image, Some(1_761_500_000)).await, Err(HandlerError::Denied));
    assert_eq!(
        sys.call(keys::TRUST_CLAIM, &Seed::new([9; 32]).public_key().0).await,
        Err(HandlerError::Denied)
    );
    let report = pi.stop().await;
    assert!(report.device.synced);
    let _ = std::fs::remove_dir_all(&state);
}

#[tokio::test(flavor = "multi_thread")]
async fn pn4_the_state_directory_is_held_by_one_node() {
    let state = state_dir("lock");
    let held = open_state_flash(&state).expect("first");
    let e = open_state_flash(&state).expect_err("second");
    assert_eq!(e.kind(), std::io::ErrorKind::WouldBlock);
    drop(held);
    let _ = std::fs::remove_dir_all(&state);
}
