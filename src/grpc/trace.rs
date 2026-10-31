//! Trace context on gRPC calls: the W3C `traceparent` and Envoy's
//! `x-request-id` headers, read into [`RequestMeta`](crate::RequestMeta) by
//! the responder ([`call_meta`](super::responder::call_meta)) and written from
//! [`RequestOptions`](crate::RequestOptions) by the client
//! ([`GrpcTransport`](super::GrpcTransport) and, with feature `istio`, the
//! mesh client).
//!
//! Both names are in Istio's list of headers an application propagates
//! (`istio::PROPAGATED_HEADERS`); this module needs only `grpc`. Values are
//! validated both ways: an invalid `traceparent` ([`TraceParent::parse`]) or
//! request id ([`RequestId::new`]) is dropped, never forwarded. The other
//! propagated headers (`tracestate`, B3, Lightstep) are not modelled here;
//! `istio::TraceContext` copies them verbatim.
//!
//! A per-call value is written into the request's metadata before the
//! channel's own layers run, so an interceptor that sets the same header
//! (for example `istio::MeshInterceptor` carrying an inbound
//! `TraceContext`) replaces it.

use crate::transport::{RequestId, TraceParent};
use tonic::metadata::{MetadataMap, MetadataValue};

/// The W3C trace-context header.
pub const TRACEPARENT_HEADER: &str = "traceparent";

/// The Envoy request-id header.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// The valid `traceparent` of `metadata`, if any. Pure.
pub fn extract_trace(metadata: &MetadataMap) -> Option<TraceParent> {
    metadata
        .get(TRACEPARENT_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(TraceParent::parse)
}

/// The valid `x-request-id` of `metadata`, if any, owned. Pure.
pub fn extract_request_id(metadata: &MetadataMap) -> Option<String> {
    metadata
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(RequestId::new)
        .map(|r| r.as_str().to_owned())
}

/// Writes `trace` and `request_id` (those present) into `metadata`,
/// replacing existing values. Pure apart from writing `metadata`.
pub fn inject(
    metadata: &mut MetadataMap,
    trace: Option<TraceParent>,
    request_id: Option<RequestId<'_>>,
) {
    if let Some(t) = trace {
        let mut buf = [0u8; TraceParent::LEN];
        // Lowercase hex and dashes: always a valid header value.
        if let Ok(v) = MetadataValue::try_from(t.format(&mut buf)) {
            metadata.insert(TRACEPARENT_HEADER, v);
        }
    }
    // Visible ASCII by construction: always a valid header value.
    if let Some(v) = request_id.and_then(|r| MetadataValue::try_from(r.as_str()).ok()) {
        metadata.insert(REQUEST_ID_HEADER, v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TP: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    #[test]
    fn k18_inject_then_extract_round_trips() {
        let t = TraceParent::parse(TP).unwrap();
        let id = RequestId::new("req-42").unwrap();
        let mut m = MetadataMap::new();
        assert_eq!((extract_trace(&m), extract_request_id(&m)), (None, None));
        inject(&mut m, Some(t), Some(id));
        assert_eq!(m.get(TRACEPARENT_HEADER).unwrap().to_str().unwrap(), TP);
        assert_eq!(extract_trace(&m), Some(t));
        assert_eq!(extract_request_id(&m).as_deref(), Some("req-42"));
        // Injecting nothing leaves existing values.
        inject(&mut m, None, None);
        assert_eq!(extract_trace(&m), Some(t));
    }

    #[test]
    fn k19_invalid_header_values_are_dropped() {
        let mut m = MetadataMap::new();
        m.insert(
            TRACEPARENT_HEADER,
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01"
                .parse()
                .unwrap(),
        );
        m.insert(REQUEST_ID_HEADER, "has a space".parse().unwrap());
        assert_eq!((extract_trace(&m), extract_request_id(&m)), (None, None));
        let long = "x".repeat(65);
        m.insert(REQUEST_ID_HEADER, long.parse().unwrap());
        assert_eq!(extract_request_id(&m), None);
    }
}
