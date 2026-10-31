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
//! * [`GrpcTimeout`] — `grpc-timeout` values per the gRPC HTTP/2 spec
//!   (defined in `grpc::timeout`, which the client and responder use
//!   without this feature; re-exported here).
//! * [`retry_condition`] / [`is_retriable`] — Istio's default `retryOn` set
//!   applied to gRPC status codes.
//! * [`MeshChannelBuilder`] — a plaintext (h2c) channel to the local Envoy
//!   sidecar with the mesh authority as `:authority`. mTLS between pods is
//!   Envoy's job, so the application never configures TLS.
//! * [`peer_principal`] — the caller's SPIFFE ID from Envoy's
//!   `x-forwarded-client-cert` ([`XFCC_HEADER`]), which
//!   [`ResponderService`](crate::grpc::ResponderService) puts in
//!   `RequestMeta::principal` when built with
//!   [`PrincipalSource::SidecarXfcc`](crate::grpc::responder::PrincipalSource).
//! * [`mesh`](crate::grpc::mesh) (same feature) builds on these: a client
//!   routing to discovered endpoints through the sidecar, gather over them,
//!   and health-checked browsing.

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
        Self {
            values: PROPAGATED_HEADERS.map(|name| metadata.get(name).cloned()),
        }
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
        Self {
            trace,
            timeout: None,
        }
    }

    /// Returns this interceptor also setting `grpc-timeout` to `timeout`.
    pub fn with_timeout(self, timeout: Duration) -> Self {
        Self {
            timeout: Some(GrpcTimeout::from_duration(timeout)),
            ..self
        }
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

pub use crate::grpc::timeout::{GRPC_TIMEOUT_HEADER, GrpcTimeout, MAX_TIMEOUT_VALUE, TimeoutUnit};

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
        Self {
            authority: authority.into(),
            sidecar: None,
            connect_timeout: None,
        }
    }

    /// Targets `service.namespace.svc.cluster.local:port`.
    pub fn for_service(service: &str, namespace: &str, port: u16) -> Result<Self, IstioError> {
        mesh_authority(service, namespace, port).map(Self::new)
    }

    /// Returns this builder connecting through `addr` (`host:port`).
    pub fn via_sidecar(self, addr: impl Into<String>) -> Self {
        Self {
            sidecar: Some(addr.into()),
            ..self
        }
    }

    /// Returns this builder with a TCP connect timeout.
    pub fn connect_timeout(self, d: Duration) -> Self {
        Self {
            connect_timeout: Some(d),
            ..self
        }
    }

    /// The `:authority` requests will carry.
    pub fn authority(&self) -> &str {
        &self.authority
    }

    /// The tonic [`Endpoint`]; no I/O.
    pub fn endpoint(&self) -> Result<Endpoint, IstioError> {
        let target = self.sidecar.as_deref().unwrap_or(&self.authority);
        let origin = Uri::try_from(format!("http://{}", self.authority))
            .map_err(|_| IstioError::InvalidUri)?;
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
        self.endpoint()?
            .connect()
            .await
            .map_err(IstioError::Transport)
    }

    /// A channel that connects on first use.
    pub fn connect_lazy(&self) -> Result<Channel, IstioError> {
        Ok(self.endpoint()?.connect_lazy())
    }
}

// ---------------------------------------------------------------- I6

/// The request header in which Envoy forwards the client certificate
/// details of the connection it terminated.
pub const XFCC_HEADER: &str = "x-forwarded-client-cert";

/// Splits `s` at the first `sep` outside double quotes (inside quotes a
/// backslash escapes the next byte). `sep` is ASCII, so both halves are
/// valid `&str`.
fn split_unquoted(s: &str, sep: u8) -> (&str, Option<&str>) {
    let (mut quoted, mut escaped) = (false, false);
    for (i, b) in s.bytes().enumerate() {
        if escaped {
            escaped = false;
        } else if quoted && b == b'\\' {
            escaped = true;
        } else if b == b'"' {
            quoted = !quoted;
        } else if b == sep && !quoted {
            return (s.get(..i).unwrap_or(s), s.get(i + 1..));
        }
    }
    (s, None)
}

/// Whether every double quote in `s` is closed and no escape is left
/// dangling.
fn quotes_balanced(s: &str) -> bool {
    let (mut quoted, mut escaped) = (false, false);
    for b in s.bytes() {
        if escaped {
            escaped = false;
        } else if quoted && b == b'\\' {
            escaped = true;
        } else if b == b'"' {
            quoted = !quoted;
        }
    }
    !quoted && !escaped
}

/// The text of an XFCC value: a quoted value without its quotes, or the
/// bare value. `None` for a quoted value containing an escape (a SPIFFE ID
/// never contains `"` or `\`, so there is nothing to unescape) or a bare
/// value containing a quote.
fn xfcc_value(v: &str) -> Option<&str> {
    match v.strip_prefix('"') {
        Some(rest) => {
            let inner = rest.strip_suffix('"')?;
            (!inner.contains(['"', '\\'])).then_some(inner)
        }
        None => (!v.contains('"')).then_some(v),
    }
}

/// Whether `s` is a SPIFFE ID as the SPIFFE ID specification defines it:
/// `spiffe://`, a non-empty trust domain of lowercase letters, digits,
/// `.`, `-` and `_`, then an optional path of `/`-separated non-empty
/// segments of letters, digits, `.`, `-` and `_`, none of them `.` or
/// `..`, and no trailing `/`.
pub fn is_spiffe_id(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("spiffe://") else {
        return false;
    };
    let (domain, path) = match rest.find('/') {
        Some(i) => (rest.get(..i).unwrap_or(""), rest.get(i..)),
        None => (rest, None),
    };
    let domain_ok = !domain.is_empty()
        && domain
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b));
    let segment_ok = |seg: &str| {
        !seg.is_empty()
            && seg != "."
            && seg != ".."
            && seg
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    };
    domain_ok && path.is_none_or(|p| p.get(1..).is_some_and(|p| p.split('/').all(segment_ok)))
}

/// The SPIFFE ID of the peer that called this hop, from an Envoy
/// `x-forwarded-client-cert` header value; pure.
///
/// The header is a `,`-separated list of elements, one per proxy that
/// terminated a TLS connection, each a `;`-separated list of `Key=Value`
/// pairs (`By`, `Hash`, `Cert`, `Chain`, `Subject`, `URI`, `DNS`; keys are
/// case-insensitive). A value containing `,`, `;` or `=` is double-quoted,
/// with `"` inside it written `\"`. Proxies append, so the **last** element
/// describes the connection closest to this server: the local sidecar's
/// view of its downstream peer. This returns the first `URI` value of that
/// element that [`is_spiffe_id`]; `None` when the header is malformed
/// (unbalanced quotes), the last element has no such URI, or it is empty.
///
/// # Trust
///
/// The header is only as trustworthy as whoever set it. Behind an Istio
/// sidecar, Envoy sanitizes or sets XFCC on every inbound request
/// (`forwardClientCertDetails`: `SANITIZE_SET` or `APPEND_FORWARD`, which
/// put the verified peer certificate last), so the last element is the
/// mTLS-authenticated caller. Without a sidecar in front of the server, any
/// client can send any XFCC value. Use this only on servers reachable
/// solely through the sidecar; the responder ignores the header unless
/// configured with
/// [`PrincipalSource::SidecarXfcc`](crate::grpc::responder::PrincipalSource).
pub fn peer_principal(xfcc: &str) -> Option<&str> {
    if !quotes_balanced(xfcc) {
        return None;
    }
    // The last element: split at every unquoted comma, keep the tail.
    // Each step consumes at least one byte, so this ends.
    let mut last = xfcc;
    let mut rest = Some(xfcc);
    while let Some(r) = rest {
        let (element, tail) = split_unquoted(r, b',');
        last = element;
        rest = tail;
    }
    let mut pairs = Some(last);
    while let Some(p) = pairs {
        let (pair, tail) = split_unquoted(p, b';');
        pairs = tail;
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        if !key.trim().eq_ignore_ascii_case("uri") {
            continue;
        }
        if let Some(v) = xfcc_value(value.trim())
            && is_spiffe_id(v)
        {
            return Some(v);
        }
    }
    None
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
        assert!(matches!(
            mesh_authority("Reviews", "ns", 1),
            Err(IstioError::InvalidLabel)
        ));
        assert!(matches!(
            mesh_authority("-a", "ns", 1),
            Err(IstioError::InvalidLabel)
        ));
        assert!(matches!(
            mesh_authority("a", "", 1),
            Err(IstioError::InvalidLabel)
        ));
        assert!(matches!(
            mesh_authority("a.b", "ns", 1),
            Err(IstioError::InvalidLabel)
        ));
        assert!(matches!(
            mesh_authority("a", "ns", 0),
            Err(IstioError::InvalidPort)
        ));
        assert!(is_dns_label(&"a".repeat(63)));
        assert!(!is_dns_label(&"a".repeat(64)));
    }

    #[test]
    fn trace_context_extracts_and_injects_only_propagated_headers() {
        let mut inbound = MetadataMap::new();
        inbound.insert("x-request-id", "req-1".parse().unwrap());
        inbound.insert(
            "traceparent",
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"
                .parse()
                .unwrap(),
        );
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
        let mut i = MeshInterceptor::new(TraceContext::from_metadata(&inbound))
            .with_timeout(Duration::from_millis(250));
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
        for d in [
            Duration::from_millis(1),
            Duration::from_secs(7),
            Duration::from_secs(86_400),
        ] {
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
        assert_eq!(
            retry_condition(Code::Unavailable),
            Some(RetryOn::Unavailable)
        );
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
        assert_eq!(
            joined.join(","),
            "connect-failure,refused-stream,unavailable,cancelled,retriable-status-codes"
        );
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

    const CLIENT: &str = "spiffe://cluster.local/ns/test/sa/client";

    #[test]
    fn i6_peer_principal_takes_the_spiffe_uri_of_the_last_element() {
        // One element, as Istio's sidecar sets it (SANITIZE_SET).
        let one = "By=spiffe://cluster.local/ns/test/sa/server;\
                   Hash=468ed33be74eee6556d90c0149c1309e9ba61d6425303443c0748a02dd8de688;\
                   Subject=\"\";URI=spiffe://cluster.local/ns/test/sa/client";
        assert_eq!(peer_principal(one), Some(CLIENT));
        // Two hops: the closest (last) element wins.
        let two = "By=spiffe://a/x;URI=spiffe://cluster.local/ns/edge/sa/gateway,\
                   By=spiffe://cluster.local/ns/test/sa/server;URI=spiffe://cluster.local/ns/test/sa/client";
        assert_eq!(peer_principal(two), Some(CLIENT));
        // Keys are case-insensitive; whitespace around pairs is tolerated.
        assert_eq!(
            peer_principal(" uri = spiffe://cluster.local/ns/test/sa/client "),
            Some(CLIENT)
        );
        // A quoted URI is unquoted.
        assert_eq!(
            peer_principal("URI=\"spiffe://cluster.local/ns/test/sa/client\""),
            Some(CLIENT)
        );
        // Several URI SANs: the first SPIFFE one.
        assert_eq!(
            peer_principal(
                "URI=https://example.com/x;URI=spiffe://cluster.local/ns/test/sa/client"
            ),
            Some(CLIENT)
        );
    }

    #[test]
    fn i6_quoted_separators_do_not_split_elements_or_pairs() {
        // `,` `;` and `=` inside a quoted Subject belong to it.
        let h = "By=spiffe://td/s;Subject=\"CN=x,O=y;L=z\";URI=spiffe://cluster.local/ns/test/sa/client";
        assert_eq!(peer_principal(h), Some(CLIENT));
        // An escaped quote inside a quoted value does not end it.
        let h =
            "Subject=\"CN=a\\\",URI=spiffe://evil/x\";URI=spiffe://cluster.local/ns/test/sa/client";
        assert_eq!(peer_principal(h), Some(CLIENT));
        // A quoted comma in an earlier element must not make its tail the
        // last element.
        let h = "URI=spiffe://cluster.local/ns/test/sa/client;Subject=\"a,URI=spiffe://evil/x\"";
        assert_eq!(peer_principal(h), Some(CLIENT));
    }

    #[test]
    fn i6_malformed_or_missing_principals_are_none() {
        for h in [
            "",
            ",",
            "By=spiffe://td/s",
            // The last element has no URI, even though an earlier one does.
            "URI=spiffe://cluster.local/ns/test/sa/client,By=spiffe://td/s",
            // Unbalanced quotes: the whole header is refused.
            "URI=spiffe://cluster.local/ns/test/sa/client;Subject=\"open",
            "URI=\"spiffe://cluster.local/ns/test/sa/client",
            // Not SPIFFE IDs.
            "URI=https://cluster.local/ns/test",
            "URI=spiffe://",
            "URI=spiffe:///ns/x",
            "URI=spiffe://Upper/ns",
            "URI=spiffe://td/ns//x",
            "URI=spiffe://td/ns/",
            "URI=spiffe://td/./x",
            "URI=spiffe://td/../x",
            "URI=spiffe://td/a b",
            "URI=spiffe://td/a\"b",
            // A quoted value with an escape is refused, not unescaped.
            "URI=\"spiffe://td/a\\\"b\"",
            "URI",
        ] {
            assert_eq!(peer_principal(h), None, "{h:?}");
        }
        assert!(is_spiffe_id("spiffe://td"));
        assert!(is_spiffe_id("spiffe://cluster.local/ns/a-b/sa/c_d.e"));
    }
}
