//! The node's own `sys/...` keys over Zenoh (PN21–PN25), end to end in
//! one process: the device thread on a state directory, its in-process
//! gateway, and the third link, the mesh link, whose far end the Zenoh
//! task (`zenoh_sys::run`) or the test drives. The node is claimed over the
//! local link (physical access); `sys/trust/claim` never passes Zenoh.

mod common;

use std::path::Path;
use std::time::Duration;

use common::{NODE, demo, node_session, peer, router, state_dir, test_seed, zenoh_pair};
use middleware::loader::manifest::Manifest;
use middleware::loader::trust::{ControlCounter, ControlOp, TARGET_LEN, TrustInfo, TrustState, Version};
use middleware::loader::{DeviceInfo, PayloadStatus, keys};
use middleware::serial::link::{INBOX_BYTES, LinkError, RECV_BUF_LEN, SerialLink};
use middleware::transport::{HandlerError, RequestOptions, Requester, Target};
use middleware::zenoh::{ZenohError, ZenohTransport, wire};
use middleware::{Key, SenderId};
use mw_gateway::bridge::encode_sys_request;
use mw_gateway::io::{ChannelIo, Closed, pair};
use mw_gateway::state::ControlStore;
use mw_gateway::{GatewayConfig, service};
use mw_posix_node::config::gateway_state_dir;
use mw_posix_node::driver::Driver;
use mw_posix_node::driver::sim::{SimConfig, SimDriver};
use mw_posix_node::mesh_gate::wrap;
use mw_posix_node::service::CHIP;
use mw_posix_node::zenoh_sys::{self, Options, Report};
use mw_posix_node::{
    DeviceParts, GatewayParts, PosixNode, control_floor, facts_with_driver, limits, open_state_flash, start,
    wall_clock_ms,
};
use mw_service::{Ctx, Links};
use mwctl::plan::upload_plan;
use mwctl::sign::{sign_commit, sign_control};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

const TIMEOUT: Duration = Duration::from_secs(15);
const A: SenderId = SenderId([0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8]);

type Link = SerialLink<ChannelIo, INBOX_BYTES>;

const UPDATER: &str = "spiffe://infra.local/ns/apps/sa/updater";

/// `gw/sys` as the service runner calls it, as the updater.
struct Sys {
    served: Vec<mw_service::Served>,
    ids: u32,
}

impl Sys {
    async fn call(&mut self, key: Key<'_>, body: &[u8]) -> Result<Vec<u8>, HandlerError> {
        self.ids += 1;
        let id = format!("zsys-{}", self.ids);
        let meta = middleware::transport::RequestMeta::NONE.with_request_id_str(&id).with_principal(UPDATER);
        let mut req = vec![0u8; 1 + key.as_str().len() + body.len()];
        let n = encode_sys_request(key, body, &mut req).expect("body");
        let request = middleware::transport::Request::new(Key::from_static("gw/sys"), &req[..n]);
        let mut links = Links::new();
        let mut ctx = Ctx::new(meta, None, &mut links);
        let mut out = vec![0u8; 2048];
        let len = self.served[0].handler.call(&mut ctx, &request, &mut out).await?;
        out.truncate(len);
        Ok(out)
    }
}

fn gateway_config(state: &Path) -> GatewayConfig {
    let mut config =
        GatewayConfig::from_env(&|k| (k == "MW_GATEWAY_UPDATERS").then(|| UPDATER.to_owned())).expect("config");
    config.state_dir = gateway_state_dir(None, state);
    config
}

/// The node on `state` with a local link and a mesh link; the test's ends
/// of both, and `gw/sys`.
async fn node_with_sys(state: &Path, session: ZenohTransport) -> (PosixNode, Link, ChannelIo, Sys) {
    let (local_test, local_node) = pair();
    let (mesh_test, mesh_node) = pair();
    let config = gateway_config(state);
    let store = ControlStore::in_dir(&config.state_dir);
    let floor = control_floor(&store, wall_clock_ms());
    let driver = SimDriver::new(SimConfig::pc_turn_50());
    let device = DeviceParts {
        flash: open_state_flash(state).expect("state"),
        facts: facts_with_driver(NODE, &driver as &dyn Driver),
        limits: limits(1024 * 1024),
        local: Some(local_node),
        mesh: Some(mesh_node),
        shared_control: Some(store.clone()),
        driver: Box::new(driver),
    };
    let gateway = GatewayParts { bridge: config.bridge_with_floor(floor), options: config.shell_options(), store };
    let (pi, sys) = start(device, gateway, session).await.expect("start");
    let (_, served) = service(&gateway_config(state), sys).expect("service").into_parts();
    (pi, SerialLink::with_inbox(local_test), mesh_test, Sys { served, ids: 0 })
}

/// [`node_with_sys`] without `gw/sys`.
async fn node(state: &Path, session: ZenohTransport) -> (PosixNode, Link, ChannelIo) {
    let (pi, local, mesh, _) = node_with_sys(state, session).await;
    (pi, local, mesh)
}

async fn request(link: &mut Link, key: Key<'_>, body: &[u8]) -> Result<Vec<u8>, LinkError<Closed>> {
    let mut out = vec![0u8; 2048];
    let r = tokio::time::timeout(TIMEOUT, link.request_with(key, body, &mut out, &RequestOptions::default()))
        .await
        .expect("in time")?;
    Ok(r.to_vec())
}

fn trust(raw: &[u8]) -> TrustState {
    TrustInfo::decode(raw).expect("trust info").state
}

#[tokio::test(flavor = "multi_thread")]
async fn pn25_the_mesh_link_carries_requests_with_their_requester_and_never_publications() {
    let state = state_dir("mesh-link");
    let (_cluster, session, _) = zenoh_pair().await;
    let (pi, mut local, mesh) = node(&state, session).await;
    let mut mesh: Link = SerialLink::with_inbox(mesh);

    let info = request(&mut mesh, keys::INFO, &wrap(Some(A), &[])).await.expect("info over the mesh link");
    assert_eq!(DeviceInfo::decode(&info).expect("decode").node, Some(NODE));
    let info = request(&mut mesh, keys::INFO, &wrap(None, &[])).await.expect("anonymous info");
    assert_eq!(DeviceInfo::decode(&info).expect("decode").node, Some(NODE));
    // A body without the envelope is malformed.
    assert!(matches!(request(&mut mesh, keys::INFO, &[]).await, Err(LinkError::Handler(HandlerError::InvalidRequest))));
    assert!(matches!(
        request(&mut mesh, keys::INFO, &[7, 1, 2]).await,
        Err(LinkError::Handler(HandlerError::InvalidRequest))
    ));
    // The mesh link has no physical-access authority: no claim.
    let key = test_seed().public_key();
    assert!(matches!(
        request(&mut mesh, keys::TRUST_CLAIM, &wrap(Some(A), key.as_bytes())).await,
        Err(LinkError::Handler(HandlerError::Denied))
    ));
    assert_eq!(trust(&request(&mut local, keys::TRUST_INFO, &[]).await.expect("trust")), TrustState::Unclaimed);
    // The local link claims; its sys/log line goes to the gateway and the
    // local link, never to the mesh link.
    request(&mut local, keys::TRUST_CLAIM, key.as_bytes()).await.expect("claim");
    let mut buf = vec![0u8; RECV_BUF_LEN];
    let heard = tokio::time::timeout(Duration::from_millis(500), mesh.next_frame(&mut buf)).await;
    assert!(heard.is_err(), "the mesh link was sent a frame: {:?}", heard.map(|f| f.map(|_| ())));
    assert!(matches!(
        trust(&request(&mut mesh, keys::TRUST_INFO, &wrap(Some(A), &[])).await.expect("trust")),
        TrustState::Claimed { .. }
    ));

    let report = pi.stop().await;
    assert!(report.device.synced);
    let _ = std::fs::remove_dir_all(&state);
}

/// A node serving its sys keys over Zenoh (what the binary runs).
struct Served {
    pi: PosixNode,
    local: Link,
    sys: Sys,
    /// The node session's sender id: the id in its keys.
    id: SenderId,
    stop: oneshot::Sender<()>,
    task: JoinHandle<Result<Report, zenoh_sys::SysError>>,
}

impl Served {
    async fn start(state: &Path, session: ZenohTransport) -> Served {
        let id = session.sender_id();
        let (pi, local, mesh, sys) = node_with_sys(state, session.clone()).await;
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(zenoh_sys::run(mesh, session, Options::default(), async {
            let _ = stopped.await;
        }));
        Served { pi, local, sys, id, stop, task }
    }

    async fn stop(self) -> Report {
        let _ = self.stop.send(());
        let report = self.task.await.expect("task").expect("zenoh_sys");
        assert!(self.pi.stop().await.device.synced);
        report
    }
}

/// What a query got: the body, an error reply, or no reply (`None`).
async fn ask(z: &ZenohTransport, node: SenderId, key: Key<'_>, body: &[u8]) -> Result<Vec<u8>, Option<HandlerError>> {
    let mut z = z.clone();
    let opts = RequestOptions::DEFAULT.with_target(Target::Node(node)).with_timeout(Duration::from_secs(8));
    let mut out = vec![0u8; 4096];
    match z.request_with(key, body, &mut out, &opts).await {
        Ok(r) => Ok(r.to_vec()),
        Err(ZenohError::Handler(e)) => Err(Some(e)),
        Err(ZenohError::NoReply) => Err(None),
        Err(e) => panic!("{}: {e:?}", key.as_str()),
    }
}

/// `sys/info` over Zenoh, retried until the queryable has propagated.
async fn info(z: &ZenohTransport, node: SenderId) -> DeviceInfo<'static> {
    let raw = tokio::time::timeout(TIMEOUT, async {
        loop {
            if let Ok(b) = ask(z, node, keys::INFO, &[]).await {
                return b;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("sys/info over zenoh");
    DeviceInfo::decode(Box::leak(raw.into_boxed_slice())).expect("decode")
}

fn steps(version: Option<u32>) -> Vec<mwctl::plan::Step> {
    let image = demo();
    let auth = version.map(|v| sign_commit(&image, &test_seed(), Version(v)).expect("sign"));
    upload_plan(&image, None, auth).expect("plan")
}

async fn upload(z: &ZenohTransport, node: SenderId, version: Option<u32>) -> Result<Vec<u8>, Option<HandlerError>> {
    let mut last = Vec::new();
    for s in steps(version) {
        last = ask(z, node, s.key, &s.body).await?;
    }
    Ok(last)
}

fn own_target() -> [u8; TARGET_LEN] {
    let mut t = [0u8; TARGET_LEN];
    t.copy_from_slice(&NODE.0[..TARGET_LEN]);
    t
}

fn control(op: ControlOp, target: [u8; TARGET_LEN], counter: u64) -> Vec<u8> {
    sign_control(&test_seed(), op, &target, ControlCounter(counter)).expect("sign").encode().to_vec()
}

const DENIED: Result<Vec<u8>, Option<HandlerError>> = Err(Some(HandlerError::Denied));
const NOT_OWNER: Result<Vec<u8>, Option<HandlerError>> = Err(Some(HandlerError::InvalidRequest));

#[tokio::test(flavor = "multi_thread")]
async fn pn21_info_manifest_and_cm_over_zenoh() {
    let state = state_dir("zsys-info");
    let (cluster, endpoint) = router().await;
    let mut node = Served::start(&state, node_session(&endpoint, NODE).await).await;
    let id = node.id;
    assert_ne!(id, NODE, "the key carries the session id, sys/info the node id");
    let zid: zenoh::session::ZenohId = mw_posix_node::node_id::zenoh_id(NODE).parse().expect("zid");
    assert_eq!(id, middleware::zenoh::sender_id_of(zid), "derived from the node id");

    // mwctl's own Zenoh link (`mwctl --zenoh <endpoint> --node <id> info`).
    let ep = endpoint.clone();
    let (dev_info, dev_manifest) = tokio::task::spawn_blocking(move || {
        use mwctl::zlink::{Route, ZenohLink};
        let mut link = ZenohLink::open(&Route::Connect(ep), id, None).expect("open");
        link.wait_for_board(TIMEOUT).expect("board");
        let mut dev = mwctl::device::Device::new(link);
        (dev.info().expect("info"), dev.manifest().expect("manifest"))
    })
    .await
    .expect("mwctl");
    assert_eq!(dev_info.chip, CHIP);
    assert_eq!(dev_info.node, Some(NODE));
    assert_eq!(dev_info.status, PayloadStatus::Empty);
    // The manifest over Zenoh is the manifest over the local link, byte
    // for byte (the browser's fixtures come from either).
    let over_zenoh = ask(&cluster, id, keys::MANIFEST, &[]).await.expect("manifest");
    let over_local = request(&mut node.local, keys::MANIFEST, &[]).await.expect("manifest");
    assert_eq!(over_zenoh, over_local);
    assert_eq!(Manifest::decode(&over_zenoh).expect("decode"), dev_manifest);
    let cm = ask(&cluster, id, keys::CONTROL_MODULES, &[]).await.expect("cm");
    assert!(cm.windows(5).any(|w| w == b"x_pos"), "{cm:?}");
    assert_eq!(cm, request(&mut node.local, keys::CONTROL_MODULES, &[]).await.expect("cm"));
    let trust = ask(&cluster, id, keys::TRUST_INFO, &[]).await.expect("trust info");
    assert_eq!(TrustInfo::decode(&trust).expect("decode").state, TrustState::Unclaimed);

    // A wildcard get finds it under its session id, and the reply says so.
    let replies = cluster.session().get("@mw/node/*/sys/info").await.expect("get");
    let mut found = Vec::new();
    while let Ok(r) = replies.recv_async().await {
        if let Ok(sample) = r.result() {
            let att = sample.attachment().map(|a| a.to_bytes().into_owned());
            found.push((sample.key_expr().as_str().to_owned(), att.as_deref().and_then(wire::decode_reply)));
        }
    }
    assert_eq!(found, [(wire::node_key(id, keys::INFO), Some(id))]);
    // No bare key: a query for any node's sys/info finds nobody.
    let mut any = cluster.clone();
    let mut out = vec![0u8; 512];
    let opts = RequestOptions::DEFAULT.with_target(Target::Any).with_timeout(Duration::from_millis(500));
    assert!(matches!(any.request_with(keys::INFO, &[], &mut out, &opts).await, Err(ZenohError::NoReply)));

    // Twenty at once all answer, one at a time.
    let all = futures_join(20, |_| {
        let c = cluster.clone();
        async move { ask(&c, id, keys::INFO, &[]).await.is_ok() }
    })
    .await;
    assert!(all.iter().all(|ok| *ok), "{all:?}");

    let report = node.stop().await;
    assert!(report.served >= 25, "{report:?}");
    assert_eq!(report.timeouts, 0);
    let _ = std::fs::remove_dir_all(&state);
}

/// Runs `n` futures concurrently; their outputs in order.
async fn futures_join<F, T>(n: usize, f: impl Fn(usize) -> F) -> Vec<T>
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let handles: Vec<_> = (0..n).map(|i| tokio::spawn(f(i))).collect();
    let mut out = Vec::new();
    for h in handles {
        out.push(h.await.expect("task"));
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn pn22_pn23_an_unclaimed_node_answers_its_description_and_nothing_else() {
    let state = state_dir("zsys-unclaimed");
    let (cluster, endpoint) = router().await;
    let mut node = Served::start(&state, node_session(&endpoint, NODE).await).await;
    let id = node.id;
    info(&cluster, id).await;
    for key in [
        "sys/trust/claim",
        "sys/board",
        "sys/peers",
        "sys/fw/begin",
        "sys/fw/info",
        "sys/log",
        "sys/zenoh/endpoint",
        "sys/payload/status",
        "sys/link/sub",
    ] {
        let body = test_seed().public_key();
        assert_eq!(ask(&cluster, id, Key::new(key).unwrap(), body.as_bytes()).await, DENIED, "{key}");
    }
    // Unclaimed: the gate refuses what installs, runs or binds.
    assert_eq!(upload(&cluster, id, None).await, DENIED);
    assert_eq!(upload(&cluster, id, Some(1)).await, DENIED);
    assert_eq!(ask(&cluster, id, keys::START, &control(ControlOp::Start, own_target(), 5)).await, DENIED);
    assert_eq!(ask(&cluster, id, keys::STOP, &[]).await, DENIED);
    assert_eq!(ask(&cluster, id, keys::BINDING, &[]).await, DENIED);
    assert_eq!(ask(&cluster, id, keys::BIND, &[1, 2, 3]).await, DENIED);
    // Nothing changed.
    let trust = request(&mut node.local, keys::TRUST_INFO, &[]).await.expect("trust");
    assert_eq!(TrustInfo::decode(&trust).expect("decode").state, TrustState::Unclaimed);
    assert_eq!(info(&cluster, id).await.status, PayloadStatus::Empty);
    let report = node.stop().await;
    assert!(report.refused >= 9, "{report:?}");
    let _ = std::fs::remove_dir_all(&state);
}

#[tokio::test(flavor = "multi_thread")]
async fn pn23_pn24_pn25_signed_installs_starts_and_owned_transfers_over_zenoh() {
    let state = state_dir("zsys-signed");
    let (a, endpoint) = router().await;
    let b = peer(&endpoint).await;
    let mut node = Served::start(&state, node_session(&endpoint, NODE).await).await;
    let id = node.id;
    info(&a, id).await;
    request(&mut node.local, keys::TRUST_CLAIM, test_seed().public_key().as_bytes()).await.expect("claim");

    // Unsigned: refused; signed: installed; the same version again: refused.
    assert_eq!(upload(&a, id, None).await, DENIED);
    upload(&a, id, Some(1)).await.expect("signed upload");
    assert_eq!(upload(&a, id, Some(1)).await, DENIED);
    // Start: unsigned, or signed for the RT1050's address, refused.
    assert_eq!(ask(&a, id, keys::START, &[]).await, DENIED);
    let rt = [0x02, NODE.0[3], NODE.0[4], NODE.0[5], NODE.0[6], NODE.0[7]];
    assert_eq!(ask(&a, id, keys::START, &control(ControlOp::Start, rt, 100)).await, DENIED);
    let start = control(ControlOp::Start, own_target(), 100);
    ask(&a, id, keys::START, &start).await.expect("signed start");
    assert_eq!(info(&a, id).await.status, PayloadStatus::Running);
    assert_eq!(ask(&b, id, keys::START, &start).await, DENIED, "a replay");

    // A second upload by A, staged beside the running payload. B can
    // neither continue nor end it; nor can A's own unsigned commit.
    let s = steps(Some(2));
    let (commit, transfer) = s.split_last().expect("steps");
    for step in transfer {
        ask(&a, id, step.key, &step.body).await.expect("staged transfer");
        assert_eq!(info(&a, id).await.status, PayloadStatus::Running);
        assert_eq!(ask(&b, id, transfer[1].key, &transfer[1].body).await, NOT_OWNER);
    }
    assert_eq!(ask(&b, id, commit.key, &commit.body).await, NOT_OWNER);
    assert_eq!(ask(&b, id, keys::COMMIT, &[]).await, NOT_OWNER);
    assert_eq!(ask(&a, id, keys::COMMIT, &[]).await, DENIED);
    assert_eq!(info(&a, id).await.status, PayloadStatus::Running);
    ask(&a, id, commit.key, &commit.body).await.expect("A's signed commit");

    // Binding: readable once claimed (none stored yet); unsigned refused.
    assert_eq!(ask(&a, id, keys::BINDING, &[]).await, Err(Some(HandlerError::NotFound)));
    let record = mwctl::bindspec::record(mwctl::sign::image_sha256(&demo()), 3, &[]).expect("record");
    assert_eq!(ask(&a, id, keys::BIND, &record).await, DENIED);

    // A serial begin takes a Zenoh transfer over.
    let s = steps(Some(3));
    ask(&a, id, s[0].key, &s[0].body).await.expect("zenoh begin");
    request(&mut node.local, s[0].key, &s[0].body).await.expect("serial begin");
    assert_eq!(ask(&a, id, s[1].key, &s[1].body).await, NOT_OWNER);

    // Restart on the same state directory: the counter holds, and the
    // node answers under the same id.
    let report = node.stop().await;
    assert_eq!(report.timeouts, 0);
    let node = Served::start(&state, node_session(&endpoint, NODE).await).await;
    assert_eq!(node.id, id, "the sender id survives a restart");
    info(&a, id).await;
    assert_eq!(ask(&a, id, keys::START, &start).await, DENIED, "a replay after a restart");
    ask(&a, id, keys::START, &control(ControlOp::Start, own_target(), 101)).await.expect("a fresh counter");
    node.stop().await;
    let _ = std::fs::remove_dir_all(&state);
}

/// The wall clock in ms: a control counter above the gateway's floor.
fn now_ms() -> u64 {
    wall_clock_ms().expect("clock")
}

#[tokio::test(flavor = "multi_thread")]
async fn pn24_gw_sys_and_zenoh_share_one_control_counter() {
    let state = state_dir("zsys-shared");
    let (a, endpoint) = router().await;
    let mut node = Served::start(&state, node_session(&endpoint, NODE).await).await;
    let id = node.id;
    info(&a, id).await;
    request(&mut node.local, keys::TRUST_CLAIM, test_seed().public_key().as_bytes()).await.expect("claim");
    node.sys.call(keys::TRUST_INFO, &[]).await.expect("the gateway learns the claim");
    upload(&a, id, Some(1)).await.expect("signed upload");
    let c = now_ms() + 10_000;

    // Accepted on gw/sys, replayed over Zenoh (whose own counter is still
    // 0): refused, the payload stays as it is.
    let gw_start = control(ControlOp::Start, own_target(), c);
    node.sys.call(keys::START, &gw_start).await.expect("gw/sys start");
    assert_eq!(info(&a, id).await.status, PayloadStatus::Running);
    let gw_stop = control(ControlOp::Stop, own_target(), c + 1);
    node.sys.call(keys::STOP, &gw_stop).await.expect("gw/sys stop");
    assert_eq!(info(&a, id).await.status, PayloadStatus::Stopped);
    assert_eq!(ask(&a, id, keys::START, &gw_start).await, DENIED, "a gw/sys start replayed over Zenoh");
    assert_eq!(info(&a, id).await.status, PayloadStatus::Stopped);
    ask(&a, id, keys::START, &control(ControlOp::Start, own_target(), c + 2)).await.expect("zenoh start");
    assert_eq!(ask(&a, id, keys::STOP, &gw_stop).await, DENIED, "a gw/sys stop replayed over Zenoh");
    assert_eq!(info(&a, id).await.status, PayloadStatus::Running);

    // Accepted over Zenoh, replayed on gw/sys: refused.
    let z_stop = control(ControlOp::Stop, own_target(), c + 3);
    let z_start = control(ControlOp::Start, own_target(), c + 4);
    ask(&a, id, keys::STOP, &z_stop).await.expect("zenoh stop");
    ask(&a, id, keys::START, &z_start).await.expect("zenoh start");
    ask(&a, id, keys::STOP, &control(ControlOp::Stop, own_target(), c + 5)).await.expect("zenoh stop");
    assert_eq!(info(&a, id).await.status, PayloadStatus::Stopped);
    assert!(node.sys.call(keys::START, &z_start).await.is_err(), "a Zenoh start replayed on gw/sys");
    assert_eq!(info(&a, id).await.status, PayloadStatus::Stopped);

    // After a restart both paths still refuse both.
    node.stop().await;
    let mut node = Served::start(&state, node_session(&endpoint, NODE).await).await;
    info(&a, id).await;
    node.sys.call(keys::TRUST_INFO, &[]).await.expect("trust info");
    let running = info(&a, id).await.status;
    assert!(node.sys.call(keys::START, &z_start).await.is_err());
    assert!(node.sys.call(keys::STOP, &gw_stop).await.is_err());
    assert_eq!(ask(&a, id, keys::STOP, &gw_stop).await, DENIED);
    assert_eq!(ask(&a, id, keys::START, &z_start).await, DENIED);
    assert_eq!(info(&a, id).await.status, running);
    node.stop().await;
    let _ = std::fs::remove_dir_all(&state);
}

/// The `sys/log` lines the local link receives within `quiet` of silence.
async fn log_lines(local: &mut Link, quiet: Duration) -> Vec<String> {
    use middleware::serial::frame::Frame;
    let mut buf = vec![0u8; RECV_BUF_LEN];
    let mut lines = Vec::new();
    while let Ok(Ok(frame)) = tokio::time::timeout(quiet, local.next_frame(&mut buf)).await {
        if let Frame::Publish { key, payload } = frame
            && key == keys::LOG
        {
            lines.push(String::from_utf8_lossy(payload).into_owned());
        }
    }
    lines
}

#[tokio::test(flavor = "multi_thread")]
async fn pn23_refusals_over_zenoh_never_reach_sys_log() {
    use middleware::loader::binding::SignedBinding;
    use middleware::loader::trust::{Signature, SignedCommit};
    let state = state_dir("zsys-nolog");
    let (a, endpoint) = router().await;
    let mut node = Served::start(&state, node_session(&endpoint, NODE).await).await;
    let id = node.id;
    info(&a, id).await;
    request(&mut node.local, keys::TRUST_CLAIM, test_seed().public_key().as_bytes()).await.expect("claim");
    upload(&a, id, Some(1)).await.expect("signed upload");
    log_lines(&mut node.local, Duration::from_millis(300)).await;

    // A transfer, a commit with a junk signature, a bind with one: each
    // passes the gate (it only sees that they are signed) and is refused
    // by the dispatcher.
    let junk_commit = SignedCommit { version: Version(2), signature: Signature([7; 64]) }.encode().to_vec();
    let mut junk_bind = mwctl::bindspec::record(mwctl::sign::image_sha256(&demo()), 3, &[]).expect("record");
    junk_bind.extend_from_slice(&SignedBinding { version: Version(3), signature: Signature([9; 64]) }.encode());
    for _ in 0..5 {
        let s = steps(None);
        let (_, transfer) = s.split_last().expect("steps");
        for step in transfer {
            ask(&a, id, step.key, &step.body).await.expect("staged");
        }
        assert_eq!(ask(&a, id, keys::COMMIT, &junk_commit).await, DENIED);
        assert_eq!(ask(&a, id, keys::BIND, &junk_bind).await, DENIED);
    }
    let lines = log_lines(&mut node.local, Duration::from_millis(500)).await;
    assert!(lines.is_empty(), "{lines:?}");

    // The same refusal from a serial link is logged (the check sees lines).
    let s = steps(None);
    let (_, transfer) = s.split_last().expect("steps");
    for step in transfer {
        request(&mut node.local, step.key, &step.body).await.expect("serial transfer");
    }
    assert!(request(&mut node.local, keys::COMMIT, &junk_commit).await.is_err());
    let lines = log_lines(&mut node.local, Duration::from_millis(500)).await;
    assert!(lines.iter().any(|l| l.contains("commit denied")), "{lines:?}");
    node.stop().await;
    let _ = std::fs::remove_dir_all(&state);
}
