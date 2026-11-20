//! WS3: a Zenoh client reaches a `ws/` listener through `wss://`, a
//! TLS-terminating proxy (as Traefik is in the cluster) and the loopback
//! [`WsBridge`], and puts, gets and subscribes end to end.
//!
//! Network conduct: loopback only. The "router" is a peer session listening
//! on `ws/127.0.0.1:<port>`; the proxy ends TLS for the name `updates.test`
//! with a self-signed certificate the bridge is told to trust, and forwards
//! the decrypted bytes to that port; the client session connects only to
//! the bridge.
#![cfg(feature = "zenoh-ws")]

use std::sync::Arc;
use std::time::Duration;

use middleware::zenoh::wsbridge::{WsBridge, WsTarget};
use middleware::zenoh::{SessionConfig, ZenohTransport};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_rustls::rustls::{self, RootCertStore, ServerConfig};

const HOST: &str = "updates.test";
const TIMEOUT: Duration = Duration::from_secs(10);

async fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    l.local_addr().expect("addr").port()
}

/// A TLS-terminating proxy for `HOST` in front of `127.0.0.1:upstream`;
/// returns its address and the root the client must trust.
async fn tls_proxy(upstream: u16) -> (std::net::SocketAddr, RootCertStore) {
    let cert = rcgen::generate_simple_self_signed(vec![HOST.to_owned()]).expect("cert");
    let der = CertificateDer::from(cert.cert.der().to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()));
    let mut roots = RootCertStore::empty();
    roots.add(der.clone()).expect("root");
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_no_client_auth()
        .with_single_cert(vec![der], key)
        .expect("server config");
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };
                // The bridge must send the host name (SNI) and the path.
                assert_eq!(tls.get_ref().1.server_name(), Some(HOST));
                let Ok(mut up) = TcpStream::connect(("127.0.0.1", upstream)).await else {
                    return;
                };
                let _ = tokio::io::copy_bidirectional(&mut tls, &mut up).await;
            });
        }
    });
    (addr, roots)
}

/// The router (listening on `ws/`), the bridge and the client.
async fn setup() -> (ZenohTransport, ZenohTransport, WsBridge) {
    let port = free_port().await;
    let router = SessionConfig::new()
        .listen(format!("ws/127.0.0.1:{port}"))
        .build()
        .expect("config");
    let router = ZenohTransport::open(router).await.expect("router");
    let (proxy, roots) = tls_proxy(port).await;
    let bridge = WsBridge::start(WsTarget {
        url: format!("wss://{HOST}/zenoh"),
        connect_addr: Some(proxy),
        roots: Some(Arc::new(roots)),
    })
    .await
    .expect("bridge");
    let client = SessionConfig::new()
        .connect(bridge.locator())
        .build()
        .expect("config");
    let client = ZenohTransport::open(client).await.expect("client");
    (router, client, bridge)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ws3_put_get_and_subscribe_through_wss_and_a_tls_proxy() {
    let (router, client, _bridge) = setup().await;
    let r = router.session();
    let c = client.session();
    // Subscribe on the router, put from the client (a large value: several
    // batches each way).
    let sub = r.declare_subscriber("ws/test/up").await.expect("sub");
    let big: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    // The routes need a moment to propagate over the new link.
    tokio::time::sleep(Duration::from_millis(300)).await;
    c.put("ws/test/up", big.clone()).await.expect("put");
    let s = tokio::time::timeout(TIMEOUT, sub.recv_async())
        .await
        .expect("sample in time")
        .expect("sample");
    assert_eq!(s.payload().to_bytes().as_ref(), big.as_slice());

    // Subscribe on the client, put from the router.
    let down = c.declare_subscriber("ws/test/down").await.expect("sub");
    tokio::time::sleep(Duration::from_millis(300)).await;
    r.put("ws/test/down", "hello").await.expect("put");
    let s = tokio::time::timeout(TIMEOUT, down.recv_async())
        .await
        .expect("sample in time")
        .expect("sample");
    assert_eq!(s.payload().to_bytes().as_ref(), b"hello");

    // A queryable on the router, a get from the client.
    let q = r.declare_queryable("ws/test/q").await.expect("queryable");
    tokio::spawn(async move {
        while let Ok(query) = q.recv_async().await {
            let body = query
                .payload()
                .map(|p| p.to_bytes().to_vec())
                .unwrap_or_default();
            let mut rev = body.clone();
            rev.reverse();
            let _ = query.reply(query.key_expr().clone(), rev).await;
        }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let replies = c
        .get("ws/test/q")
        .payload("abc")
        .timeout(TIMEOUT)
        .await
        .expect("get");
    let reply = tokio::time::timeout(TIMEOUT, replies.recv_async())
        .await
        .expect("reply in time")
        .expect("reply");
    let sample = reply.result().expect("ok reply");
    assert_eq!(sample.payload().to_bytes().as_ref(), b"cba");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ws2_a_wrong_root_never_connects() {
    let port = free_port().await;
    let router = SessionConfig::new()
        .listen(format!("ws/127.0.0.1:{port}"))
        .build()
        .expect("config");
    let _router = ZenohTransport::open(router).await.expect("router");
    let (proxy, _roots) = tls_proxy(port).await;
    // Trust a different self-signed root: the TLS handshake fails, so the
    // bridge closes the session's TCP link at once.
    let (_, other) = tls_proxy(port).await;
    let bridge = WsBridge::start(WsTarget {
        url: format!("wss://{HOST}/zenoh"),
        connect_addr: Some(proxy),
        roots: Some(Arc::new(other)),
    })
    .await
    .expect("bridge");
    let mut tcp = TcpStream::connect(bridge.local_addr()).await.expect("dial");
    let mut buf = [0u8; 1];
    let n = tokio::time::timeout(TIMEOUT, tokio::io::AsyncReadExt::read(&mut tcp, &mut buf))
        .await
        .expect("closed in time")
        .unwrap_or(0);
    assert_eq!(n, 0, "the bridge closes the link");
}
