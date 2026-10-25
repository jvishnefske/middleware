//! In-process gRPC tests: every server binds `127.0.0.1:0` and the client
//! connects to exactly that bound address (no other network traffic).
#![cfg(feature = "grpc")]

use core::time::Duration;
use middleware::grpc::server::serve;
use middleware::grpc::{BusService, GrpcError, GrpcServer, GrpcTransport, responder};
use middleware::{
    HandlerError, Key, KeyFilter, Publisher, Requester, Responder, Subscribe, Subscriber,
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
fn echo_handler(key: Key<'_>, req: &[u8], resp: &mut [u8]) -> Result<usize, HandlerError> {
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

async fn client(addr: SocketAddr) -> GrpcTransport {
    GrpcTransport::connect(format!("http://{addr}"))
        .await
        .unwrap()
}

/// Polls `fut` exactly once with a no-op waker, then drops it.
fn poll_once_and_drop<F: core::future::Future>(fut: F) -> bool {
    let fut = core::pin::pin!(fut);
    let waker = core::task::Waker::noop();
    fut.poll(&mut core::task::Context::from_waker(waker)).is_ready()
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

    let mut sub = within(c.subscribe(KeyFilter::Exact(Key::from_static("a/b")))).await.unwrap();
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
    let mut sub = within(c.subscribe(KeyFilter::Exact(Key::from_static("x")))).await.unwrap();
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
    assert_eq!(meta, middleware::SampleMeta::default());
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
    assert_eq!(1 + limits.max_key + limits.max_payload, MAX_MESSAGE);
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
