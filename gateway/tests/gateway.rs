//! The gateway end to end, in process: a fake device on one end of an
//! in-memory serial cable (`io::pair`, the real `SerialLink` framing on
//! both ends), the gateway on the other, and a "cluster" Zenoh session
//! listening on one explicit 127.0.0.1 port (reserved by binding port 0,
//! never probed) that the gateway's session connects to. No serial port is
//! opened.

use std::time::Duration;

use middleware::SenderId;
use middleware::discovery::beacon::{beacon_key, encode};
use middleware::discovery::{Browse, Endpoint, Event, Service, ServiceKind};
use middleware::key::{Key, KeyFilter};
use middleware::loader::{DeviceInfo, PayloadStatus, keys};
use middleware::serial::frame::Frame;
use middleware::serial::link::{RECV_BUF_LEN, SerialLink};
use middleware::stream::{Chunker, ESPNOW_CHUNK_DATA, chunk};
use middleware::transport::{HandlerError, Publisher, Request, RequestMeta, Subscribe, Subscriber};
use middleware::zenoh::{SessionConfig, ZenohTransport};
use mw_gateway::bridge::encode_sys_request;
use mw_gateway::io::{ChannelIo, pair};
use mw_gateway::{Gateway, GatewayConfig, SysForward, service, start};
use tokio::sync::mpsc;

const MAC: [u8; 6] = [0x24, 0x6F, 0x28, 0xAA, 0xBB, 0xCC];
const DEVICE: SenderId = SenderId::from_mac(MAC);
const OTHER: SenderId = SenderId::from_mac([0x02, 0, 0, 0, 0, 0x66]);
const CAM: ServiceKind<'static> = ServiceKind::from_static("cam/jpeg");
const TIMEOUT: Duration = Duration::from_secs(10);

/// What the fake device saw from the gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seen {
    Request(String),
    Publish(String, Vec<u8>),
}

/// A device on the serial cable: answers `sys/info` with its node id,
/// reports every frame it gets, publishes what the test sends it.
fn fake_device(
    io: ChannelIo,
) -> (
    mpsc::Sender<(String, Vec<u8>)>,
    mpsc::UnboundedReceiver<Seen>,
) {
    let (publish, mut to_publish) = mpsc::channel::<(String, Vec<u8>)>(64);
    let (seen_tx, seen) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut link = SerialLink::<ChannelIo, 0>::with_inbox(io);
        let mut buf = vec![0u8; RECV_BUF_LEN];
        let mut info = [0u8; 128];
        let n = DeviceInfo {
            abi_version: 4,
            capacity: 65_536,
            status: PayloadStatus::Empty,
            chip: "esp32",
            firmware: "fake-0.6",
            node: Some(DEVICE),
        }
        .encode(&mut info)
        .expect("info");
        loop {
            tokio::select! {
                p = to_publish.recv() => {
                    let Some((key, payload)) = p else { break };
                    let key = Key::new(&key).expect("key");
                    if link.send(&Frame::Publish { key, payload: &payload }).await.is_err() {
                        break;
                    }
                }
                f = link.next_frame(&mut buf) => match f {
                    Ok(Frame::Request { id, key, .. }) => {
                        let _ = seen_tx.send(Seen::Request(key.as_str().to_owned()));
                        let result = if key == keys::INFO {
                            Ok(&info[..n])
                        } else {
                            Err(HandlerError::NotFound)
                        };
                        if link.send(&Frame::Response { id, result }).await.is_err() {
                            break;
                        }
                    }
                    Ok(Frame::Publish { key, payload }) => {
                        let _ = seen_tx.send(Seen::Publish(key.as_str().to_owned(), payload.to_vec()));
                    }
                    Ok(Frame::Response { .. }) => {}
                    Err(_) => break,
                },
            }
        }
    });
    (publish, seen)
}

/// The cluster session (listening on a fresh loopback port) and a session
/// for the gateway connected to it.
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
    let gateway = SessionConfig::new()
        .connect(endpoint)
        .build()
        .expect("config");
    let cluster = ZenohTransport::open(cluster).await.expect("open cluster");
    let gateway = ZenohTransport::open(gateway).await.expect("open gateway");
    (cluster, gateway)
}

struct Rig {
    cluster: ZenohTransport,
    gateway: Gateway,
    sys: SysForward,
    publish: mpsc::Sender<(String, Vec<u8>)>,
    seen: mpsc::UnboundedReceiver<Seen>,
}

async fn rig(config: GatewayConfig) -> Rig {
    let (cluster, session) = zenoh_pair().await;
    let (device_io, gateway_io) = pair();
    let (publish, seen) = fake_device(device_io);
    let (gateway, sys) = start(gateway_io, session, config.bridge())
        .await
        .expect("start");
    Rig {
        cluster,
        gateway,
        sys,
        publish,
        seen,
    }
}

fn cam_beacon(instance: &str, node: Option<SenderId>, ttl_s: u32) -> (String, Vec<u8>) {
    let mut key = [0u8; 64];
    let key = beacon_key(CAM, &mut key).expect("key").as_str().to_owned();
    let mut out = [0u8; 256];
    let n = encode(
        &Service {
            kind: CAM,
            instance,
            node,
            endpoint: Endpoint::Local,
            txt: b"\x10key=app/cam/jpeg",
            ttl_s,
        },
        &mut out,
    )
    .expect("beacon");
    (key, out[..n].to_vec())
}

/// Publishes `beacon` every 100 ms until the browser reports an event for
/// `instance` (the gateway binds on its first tick; earlier beacons are
/// refused, as the firmware's re-announcing makes harmless), returning it.
async fn beacon_until_event<B: Browse>(
    browser: &mut B,
    publish: &mpsc::Sender<(String, Vec<u8>)>,
    beacon: (String, Vec<u8>),
    instance: &str,
) -> (bool, Option<SenderId>) {
    let mut buf = [0u8; 512];
    tokio::time::timeout(TIMEOUT, async {
        loop {
            publish.send(beacon.clone()).await.expect("device");
            let wait =
                tokio::time::timeout(Duration::from_millis(100), browser.next(&mut buf)).await;
            match wait {
                Ok(Ok(Event::Up(s))) if s.instance == instance => return (true, s.node),
                Ok(Ok(Event::Down { instance: i, .. })) if i == instance => return (false, None),
                _ => {}
            }
        }
    })
    .await
    .expect("no discovery event")
}

#[tokio::test(flavor = "multi_thread")]
async fn v16_device_beacon_is_announced_on_zenoh_with_the_device_node() {
    let mut r = rig(GatewayConfig::default()).await;
    let mut browser = r.cluster.discovery();
    browser
        .browse(KeyFilter::Exact(CAM.key()))
        .await
        .expect("browse");
    let (up, node) = beacon_until_event(
        &mut browser,
        &r.publish,
        cam_beacon("cam-aabbcc", None, 30),
        "cam-aabbcc",
    )
    .await;
    assert!(up);
    assert_eq!(node, Some(DEVICE), "node bound to the device MAC");
    // Its goodbye withdraws it.
    let (up, _) = beacon_until_event(
        &mut browser,
        &r.publish,
        cam_beacon("cam-aabbcc", None, 0),
        "cam-aabbcc",
    )
    .await;
    assert!(!up);
    let report = r.gateway.stop().await;
    assert_eq!(report.device, Some(DEVICE));
    assert_eq!(
        (report.counters.announced, report.counters.withdrawn),
        (1, 1)
    );
    // The gateway asked the device who it is.
    let mut asked = false;
    while let Ok(s) = r.seen.try_recv() {
        asked |= s == Seen::Request("sys/info".into());
    }
    assert!(asked);
}

#[tokio::test(flavor = "multi_thread")]
async fn v16_spoofed_beacon_is_refused() {
    let r = rig(GatewayConfig::default()).await;
    let mut browser = r.cluster.discovery();
    browser
        .browse(KeyFilter::Exact(CAM.key()))
        .await
        .expect("browse");
    // Bound once the device's own beacon is up.
    beacon_until_event(
        &mut browser,
        &r.publish,
        cam_beacon("cam-1", Some(DEVICE), 30),
        "cam-1",
    )
    .await;
    for _ in 0..5 {
        r.publish
            .send(cam_beacon("cam-spoof", Some(OTHER), 30))
            .await
            .expect("device");
    }
    // A later genuine beacon still gets through; the spoofed one never did.
    let mut buf = [0u8; 512];
    tokio::time::timeout(TIMEOUT, async {
        loop {
            r.publish
                .send(cam_beacon("cam-2", None, 30))
                .await
                .expect("device");
            if let Ok(Ok(e)) =
                tokio::time::timeout(Duration::from_millis(100), browser.next(&mut buf)).await
            {
                match e {
                    Event::Up(s) if s.instance == "cam-2" => break,
                    Event::Up(s) => assert_ne!(s.instance, "cam-spoof", "spoofed beacon announced"),
                    Event::Down { .. } => {}
                }
            }
        }
    })
    .await
    .expect("cam-2 up");
    let report = r.gateway.stop().await;
    assert!(report.counters.beacons_refused >= 5, "{report:?}");
    assert_eq!(report.counters.announced, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn v17_camera_chunks_cross_the_gateway_intact() {
    let mut r = rig(GatewayConfig::default()).await;
    let mut sub = r
        .cluster
        .subscribe(KeyFilter::Exact(Key::from_static("app/cam/jpeg")))
        .await
        .expect("subscribe");
    let mut buf = vec![0u8; 2048];
    // Declarations propagate asynchronously: probe until one arrives.
    tokio::time::timeout(TIMEOUT, async {
        loop {
            r.publish
                .send(("app/cam/jpeg".into(), b"probe".to_vec()))
                .await
                .expect("device");
            if let Ok(Ok(s)) =
                tokio::time::timeout(Duration::from_millis(100), sub.recv(&mut buf)).await
            {
                assert_eq!(s.key.as_str(), "app/cam/jpeg");
                break;
            }
        }
    })
    .await
    .expect("probe");
    let frame: Vec<u8> = (0..5000u32).map(|i| (i * 7 % 251) as u8).collect();
    let chunker = Chunker::new(3, 5000, ESPNOW_CHUNK_DATA).expect("chunker");
    let mut sent = Vec::new();
    for plan in chunker.chunks() {
        let mut msg = vec![0u8; 256];
        let n = plan
            .header
            .write(&frame[plan.range.clone()], &mut msg)
            .expect("chunk");
        msg.truncate(n);
        sent.push(msg.clone());
        r.publish
            .send(("app/cam/jpeg".into(), msg))
            .await
            .expect("device");
    }
    let mut got = Vec::new();
    let mut rebuilt = vec![0u8; 5000];
    tokio::time::timeout(TIMEOUT, async {
        while got.len() < sent.len() {
            let s = sub.recv(&mut buf).await.expect("recv");
            if s.payload == b"probe" {
                continue;
            }
            let c = chunk::decode(s.payload).expect("a chunk");
            let at = c.header.offset as usize;
            rebuilt[at..at + c.data.len()].copy_from_slice(c.data);
            got.push(s.payload.to_vec());
        }
    })
    .await
    .expect("every chunk");
    assert_eq!(got, sent, "byte for byte, in order");
    assert_eq!(rebuilt, frame);
    let report = r.gateway.stop().await;
    assert_eq!(report.drops.uplink, 0);
    assert!(report.counters.published >= sent.len() as u32);
}

#[tokio::test(flavor = "multi_thread")]
async fn v18_sys_info_is_forwarded_and_the_loader_is_refused() {
    let config = GatewayConfig::default();
    let mut r = rig(config.clone()).await;
    // Through the served kind, as the runner calls it.
    let (spec, mut served) = service(&config, r.sys).expect("spec").into_parts();
    assert_eq!(spec.serves, ["gw/sys"]);
    assert!(spec.uses_zenoh());
    let handler = &mut served[0].handler;
    let mut links = mw_service::Links::new();
    let mut body = [0u8; 64];
    let mut out = [0u8; 2048];

    let n = encode_sys_request(keys::INFO, b"", &mut body).expect("body");
    let req = Request::new(Key::from_static("gw/sys"), &body[..n]);
    let mut ctx = mw_service::Ctx::new(RequestMeta::NONE, None, &mut links);
    let len = handler
        .call(&mut ctx, &req, &mut out)
        .await
        .expect("sys/info");
    let info = DeviceInfo::decode(&out[..len]).expect("decodes");
    assert_eq!((info.chip, info.node), ("esp32", Some(DEVICE)));

    for denied in [
        keys::BEGIN,
        keys::CHUNK,
        keys::COMMIT,
        keys::TRUST_CLAIM,
        keys::START,
    ] {
        let n = encode_sys_request(denied, b"\x00\x00\x00\x00", &mut body).expect("body");
        let req = Request::new(Key::from_static("gw/sys"), &body[..n]);
        let mut ctx = mw_service::Ctx::new(RequestMeta::NONE, None, &mut links);
        assert_eq!(
            handler.call(&mut ctx, &req, &mut out).await,
            Err(HandlerError::Denied),
            "{denied}"
        );
    }
    let mut ctx = mw_service::Ctx::new(RequestMeta::NONE, None, &mut links);
    let bad = Request::new(Key::from_static("gw/sys"), b"\x40");
    assert_eq!(
        handler.call(&mut ctx, &bad, &mut out).await,
        Err(HandlerError::InvalidRequest)
    );

    // sys/log is read from the gateway's tail of the device's log.
    r.publish
        .send(("sys/log".into(), b"boot ok\n".to_vec()))
        .await
        .expect("device");
    let n = encode_sys_request(keys::LOG, b"", &mut body).expect("body");
    let req = Request::new(Key::from_static("gw/sys"), &body[..n]);
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let mut ctx = mw_service::Ctx::new(RequestMeta::NONE, None, &mut links);
            let len = handler
                .call(&mut ctx, &req, &mut out)
                .await
                .expect("sys/log");
            if out[..len].ends_with(b"boot ok\n") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("log line");

    let report = r.gateway.stop().await;
    assert_eq!(report.counters.sys_denied, 5);
    assert_eq!(report.counters.logs, 1);
    // No loader or trust request ever reached the device.
    while let Ok(s) = r.seen.try_recv() {
        if let Seen::Request(k) = s {
            assert_eq!(k, "sys/info", "only sys/info reaches the device");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn v11_cluster_samples_reach_the_device_on_declared_filters_only() {
    let config = GatewayConfig::from_env(&|k| match k {
        "MW_GATEWAY_NAMESPACE" => Some("bench".into()),
        "MW_GATEWAY_DOWNLINK" => Some("app/bench/robot/cmd/**".into()),
        _ => None,
    })
    .expect("config");
    let mut r = rig(config).await;
    let got = tokio::time::timeout(TIMEOUT, async {
        loop {
            r.cluster
                .publish(
                    Key::from_static("app/bench/cam/jpeg"),
                    b"not for the device",
                )
                .await
                .expect("publish");
            r.cluster
                .publish(Key::from_static("app/bench/robot/cmd/speed"), b"\x05")
                .await
                .expect("publish");
            if let Ok(Some(Seen::Publish(k, p))) =
                tokio::time::timeout(Duration::from_millis(100), r.seen.recv()).await
            {
                return (k, p);
            }
        }
    })
    .await
    .expect("downlink");
    assert_eq!(got, ("app/robot/cmd/speed".to_owned(), vec![5]));
    // And the device's own app keys go up under the namespace.
    let mut sub = r
        .cluster
        .subscribe(KeyFilter::Prefix(Key::from_static("app/bench")))
        .await
        .expect("subscribe");
    let mut buf = [0u8; 64];
    let key = tokio::time::timeout(TIMEOUT, async {
        loop {
            r.publish
                .send(("app/temp".into(), b"21".to_vec()))
                .await
                .expect("device");
            let got = tokio::time::timeout(Duration::from_millis(100), sub.recv(&mut buf)).await;
            if let Ok(Ok(s)) = got
                && s.key.as_str() == "app/bench/temp"
            {
                return s.key.as_str().to_owned();
            }
        }
    })
    .await
    .expect("uplink");
    assert_eq!(key, "app/bench/temp");
    let report = r.gateway.stop().await;
    assert!(report.counters.downlinked >= 1);
    // Every sample the device saw was on the declared filter.
    while let Ok(s) = r.seen.try_recv() {
        if let Seen::Publish(k, _) = s {
            assert_eq!(k, "app/robot/cmd/speed");
        }
    }
}
