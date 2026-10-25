//! The generic `middleware.v1.Bus` service: publish/subscribe over gRPC.
//!
//! Wire contract (bodies are raw bytes, see [`RawCodec`]):
//!
//! | method                         | kind             | request                                         | response                         |
//! |--------------------------------|------------------|-------------------------------------------------|----------------------------------|
//! | `/middleware.v1.Bus/Publish`   | unary            | body = payload, [`KEY_HEADER`] = key            | empty                            |
//! | `/middleware.v1.Bus/Subscribe` | server streaming | body = filter key, [`FILTER_HEADER`] = kind     | stream of `[key_len][key][payload]` items |
//!
//! The filter kind is [`FILTER_EXACT`] (the default when the header is
//! absent) or [`FILTER_PREFIX`]; any other value is `INVALID_ARGUMENT`. Each
//! stream item carries its key because a prefix subscription receives many
//! keys: one length byte, the key text, then the payload
//! ([`encode_item`] / [`decode_item`]).
//!
//! [`BusService`] fans published samples out with a
//! [`tokio::sync::broadcast`] channel; each `Subscribe` stream keeps the
//! samples whose key its [`KeyFilter`] matches, segment-wise (a prefix
//! `app` selects `app` and `app/x`, never `apple`). The bus does not know
//! which client published a sample, so a client subscribed to a key it
//! publishes on receives its own publications. Publishing with no subscribers
//! succeeds (fire and forget). A subscriber that falls more than the channel
//! capacity behind loses the oldest samples, matching the best-effort
//! delivery of the other pub/sub backends. An invalid key is rejected with
//! `INVALID_ARGUMENT`; an unknown method with `UNIMPLEMENTED`.
//!
//! The service is implemented by hand on [`tonic::server::Grpc`] (no
//! codegen): a tower [`Service`] over `http::Request<Body>` plus
//! [`NamedService`], so it can be added to a stock
//! `tonic::transport::Server` with `add_service`, or routed by
//! [`GrpcServer`](super::GrpcServer).

use super::codec::RawCodec;
use crate::key::{Key, KeyFilter};
use bytes::{BufMut, Bytes, BytesMut};
use core::convert::Infallible;
use core::pin::Pin;
use core::task::{Context, Poll};
use tokio::sync::broadcast;
use tokio_stream::Stream;
use tokio_stream::wrappers::BroadcastStream;
use tonic::body::Body;
use tonic::codegen::{BoxFuture, Service};
use tonic::server::{Grpc, NamedService, ServerStreamingService, UnaryService};
use tonic::{Request, Response, Status};

/// Fully qualified service name.
pub const SERVICE_NAME: &str = "middleware.v1.Bus";
/// HTTP/2 path of the unary publish method.
pub const PUBLISH_PATH: &str = "/middleware.v1.Bus/Publish";
/// HTTP/2 path of the server-streaming subscribe method.
pub const SUBSCRIBE_PATH: &str = "/middleware.v1.Bus/Subscribe";
/// Metadata entry carrying the key of a `Publish` call.
pub const KEY_HEADER: &str = "mw-key";
/// Metadata entry selecting the filter kind of a `Subscribe` call.
pub const FILTER_HEADER: &str = "mw-filter";
/// [`FILTER_HEADER`] value for [`KeyFilter::Exact`] (also the default).
pub const FILTER_EXACT: &str = "exact";
/// [`FILTER_HEADER`] value for [`KeyFilter::Prefix`].
pub const FILTER_PREFIX: &str = "prefix";
/// Default broadcast capacity (samples buffered per lagging subscriber).
pub const DEFAULT_CAPACITY: usize = 64;

/// One published sample as carried by the broadcast channel.
#[derive(Debug, Clone)]
struct Published {
    key: Bytes,
    payload: Bytes,
}

/// Validates key bytes from the wire.
fn parse_key(bytes: &[u8]) -> Result<&str, Status> {
    let text =
        core::str::from_utf8(bytes).map_err(|_| Status::invalid_argument("key is not UTF-8"))?;
    Key::new(text).map_err(|_| Status::invalid_argument("invalid key"))?;
    Ok(text)
}

/// Key bytes as a [`Key`], or `None` if they are not one.
fn key_of(bytes: &[u8]) -> Option<Key<'_>> {
    Key::new(core::str::from_utf8(bytes).ok()?).ok()
}

/// Which filter a subscription stream applies to its key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterKind {
    Exact,
    Prefix,
}

impl FilterKind {
    /// Parses the [`FILTER_HEADER`] value; absent means exact.
    fn from_header(value: Option<&[u8]>) -> Result<Self, Status> {
        match value {
            None => Ok(FilterKind::Exact),
            Some(v) if v == FILTER_EXACT.as_bytes() => Ok(FilterKind::Exact),
            Some(v) if v == FILTER_PREFIX.as_bytes() => Ok(FilterKind::Prefix),
            Some(_) => Err(Status::invalid_argument("unknown mw-filter")),
        }
    }

    fn filter(self, key: Key<'_>) -> KeyFilter<'_> {
        match self {
            FilterKind::Exact => KeyFilter::Exact(key),
            FilterKind::Prefix => KeyFilter::Prefix(key),
        }
    }
}

/// The [`FILTER_HEADER`] value for `filter`.
pub fn filter_header(filter: KeyFilter<'_>) -> &'static str {
    match filter {
        KeyFilter::Exact(_) => FILTER_EXACT,
        KeyFilter::Prefix(_) => FILTER_PREFIX,
    }
}

/// Encodes one `Subscribe` stream item: `[key_len][key][payload]`. Keys are
/// at most [`MAX_KEY_LEN`](crate::key::MAX_KEY_LEN) bytes, so the length
/// fits one byte.
pub fn encode_item(key: Key<'_>, payload: &[u8]) -> Bytes {
    let key = key.as_bytes();
    let mut out = BytesMut::with_capacity(1 + key.len() + payload.len());
    out.put_u8(u8::try_from(key.len()).unwrap_or(u8::MAX));
    out.put_slice(key);
    out.put_slice(payload);
    out.freeze()
}

/// Splits a `Subscribe` stream item into its key and payload; `None` when
/// the length byte overruns the item or the key is not a valid [`Key`].
pub fn decode_item(item: &[u8]) -> Option<(Key<'_>, &[u8])> {
    let (&len, rest) = item.split_first()?;
    let (key, payload) = rest.split_at_checked(usize::from(len))?;
    Some((key_of(key)?, payload))
}

/// The `middleware.v1.Bus` server. Cheap to clone; clones share one
/// broadcast channel.
#[derive(Debug, Clone)]
pub struct BusService {
    tx: broadcast::Sender<Published>,
}

impl Default for BusService {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

impl BusService {
    /// A bus buffering up to `capacity` samples per subscriber (minimum 1).
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(1));
        Self { tx }
    }

    /// Number of open `Subscribe` streams.
    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }

    /// Handles one HTTP/2 request addressed to this service.
    pub async fn handle(self, req: http::Request<Body>) -> http::Response<Body> {
        match req.uri().path() {
            PUBLISH_PATH => {
                Grpc::new(RawCodec)
                    .unary(PublishMethod { tx: self.tx }, req)
                    .await
            }
            SUBSCRIBE_PATH => {
                Grpc::new(RawCodec)
                    .server_streaming(SubscribeMethod { tx: self.tx }, req)
                    .await
            }
            _ => Status::unimplemented("unknown bus method").into_http(),
        }
    }
}

impl NamedService for BusService {
    const NAME: &'static str = SERVICE_NAME;
}

impl Service<http::Request<Body>> for BusService {
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

/// `Publish`: key from metadata, payload from body.
struct PublishMethod {
    tx: broadcast::Sender<Published>,
}

impl UnaryService<Bytes> for PublishMethod {
    type Response = Bytes;
    type Future = core::future::Ready<Result<Response<Bytes>, Status>>;

    fn call(&mut self, request: Request<Bytes>) -> Self::Future {
        core::future::ready(publish(&self.tx, request))
    }
}

fn publish(
    tx: &broadcast::Sender<Published>,
    request: Request<Bytes>,
) -> Result<Response<Bytes>, Status> {
    let key = request
        .metadata()
        .get(KEY_HEADER)
        .ok_or_else(|| Status::invalid_argument("missing mw-key metadata"))?;
    let key = Bytes::copy_from_slice(parse_key(key.as_bytes())?.as_bytes());
    // A send error only means nobody is subscribed: fire and forget.
    let _ = tx.send(Published {
        key,
        payload: request.into_inner(),
    });
    Ok(Response::new(Bytes::new()))
}

/// `Subscribe`: key from body, stream of matching payloads.
struct SubscribeMethod {
    tx: broadcast::Sender<Published>,
}

impl ServerStreamingService<Bytes> for SubscribeMethod {
    type Response = Bytes;
    type ResponseStream = KeyedStream;
    type Future = core::future::Ready<Result<Response<KeyedStream>, Status>>;

    fn call(&mut self, request: Request<Bytes>) -> Self::Future {
        core::future::ready(subscribe(&self.tx, request))
    }
}

fn subscribe(
    tx: &broadcast::Sender<Published>,
    request: Request<Bytes>,
) -> Result<Response<KeyedStream>, Status> {
    let kind = FilterKind::from_header(
        request
            .metadata()
            .get(FILTER_HEADER)
            .map(|v| v.as_bytes()),
    )?;
    let key = request.into_inner();
    parse_key(&key)?;
    Ok(Response::new(KeyedStream {
        key,
        kind,
        inner: BroadcastStream::new(tx.subscribe()),
    }))
}

/// Broadcast receiver filtered by a [`KeyFilter`], yielding
/// [`encode_item`] items.
pub struct KeyedStream {
    key: Bytes,
    kind: FilterKind,
    inner: BroadcastStream<Published>,
}

impl KeyedStream {
    /// True if a sample published on `key` belongs on this stream.
    fn selects(&self, key: &[u8]) -> bool {
        match (key_of(&self.key), key_of(key)) {
            (Some(own), Some(key)) => self.kind.filter(own).matches(key),
            _ => false,
        }
    }
}

impl core::fmt::Debug for KeyedStream {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("KeyedStream")
            .field("key", &self.key)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl Stream for KeyedStream {
    type Item = Result<Bytes, Status>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // Each iteration consumes one ready item; the loop ends when the
        // channel is pending or closed, so it is bounded by the backlog.
        loop {
            match Pin::new(&mut self.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(p))) if self.selects(&p.key) => {
                    let item = key_of(&p.key).map(|k| encode_item(k, &p.payload));
                    match item {
                        Some(item) => return Poll::Ready(Some(Ok(item))),
                        None => continue,
                    }
                }
                // Other keys, and lag notifications (samples already lost).
                Poll::Ready(Some(_)) => continue,
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_requires_valid_key_header() {
        let bus = BusService::new(4);
        let err = publish(&bus.tx, Request::new(Bytes::new())).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);

        let mut req = Request::new(Bytes::new());
        req.metadata_mut()
            .insert(KEY_HEADER, "a b".parse().unwrap());
        assert_eq!(
            publish(&bus.tx, req).unwrap_err().code(),
            tonic::Code::InvalidArgument
        );

        let mut req = Request::new(Bytes::from_static(b"x"));
        req.metadata_mut()
            .insert(KEY_HEADER, "a/b".parse().unwrap());
        assert!(
            publish(&bus.tx, req).is_ok(),
            "no subscribers is not an error"
        );
    }

    fn publish_on(bus: &BusService, k: &str, p: &'static str) {
        let mut req = Request::new(Bytes::from(p));
        req.metadata_mut().insert(KEY_HEADER, k.parse().unwrap());
        publish(&bus.tx, req).unwrap();
    }

    #[tokio::test]
    async fn keyed_stream_filters_by_key() {
        use tokio_stream::StreamExt;
        let bus = BusService::new(8);
        let mut s = KeyedStream {
            key: Bytes::from_static(b"a/b"),
            kind: FilterKind::Exact,
            inner: BroadcastStream::new(bus.tx.subscribe()),
        };
        for (k, p) in [("x/y", "1"), ("a/b/c", "x"), ("a/b", "2")] {
            publish_on(&bus, k, p);
        }
        let item = s.next().await.unwrap().unwrap();
        assert_eq!(decode_item(&item), Some((Key::from_static("a/b"), &b"2"[..])));
    }

    #[tokio::test]
    async fn keyed_stream_prefix_selects_segments_not_apple() {
        use tokio_stream::StreamExt;
        let bus = BusService::new(8);
        let mut req = Request::new(Bytes::from_static(b"app"));
        req.metadata_mut()
            .insert(FILTER_HEADER, FILTER_PREFIX.parse().unwrap());
        let mut s = subscribe(&bus.tx, req).unwrap().into_inner();
        for (k, p) in [("apple", "no"), ("app/x", "1"), ("app", "2")] {
            publish_on(&bus, k, p);
        }
        let a = s.next().await.unwrap().unwrap();
        let b = s.next().await.unwrap().unwrap();
        assert_eq!(decode_item(&a), Some((Key::from_static("app/x"), &b"1"[..])));
        assert_eq!(decode_item(&b), Some((Key::from_static("app"), &b"2"[..])));
    }

    #[test]
    fn subscribe_rejects_unknown_filter_kind_and_bad_key() {
        let bus = BusService::new(1);
        let mut req = Request::new(Bytes::from_static(b"a"));
        req.metadata_mut()
            .insert(FILTER_HEADER, "glob".parse().unwrap());
        assert_eq!(
            subscribe(&bus.tx, req).unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
        let req = Request::new(Bytes::from_static(b"a b"));
        assert_eq!(
            subscribe(&bus.tx, req).unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
    }

    #[test]
    fn items_round_trip_and_reject_garbage() {
        let k = Key::from_static("a/b");
        let item = encode_item(k, b"xyz");
        assert_eq!(&item[..], b"\x03a/bxyz");
        assert_eq!(decode_item(&item), Some((k, &b"xyz"[..])));
        assert_eq!(decode_item(b""), None);
        assert_eq!(decode_item(b"\x09a/b"), None);
        assert_eq!(decode_item(b"\x03a b"), None);
        assert_eq!(filter_header(KeyFilter::Prefix(k)), FILTER_PREFIX);
        assert_eq!(filter_header(KeyFilter::Exact(k)), FILTER_EXACT);
    }
}
