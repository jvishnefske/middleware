//! Logs and traces of a run.
//!
//! # Logs
//!
//! [`init_logs`] installs a process-wide `tracing` subscriber writing one
//! JSON object per line to stdout (Alloy ships container stdout to Loki),
//! filtered by `RUST_LOG` (default [`DEFAULT_LOG_FILTER`]). Only
//! [`Service::run`](crate::Service::run) installs it: a process embedding
//! the runner, or a test calling `run_with`, keeps its own subscriber.
//! The runner logs its lifecycle at `info` and one line per request
//! (target `mw_service::request`: kind, outcome, duration, trace and span
//! id, request id, principal), so Loki lines join Tempo traces by
//! `trace_id`.
//!
//! # Traces
//!
//! With `OTEL_EXPORTER_OTLP_ENDPOINT` set, [`Telemetry::otlp`] builds an
//! OpenTelemetry tracer provider owned by the run (not the global one, so
//! runs in one process stay apart) exporting over OTLP/gRPC (tonic, batch
//! processor) with the resource attributes of [`resource_attributes`]:
//! `service.name` (the spec's name), and `k8s.namespace.name` /
//! `k8s.pod.name` when the downward API sets them. [`Telemetry::shutdown`]
//! flushes, bounded by a timeout.
//!
//! Each request gets one server span ([`Spans::start`]):
//!
//! * its parent is the caller's `traceparent` ([`RequestMeta::trace`]),
//!   entered as a remote span context, so the span joins the caller's
//!   trace; without one it starts a new trace;
//! * its own trace and span id are what the handler gets as
//!   [`Ctx::trace`](crate::Ctx::trace), so every downstream call made
//!   through the context is its child;
//! * it carries [`span_attributes`]: `rpc.system`, `rpc.service`,
//!   `rpc.method` (the kind), `mw.kind`, and `mw.principal` /
//!   `mw.request_id` when the request has them; it ends with status error
//!   (the `HandlerError`) when the handler fails.
//!
//! Without an endpoint no span is exported, but the handler still gets a
//! span context ([`server_trace`] with random ids), so trace ids
//! propagate through services that do not export.

use std::borrow::Cow;
use std::time::Duration;

use middleware::transport::{HandlerError, RequestMeta, TraceParent};
use opentelemetry::trace::{
    Span as _, SpanContext, SpanId, SpanKind, Status, TraceContextExt, TraceFlags, TraceId,
    TraceState, Tracer as _, TracerProvider as _,
};
use opentelemetry::{Context, KeyValue};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::{
    IdGenerator, RandomIdGenerator, SdkTracer, SdkTracerProvider, Span,
};

use crate::config::Config;
use crate::ctx::server_trace;
use crate::error::ServiceError;

/// The log filter without `RUST_LOG`.
pub const DEFAULT_LOG_FILTER: &str = "info,zenoh=warn,h2=warn,tower=warn,hyper=warn";

/// Installs the JSON stdout subscriber (module docs); a no-op if the
/// process already has one.
pub fn init_logs() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(DEFAULT_LOG_FILTER));
    let _ = tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_writer(std::io::stdout)
        .with_current_span(false)
        .with_span_list(false)
        .try_init();
}

/// The resource attributes of a run of the service `name` with `config`
/// (module docs). Pure.
pub fn resource_attributes(name: &str, config: &Config) -> Vec<(&'static str, String)> {
    let mut out = vec![("service.name", name.to_owned())];
    if let Some(ns) = &config.namespace {
        out.push(("k8s.namespace.name", ns.clone()));
    }
    if let Some(pod) = &config.pod_name {
        out.push(("k8s.pod.name", pod.clone()));
    }
    out
}

/// The attributes of the server span of a request on `kind` with `meta`
/// (module docs). Pure.
pub fn span_attributes(kind: &str, meta: &RequestMeta<'_>) -> Vec<(&'static str, String)> {
    let service = kind.split_once('/').map_or(kind, |(s, _)| s);
    let mut out = vec![
        ("rpc.system", "grpc".to_owned()),
        ("rpc.service", service.to_owned()),
        ("rpc.method", kind.to_owned()),
        ("mw.kind", kind.to_owned()),
    ];
    if let Some(p) = meta.principal {
        out.push(("mw.principal", p.to_owned()));
    }
    if let Some(r) = meta.request_id {
        out.push(("mw.request_id", r.as_str().to_owned()));
    }
    out
}

/// The remote span context of an inbound `traceparent`.
fn remote_context(parent: TraceParent) -> Context {
    Context::new().with_remote_span_context(SpanContext::new(
        TraceId::from_bytes(parent.trace_id()),
        SpanId::from_bytes(parent.span_id()),
        TraceFlags::new(parent.flags()),
        true,
        TraceState::default(),
    ))
}

/// What a run exports traces through: nothing, or an OTLP provider.
#[derive(Debug)]
pub struct Telemetry {
    provider: Option<SdkTracerProvider>,
}

impl Telemetry {
    /// No export.
    pub fn off() -> Self {
        Telemetry { provider: None }
    }

    /// OTLP/gRPC export to `endpoint` with `resource` (from
    /// [`resource_attributes`]). Must be called inside a tokio runtime
    /// (the exporter's channel connects lazily on it).
    pub fn otlp(
        endpoint: &str,
        resource: Vec<(&'static str, String)>,
    ) -> Result<Self, ServiceError> {
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .with_timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| ServiceError::Telemetry(e.to_string()))?;
        let resource = Resource::builder_empty()
            .with_attributes(resource.into_iter().map(|(k, v)| KeyValue::new(k, v)))
            .build();
        let provider = SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(resource)
            .build();
        Ok(Telemetry {
            provider: Some(provider),
        })
    }

    /// For `config`'s endpoint, [`Telemetry::otlp`]; without one,
    /// [`Telemetry::off`].
    pub fn for_config(name: &str, config: &Config) -> Result<Self, ServiceError> {
        match &config.otlp_endpoint {
            Some(endpoint) => Self::otlp(endpoint, resource_attributes(name, config)),
            None => Ok(Self::off()),
        }
    }

    /// The span factory serve tasks use (cheap to clone).
    pub fn spans(&self) -> Spans {
        Spans {
            tracer: self.provider.as_ref().map(|p| p.tracer("mw-service")),
            ids: RandomIdGenerator::default(),
        }
    }

    /// Flushes and shuts the exporter down, waiting at most `timeout`.
    pub async fn shutdown(self, timeout: Duration) {
        let Some(provider) = self.provider else {
            return;
        };
        // The SDK's shutdown blocks its thread until the batch is out.
        let flush = tokio::task::spawn_blocking(move || provider.shutdown_with_timeout(timeout));
        match tokio::time::timeout(timeout + Duration::from_millis(500), flush).await {
            Ok(Ok(Ok(()))) => tracing::debug!("traces flushed"),
            Ok(Ok(Err(e))) => tracing::warn!(error = %e, "trace flush failed"),
            _ => tracing::warn!("trace flush timed out"),
        }
    }
}

/// Starts server spans (module docs); one per serve task.
#[derive(Debug, Clone)]
pub struct Spans {
    tracer: Option<SdkTracer>,
    ids: RandomIdGenerator,
}

impl Spans {
    /// Starts the server span of a request on `kind` with `meta`.
    pub fn start(&self, kind: &str, meta: &RequestMeta<'_>) -> ServerSpan {
        match &self.tracer {
            Some(tracer) => {
                let cx = meta.trace.map_or_else(Context::new, remote_context);
                let attributes = span_attributes(kind, meta)
                    .into_iter()
                    .map(|(k, v)| KeyValue::new(k, v))
                    .collect::<Vec<_>>();
                let span = tracer
                    .span_builder(kind.to_owned())
                    .with_kind(SpanKind::Server)
                    .with_attributes(attributes)
                    .start_with_context(tracer, &cx);
                let sc = span.span_context();
                let trace = TraceParent::new(
                    sc.trace_id().to_bytes(),
                    sc.span_id().to_bytes(),
                    sc.trace_flags().to_u8(),
                );
                ServerSpan {
                    span: Some(span),
                    trace,
                }
            }
            None => ServerSpan {
                span: None,
                trace: server_trace(
                    meta.trace,
                    self.ids.new_span_id().to_bytes(),
                    self.ids.new_trace_id().to_bytes(),
                ),
            },
        }
    }
}

/// One request's server span.
#[derive(Debug)]
pub struct ServerSpan {
    span: Option<Span>,
    trace: Option<TraceParent>,
}

impl ServerSpan {
    /// The span context handed to the handler as `Ctx::trace`.
    pub fn trace(&self) -> Option<TraceParent> {
        self.trace
    }

    /// Ends the span with the handler's outcome.
    pub fn end(self, outcome: Result<(), HandlerError>) {
        if let Some(mut span) = self.span {
            if let Err(e) = outcome {
                span.set_status(Status::Error {
                    description: Cow::Owned(format!("{e:?}")),
                });
            }
            span.end();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TP: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    #[test]
    fn m7_resource_and_span_attributes() {
        assert_eq!(
            resource_attributes("demo/echo", &Config::default()),
            [("service.name", "demo/echo".to_owned())]
        );
        let config = Config {
            namespace: Some("apps".into()),
            pod_name: Some("demo-echo-0".into()),
            ..Config::default()
        };
        assert_eq!(
            resource_attributes("demo/echo", &config),
            [
                ("service.name", "demo/echo".to_owned()),
                ("k8s.namespace.name", "apps".to_owned()),
                ("k8s.pod.name", "demo-echo-0".to_owned()),
            ]
        );
        let meta = RequestMeta {
            principal: Some("spiffe://infra.local/ns/apps/sa/demo-caller"),
            ..RequestMeta::NONE
        }
        .with_request_id_str("r-9");
        let attrs = span_attributes("demo/echo", &meta);
        assert_eq!(
            attrs,
            [
                ("rpc.system", "grpc".to_owned()),
                ("rpc.service", "demo".to_owned()),
                ("rpc.method", "demo/echo".to_owned()),
                ("mw.kind", "demo/echo".to_owned()),
                (
                    "mw.principal",
                    "spiffe://infra.local/ns/apps/sa/demo-caller".to_owned()
                ),
                ("mw.request_id", "r-9".to_owned()),
            ]
        );
        assert_eq!(span_attributes("demo/echo", &RequestMeta::NONE).len(), 4);
    }

    #[test]
    fn m7_without_export_spans_still_continue_the_trace() {
        let spans = Telemetry::off().spans();
        let inbound = TraceParent::parse(TP).unwrap();
        let s = spans.start("demo/echo", &RequestMeta::NONE.with_trace(inbound));
        let t = s.trace().expect("span context");
        assert_eq!(t.trace_id(), inbound.trace_id());
        assert_ne!(t.span_id(), inbound.span_id());
        s.end(Ok(()));
        let root = spans
            .start("demo/echo", &RequestMeta::NONE)
            .trace()
            .unwrap();
        assert!(root.is_sampled());
        assert_ne!(root.trace_id(), inbound.trace_id());
    }
}
