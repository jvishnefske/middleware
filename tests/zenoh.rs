//! Zenoh backend integration tests (Z1–Z8).
//!
//! Network conduct: every session is built with [`SessionConfig`], which
//! disables multicast scouting and gossip. Single-session tests have no
//! endpoints at all; the two-session test listens and connects only on an
//! explicit `127.0.0.1` port.
#![cfg(feature = "zenoh")]

use std::time::Duration;

use core::future::Future;
use core::pin::pin;
use core::task::{Context, Waker};

use middleware::zenoh::{SessionConfig, ZenohError, ZenohTransport};
use middleware::{
    Handler, HandlerError, Key, KeyFilter, Publisher, Requester, Responder, Subscribe, Subscriber,
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
    let a = SessionConfig::new().listen(endpoint.clone()).build().expect("config");
    let b = SessionConfig::new().connect(endpoint).build().expect("config");
    let a = ZenohTransport::open(a).await.expect("open a");
    let b = ZenohTransport::open(b).await.expect("open b");
    (a, b)
}

/// Echoes the request reversed; empty requests are `InvalidRequest`.
fn reverse(_key: Key<'_>, req: &[u8], resp: &mut [u8]) -> Result<usize, HandlerError> {
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
    assert_eq!((key.as_str(), payload.as_slice()), (K.as_str(), &b"now"[..]));
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
        t.publish(Key::new(k).unwrap(), k.as_bytes()).await.expect("publish");
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
    assert!(matches!(sub.try_recv(&mut buf), Ok(None)), "apple not delivered");
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
    a.publish(Key::from_static("test/z7/own"), b"self").await.expect("publish");
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
    a.publish(Key::from_static("test/z7/own"), b"self").await.expect("publish");
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
    assert!(!poll_once_and_drop(sub.recv(&mut buf)), "nothing queued yet");
    t.publish(K, b"kept").await.expect("publish");
    let s = tokio::time::timeout(TIMEOUT, sub.recv(&mut buf))
        .await
        .expect("timeout")
        .expect("recv");
    assert_eq!(s.payload, b"kept");
}
