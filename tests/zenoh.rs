//! Zenoh backend integration tests (Z1–Z4).
//!
//! Network conduct: every session is built with [`SessionConfig`], which
//! disables multicast scouting and gossip. Single-session tests have no
//! endpoints at all; the two-session test listens and connects only on an
//! explicit `127.0.0.1` port.
#![cfg(feature = "zenoh")]

use std::time::Duration;

use middleware::zenoh::{SessionConfig, ZenohError, ZenohTransport};
use middleware::{Handler, HandlerError, Key, Publisher, Requester, Responder, Subscriber};

const TIMEOUT: Duration = Duration::from_secs(10);

async fn isolated() -> ZenohTransport {
    let config = SessionConfig::new().build().expect("config");
    ZenohTransport::open(config)
        .await
        .expect("open")
        .with_timeout(Duration::from_secs(2))
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
