//! The Zenoh backend's spans and metrics (feature `zenoh-otel`), recorded
//! by the OpenTelemetry SDK's in-memory exporters over two real sessions on
//! one loopback endpoint (as `tests/zenoh.rs`: explicit `127.0.0.1` port,
//! no scouting).
//!
//! One test function: the tracer and meter providers are process globals,
//! so the checks run in sequence against one pair of providers.
#![cfg(feature = "zenoh-otel")]

use std::time::Duration;

use middleware::zenoh::{SessionConfig, ZenohTransport};
use middleware::{
    HandlerError, Key, KeyFilter, Publisher, Request, RequestOptions, Requester, Responder,
    Subscribe, Subscriber, TraceParent,
};
use opentelemetry::trace::{SpanKind, Status, TraceContextExt, Tracer, TracerProvider as _};
use opentelemetry::{Context, Value, global};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};

const TIMEOUT: Duration = Duration::from_secs(10);

async fn free_loopback_port() -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    l.local_addr().expect("addr").port()
}

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
    // Let the sessions see each other's declarations.
    tokio::time::sleep(Duration::from_millis(300)).await;
    (a, b)
}

fn attr<'a>(s: &'a SpanData, key: &str) -> Option<&'a Value> {
    s.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| &kv.value)
}

fn named<'a>(spans: &'a [SpanData], name: &str) -> Vec<&'a SpanData> {
    spans.iter().filter(|s| s.name == name).collect()
}

fn reverse(r: &Request<'_>, resp: &mut [u8]) -> Result<usize, HandlerError> {
    if r.body.is_empty() {
        return Err(HandlerError::InvalidRequest);
    }
    let out = resp
        .get_mut(..r.body.len())
        .ok_or(HandlerError::BufferTooSmall)?;
    out.copy_from_slice(r.body);
    out.reverse();
    Ok(r.body.len())
}

#[tokio::test(flavor = "multi_thread")]
async fn ot1_spans_and_metrics_cross_two_sessions() {
    let spans = InMemorySpanExporter::default();
    let tracer_provider = SdkTracerProvider::builder()
        .with_simple_exporter(spans.clone())
        .build();
    global::set_tracer_provider(tracer_provider.clone());
    let metrics = InMemoryMetricExporter::default();
    let meter_provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(metrics.clone()).build())
        .build();
    global::set_meter_provider(meter_provider.clone());
    let test_tracer = tracer_provider.tracer("test");

    let (a, b) = loopback_pair().await;
    let mut traced = a.clone().with_trace_context(true);
    let mut plain = a.clone();
    let mut b = b;

    // ---- OT1: a traced publication's delivery continues the publisher's trace.
    const K: Key<'static> = Key::from_static("test/ot/temp");
    let mut sub = b.subscribe(KeyFilter::Exact(K)).await.expect("subscribe");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let root = test_tracer.start("ot1 root");
    let root_cx = Context::current_with_span(root);
    let trace_id = root_cx.span().span_context().trace_id();
    {
        let _g = root_cx.clone().attach();
        traced.publish(K, b"21.5").await.expect("publish");
    }
    let mut buf = [0u8; 64];
    let s = tokio::time::timeout(TIMEOUT, sub.recv(&mut buf))
        .await
        .expect("timeout")
        .expect("recv");
    assert_eq!(s.payload, b"21.5");

    let all = spans.get_finished_spans().expect("spans");
    let publish = named(&all, "mw publish test/ot/temp");
    let deliver = named(&all, "mw deliver test/ot/temp");
    assert_eq!((publish.len(), deliver.len()), (1, 1), "{all:#?}");
    let (p, d) = (publish[0], deliver[0]);
    assert_eq!(p.span_kind, SpanKind::Producer);
    assert_eq!(d.span_kind, SpanKind::Consumer);
    assert_eq!(
        p.span_context.trace_id(),
        trace_id,
        "publish under the caller's span"
    );
    assert_eq!(
        d.span_context.trace_id(),
        trace_id,
        "delivery in the same trace"
    );
    assert_eq!(
        d.parent_span_id,
        p.span_context.span_id(),
        "delivery is the publish's child"
    );
    assert_eq!(attr(p, "mw.key"), Some(&Value::from("test/ot/temp")));
    assert_eq!(attr(d, "mw.seq"), Some(&Value::I64(0)));
    assert_eq!(attr(p, "mw.sender"), attr(d, "mw.sender"), "{p:?} {d:?}");

    // ---- OT2: without with_trace_context nothing crosses: a new trace.
    let root = test_tracer.start("ot2 root");
    let root_cx = Context::current_with_span(root);
    {
        let _g = root_cx.clone().attach();
        plain.publish(K, b"22.0").await.expect("publish");
    }
    let s = tokio::time::timeout(TIMEOUT, sub.recv(&mut buf))
        .await
        .expect("timeout")
        .expect("recv");
    assert_eq!(s.payload, b"22.0");
    let all = spans.get_finished_spans().expect("spans");
    let deliver = named(&all, "mw deliver test/ot/temp");
    let last = deliver.last().expect("second delivery");
    assert_ne!(
        last.span_context.trace_id(),
        root_cx.span().span_context().trace_id(),
        "an untraced attachment carries no context"
    );

    // ---- OT3: a traced request's handling continues the requester's trace;
    // a handler error marks both ends.
    const R: Key<'static> = Key::from_static("test/ot/reverse");
    let mut responder = b.responder(R).await.expect("responder");
    let server = tokio::spawn(async move {
        let mut scratch = [0u8; 64];
        let mut h = reverse;
        responder
            .serve_one(&mut h, &mut scratch)
            .await
            .expect("serve ok");
        responder
            .serve_one(&mut h, &mut scratch)
            .await
            .expect("serve err");
        responder
            .serve_one(&mut h, &mut scratch)
            .await
            .expect("serve K18");
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let root = test_tracer.start("ot3 root");
    let root_cx = Context::current_with_span(root);
    let trace_id = root_cx.span().span_context().trace_id();
    let mut resp = [0u8; 16];
    {
        let _g = root_cx.clone().attach();
        let out = traced.request(R, b"abc", &mut resp).await.expect("request");
        assert_eq!(out, b"cba");
        assert!(traced.request(R, b"", &mut resp).await.is_err());
    }
    // The query's trace rides in the K18 trace extension, so a plain
    // transport's explicit `RequestOptions::trace` (no current span) is the
    // request span's parent and reaches the handler's span the same way.
    let k18 = TraceParent::parse("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01")
        .expect("spec traceparent");
    let out = plain
        .request_with(
            R,
            b"xy",
            &mut resp,
            &RequestOptions::DEFAULT.with_trace(k18),
        )
        .await
        .expect("K18 request");
    assert_eq!(out, b"yx");
    server.await.expect("server task");
    let all = spans.get_finished_spans().expect("spans");
    let requests = named(&all, "mw request test/ot/reverse");
    let handles = named(&all, "mw handle test/ot/reverse");
    assert_eq!((requests.len(), handles.len()), (3, 3), "{all:#?}");
    let k18_trace = opentelemetry::trace::TraceId::from_bytes(k18.trace_id());
    assert_eq!(requests[2].span_context.trace_id(), k18_trace);
    assert_eq!(
        requests[2].parent_span_id,
        opentelemetry::trace::SpanId::from_bytes(k18.span_id())
    );
    assert_eq!(handles[2].span_context.trace_id(), k18_trace);
    for (r, h) in requests.iter().zip(&handles).take(2) {
        assert_eq!(r.span_kind, SpanKind::Client);
        assert_eq!(h.span_kind, SpanKind::Server);
        assert_eq!(r.span_context.trace_id(), trace_id);
        assert_eq!(h.span_context.trace_id(), trace_id);
        assert_eq!(h.parent_span_id, r.span_context.span_id());
    }
    assert_eq!(
        attr(handles[1], "mw.error"),
        Some(&Value::from("invalid_request"))
    );
    assert!(matches!(handles[1].status, Status::Error { .. }));
    assert_eq!(
        attr(requests[1], "mw.error"),
        Some(&Value::from("invalid_request"))
    );
    assert_eq!(handles[0].status, Status::Unset);

    // ---- OT4: the metrics exist under their documented names.
    meter_provider.force_flush().expect("flush");
    let exported = metrics.get_finished_metrics().expect("metrics");
    let mut names: Vec<String> = exported
        .iter()
        .flat_map(|rm| rm.scope_metrics())
        .filter(|sm| sm.scope().name() == "middleware.zenoh")
        .flat_map(|sm| sm.metrics())
        .map(|m| m.name().to_owned())
        .collect();
    names.sort();
    names.dedup();
    for want in [
        "mw.zenoh.bytes",
        "mw.zenoh.delivered",
        "mw.zenoh.handle.duration",
        "mw.zenoh.published",
        "mw.zenoh.request.duration",
    ] {
        assert!(
            names.iter().any(|n| n == want),
            "{want} missing from {names:?}"
        );
    }
}
