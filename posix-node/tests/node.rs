//! The node end to end, in one process: the device thread on a state
//! directory under the build's temporary directory, the in-process gateway
//! on the other end of its `ChannelIo` pair, and a "cluster" Zenoh session
//! listening on one explicit 127.0.0.1 port (reserved by binding port 0,
//! never probed) that the node's session connects to. A local link (the
//! second `ChannelIo` pair, what `MW_POSIX_NODE_LOCAL_SERIAL` opens on a
//! host) carries requests on the payload's served keys, which `gw/sys`
//! does not pass. No serial port is opened.
//!
//! The node is claimed over the local link (physical access, as over a
//! USB cable: `sys/trust/claim` never passes `gw/sys`). Then a signed
//! payload (the WAT fixture, signed with the golden vector's test key) is
//! uploaded and started with a signed control request through `gw/sys`,
//! as the service runner calls it, by an updater principal
//! (`MW_GATEWAY_UPDATERS`) with request ids, through the gateway's update
//! gate (RG20–RG27) and its persisted control counter floor. Unsigned
//! commits and starts, a replayed start and a caller that is not an
//! updater are refused. The payload's tick publications arrive on the
//! cluster session, a cluster sample on a downlink filter reaches it and
//! comes back, and its ABI 5 served key answers with the node id. After a
//! restart on the same state directory it starts by itself and a replayed
//! commit is denied.

use std::path::{Path, PathBuf};
use std::time::Duration;

use middleware::loader::trust::{ControlCounter, ControlOp, TARGET_LEN, Version};
use middleware::loader::{Caps, DeviceInfo, PayloadStatus, keys};
use middleware::serial::link::{INBOX_BYTES, LinkError, SerialLink};
use middleware::transport::{
    HandlerError, Publisher, Request, RequestMeta, RequestOptions, Requester, Subscribe, Subscriber,
};
use middleware::zenoh::{SessionConfig, ZenohSubscriber, ZenohTransport};
use middleware::{Key, KeyFilter, SenderId};
use mw_gateway::bridge::encode_sys_request;
use mw_gateway::io::{ChannelIo, pair};
use mw_gateway::state::{ControlStore, Persisted};
use mw_gateway::{GatewayConfig, SysForward, service};
use mw_posix_node::config::gateway_state_dir;
use mw_posix_node::service::CHIP;
use mw_posix_node::shell::Ended;
use mw_posix_node::{
    DeviceParts, GatewayParts, PosixNode, control_floor, facts, limits, open_state_flash, start,
    wall_clock_ms,
};
use mw_service::{Ctx, Links, Served};
use mwctl::plan::upload_plan;
use mwctl::sign::{Seed, sign_commit, sign_control};

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
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("posix-node-{name}-{}", std::process::id()));
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
    let cluster = SessionConfig::new()
        .listen(endpoint.clone())
        .build()
        .expect("config");
    let node = SessionConfig::new()
        .connect(endpoint)
        .build()
        .expect("config");
    let cluster = ZenohTransport::open(cluster).await.expect("open cluster");
    let node = ZenohTransport::open(node).await.expect("open node");
    (cluster, node)
}

const UPDATER: &str = "spiffe://infra.local/ns/apps/sa/updater";
const STRANGER: &str = "spiffe://infra.local/ns/apps/sa/someone-else";

/// The gateway's settings for the test: one updater, `app/pi/in` relayed
/// down to the node, the gateway's state under the node's.
fn gateway_config(state: &Path) -> GatewayConfig {
    let mut c = GatewayConfig::from_env(&|k| match k {
        "MW_GATEWAY_DOWNLINK" => Some("app/pi/in".into()),
        "MW_GATEWAY_UPDATERS" => Some(UPDATER.into()),
        _ => None,
    })
    .expect("config");
    c.state_dir = gateway_state_dir(None, state);
    c
}

/// `gw/sys` as the service runner calls it, for a caller.
struct Sys {
    served: Vec<Served>,
    ids: u32,
}

impl Sys {
    fn new(sys: SysForward, state: &Path) -> Self {
        let (spec, served) = service(&gateway_config(state), sys)
            .expect("service")
            .into_parts();
        assert_eq!(spec.serves, ["gw/sys"]);
        Sys { served, ids: 0 }
    }

    /// One call as `principal` (anonymous for `None`) with a fresh request id.
    async fn call_as(
        &mut self,
        principal: Option<&str>,
        key: Key<'_>,
        body: &[u8],
    ) -> Result<Vec<u8>, HandlerError> {
        self.ids += 1;
        let id = format!("pn8-{}", self.ids);
        let meta = RequestMeta::NONE.with_request_id_str(&id);
        let meta = match principal {
            Some(p) => meta.with_principal(p),
            None => meta,
        };
        let handler = &mut self.served[0].handler;
        let mut req = vec![0u8; 1 + key.as_str().len() + body.len()];
        let n = encode_sys_request(key, body, &mut req).expect("body");
        let request = Request::new(Key::from_static("gw/sys"), &req[..n]);
        let mut links = Links::new();
        let mut ctx = Ctx::new(meta, None, &mut links);
        let mut out = vec![0u8; 2048];
        let len = handler.call(&mut ctx, &request, &mut out).await?;
        out.truncate(len);
        Ok(out)
    }

    /// One call as the updater.
    async fn call(&mut self, key: Key<'_>, body: &[u8]) -> Result<Vec<u8>, HandlerError> {
        self.call_as(Some(UPDATER), key, body).await
    }

    /// Uploads `image` as the updater (signed as `version`, when given);
    /// the commit's reply.
    async fn upload(
        &mut self,
        image: &[u8],
        version: Option<u32>,
    ) -> Result<Vec<u8>, HandlerError> {
        let auth = version.and_then(|v| sign_commit(image, &test_seed(), Version(v)).ok());
        let steps = upload_plan(image, None, auth).expect("plan");
        let (commit, transfer) = steps.split_last().expect("steps");
        for step in transfer {
            self.call(step.key, &step.body).await?;
        }
        self.call(commit.key, &commit.body).await
    }

    /// A signed `sys/payload/start` as the updater, with `counter`.
    async fn signed_start(&mut self, counter: u64) -> Result<Vec<u8>, HandlerError> {
        let mut target = [0u8; TARGET_LEN];
        target.copy_from_slice(&NODE.0[..TARGET_LEN]);
        let body = sign_control(
            &test_seed(),
            ControlOp::Start,
            &target,
            ControlCounter(counter),
        )
        .unwrap()
        .encode();
        self.call(keys::START, &body).await
    }

    /// Asks `sys/info` until the gateway's link to the device answers;
    /// the raw reply (decode with `DeviceInfo::decode`).
    async fn info(&mut self) -> Vec<u8> {
        tokio::time::timeout(TIMEOUT, async {
            loop {
                if let Ok(b) = self.call_as(None, keys::INFO, &[]).await {
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
    let s = tokio::time::timeout(TIMEOUT, sub.recv(&mut buf))
        .await
        .expect("in time")
        .expect("sample");
    s.payload.to_vec()
}

type Local = SerialLink<ChannelIo, INBOX_BYTES>;

async fn local_request(
    link: &mut Local,
    key: &'static str,
    body: &[u8],
) -> Result<Vec<u8>, LinkError<mw_gateway::io::Closed>> {
    let mut out = vec![0u8; 1024];
    let r = tokio::time::timeout(
        TIMEOUT,
        link.request_with(
            Key::from_static(key),
            body,
            &mut out,
            &RequestOptions::default(),
        ),
    )
    .await
    .expect("in time")?;
    Ok(r.to_vec())
}

async fn node(state: &Path, session: ZenohTransport) -> (PosixNode, Sys, Local) {
    let (local_test, local_node) = pair();
    let config = gateway_config(state);
    let store = ControlStore::in_dir(&config.state_dir);
    let floor = control_floor(&store, wall_clock_ms());
    let device = DeviceParts {
        flash: open_state_flash(state).expect("state"),
        facts: facts(NODE),
        limits: limits(1024 * 1024),
        local: Some(local_node),
    };
    let gateway = GatewayParts {
        bridge: config.bridge_with_floor(floor),
        options: config.shell_options(),
        store,
    };
    let (pi, sys) = start(device, gateway, session).await.expect("start");
    (pi, Sys::new(sys, state), SerialLink::with_inbox(local_test))
}

/// The wall clock in milliseconds: a control counter above the floor.
fn now_ms() -> u64 {
    wall_clock_ms().expect("clock")
}

#[tokio::test(flavor = "multi_thread")]
async fn pn8_a_signed_payload_through_gw_sys_publishes_to_zenoh_and_serves() {
    let state = state_dir("e2e");
    let (mut cluster, session) = zenoh_pair().await;
    let mut count = cluster
        .subscribe(KeyFilter::Exact(Key::from_static("app/pi/count")))
        .await
        .expect("subscribe");
    let mut out = cluster
        .subscribe(KeyFilter::Exact(Key::from_static("app/pi/out")))
        .await
        .expect("subscribe");
    let (pi, mut sys, mut local) = node(&state, session).await;

    // sys/info through gw/sys: the node is a board named after its platform.
    let raw = sys.info().await;
    let info = DeviceInfo::decode(&raw).expect("decode");
    assert_eq!(info.chip, CHIP);
    assert_eq!(info.node, Some(NODE));
    assert_eq!(info.caps, Caps::SERVE);
    assert_eq!(info.status, PayloadStatus::Empty);
    assert_eq!(info.capacity, 252 * 1024);

    // Claim over the local link: physical access. Through gw/sys a claim
    // is always refused, even for an updater.
    assert_eq!(
        sys.call(keys::TRUST_CLAIM, test_seed().public_key().as_bytes())
            .await,
        Err(HandlerError::Denied)
    );
    local_request(
        &mut local,
        "sys/trust/claim",
        test_seed().public_key().as_bytes(),
    )
    .await
    .expect("claim");
    // The gateway learns the claim from a forwarded sys/trust/info.
    sys.call(keys::TRUST_INFO, &[]).await.expect("trust info");

    let image = demo();
    // Not an updater: refused before the device.
    assert_eq!(
        sys.call_as(Some(STRANGER), keys::BEGIN, &[0; 8]).await,
        Err(HandlerError::Denied)
    );
    assert_eq!(
        sys.call_as(None, keys::BEGIN, &[0; 8]).await,
        Err(HandlerError::Denied)
    );
    // Unsigned commit: refused; signed: installs.
    assert_eq!(sys.upload(&image, None).await, Err(HandlerError::Denied));
    sys.upload(&image, Some(1_761_500_000))
        .await
        .expect("signed upload");
    // Unsigned start refused; a signed start runs it; its replay is refused.
    assert_eq!(sys.call(keys::START, &[]).await, Err(HandlerError::Denied));
    let counter = now_ms() + 1_000;
    sys.signed_start(counter).await.expect("signed start");
    assert_eq!(sys.signed_start(counter).await, Err(HandlerError::Denied));

    // The payload's tick publications reach the cluster.
    let first = u32::from_le_bytes(next(&mut count).await.try_into().expect("u32"));
    let second = u32::from_le_bytes(next(&mut count).await.try_into().expect("u32"));
    assert!(second > first, "{first} then {second}");

    // A cluster sample on the downlink filter reaches on_message, and its
    // republication comes back.
    let echoed = tokio::time::timeout(TIMEOUT, async {
        let mut buf = vec![0u8; 256];
        loop {
            cluster
                .publish(Key::from_static("app/pi/in"), b"from the cluster")
                .await
                .expect("publish");
            if let Ok(Ok(s)) =
                tokio::time::timeout(Duration::from_millis(200), out.recv(&mut buf)).await
            {
                return s.payload.to_vec();
            }
        }
    })
    .await
    .expect("downlink round trip");
    assert_eq!(echoed, b"from the cluster");

    // The ABI 5 served key answers over the local link with the node id.
    let reply = local_request(&mut local, "app/pi/echo", b"hello")
        .await
        .expect("echo");
    assert_eq!(&reply[..8], &NODE.0);
    assert_eq!(&reply[8..], b"hello");
    assert!(matches!(
        local_request(&mut local, "app/pi/none", b"").await,
        Err(LinkError::Handler(HandlerError::NotFound))
    ));
    // An unsigned stop is refused, even for the updater.
    assert_eq!(sys.call(keys::STOP, &[]).await, Err(HandlerError::Denied));
    // The log tail holds the payload's lines.
    let log = sys.call_as(None, keys::LOG, &[]).await.expect("log");
    assert!(String::from_utf8_lossy(&log).contains("pi: started"));

    let report = pi.stop().await;
    assert_eq!(report.device.ended, Some(Ended::Stopped));
    assert!(report.device.synced);
    assert_eq!(report.gateway.device, Some(NODE));
    // The accepted start's counter is persisted in the gateway's state,
    // under the node's state directory.
    assert_eq!(
        ControlStore::in_dir(gateway_state_dir(None, &state)).load(),
        Persisted::At(ControlCounter(counter))
    );

    // Restart on the same state directory: the payload starts by itself,
    // the pinned key and min_version hold.
    let (mut cluster, session) = zenoh_pair().await;
    let mut count = cluster
        .subscribe(KeyFilter::Exact(Key::from_static("app/pi/count")))
        .await
        .expect("subscribe");
    let (pi, mut sys, mut local) = node(&state, session).await;
    let raw = sys.info().await;
    assert_eq!(
        DeviceInfo::decode(&raw).expect("decode").status,
        PayloadStatus::Running
    );
    next(&mut count).await;
    sys.call(keys::TRUST_INFO, &[]).await.expect("trust info");
    assert_eq!(
        sys.upload(&image, Some(1_761_500_000)).await,
        Err(HandlerError::Denied)
    );
    assert!(matches!(
        local_request(
            &mut local,
            "sys/trust/claim",
            Seed::new([9; 32]).public_key().as_bytes()
        )
        .await,
        Err(LinkError::Handler(HandlerError::Denied))
    ));
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
