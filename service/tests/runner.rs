//! The runner end to end on 127.0.0.1 (M6–M10): `Service::run_with_options`
//! serving real gRPC, draining on shutdown, exporting spans to an
//! in-process OTLP collector, discovering called kinds through a stub DNS
//! server, and talking to an in-process Zenoh router.

use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use http::uri::PathAndQuery;
use middleware::Key;
use middleware::grpc::{HealthClient, RawCodec, ServingStatus};
use middleware::transport::{
    Flow, Gather, HandlerError, Publisher, Request, RequestOptions, Requester, SenderId, Target,
    TraceParent, handler_fn,
};
use mw_service::config::parse_service_map;
use mw_service::run::node::pod_node_id;
use mw_service::{Config, Ctx, RunOptions, Service, ServiceError, ServiceHandler};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tonic::transport::Channel;

#[path = "../../tests/dns_sd/stub.rs"]
#[allow(dead_code)]
mod dns_stub;

#[path = "runner/otlp_stub.rs"]
mod otlp_stub;

const CALLER: &str = "spiffe://infra.local/ns/apps/sa/demo-caller";
const TP: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

/// What Envoy puts in `x-forwarded-client-cert` for a caller.
fn xfcc(principal: &str) -> String {
    format!("By=spiffe://infra.local/ns/apps/sa/demo-echo;Hash=00;URI={principal}")
}

fn channel(addr: SocketAddr) -> Channel {
    Channel::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect_lazy()
}

/// One unary call on `/kind` with optional `x-forwarded-client-cert`,
/// `traceparent` and `x-request-id`.
async fn call(
    addr: SocketAddr,
    kind: &str,
    body: &[u8],
    principal: Option<&str>,
    traceparent: Option<&str>,
) -> Result<Bytes, tonic::Status> {
    let mut grpc = tonic::client::Grpc::new(channel(addr));
    grpc.ready()
        .await
        .map_err(|e| tonic::Status::unavailable(e.to_string()))?;
    let mut req = tonic::Request::new(Bytes::copy_from_slice(body));
    if let Some(p) = principal {
        req.metadata_mut()
            .insert("x-forwarded-client-cert", xfcc(p).parse().unwrap());
    }
    if let Some(t) = traceparent {
        req.metadata_mut().insert("traceparent", t.parse().unwrap());
    }
    req.metadata_mut()
        .insert("x-request-id", "test-req-1".parse().unwrap());
    req.metadata_mut()
        .insert("grpc-timeout", "5S".parse().unwrap());
    let path = PathAndQuery::try_from(format!("/{kind}")).unwrap();
    grpc.unary(req, path, RawCodec)
        .await
        .map(tonic::Response::into_inner)
}

/// Waits until `name`'s health on `addr` is `want`.
async fn until_health(addr: SocketAddr, name: &str, want: ServingStatus) {
    let mut h = HealthClient::new(channel(addr));
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if h.check(name).await.ok() == Some(want) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{name} never became {want:?}"));
}

/// A running service: its address, its shutdown trigger and its run.
struct Running {
    addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    run: JoinHandle<Result<(), ServiceError>>,
}

impl Running {
    async fn start(service: Service, config: Config, options: RunOptions) -> Running {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let run =
            tokio::spawn(
                service.run_with_options(config, options.listener(listener), async {
                    let _ = stopped.await;
                }),
            );
        until_health(addr, "", ServingStatus::Serving).await;
        Running {
            addr,
            stop: Some(stop),
            run,
        }
    }

    fn shutdown(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
    }

    async fn stopped(self) -> Result<(), ServiceError> {
        tokio::time::timeout(Duration::from_secs(15), self.run)
            .await
            .expect("stops in bounded time")
            .expect("run task")
    }
}

/// Answers `<body>` after `delay_ms` when the body is `slow`, at once
/// otherwise; reports the principal it saw.
struct Slow;

impl ServiceHandler for Slow {
    async fn handle(
        &mut self,
        ctx: &mut Ctx<'_>,
        req: &Request<'_>,
        out: &mut [u8],
    ) -> Result<usize, HandlerError> {
        if req.body == b"slow" {
            tokio::time::sleep(Duration::from_millis(600)).await;
        }
        let text = format!(
            "{}|{}",
            String::from_utf8_lossy(req.body),
            ctx.meta().principal.unwrap_or("-")
        );
        let dst = out
            .get_mut(..text.len())
            .ok_or(HandlerError::BufferTooSmall)?;
        dst.copy_from_slice(text.as_bytes());
        Ok(text.len())
    }
}

/// M6: health, in-process `allow_callers`, routing, and the drain: health
/// goes NOT_SERVING at once on shutdown, requests are still answered
/// during the grace, the one in flight completes, then the server stops.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn m6_serves_admitted_callers_and_drains_on_shutdown() {
    let grace = Duration::from_millis(800);
    let service = Service::builder("demo/echo")
        .serve("demo/echo", Slow)
        .allow_callers([CALLER])
        .drain_grace(grace)
        .build()
        .unwrap();
    let config = Config {
        namespace: Some("apps".into()),
        pod_name: Some("demo-echo-0".into()),
        ..Config::default()
    };
    let mut running = Running::start(service, config, RunOptions::new()).await;
    let addr = running.addr;
    until_health(addr, "demo/echo", ServingStatus::Serving).await;

    // The node id is in the health reply.
    let mut h = HealthClient::new(channel(addr));
    let (status, node) = h.check_node("demo/echo").await.unwrap();
    assert_eq!(status, ServingStatus::Serving);
    assert_eq!(node, Some(pod_node_id(Some("apps"), "demo-echo-0")));

    // Admitted, denied, unidentified, unknown.
    let ok = call(addr, "demo/echo", b"hi", Some(CALLER), None)
        .await
        .unwrap();
    assert_eq!(&ok[..], format!("hi|{CALLER}").as_bytes());
    let other = "spiffe://infra.local/ns/apps/sa/intruder";
    let e = call(addr, "demo/echo", b"hi", Some(other), None)
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::PermissionDenied);
    let e = call(addr, "demo/echo", b"hi", None, None)
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::PermissionDenied);
    let e = call(addr, "demo/nope", b"hi", Some(CALLER), None)
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::Unimplemented);

    // Drain: a slow call in flight, a health watch open, then shutdown.
    let mut watch = h.watch("demo/echo").await.unwrap();
    assert_eq!(watch.next().await.unwrap(), ServingStatus::Serving);
    let slow = tokio::spawn(call(addr, "demo/echo", b"slow", Some(CALLER), None));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let asked = tokio::time::Instant::now();
    running.shutdown();
    let flipped = tokio::time::timeout(Duration::from_millis(300), watch.next())
        .await
        .expect("NOT_SERVING at once")
        .unwrap();
    assert_eq!(flipped, ServingStatus::NotServing);
    assert!(
        !running.run.is_finished(),
        "still running after NOT_SERVING"
    );
    // Still answering during the grace.
    let during = call(addr, "demo/echo", b"late", Some(CALLER), None)
        .await
        .unwrap();
    assert_eq!(&during[..], format!("late|{CALLER}").as_bytes());
    assert_eq!(h.check("").await.unwrap(), ServingStatus::NotServing);
    // The in-flight call completes.
    let slow = slow.await.unwrap().unwrap();
    assert_eq!(&slow[..], format!("slow|{CALLER}").as_bytes());
    // The watch ends when serving stops; the run ends cleanly after the
    // grace.
    let ended = tokio::time::timeout(Duration::from_secs(5), watch.next()).await;
    assert!(matches!(ended, Ok(Err(_))), "{ended:?}");
    running.stopped().await.expect("clean drain");
    assert!(asked.elapsed() >= grace, "drained for the grace");
    // Nothing listens any more.
    let e = call(addr, "demo/echo", b"hi", Some(CALLER), None)
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::Unavailable);
}

/// Answers with the span context it was given (`Ctx::trace`).
struct ReportTrace;

impl ServiceHandler for ReportTrace {
    async fn handle(
        &mut self,
        ctx: &mut Ctx<'_>,
        req: &Request<'_>,
        out: &mut [u8],
    ) -> Result<usize, HandlerError> {
        if req.body == b"fail" {
            return Err(HandlerError::Internal);
        }
        let text = ctx.trace().map_or("-".to_owned(), |t| t.to_string());
        let dst = out
            .get_mut(..text.len())
            .ok_or(HandlerError::BufferTooSmall)?;
        dst.copy_from_slice(text.as_bytes());
        Ok(text.len())
    }
}

/// M7: one server span per request exported over OTLP/gRPC: parent is
/// the inbound traceparent, id is what the handler got, attributes and
/// resource as documented; flushed at shutdown.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn m7_server_spans_reach_the_otlp_collector() {
    let (collector, mut spans) = otlp_stub::start().await;
    let service = Service::builder("demo/echo")
        .serve("demo/echo", ReportTrace)
        .allow_callers([CALLER])
        .drain_grace(Duration::ZERO)
        .build()
        .unwrap();
    let config = Config {
        namespace: Some("apps".into()),
        pod_name: Some("demo-echo-0".into()),
        otlp_endpoint: Some(format!("http://{collector}")),
        ..Config::default()
    };
    let mut running = Running::start(service, config, RunOptions::new()).await;
    let inbound = TraceParent::parse(TP).unwrap();
    let reply = call(running.addr, "demo/echo", b"x", Some(CALLER), Some(TP))
        .await
        .unwrap();
    let handed = TraceParent::parse(std::str::from_utf8(&reply).unwrap()).expect("Ctx::trace");
    assert_eq!(handed.trace_id(), inbound.trace_id());
    assert_ne!(handed.span_id(), inbound.span_id());
    let root = call(running.addr, "demo/echo", b"y", Some(CALLER), None)
        .await
        .unwrap();
    let root = TraceParent::parse(std::str::from_utf8(&root).unwrap()).unwrap();
    let e = call(running.addr, "demo/echo", b"fail", Some(CALLER), Some(TP))
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::Internal);
    running.shutdown();
    running.stopped().await.unwrap();

    // Flushed at shutdown: all three spans are at the collector.
    let mut got = Vec::new();
    while got.len() < 3 {
        let s = tokio::time::timeout(Duration::from_secs(5), spans.recv())
            .await
            .expect("spans exported")
            .unwrap();
        got.push(s);
    }
    let child = got
        .iter()
        .find(|s| s.span_id == handed.span_id())
        .expect("the handler's span id is the server span's");
    assert_eq!(child.trace_id, inbound.trace_id());
    assert_eq!(
        child.parent_span_id,
        inbound.span_id(),
        "parent is the caller"
    );
    assert_eq!(child.name, "demo/echo");
    assert_eq!(child.kind, 2, "SPAN_KIND_SERVER");
    assert_eq!(child.attribute("rpc.method"), Some("demo/echo"));
    assert_eq!(child.attribute("rpc.system"), Some("grpc"));
    assert_eq!(child.attribute("mw.kind"), Some("demo/echo"));
    assert_eq!(child.attribute("mw.principal"), Some(CALLER));
    assert_eq!(child.attribute("mw.request_id"), Some("test-req-1"));
    assert_eq!(child.resource("service.name"), Some("demo/echo"));
    assert_eq!(child.resource("k8s.namespace.name"), Some("apps"));
    assert_eq!(child.resource("k8s.pod.name"), Some("demo-echo-0"));
    let new_root = got
        .iter()
        .find(|s| s.span_id == root.span_id())
        .expect("root span");
    assert!(new_root.parent_span_id.is_empty(), "no inbound: a root");
    assert_eq!(new_root.trace_id, root.trace_id());
    let failed = got
        .iter()
        .find(|s| s.status == 2)
        .expect("failed request: status error");
    assert_eq!(failed.parent_span_id, inbound.span_id());
}

/// An echo pod: answers `<pod>|<traceparent it saw>`.
fn echo_pod(pod: &'static str, grace: Duration) -> Service {
    Service::builder("demo/echo")
        .serve(
            "demo/echo",
            handler_fn(move |r, out| {
                let text = format!(
                    "{pod}|{}",
                    r.meta.trace.map_or("-".to_owned(), |t| t.to_string())
                );
                let dst = out
                    .get_mut(..text.len())
                    .ok_or(HandlerError::BufferTooSmall)?;
                dst.copy_from_slice(text.as_bytes());
                Ok(text.len())
            }),
        )
        .drain_grace(grace)
        .build()
        .unwrap()
}

/// `demo/hello`: `gather` lists every echo pod's node and answer; `node:<hex>`
/// asks that node; anything else asks any pod and adds its own span.
struct Hello;

impl ServiceHandler for Hello {
    async fn handle(
        &mut self,
        ctx: &mut Ctx<'_>,
        req: &Request<'_>,
        out: &mut [u8],
    ) -> Result<usize, HandlerError> {
        const ECHO: Key<'static> = Key::from_static("demo/echo");
        let own = ctx.trace().map_or("-".to_owned(), |t| t.to_string());
        let mut down = ctx.client("demo/echo").ok_or(HandlerError::Unavailable)?;
        let mut buf = [0u8; 256];
        let text = if req.body == b"gather" {
            let mut lines = Vec::new();
            down.request_each(ECHO, b"", &mut buf, &RequestOptions::DEFAULT, 8, |r| {
                let node = r.sender.map_or("-".to_owned(), |s| hex(&s.0));
                let body = r.result.map_or_else(
                    |e| format!("{e:?}"),
                    |b| String::from_utf8_lossy(b).into_owned(),
                );
                lines.push(format!("{node} {body}"));
                Flow::Continue
            })
            .await
            .map_err(|_| HandlerError::Unavailable)?;
            lines.sort();
            lines.join("\n")
        } else if let Some(node) = req.body.strip_prefix(b"node:") {
            let id =
                middleware::grpc::health::parse_node(node).ok_or(HandlerError::InvalidRequest)?;
            let opts = RequestOptions::DEFAULT.with_target(Target::Node(id));
            let r = down
                .request_with(ECHO, b"", &mut buf, &opts)
                .await
                .map_err(|e| e.handler_error().unwrap_or(HandlerError::Internal))?;
            String::from_utf8_lossy(r).into_owned()
        } else {
            let r = down
                .request(ECHO, b"", &mut buf)
                .await
                .map_err(|e| e.handler_error().unwrap_or(HandlerError::Internal))?;
            format!("{own}\n{}", String::from_utf8_lossy(r))
        };
        let dst = out
            .get_mut(..text.len())
            .ok_or(HandlerError::BufferTooSmall)?;
        dst.copy_from_slice(text.as_bytes());
        Ok(text.len())
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Polls `demo/hello` with `body` until `ok` accepts the answer.
async fn until_hello(addr: SocketAddr, body: &[u8], ok: impl Fn(&str) -> bool) -> String {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(r) = call(addr, "demo/hello", body, None, None).await {
                let text = String::from_utf8_lossy(&r).into_owned();
                if ok(&text) {
                    return text;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("expected answer in time")
}

/// M8/M9: a caller discovers two echo pods through the (stub) cluster DNS,
/// sees each one's node id from its health header, gathers across both,
/// targets one by node, continues the trace, and drops a draining pod
/// before it stops.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn m8_m9_calls_discover_pods_by_dns_and_target_them_by_node() {
    let stub = dns_stub::Stub::start().await;
    let quiet = || RunOptions::new().without_sidecar();
    let pod_config = |pod: &str| Config {
        namespace: Some("apps".into()),
        pod_name: Some(pod.into()),
        ..Config::default()
    };
    let echo0 = Running::start(
        echo_pod("echo-0", Duration::ZERO),
        pod_config("echo-0"),
        quiet(),
    )
    .await;
    let mut echo1 = Running::start(
        echo_pod("echo-1", Duration::from_secs(2)),
        pod_config("echo-1"),
        quiet(),
    )
    .await;
    let suffix = "demo-echo.apps.svc.cluster.local";
    let srv = "_grpc._tcp.demo-echo.apps.svc.cluster.local";
    stub.set_pods(
        srv,
        vec![
            dns_stub::Pod::v4("echo-0", suffix, [127, 0, 0, 1], echo0.addr.port(), 30),
            dns_stub::Pod::v4("echo-1", suffix, [127, 0, 0, 1], echo1.addr.port(), 30),
        ],
    )
    .await;
    let caller = Service::builder("demo/caller")
        .serve("demo/hello", Hello)
        .calls("demo/echo")
        .drain_grace(Duration::ZERO)
        .build()
        .unwrap();
    let caller_config = Config {
        service_map: parse_service_map("demo/echo=demo-echo.apps:grpc").unwrap(),
        ..Config::default()
    };
    let fast = middleware::dns::unicast::Schedule::new(50, 200)
        .empty_interval(50)
        .error_interval(50);
    let mut caller = Running::start(
        caller,
        caller_config,
        quiet().nameserver(stub.addr).dns_schedule(fast),
    )
    .await;

    let node0 = pod_node_id(Some("apps"), "echo-0");
    let node1 = pod_node_id(Some("apps"), "echo-1");
    let both = until_hello(caller.addr, b"gather", |t| t.lines().count() == 2).await;
    let mut expected = vec![
        format!("{} echo-0|", hex(&node0.0)),
        format!("{} echo-1|", hex(&node1.0)),
    ];
    expected.sort();
    for (line, want) in both.lines().zip(&expected) {
        assert!(line.starts_with(want.as_str()), "{both}");
    }
    // Target::Node through DNS-discovered endpoints.
    for (node, pod) in [(node1, "echo-1"), (node0, "echo-0"), (node1, "echo-1")] {
        let body = format!("node:{}", hex(&node.0));
        let r = call(caller.addr, "demo/hello", body.as_bytes(), None, None)
            .await
            .unwrap();
        assert!(r.starts_with(format!("{pod}|").as_bytes()), "{r:?}");
    }
    let unknown = format!("node:{}", hex(&SenderId([9; 8]).0));
    let e = call(caller.addr, "demo/hello", unknown.as_bytes(), None, None)
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::Unavailable);
    // The trace continues: echo's parent is the caller's server span.
    let r = call(caller.addr, "demo/hello", b"one", None, Some(TP))
        .await
        .unwrap();
    let text = String::from_utf8(r.to_vec()).unwrap();
    let (own, echoed) = text.split_once('\n').unwrap();
    let own = TraceParent::parse(own).unwrap();
    let seen = TraceParent::parse(echoed.split_once('|').unwrap().1).unwrap();
    assert_eq!(own.trace_id(), TraceParent::parse(TP).unwrap().trace_id());
    assert_eq!(seen.trace_id(), own.trace_id());
    assert_eq!(
        seen.span_id(),
        own.span_id(),
        "echo's parent is the caller's span"
    );

    // echo-1 drains: it leaves the caller's directory while it still runs.
    echo1.shutdown();
    let one = until_hello(caller.addr, b"gather", |t| t.lines().count() == 1).await;
    assert!(one.contains("echo-0|"), "{one}");
    assert!(!echo1.run.is_finished(), "dropped before it stopped");
    echo1.stopped().await.unwrap();
    caller.shutdown();
    caller.stopped().await.unwrap();
    let mut echo0 = echo0;
    echo0.shutdown();
    echo0.stopped().await.unwrap();
}

/// Picks a free port on 127.0.0.1 (bound, read, released).
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// An in-process Zenoh router on 127.0.0.1 accepting `user:password`
/// from `dictionary`.
async fn zenoh_router(dictionary: &std::path::Path) -> (String, middleware::zenoh::ZenohTransport) {
    let locator = format!("tcp/127.0.0.1:{}", free_port());
    let mut config = zenoh::Config::default();
    config.insert_json5("mode", r#""router""#).unwrap();
    config
        .insert_json5("scouting/multicast/enabled", "false")
        .unwrap();
    config
        .insert_json5("listen/endpoints", &format!(r#"["{locator}"]"#))
        .unwrap();
    config
        .insert_json5(
            "transport/auth/usrpwd/dictionary_file",
            &format!("\"{}\"", dictionary.display()),
        )
        .unwrap();
    let router = middleware::zenoh::ZenohTransport::open(config)
        .await
        .unwrap();
    (locator, router)
}

/// Publishes its body on `app/demo/out`; `undeclared` tries `sys/x`.
struct Pub;

impl ServiceHandler for Pub {
    async fn handle(
        &mut self,
        ctx: &mut Ctx<'_>,
        req: &Request<'_>,
        _out: &mut [u8],
    ) -> Result<usize, HandlerError> {
        let mut p = ctx.publisher().ok_or(HandlerError::Unavailable)?;
        let key = if req.body == b"undeclared" {
            Key::from_static("sys/x")
        } else {
            Key::from_static("app/demo/out")
        };
        p.publish(key, req.body).await.map_err(|e| match e {
            mw_service::PublishError::Undeclared => HandlerError::Denied,
            _ => HandlerError::Unavailable,
        })?;
        Ok(0)
    }
}

/// M10 (runner side): a service that publishes and subscribes opens an
/// authenticated client session to the router from `MW_ZENOH_ROUTER` /
/// `MW_ZENOH_CREDENTIALS`; publishes are limited to `publishes`, samples
/// of `on_sample` filters reach the hook; wrong credentials fail the start.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn m10_zenoh_session_authenticates_publishes_and_subscribes() {
    use middleware::transport::{Subscribe, Subscriber};
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("m10-zenoh");
    std::fs::create_dir_all(&dir).unwrap();
    let dictionary = dir.join("dictionary");
    std::fs::write(&dictionary, "demo-pub:right-password\n").unwrap();
    let good = dir.join("good");
    std::fs::write(&good, "demo-pub:right-password\n").unwrap();
    let bad = dir.join("bad");
    std::fs::write(&bad, "demo-pub:wrong-password\n").unwrap();
    let (locator, mut router) = zenoh_router(&dictionary).await;
    let mut out = router
        .subscribe(middleware::KeyFilter::Exact(Key::from_static(
            "app/demo/out",
        )))
        .await
        .unwrap();

    let build = |tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>| {
        Service::builder("demo/pub")
            .serve("demo/pub", Pub)
            .publishes("app/demo/**")
            .on_sample("app/demo/in/**", move |s| {
                let _ = tx.send(s.payload.to_vec());
            })
            .drain_grace(Duration::ZERO)
            .build()
            .unwrap()
    };
    let config = |creds: &std::path::Path| Config {
        zenoh_router: Some(locator.clone()),
        zenoh_credentials: Some(creds.to_owned()),
        ..Config::default()
    };

    // Wrong password: the run fails before serving.
    let (tx, _) = tokio::sync::mpsc::unbounded_channel();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let e = tokio::time::timeout(
        Duration::from_secs(20),
        build(tx).run_with_options(config(&bad), RunOptions::new().listener(listener), async {}),
    )
    .await
    .expect("fails in bounded time");
    assert!(matches!(e, Err(ServiceError::Zenoh(_))), "{e:?}");
    // No router configured: refused.
    let (tx, _) = tokio::sync::mpsc::unbounded_channel();
    let e = build(tx).run_with(Config::default(), async {}).await;
    assert!(matches!(e, Err(ServiceError::NoZenohRouter)), "{e:?}");

    // Right password: publishes reach the router, samples reach the hook.
    let (tx, mut samples) = tokio::sync::mpsc::unbounded_channel();
    let mut running = Running::start(
        build(tx),
        config(&good),
        RunOptions::new().without_sidecar(),
    )
    .await;
    call(running.addr, "demo/pub", b"hello", None, None)
        .await
        .unwrap();
    let mut buf = [0u8; 64];
    let s = tokio::time::timeout(Duration::from_secs(5), out.recv(&mut buf))
        .await
        .expect("published")
        .unwrap();
    assert_eq!(s.payload, b"hello");
    let e = call(running.addr, "demo/pub", b"undeclared", None, None)
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::PermissionDenied);
    // The hook's subscription is declared after Ready; publish until seen.
    let got = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            router
                .publish(Key::from_static("app/demo/in/pose"), b"p1")
                .await
                .unwrap();
            if let Ok(Some(p)) =
                tokio::time::timeout(Duration::from_millis(100), samples.recv()).await
            {
                return p;
            }
        }
    })
    .await
    .expect("sample reached the hook");
    assert_eq!(got, b"p1");
    running.shutdown();
    running.stopped().await.unwrap();
}
