//! In-process gRPC tests: every server binds `127.0.0.1:0` and the client
//! connects to exactly that bound address (no other network traffic).
#![cfg(feature = "grpc")]

use core::time::Duration;
use middleware::grpc::server::serve;
use middleware::grpc::{BusService, GrpcError, GrpcServer, GrpcTransport, responder};
use middleware::{
    HandlerError, Key, KeyFilter, Publisher, Request, Requester, Responder, Subscribe, Subscriber,
};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tonic::Code;

const REVERSE: Key<'static> = Key::from_static("test.Echo/Reverse");
const FAIL: Key<'static> = Key::from_static("test.Echo/Fail");
const BIG: Key<'static> = Key::from_static("test.Echo/Big");
const BROKEN: Key<'static> = Key::from_static("test.Echo/Broken");
const MISSING: Key<'static> = Key::from_static("test.Echo/Missing");

/// Pure handler used by the responder tests.
fn echo_handler(r: &Request<'_>, resp: &mut [u8]) -> Result<usize, HandlerError> {
    let (key, req) = (r.key, r.body);
    match key.as_str() {
        "test.Echo/Reverse" => {
            let out = resp
                .get_mut(..req.len())
                .ok_or(HandlerError::BufferTooSmall)?;
            out.copy_from_slice(req);
            out.reverse();
            Ok(req.len())
        }
        "test.Echo/Fail" => Err(HandlerError::InvalidRequest),
        "test.Echo/Big" => Err(HandlerError::BufferTooSmall),
        "test.Echo/Broken" => Err(HandlerError::Internal),
        #[cfg(feature = "prost")]
        "test.Thermostat/Double" => prost_double(req, resp),
        _ => Err(HandlerError::NotFound),
    }
}

/// Loopback server handle: dropping `stop` shuts the server down.
struct Running {
    addr: SocketAddr,
    _stop: oneshot::Sender<()>,
}

async fn bind_loopback() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    (listener, addr)
}

/// Starts a server with a bus and an `echo_handler` responder.
async fn start() -> Running {
    let (svc, mut resp) = responder(16);
    tokio::spawn(async move {
        let mut handler = echo_handler;
        let mut scratch = [0u8; 256];
        while resp.serve_one(&mut handler, &mut scratch).await.is_ok() {}
    });
    let server = GrpcServer::new()
        .with_bus(BusService::default())
        .with_responder(svc);
    let (listener, addr) = bind_loopback().await;
    let (stop, stopped) = oneshot::channel::<()>();
    tokio::spawn(serve(server, listener, async {
        let _ = stopped.await;
    }));
    Running { addr, _stop: stop }
}

/// A client that also receives its own publications, so one transport can
/// publish and subscribe in a test (as `tests/zenoh.rs` does).
async fn client(addr: SocketAddr) -> GrpcTransport {
    remote_client(addr).await.with_local_delivery(true)
}

/// A client with the default rule: it never receives its own publications.
async fn remote_client(addr: SocketAddr) -> GrpcTransport {
    GrpcTransport::connect(format!("http://{addr}"))
        .await
        .unwrap()
}

/// Polls `fut` exactly once with a no-op waker, then drops it.
fn poll_once_and_drop<F: core::future::Future>(fut: F) -> bool {
    let fut = core::pin::pin!(fut);
    let waker = core::task::Waker::noop();
    fut.poll(&mut core::task::Context::from_waker(waker))
        .is_ready()
}

async fn within<F: core::future::Future>(f: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(5), f)
        .await
        .expect("timed out")
}

#[tokio::test]
async fn g1_unary_request_calls_service_method_with_raw_bytes() {
    let srv = start().await;
    let mut c = client(srv.addr).await;
    let mut buf = [0u8; 16];
    let out = within(c.request(REVERSE, b"abc", &mut buf)).await.unwrap();
    assert_eq!(out, b"cba");
}

#[tokio::test]
async fn g1_non_service_method_key_is_rejected_before_sending() {
    let srv = start().await;
    let mut c = client(srv.addr).await;
    let mut buf = [0u8; 16];
    for k in ["single", "a/b/c"] {
        let r = c.request(Key::new(k).unwrap(), b"x", &mut buf).await;
        assert!(matches!(r, Err(GrpcError::NotServiceMethod)), "{k}");
    }
}

#[tokio::test]
async fn g1_reply_larger_than_buffer_is_an_error() {
    let srv = start().await;
    let mut c = client(srv.addr).await;
    let mut buf = [0u8; 2];
    let r = within(c.request(REVERSE, b"abcd", &mut buf)).await;
    assert!(matches!(r, Err(GrpcError::BufferTooSmall { needed: 4 })));
}

#[cfg(feature = "prost")]
#[derive(Clone, PartialEq, prost::Message)]
struct SetPoint {
    #[prost(uint32, tag = "1")]
    millikelvin: u32,
}

#[cfg(feature = "prost")]
fn prost_double(req: &[u8], resp: &mut [u8]) -> Result<usize, HandlerError> {
    use middleware::Codec;
    use middleware::codec::prost::Prost;
    let m: SetPoint = Prost
        .decode(req)
        .map_err(|_| HandlerError::InvalidRequest)?;
    let out = SetPoint {
        millikelvin: m.millikelvin * 2,
    };
    Prost
        .encode(&out, resp)
        .map_err(|_| HandlerError::BufferTooSmall)
}

#[cfg(feature = "prost")]
#[tokio::test]
async fn g1_prost_codec_on_top_of_raw_bytes() {
    use middleware::codec::prost::Prost;
    use middleware::typed::Client;
    let srv = start().await;
    let mut c: Client<_, _, 64> = Client::new(client(srv.addr).await, Prost);
    let key = Key::from_static("test.Thermostat/Double");
    let resp: SetPoint = within(c.request(
        key,
        &SetPoint {
            millikelvin: 150_000,
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        resp,
        SetPoint {
            millikelvin: 300_000
        }
    );
}

#[tokio::test]
async fn g2_publish_reaches_matching_subscriber_only() {
    let srv = start().await;
    let mut c = client(srv.addr).await;
    let mut sub = within(c.subscribe(KeyFilter::Exact(Key::from_static("sensor/temp"))))
        .await
        .unwrap();
    within(c.publish(Key::from_static("sensor/other"), b"no"))
        .await
        .unwrap();
    within(c.publish(Key::from_static("sensor/temp"), b"21C"))
        .await
        .unwrap();
    let mut buf = [0u8; 64];
    let s = within(sub.recv(&mut buf)).await.unwrap();
    assert_eq!(s.key.as_str(), "sensor/temp");
    assert_eq!(s.payload, b"21C");
}

#[tokio::test]
async fn g2_publish_without_subscribers_succeeds_and_small_buffer_fails() {
    let srv = start().await;
    let mut c = client(srv.addr).await;
    within(c.publish(Key::from_static("nobody/listens"), b"x"))
        .await
        .unwrap();

    let mut sub = within(c.subscribe(KeyFilter::Exact(Key::from_static("a/b"))))
        .await
        .unwrap();
    within(c.publish(Key::from_static("a/b"), b"payload"))
        .await
        .unwrap();
    let mut tiny = [0u8; 4];
    let r = within(sub.recv(&mut tiny)).await;
    assert!(matches!(r, Err(GrpcError::BufferTooSmall { needed: 10 })));
}

#[tokio::test]
async fn g2_bus_service_mounts_on_stock_tonic_router() {
    let (listener, addr) = bind_loopback().await;
    let (_stop, stopped) = oneshot::channel::<()>();
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(BusService::new(8))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                async {
                    let _ = stopped.await;
                },
            ),
    );
    let mut c = client(addr).await;
    let mut sub = within(c.subscribe(KeyFilter::Exact(Key::from_static("x"))))
        .await
        .unwrap();
    within(c.publish(Key::from_static("x"), b"1"))
        .await
        .unwrap();
    let mut buf = [0u8; 8];
    assert_eq!(within(sub.recv(&mut buf)).await.unwrap().payload, b"1");
    // Only the bus is mounted: other services are UNIMPLEMENTED.
    let r = within(c.request(REVERSE, b"a", &mut buf)).await;
    assert!(matches!(
        r,
        Err(GrpcError::Status {
            code: Code::Unimplemented,
            ..
        })
    ));
}

#[tokio::test]
async fn g3_handler_errors_arrive_as_grpc_status_and_map_back() {
    let srv = start().await;
    let mut c = client(srv.addr).await;
    let mut buf = [0u8; 16];
    for (key, code, handler_error) in [
        (FAIL, Code::InvalidArgument, HandlerError::InvalidRequest),
        (BIG, Code::ResourceExhausted, HandlerError::BufferTooSmall),
        (BROKEN, Code::Internal, HandlerError::Internal),
        (MISSING, Code::Unimplemented, HandlerError::NotFound),
    ] {
        let err = within(c.request(key, b"x", &mut buf)).await.unwrap_err();
        assert!(
            matches!(err, GrpcError::Status { code: c, .. } if c == code),
            "{key}: {err}"
        );
        assert_eq!(err.handler_error(), Some(handler_error), "{key}");
    }
}

#[tokio::test]
async fn g3_request_larger_than_server_scratch_is_resource_exhausted() {
    let srv = start().await;
    let mut c = client(srv.addr).await;
    let mut buf = [0u8; 16];
    let err = within(c.request(REVERSE, &[7u8; 300], &mut buf))
        .await
        .unwrap_err();
    assert_eq!(err.handler_error(), Some(HandlerError::BufferTooSmall));
}

#[tokio::test]
async fn g4_responder_reports_closed_when_server_side_dropped() {
    let (svc, mut resp) = responder(1);
    drop(svc);
    let mut h = echo_handler;
    let r = resp.serve_one(&mut h, &mut [0u8; 8]).await;
    assert!(matches!(r, Err(GrpcError::Closed)));
}

#[tokio::test]
async fn g5_try_recv_is_none_when_empty_and_some_without_waiting() {
    let srv = start().await;
    let mut c = client(srv.addr).await;
    let mut sub = within(c.subscribe(KeyFilter::Exact(Key::from_static("t/try"))))
        .await
        .unwrap();
    let mut buf = [0u8; 64];
    assert!(matches!(sub.try_recv(&mut buf), Ok(None)));
    within(c.publish(Key::from_static("t/try"), b"now"))
        .await
        .unwrap();
    // The item crosses a real HTTP/2 connection, so retry briefly; each
    // `try_recv` returns immediately.
    let mut got = None;
    for _ in 0..200 {
        if let Some(s) = sub.try_recv(&mut buf).unwrap() {
            got = Some((s.key.as_str().to_owned(), s.payload.to_vec(), s.meta));
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (key, payload, meta) = got.expect("sample");
    assert_eq!((key.as_str(), payload.as_slice()), ("t/try", &b"now"[..]));
    assert_eq!(
        meta,
        middleware::SampleMeta::from_sender(c.client_id()).with_gap(Some(0))
    );
    assert!(matches!(sub.try_recv(&mut buf), Ok(None)));
}

#[tokio::test]
async fn g5_prefix_subscription_delivers_app_x_but_not_apple() {
    let srv = start().await;
    let mut c = client(srv.addr).await;
    let mut sub = within(c.subscribe(KeyFilter::Prefix(Key::from_static("app"))))
        .await
        .unwrap();
    for k in ["apple", "app/x", "app", "app/x/y"] {
        within(c.publish(Key::new(k).unwrap(), k.as_bytes()))
            .await
            .unwrap();
    }
    within(c.publish(Key::from_static("app/end"), b"app/end"))
        .await
        .unwrap();
    let mut buf = [0u8; 64];
    let mut keys = Vec::new();
    for _ in 0..4 {
        let s = within(sub.recv(&mut buf)).await.unwrap();
        assert_eq!(s.key.as_str().as_bytes(), s.payload);
        keys.push(s.key.as_str().to_owned());
    }
    // `apple` was published first; had it matched it would have come first.
    assert_eq!(keys, ["app/x", "app", "app/x/y", "app/end"]);
}

#[tokio::test]
async fn g6_cancelled_recv_loses_no_sample() {
    let srv = start().await;
    let mut c = client(srv.addr).await;
    let mut sub = within(c.subscribe(KeyFilter::Exact(Key::from_static("t/cancel"))))
        .await
        .unwrap();
    let mut buf = [0u8; 64];
    assert!(!poll_once_and_drop(sub.recv(&mut buf)), "nothing sent yet");
    within(c.publish(Key::from_static("t/cancel"), b"kept"))
        .await
        .unwrap();
    assert_eq!(within(sub.recv(&mut buf)).await.unwrap().payload, b"kept");
}

#[test]
fn g7_limits_fit_tonic_default_message_size() {
    use middleware::grpc::{MAX_MESSAGE, MAX_PAYLOAD};
    let limits = <GrpcTransport as Publisher>::LIMITS;
    assert_eq!(limits.max_key, middleware::key::MAX_KEY_LEN);
    assert_eq!(limits.max_payload, MAX_PAYLOAD);
    assert_eq!(
        middleware::grpc::bus::ITEM_OVERHEAD + limits.max_key + limits.max_payload,
        MAX_MESSAGE
    );
}

#[tokio::test]
async fn g7_payload_at_limit_round_trips_through_subscription() {
    let srv = start().await;
    let mut c = client(srv.addr).await;
    let long = "k".repeat(middleware::key::MAX_KEY_LEN);
    let key = Key::new(&long).unwrap();
    let mut sub = within(c.subscribe(KeyFilter::Exact(key))).await.unwrap();
    let payload = vec![5u8; middleware::grpc::MAX_PAYLOAD];
    within(c.publish(key, &payload)).await.unwrap();
    let mut buf = vec![0u8; long.len() + payload.len()];
    let s = within(sub.recv(&mut buf)).await.unwrap();
    assert_eq!(s.payload.len(), payload.len());
}

#[tokio::test]
async fn g8_samples_carry_the_publisher_and_self_delivery_is_refused() {
    let srv = start().await;
    let mut me = remote_client(srv.addr).await;
    let mut peer = remote_client(srv.addr).await;
    assert_ne!(me.client_id(), peer.client_id());
    let key = Key::from_static("g8/topic");
    let mut mine = within(me.subscribe(KeyFilter::Exact(key))).await.unwrap();
    let mut theirs = within(peer.subscribe(KeyFilter::Exact(key))).await.unwrap();
    within(me.publish(key, b"from me")).await.unwrap();
    within(peer.publish(key, b"from peer")).await.unwrap();
    let mut buf = [0u8; 64];
    // Each side's first sample is the other's: its own was not delivered.
    let s = within(mine.recv(&mut buf)).await.unwrap();
    assert_eq!(s.payload, b"from peer");
    assert_eq!(s.meta.sender, Some(peer.client_id()));
    assert_eq!(s.meta.seq, None);
    let s = within(theirs.recv(&mut buf)).await.unwrap();
    assert_eq!(s.payload, b"from me");
    assert_eq!(s.meta.sender, Some(me.client_id()));
    assert!(matches!(mine.try_recv(&mut buf), Ok(None)));
}

#[tokio::test]
async fn g8_local_delivery_and_shared_ids_follow_the_client_id() {
    let srv = start().await;
    let key = Key::from_static("g8/local");
    let mut looped = client(srv.addr).await;
    let mut sub = within(looped.subscribe(KeyFilter::Exact(key)))
        .await
        .unwrap();
    within(looped.publish(key, b"echo")).await.unwrap();
    let mut buf = [0u8; 64];
    let s = within(sub.recv(&mut buf)).await.unwrap();
    assert_eq!(
        (s.payload, s.meta.sender),
        (&b"echo"[..], Some(looped.client_id()))
    );

    // A second transport set to the same id is the same client: refused.
    let mut a = remote_client(srv.addr).await;
    let mut twin = remote_client(srv.addr).await.with_client_id(a.client_id());
    let mut other = remote_client(srv.addr).await;
    let mut a_sub = within(a.subscribe(KeyFilter::Exact(key))).await.unwrap();
    within(twin.publish(key, b"twin")).await.unwrap();
    within(other.publish(key, b"other")).await.unwrap();
    let s = within(a_sub.recv(&mut buf)).await.unwrap();
    assert_eq!(
        (s.payload, s.meta.sender),
        (&b"other"[..], Some(other.client_id()))
    );
}

#[cfg(feature = "istio")]
mod istio {
    use super::*;
    use bytes::Bytes;
    use core::future::Ready;
    use core::task::{Context, Poll};
    use middleware::grpc::RawCodec;
    use middleware::grpc::istio::{
        MeshChannelBuilder, MeshInterceptor, PROPAGATED_HEADERS, TraceContext,
    };
    use tonic::body::Body;
    use tonic::codegen::{BoxFuture, Service};
    use tonic::metadata::MetadataMap;
    use tonic::server::{Grpc, UnaryService};
    use tonic::service::interceptor::InterceptedService;

    /// Test server that answers every unary call with the request's
    /// `:authority` and the propagated headers it received.
    #[derive(Clone)]
    struct MetaEcho;

    struct Reply(Bytes);

    impl UnaryService<Bytes> for Reply {
        type Response = Bytes;
        type Future = Ready<Result<tonic::Response<Bytes>, tonic::Status>>;
        fn call(&mut self, _r: tonic::Request<Bytes>) -> Self::Future {
            core::future::ready(Ok(tonic::Response::new(self.0.clone())))
        }
    }

    impl Service<http::Request<Body>> for MetaEcho {
        type Response = http::Response<Body>;
        type Error = core::convert::Infallible;
        type Future = BoxFuture<Self::Response, Self::Error>;
        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, req: http::Request<Body>) -> Self::Future {
            let mut lines = vec![format!(
                "authority={}",
                req.uri().authority().map_or("", |a| a.as_str())
            )];
            for name in PROPAGATED_HEADERS.iter().chain(&["grpc-timeout"]) {
                if let Some(v) = req.headers().get(*name).and_then(|v| v.to_str().ok()) {
                    lines.push(format!("{name}={v}"));
                }
            }
            let body = Bytes::from(lines.join("\n"));
            Box::pin(async move { Ok(Grpc::new(RawCodec).unary(Reply(body), req).await) })
        }
    }

    #[tokio::test]
    async fn i2_i5_interceptor_propagates_trace_and_channel_sets_mesh_authority() {
        let (listener, addr) = bind_loopback().await;
        let (_stop, stopped) = oneshot::channel::<()>();
        tokio::spawn(
            tonic::transport::Server::builder().serve_with_incoming_shutdown(
                MetaEcho,
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                async {
                    let _ = stopped.await;
                },
            ),
        );

        // Inbound request metadata as a mesh server would receive it.
        let mut inbound = MetadataMap::new();
        for name in PROPAGATED_HEADERS {
            inbound.insert(name, format!("v-{name}").parse().unwrap());
        }
        inbound.insert("authorization", "must-not-leak".parse().unwrap());
        let trace = TraceContext::from_metadata(&inbound);

        // The loopback server stands in for the sidecar.
        let channel = within(
            MeshChannelBuilder::for_service("reviews", "bookinfo", 9080)
                .unwrap()
                .via_sidecar(addr.to_string())
                .connect(),
        )
        .await
        .unwrap();
        let interceptor = MeshInterceptor::new(trace).with_timeout(Duration::from_secs(2));
        let mut c = GrpcTransport::new(InterceptedService::new(channel, interceptor));

        let mut buf = [0u8; 1024];
        let out = within(c.request(Key::from_static("pkg.Reviews/Get"), b"", &mut buf))
            .await
            .unwrap();
        let text = core::str::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines.first(),
            Some(&"authority=reviews.bookinfo.svc.cluster.local:9080")
        );
        for name in PROPAGATED_HEADERS {
            let expected = format!("{name}=v-{name}");
            assert!(
                lines.contains(&expected.as_str()),
                "missing {expected} in {text}"
            );
        }
        assert!(lines.contains(&"grpc-timeout=2000000u"), "{text}");
        assert!(!text.contains("must-not-leak"));
    }
}

// ------------------------------------------------------------ 0.5 (G10–G13, I6–I8)

/// Serves `server` on a fresh loopback port.
async fn serve_on_loopback(server: GrpcServer) -> Running {
    let (listener, addr) = bind_loopback().await;
    let (stop, stopped) = oneshot::channel::<()>();
    tokio::spawn(serve(server, listener, async {
        let _ = stopped.await;
    }));
    Running { addr, _stop: stop }
}

/// Inserts an Envoy-style XFCC header, as a sidecar in front of the
/// server would; tests with an untrusting responder use it to show the
/// header is ignored.
fn forged_xfcc(mut r: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
    r.metadata_mut().insert(
        "x-forwarded-client-cert",
        "By=spiffe://cluster.local/ns/test/sa/server;URI=spiffe://cluster.local/ns/test/sa/client"
            .parse()
            .unwrap(),
    );
    Ok(r)
}

#[tokio::test]
async fn g10_handler_sees_client_id_and_deadline_but_no_untrusted_principal() {
    let (svc, mut resp) = responder(4);
    let srv = serve_on_loopback(GrpcServer::new().with_responder(svc)).await;
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{}", srv.addr))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut c = GrpcTransport::new(tonic::service::interceptor::InterceptedService::new(
        channel,
        forged_xfcc as fn(_) -> _,
    ))
    .with_client_id(middleware::SenderId([0xAB; 8]));
    let seen = std::cell::RefCell::new(None);
    let mut h = middleware::transport::handler_fn(|r, _out| {
        *seen.borrow_mut() = Some((
            r.meta.sender,
            r.meta.principal.map(str::to_owned),
            r.meta.deadline_ms,
        ));
        Ok(0)
    });
    let mut scratch = [0u8; 64];
    let mut out = [0u8; 8];
    let opts = middleware::RequestOptions::DEFAULT.with_timeout(Duration::from_millis(1500));
    let (got, served) = within(async {
        tokio::join!(
            c.request_with(REVERSE, b"x", &mut out, &opts),
            resp.serve_one(&mut h, &mut scratch)
        )
    })
    .await;
    served.unwrap();
    got.unwrap();
    let (sender, principal, deadline) = seen.borrow().clone().unwrap();
    assert_eq!(sender, Some(middleware::SenderId([0xAB; 8])));
    assert_eq!(principal, None, "XFCC is only trusted when configured");
    let ms = deadline.expect("grpc-timeout carried");
    assert!(ms > 1000 && ms <= 1500, "{ms}");

    // No timeout: no deadline.
    let (got, served) = within(async {
        tokio::join!(
            c.request(REVERSE, b"x", &mut out),
            resp.serve_one(&mut h, &mut scratch)
        )
    })
    .await;
    served.unwrap();
    got.unwrap();
    assert_eq!(seen.borrow().clone().unwrap().2, None);
}

#[tokio::test]
async fn g11_busy_responder_and_refused_connection_are_unavailable() {
    // Queue of one, never served: the second concurrent call is refused.
    let (svc, _idle) = responder(1);
    let srv = serve_on_loopback(GrpcServer::new().with_responder(svc)).await;
    let mut a = client(srv.addr).await;
    let mut b = client(srv.addr).await;
    let (mut x, mut y) = ([0u8; 8], [0u8; 8]);
    let opts = middleware::RequestOptions::DEFAULT.with_timeout(Duration::from_millis(500));
    let (first, second) = tokio::join!(a.request_with(REVERSE, b"1", &mut x, &opts), async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        b.request_with(REVERSE, b"2", &mut y, &opts).await
    });
    assert_eq!(
        first.unwrap_err().handler_error(),
        Some(HandlerError::DeadlineExceeded)
    );
    assert_eq!(
        second.unwrap_err().handler_error(),
        Some(HandlerError::Unavailable)
    );

    // A port we bound and closed again: the connection is refused.
    let (listener, addr) = bind_loopback().await;
    drop(listener);
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect_lazy();
    let mut c = GrpcTransport::new(channel);
    let r = within(c.request_with(REVERSE, b"x", &mut x, &opts)).await;
    assert_eq!(
        r.unwrap_err().handler_error(),
        Some(HandlerError::Unavailable)
    );
}

#[tokio::test]
async fn g12_subscriber_queue_drops_oldest_and_reports_the_gap() {
    let srv = start().await;
    let mut publisher = remote_client(srv.addr).await;
    let mut c = remote_client(srv.addr).await.with_queue_depth(3);
    assert_eq!(c.queue_depth(), 4, "rounded up to a power of two");
    let key = Key::from_static("g11/load");
    let mut sub = within(c.subscribe(KeyFilter::Exact(key))).await.unwrap();
    for n in 0u8..10 {
        within(publisher.publish(key, &[n])).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut buf = [0u8; 32];
    let mut got = Vec::new();
    while let Some(s) = sub.try_recv(&mut buf).unwrap() {
        got.push((s.payload[0], s.meta.gap));
    }
    assert_eq!(
        got,
        [(6, Some(6)), (7, Some(0)), (8, Some(0)), (9, Some(0))]
    );
    // A too-small buffer keeps the sample for a retry.
    within(publisher.publish(key, b"long payload"))
        .await
        .unwrap();
    let mut tiny = [0u8; 4];
    let e = within(sub.recv(&mut tiny)).await.unwrap_err();
    assert!(
        matches!(e, GrpcError::BufferTooSmall { needed: 20 }),
        "{e:?}"
    );
    assert_eq!(
        within(sub.recv(&mut buf)).await.unwrap().payload,
        b"long payload"
    );
}

#[tokio::test]
async fn g13_health_check_and_watch_over_loopback() {
    use middleware::grpc::{HealthClient, ServingStatus, health};
    let (svc, mut announcer) = health();
    let srv = serve_on_loopback(GrpcServer::new().with_health(svc)).await;
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{}", srv.addr))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut hc = HealthClient::new(channel);
    assert_eq!(within(hc.check("")).await.unwrap(), ServingStatus::Serving);
    let e = within(hc.check("cam/jpeg")).await.unwrap_err();
    assert!(
        matches!(
            e,
            GrpcError::Status {
                code: Code::NotFound,
                ..
            }
        ),
        "{e:?}"
    );
    let mut w = within(hc.watch("cam/jpeg")).await.unwrap();
    assert_eq!(
        within(w.next()).await.unwrap(),
        ServingStatus::ServiceUnknown
    );
    announcer
        .set_status("cam/jpeg", ServingStatus::Serving)
        .unwrap();
    assert_eq!(within(w.next()).await.unwrap(), ServingStatus::Serving);
    assert_eq!(
        within(hc.check("cam/jpeg")).await.unwrap(),
        ServingStatus::Serving
    );
    announcer
        .set_status("cam/jpeg", ServingStatus::NotServing)
        .unwrap();
    assert_eq!(within(w.next()).await.unwrap(), ServingStatus::NotServing);
    drop(announcer);
    assert!(matches!(
        within(w.next()).await,
        Err(GrpcError::StreamEnded)
    ));
}

#[cfg(feature = "istio")]
mod mesh {
    use super::*;
    use middleware::discovery::{
        Announce, Browse, Directory, Endpoint, Event, Service, ServiceKind,
    };
    use middleware::grpc::mesh::{MeshBrowse, MeshClient, MeshDialer};
    use middleware::grpc::{PrincipalSource, health};
    use middleware::transport::memory::MemoryBus;
    use middleware::{Flow, Gather, RequestOptions, SenderId, Target};

    const KIND: ServiceKind<'static> = ServiceKind::from_static("test/echo");

    /// A responder server whose handler answers with its node byte.
    async fn node_server(node: u8) -> Running {
        let (svc, mut resp) = responder(8);
        tokio::spawn(async move {
            let mut h = middleware::transport::handler_fn(move |_r, out| {
                *out.first_mut().ok_or(HandlerError::BufferTooSmall)? = node;
                Ok(1)
            });
            let mut scratch = [0u8; 64];
            while resp.serve_one(&mut h, &mut scratch).await.is_ok() {}
        });
        serve_on_loopback(
            GrpcServer::new()
                .with_responder(svc.with_principal_source(PrincipalSource::SidecarXfcc)),
        )
        .await
    }

    fn up(node: u8, addr: SocketAddr) -> Event<'static> {
        let instance: &'static str = Box::leak(format!("n{node}").into_boxed_str());
        Event::Up(Service {
            kind: KIND,
            instance,
            node: Some(SenderId([node; 8])),
            endpoint: Endpoint::Socket(addr),
            txt: b"",
            ttl_s: 60,
        })
    }

    #[tokio::test]
    async fn i7_mesh_client_routes_by_node_and_gathers_every_endpoint() {
        let (a, b) = (node_server(1).await, node_server(2).await);
        let d = Directory::<4>::new();
        let (d, _) = d.observe(&up(1, a.addr), 0);
        let (d, _) = d.observe(&up(2, b.addr), 0);
        let mut c = MeshClient::<4>::new(KIND, MeshDialer::direct()).with_directory(d);
        let mut out = [0u8; 4];
        for node in [2u8, 1, 2] {
            let opts = RequestOptions::DEFAULT.with_target(Target::Node(SenderId([node; 8])));
            let r = within(c.request_with(REVERSE, b"", &mut out, &opts))
                .await
                .unwrap();
            assert_eq!(r, [node]);
        }
        // Any: round robin over both.
        let mut seen = Vec::new();
        for _ in 0..2 {
            let r = within(c.request(REVERSE, b"", &mut out)).await.unwrap();
            seen.push(r[0]);
        }
        seen.sort_unstable();
        assert_eq!(seen, [1, 2]);
        // Gather: one reply per endpoint, sender = its node.
        let mut replies = Vec::new();
        let n = within(c.request_each(
            REVERSE,
            b"",
            &mut out,
            &RequestOptions::DEFAULT.with_timeout(Duration::from_secs(2)),
            8,
            |r| {
                replies.push((r.sender, r.result.map(<[u8]>::to_vec)));
                Flow::Continue
            },
        ))
        .await
        .unwrap();
        assert_eq!(n, 2);
        replies.sort_by_key(|(s, _)| s.map(|s| s.0));
        assert_eq!(
            replies,
            [
                (Some(SenderId([1; 8])), Ok(vec![1])),
                (Some(SenderId([2; 8])), Ok(vec![2]))
            ]
        );
        // max_replies bounds it.
        let n = within(
            c.request_each(REVERSE, b"", &mut out, &RequestOptions::DEFAULT, 1, |_| {
                Flow::Continue
            }),
        )
        .await
        .unwrap();
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn i8_mesh_browse_reports_up_only_while_serving() {
        let (svc, mut health_state) = health();
        let srv = serve_on_loopback(GrpcServer::new().with_health(svc)).await;
        let bus = MemoryBus::new();
        let mut dns = bus.node(SenderId([0xD0; 8])).discovery().unwrap();
        let mut b: MeshBrowse<_, 4> = MeshBrowse::new(
            bus.node(SenderId([0xB0; 8])).discovery().unwrap(),
            // The test server stands in for the sidecar.
            MeshDialer::via_sidecar(srv.addr.to_string()),
        );
        b.browse(KeyFilter::Exact(KIND.key())).await.unwrap();
        let service = Service {
            kind: KIND,
            instance: "pod-1",
            node: None,
            endpoint: Endpoint::Host {
                name: "echo.test.svc.cluster.local",
                port: 50051,
            },
            txt: b"",
            ttl_s: 60,
        };
        let mut buf = [0u8; 512];
        // Listed by DNS but not ready: nothing yet.
        dns.announce(&service).await.unwrap();
        let quiet = tokio::time::timeout(Duration::from_millis(300), b.next(&mut buf)).await;
        assert!(quiet.is_err(), "Up before SERVING: {quiet:?}");
        assert_eq!(b.tracked(), 1);
        // Ready: Up, with the DNS record.
        health_state
            .set_status("test/echo", middleware::grpc::ServingStatus::Serving)
            .unwrap();
        let e = within(b.next(&mut buf)).await.unwrap();
        assert_eq!(e, Event::Up(service));
        // Not ready: Down; ready again: Up.
        health_state.withdraw(KIND, "x").await.unwrap();
        let e = within(b.next(&mut buf)).await.unwrap();
        assert_eq!(
            e,
            Event::Down {
                kind: KIND,
                instance: "pod-1"
            }
        );
        health_state.announce(&service).await.unwrap();
        assert!(matches!(
            within(b.next(&mut buf)).await.unwrap(),
            Event::Up(_)
        ));
        // The health stream ends (its announcer is gone): Down.
        drop(health_state);
        let e = within(b.next(&mut buf)).await.unwrap();
        assert_eq!(
            e,
            Event::Down {
                kind: KIND,
                instance: "pod-1"
            }
        );
        // DNS withdraws it: forgotten.
        dns.withdraw(KIND, "pod-1").await.unwrap();
        let quiet = tokio::time::timeout(Duration::from_millis(200), b.next(&mut buf)).await;
        assert!(quiet.is_err(), "{quiet:?}");
        assert_eq!(b.tracked(), 0);
    }

    #[tokio::test]
    async fn i8_mesh_browse_reports_down_when_dns_withdraws() {
        let (svc, mut health_state) = health();
        // Kept alive: dropping it would end the watch.
        let srv = serve_on_loopback(GrpcServer::new().with_health(svc)).await;
        let bus = MemoryBus::new();
        let mut dns = bus.node(SenderId([0xD0; 8])).discovery().unwrap();
        let mut b: MeshBrowse<_, 4> = MeshBrowse::new(
            bus.node(SenderId([0xB0; 8])).discovery().unwrap(),
            MeshDialer::direct(),
        );
        b.browse(KeyFilter::Exact(KIND.key())).await.unwrap();
        let service = Service {
            kind: KIND,
            instance: "pod-2",
            node: None,
            endpoint: Endpoint::Socket(srv.addr),
            txt: b"",
            ttl_s: 60,
        };
        health_state.announce(&service).await.unwrap();
        dns.announce(&service).await.unwrap();
        let mut buf = [0u8; 512];
        assert!(matches!(
            within(b.next(&mut buf)).await.unwrap(),
            Event::Up(_)
        ));
        dns.withdraw(KIND, "pod-2").await.unwrap();
        let e = within(b.next(&mut buf)).await.unwrap();
        assert_eq!(
            e,
            Event::Down {
                kind: KIND,
                instance: "pod-2"
            }
        );
        assert_eq!(b.tracked(), 0);
    }
}

// ------------------------------------------------------------ conformance

#[cfg(feature = "istio")]
mod conformance;

/// Every applicable conformance case over in-process servers on
/// 127.0.0.1:0 (K15 for gRPC). The requester injects an Envoy-style
/// `x-forwarded-client-cert`, standing in for the sidecar, and the
/// responder trusts it ([`PrincipalSource::SidecarXfcc`]); gather and
/// discovery go through `grpc::mesh`.
#[cfg(feature = "istio")]
mod conformance_cases {
    use super::conformance::{
        DiscoveryFixture, GatherFixture, PubSubFixture, ReqRepFixture, discovery, pubsub, reqrep,
    };
    use super::*;
    use middleware::SenderId;
    use middleware::discovery::{Announce, Directory, Endpoint, Event, Service, ServiceKind};
    use middleware::grpc::mesh::{MeshBrowse, MeshClient, MeshDialer};
    use middleware::grpc::{GrpcAnnouncer, GrpcResponder, GrpcSubscriber, PrincipalSource, health};
    use middleware::transport::memory::{MemoryBus, MemoryDiscovery};
    use tonic::service::interceptor::InterceptedService;
    use tonic::transport::Channel;

    const CLIENT: SenderId = SenderId([0xC1; 8]);
    const PRINCIPAL: &str = "spiffe://cluster.local/ns/test/sa/client";
    const XFCC: &str = "By=spiffe://cluster.local/ns/test/sa/server;\
        Hash=468ed33be74eee6556d90c0149c1309e9ba61d6425303443c0748a02dd8de688;\
        Subject=\"CN=client,O=test\";URI=spiffe://cluster.local/ns/test/sa/client";

    type Interceptor = fn(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status>;

    fn sidecar(mut r: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
        r.metadata_mut()
            .insert("x-forwarded-client-cert", XFCC.parse().unwrap());
        Ok(r)
    }

    /// Keeps every server it started running.
    #[derive(Default)]
    struct Grpc {
        servers: Vec<Running>,
    }

    impl Grpc {
        async fn responder_server(&mut self) -> (GrpcResponder, SocketAddr) {
            let (svc, resp) = responder(16);
            let srv = serve_on_loopback(
                GrpcServer::new()
                    .with_responder(svc.with_principal_source(PrincipalSource::SidecarXfcc)),
            )
            .await;
            let addr = srv.addr;
            self.servers.push(srv);
            (resp, addr)
        }
    }

    async fn channel(addr: SocketAddr) -> Channel {
        tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect()
            .await
            .unwrap()
    }

    impl ReqRepFixture for Grpc {
        type Requester = GrpcTransport<InterceptedService<Channel, Interceptor>>;
        type Responder = GrpcResponder;
        const PROPAGATES_DEADLINE: bool = true;

        async fn connect(&mut self, _key: Key<'static>) -> (Self::Requester, GrpcResponder) {
            let (resp, addr) = self.responder_server().await;
            let c = GrpcTransport::new(InterceptedService::new(
                channel(addr).await,
                sidecar as Interceptor,
            ))
            .with_client_id(CLIENT);
            (c, resp)
        }

        fn handler_error(e: &GrpcError) -> Option<HandlerError> {
            e.handler_error()
        }

        fn is_timeout(e: &GrpcError) -> bool {
            e.handler_error() == Some(HandlerError::DeadlineExceeded)
        }

        fn expected_sender(&self) -> Option<SenderId> {
            Some(CLIENT)
        }

        fn expected_principal(&self) -> Option<&str> {
            Some(PRINCIPAL)
        }
    }

    impl GatherFixture for Grpc {
        type Gatherer = MeshClient<8>;
        type Responder = GrpcResponder;

        async fn connect(
            &mut self,
            key: Key<'static>,
            n: usize,
        ) -> (MeshClient<8>, Vec<GrpcResponder>) {
            let kind = ServiceKind::new(key);
            let mut directory = Directory::<8>::new();
            let mut responders = Vec::new();
            for i in 0..n {
                let (resp, addr) = self.responder_server().await;
                responders.push(resp);
                let instance: &'static str = Box::leak(format!("r{i}").into_boxed_str());
                let up = Event::Up(Service {
                    kind,
                    instance,
                    node: Some(SenderId([i as u8 + 1; 8])),
                    endpoint: Endpoint::Socket(addr),
                    txt: b"",
                    ttl_s: 60,
                });
                directory = directory.observe(&up, 0).0;
            }
            let client = MeshClient::new(kind, MeshDialer::direct())
                .with_directory(directory)
                .with_client_id(CLIENT);
            (client, responders)
        }
    }

    impl PubSubFixture for Grpc {
        type Publisher = GrpcTransport;
        type Subscriber = GrpcSubscriber;
        const REPORTS_GAPS: bool = true;

        async fn connect(&mut self, filter: KeyFilter<'static>) -> (GrpcTransport, GrpcSubscriber) {
            let srv = start().await;
            let publisher = remote_client(srv.addr).await;
            let mut c = remote_client(srv.addr).await.with_queue_depth(4);
            let sub = c.subscribe(filter).await.unwrap();
            self.servers.push(srv);
            (publisher, sub)
        }

        fn queue_depth(&self) -> usize {
            4
        }

        async fn settle(&mut self) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// What a mesh announcement is: the DNS-SD record (in a cluster, the
    /// Kubernetes Service; here the in-process bus) plus the pod's health.
    /// Withdrawing only changes the health, as a pod going unready does:
    /// the Service still lists it, and `MeshBrowse` must see it go.
    struct MeshAnnouncer {
        dns: MemoryDiscovery,
        health: GrpcAnnouncer,
    }

    impl Announce for MeshAnnouncer {
        type Error = String;

        async fn announce(&mut self, service: &Service<'_>) -> Result<(), String> {
            self.health
                .announce(service)
                .await
                .map_err(|e| e.to_string())?;
            self.dns
                .announce(service)
                .await
                .map_err(|e| format!("{e:?}"))
        }

        async fn withdraw(&mut self, kind: ServiceKind<'_>, instance: &str) -> Result<(), String> {
            self.health
                .withdraw(kind, instance)
                .await
                .map_err(|e| e.to_string())
        }
    }

    struct Discovery {
        bus: MemoryBus,
        servers: Vec<Running>,
    }

    impl DiscoveryFixture for Discovery {
        type Announcer = MeshAnnouncer;
        type Browser = MeshBrowse<MemoryDiscovery, 8>;

        async fn connect(&mut self) -> (MeshAnnouncer, MeshBrowse<MemoryDiscovery, 8>) {
            let (svc, announcer) = health();
            let srv = serve_on_loopback(GrpcServer::new().with_health(svc)).await;
            // The case announces 127.0.0.1:7447; the health server stands
            // in for the sidecar that would route that authority.
            let dialer = MeshDialer::via_sidecar(srv.addr.to_string());
            self.servers.push(srv);
            let a = MeshAnnouncer {
                dns: self.bus.node(SenderId([0xA0; 8])).discovery().unwrap(),
                health: announcer,
            };
            let b = MeshBrowse::new(
                self.bus.node(SenderId([0xB0; 8])).discovery().unwrap(),
                dialer,
            );
            (a, b)
        }
    }

    fn discovery_fixture() -> Discovery {
        Discovery {
            bus: MemoryBus::new(),
            servers: Vec::new(),
        }
    }

    #[tokio::test]
    async fn k15_grpc_round_trip() {
        reqrep::round_trip(&mut Grpc::default()).await;
    }

    #[tokio::test]
    async fn k15_grpc_timeout_is_honoured() {
        reqrep::timeout_is_honoured(&mut Grpc::default()).await;
    }

    #[tokio::test]
    async fn k15_grpc_handler_errors_round_trip() {
        reqrep::handler_errors_round_trip(&mut Grpc::default()).await;
    }

    #[tokio::test]
    async fn k15_grpc_handler_sees_sender_principal_and_deadline() {
        reqrep::handler_sees_caller_identity(&mut Grpc::default()).await;
    }

    #[tokio::test]
    async fn k15_grpc_gather_collects_every_responder() {
        reqrep::gather_collects_every_responder(&mut Grpc::default(), 3).await;
    }

    #[tokio::test]
    async fn k15_grpc_overload_matches_declared_delivery() {
        pubsub::overload_matches_declared_delivery(&mut Grpc::default()).await;
    }

    #[tokio::test]
    async fn k15_grpc_gap_is_reported() {
        pubsub::gap_is_reported(&mut Grpc::default()).await;
    }

    #[tokio::test]
    async fn k15_grpc_announce_then_withdraw() {
        discovery::announce_then_withdraw(&mut discovery_fixture()).await;
    }

    #[tokio::test]
    async fn k15_grpc_goodbye_removes_record_at_once() {
        discovery::goodbye_removes_record_at_once(&mut discovery_fixture()).await;
    }

    #[test]
    fn k15_grpc_directory_expiry() {
        discovery::directory_expiry();
    }
}
