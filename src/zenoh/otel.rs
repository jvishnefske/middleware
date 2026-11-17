//! Spans and metrics of the Zenoh backend: feature `zenoh-otel`, **off by
//! default**. Without the feature none of this is compiled, the crate does
//! not depend on OpenTelemetry, and the backend's behaviour and wire bytes
//! are exactly those of plain `zenoh`.
//!
//! With it the backend records through the OpenTelemetry **API** only
//! (`opentelemetry::global`): it never installs an exporter. Until the
//! application installs a tracer and meter provider (an SDK with an OTLP
//! exporter, for example `tools/mw-otel`'s `init`) every call is a no-op.
//! Install them before the first transport is used: the instruments are
//! created once, from whatever meter provider is installed then.
//!
//! # Spans (instrumentation scope `middleware.zenoh`)
//!
//! | span | kind | when | parent |
//! |---|---|---|---|
//! | `mw publish <key>` | producer | around each `put` | the caller's current context |
//! | `mw deliver <key>` | consumer | each sample a subscriber hands out (zero length) | the publisher's span, if the attachment carries it (kind 3); else the current context |
//! | `mw request <key>` | client | `request_with` / `request_each`, until the result | the caller's current context |
//! | `mw handle <key>` | server | `serve_one`: handler and reply | the requester's span, if the attachment carries it (kind `0x12`) |
//!
//! Attributes: `mw.key`, `mw.sender` (16 hex digits), `mw.seq`, `mw.stream`,
//! `mw.gap`, `mw.bytes`, `mw.target`, `mw.error` (a [`HandlerError`] or
//! backend error name; the span status is then `Error`).
//!
//! Crossing nodes needs the trace context on the wire, which a transport
//! sends only with `ZenohTransport::with_trace_context(true)` (see
//! `wire` for why it is opt-in). Without it each node's spans are their own
//! traces, still findable by `mw.sender` / `mw.stream` / `mw.seq`.
//!
//! # Metrics (meter `middleware.zenoh`)
//!
//! | instrument | unit | attributes |
//! |---|---|---|
//! | `mw.zenoh.published` counter | `{message}` | `mw.key`, `mw.outcome` |
//! | `mw.zenoh.delivered` counter | `{message}` | `mw.key`, `mw.sender` |
//! | `mw.zenoh.lost` counter: samples known lost before a delivered one ([`SampleMeta::gap`]) | `{message}` | `mw.key`, `mw.sender` |
//! | `mw.zenoh.bytes` counter: payload bytes | `By` | `mw.key`, `mw.direction` (`out`, `in`) |
//! | `mw.zenoh.request.duration` histogram | `s` | `mw.key`, `mw.outcome` |
//! | `mw.zenoh.handle.duration` histogram | `s` | `mw.key`, `mw.outcome` |
//!
//! `mw.key` is the concrete key, so a network with many distinct keys makes
//! many series; that is the point for a debug channel, and the reason this
//! is not on by default.

use std::sync::OnceLock;
use std::time::Instant;

use opentelemetry::metrics::{Counter, Histogram};
use opentelemetry::trace::{
    Span as _, SpanContext, SpanId, SpanKind, Status, TraceContextExt, TraceFlags, TraceId,
    TraceState, Tracer as _,
};
use opentelemetry::{Context, KeyValue, global};

use super::wire::{self, TraceContext};
use crate::transport::{HandlerError, SampleMeta, SenderId, Target};

const SCOPE: &str = "middleware.zenoh";

struct Instruments {
    published: Counter<u64>,
    delivered: Counter<u64>,
    lost: Counter<u64>,
    bytes: Counter<u64>,
    request_duration: Histogram<f64>,
    handle_duration: Histogram<f64>,
}

fn instruments() -> &'static Instruments {
    static INSTRUMENTS: OnceLock<Instruments> = OnceLock::new();
    INSTRUMENTS.get_or_init(|| {
        let m = global::meter(SCOPE);
        Instruments {
            published: m
                .u64_counter("mw.zenoh.published")
                .with_unit("{message}")
                .with_description("Publications put, by key and outcome")
                .build(),
            delivered: m
                .u64_counter("mw.zenoh.delivered")
                .with_unit("{message}")
                .with_description("Samples handed out by subscribers, by key and sender")
                .build(),
            lost: m
                .u64_counter("mw.zenoh.lost")
                .with_unit("{message}")
                .with_description("Samples known lost (sequence gaps) before delivered ones")
                .build(),
            bytes: m
                .u64_counter("mw.zenoh.bytes")
                .with_unit("By")
                .with_description("Payload bytes published (out) and delivered (in)")
                .build(),
            request_duration: m
                .f64_histogram("mw.zenoh.request.duration")
                .with_unit("s")
                .with_description("Requests until their result, by key and outcome")
                .build(),
            handle_duration: m
                .f64_histogram("mw.zenoh.handle.duration")
                .with_unit("s")
                .with_description("Queries served (handler and reply), by key and outcome")
                .build(),
        }
    })
}

/// A handler error's name, as span attributes and metric labels carry it.
pub fn handler_name(e: HandlerError) -> &'static str {
    match e {
        HandlerError::NotFound => "not_found",
        HandlerError::InvalidRequest => "invalid_request",
        HandlerError::BufferTooSmall => "buffer_too_small",
        HandlerError::Internal => "internal",
        HandlerError::Denied => "denied",
        HandlerError::Unavailable => "unavailable",
        HandlerError::DeadlineExceeded => "deadline_exceeded",
    }
}

fn hex16(id: SenderId) -> String {
    wire::zid16(id)
}

/// The trace context to put on the wire for `cx`'s span, if it has a
/// valid one.
pub(crate) fn to_wire(cx: &Context) -> Option<TraceContext> {
    let span = cx.span();
    let sc = span.span_context();
    sc.is_valid().then(|| TraceContext {
        trace_id: sc.trace_id().to_bytes(),
        span_id: sc.span_id().to_bytes(),
        flags: sc.trace_flags().to_u8(),
    })
}

/// A context whose (remote) parent is the span named by `t`.
pub(crate) fn from_wire(t: &TraceContext) -> Context {
    let sc = SpanContext::new(
        TraceId::from_bytes(t.trace_id),
        SpanId::from_bytes(t.span_id),
        TraceFlags::new(t.flags),
        true,
        TraceState::default(),
    );
    Context::new().with_remote_span_context(sc)
}

#[derive(Clone, Copy)]
enum OpKind {
    Publish,
    Request,
    Handle,
}

/// One instrumented operation: its span (as a context) and its start.
pub(crate) struct Op {
    cx: Context,
    start: Instant,
    kind: OpKind,
    key: String,
}

impl Op {
    fn start(kind: OpKind, key: &str, parent: &Context, mut attributes: Vec<KeyValue>) -> Op {
        let (verb, span_kind) = match kind {
            OpKind::Publish => ("publish", SpanKind::Producer),
            OpKind::Request => ("request", SpanKind::Client),
            OpKind::Handle => ("handle", SpanKind::Server),
        };
        attributes.push(KeyValue::new("mw.key", key.to_owned()));
        let span = global::tracer(SCOPE)
            .span_builder(format!("mw {verb} {key}"))
            .with_kind(span_kind)
            .with_attributes(attributes)
            .start_with_context(&global::tracer(SCOPE), parent);
        Op {
            cx: parent.with_span(span),
            start: Instant::now(),
            kind,
            key: key.to_owned(),
        }
    }

    /// A `put` of `bytes` on `key` as `seq` of `stream`, under the
    /// caller's current context.
    pub(crate) fn publish(key: &str, sender: SenderId, seq: u8, stream: u32, bytes: usize) -> Op {
        Op::start(
            OpKind::Publish,
            key,
            &Context::current(),
            vec![
                KeyValue::new("mw.sender", hex16(sender)),
                KeyValue::new("mw.seq", i64::from(seq)),
                KeyValue::new("mw.stream", i64::from(stream)),
                KeyValue::new("mw.bytes", bytes as i64),
            ],
        )
    }

    /// A request for `key` to `target`, under the caller's current context.
    pub(crate) fn request(key: &str, target: Target, bytes: usize) -> Op {
        let target = match target {
            Target::Any => "any".to_owned(),
            Target::Node(id) => hex16(id),
        };
        Op::start(
            OpKind::Request,
            key,
            &Context::current(),
            vec![
                KeyValue::new("mw.target", target),
                KeyValue::new("mw.bytes", bytes as i64),
            ],
        )
    }

    /// Serving a query for `key` from `sender`, continuing the requester's
    /// span when the attachment carried it.
    pub(crate) fn handle(key: &str, sender: Option<SenderId>, trace: Option<TraceContext>) -> Op {
        let parent = trace
            .as_ref()
            .map(from_wire)
            .unwrap_or_else(Context::current);
        let mut attributes = Vec::new();
        if let Some(s) = sender {
            attributes.push(KeyValue::new("mw.sender", hex16(s)));
        }
        Op::start(OpKind::Handle, key, &parent, attributes)
    }

    /// The trace context to send with this operation.
    pub(crate) fn trace(&self) -> Option<TraceContext> {
        to_wire(&self.cx)
    }

    /// Ends the span, `error` naming what went wrong, and records the
    /// operation's metrics.
    pub(crate) fn end(self, bytes: usize, error: Option<&str>) {
        let outcome = error.unwrap_or("ok").to_owned();
        let span = self.cx.span();
        if let Some(e) = error {
            span.set_attribute(KeyValue::new("mw.error", e.to_owned()));
            span.set_status(Status::error(e.to_owned()));
        }
        span.end();
        let i = instruments();
        let attrs = [
            KeyValue::new("mw.key", self.key.clone()),
            KeyValue::new("mw.outcome", outcome),
        ];
        let secs = self.start.elapsed().as_secs_f64();
        match self.kind {
            OpKind::Publish => {
                i.published.add(1, &attrs);
                if error.is_none() {
                    i.bytes.add(
                        bytes as u64,
                        &[
                            KeyValue::new("mw.key", self.key),
                            KeyValue::new("mw.direction", "out"),
                        ],
                    );
                }
            }
            OpKind::Request => i.request_duration.record(secs, &attrs),
            OpKind::Handle => i.handle_duration.record(secs, &attrs),
        }
    }
}

/// Records a sample a subscriber hands out: a zero-length consumer span
/// (child of the publisher's span when `trace` names it) and the delivery
/// metrics.
pub(crate) fn deliver(key: &str, meta: &SampleMeta, trace: Option<TraceContext>, bytes: usize) {
    let parent = trace
        .as_ref()
        .map(from_wire)
        .unwrap_or_else(Context::current);
    let sender = meta
        .sender
        .map(hex16)
        .unwrap_or_else(|| "unknown".to_owned());
    let mut attributes = vec![
        KeyValue::new("mw.key", key.to_owned()),
        KeyValue::new("mw.sender", sender.clone()),
        KeyValue::new("mw.bytes", bytes as i64),
    ];
    if let Some(seq) = meta.seq {
        attributes.push(KeyValue::new("mw.seq", i64::from(seq)));
    }
    if let Some(gap) = meta.gap {
        attributes.push(KeyValue::new("mw.gap", i64::from(gap)));
    }
    let tracer = global::tracer(SCOPE);
    let mut span = tracer
        .span_builder(format!("mw deliver {key}"))
        .with_kind(SpanKind::Consumer)
        .with_attributes(attributes)
        .start_with_context(&tracer, &parent);
    span.end();
    let i = instruments();
    let attrs = [
        KeyValue::new("mw.key", key.to_owned()),
        KeyValue::new("mw.sender", sender),
    ];
    i.delivered.add(1, &attrs);
    if let Some(gap @ 1..) = meta.gap {
        i.lost.add(u64::from(gap), &attrs);
    }
    i.bytes.add(
        bytes as u64,
        &[
            KeyValue::new("mw.key", key.to_owned()),
            KeyValue::new("mw.direction", "in"),
        ],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contexts_cross_the_wire_unchanged() {
        let t = TraceContext {
            trace_id: [7; 16],
            span_id: [9; 8],
            flags: 1,
        };
        let cx = from_wire(&t);
        assert!(cx.span().span_context().is_remote());
        assert_eq!(to_wire(&cx), Some(t));
        // No span: nothing to send.
        assert_eq!(to_wire(&Context::new()), None);
    }
}
