//! Istio service-mesh support (feature `istio`).
//!
//! Everything here is pure value manipulation except
//! [`MeshChannelBuilder::connect`], which opens the channel.
//!
//! * [`mesh_authority`] — the in-mesh DNS authority
//!   `svc.ns.svc.cluster.local:port`, with RFC 1123 label validation.
//! * [`TraceContext`] — the headers Istio/Envoy need an application to copy
//!   from each inbound request onto its outbound calls so a trace stays one
//!   trace ([`PROPAGATED_HEADERS`]: `x-request-id`, W3C `traceparent` /
//!   `tracestate`, B3 multi- and single-header, and Lightstep
//!   `x-ot-span-context`).
//! * [`MeshInterceptor`] — a tonic [`Interceptor`] injecting a
//!   [`TraceContext`] (and optionally a `grpc-timeout`) into every outbound
//!   request. Build one per inbound request and wrap a cloned channel with
//!   it: `GrpcTransport::new(InterceptedService::new(channel.clone(), interceptor))`.
//! * [`GrpcTimeout`] — `grpc-timeout` values per the gRPC HTTP/2 spec.
//! * [`retry_condition`] / [`is_retriable`] — Istio's default `retryOn` set
//!   applied to gRPC status codes.
//! * [`MeshChannelBuilder`] — a plaintext (h2c) channel to the local Envoy
//!   sidecar with the mesh authority as `:authority`. mTLS between pods is
//!   Envoy's job, so the application never configures TLS.

use crate::grpc::GrpcError;
use core::time::Duration;
use http::Uri;
use tonic::metadata::{Ascii, MetadataMap, MetadataValue};
use tonic::service::Interceptor;
use tonic::transport::{Channel, Endpoint};
use tonic::{Code, Request, Status};

/// Failure building mesh addresses or channels.
#[derive(Debug)]
pub enum IstioError {
    /// A service or namespace name is not an RFC 1123 DNS label.
    InvalidLabel,
    /// Port 0 is not a valid service port.
    InvalidPort,
    /// The sidecar address or authority did not form a valid URI.
    InvalidUri,
    /// Connecting the channel failed.
    Transport(tonic::transport::Error),
}

impl core::fmt::Display for IstioError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            IstioError::InvalidLabel => f.write_str("name is not an RFC 1123 DNS label"),
            IstioError::InvalidPort => f.write_str("port must be non-zero"),
            IstioError::InvalidUri => f.write_str("address does not form a valid URI"),
            IstioError::Transport(e) => write!(f, "connecting to the mesh failed: {e}"),
        }
    }
}

impl std::error::Error for IstioError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            IstioError::Transport(e) => Some(e),
            _ => None,
        }
    }
}

impl From<IstioError> for GrpcError {
    fn from(e: IstioError) -> Self {
        match e {
            IstioError::Transport(t) => GrpcError::Transport(t),
            _ => GrpcError::InvalidHeader,
        }
    }
}

// ---------------------------------------------------------------- I1

/// Kubernetes cluster DNS suffix used by Istio.
pub const CLUSTER_DOMAIN: &str = "svc.cluster.local";

/// Whether `s` is an RFC 1123 DNS label: 1..=63 bytes of lowercase ASCII
/// letters, digits and `-`, starting and ending with a letter or digit.
pub fn is_dns_label(s: &str) -> bool {
    let b = s.as_bytes();
    let edge_ok = |c: &u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    (1..=63).contains(&b.len())
        && b.first().is_some_and(edge_ok)
        && b.last().is_some_and(edge_ok)
        && b.iter().all(|c| edge_ok(c) || *c == b'-')
}

/// The in-mesh authority `service.namespace.svc.cluster.local:port`.
pub fn mesh_authority(service: &str, namespace: &str, port: u16) -> Result<String, IstioError> {
    if !is_dns_label(service) || !is_dns_label(namespace) {
        return Err(IstioError::InvalidLabel);
    }
    if port == 0 {
        return Err(IstioError::InvalidPort);
    }
    Ok(format!("{service}.{namespace}.{CLUSTER_DOMAIN}:{port}"))
}

// ---------------------------------------------------------------- I2

/// Headers copied from an inbound request to outbound calls, as listed in
/// Istio's distributed-tracing documentation.
pub const PROPAGATED_HEADERS: [&str; 10] = [
    "x-request-id",
    "traceparent",
    "tracestate",
    "x-b3-traceid",
    "x-b3-spanid",
    "x-b3-parentspanid",
    "x-b3-sampled",
    "x-b3-flags",
    "b3",
    "x-ot-span-context",
];

/// The propagated trace headers of one inbound request. Absent headers are
/// simply not injected.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TraceContext {
    values: [Option<MetadataValue<Ascii>>; PROPAGATED_HEADERS.len()],
}

impl TraceContext {
    /// Extracts the [`PROPAGATED_HEADERS`] present in `metadata`.
    pub fn from_metadata(metadata: &MetadataMap) -> Self {
        Self { values: PROPAGATED_HEADERS.map(|name| metadata.get(name).cloned()) }
    }

    /// Extracts the trace headers of an inbound request.
    pub fn from_request<T>(request: &Request<T>) -> Self {
        Self::from_metadata(request.metadata())
    }

    /// The value of propagated header `name`, if present and visible ASCII.
    pub fn get(&self, name: &str) -> Option<&str> {
        let i = PROPAGATED_HEADERS.iter().position(|h| *h == name)?;
        self.values.get(i)?.as_ref()?.to_str().ok()
    }

    /// Whether no trace header was present.
    pub fn is_empty(&self) -> bool {
        self.values.iter().all(Option::is_none)
    }

    /// Writes every present header into `metadata`, replacing existing
    /// values of the same name.
    pub fn inject(&self, metadata: &mut MetadataMap) {
        for (name, value) in PROPAGATED_HEADERS.iter().zip(&self.values) {
            if let Some(v) = value {
                metadata.insert(*name, v.clone());
            }
        }
    }
}

/// tonic [`Interceptor`] adding a [`TraceContext`] and optional
/// `grpc-timeout` to every outbound request.
#[derive(Debug, Clone, Default)]
pub struct MeshInterceptor {
    trace: TraceContext,
    timeout: Option<GrpcTimeout>,
}

impl MeshInterceptor {
    /// Propagates `trace`.
    pub fn new(trace: TraceContext) -> Self {
        Self { trace, timeout: None }
    }

    /// Returns this interceptor also setting `grpc-timeout` to `timeout`.
    pub fn with_timeout(self, timeout: Duration) -> Self {
        Self { timeout: Some(GrpcTimeout::from_duration(timeout)), ..self }
    }

    /// Applies the interceptor to `metadata`. Pure; [`Interceptor::call`]
    /// delegates here.
    pub fn apply(&self, metadata: &mut MetadataMap) {
        self.trace.inject(metadata);
        if let Some(t) = self.timeout
            && let Ok(v) = MetadataValue::try_from(t.to_string())
        {
            metadata.insert(GRPC_TIMEOUT_HEADER, v);
        }
    }
}

impl Interceptor for MeshInterceptor {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        self.apply(request.metadata_mut());
        Ok(request)
    }
}

// ---------------------------------------------------------------- I3

/// Request header carrying the deadline.
pub const GRPC_TIMEOUT_HEADER: &str = "grpc-timeout";

/// Largest `TimeoutValue`: the spec allows at most 8 ASCII digits.
pub const MAX_TIMEOUT_VALUE: u32 = 99_999_999;

/// `TimeoutUnit` of a `grpc-timeout` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutUnit {
    /// `n`
    Nanos,
    /// `u`
    Micros,
    /// `m`
    Millis,
    /// `S`
    Seconds,
    /// `M`
    Minutes,
    /// `H`
    Hours,
}

impl TimeoutUnit {
    /// Finest to coarsest.
    const ALL: [TimeoutUnit; 6] = [
        TimeoutUnit::Nanos,
        TimeoutUnit::Micros,
        TimeoutUnit::Millis,
        TimeoutUnit::Seconds,
        TimeoutUnit::Minutes,
        TimeoutUnit::Hours,
    ];

    /// Length of one unit in nanoseconds.
    pub const fn nanos(self) -> u128 {
        match self {
            TimeoutUnit::Nanos => 1,
            TimeoutUnit::Micros => 1_000,
            TimeoutUnit::Millis => 1_000_000,
            TimeoutUnit::Seconds => 1_000_000_000,
            TimeoutUnit::Minutes => 60_000_000_000,
            TimeoutUnit::Hours => 3_600_000_000_000,
        }
    }

    /// The unit's wire character.
    pub const fn as_char(self) -> char {
        match self {
            TimeoutUnit::Nanos => 'n',
            TimeoutUnit::Micros => 'u',
            TimeoutUnit::Millis => 'm',
            TimeoutUnit::Seconds => 'S',
            TimeoutUnit::Minutes => 'M',
            TimeoutUnit::Hours => 'H',
        }
    }

    /// Inverse of [`TimeoutUnit::as_char`].
    pub const fn from_char(c: char) -> Option<Self> {
        match c {
            'n' => Some(TimeoutUnit::Nanos),
            'u' => Some(TimeoutUnit::Micros),
            'm' => Some(TimeoutUnit::Millis),
            'S' => Some(TimeoutUnit::Seconds),
            'M' => Some(TimeoutUnit::Minutes),
            'H' => Some(TimeoutUnit::Hours),
            _ => None,
        }
    }
}

/// A `grpc-timeout` value: at most [`MAX_TIMEOUT_VALUE`] of a
/// [`TimeoutUnit`]. `Display` renders the wire form, e.g. `250m`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrpcTimeout {
    value: u32,
    unit: TimeoutUnit,
}

impl GrpcTimeout {
    /// Encodes `d` in the finest unit whose value fits in 8 digits. Coarser
    /// units round up so the peer never sees a shorter deadline than asked
    /// for; durations beyond `99999999H` saturate there.
    pub fn from_duration(d: Duration) -> Self {
        let nanos = d.as_nanos();
        for unit in TimeoutUnit::ALL {
            let value = nanos.div_ceil(unit.nanos());
            if let Ok(value) = u32::try_from(value)
                && value <= MAX_TIMEOUT_VALUE
            {
                return Self { value, unit };
            }
        }
        Self { value: MAX_TIMEOUT_VALUE, unit: TimeoutUnit::Hours }
    }

    /// Parses the wire form; `None` when malformed or over 8 digits.
    pub fn parse(s: &str) -> Option<Self> {
        let unit = TimeoutUnit::from_char(s.chars().last()?)?;
        let digits = s.get(..s.len() - 1)?;
        if digits.is_empty() || digits.len() > 8 || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Some(Self { value: digits.parse().ok()?, unit })
    }

    /// The numeric part.
    pub const fn value(self) -> u32 {
        self.value
    }

    /// The unit.
    pub const fn unit(self) -> TimeoutUnit {
        self.unit
    }

    /// The deadline this value represents.
    pub fn as_duration(self) -> Duration {
        let nanos = u128::from(self.value) * self.unit.nanos();
        Duration::new(
            u64::try_from(nanos / 1_000_000_000).unwrap_or(u64::MAX),
            // Remainder of a division by 1e9 always fits in u32.
            u32::try_from(nanos % 1_000_000_000).unwrap_or(0),
        )
    }
}

impl core::fmt::Display for GrpcTimeout {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}{}", self.value, self.unit.as_char())
    }
}

// ---------------------------------------------------------------- I4

/// The conditions of Istio's default retry policy,
/// `retryOn: connect-failure,refused-stream,unavailable,cancelled,retriable-status-codes`
/// (with `retriableStatusCodes: [503]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryOn {
    /// Envoy `connect-failure`: the upstream TCP connection failed.
    ConnectFailure,
    /// Envoy `refused-stream`: the upstream reset the stream with
    /// `REFUSED_STREAM` before processing it.
    RefusedStream,
    /// gRPC `unavailable`: status `UNAVAILABLE` (14).
    Unavailable,
    /// gRPC `cancelled`: status `CANCELLED` (1).
    Cancelled,
    /// Envoy `retriable-status-codes`: HTTP 503.
    RetriableStatusCodes,
}

/// Istio's default `retryOn` set, in its documented order.
pub const DEFAULT_RETRY_ON: [RetryOn; 5] = [
    RetryOn::ConnectFailure,
    RetryOn::RefusedStream,
    RetryOn::Unavailable,
    RetryOn::Cancelled,
    RetryOn::RetriableStatusCodes,
];

impl RetryOn {
    /// The Envoy `retryOn` token.
    pub const fn as_str(self) -> &'static str {
        match self {
            RetryOn::ConnectFailure => "connect-failure",
            RetryOn::RefusedStream => "refused-stream",
            RetryOn::Unavailable => "unavailable",
            RetryOn::Cancelled => "cancelled",
            RetryOn::RetriableStatusCodes => "retriable-status-codes",
        }
    }
}

/// Which default `retryOn` condition covers a call that ended with `code`.
///
/// Mapping, as the application sees it:
///
/// * `CANCELLED` → [`RetryOn::Cancelled`].
/// * `UNAVAILABLE` → [`RetryOn::Unavailable`]. The three transport-level
///   conditions also surface as `UNAVAILABLE` in a gRPC client: a connect
///   failure and a refused stream are reported by tonic/hyper as
///   `UNAVAILABLE`, and HTTP 503 maps to `UNAVAILABLE` under the gRPC
///   HTTP-to-status mapping. Envoy retries those before the application
///   sees them; the gRPC code is the only thing left to classify.
/// * Every other code, including `DEADLINE_EXCEEDED`, `RESOURCE_EXHAUSTED`
///   and `INTERNAL`, is not in the default set → `None`.
pub const fn retry_condition(code: Code) -> Option<RetryOn> {
    match code {
        Code::Cancelled => Some(RetryOn::Cancelled),
        Code::Unavailable => Some(RetryOn::Unavailable),
        _ => None,
    }
}

/// Whether Istio's default retry policy would retry a call ending in
/// `code`.
pub const fn is_retriable(code: Code) -> bool {
    retry_condition(code).is_some()
}

// ---------------------------------------------------------------- I5

/// Builds a plaintext HTTP/2 (h2c) [`Channel`] for an in-mesh service.
///
/// The TCP connection goes to the sidecar address when one is set (for
/// example an Envoy outbound listener on `127.0.0.1`), otherwise to the
/// authority itself, which the sidecar's iptables redirect intercepts. In
/// both cases the mesh authority is sent as `:authority`, which Envoy uses
/// to pick the route; Envoy originates mTLS to the destination pod.
#[derive(Debug, Clone)]
pub struct MeshChannelBuilder {
    authority: String,
    sidecar: Option<String>,
    connect_timeout: Option<Duration>,
}

impl MeshChannelBuilder {
    /// Targets `authority`, typically from [`mesh_authority`].
    pub fn new(authority: impl Into<String>) -> Self {
        Self { authority: authority.into(), sidecar: None, connect_timeout: None }
    }

    /// Targets `service.namespace.svc.cluster.local:port`.
    pub fn for_service(service: &str, namespace: &str, port: u16) -> Result<Self, IstioError> {
        mesh_authority(service, namespace, port).map(Self::new)
    }

    /// Returns this builder connecting through `addr` (`host:port`).
    pub fn via_sidecar(self, addr: impl Into<String>) -> Self {
        Self { sidecar: Some(addr.into()), ..self }
    }

    /// Returns this builder with a TCP connect timeout.
    pub fn connect_timeout(self, d: Duration) -> Self {
        Self { connect_timeout: Some(d), ..self }
    }

    /// The `:authority` requests will carry.
    pub fn authority(&self) -> &str {
        &self.authority
    }

    /// The tonic [`Endpoint`]; no I/O.
    pub fn endpoint(&self) -> Result<Endpoint, IstioError> {
        let target = self.sidecar.as_deref().unwrap_or(&self.authority);
        let origin = Uri::try_from(format!("http://{}", self.authority)).map_err(|_| IstioError::InvalidUri)?;
        if origin.authority().map(|a| a.as_str()) != Some(self.authority.as_str()) {
            return Err(IstioError::InvalidUri);
        }
        let endpoint = Endpoint::from_shared(format!("http://{target}"))
            .map_err(|_| IstioError::InvalidUri)?
            .origin(origin);
        Ok(match self.connect_timeout {
            Some(d) => endpoint.connect_timeout(d),
            None => endpoint,
        })
    }

    /// Connects the channel.
    pub async fn connect(&self) -> Result<Channel, IstioError> {
        self.endpoint()?.connect().await.map_err(IstioError::Transport)
    }

    /// A channel that connects on first use.
    pub fn connect_lazy(&self) -> Result<Channel, IstioError> {
        Ok(self.endpoint()?.connect_lazy())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mesh_authority_formats_cluster_dns() {
        assert_eq!(
            mesh_authority("reviews", "bookinfo", 9080).unwrap(),
            "reviews.bookinfo.svc.cluster.local:9080"
        );
        assert!(matches!(mesh_authority("Reviews", "ns", 1), Err(IstioError::InvalidLabel)));
        assert!(matches!(mesh_authority("-a", "ns", 1), Err(IstioError::InvalidLabel)));
        assert!(matches!(mesh_authority("a", "", 1), Err(IstioError::InvalidLabel)));
        assert!(matches!(mesh_authority("a.b", "ns", 1), Err(IstioError::InvalidLabel)));
        assert!(matches!(mesh_authority("a", "ns", 0), Err(IstioError::InvalidPort)));
        assert!(is_dns_label(&"a".repeat(63)));
        assert!(!is_dns_label(&"a".repeat(64)));
    }

    #[test]
    fn trace_context_extracts_and_injects_only_propagated_headers() {
        let mut inbound = MetadataMap::new();
        inbound.insert("x-request-id", "req-1".parse().unwrap());
        inbound.insert("traceparent", "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01".parse().unwrap());
        inbound.insert("x-b3-sampled", "1".parse().unwrap());
        inbound.insert("authorization", "secret".parse().unwrap());
        let ctx = TraceContext::from_metadata(&inbound);
        assert_eq!(ctx.get("x-request-id"), Some("req-1"));
        assert_eq!(ctx.get("tracestate"), None);
        assert_eq!(ctx.get("authorization"), None);
        assert!(!ctx.is_empty());
        assert!(TraceContext::default().is_empty());

        let mut outbound = MetadataMap::new();
        ctx.inject(&mut outbound);
        assert_eq!(outbound.len(), 3);
        assert_eq!(outbound.get("x-b3-sampled").unwrap(), "1");
        assert!(outbound.get("authorization").is_none());
    }

    #[test]
    fn interceptor_adds_trace_and_timeout() {
        let mut inbound = MetadataMap::new();
        for name in PROPAGATED_HEADERS {
            inbound.insert(name, "v".parse().unwrap());
        }
        let mut i = MeshInterceptor::new(TraceContext::from_metadata(&inbound)).with_timeout(Duration::from_millis(250));
        let req = Interceptor::call(&mut i, Request::new(())).unwrap();
        for name in PROPAGATED_HEADERS {
            assert_eq!(req.metadata().get(name).unwrap(), "v", "{name}");
        }
        assert_eq!(req.metadata().get(GRPC_TIMEOUT_HEADER).unwrap(), "250000u");
    }

    #[test]
    fn grpc_timeout_picks_finest_unit_that_fits() {
        let f = |d| GrpcTimeout::from_duration(d).to_string();
        assert_eq!(f(Duration::ZERO), "0n");
        assert_eq!(f(Duration::from_nanos(99_999_999)), "99999999n");
        assert_eq!(f(Duration::from_nanos(100_000_000)), "100000u");
        assert_eq!(f(Duration::from_nanos(100_000_001)), "100001u"); // rounds up
        assert_eq!(f(Duration::from_millis(250)), "250000u");
        assert_eq!(f(Duration::from_secs(30)), "30000000u");
        assert_eq!(f(Duration::from_secs(3600)), "3600000m");
        assert_eq!(f(Duration::from_secs(100_000_000)), "1666667M");
        assert_eq!(f(Duration::from_secs(99_999_999 * 60)), "99999999M");
        assert_eq!(f(Duration::from_secs(100_000_000 * 60)), "1666667H");
        assert_eq!(f(Duration::MAX), "99999999H");
        for d in [Duration::from_millis(1), Duration::from_secs(7), Duration::from_secs(86_400)] {
            let t = GrpcTimeout::from_duration(d);
            assert!(t.to_string().len() <= 9);
            assert_eq!(GrpcTimeout::parse(&t.to_string()), Some(t));
            assert!(t.as_duration() >= d);
        }
        assert_eq!(GrpcTimeout::parse("123456789n"), None);
        assert_eq!(GrpcTimeout::parse("m"), None);
        assert_eq!(GrpcTimeout::parse("12x"), None);
        assert_eq!(GrpcTimeout::parse("-1S"), None);
    }

    #[test]
    fn retriable_codes_match_istio_default_retry_on() {
        assert_eq!(retry_condition(Code::Unavailable), Some(RetryOn::Unavailable));
        assert_eq!(retry_condition(Code::Cancelled), Some(RetryOn::Cancelled));
        for c in [
            Code::Ok,
            Code::Unknown,
            Code::InvalidArgument,
            Code::DeadlineExceeded,
            Code::NotFound,
            Code::ResourceExhausted,
            Code::Internal,
            Code::Unimplemented,
        ] {
            assert!(!is_retriable(c), "{c:?}");
        }
        let joined: Vec<_> = DEFAULT_RETRY_ON.iter().map(|r| r.as_str()).collect();
        assert_eq!(joined.join(","), "connect-failure,refused-stream,unavailable,cancelled,retriable-status-codes");
    }

    #[test]
    fn channel_builder_targets_sidecar_with_mesh_origin() {
        let b = MeshChannelBuilder::for_service("reviews", "bookinfo", 9080)
            .unwrap()
            .via_sidecar("127.0.0.1:15001");
        assert_eq!(b.authority(), "reviews.bookinfo.svc.cluster.local:9080");
        let e = b.endpoint().unwrap();
        assert_eq!(e.uri().to_string(), "http://127.0.0.1:15001/");
        assert!(MeshChannelBuilder::new("bad host/x").endpoint().is_err());
    }
}
