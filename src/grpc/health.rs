//! The standard gRPC health service, `grpc.health.v1.Health`, without
//! generated code: [`HealthService`] (server), [`GrpcAnnouncer`] (sets
//! statuses, implements [`Announce`]) and [`HealthClient`] / [`HealthWatch`]
//! (client).
//!
//! # Wire format
//!
//! The two messages are hand-encoded protobuf on [`RawCodec`] (no prost):
//!
//! * `HealthCheckRequest { string service = 1; }` — [`encode_request`] /
//!   [`decode_request`].
//! * `HealthCheckResponse { ServingStatus status = 1; }` —
//!   [`encode_response`] / [`decode_response`]. `UNKNOWN` (0) is the
//!   proto3 default and encodes as an empty message.
//!
//! Decoders skip unknown fields of every wire type and take the last
//! occurrence of a repeated scalar, as protobuf requires; anything
//! malformed is [`ProtoError`]. Every loop consumes at least one byte per
//! step, so decoding is bounded by the message length.
//!
//! | method                             | kind             | unknown service        |
//! |------------------------------------|------------------|------------------------|
//! | `/grpc.health.v1.Health/Check`     | unary            | status `NOT_FOUND`     |
//! | `/grpc.health.v1.Health/Watch`     | server streaming | sends `SERVICE_UNKNOWN`|
//!
//! `Watch` sends the current status at once and then every change of it
//! (repeats are suppressed); it ends when the [`GrpcAnnouncer`] is dropped.
//!
//! # Names
//!
//! The service name `""` is the server as a whole and starts `SERVING`.
//! [`GrpcAnnouncer`] keys a discovered service by its kind
//! ([`health_name`]: the kind's key text, e.g. `cam/jpeg`), which is what
//! `grpc::mesh::MeshBrowse` (feature `istio`) watches for each endpoint.
//! At most [`MAX_HEALTH_ENTRIES`] names are held.

use super::GrpcError;
use super::codec::RawCodec;
use crate::discovery::{Announce, Service, ServiceKind};
use bytes::{BufMut, Bytes, BytesMut};
use core::convert::Infallible;
use core::pin::Pin;
use core::task::{Context, Poll};
use http::uri::PathAndQuery;
use tokio::sync::watch;
use tokio_stream::Stream;
use tokio_stream::wrappers::WatchStream;
use tonic::Streaming;
use tonic::body::Body;
use tonic::client::{Grpc as GrpcClient, GrpcService};
use tonic::codegen::{Body as HttpBody, BoxFuture, Service as TowerService, StdError};
use tonic::server::{Grpc, NamedService, ServerStreamingService, UnaryService};
use tonic::transport::Channel;
use tonic::{Request, Response, Status};

/// Fully qualified service name.
pub const SERVICE_NAME: &str = "grpc.health.v1.Health";
/// HTTP/2 path of `Check`.
pub const CHECK_PATH: &str = "/grpc.health.v1.Health/Check";
/// HTTP/2 path of `Watch`.
pub const WATCH_PATH: &str = "/grpc.health.v1.Health/Watch";
/// Most service names a [`HealthTable`] holds.
pub const MAX_HEALTH_ENTRIES: usize = 64;

/// `HealthCheckResponse.ServingStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServingStatus {
    /// `UNKNOWN` (0); also any value this crate does not know.
    Unknown,
    /// `SERVING` (1).
    Serving,
    /// `NOT_SERVING` (2).
    NotServing,
    /// `SERVICE_UNKNOWN` (3): only sent by `Watch`.
    ServiceUnknown,
}

impl ServingStatus {
    /// The enum's wire value.
    pub const fn code(self) -> u64 {
        match self {
            ServingStatus::Unknown => 0,
            ServingStatus::Serving => 1,
            ServingStatus::NotServing => 2,
            ServingStatus::ServiceUnknown => 3,
        }
    }

    /// Inverse of [`ServingStatus::code`]; unknown values are `Unknown`
    /// (proto3 open enums).
    pub const fn from_code(code: u64) -> Self {
        match code {
            1 => ServingStatus::Serving,
            2 => ServingStatus::NotServing,
            3 => ServingStatus::ServiceUnknown,
            _ => ServingStatus::Unknown,
        }
    }
}

/// A malformed health message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtoError {
    /// A varint ran past the end or over 10 bytes.
    Varint,
    /// A length-delimited or fixed field ran past the end.
    Truncated,
    /// Wire type 3, 4 (groups) or 6, 7, or field number 0.
    WireType,
    /// `service` is not UTF-8.
    Utf8,
}

/// Reads a base-128 varint; at most 10 bytes.
fn read_varint(buf: &[u8]) -> Result<(u64, &[u8]), ProtoError> {
    let mut value = 0u64;
    for (i, b) in buf.iter().take(10).enumerate() {
        value |= u64::from(b & 0x7F) << (7 * i);
        if b & 0x80 == 0 {
            return Ok((value, buf.get(i + 1..).unwrap_or(&[])));
        }
    }
    Err(ProtoError::Varint)
}

fn put_varint(out: &mut BytesMut, mut v: u64) {
    // At most 10 iterations: each shifts out 7 bits.
    while v >= 0x80 {
        out.put_u8((v as u8 & 0x7F) | 0x80);
        v >>= 7;
    }
    out.put_u8(v as u8);
}

/// One protobuf field.
enum Field<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed,
}

/// Reads the next field: its number and value.
fn read_field(buf: &[u8]) -> Result<(u64, Field<'_>, &[u8]), ProtoError> {
    let (tag, rest) = read_varint(buf)?;
    let number = tag >> 3;
    if number == 0 {
        return Err(ProtoError::WireType);
    }
    match tag & 7 {
        0 => {
            let (v, rest) = read_varint(rest)?;
            Ok((number, Field::Varint(v), rest))
        }
        1 | 5 => {
            let n = if tag & 7 == 1 { 8 } else { 4 };
            let rest = rest.get(n..).ok_or(ProtoError::Truncated)?;
            Ok((number, Field::Fixed, rest))
        }
        2 => {
            let (len, rest) = read_varint(rest)?;
            let len = usize::try_from(len).map_err(|_| ProtoError::Truncated)?;
            let (bytes, rest) = rest.split_at_checked(len).ok_or(ProtoError::Truncated)?;
            Ok((number, Field::Bytes(bytes), rest))
        }
        _ => Err(ProtoError::WireType),
    }
}

/// Encodes `HealthCheckRequest { service }`.
pub fn encode_request(service: &str) -> Bytes {
    let mut out = BytesMut::with_capacity(service.len() + 11);
    if !service.is_empty() {
        out.put_u8(0x0A);
        put_varint(&mut out, service.len() as u64);
        out.put_slice(service.as_bytes());
    }
    out.freeze()
}

/// Decodes a `HealthCheckRequest`; an absent `service` is `""`.
pub fn decode_request(mut msg: &[u8]) -> Result<&str, ProtoError> {
    let mut service: &[u8] = &[];
    // Each field consumes at least one byte.
    while !msg.is_empty() {
        let (number, field, rest) = read_field(msg)?;
        if let (1, Field::Bytes(b)) = (number, field) {
            service = b;
        }
        msg = rest;
    }
    core::str::from_utf8(service).map_err(|_| ProtoError::Utf8)
}

/// Encodes `HealthCheckResponse { status }`.
pub fn encode_response(status: ServingStatus) -> Bytes {
    let mut out = BytesMut::with_capacity(2);
    if status.code() != 0 {
        out.put_u8(0x08);
        put_varint(&mut out, status.code());
    }
    out.freeze()
}

/// Decodes a `HealthCheckResponse`; an absent `status` is `Unknown`.
pub fn decode_response(mut msg: &[u8]) -> Result<ServingStatus, ProtoError> {
    let mut status = ServingStatus::Unknown;
    while !msg.is_empty() {
        let (number, field, rest) = read_field(msg)?;
        if let (1, Field::Varint(v)) = (number, field) {
            status = ServingStatus::from_code(v);
        }
        msg = rest;
    }
    Ok(status)
}

/// The health service name [`GrpcAnnouncer`] and `MeshBrowse` use for a
/// service of `kind`: the kind's key text.
pub fn health_name<'a>(kind: ServiceKind<'a>) -> &'a str {
    kind.as_str()
}

/// Failure of a health update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthError {
    /// [`MAX_HEALTH_ENTRIES`] names (or announced instances) are held
    /// already.
    Full,
}

impl core::fmt::Display for HealthError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            HealthError::Full => f.write_str("health table full"),
        }
    }
}

impl std::error::Error for HealthError {}

/// Status per service name, as a value: every update returns a new table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthTable {
    entries: Vec<(String, ServingStatus)>,
}

impl Default for HealthTable {
    fn default() -> Self {
        Self::new()
    }
}

impl HealthTable {
    /// The server (`""`) `SERVING`, nothing else.
    pub fn new() -> Self {
        HealthTable {
            entries: vec![(String::new(), ServingStatus::Serving)],
        }
    }

    /// The status of `name`, if it was ever set.
    pub fn get(&self, name: &str) -> Option<ServingStatus> {
        self.entries
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, s)| *s)
    }

    /// This table with `name` at `status`; [`HealthError::Full`] for a new
    /// name when [`MAX_HEALTH_ENTRIES`] are held.
    pub fn with(self, name: &str, status: ServingStatus) -> Result<Self, HealthError> {
        let mut entries = self.entries;
        let held = entries.len();
        match entries.iter_mut().find(|(n, _)| n == name) {
            Some(e) => e.1 = status,
            None if held >= MAX_HEALTH_ENTRIES => return Err(HealthError::Full),
            None => entries.push((name.to_owned(), status)),
        }
        Ok(HealthTable { entries })
    }

    /// Names held.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Always false: the server entry `""` is always held.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Creates a connected pair: the server half to mount (on
/// [`GrpcServer::with_health`](super::GrpcServer::with_health) or a stock
/// tonic router) and the announcer that sets its statuses.
pub fn health() -> (HealthService, GrpcAnnouncer) {
    let (tx, rx) = watch::channel(HealthTable::new());
    (
        HealthService { table: rx },
        GrpcAnnouncer {
            table: tx,
            announced: Vec::new(),
        },
    )
}

/// Sets health statuses and implements [`Announce`]: `announce` makes the
/// service's kind ([`health_name`]) `SERVING`, `withdraw` of its last
/// announced instance `NOT_SERVING`.
///
/// In a Kubernetes mesh the Service object (and the DNS records derived
/// from it) is the real announcement: it says where pods are, whatever the
/// application does. What the application controls is whether it is
/// ready, which is what this reports and what `grpc::mesh::MeshBrowse`
/// checks before calling an endpoint up. TTLs, endpoints and TXT of the
/// announced [`Service`] are not used.
#[derive(Debug)]
pub struct GrpcAnnouncer {
    table: watch::Sender<HealthTable>,
    /// (kind, instance) announced and not withdrawn; at most
    /// [`MAX_HEALTH_ENTRIES`].
    announced: Vec<(String, String)>,
}

impl GrpcAnnouncer {
    /// Sets `name` to `status` now.
    pub fn set_status(&mut self, name: &str, status: ServingStatus) -> Result<(), HealthError> {
        let next = self.table.borrow().clone().with(name, status)?;
        self.table.send_replace(next);
        Ok(())
    }

    /// The current status of `name`.
    pub fn status(&self, name: &str) -> Option<ServingStatus> {
        self.table.borrow().get(name)
    }
}

impl Announce for GrpcAnnouncer {
    type Error = HealthError;

    /// Records the instance and sets its kind `SERVING`.
    async fn announce(&mut self, service: &Service<'_>) -> Result<(), HealthError> {
        let kind = health_name(service.kind);
        let known = self
            .announced
            .iter()
            .any(|(k, i)| k == kind && i == service.instance);
        if !known {
            if self.announced.len() >= MAX_HEALTH_ENTRIES {
                return Err(HealthError::Full);
            }
            self.set_status(kind, ServingStatus::Serving)?;
            self.announced
                .push((kind.to_owned(), service.instance.to_owned()));
            return Ok(());
        }
        self.set_status(kind, ServingStatus::Serving)
    }

    /// Forgets the instance; when no instance of the kind is left, sets the
    /// kind `NOT_SERVING`.
    async fn withdraw(&mut self, kind: ServiceKind<'_>, instance: &str) -> Result<(), HealthError> {
        let kind = health_name(kind);
        self.announced
            .retain(|(k, i)| !(k == kind && i == instance));
        if self.announced.iter().any(|(k, _)| k == kind) {
            return Ok(());
        }
        self.set_status(kind, ServingStatus::NotServing)
    }
}

/// The `grpc.health.v1.Health` server half of [`health`]. Cheap to clone;
/// clones serve the same table.
#[derive(Debug, Clone)]
pub struct HealthService {
    table: watch::Receiver<HealthTable>,
}

impl HealthService {
    /// Handles one HTTP/2 request addressed to this service.
    pub async fn handle(self, req: http::Request<Body>) -> http::Response<Body> {
        match req.uri().path() {
            CHECK_PATH => {
                Grpc::new(RawCodec)
                    .unary(CheckMethod { table: self.table }, req)
                    .await
            }
            WATCH_PATH => {
                Grpc::new(RawCodec)
                    .server_streaming(WatchMethod { table: self.table }, req)
                    .await
            }
            _ => Status::unimplemented("unknown health method").into_http(),
        }
    }
}

impl NamedService for HealthService {
    const NAME: &'static str = SERVICE_NAME;
}

impl TowerService<http::Request<Body>> for HealthService {
    type Response = http::Response<Body>;
    type Error = Infallible;
    type Future = BoxFuture<Self::Response, Infallible>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<Body>) -> Self::Future {
        let this = self.clone();
        Box::pin(async move { Ok(this.handle(req).await) })
    }
}

fn bad_request(e: ProtoError) -> Status {
    Status::invalid_argument(format!("malformed HealthCheckRequest: {e:?}"))
}

struct CheckMethod {
    table: watch::Receiver<HealthTable>,
}

impl UnaryService<Bytes> for CheckMethod {
    type Response = Bytes;
    type Future = core::future::Ready<Result<Response<Bytes>, Status>>;

    fn call(&mut self, request: Request<Bytes>) -> Self::Future {
        let body = request.into_inner();
        let reply = decode_request(&body).map_err(bad_request).and_then(|name| {
            self.table
                .borrow()
                .get(name)
                .map(|s| Response::new(encode_response(s)))
                .ok_or_else(|| Status::not_found("unknown service"))
        });
        core::future::ready(reply)
    }
}

struct WatchMethod {
    table: watch::Receiver<HealthTable>,
}

impl ServerStreamingService<Bytes> for WatchMethod {
    type Response = Bytes;
    type ResponseStream = StatusStream;
    type Future = core::future::Ready<Result<Response<StatusStream>, Status>>;

    fn call(&mut self, request: Request<Bytes>) -> Self::Future {
        let body = request.into_inner();
        let reply = decode_request(&body).map_err(bad_request).map(|name| {
            Response::new(StatusStream {
                name: name.to_owned(),
                inner: WatchStream::new(self.table.clone()),
                last: None,
            })
        });
        core::future::ready(reply)
    }
}

/// The `Watch` response stream: the named service's status on every
/// change of the table that changes it.
pub struct StatusStream {
    name: String,
    inner: WatchStream<HealthTable>,
    last: Option<ServingStatus>,
}

impl core::fmt::Debug for StatusStream {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StatusStream")
            .field("name", &self.name)
            .field("last", &self.last)
            .finish_non_exhaustive()
    }
}

impl Stream for StatusStream {
    type Item = Result<Bytes, Status>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // Each iteration consumes one ready table version; the loop ends
        // when none is ready or the announcer is gone.
        loop {
            match Pin::new(&mut self.inner).poll_next(cx) {
                Poll::Ready(Some(table)) => {
                    let status = table
                        .get(&self.name)
                        .unwrap_or(ServingStatus::ServiceUnknown);
                    if self.last != Some(status) {
                        self.last = Some(status);
                        return Poll::Ready(Some(Ok(encode_response(status))));
                    }
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

fn malformed(e: ProtoError) -> GrpcError {
    GrpcError::Status {
        code: tonic::Code::Internal,
        message: format!("malformed HealthCheckResponse: {e:?}"),
    }
}

/// Client of `grpc.health.v1.Health` over any tonic service.
#[derive(Debug, Clone)]
pub struct HealthClient<T = Channel> {
    grpc: GrpcClient<T>,
}

impl<T> HealthClient<T>
where
    T: GrpcService<Body>,
    T::Error: Into<StdError>,
    T::ResponseBody: HttpBody<Data = Bytes> + Send + 'static,
    <T::ResponseBody as HttpBody>::Error: Into<StdError>,
{
    /// Wraps a channel (or intercepted service).
    pub fn new(inner: T) -> Self {
        HealthClient {
            grpc: GrpcClient::new(inner),
        }
    }

    async fn ready(&mut self) -> Result<(), GrpcError> {
        self.grpc
            .ready()
            .await
            .map_err(|e| GrpcError::NotReady(e.into().to_string()))
    }

    /// `Check(service)`. An unknown service is the server's `NOT_FOUND`
    /// status; a malformed reply is status `INTERNAL`.
    pub async fn check(&mut self, service: &str) -> Result<ServingStatus, GrpcError> {
        self.ready().await?;
        let reply = self
            .grpc
            .unary(
                Request::new(encode_request(service)),
                PathAndQuery::from_static(CHECK_PATH),
                RawCodec,
            )
            .await?;
        decode_response(reply.get_ref()).map_err(malformed)
    }

    /// Opens `Watch(service)`; returns once the server accepted it.
    pub async fn watch(&mut self, service: &str) -> Result<HealthWatch, GrpcError> {
        self.ready().await?;
        let reply = self
            .grpc
            .server_streaming(
                Request::new(encode_request(service)),
                PathAndQuery::from_static(WATCH_PATH),
                RawCodec,
            )
            .await?;
        Ok(HealthWatch {
            stream: reply.into_inner(),
        })
    }
}

/// An open `Watch` stream.
#[derive(Debug)]
pub struct HealthWatch {
    stream: Streaming<Bytes>,
}

impl HealthWatch {
    /// Waits for the next status. [`GrpcError::StreamEnded`] when the
    /// server closed the stream. Cancel safe (tonic's `Streaming` keeps
    /// partial messages).
    pub async fn next(&mut self) -> Result<ServingStatus, GrpcError> {
        let msg = self.stream.message().await?.ok_or(GrpcError::StreamEnded)?;
        decode_response(&msg).map_err(malformed)
    }

    /// Polls for the next status, as [`HealthWatch::next`] does; for
    /// callers multiplexing several watches.
    pub fn poll_status(&mut self, cx: &mut Context<'_>) -> Poll<Result<ServingStatus, GrpcError>> {
        match Pin::new(&mut self.stream).poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(Err(GrpcError::StreamEnded)),
            Poll::Ready(Some(Err(status))) => Poll::Ready(Err(status.into())),
            Poll::Ready(Some(Ok(msg))) => Poll::Ready(decode_response(&msg).map_err(malformed)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::Endpoint;

    #[test]
    fn g13_health_messages_round_trip_and_skip_unknown_fields() {
        assert_eq!(&encode_request("cam/jpeg")[..], b"\x0a\x08cam/jpeg");
        assert_eq!(decode_request(&encode_request("cam/jpeg")), Ok("cam/jpeg"));
        assert!(encode_request("").is_empty());
        assert_eq!(decode_request(b""), Ok(""));
        for s in [
            ServingStatus::Unknown,
            ServingStatus::Serving,
            ServingStatus::NotServing,
            ServingStatus::ServiceUnknown,
        ] {
            assert_eq!(decode_response(&encode_response(s)), Ok(s));
            assert_eq!(ServingStatus::from_code(s.code()), s);
        }
        assert!(encode_response(ServingStatus::Unknown).is_empty());
        assert_eq!(&encode_response(ServingStatus::NotServing)[..], b"\x08\x02");
        // Unknown fields of every wire type are skipped; last value wins.
        let msg = b"\x10\x05\x19\x01\x02\x03\x04\x05\x06\x07\x08\x22\x02ab\x2d\x01\x02\x03\x04\x08\x02\x08\x01";
        assert_eq!(decode_response(msg), Ok(ServingStatus::Serving));
        // A multi-byte varint value and an unknown enum value.
        assert_eq!(decode_response(b"\x08\x80\x01"), Ok(ServingStatus::Unknown));
        let mut long = b"\x0a\x80\x01".to_vec();
        long.extend_from_slice("s".repeat(128).as_bytes());
        assert_eq!(decode_request(&long).map(str::len), Ok(128));
    }

    #[test]
    fn g13_malformed_health_messages_are_refused() {
        assert_eq!(decode_response(b"\x08"), Err(ProtoError::Varint));
        assert_eq!(
            decode_response(b"\x08\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\x01"),
            Err(ProtoError::Varint)
        );
        assert_eq!(decode_request(b"\x0a\x05ab"), Err(ProtoError::Truncated));
        assert_eq!(decode_response(b"\x09\x01"), Err(ProtoError::Truncated));
        assert_eq!(decode_response(b"\x0b"), Err(ProtoError::WireType));
        assert_eq!(decode_response(b"\x00\x01"), Err(ProtoError::WireType));
        assert_eq!(decode_request(b"\x0a\x01\xff"), Err(ProtoError::Utf8));
    }

    #[test]
    fn g13_health_table_is_a_bounded_value() {
        let t = HealthTable::new();
        assert_eq!(t.get(""), Some(ServingStatus::Serving));
        assert_eq!(t.get("x"), None);
        let t = t.with("x", ServingStatus::NotServing).unwrap();
        let t = t.with("x", ServingStatus::Serving).unwrap();
        assert_eq!((t.get("x"), t.len()), (Some(ServingStatus::Serving), 2));
        let mut full = HealthTable::new();
        for i in 1..MAX_HEALTH_ENTRIES {
            full = full.with(&i.to_string(), ServingStatus::Serving).unwrap();
        }
        assert_eq!(
            full.clone().with("new", ServingStatus::Serving),
            Err(HealthError::Full)
        );
        assert!(
            full.with("1", ServingStatus::NotServing).is_ok(),
            "updates fit"
        );
    }

    fn svc(instance: &'static str) -> Service<'static> {
        Service {
            kind: ServiceKind::from_static("cam/jpeg"),
            instance,
            node: None,
            endpoint: Endpoint::Local,
            txt: b"",
            ttl_s: 60,
        }
    }

    #[tokio::test]
    async fn g13_announcer_serves_while_any_instance_is_announced() {
        let (_svc, mut a) = health();
        let kind = ServiceKind::from_static("cam/jpeg");
        assert_eq!(a.status("cam/jpeg"), None);
        a.announce(&svc("one")).await.unwrap();
        a.announce(&svc("two")).await.unwrap();
        a.announce(&svc("two")).await.unwrap();
        assert_eq!(a.status("cam/jpeg"), Some(ServingStatus::Serving));
        a.withdraw(kind, "one").await.unwrap();
        assert_eq!(a.status("cam/jpeg"), Some(ServingStatus::Serving));
        a.withdraw(kind, "two").await.unwrap();
        assert_eq!(a.status("cam/jpeg"), Some(ServingStatus::NotServing));
        a.announce(&svc("one")).await.unwrap();
        assert_eq!(a.status("cam/jpeg"), Some(ServingStatus::Serving));
    }

    #[tokio::test]
    async fn g13_watch_stream_sends_current_then_changes_only() {
        use tokio_stream::StreamExt;
        let (svc, mut a) = health();
        let mut s = StatusStream {
            name: "k".to_owned(),
            inner: WatchStream::new(svc.table.clone()),
            last: None,
        };
        let next = |b: Bytes| decode_response(&b).unwrap();
        assert_eq!(
            next(s.next().await.unwrap().unwrap()),
            ServingStatus::ServiceUnknown
        );
        a.set_status("other", ServingStatus::Serving).unwrap();
        a.set_status("k", ServingStatus::Serving).unwrap();
        assert_eq!(
            next(s.next().await.unwrap().unwrap()),
            ServingStatus::Serving
        );
        a.set_status("k", ServingStatus::NotServing).unwrap();
        assert_eq!(
            next(s.next().await.unwrap().unwrap()),
            ServingStatus::NotServing
        );
        drop(a);
        assert!(s.next().await.is_none(), "ends with the announcer");
    }
}
