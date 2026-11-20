//! The ESP-NOW gateway end to end, in one process: `espnow_gw::run` on one
//! end of a `ChannelIo` pair, a fake USB bridge with a fake ESP-NOW device
//! behind it on the other, the gateway's Zenoh session connected to an
//! observer session listening on a fresh loopback port. No serial port is
//! opened. EG3, EG6–EG10.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use embedded_io_async::{Read, Write};
use middleware::discovery::beacon;
use middleware::discovery::{Browse, Endpoint, Event, Service, ServiceKind};
use middleware::espnow::rpc::Retry;
use middleware::espnow::{Frame, MAX_FRAME, Mac};
use middleware::loader::{keys, peers};
use middleware::radio_bridge::{self, BROADCAST, MAX_WIRE, Msg, Stats, VERSION};
use middleware::rpc::Message;
use middleware::serial::frame::{Split, Splitter};
use middleware::transport::{
    HandlerError, Publisher, RequestOptions, Requester, SenderId, Subscribe, Subscriber, Target,
};
use middleware::zenoh::{SessionConfig, ZenohError, ZenohTransport, wire};
use middleware::{Key, KeyFilter};
use mw_gateway::io::{ChannelIo, pair};
use mw_posix_node::config::parse_downlink;
use mw_posix_node::espnow_gw::{self, GwConfig, Report};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

const BRIDGE: Mac = [0x5c, 0xcf, 0x7f, 0x72, 0xf6, 0x54];
const DEV: Mac = [0xe4, 0xb0, 0x63, 0x81, 0xd3, 0x24];
const DEV2: Mac = [0x24, 0x6f, 0x28, 0xaa, 0xbb, 0xcc];
const WAIT: Duration = Duration::from_secs(10);

/// What the fake bridge does with the next `Send`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fate {
    /// `Sent{ok}`, and the frame reaches the device.
    Ack,
    /// `Sent{failed}`; the frame is lost.
    Nack,
    /// The frame reaches the device but no `Sent` comes back.
    LoseSent,
}

/// Shared state of the fake bridge and device.
#[derive(Default)]
struct Air {
    script: VecDeque<Fate>,
    /// Every `Send`: destination and data.
    sends: Vec<(Mac, Vec<u8>)>,
    /// Request ids the device received.
    requests: Vec<u16>,
    /// Requests the device ran (repeats are answered from its cache).
    executed: Vec<u16>,
}

struct FakeBridge {
    air: Arc<Mutex<Air>>,
    inject: mpsc::Sender<Vec<u8>>,
    task: JoinHandle<()>,
}

fn wire_of(msg: &Msg<'_>) -> Vec<u8> {
    let mut out = [0u8; MAX_WIRE];
    let n = msg.encode_wire(&mut out).unwrap();
    out[..n].to_vec()
}

/// The device's answer to an `MR` request sent to it, if any.
fn device(air: &Mutex<Air>, data: &[u8]) -> Option<Vec<u8>> {
    let Ok(Message::Request { id, to, key, .. }) = Message::decode(data) else { return None };
    if to != DEV {
        return None;
    }
    let mut a = air.lock().unwrap();
    a.requests.push(id);
    if !a.executed.contains(&id) {
        a.executed.push(id);
    }
    let result: Result<&[u8], HandlerError> = if key == keys::INFO { Ok(b"INFO") } else { Err(HandlerError::NotFound) };
    let mut frame = [0u8; MAX_FRAME];
    let n = Message::Response { id, to: BRIDGE, result }.encode(&mut frame).unwrap();
    Some(wire_of(&Msg::Recv { src: DEV, data: &frame[..n] }))
}

impl FakeBridge {
    fn start(mut io: ChannelIo) -> Self {
        let air = Arc::new(Mutex::new(Air::default()));
        let (inject, mut injected) = mpsc::channel::<Vec<u8>>(64);
        let a = air.clone();
        let task = tokio::spawn(async move {
            let mut split: Splitter<MAX_WIRE> = Splitter::new();
            let mut buf = [0u8; 256];
            // The boot banner and Hello.
            let mut out = b"\r\n ets Jan  8 2013,rst cause:2\r\n".to_vec();
            out.extend(wire_of(&Msg::Hello { mac: BRIDGE, channel: 1, version: VERSION }));
            if io.write_all(&out).await.is_err() {
                return;
            }
            loop {
                let n = tokio::select! {
                    r = io.read(&mut buf) => match r { Ok(0) | Err(_) => return, Ok(n) => n },
                    w = injected.recv() => {
                        let Some(w) = w else { return };
                        if io.write_all(&w).await.is_err() { return; }
                        continue;
                    }
                };
                let mut replies = Vec::new();
                for &b in &buf[..n] {
                    if let Split::Frame(_) = split.push(b) {
                        let mut scratch = [0u8; MAX_WIRE];
                        match radio_bridge::decode_run(split.frame(), &mut scratch).map(|(_, m)| m) {
                            Some(Msg::Query) => {
                                replies.extend(wire_of(&Msg::Hello { mac: BRIDGE, channel: 1, version: VERSION }));
                                replies.extend(wire_of(&Msg::Stats(Stats::default())));
                            }
                            Some(Msg::Send { dst, data }) => {
                                let fate = {
                                    let mut air = a.lock().unwrap();
                                    air.sends.push((dst, data.to_vec()));
                                    air.script.pop_front().unwrap_or(Fate::Ack)
                                };
                                match fate {
                                    Fate::Ack => replies.extend(wire_of(&Msg::Sent { ok: true, dst })),
                                    Fate::Nack => {
                                        replies.extend(wire_of(&Msg::Sent { ok: false, dst }));
                                        continue;
                                    }
                                    Fate::LoseSent => {}
                                }
                                if dst == DEV
                                    && let Some(answer) = device(&a, data)
                                {
                                    replies.extend(answer);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                if !replies.is_empty() && io.write_all(&replies).await.is_err() {
                    return;
                }
            }
        });
        FakeBridge { air, inject, task }
    }

    /// The bridge hears `data` from `src`.
    async fn air_recv(&self, src: Mac, data: &[u8]) {
        self.inject.send(wire_of(&Msg::Recv { src, data })).await.unwrap();
    }

    async fn publish(&self, src: Mac, seq: u8, key: &str, payload: &[u8]) {
        let mut out = [0u8; MAX_FRAME];
        let n = Frame { seq, key: Key::new(key).unwrap(), payload }.encode(&mut out).unwrap();
        self.air_recv(src, &out[..n]).await;
    }

    async fn beacon(&self, src: Mac, seq: u8, instance: &str, node: Option<SenderId>, ttl_s: u32) {
        let s = Service {
            kind: ServiceKind::from_static("mw/node"),
            instance,
            node,
            endpoint: Endpoint::Local,
            txt: &[],
            ttl_s,
        };
        let mut out = [0u8; 256];
        let n = beacon::encode(&s, &mut out).unwrap();
        let payload = out[..n].to_vec();
        self.publish(src, seq, "disc/mw/node", &payload).await;
    }

    fn script(&self, fates: &[Fate]) {
        self.air.lock().unwrap().script.extend(fates);
    }
}

/// The observer session (listening on a fresh loopback port) and the
/// gateway's session connected to it.
async fn zenoh_pair() -> (ZenohTransport, ZenohTransport) {
    let port = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port();
    let endpoint = format!("tcp/127.0.0.1:{port}");
    let observer = SessionConfig::new().listen(endpoint.clone()).build().unwrap();
    let gw = SessionConfig::new().connect(endpoint).build().unwrap();
    let observer = ZenohTransport::open(observer).await.unwrap();
    let gw = ZenohTransport::open(gw).await.unwrap();
    (observer, gw)
}

struct Running {
    observer: ZenohTransport,
    bridge: FakeBridge,
    stop: oneshot::Sender<()>,
    gw: JoinHandle<Result<Report, espnow_gw::GwError>>,
}

impl Running {
    async fn start(cfg: GwConfig) -> Self {
        let (observer, session) = zenoh_pair().await;
        let (gw_io, bridge_io) = pair();
        let bridge = FakeBridge::start(bridge_io);
        let (stop, stopped) = oneshot::channel::<()>();
        let gw = tokio::spawn(espnow_gw::run(gw_io, session, cfg, async {
            let _ = stopped.await;
        }));
        Running { observer, bridge, stop, gw }
    }

    async fn finish(self) -> Report {
        let _ = self.stop.send(());
        let r = tokio::time::timeout(WAIT, self.gw).await.expect("gateway stops").expect("join").expect("report");
        self.bridge.task.abort();
        r
    }

    /// `sys/info` of `mac` through the gateway.
    async fn info(&self, mac: Mac, timeout: Duration) -> Result<Vec<u8>, ZenohError> {
        let mut z = self.observer.clone();
        let mut resp = [0u8; 256];
        let opts = RequestOptions::default().with_target(Target::Node(SenderId::from_mac(mac))).with_timeout(timeout);
        z.request_with(keys::INFO, b"", &mut resp, &opts).await.map(<[u8]>::to_vec)
    }

    /// Repeats `info` until the per-device queryables are routed.
    async fn info_when_heard(&self, mac: Mac) -> Result<Vec<u8>, ZenohError> {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let r = self.info(mac, Duration::from_secs(5)).await;
            if !matches!(r, Err(ZenohError::NoReply)) || tokio::time::Instant::now() > deadline {
                return r;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

fn fast() -> GwConfig {
    GwConfig {
        sent_timeout_ms: 100,
        request_retry: Retry { timeout_ms: 300, attempts: 4 },
        heard_period: Duration::from_millis(200),
        ..GwConfig::default()
    }
}

/// Waits for the next sample, at most `WAIT`.
async fn next(sub: &mut impl Subscriber) -> (String, Vec<u8>, Option<SenderId>) {
    let mut buf = [0u8; 1024];
    let s = tokio::time::timeout(WAIT, sub.recv(&mut buf)).await.expect("a sample").expect("recv");
    (s.key.as_str().to_owned(), s.payload.to_vec(), s.meta.sender)
}

/// Publishes until `sub` sees something (declarations take a moment to
/// propagate), then returns that first sample.
async fn first_sample(
    r: &Running,
    sub: &mut impl Subscriber,
    mac: Mac,
    key: &str,
    payload: &[u8],
) -> (String, Vec<u8>, Option<SenderId>) {
    let mut buf = [0u8; 1024];
    for seq in 0..100u8 {
        r.bridge.publish(mac, seq, key, payload).await;
        if let Ok(Ok(s)) = tokio::time::timeout(Duration::from_millis(200), sub.recv(&mut buf)).await {
            return (s.key.as_str().to_owned(), s.payload.to_vec(), s.meta.sender);
        }
    }
    panic!("no sample on {key}");
}

#[tokio::test(flavor = "multi_thread")]
async fn eg6_publish_up() {
    let r = Running::start(fast()).await;
    let mut o = r.observer.clone();
    let mut sub = o.subscribe(KeyFilter::Prefix(Key::from_static("app"))).await.unwrap();
    let (key, payload, sender) = first_sample(&r, &mut sub, DEV, "app/thing/temp", b"21.5").await;
    assert_eq!((key.as_str(), payload.as_slice()), ("app/thing/temp", &b"21.5"[..]));
    assert_eq!(sender, Some(SenderId::from_mac(DEV)));
    // A repeat (same seq) is dropped; sys and disc never appear under app.
    r.bridge.publish(DEV, 200, "app/thing/temp", b"once").await;
    r.bridge.publish(DEV, 200, "app/thing/temp", b"once").await;
    r.bridge.publish(DEV, 201, "sys/log", b"hello").await;
    r.bridge.beacon(DEV, 202, "dev-1", None, 30).await;
    r.bridge.publish(DEV, 203, "app/thing/temp", b"last").await;
    let mut got = Vec::new();
    loop {
        let (k, p, s) = next(&mut sub).await;
        assert!(k.starts_with("app/"), "{k}");
        assert_eq!(s, Some(SenderId::from_mac(DEV)));
        if p == b"last" {
            break;
        }
        got.push(p);
    }
    assert_eq!(got.iter().filter(|p| p.as_slice() == b"once").count(), 1, "{got:?}");
    let report = r.finish().await;
    assert!(report.frames_up >= 3, "{report:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn eg7_beacon_becomes_discovery() {
    let r = Running::start(fast()).await;
    let mut d = r.observer.discovery();
    d.browse(KeyFilter::Exact(Key::from_static("mw/node"))).await.unwrap();
    // A beacon claiming another node is refused; the device's own is announced.
    r.bridge.beacon(DEV, 1, "spoof", Some(SenderId::from_mac(DEV2)), 30).await;
    r.bridge.beacon(DEV, 2, "dev-1", None, 2).await;
    let mut buf = [0u8; 1024];
    let ev = tokio::time::timeout(WAIT, d.next(&mut buf)).await.expect("up").unwrap();
    let Event::Up(s) = ev else { panic!("{ev:?}") };
    assert_eq!(s.instance, "dev-1");
    assert_eq!(s.node, Some(SenderId::from_mac(DEV)));
    // Its 2 s TTL passes: down.
    let ev = tokio::time::timeout(WAIT, d.next(&mut buf)).await.expect("down").unwrap();
    assert!(matches!(ev, Event::Down { instance: "dev-1", .. }), "{ev:?}");
    let report = r.finish().await;
    assert_eq!(report.refused, 1);
    assert_eq!(report.beacons, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn eg7_beacons_are_withdrawn_when_the_gateway_stops() {
    let r = Running::start(fast()).await;
    let mut d = r.observer.discovery();
    d.browse(KeyFilter::Exact(Key::from_static("mw/node"))).await.unwrap();
    r.bridge.beacon(DEV, 1, "dev-1", None, 60).await;
    let mut buf = [0u8; 1024];
    let ev = tokio::time::timeout(WAIT, d.next(&mut buf)).await.expect("up").unwrap();
    assert!(matches!(ev, Event::Up(_)));
    let _ = r.stop.send(());
    let _ = tokio::time::timeout(WAIT, r.gw).await;
    let ev = tokio::time::timeout(WAIT, d.next(&mut buf)).await.expect("down").unwrap();
    assert!(matches!(ev, Event::Down { instance: "dev-1", .. }), "{ev:?}");
    r.bridge.task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn eg9_request_response() {
    let r = Running::start(fast()).await;
    r.bridge.publish(DEV, 1, "app/thing/temp", b"1").await;
    assert_eq!(r.info_when_heard(DEV).await.unwrap(), b"INFO");
    // No queryable for a key off the allowlist, nor for an unheard device;
    // the bridge never sees either.
    let sends = r.bridge.air.lock().unwrap().sends.len();
    let mut z = r.observer.clone();
    let mut resp = [0u8; 64];
    let opts = RequestOptions::default()
        .with_target(Target::Node(SenderId::from_mac(DEV)))
        .with_timeout(Duration::from_millis(500));
    assert!(matches!(z.request_with(keys::TRUST_CLAIM, b"", &mut resp, &opts).await, Err(ZenohError::NoReply)));
    assert!(matches!(r.info(DEV2, Duration::from_millis(500)).await, Err(ZenohError::NoReply)));
    assert_eq!(r.bridge.air.lock().unwrap().sends.len(), sends);
    let report = r.finish().await;
    assert_eq!((report.requests, report.request_failures), (1, 0));
}

#[tokio::test(flavor = "multi_thread")]
async fn eg9_lost_sent_is_retried() {
    let r = Running::start(fast()).await;
    r.bridge.publish(DEV, 1, "app/thing/temp", b"1").await;
    assert_eq!(r.info_when_heard(DEV).await.unwrap(), b"INFO");
    {
        let mut a = r.bridge.air.lock().unwrap();
        a.requests.clear();
        a.executed.clear();
    }
    r.bridge.script(&[Fate::LoseSent, Fate::Ack]);
    assert_eq!(r.info(DEV, Duration::from_secs(5)).await.unwrap(), b"INFO");
    let a = r.bridge.air.lock().unwrap();
    assert_eq!(a.requests.len(), 2, "{:?}", a.requests);
    assert_eq!(a.requests[0], a.requests[1], "the resend carries the same id");
    assert_eq!(a.executed.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn eg9_nack_and_timeout_errors() {
    let cfg = GwConfig { request_retry: Retry { timeout_ms: 100, attempts: 2 }, ..fast() };
    let r = Running::start(cfg).await;
    r.bridge.publish(DEV, 1, "app/thing/temp", b"1").await;
    assert_eq!(r.info_when_heard(DEV).await.unwrap(), b"INFO");
    r.bridge.script(&[Fate::Nack; 8]);
    match r.info(DEV, Duration::from_secs(5)).await {
        Err(ZenohError::Handler(e)) => assert_eq!(e, HandlerError::Unavailable),
        other => panic!("{other:?}"),
    }
    let report = r.finish().await;
    assert_eq!(report.request_failures, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn eg8_downlink_broadcasts() {
    let cfg = GwConfig { downlink: parse_downlink("app/cmd/**").unwrap(), ..fast() };
    let r = Running::start(cfg).await;
    r.bridge.publish(DEV, 1, "app/x", b"heard").await;
    let mut z = r.observer.clone();
    let broadcast_of = |key: &str| {
        let a = r.bridge.air.lock().unwrap();
        a.sends.iter().any(|(dst, data)| *dst == BROADCAST && Frame::decode(data).is_ok_and(|f| f.key.as_str() == key))
    };
    let deadline = tokio::time::Instant::now() + WAIT;
    while !broadcast_of("app/cmd/led") {
        assert!(tokio::time::Instant::now() < deadline, "no broadcast");
        z.publish(Key::from_static("app/cmd/led"), b"on").await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Not on a downlink filter: not broadcast. Published under a heard
    // device's id (another gateway's uplink): not broadcast.
    z.publish(Key::from_static("app/other"), b"x").await.unwrap();
    let raw = r.observer.session().clone();
    let attachment = wire::encode_pub(SenderId::from_mac(DEV), Default::default(), 7);
    raw.put("app/cmd/loop", b"x".to_vec()).attachment(attachment.to_vec()).await.unwrap();
    // Another gateway republishing this bridge's own broadcast carries the
    // bridge's MAC-derived id: not broadcast again (it would loop between
    // the two gateways). A device behind another gateway is.
    let echo = wire::encode_pub(SenderId::from_mac(BRIDGE), Default::default(), 8);
    raw.put("app/cmd/echo", b"x".to_vec()).attachment(echo.to_vec()).await.unwrap();
    let far = wire::encode_pub(SenderId::from_mac(DEV2), Default::default(), 9);
    raw.put("app/cmd/far", b"x".to_vec()).attachment(far.to_vec()).await.unwrap();
    z.publish(Key::from_static("app/cmd/end"), b"x").await.unwrap();
    while !broadcast_of("app/cmd/end") {
        assert!(tokio::time::Instant::now() < deadline, "no end broadcast");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!broadcast_of("app/other"));
    assert!(!broadcast_of("app/cmd/loop"));
    assert!(!broadcast_of("app/cmd/echo"));
    assert!(broadcast_of("app/cmd/far"));
    let report = r.finish().await;
    assert!(report.frames_down >= 2, "{report:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn eg10_a_mac_flood_leaves_heard_devices_and_their_queryables() {
    let r = Running::start(fast()).await;
    r.bridge.publish(DEV, 1, "app/thing/temp", b"1").await;
    assert_eq!(r.info_when_heard(DEV).await.unwrap(), b"INFO");
    let mut o = r.observer.clone();
    let mut sub = o.subscribe(KeyFilter::Exact(Key::from_static("app/flood"))).await.unwrap();
    // 1000 forged source MACs, each new.
    for i in 0..1000u32 {
        let b = i.to_be_bytes();
        r.bridge.publish([0x02, 0xee, b[0], b[1], b[2], b[3]], 0, "app/flood", b"x").await;
    }
    r.bridge.publish(DEV, 2, "app/flood", b"end").await;
    let mut senders = std::collections::HashSet::new();
    loop {
        let (_, p, s) = next(&mut sub).await;
        if p == b"end" {
            break;
        }
        senders.insert(s);
    }
    // Only as many as fit beside DEV were heard (and republished).
    assert!(senders.len() < 64, "{} flood senders republished", senders.len());
    // DEV and its queryables are intact.
    assert_eq!(r.info(DEV, Duration::from_secs(5)).await.unwrap(), b"INFO");
    let report = r.finish().await;
    assert!(report.refused_macs > 0, "{report:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn eg10_heard_list() {
    let r = Running::start(fast()).await;
    let hex = String::from_utf8(wire::zid16_hex(SenderId::from_mac(BRIDGE)).to_vec()).unwrap();
    let mut o = r.observer.clone();
    let heard_key = format!("gw/espnow/{hex}/heard");
    let mut sub = o.subscribe(KeyFilter::Exact(Key::new(&heard_key).unwrap())).await.unwrap();
    r.bridge.publish(DEV, 1, "app/a", b"1").await;
    r.bridge.publish(DEV2, 1, "sys/log", b"hi").await;
    let both = [SenderId::from_mac(DEV), SenderId::from_mac(DEV2)];
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let (_, p, _) = next(&mut sub).await;
        let ids: Vec<SenderId> = peers::decode(&p).expect("a peer set").collect();
        if both.iter().all(|id| ids.contains(id)) {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "{ids:?}");
    }
    let mut resp = [0u8; peers::PEERS_MAX];
    let opts = RequestOptions::default()
        .with_target(Target::Node(SenderId::from_mac(BRIDGE)))
        .with_timeout(Duration::from_secs(2));
    let got = o.request_with(keys::PEERS, b"", &mut resp, &opts).await.unwrap();
    let ids: Vec<SenderId> = peers::decode(got).unwrap().collect();
    assert!(both.iter().all(|id| ids.contains(id)), "{ids:?}");
    r.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn eg3_uplink_survives_a_request() {
    // The lost Sent stalls the request for 1.5 s, longer than the burst.
    let r = Running::start(GwConfig { sent_timeout_ms: 1_500, ..fast() }).await;
    let mut o = r.observer.clone();
    let mut sub = o.subscribe(KeyFilter::Prefix(Key::from_static("app"))).await.unwrap();
    first_sample(&r, &mut sub, DEV, "app/warm", b"up").await;
    assert_eq!(r.info_when_heard(DEV).await.unwrap(), b"INFO");
    r.bridge.script(&[Fate::LoseSent, Fate::Ack]);
    let sends = r.bridge.air.lock().unwrap().sends.len();
    let request = async {
        let answer = r.info(DEV, Duration::from_secs(8)).await;
        (answer, tokio::time::Instant::now())
    };
    // Once the request's Send is out (and its Sent lost), the device
    // publishes more frames than the bridge radio's ring holds.
    let publishing = async {
        while r.bridge.air.lock().unwrap().sends.len() == sends {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        for i in 0..20u8 {
            r.bridge.publish(DEV, 100 + i, "app/during", &[i]).await;
        }
        let mut seen = Vec::new();
        while seen.len() < 20 {
            let (k, p, _) = next(&mut sub).await;
            if k == "app/during" {
                seen.push(p[0]);
            }
        }
        (seen, tokio::time::Instant::now())
    };
    let ((answer, answered), (seen, all_up)) = tokio::join!(request, publishing);
    assert_eq!(answer.unwrap(), b"INFO");
    assert_eq!(seen, (0..20).collect::<Vec<u8>>());
    // They were published while the request still waited, not after.
    assert!(all_up < answered, "uplink waited for the request");
    r.finish().await;
}
