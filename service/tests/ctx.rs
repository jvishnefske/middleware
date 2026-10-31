//! `Ctx` and `ServiceHandler` from outside the crate (M4): a downstream
//! crate implements `ServiceHandler` for its own type next to the blanket
//! implementation for plain handlers, and a call through `Ctx::client`
//! continues the inbound trace over a real gRPC hop on 127.0.0.1.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use middleware::Key;
use middleware::discovery::{Directory, Endpoint, Event, Service as Record, ServiceKind};
use middleware::grpc::mesh::{MeshClient, MeshDialer};
use middleware::grpc::server::serve;
use middleware::grpc::{GrpcServer, responder};
use middleware::transport::{
    HandlerError, Request, RequestMeta, Requester, Responder, SenderId, TraceParent, handler_fn,
};
use mw_service::{BoxedHandler, Ctx, Links, Service, ServiceHandler, server_trace};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

const KIND: ServiceKind<'static> = ServiceKind::from_static("demo/echo");
const KEY: Key<'static> = Key::from_static("demo/echo");
const TP: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

/// A downstream server answering `"<traceparent>|<request id>|<deadline?>"`
/// as its handler saw them.
async fn reporting_server() -> (SocketAddr, oneshot::Sender<()>) {
    let (svc, mut resp) = responder(8);
    tokio::spawn(async move {
        let mut h = handler_fn(|r, out| {
            let text = format!(
                "{}|{}|{}",
                r.meta.trace.map_or("-".to_owned(), |t| t.to_string()),
                r.meta.request_id.map_or("-", |i| i.as_str()),
                r.meta.deadline_ms.map_or("-".to_owned(), |d| d.to_string()),
            );
            let dst = out
                .get_mut(..text.len())
                .ok_or(HandlerError::BufferTooSmall)?;
            dst.copy_from_slice(text.as_bytes());
            Ok(text.len())
        });
        let mut scratch = [0u8; 512];
        while resp.serve_one(&mut h, &mut scratch).await.is_ok() {}
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    tokio::spawn(serve(
        GrpcServer::new().with_responder(svc),
        listener,
        async {
            let _ = stopped.await;
        },
    ));
    (addr, stop)
}

/// A local type with its own `ServiceHandler` impl: forwards the request
/// to `demo/echo` through the context.
struct Forward;

impl ServiceHandler for Forward {
    async fn handle(
        &mut self,
        ctx: &mut Ctx<'_>,
        req: &Request<'_>,
        out: &mut [u8],
    ) -> Result<usize, HandlerError> {
        let mut down = ctx.client("demo/echo").ok_or(HandlerError::Unavailable)?;
        let reply = down
            .request(KEY, req.body, out)
            .await
            .map_err(|e| e.handler_error().unwrap_or(HandlerError::Internal))?;
        Ok(reply.len())
    }
}

#[tokio::test]
async fn m4_ctx_client_continues_trace_request_id_and_deadline_downstream() {
    let (addr, _stop) = reporting_server().await;
    let directory = Directory::<{ mw_service::ctx::DIRECTORY_SLOTS }>::new()
        .observe(
            &Event::Up(Record {
                kind: KIND,
                instance: "echo-0",
                node: Some(SenderId([1; 8])),
                endpoint: Endpoint::Socket(addr),
                txt: b"",
                ttl_s: 60,
            }),
            0,
        )
        .0;
    let mut links = Links::new().with_client(
        KIND,
        MeshClient::new(KIND, MeshDialer::direct()).with_directory(directory),
    );

    // The inbound request, as a runner would hand it over.
    let inbound = TraceParent::parse(TP).unwrap();
    let meta = RequestMeta::NONE
        .with_trace(inbound)
        .with_request_id_str("edge-req-7")
        .with_deadline_ms(2_000);
    let span = server_trace(meta.trace, [0x5a; 8], [0x11; 16]);
    let mut ctx = Ctx::new(meta, span, &mut links)
        .with_deadline(Instant::now() + Duration::from_millis(2_000));

    let mut handler = BoxedHandler::new(Forward);
    let req = Request::new(Key::from_static("demo/front"), b"x").with_meta(meta);
    let mut out = [0u8; 256];
    let n = tokio::time::timeout(
        Duration::from_secs(5),
        handler.call(&mut ctx, &req, &mut out),
    )
    .await
    .expect("prompt")
    .expect("forwarded");
    let text = std::str::from_utf8(&out[..n]).unwrap();
    let mut parts = text.split('|');
    let seen = TraceParent::parse(parts.next().unwrap()).expect("downstream saw a trace");
    assert_eq!(seen.trace_id(), inbound.trace_id(), "same trace");
    assert_eq!(seen.span_id(), [0x5a; 8], "parent is this handler's span");
    assert!(seen.is_sampled());
    assert_eq!(parts.next(), Some("edge-req-7"));
    let deadline: u32 = parts.next().unwrap().parse().expect("deadline propagated");
    assert!(deadline <= 2_000 && deadline > 0, "{deadline}");
}

#[test]
fn m4_builder_takes_plain_and_async_handlers() {
    let svc = Service::builder("demo/front")
        .serve("demo/front", Forward)
        .serve("demo/ping", handler_fn(|_r, _out| Ok(0)))
        .calls("demo/echo")
        .build()
        .expect("valid");
    assert_eq!(svc.spec().serves, ["demo/front", "demo/ping"]);
}
