//! Updates through the gateway, end to end in process: a fake device on
//! an in-memory serial cable (the real `SerialLink` framing, the real
//! loader state machine and trust check, the real `FwService` on the
//! in-memory RP2350 bootrom model), the gateway on the other end, its
//! `gw/sys` served over gRPC on 127.0.0.1 by `serve_sys`, and the
//! `mw-update` client (`remote::execute` over `MeshSysCall`, through a
//! static endpoint) as the caller. Two stand-in sidecars on two loopback
//! ports set `x-forwarded-client-cert` as Envoy would, one for an
//! allowlisted updater and one for another workload. No serial port is
//! opened; ports are reserved by binding port 0, never probed.

use std::convert::Infallible;
use std::net::Ipv4Addr;
use std::task::{Context, Poll};
use std::time::Duration;

use middleware::SenderId;
use middleware::fw_ota::service::{Context as FwContext, FwService};
use middleware::fw_ota::sim::{self, Rp2350Sim};
use middleware::fw_ota::{Slot, StateCode};
use middleware::grpc::{GrpcServer, PrincipalSource, responder};
use middleware::key::Key;
use middleware::loader::trust::{ControlCounter, ControlOp, PublicKey, TrustState, Version};
use middleware::loader::{Caps, Command, DeviceInfo, Effect, Loader, PayloadStatus, keys};
use middleware::serial::frame::Frame;
use middleware::serial::link::{RECV_BUF_LEN, SerialLink};
use middleware::transport::HandlerError;
use middleware::zenoh::{SessionConfig, ZenohTransport};
use mw_gateway::bridge::encode_sys_request;
use mw_gateway::io::{ChannelIo, pair};
use mw_gateway::remote::mesh::MeshSysCall;
use mw_gateway::remote::{CallError, GwEndpoint, Job, RequestIds, SysCall, execute};
use mw_gateway::state::{ControlStore, Persisted};
use mw_gateway::{GatewayConfig, Report, serve_sys, start_with_store};
use mw_service::StaticEndpoints;
use mwctl::device::Device;
use mwctl::fw::load_image;
use mwctl::sign::{P256Key, Seed, Signer};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tonic::body::Body;
use tonic::codegen::{BoxFuture, Service};

const MAC: [u8; 6] = [0x24, 0x6F, 0x28, 0xAA, 0xBB, 0xCC];
const NODE: SenderId = SenderId::from_mac(MAC);
const UPDATER: &str = "spiffe://infra.local/ns/ops/sa/updater";
const STRANGER: &str = "spiffe://infra.local/ns/apps/sa/demo-caller";
const SEED: [u8; 32] = [7; 32];
const V1_0: u32 = 0x0001_0000;
const V1_1: u32 = 0x0001_0001;
const TIMEOUT: Duration = Duration::from_secs(20);

fn seed() -> Seed {
    Seed::from_file_bytes(&SEED).expect("seed")
}

/// The Ed25519 key file's signer, as `mw-update --key` loads it.
fn signer() -> Box<dyn Signer + Send> {
    Box::new(seed())
}

/// A P-256 key, as a token holds one (TEST KEY ONLY: the OpenSSL vector's
/// scalar).
fn p256() -> P256Key {
    P256Key::from_scalar(&[0x20; 32]).expect("scalar")
}

/// What the fake device saw: (key, body length).
type Seen = (String, usize);

/// The device's firmware updater on the bootrom model.
struct Fw {
    sim: Rp2350Sim,
    svc: FwService,
}

impl Fw {
    fn boot(mut sim: Rp2350Sim) -> Fw {
        if sim.booted().is_none() {
            sim.power_on().expect("bootable");
        }
        let svc = FwService::boot(&mut sim).expect("updater boots");
        Fw { sim, svc }
    }
}

/// A claimed Pico 2 running the base firmware's request rules.
struct FakeDevice {
    loader: Loader,
    trust: TrustState,
    status: PayloadStatus,
    staging: Vec<u8>,
    installed: Option<Vec<u8>>,
    fw: Option<Fw>,
}

impl FakeDevice {
    fn handle(&mut self, key: Key<'_>, body: &[u8], out: &mut [u8]) -> Result<usize, HandlerError> {
        if let Some(fw) = &mut self.fw {
            let ctx = FwContext {
                node: NODE,
                trust: self.trust,
                self_test_ok: true,
            };
            if let Some(r) = fw.svc.handle(&mut fw.sim, &ctx, key, body, out) {
                return r;
            }
        }
        let cmd = Command::parse(key, body)?.ok_or(HandlerError::NotFound)?;
        match cmd {
            Command::Info => DeviceInfo {
                abi_version: 4,
                capacity: 65_536,
                status: self.status,
                chip: "rp2350",
                firmware: "fake-0.6",
                node: Some(NODE),
                caps: Caps::NONE,
            }
            .encode(out)
            .ok_or(HandlerError::BufferTooSmall),
            Command::TrustInfo => self
                .trust
                .info(false)
                .encode(out)
                .ok_or(HandlerError::BufferTooSmall),
            Command::TrustClaim { .. } => Err(HandlerError::Denied),
            // A board without an esp-base chip profile (rp-base).
            Command::Board => Err(HandlerError::NotFound),
            // Serial start/stop: empty bodies, no signature (the gateway
            // checked it).
            Command::Start => match self.installed {
                Some(_) => {
                    self.status = PayloadStatus::Running;
                    Ok(0)
                }
                None => Err(HandlerError::InvalidRequest),
            },
            Command::Stop => {
                if self.status == PayloadStatus::Running {
                    self.status = PayloadStatus::Stopped;
                }
                Ok(0)
            }
            Command::Begin { .. } | Command::Chunk { .. } | Command::Commit { .. } => {
                let (next, effect) = self.loader.step(&cmd, 65_536);
                self.loader = next;
                match effect? {
                    Effect::Erase { .. } => {
                        self.staging.clear();
                        self.status = PayloadStatus::Loading;
                    }
                    Effect::Write { data, .. } => self.staging.extend_from_slice(data),
                    Effect::Activate { image, auth } => {
                        self.trust = self.trust.accept_commit(&image, auth.as_ref())?;
                        self.installed = Some(std::mem::take(&mut self.staging));
                        self.status = PayloadStatus::Stopped;
                    }
                    Effect::None => {}
                }
                Ok(0)
            }
        }
    }
}

/// Runs `device` on the cable; reports every request it gets. With
/// `slow`, answers requests on that key only after that long.
fn spawn_device(
    io: ChannelIo,
    mut device: FakeDevice,
    slow: Option<(Key<'static>, Duration)>,
) -> mpsc::UnboundedReceiver<Seen> {
    let (seen_tx, seen) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut link = SerialLink::<ChannelIo, 0>::with_inbox(io);
        let mut buf = vec![0u8; RECV_BUF_LEN];
        let mut out = [0u8; 256];
        loop {
            let (id, result, delay) = match link.next_frame(&mut buf).await {
                Ok(Frame::Request { id, key, payload }) => {
                    let _ = seen_tx.send((key.as_str().to_owned(), payload.len()));
                    let delay = slow.filter(|(k, _)| *k == key).map(|(_, d)| d);
                    (id, device.handle(key, payload, &mut out), delay)
                }
                Ok(_) => continue,
                Err(_) => break,
            };
            if let Some(d) = delay {
                tokio::time::sleep(d).await;
            }
            let result = result.map(|n| &out[..n]);
            if link.send(&Frame::Response { id, result }).await.is_err() {
                break;
            }
            // A reboot the updater scheduled fires after the reply.
            if let Some(fw) = device.fw.take() {
                device.fw = Some(if fw.sim.scheduled().is_some() {
                    let mut sim = fw.sim;
                    sim.reset().expect("bootable");
                    Fw::boot(sim)
                } else {
                    fw
                });
            }
        }
    });
    seen
}

fn claimed_pico2(key: PublicKey) -> FakeDevice {
    let mut sim = Rp2350Sim::new(256 * 1024);
    sim.program(Slot::A, &sim::image(40_000, V1_0, false, 1));
    FakeDevice {
        loader: Loader::Idle,
        trust: TrustState::Claimed {
            key,
            min_version: Version(0),
        },
        status: PayloadStatus::Empty,
        staging: Vec::new(),
        installed: None,
        fw: Some(Fw::boot(sim)),
    }
}

/// A stand-in for the gateway's sidecar: sets the caller's
/// `x-forwarded-client-cert` as Envoy does after mTLS.
#[derive(Clone)]
struct Sidecar {
    inner: GrpcServer,
    xfcc: http::HeaderValue,
}

impl Service<http::Request<Body>> for Sidecar {
    type Response = http::Response<Body>;
    type Error = Infallible;
    type Future = BoxFuture<Self::Response, Infallible>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<Body>) -> Self::Future {
        let mut req = req;
        req.headers_mut()
            .insert("x-forwarded-client-cert", self.xfcc.clone());
        self.inner.call(req)
    }
}

/// Serves `server` behind a sidecar for `principal` on 127.0.0.1:0.
async fn sidecar(server: GrpcServer, principal: &str) -> (String, oneshot::Sender<()>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    let xfcc = format!("By=spiffe://infra.local/ns/robots/sa/mw-gateway;URI={principal}")
        .parse()
        .expect("header");
    let (stop, stopped) = oneshot::channel::<()>();
    tokio::spawn(
        tonic::transport::Server::builder().serve_with_incoming_shutdown(
            Sidecar {
                inner: server,
                xfcc,
            },
            tokio_stream::wrappers::TcpListenerStream::new(listener),
            async {
                let _ = stopped.await;
            },
        ),
    );
    (addr, stop)
}

async fn zenoh_pair() -> (ZenohTransport, ZenohTransport) {
    let port = TcpListener::bind("127.0.0.1:0")
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
    let gateway = SessionConfig::new()
        .connect(endpoint)
        .build()
        .expect("config");
    let cluster = ZenohTransport::open(cluster).await.expect("open cluster");
    let gateway = ZenohTransport::open(gateway).await.expect("open gateway");
    (cluster, gateway)
}

struct Rig {
    _cluster: ZenohTransport,
    gateway: mw_gateway::Gateway,
    seen: mpsc::UnboundedReceiver<Seen>,
    /// `gw/sys=...` entries for the updater's and the stranger's sidecar.
    updater: String,
    stranger: String,
    _stops: (oneshot::Sender<()>, oneshot::Sender<()>),
}

impl Rig {
    /// Drains what the device saw so far.
    fn drain(&mut self) -> Vec<Seen> {
        let mut v = Vec::new();
        while let Ok(s) = self.seen.try_recv() {
            v.push(s);
        }
        v
    }
}

/// The gateway with `UPDATER` allowlisted, bound to a claimed Pico 2 and
/// past its `sys/trust/info`.
async fn rig() -> Rig {
    rig_with(None).await
}

/// A fresh state directory for one rig.
fn state_dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("mw-gateway-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

async fn rig_with(slow: Option<(Key<'static>, Duration)>) -> Rig {
    let name = format!(
        "{:x}",
        SESSION.fetch_add(1000, std::sync::atomic::Ordering::Relaxed)
    );
    rig_in(slow, ControlStore::in_dir(state_dir(&name))).await
}

async fn rig_in(slow: Option<(Key<'static>, Duration)>, store: ControlStore) -> Rig {
    rig_claimed(slow, store, seed().public_key()).await
}

/// [`rig_in`] with the device claimed by `key`.
async fn rig_claimed(
    slow: Option<(Key<'static>, Duration)>,
    store: ControlStore,
    key: PublicKey,
) -> Rig {
    let (cluster, session) = zenoh_pair().await;
    let (device_io, gateway_io) = pair();
    let mut seen = spawn_device(device_io, claimed_pico2(key), slow);
    let config = GatewayConfig::from_env(&|k: &str| {
        (k == "MW_GATEWAY_UPDATERS").then(|| UPDATER.to_owned())
    })
    .expect("config");
    let (gateway, sys) = start_with_store(
        gateway_io,
        session,
        config.bridge_at(0),
        config.shell_options(),
        store,
    )
    .await
    .expect("start");
    let (svc, resp) = responder(8);
    tokio::spawn(serve_sys(resp, sys));
    let server =
        GrpcServer::new().with_responder(svc.with_principal_source(PrincipalSource::SidecarXfcc));
    let (up, stop_up) = sidecar(server.clone(), UPDATER).await;
    let (other, stop_other) = sidecar(server, STRANGER).await;
    // Bound (sys/info) and the trust facts known (sys/trust/info).
    tokio::time::timeout(TIMEOUT, async {
        while let Some((key, _)) = seen.recv().await {
            if key == "sys/trust/info" {
                break;
            }
        }
    })
    .await
    .expect("the gateway asks sys/trust/info");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let entry = |addr: &str| format!("gw/sys={addr}@mw-gateway.robots.svc.cluster.local:50051");
    Rig {
        _cluster: cluster,
        gateway,
        seen,
        updater: entry(&up),
        stranger: entry(&other),
        _stops: (stop_up, stop_other),
    }
}

/// Client sessions: each run gets fresh request ids, as the binary's
/// clock-and-pid session does.
static SESSION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Runs `job` as the caller behind `entry`, on a blocking thread (the
/// client owns its own runtime).
async fn run_job(entry: &str, job: Job) -> Result<String, mwctl::error::Error> {
    let entry = entry.to_owned();
    let session = SESSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    tokio::task::spawn_blocking(move || {
        let endpoints = StaticEndpoints::parse(&entry).expect("entry");
        let ep = endpoints.get("gw/sys").expect("gw/sys");
        let call = MeshSysCall::new(ep, Duration::from_millis(50)).expect("runtime");
        let gw = GwEndpoint::new(call, RequestIds::new(session), Duration::from_secs(5), 3);
        let mut device = Device::new(gw);
        execute(job, &mut device, || {
            std::thread::sleep(Duration::from_millis(50))
        })
    })
    .await
    .expect("client thread")
}

fn upload(image: &[u8], start: bool) -> Job {
    Job::Upload {
        image: image.to_vec(),
        signer: signer(),
        version: Version(1_761_500_000),
        start: start.then_some(middleware::loader::trust::ControlCounter(1_761_500_000_123)),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn rg25_an_updater_uploads_a_signed_payload_and_others_are_denied() {
    let mut rig = rig().await;
    let image: Vec<u8> = (0..1500u32).map(|i| (i * 7 % 251) as u8).collect();
    // Someone else: Denied, and nothing reaches the device.
    rig.drain();
    let err = run_job(&rig.stranger, upload(&image, true))
        .await
        .unwrap_err();
    assert_eq!(err.status(), Some(HandlerError::Denied), "{err}");
    let seen = rig.drain();
    assert!(
        seen.iter()
            .all(|(k, _)| !k.starts_with("sys/loader/") && !k.starts_with("sys/payload/")),
        "{seen:?}"
    );
    // The updater: sys/info (the control target), begin, three chunks,
    // the signed commit, a start whose body the gateway emptied after
    // checking its signature.
    let text = run_job(&rig.updater, upload(&image, true))
        .await
        .expect("upload");
    assert!(text.contains("installed 1500 bytes"), "{text}");
    let seen = rig.drain();
    let keys: Vec<&str> = seen.iter().map(|(k, _)| k.as_str()).collect();
    let i = keys
        .iter()
        .position(|k| *k == "sys/loader/begin")
        .expect("begin");
    // The gateway's own read-only polls (its 30 s recheck of the bound
    // device) may land anywhere in the run; every other key must be the
    // upload's, in order.
    let mutating: Vec<&str> = keys[i..]
        .iter()
        .copied()
        .filter(|k| !matches!(*k, "sys/info" | "sys/trust/info"))
        .collect();
    assert_eq!(
        mutating,
        [
            "sys/loader/begin",
            "sys/loader/chunk",
            "sys/loader/chunk",
            "sys/loader/chunk",
            "sys/loader/commit",
            "sys/payload/start"
        ]
    );
    assert!(
        keys[..i].contains(&"sys/info"),
        "the control target comes from sys/info"
    );
    assert_eq!(seen[i + 4].1, 68, "the signed commit reaches the device");
    assert_eq!(
        seen.last().map(|s| s.1),
        Some(0),
        "serial start, empty body"
    );
    // A start the updater did not sign is Denied by the gateway.
    let unsigned = run_job(
        &rig.updater,
        Job::Control {
            op: middleware::loader::trust::ControlOp::Start,
            signer: Box::new(Seed::from_file_bytes(&[9u8; 32]).expect("seed")),
            counter: middleware::loader::trust::ControlCounter(1_761_500_000_999),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(unsigned.status(), Some(HandlerError::Denied));
    // The same signed upload again: the version is used up; the device's
    // own trust check refuses the commit.
    let again = run_job(&rig.updater, upload(&image, false))
        .await
        .unwrap_err();
    assert_eq!(again.status(), Some(HandlerError::Denied), "{again}");
    let report: Report = rig.gateway.stop().await;
    assert!(report.updates.denied >= 2, "{report:?}");
    assert!(report.updates.forwarded >= 6, "{report:?}");
}

/// A raw `gw/sys` call as the caller behind `entry`, with an explicit
/// request id.
async fn raw(
    entry: &str,
    key: Key<'static>,
    body: Vec<u8>,
    id: &'static str,
) -> Result<Vec<u8>, CallError> {
    raw_within(entry, key, body, id, Duration::from_secs(5)).await
}

async fn raw_within(
    entry: &str,
    key: Key<'static>,
    body: Vec<u8>,
    id: &'static str,
    timeout: Duration,
) -> Result<Vec<u8>, CallError> {
    let entry = entry.to_owned();
    tokio::task::spawn_blocking(move || {
        let endpoints = StaticEndpoints::parse(&entry).expect("entry");
        let mut call = MeshSysCall::new(endpoints.get("gw/sys").expect("gw/sys"), Duration::ZERO)
            .expect("runtime");
        let mut gw = vec![0u8; 1 + key.as_str().len() + body.len()];
        let n = encode_sys_request(key, &body, &mut gw).expect("body");
        call.call(&gw[..n], id, timeout)
    })
    .await
    .expect("client thread")
}

#[tokio::test(flavor = "multi_thread")]
async fn rg25_a_resend_with_the_same_request_id_runs_once() {
    let mut rig = rig().await;
    for _ in 0..3 {
        assert_eq!(
            raw(&rig.updater, keys::BEGIN, begin_body(), "r-1").await,
            Ok(Vec::new())
        );
    }
    let mut chunk = vec![0u8; 4];
    chunk.extend_from_slice(b"wasm");
    for _ in 0..2 {
        assert_eq!(
            raw(&rig.updater, keys::CHUNK, chunk.clone(), "r-2").await,
            Ok(Vec::new())
        );
    }
    // The same id for another request is refused.
    assert_eq!(
        raw(&rig.updater, keys::CHUNK, b"\0\0\0\0x".to_vec(), "r-2").await,
        Err(CallError::Status(HandlerError::InvalidRequest))
    );
    // The stranger cannot touch the updater's transfer.
    assert_eq!(
        raw(&rig.stranger, keys::COMMIT, Vec::new(), "s-1").await,
        Err(CallError::Status(HandlerError::Denied))
    );
    let seen: Vec<String> = rig.drain().into_iter().map(|(k, _)| k).collect();
    let loader: Vec<&str> = seen
        .iter()
        .map(String::as_str)
        .filter(|k| k.starts_with("sys/loader/"))
        .collect();
    assert_eq!(loader, ["sys/loader/begin", "sys/loader/chunk"]);
    let report = rig.gateway.stop().await;
    assert_eq!(report.updates.replayed, 3, "{report:?}");
}

fn begin_body() -> Vec<u8> {
    let mut begin = vec![0u8; 8];
    begin[..4].copy_from_slice(&4u32.to_le_bytes());
    begin[4..].copy_from_slice(&middleware::crc::Crc32::checksum(b"wasm").to_le_bytes());
    begin
}

#[tokio::test(flavor = "multi_thread")]
async fn rg25_a_late_answer_after_the_callers_deadline_is_replayed_not_rerun() {
    let mut rig = rig_with(Some((keys::BEGIN, Duration::from_millis(1500)))).await;
    rig.drain();
    let short = Duration::from_millis(300);
    // The caller gives up before the device answers.
    assert_eq!(
        raw_within(&rig.updater, keys::BEGIN, begin_body(), "late-1", short).await,
        Err(CallError::Status(HandlerError::DeadlineExceeded))
    );
    // Its resend while the first copy is at the device: retriable.
    assert_eq!(
        raw_within(&rig.updater, keys::BEGIN, begin_body(), "late-1", short).await,
        Err(CallError::Status(HandlerError::Unavailable))
    );
    tokio::time::sleep(Duration::from_millis(1500)).await;
    // The device's late answer was kept: the resend gets it.
    assert_eq!(
        raw(&rig.updater, keys::BEGIN, begin_body(), "late-1").await,
        Ok(Vec::new())
    );
    let begins = rig
        .drain()
        .into_iter()
        .filter(|(k, _)| k == "sys/loader/begin")
        .count();
    assert_eq!(begins, 1, "the begin reached the device once");
    let _ = rig.gateway.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rg26_an_rp2350_firmware_update_reaches_the_device_and_is_confirmed() {
    let mut rig = rig().await;
    let image = load_image(&sim::image(20_000, V1_1, true, 2), None).expect("image");
    // Not an updater: Denied before anything is sent.
    let err = run_job(
        &rig.stranger,
        Job::FwUpdate {
            image: image.clone(),
            signer: signer(),
            reboot: true,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err.status(), Some(HandlerError::Denied), "{err}");
    rig.drain();
    let text = run_job(
        &rig.updater,
        Job::FwUpdate {
            image,
            signer: signer(),
            reboot: true,
        },
    )
    .await
    .expect("fw update");
    assert!(text.contains("rebooted and confirmed"), "{text}");
    let keys: Vec<String> = rig.drain().into_iter().map(|(k, _)| k).collect();
    assert_eq!(
        keys.iter().filter(|k| *k == "sys/fw/chunk").count(),
        20_000usize.div_ceil(512)
    );
    for k in [
        "sys/fw/info",
        "sys/fw/begin",
        "sys/fw/commit",
        "sys/fw/reboot",
        "sys/fw/confirm",
    ] {
        assert!(keys.iter().any(|s| s == k), "{k} in {keys:?}");
    }
    let info = run_job(&rig.updater, Job::FwInfo).await.expect("fw info");
    assert!(info.contains(StateCode::Confirmed.name()), "{info}");
    let _ = rig.gateway.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rg27_a_start_whose_counter_cannot_be_persisted_is_refused_not_forwarded() {
    // The state "directory" is a regular file: every write fails.
    let dir = state_dir("unwritable");
    std::fs::create_dir_all(&dir).expect("dir");
    let file = dir.join("not-a-dir");
    std::fs::write(&file, b"x").expect("file");
    let mut rig = rig_in(None, ControlStore::in_dir(&file)).await;
    let image = b"\0asm-payload".to_vec();
    // Uploads do not need the counter: the device checks the version.
    run_job(&rig.updater, upload(&image, false))
        .await
        .expect("upload");
    rig.drain();
    let err = run_job(
        &rig.updater,
        Job::Control {
            op: middleware::loader::trust::ControlOp::Start,
            signer: signer(),
            counter: middleware::loader::trust::ControlCounter(1_761_500_000_500),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err.status(), Some(HandlerError::Unavailable), "{err}");
    let seen = rig.drain();
    assert!(
        seen.iter().all(|(k, _)| k != "sys/payload/start"),
        "{seen:?}"
    );
    let report = rig.gateway.stop().await;
    assert!(report.updates.unpersisted >= 1, "{report:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn rg27_an_accepted_start_is_persisted_before_it_reaches_the_device() {
    let dir = state_dir("persisted");
    let mut rig = rig_in(None, ControlStore::in_dir(&dir)).await;
    run_job(&rig.updater, upload(b"\0asm-x", true))
        .await
        .expect("upload and start");
    assert!(rig.drain().iter().any(|(k, _)| k == "sys/payload/start"));
    assert_eq!(
        ControlStore::in_dir(&dir).load(),
        Persisted::At(middleware::loader::trust::ControlCounter(1_761_500_000_123))
    );
    let _ = rig.gateway.stop().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// A device claimed with a P-256 key (a token key, `mw-update --pkcs11`):
/// the gateway checks the signed start against it with the device's own
/// verification and forwards it, persisted first (RG27); a forged start
/// (another key, or the Ed25519 key file) is refused at the gateway and
/// never reaches the device.
#[tokio::test(flavor = "multi_thread")]
async fn rg20_a_p256_claimed_devices_signed_start_passes_and_a_forged_one_is_refused() {
    let dir = state_dir("p256");
    let mut rig = rig_claimed(None, ControlStore::in_dir(&dir), p256().public_key()).await;
    let image = b"\0asm-p256".to_vec();
    let text = run_job(
        &rig.updater,
        Job::Upload {
            image: image.clone(),
            signer: Box::new(p256()),
            version: Version(1_761_500_000),
            start: None,
        },
    )
    .await
    .expect("a P-256 signed upload");
    assert!(text.contains("installed"), "{text}");
    rig.drain();
    for forger in [
        Box::new(P256Key::from_scalar(&[0x21; 32]).expect("scalar")) as Box<dyn Signer + Send>,
        signer(),
    ] {
        let err = run_job(
            &rig.updater,
            Job::Control {
                op: ControlOp::Start,
                signer: forger,
                counter: ControlCounter(1_761_500_000_700),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(err.status(), Some(HandlerError::Denied), "{err}");
    }
    assert!(
        rig.drain().iter().all(|(k, _)| k != "sys/payload/start"),
        "a forged start never reaches the device"
    );
    let text = run_job(
        &rig.updater,
        Job::Control {
            op: ControlOp::Start,
            signer: Box::new(p256()),
            counter: ControlCounter(1_761_500_000_800),
        },
    )
    .await
    .expect("signed P-256 start");
    assert!(text.starts_with("started"), "{text}");
    let seen = rig.drain();
    assert_eq!(
        seen.iter()
            .find(|(k, _)| k == "sys/payload/start")
            .map(|s| s.1),
        Some(0),
        "forwarded as the serial start: {seen:?}"
    );
    assert_eq!(
        ControlStore::in_dir(&dir).load(),
        Persisted::At(ControlCounter(1_761_500_000_800))
    );
    let report = rig.gateway.stop().await;
    assert!(report.updates.denied >= 2, "{report:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
