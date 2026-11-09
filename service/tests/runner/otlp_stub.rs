//! An in-process OTLP/gRPC trace collector on 127.0.0.1:0 for the
//! runner tests: a tonic `TraceService` that forwards every span it
//! receives, flattened with its resource attributes, to the test.

use std::net::SocketAddr;

use opentelemetry_proto::tonic::collector::trace::v1::trace_service_server::{
    TraceService, TraceServiceServer,
};
use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use opentelemetry_proto::tonic::common::v1::{KeyValue, any_value};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_stream::wrappers::TcpListenerStream;

/// One exported span, flattened.
#[derive(Debug, Clone)]
pub struct Exported {
    pub name: String,
    pub kind: i32,
    pub trace_id: Vec<u8>,
    pub span_id: Vec<u8>,
    pub parent_span_id: Vec<u8>,
    /// String attributes of the span.
    pub attributes: Vec<(String, String)>,
    /// String attributes of its resource.
    pub resource: Vec<(String, String)>,
    /// `Status.code` (0 unset, 2 error).
    pub status: i32,
}

impl Exported {
    pub fn attribute(&self, key: &str) -> Option<&str> {
        find(&self.attributes, key)
    }

    pub fn resource(&self, key: &str) -> Option<&str> {
        find(&self.resource, key)
    }
}

fn find<'a>(list: &'a [(String, String)], key: &str) -> Option<&'a str> {
    list.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn strings(kvs: &[KeyValue]) -> Vec<(String, String)> {
    kvs.iter()
        .filter_map(|kv| match kv.value.as_ref()?.value.as_ref()? {
            any_value::Value::StringValue(s) => Some((kv.key.clone(), s.clone())),
            _ => None,
        })
        .collect()
}

struct Collector {
    tx: mpsc::UnboundedSender<Exported>,
}

#[tonic::async_trait]
impl TraceService for Collector {
    async fn export(
        &self,
        request: tonic::Request<ExportTraceServiceRequest>,
    ) -> Result<tonic::Response<ExportTraceServiceResponse>, tonic::Status> {
        for rs in request.into_inner().resource_spans {
            let resource = rs
                .resource
                .as_ref()
                .map(|r| strings(&r.attributes))
                .unwrap_or_default();
            for ss in rs.scope_spans {
                for s in ss.spans {
                    let _ = self.tx.send(Exported {
                        name: s.name,
                        kind: s.kind,
                        trace_id: s.trace_id,
                        span_id: s.span_id,
                        parent_span_id: s.parent_span_id,
                        attributes: strings(&s.attributes),
                        resource: resource.clone(),
                        status: s.status.map_or(0, |st| st.code),
                    });
                }
            }
        }
        Ok(tonic::Response::new(ExportTraceServiceResponse {
            partial_success: None,
        }))
    }
}

/// Starts the collector; returns its address and the spans it receives.
pub async fn start() -> (SocketAddr, mpsc::UnboundedReceiver<Exported>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(TraceServiceServer::new(Collector { tx }))
            .serve_with_incoming(TcpListenerStream::new(listener)),
    );
    (addr, rx)
}
