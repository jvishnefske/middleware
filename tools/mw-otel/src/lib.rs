//! OpenTelemetry for running middleware.
//!
//! * [`init`]: the exporter side the library leaves to applications. It
//!   installs global tracer, meter and logger providers that send OTLP over
//!   HTTP/protobuf, plus a `tracing` subscriber that prints to stderr and
//!   forwards events as OTLP logs. Any host service built with the
//!   `middleware` feature `zenoh-otel` then exports the backend's spans and
//!   metrics (`middleware::zenoh::otel`) once it calls this.
//! * The pure pieces of the bridge (`mw-otel-bridge`, `src/main.rs`):
//!   subscription parsing ([`parse_filter`]), board log lines
//!   ([`log_text`]) and node records from `sys/info` ([`Nodes`]).
//!
//! # Where the data goes
//!
//! The standard OpenTelemetry environment variables decide, as the OTLP
//! exporter reads them:
//!
//! | variable | example |
//! |---|---|
//! | `OTEL_EXPORTER_OTLP_ENDPOINT` | `http://collector:4318` (all three signals, `/v1/<signal>` appended) |
//! | `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` | `http://10.51.200.16/prometheus/api/v1/otlp/v1/metrics` (full URL: Prometheus' own OTLP receiver) |
//! | `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, `…_LOGS_ENDPOINT` | full URLs, per signal |
//! | `OTEL_EXPORTER_OTLP_HEADERS` | `authorization=Bearer …` |
//! | `OTEL_TRACES_SAMPLER`, `OTEL_TRACES_SAMPLER_ARG` | `traceidratio`, `0.1`: one span in ten on busy keys |
//! | `OTEL_RESOURCE_ATTRIBUTES` | `deployment.environment=lab` |
//!
//! A signal can be switched off with [`Signals`] (for example metrics only,
//! when the backend has no trace or log store yet).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use middleware::key::{Key, KeyFilterBuf, MAX_KEY_LEN};
use middleware::loader::{DeviceInfo, PayloadStatus};
use middleware::transport::SenderId;
use opentelemetry::{KeyValue, global};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Which signals [`init`] exports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Signals {
    pub traces: bool,
    pub metrics: bool,
    pub logs: bool,
}

impl Signals {
    pub const ALL: Signals = Signals {
        traces: true,
        metrics: true,
        logs: true,
    };
}

/// The installed providers; dropping it (or [`Telemetry::shutdown`])
/// flushes what is still batched.
pub struct Telemetry {
    tracer: Option<SdkTracerProvider>,
    meter: Option<SdkMeterProvider>,
    logger: Option<SdkLoggerProvider>,
}

impl Telemetry {
    /// Flushes and stops every exporter.
    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        if let Some(t) = self.tracer.take() {
            let _ = t.shutdown();
        }
        if let Some(m) = self.meter.take() {
            let _ = m.shutdown();
        }
        if let Some(l) = self.logger.take() {
            let _ = l.shutdown();
        }
    }
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Error building an exporter.
#[derive(Debug)]
pub struct InitError(pub String);

impl std::fmt::Display for InitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for InitError {}

/// Installs the OTLP exporters for `signals` as the global providers, with
/// `service` as `service.name`, and a `tracing` subscriber (stderr, filter
/// from `RUST_LOG`, default `info`; events also become OTLP logs when logs
/// are on). Call once, before opening any transport.
pub fn init(service: &str, signals: Signals) -> Result<Telemetry, InitError> {
    use opentelemetry_otlp::WithExportConfig as _;
    let resource = Resource::builder()
        .with_service_name(service.to_owned())
        .build();
    let e = |what: &str| {
        let what = what.to_owned();
        move |err: opentelemetry_otlp::ExporterBuildError| {
            InitError(format!("{what} exporter: {err}"))
        }
    };

    let tracer = if signals.traces {
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
            .build()
            .map_err(e("span"))?;
        let p = SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(resource.clone())
            .build();
        global::set_tracer_provider(p.clone());
        Some(p)
    } else {
        None
    };
    let meter = if signals.metrics {
        let exporter = opentelemetry_otlp::MetricExporter::builder()
            .with_http()
            .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
            .build()
            .map_err(e("metric"))?;
        let p = SdkMeterProvider::builder()
            .with_periodic_exporter(exporter)
            .with_resource(resource.clone())
            .build();
        global::set_meter_provider(p.clone());
        Some(p)
    } else {
        None
    };
    let logger = if signals.logs {
        let exporter = opentelemetry_otlp::LogExporter::builder()
            .with_http()
            .with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
            .build()
            .map_err(e("log"))?;
        Some(
            SdkLoggerProvider::builder()
                .with_batch_exporter(exporter)
                .with_resource(resource)
                .build(),
        )
    } else {
        None
    };

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // The exporters' own HTTP client logs through `tracing` too; keep it out
    // of the OTLP log stream, or every export would log about itself.
    let otlp_filter =
        EnvFilter::new("info,zenoh=warn,opentelemetry=off,reqwest=off,hyper=off,h2=off");
    let registry = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_filter(filter),
    );
    use tracing_subscriber::Layer as _;
    match &logger {
        Some(l) => registry
            .with(
                opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(l)
                    .with_filter(otlp_filter),
            )
            .try_init(),
        None => registry.try_init(),
    }
    .map_err(|err| InitError(format!("tracing subscriber: {err}")))?;
    Ok(Telemetry {
        tracer,
        meter,
        logger,
    })
}

// ------------------------------------------------------------ the bridge

/// Longest subscription key the bridge stores.
pub const FILTER_KEY_LEN: usize = MAX_KEY_LEN;

/// `a/b/**` is everything under `a/b` (and `a/b` itself); anything else is
/// that exact key. Zenoh's other wildcards (`*`, `$*`) are not keys and are
/// refused.
pub fn parse_filter(s: &str) -> Result<KeyFilterBuf<FILTER_KEY_LEN>, String> {
    let (key, prefix) = match s.strip_suffix("/**") {
        Some(k) => (k, true),
        None => (s, false),
    };
    let key = Key::new(key).map_err(|e| format!("{s:?}: {e:?}"))?;
    let f = if prefix {
        middleware::KeyFilter::Prefix(key)
    } else {
        middleware::KeyFilter::Exact(key)
    };
    f.to_buf().map_err(|e| format!("{s:?}: {e:?}"))
}

/// The bridge's default subscriptions: the boards' log, application
/// traffic, and the sensor routes' low-rate keys. Not `mic/pcm` or
/// `imu/raw`: a board's Zenoh link sends what a peer subscribes to, so
/// subscribing to raw sample streams makes the board send them.
pub const DEFAULT_SUBSCRIPTIONS: [&str; 5] =
    ["sys/log", "app/**", "imu/stat", "mic/stat", "mic/level"];

/// A board's log line as text: UTF-8 (invalid bytes replaced), control
/// characters other than tab shown as `\u{..}` so a line cannot forge
/// others or drive a terminal, trailing line breaks dropped.
pub fn log_text(payload: &[u8]) -> String {
    let text = String::from_utf8_lossy(payload);
    let text = text.trim_end_matches(['\r', '\n']);
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() && c != '\t' {
            out.extend(c.escape_unicode());
        } else {
            out.push(c);
        }
    }
    out
}

/// A sender id as the spans and metrics carry it (16 hex digits).
pub fn sender_hex(id: SenderId) -> String {
    middleware::zenoh::wire::zid16(id)
}

pub fn status_name(s: PayloadStatus) -> &'static str {
    match s {
        PayloadStatus::Empty => "empty",
        PayloadStatus::Stopped => "stopped",
        PayloadStatus::Running => "running",
        PayloadStatus::Trapped => "trapped",
        PayloadStatus::Loading => "loading",
    }
}

/// What the bridge knows of one node from its `sys/info` answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub chip: String,
    pub firmware: String,
    pub status: &'static str,
    pub capacity: u32,
    pub abi_version: u16,
    pub last_seen: Instant,
}

/// The nodes that answered `sys/info`, keyed by sender.
#[derive(Debug, Default)]
pub struct Nodes {
    nodes: BTreeMap<[u8; 8], Node>,
}

/// A change worth a log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeEvent {
    Up(SenderId),
    StatusChanged(SenderId, &'static str, &'static str),
    Down(SenderId),
}

impl Nodes {
    /// Records an answer from `sender` at `now`; `None` if it does not
    /// decode as a `sys/info` response.
    pub fn record(
        &mut self,
        sender: SenderId,
        info: &[u8],
        now: Instant,
    ) -> Option<Option<NodeEvent>> {
        let i = DeviceInfo::decode(info)?;
        let node = Node {
            chip: log_text(i.chip.as_bytes()),
            firmware: log_text(i.firmware.as_bytes()),
            status: status_name(i.status),
            capacity: i.capacity,
            abi_version: i.abi_version,
            last_seen: now,
        };
        let event = match self.nodes.insert(sender.0, node.clone()) {
            None => Some(NodeEvent::Up(sender)),
            Some(old) if old.status != node.status => {
                Some(NodeEvent::StatusChanged(sender, old.status, node.status))
            }
            Some(_) => None,
        };
        Some(event)
    }

    /// Forgets nodes not seen for `forget_after`, reporting each.
    pub fn expire(&mut self, now: Instant, forget_after: Duration) -> Vec<NodeEvent> {
        let gone: Vec<[u8; 8]> = self
            .nodes
            .iter()
            .filter(|(_, n)| now.saturating_duration_since(n.last_seen) > forget_after)
            .map(|(id, _)| *id)
            .collect();
        gone.iter().for_each(|id| {
            self.nodes.remove(id);
        });
        gone.into_iter()
            .map(|id| NodeEvent::Down(SenderId(id)))
            .collect()
    }

    /// One `mw.node.up` observation per node: 1 if it answered within
    /// `fresh`, else 0, with its identity as attributes.
    pub fn observations(&self, now: Instant, fresh: Duration) -> Vec<(u64, Vec<KeyValue>)> {
        self.nodes
            .iter()
            .map(|(id, n)| {
                let up = u64::from(now.saturating_duration_since(n.last_seen) <= fresh);
                (
                    up,
                    vec![
                        KeyValue::new("mw.sender", sender_hex(SenderId(*id))),
                        KeyValue::new("mw.chip", n.chip.clone()),
                        KeyValue::new("mw.firmware", n.firmware.clone()),
                        KeyValue::new("mw.payload.status", n.status),
                    ],
                )
            })
            .collect()
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::Filter;
    use middleware::loader::Caps;

    #[test]
    fn filters_parse_prefixes_and_exact_keys() {
        assert_eq!(
            parse_filter("app/**").unwrap().as_filter(),
            Filter::Prefix(Key::from_static("app"))
        );
        assert_eq!(
            parse_filter("sys/log").unwrap().as_filter(),
            Filter::Exact(Key::from_static("sys/log"))
        );
        for bad in ["**", "app/*", "", "a//b", "app/**/x"] {
            assert!(parse_filter(bad).is_err(), "{bad}");
        }
        for d in DEFAULT_SUBSCRIPTIONS {
            parse_filter(d).expect(d);
        }
        assert!(
            !DEFAULT_SUBSCRIPTIONS
                .iter()
                .any(|d| d.starts_with("mic/pcm") || *d == "mic/**")
        );
    }

    #[test]
    fn log_lines_cannot_forge_lines_or_escape_sequences() {
        assert_eq!(log_text(b"zenoh: session lost\r\n"), "zenoh: session lost");
        assert_eq!(log_text(b"a\nb"), "a\\u{a}b");
        assert_eq!(log_text(b"\x1b[2Jx\tq"), "\\u{1b}[2Jx\tq");
        assert_eq!(log_text(&[0x66, 0xff]), "f\u{fffd}");
    }

    fn info(status: PayloadStatus) -> Vec<u8> {
        let mut buf = [0u8; 128];
        let n = DeviceInfo {
            abi_version: 3,
            capacity: 65536,
            status,
            chip: "mimxrt1052",
            firmware: "imxrt-base 0.1",
            node: Some(SenderId([1; 8])),
            caps: Caps::NONE,
        }
        .encode(&mut buf)
        .unwrap();
        buf[..n].to_vec()
    }

    #[test]
    fn nodes_come_change_and_go() {
        let id = SenderId([0xab; 8]);
        let t0 = Instant::now();
        let mut nodes = Nodes::default();
        assert_eq!(
            nodes.record(id, &info(PayloadStatus::Stopped), t0),
            Some(Some(NodeEvent::Up(id)))
        );
        assert_eq!(
            nodes.record(id, &info(PayloadStatus::Stopped), t0),
            Some(None)
        );
        assert_eq!(
            nodes.record(id, &info(PayloadStatus::Running), t0),
            Some(Some(NodeEvent::StatusChanged(id, "stopped", "running")))
        );
        assert_eq!(nodes.record(id, b"\x01", t0), None, "not a sys/info answer");

        let obs = nodes.observations(t0 + Duration::from_secs(5), Duration::from_secs(30));
        assert_eq!(obs.len(), 1);
        assert_eq!(obs[0].0, 1);
        assert!(
            obs[0]
                .1
                .contains(&KeyValue::new("mw.sender", "abababababababab"))
        );
        assert!(
            obs[0]
                .1
                .contains(&KeyValue::new("mw.payload.status", "running"))
        );
        let stale = nodes.observations(t0 + Duration::from_secs(60), Duration::from_secs(30));
        assert_eq!(stale[0].0, 0, "stale but not yet forgotten: reported down");

        assert!(
            nodes
                .expire(t0 + Duration::from_secs(60), Duration::from_secs(120))
                .is_empty()
        );
        assert_eq!(
            nodes.expire(t0 + Duration::from_secs(200), Duration::from_secs(120)),
            vec![NodeEvent::Down(id)]
        );
        assert!(nodes.is_empty());
    }
}
