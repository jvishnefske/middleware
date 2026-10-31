//! Zenoh backend integration tests (Z1–Z18) and the 0.5 conformance cases
//! (`tests/conformance.rs`) run against the Zenoh backend.
//!
//! Network conduct: every session is built with [`SessionConfig`], which
//! disables multicast scouting and gossip. Single-session tests have no
//! endpoints at all; multi-session tests listen on one explicit
//! `127.0.0.1` port (reserved by binding port 0, never probed) and the
//! other sessions connect only to it.
#![cfg(feature = "zenoh")]

mod conformance;

use std::time::Duration;

use core::future::Future;
use core::pin::pin;
use core::task::{Context, Waker};

use middleware::zenoh::{SessionConfig, ZenohError, ZenohTransport};
use middleware::{
    Handler, HandlerError, Key, KeyFilter, Publisher, Request, Requester, Responder, Subscribe,
    Subscriber,
};

const TIMEOUT: Duration = Duration::from_secs(10);

/// A session with no endpoints. It can only talk to itself, so local
/// delivery is switched on.
async fn isolated() -> ZenohTransport {
    let config = SessionConfig::new().build().expect("config");
    ZenohTransport::open(config)
        .await
        .expect("open")
        .with_timeout(Duration::from_secs(2))
        .with_local_delivery(true)
}

/// Polls `fut` exactly once with a no-op waker, then drops it.
fn poll_once_and_drop<F: Future>(fut: F) -> bool {
    let fut = pin!(fut);
    fut.poll(&mut Context::from_waker(Waker::noop())).is_ready()
}

/// Two sessions joined over one explicit loopback endpoint; default
/// (remote-only) delivery.
async fn loopback_pair() -> (ZenohTransport, ZenohTransport) {
    let endpoint = format!("tcp/127.0.0.1:{}", free_loopback_port().await);
    let a = SessionConfig::new()
        .listen(endpoint.clone())
        .build()
        .expect("config");
    let b = SessionConfig::new()
        .connect(endpoint)
        .build()
        .expect("config");
    let a = ZenohTransport::open(a).await.expect("open a");
    let b = ZenohTransport::open(b).await.expect("open b");
    (a, b)
}

/// Echoes the request reversed; empty requests are `InvalidRequest`.
fn reverse(r: &Request<'_>, resp: &mut [u8]) -> Result<usize, HandlerError> {
    let req = r.body;
    if req.is_empty() {
        return Err(HandlerError::InvalidRequest);
    }
    let out = resp
        .get_mut(..req.len())
        .ok_or(HandlerError::BufferTooSmall)?;
    out.copy_from_slice(req);
    out.reverse();
    Ok(req.len())
}

/// Spawns a task that serves exactly one query with `handler`.
fn serve_one_in_background<H: Handler + Send + 'static>(
    mut responder: middleware::zenoh::ZenohResponder<'static>,
    mut handler: H,
) -> tokio::task::JoinHandle<Result<(), ZenohError>> {
    tokio::spawn(async move {
        let mut scratch = [0u8; 64];
        responder.serve_one(&mut handler, &mut scratch).await
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn z1_publish_subscribe_round_trip() {
    const K: Key<'static> = Key::from_static("test/z1/temperature");
    let mut t = isolated().await;
    let mut sub = t.subscriber(K).await.expect("subscriber");
    t.publish(K, b"21.5").await.expect("publish");
    let mut buf = [0u8; 64];
    let sample = tokio::time::timeout(TIMEOUT, sub.recv(&mut buf))
        .await
        .expect("timeout");
    let sample = sample.expect("recv");
    assert_eq!(sample.key, K);
    assert_eq!(sample.payload, b"21.5");
}

#[tokio::test(flavor = "multi_thread")]
async fn z1_subscriber_buffer_too_small_is_error() {
    const K: Key<'static> = Key::from_static("test/z1/small");
    let mut t = isolated().await;
    let mut sub = t.subscriber(K).await.expect("subscriber");
    t.publish(K, b"payload").await.expect("publish");
    let mut buf = [0u8; 8];
    let r = tokio::time::timeout(TIMEOUT, sub.recv(&mut buf))
        .await
        .expect("timeout");
    assert!(
        matches!(
            r,
            Err(ZenohError::BufferTooSmall {
                needed: 20,
                capacity: 8
            })
        ),
        "{r:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn z2_request_runs_handler_in_responder() {
    const K: Key<'static> = Key::from_static("test/z2/reverse");
    let mut t = isolated().await;
    let responder = t.responder(K).await.expect("responder");
    let server = serve_one_in_background(responder, reverse);
    let mut resp = [0u8; 16];
    let out = t.request(K, b"abc", &mut resp).await.expect("request");
    assert_eq!(out, b"cba");
    server.await.expect("join").expect("serve_one");
}

#[tokio::test(flavor = "multi_thread")]
async fn z2_response_buffer_too_small_is_error() {
    const K: Key<'static> = Key::from_static("test/z2/small");
    let mut t = isolated().await;
    let responder = t.responder(K).await.expect("responder");
    let server = serve_one_in_background(responder, reverse);
    let mut resp = [0u8; 2];
    let r = t.request(K, b"abcd", &mut resp).await;
    assert!(
        matches!(
            r,
            Err(ZenohError::BufferTooSmall {
                needed: 4,
                capacity: 2
            })
        ),
        "{r:?}"
    );
    server.await.expect("join").expect("serve_one");
}

#[tokio::test(flavor = "multi_thread")]
async fn z3_handler_error_surfaces_at_requester() {
    const K: Key<'static> = Key::from_static("test/z3/error");
    let mut t = isolated().await;
    let responder = t.responder(K).await.expect("responder");
    let server = serve_one_in_background(responder, reverse);
    let mut resp = [0u8; 16];
    let r = t.request(K, b"", &mut resp).await;
    assert!(
        matches!(r, Err(ZenohError::Handler(HandlerError::InvalidRequest))),
        "{r:?}"
    );
    server.await.expect("join").expect("serve_one");
}

#[tokio::test(flavor = "multi_thread")]
async fn z3_oversized_request_is_buffer_too_small_reply() {
    const K: Key<'static> = Key::from_static("test/z3/oversized");
    let mut t = isolated().await;
    let responder = t.responder(K).await.expect("responder");
    let server = serve_one_in_background(responder, reverse);
    let mut resp = [0u8; 16];
    let r = t.request(K, &[1u8; 100], &mut resp).await;
    assert!(
        matches!(r, Err(ZenohError::Handler(HandlerError::BufferTooSmall))),
        "{r:?}"
    );
    server.await.expect("join").expect("serve_one");
}

#[tokio::test(flavor = "multi_thread")]
async fn z2_request_without_responder_is_no_reply() {
    const K: Key<'static> = Key::from_static("test/z2/nobody");
    let mut t = isolated().await.with_timeout(Duration::from_millis(200));
    let mut resp = [0u8; 16];
    let r = t.request(K, b"x", &mut resp).await;
    assert!(matches!(r, Err(ZenohError::NoReply)), "{r:?}");
}

/// Reserves a free loopback port by binding port 0 (no probing).
async fn free_loopback_port() -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    l.local_addr().expect("addr").port()
}

#[tokio::test(flavor = "multi_thread")]
async fn z4_two_sessions_over_explicit_loopback_endpoint() {
    const TOPIC: Key<'static> = Key::from_static("test/z4/topic");
    const RPC: Key<'static> = Key::from_static("test/z4/rpc");
    let endpoint = format!("tcp/127.0.0.1:{}", free_loopback_port().await);

    let server_cfg = SessionConfig::new()
        .listen(endpoint.clone())
        .build()
        .expect("config");
    let client_cfg = SessionConfig::new()
        .connect(endpoint)
        .build()
        .expect("config");
    let server = ZenohTransport::open(server_cfg).await.expect("open server");
    let mut client = ZenohTransport::open(client_cfg)
        .await
        .expect("open client")
        .with_timeout(Duration::from_millis(500));

    // Pub/sub across sessions. Declarations propagate asynchronously, so
    // publish until the first sample arrives (bounded by TIMEOUT).
    let mut sub = server.subscriber(TOPIC).await.expect("subscriber");
    let mut buf = [0u8; 64];
    let sample = tokio::time::timeout(TIMEOUT, async {
        loop {
            client.publish(TOPIC, b"hello").await.expect("publish");
            let wait = tokio::time::timeout(Duration::from_millis(200), sub.recv(&mut buf));
            if let Ok(r) = wait.await {
                break r.map(|s| (s.key.as_str().to_owned(), s.payload.to_vec()));
            }
        }
    })
    .await
    .expect("timeout")
    .expect("recv");
    assert_eq!(sample.0, TOPIC.as_str());
    assert_eq!(sample.1, b"hello");

    // Request/response across sessions, served in a loop until done.
    let mut responder = server.responder(RPC).await.expect("responder");
    let serving = tokio::spawn(async move {
        let mut scratch = [0u8; 64];
        let mut handler = reverse;
        loop {
            if responder
                .serve_one(&mut handler, &mut scratch)
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let mut resp = [0u8; 16];
    let out = tokio::time::timeout(TIMEOUT, async {
        loop {
            match client.request(RPC, b"xyz", &mut resp).await {
                Err(ZenohError::NoReply) => continue,
                other => break other.map(<[u8]>::to_vec),
            }
        }
    })
    .await
    .expect("timeout")
    .expect("request");
    assert_eq!(out, b"zyx");
    serving.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn z5_try_recv_is_none_when_empty_and_some_without_waiting() {
    const K: Key<'static> = Key::from_static("test/z5/try");
    let mut t = isolated().await;
    let mut sub = t.subscribe(KeyFilter::Exact(K)).await.expect("subscribe");
    let mut buf = [0u8; 64];
    assert!(matches!(sub.try_recv(&mut buf), Ok(None)));
    t.publish(K, b"now").await.expect("publish");
    // Local delivery is synchronous; the bounded retry only guards against a
    // scheduler hiccup. `try_recv` itself never waits.
    let mut got = None;
    for _ in 0..100 {
        if let Some(s) = sub.try_recv(&mut buf).expect("try_recv") {
            got = Some((s.key.as_str().to_owned(), s.payload.to_vec(), s.meta));
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (key, payload, meta) = got.expect("sample");
    assert_eq!(
        (key.as_str(), payload.as_slice()),
        (K.as_str(), &b"now"[..])
    );
    assert_eq!(meta.sender, Some(t.sender_id()));
    assert_eq!(meta.seq, Some(0));
    assert!(matches!(sub.try_recv(&mut buf), Ok(None)));
}

#[tokio::test(flavor = "multi_thread")]
async fn z6_prefix_subscription_delivers_app_x_but_not_apple() {
    let mut t = isolated().await;
    let mut sub = t
        .subscribe(KeyFilter::Prefix(Key::from_static("app")))
        .await
        .expect("subscribe");
    for k in ["apple", "app/x", "app", "app/x/y"] {
        t.publish(Key::new(k).unwrap(), k.as_bytes())
            .await
            .expect("publish");
    }
    let mut buf = [0u8; 64];
    let mut keys = Vec::new();
    for _ in 0..3 {
        let s = tokio::time::timeout(TIMEOUT, sub.recv(&mut buf))
            .await
            .expect("timeout")
            .expect("recv");
        assert_eq!(s.key.as_str().as_bytes(), s.payload);
        keys.push(s.key.as_str().to_owned());
    }
    assert_eq!(keys, ["app/x", "app", "app/x/y"]);
    assert!(
        matches!(sub.try_recv(&mut buf), Ok(None)),
        "apple not delivered"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn z7_no_self_delivery_and_sender_meta_present() {
    let (mut a, mut b) = loopback_pair().await;
    assert_ne!(a.sender_id(), b.sender_id());
    let filter = KeyFilter::Prefix(Key::from_static("test/z7"));
    let mut sub = a.subscribe(filter).await.expect("subscribe");
    let mut buf = [0u8; 64];

    // A's own publication is not delivered to A (local delivery would be
    // synchronous, so it would already be queued).
    a.publish(Key::from_static("test/z7/own"), b"self")
        .await
        .expect("publish");
    assert!(matches!(sub.try_recv(&mut buf), Ok(None)));

    // B's publications are, once the declaration has propagated.
    let got = tokio::time::timeout(TIMEOUT, async {
        loop {
            b.publish(Key::from_static("test/z7/remote"), b"peer")
                .await
                .expect("publish");
            let wait = tokio::time::timeout(Duration::from_millis(200), sub.recv(&mut buf));
            if let Ok(r) = wait.await {
                let s = r.expect("recv");
                break (s.key.as_str().to_owned(), s.payload.to_vec(), s.meta);
            }
        }
    })
    .await
    .expect("timeout");
    assert_eq!(got.0, "test/z7/remote");
    assert_eq!(got.1, b"peer");
    assert_eq!(got.2.sender, Some(b.sender_id()));
    assert!(got.2.seq.is_some());

    // Whatever else is queued also came from B, never from A.
    a.publish(Key::from_static("test/z7/own"), b"self")
        .await
        .expect("publish");
    while let Some(s) = sub.try_recv(&mut buf).expect("try_recv") {
        assert_eq!(s.meta.sender, Some(b.sender_id()));
        assert_eq!(s.payload, b"peer");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn z8_cancelled_recv_loses_no_sample() {
    const K: Key<'static> = Key::from_static("test/z8/cancel");
    let mut t = isolated().await;
    let mut sub = t.subscriber(K).await.expect("subscriber");
    let mut buf = [0u8; 64];
    assert!(
        !poll_once_and_drop(sub.recv(&mut buf)),
        "nothing queued yet"
    );
    t.publish(K, b"kept").await.expect("publish");
    let s = tokio::time::timeout(TIMEOUT, sub.recv(&mut buf))
        .await
        .expect("timeout")
        .expect("recv");
    assert_eq!(s.payload, b"kept");
}

// ---------------------------------------------------------------------------
// 0.5: conformance fixtures over loopback sessions, and Zenoh specifics.
// ---------------------------------------------------------------------------

use conformance::{
    DiscoveryFixture, GatherFixture, PubSubFixture, ReqRepFixture, discovery, pubsub, reqrep,
};
use middleware::discovery::{Announce, Browse, Endpoint, Event, Service, ServiceKind};
use middleware::zenoh::{ZenohDiscovery, ZenohResponder, ZenohSubscriber, node_key};
use middleware::{Flow, Gather, RequestOptions, SenderId, Target};

/// Queue depth of the conformance subscriber.
const DEPTH: usize = 4;

/// A hub session listening on a fresh loopback port, and its endpoint.
async fn hub() -> (ZenohTransport, String) {
    let endpoint = format!("tcp/127.0.0.1:{}", free_loopback_port().await);
    let cfg = SessionConfig::new()
        .listen(endpoint.clone())
        .build()
        .expect("config");
    let t = ZenohTransport::open(cfg).await.expect("open hub");
    (t, endpoint)
}

/// A session connected to the hub at `endpoint` only.
async fn spoke(endpoint: &str) -> ZenohTransport {
    let cfg = SessionConfig::new()
        .connect(endpoint)
        .build()
        .expect("config");
    ZenohTransport::open(cfg).await.expect("open spoke")
}

/// Polls `matched` every 10 ms until it is true, failing after `TIMEOUT`.
async fn until(what: &str, mut matched: impl AsyncFnMut() -> bool) {
    let r = tokio::time::timeout(TIMEOUT, async {
        while !matched().await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(r.is_ok(), "{what}: not visible within {TIMEOUT:?}");
}

/// Waits until `from` sees a queryable on `expr` (no query is sent).
async fn queryable_visible(from: &ZenohTransport, expr: &str) {
    let querier = from
        .session()
        .declare_querier(expr.to_owned())
        .await
        .expect("querier");
    until(expr, async || {
        querier
            .matching_status()
            .await
            .expect("matching")
            .matching()
    })
    .await;
}

/// Waits until `from` sees a subscriber on `key` (no sample is sent).
async fn subscriber_visible(from: &ZenohTransport, key: Key<'_>) {
    let publisher = from
        .session()
        .declare_publisher(key.as_str().to_owned())
        .await
        .expect("publisher");
    until(key.as_str(), async || {
        publisher
            .matching_status()
            .await
            .expect("matching")
            .matching()
    })
    .await;
}

/// The Zenoh fixture: sessions on loopback, kept alive for the case.
#[derive(Default)]
struct Zenoh {
    keep: Vec<ZenohTransport>,
    client: Option<SenderId>,
}

impl ReqRepFixture for Zenoh {
    type Requester = ZenohTransport;
    type Responder = ZenohResponder<'static>;
    const PROPAGATES_DEADLINE: bool = true;

    async fn connect(&mut self, key: Key<'static>) -> (ZenohTransport, ZenohResponder<'static>) {
        let (server, endpoint) = hub().await;
        let client = spoke(&endpoint).await.with_timeout(Duration::from_secs(2));
        let responder = server.responder(key).await.expect("responder");
        queryable_visible(&client, &node_key(server.sender_id(), key)).await;
        self.client = Some(client.sender_id());
        self.keep.push(server);
        (client, responder)
    }

    fn handler_error(e: &ZenohError) -> Option<HandlerError> {
        match e {
            ZenohError::Handler(h) => Some(*h),
            _ => None,
        }
    }

    fn is_timeout(e: &ZenohError) -> bool {
        matches!(e, ZenohError::NoReply)
    }

    fn expected_sender(&self) -> Option<SenderId> {
        self.client
    }

    fn expected_principal(&self) -> Option<&str> {
        // Zenoh 1.x authenticates no caller name this backend can see.
        None
    }
}

impl GatherFixture for Zenoh {
    type Gatherer = ZenohTransport;
    type Responder = ZenohResponder<'static>;

    async fn connect(
        &mut self,
        key: Key<'static>,
        n: usize,
    ) -> (ZenohTransport, Vec<ZenohResponder<'static>>) {
        let (gatherer, endpoint) = hub().await;
        let mut responders = Vec::new();
        for _ in 0..n {
            let node = spoke(&endpoint).await;
            let r = node.responder(key).await.expect("responder");
            queryable_visible(&gatherer, &node_key(node.sender_id(), key)).await;
            responders.push(r);
            self.keep.push(node);
        }
        (gatherer, responders)
    }
}

impl PubSubFixture for Zenoh {
    type Publisher = ZenohTransport;
    type Subscriber = ZenohSubscriber;
    const REPORTS_GAPS: bool = true;

    async fn connect(&mut self, filter: KeyFilter<'static>) -> (ZenohTransport, ZenohSubscriber) {
        let (receiver, endpoint) = hub().await;
        let receiver = receiver.with_queue_depth(DEPTH);
        let publisher = spoke(&endpoint).await;
        let mut receiver_handle = receiver.clone();
        let sub = receiver_handle.subscribe(filter).await.expect("subscribe");
        let key = match filter {
            KeyFilter::Exact(k) | KeyFilter::Prefix(k) => k,
        };
        subscriber_visible(&publisher, key).await;
        self.keep.push(receiver);
        (publisher, sub)
    }

    fn queue_depth(&self) -> usize {
        DEPTH
    }

    async fn settle(&mut self) {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

impl DiscoveryFixture for Zenoh {
    type Announcer = ZenohDiscovery;
    type Browser = ZenohDiscovery;

    async fn connect(&mut self) -> (ZenohDiscovery, ZenohDiscovery) {
        let (browser, endpoint) = hub().await;
        let announcer = spoke(&endpoint).await;
        let pair = (announcer.discovery(), browser.discovery());
        self.keep.push(browser);
        self.keep.push(announcer);
        pair
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn k_round_trip() {
    reqrep::round_trip(&mut Zenoh::default()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn k_timeout_is_honoured() {
    reqrep::timeout_is_honoured(&mut Zenoh::default()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn k_handler_errors_round_trip() {
    reqrep::handler_errors_round_trip(&mut Zenoh::default()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn k_handler_sees_caller_identity() {
    reqrep::handler_sees_caller_identity(&mut Zenoh::default()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn k_gather_collects_every_responder() {
    reqrep::gather_collects_every_responder(&mut Zenoh::default(), 3).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn k_overload_matches_declared_delivery() {
    pubsub::overload_matches_declared_delivery(&mut Zenoh::default()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn k_gap_is_reported() {
    pubsub::gap_is_reported(&mut Zenoh::default()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn l_announce_then_withdraw() {
    discovery::announce_then_withdraw(&mut Zenoh::default()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn l_goodbye_removes_record_at_once() {
    discovery::goodbye_removes_record_at_once(&mut Zenoh::default()).await;
}

#[test]
fn l_directory_expiry() {
    discovery::directory_expiry();
}

/// Answers with `name`, recording the caller.
fn named(
    name: &'static [u8],
) -> impl FnMut(&Request<'_>, &mut [u8]) -> Result<usize, HandlerError> {
    move |_r, out| {
        let out = out
            .get_mut(..name.len())
            .ok_or(HandlerError::BufferTooSmall)?;
        out.copy_from_slice(name);
        Ok(name.len())
    }
}

/// Serves `responder` with `handler` until the task is aborted.
fn serve_forever<H: Handler + Send + 'static>(
    mut responder: ZenohResponder<'static>,
    mut handler: H,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut scratch = [0u8; 64];
        while responder
            .serve_one(&mut handler, &mut scratch)
            .await
            .is_ok()
        {}
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn z9_request_with_timeout_overrides_the_default() {
    const K: Key<'static> = Key::from_static("test/z9/idle");
    let (server, endpoint) = hub().await;
    // A default far longer than the per-call timeout.
    let mut client = spoke(&endpoint).await.with_timeout(Duration::from_secs(30));
    let _idle = server.responder(K).await.expect("responder");
    queryable_visible(&client, K.as_str()).await;
    let start = std::time::Instant::now();
    let mut out = [0u8; 8];
    let opts = RequestOptions::DEFAULT.with_timeout(Duration::from_millis(300));
    let r = client.request_with(K, b"x", &mut out, &opts).await;
    assert!(matches!(r, Err(ZenohError::NoReply)), "{r:?}");
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(295) && elapsed < Duration::from_secs(2),
        "{elapsed:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn z10_target_node_reaches_only_that_node() {
    const K: Key<'static> = Key::from_static("test/z10/who");
    let (mut client, endpoint) = hub().await;
    let one = spoke(&endpoint).await;
    let two = spoke(&endpoint).await;
    let r1 = one.responder(K).await.expect("responder");
    let r2 = two.responder(K).await.expect("responder");
    assert_eq!(r1.sender_id(), one.sender_id());
    queryable_visible(&client, &node_key(one.sender_id(), K)).await;
    queryable_visible(&client, &node_key(two.sender_id(), K)).await;
    let s1 = serve_forever(r1, named(b"one"));
    let s2 = serve_forever(r2, named(b"two"));
    let opts = RequestOptions::DEFAULT.with_timeout(Duration::from_secs(3));
    for _ in 0..3 {
        for (id, name) in [
            (one.sender_id(), &b"one"[..]),
            (two.sender_id(), &b"two"[..]),
        ] {
            let mut out = [0u8; 8];
            let got = client
                .request_with(K, b"?", &mut out, &opts.with_target(Target::Node(id)))
                .await
                .expect("request");
            assert_eq!(got, name);
        }
    }
    // A node nobody runs: no queryable matches, so Zenoh finalises the
    // query at once instead of waiting for the 3 s timeout.
    let start = std::time::Instant::now();
    let mut out = [0u8; 8];
    let r = client
        .request_with(
            K,
            b"?",
            &mut out,
            &opts.with_target(Target::Node(SenderId([9; 8]))),
        )
        .await;
    assert!(matches!(r, Err(ZenohError::NoReply)), "{r:?}");
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "{:?}",
        start.elapsed()
    );
    s1.abort();
    s2.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn z11_gather_names_repliers_and_carries_their_errors() {
    const K: Key<'static> = Key::from_static("test/z11/all");
    let (mut gatherer, endpoint) = hub().await;
    let ok = spoke(&endpoint).await;
    let busy = spoke(&endpoint).await;
    let r_ok = ok.responder(K).await.expect("responder");
    let r_busy = busy.responder(K).await.expect("responder");
    queryable_visible(&gatherer, &node_key(ok.sender_id(), K)).await;
    queryable_visible(&gatherer, &node_key(busy.sender_id(), K)).await;
    let s1 = serve_forever(r_ok, named(b"ok"));
    let s2 = serve_forever(r_busy, |_r: &Request<'_>, _o: &mut [u8]| {
        Err(HandlerError::Unavailable)
    });
    let opts = RequestOptions::DEFAULT.with_timeout(Duration::from_secs(3));
    let mut buf = [0u8; 8];
    let mut seen = Vec::new();
    let start = std::time::Instant::now();
    let n = gatherer
        .request_each(K, b"?", &mut buf, &opts, 10, |reply| {
            seen.push((reply.sender, reply.result.map(<[u8]>::to_vec)));
            Flow::Continue
        })
        .await
        .expect("gather");
    // Both answered, so Zenoh finalised the query well before the timeout.
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(n, 2);
    seen.sort_by_key(|(s, _)| s.map(|s| s.0));
    let mut want = vec![
        (Some(ok.sender_id()), Ok(b"ok".to_vec())),
        (Some(busy.sender_id()), Err(HandlerError::Unavailable)),
    ];
    want.sort_by_key(|(s, _)| s.map(|s| s.0));
    assert_eq!(seen, want);
    // max_replies bounds the gather.
    let n = gatherer
        .request_each(K, b"?", &mut buf, &opts, 1, |_| Flow::Continue)
        .await
        .expect("gather");
    assert_eq!(n, 1);
    s1.abort();
    s2.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn z14_gaps_are_per_key() {
    const K1: Key<'static> = Key::from_static("test/z12/one");
    const K2: Key<'static> = Key::from_static("test/z12/two");
    let (receiver, endpoint) = hub().await;
    let mut publisher = spoke(&endpoint).await;
    let mut sub = receiver.subscriber(K1).await.expect("subscriber");
    subscriber_visible(&publisher, K1).await;
    // Interleaved publications on a key the subscriber does not see must
    // not look like losses on the one it does.
    for k in [K1, K2, K2, K1, K2, K1] {
        publisher.publish(k, b"x").await.expect("publish");
    }
    let mut buf = [0u8; 64];
    let mut got = Vec::new();
    for _ in 0..3 {
        let s = tokio::time::timeout(TIMEOUT, sub.recv(&mut buf))
            .await
            .expect("timeout")
            .expect("recv");
        got.push((s.meta.seq, s.meta.gap));
    }
    assert_eq!(
        got,
        [(Some(0), None), (Some(1), Some(0)), (Some(2), Some(0))]
    );
    // A clone is a new stream: its first sample has an unknown gap, and
    // does not disturb the original's numbering.
    let mut clone = publisher.clone();
    clone.publish(K1, b"x").await.expect("publish");
    publisher.publish(K1, b"x").await.expect("publish");
    let mut got = Vec::new();
    for _ in 0..2 {
        let s = tokio::time::timeout(TIMEOUT, sub.recv(&mut buf))
            .await
            .expect("timeout")
            .expect("recv");
        got.push((s.meta.seq, s.meta.gap));
    }
    got.sort();
    assert_eq!(got, [(Some(0), None), (Some(3), Some(0))]);
}

const CAM: ServiceKind<'static> = ServiceKind::from_static("test/cam");

fn cam(instance: &'static str) -> Service<'static> {
    Service {
        kind: CAM,
        instance,
        node: None,
        endpoint: Endpoint::Locator("tcp/127.0.0.1:7447"),
        txt: b"\x05ver=5",
        ttl_s: 60,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn z15_browse_sees_services_announced_before_it_with_the_announcer_as_node() {
    let (b, endpoint) = hub().await;
    let a = spoke(&endpoint).await;
    let mut announcer = a.discovery();
    let mut browser = b.discovery();
    announcer.announce(&cam("early")).await.expect("announce");
    assert_eq!(
        announcer.announced().collect::<Vec<_>>(),
        ["@mw/svc/test/cam/early"]
    );
    // The token is visible to the browser's session before browsing starts.
    until("token", async || {
        let replies = b
            .session()
            .liveliness()
            .get("@mw/svc/test/cam/early")
            .await
            .expect("liveliness get");
        replies.recv_async().await.is_ok()
    })
    .await;
    browser
        .browse(KeyFilter::Prefix(Key::from_static("test")))
        .await
        .expect("browse");
    let mut buf = [0u8; 256];
    let e = tokio::time::timeout(TIMEOUT, browser.next(&mut buf))
        .await
        .expect("timeout")
        .expect("next");
    let Event::Up(s) = e else { panic!("{e:?}") };
    assert_eq!((s.kind, s.instance, s.ttl_s), (CAM, "early", 60));
    assert_eq!(s.node, Some(a.sender_id()));
    // Instances must be one plain key chunk.
    let r = announcer.announce(&cam("a/b")).await;
    assert!(matches!(r, Err(ZenohError::InvalidInstance)), "{r:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn z15_closing_the_announcer_session_is_down_and_try_next_does_not_wait() {
    let (b, endpoint) = hub().await;
    let a = spoke(&endpoint).await;
    let mut announcer = a.discovery();
    let mut browser = b.discovery();
    let mut buf = [0u8; 256];
    assert!(matches!(
        browser.try_next(&mut buf),
        Err(ZenohError::NotBrowsing)
    ));
    browser
        .browse(KeyFilter::Exact(CAM.key()))
        .await
        .expect("browse");
    assert!(matches!(browser.try_next(&mut buf), Ok(None)));
    announcer.announce(&cam("unit")).await.expect("announce");
    // try_next starts the record fetch and reports Up once it arrived.
    let mut up = None;
    for _ in 0..500 {
        if let Some(e) = browser.try_next(&mut buf).expect("try_next") {
            up = Some(matches!(e, Event::Up(s) if s.instance == "unit"));
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(up, Some(true));
    // No goodbye is sent: the session goes away and Zenoh drops its token.
    drop(announcer);
    a.session().close().await.expect("close");
    let e = tokio::time::timeout(TIMEOUT, browser.next(&mut buf))
        .await
        .expect("timeout")
        .expect("next");
    assert_eq!(
        e,
        Event::Down {
            kind: CAM,
            instance: "unit"
        }
    );
}

/// Declares, on `t`'s session, a raw queryable on `expr` answering every
/// query with `payload` and `attachment`: a peer squatting on a key.
async fn squat(
    t: &ZenohTransport,
    expr: String,
    payload: Vec<u8>,
    attachment: Option<Vec<u8>>,
) -> zenoh::query::Queryable<()> {
    use zenoh::Wait;
    t.session()
        .declare_queryable(expr)
        .callback(move |q| {
            let reply = q.reply(q.key_expr().clone(), payload.clone());
            let _ = match &attachment {
                Some(a) => reply.attachment(a.clone()).wait(),
                None => reply.wait(),
            };
        })
        .await
        .expect("squatting queryable")
}

#[tokio::test(flavor = "multi_thread")]
async fn z17_target_node_drops_replies_not_naming_the_node() {
    const K: Key<'static> = Key::from_static("test/z17/who");
    let (mut client, endpoint) = hub().await;
    let victim = spoke(&endpoint).await;
    let forger = spoke(&endpoint).await;
    let expr = node_key(victim.sender_id(), K);
    // One squatter names itself, one names nobody; neither is the victim.
    let _q1 = squat(
        &forger,
        expr.clone(),
        b"forged".to_vec(),
        Some(middleware::zenoh::wire::encode_reply(forger.sender_id()).to_vec()),
    )
    .await;
    let _q2 = squat(&forger, expr.clone(), b"anon".to_vec(), None).await;
    queryable_visible(&client, &expr).await;
    let opts = RequestOptions::DEFAULT
        .with_timeout(Duration::from_millis(500))
        .with_target(Target::Node(victim.sender_id()));
    let mut out = [0u8; 16];
    let r = client.request_with(K, b"?", &mut out, &opts).await;
    assert!(matches!(r, Err(ZenohError::NoReply)), "{r:?}");
    let mut buf = [0u8; 16];
    let n = client
        .request_each(K, b"?", &mut buf, &opts, 10, |_| Flow::Continue)
        .await
        .expect("gather");
    assert_eq!(n, 0, "squatters' replies reached the gather");
    // The victim's own responder is still heard next to the squatters.
    let real = victim.responder(K).await.expect("responder");
    queryable_visible(&client, K.as_str()).await;
    let serving = serve_forever(real, named(b"real"));
    let got = client
        .request_with(K, b"?", &mut out, &opts)
        .await
        .expect("request");
    assert_eq!(got, b"real");
    serving.abort();
}

/// A record of `CAM` for `instance` with `txt`, encoded.
fn cam_record(instance: &'static str, txt: &'static [u8]) -> Vec<u8> {
    let mut out = [0u8; middleware::discovery::beacon::MAX_BEACON_LEN];
    let s = Service {
        txt,
        ..cam(instance)
    };
    let n = middleware::discovery::beacon::encode(&s, &mut out).expect("encode");
    out[..n].to_vec()
}

#[tokio::test(flavor = "multi_thread")]
async fn z18_conflicting_service_records_are_refused() {
    let (b, endpoint) = hub().await;
    let a = spoke(&endpoint).await;
    let forger = spoke(&endpoint).await;
    let key = "@mw/svc/test/cam/unit".to_owned();
    // A forged record that decodes for this very key, but says otherwise.
    let _forged = squat(
        &forger,
        key.clone(),
        cam_record("unit", b"\x06evil=1"),
        None,
    )
    .await;
    queryable_visible(&b, &key).await;
    let mut browser = b.discovery();
    browser
        .browse(KeyFilter::Exact(CAM.key()))
        .await
        .expect("browse");
    let mut announcer = a.discovery();
    announcer.announce(&cam("unit")).await.expect("announce");
    let mut buf = [0u8; 256];
    let r = tokio::time::timeout(TIMEOUT, browser.next(&mut buf))
        .await
        .expect("timeout");
    assert!(matches!(r, Err(ZenohError::ConflictingRecords)), "{r:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn z18_records_for_another_service_are_ignored() {
    let (b, endpoint) = hub().await;
    let a = spoke(&endpoint).await;
    let forger = spoke(&endpoint).await;
    let key = "@mw/svc/test/cam/unit".to_owned();
    // Answers on the key, but with another instance's record and junk:
    // neither is a candidate, so the genuine record stands alone.
    let _other = squat(&forger, key.clone(), cam_record("other", b""), None).await;
    let _junk = squat(&forger, key.clone(), b"junk".to_vec(), None).await;
    queryable_visible(&b, &key).await;
    let mut browser = b.discovery();
    browser
        .browse(KeyFilter::Exact(CAM.key()))
        .await
        .expect("browse");
    let mut announcer = a.discovery();
    announcer.announce(&cam("unit")).await.expect("announce");
    let mut buf = [0u8; 256];
    let e = tokio::time::timeout(TIMEOUT, browser.next(&mut buf))
        .await
        .expect("timeout")
        .expect("next");
    let Event::Up(s) = e else { panic!("{e:?}") };
    assert_eq!((s.instance, s.txt), ("unit", &b"\x05ver=5"[..]));
}
