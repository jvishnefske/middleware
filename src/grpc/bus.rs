//! The generic `middleware.v1.Bus` service: publish/subscribe over gRPC.
//!
//! Wire contract (bodies are raw bytes, see [`RawCodec`]):
//!
//! | method                         | kind             | request                                         | response                         |
//! |--------------------------------|------------------|-------------------------------------------------|----------------------------------|
//! | `/middleware.v1.Bus/Publish`   | unary            | body = payload, [`KEY_HEADER`] = key, optional [`CLIENT_HEADER`] | empty |
//! | `/middleware.v1.Bus/Subscribe` | server streaming | body = filter key, [`FILTER_HEADER`] = kind, optional [`CLIENT_HEADER`] and [`SELF_HEADER`] | stream of items (below) |
//!
//! The filter kind is [`FILTER_EXACT`] (the default when the header is
//! absent) or [`FILTER_PREFIX`]; any other value is `INVALID_ARGUMENT`. Each
//! stream item carries its key because a prefix subscription receives many
//! keys.
//!
//! # Client identity
//!
//! A client names itself with [`CLIENT_HEADER`]: its [`SenderId`] as 16
//! lowercase hex digits ([`client_id_header`] / [`parse_client_id`]); a
//! malformed value is `INVALID_ARGUMENT`. On `Publish` the bus records the id
//! with the sample. On `Subscribe` the header selects the item format and the
//! self-delivery rule, mirroring Zenoh's `allowed_origin(Remote)`:
//!
//! * **With [`CLIENT_HEADER`]:** items are `[key_len][key][sender_len]
//!   [sender][payload]` ([`encode_item_with_sender`] /
//!   [`decode_item_with_sender`]), where `sender_len` is 8 when the publisher
//!   sent its id and 0 otherwise. Samples published under the subscriber's
//!   own id are **not delivered**, unless [`SELF_HEADER`] is
//!   [`SELF_DELIVER`] ([`SELF_REFUSE`], the default, states it explicitly;
//!   any other value is `INVALID_ARGUMENT`).
//! * **Without it** (0.2 clients): items are `[key_len][key][payload]`
//!   ([`encode_item`] / [`decode_item`]) and every matching sample is
//!   delivered, the subscriber's own included.
//!
//! [`BusService`] fans published samples out with a
//! [`tokio::sync::broadcast`] channel; each `Subscribe` stream keeps the
//! samples whose key its [`KeyFilter`] matches, segment-wise (a prefix
//! `app` selects `app` and `app/x`, never `apple`), minus refused
//! self-deliveries. Publishing with no subscribers succeeds (fire and
//! forget). A subscriber that falls more than the channel
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
use crate::transport::SenderId;
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
/// Metadata entry naming the calling client (16 lowercase hex digits of its
/// [`SenderId`]); see the module docs.
pub const CLIENT_HEADER: &str = "mw-client";
/// Metadata entry of a `Subscribe` call choosing whether samples published
/// under the subscriber's own [`CLIENT_HEADER`] id are delivered.
pub const SELF_HEADER: &str = "mw-self";
/// [`SELF_HEADER`] value: do not deliver own publications (the default).
pub const SELF_REFUSE: &str = "refuse";
/// [`SELF_HEADER`] value: deliver own publications too.
pub const SELF_DELIVER: &str = "deliver";
/// Default broadcast capacity (samples buffered per lagging subscriber).
pub const DEFAULT_CAPACITY: usize = 64;

/// One published sample as carried by the broadcast channel.
#[derive(Debug, Clone)]
struct Published {
    key: Bytes,
    payload: Bytes,
    sender: Option<SenderId>,
}

/// The [`CLIENT_HEADER`] value for `id`: 16 lowercase hex digits.
pub fn client_id_header(id: SenderId) -> String {
    id.0.iter().map(|b| format!("{b:02x}")).collect()
}

/// Parses a [`CLIENT_HEADER`] value; `None` unless it is exactly 16 hex
/// digits (either case).
pub fn parse_client_id(value: &[u8]) -> Option<SenderId> {
    // `from_str_radix` alone would also accept a leading `+`.
    if value.len() != 16 || !value.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    let mut id = [0u8; 8];
    for (out, pair) in id.iter_mut().zip(value.chunks_exact(2)) {
        let text = core::str::from_utf8(pair).ok()?;
        *out = u8::from_str_radix(text, 16).ok()?;
    }
    Some(SenderId(id))
}

/// The client id a request names, `None` without [`CLIENT_HEADER`].
fn client_of<T>(request: &Request<T>) -> Result<Option<SenderId>, Status> {
    match request.metadata().get(CLIENT_HEADER) {
        None => Ok(None),
        Some(v) => parse_client_id(v.as_bytes())
            .map(Some)
            .ok_or_else(|| Status::invalid_argument("malformed mw-client")),
    }
}

/// Whether a subscriber takes its own publications, from [`SELF_HEADER`].
fn deliver_own(value: Option<&[u8]>) -> Result<bool, Status> {
    match value {
        None => Ok(false),
        Some(v) if v == SELF_REFUSE.as_bytes() => Ok(false),
        Some(v) if v == SELF_DELIVER.as_bytes() => Ok(true),
        Some(_) => Err(Status::invalid_argument("unknown mw-self")),
    }
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

/// Bytes a stream item adds to key and payload at most: the key length,
/// the sender length and an 8-byte sender.
pub const ITEM_OVERHEAD: usize = 2 + 8;

/// Encodes one stream item for a subscriber that sent [`CLIENT_HEADER`]:
/// `[key_len][key][sender_len][sender][payload]`, `sender_len` 8 or 0.
pub fn encode_item_with_sender(key: Key<'_>, sender: Option<SenderId>, payload: &[u8]) -> Bytes {
    let key = key.as_bytes();
    let mut out = BytesMut::with_capacity(ITEM_OVERHEAD + key.len() + payload.len());
    out.put_u8(u8::try_from(key.len()).unwrap_or(u8::MAX));
    out.put_slice(key);
    match sender {
        Some(id) => {
            out.put_u8(8);
            out.put_slice(&id.0);
        }
        None => out.put_u8(0),
    }
    out.put_slice(payload);
    out.freeze()
}

/// Splits an [`encode_item_with_sender`] item; `None` when a length
/// overruns the item, the sender length is neither 0 nor 8, or the key is
/// not a valid [`Key`].
pub fn decode_item_with_sender(item: &[u8]) -> Option<(Key<'_>, Option<SenderId>, &[u8])> {
    let (&len, rest) = item.split_first()?;
    let (key, rest) = rest.split_at_checked(usize::from(len))?;
    let (&sender_len, rest) = rest.split_first()?;
    let (sender, payload) = match sender_len {
        0 => (None, rest),
        8 => {
            let (id, payload) = rest.split_first_chunk::<8>()?;
            (Some(SenderId(*id)), payload)
        }
        _ => return None,
    };
    Some((key_of(key)?, sender, payload))
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
    let sender = client_of(&request)?;
    // A send error only means nobody is subscribed: fire and forget.
    let _ = tx.send(Published {
        key,
        payload: request.into_inner(),
        sender,
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
    let kind =
        FilterKind::from_header(request.metadata().get(FILTER_HEADER).map(|v| v.as_bytes()))?;
    let client = client_of(&request)?;
    let deliver_own = deliver_own(request.metadata().get(SELF_HEADER).map(|v| v.as_bytes()))?;
    let key = request.into_inner();
    parse_key(&key)?;
    Ok(Response::new(KeyedStream {
        key,
        kind,
        client,
        deliver_own,
        inner: BroadcastStream::new(tx.subscribe()),
    }))
}

/// Broadcast receiver filtered by a [`KeyFilter`] and the self-delivery
/// rule, yielding [`encode_item_with_sender`] items to a subscriber that
/// named itself and [`encode_item`] items otherwise.
pub struct KeyedStream {
    key: Bytes,
    kind: FilterKind,
    client: Option<SenderId>,
    deliver_own: bool,
    inner: BroadcastStream<Published>,
}

impl KeyedStream {
    /// True if sample `p` belongs on this stream: its key matches the
    /// filter and it is not a refused self-delivery.
    fn selects(&self, p: &Published) -> bool {
        let own = self.client.is_some() && p.sender == self.client;
        if own && !self.deliver_own {
            return false;
        }
        match (key_of(&self.key), key_of(&p.key)) {
            (Some(filter), Some(key)) => self.kind.filter(filter).matches(key),
            _ => false,
        }
    }

    /// The stream item for sample `p`, in this subscriber's format.
    fn item(&self, p: &Published) -> Option<Bytes> {
        let key = key_of(&p.key)?;
        Some(match self.client {
            Some(_) => encode_item_with_sender(key, p.sender, &p.payload),
            None => encode_item(key, &p.payload),
        })
    }
}

impl core::fmt::Debug for KeyedStream {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("KeyedStream")
            .field("key", &self.key)
            .field("kind", &self.kind)
            .field("client", &self.client)
            .field("deliver_own", &self.deliver_own)
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
                Poll::Ready(Some(Ok(p))) if self.selects(&p) => match self.item(&p) {
                    Some(item) => return Poll::Ready(Some(Ok(item))),
                    None => continue,
                },
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
            client: None,
            deliver_own: false,
            inner: BroadcastStream::new(bus.tx.subscribe()),
        };
        for (k, p) in [("x/y", "1"), ("a/b/c", "x"), ("a/b", "2")] {
            publish_on(&bus, k, p);
        }
        let item = s.next().await.unwrap().unwrap();
        assert_eq!(
            decode_item(&item),
            Some((Key::from_static("a/b"), &b"2"[..]))
        );
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
        assert_eq!(
            decode_item(&a),
            Some((Key::from_static("app/x"), &b"1"[..]))
        );
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

    const ME: SenderId = SenderId([0xde, 0xad, 0xbe, 0xef, 0, 1, 2, 0xff]);
    const PEER: SenderId = SenderId([1; 8]);

    #[test]
    fn g8_client_ids_round_trip_as_hex_and_bad_ones_are_refused() {
        assert_eq!(client_id_header(ME), "deadbeef000102ff");
        assert_eq!(parse_client_id(b"deadbeef000102ff"), Some(ME));
        assert_eq!(parse_client_id(b"DEADBEEF000102FF"), Some(ME));
        for bad in [
            &b""[..],
            b"deadbeef",
            b"deadbeef000102fff",
            b"deadbeef000102fg",
            b"+eadbeef000102ff",
        ] {
            assert_eq!(parse_client_id(bad), None, "{bad:?}");
        }
        let bus = BusService::new(1);
        let mut req = Request::new(Bytes::new());
        req.metadata_mut().insert(KEY_HEADER, "a".parse().unwrap());
        req.metadata_mut()
            .insert(CLIENT_HEADER, "xyz".parse().unwrap());
        assert_eq!(
            publish(&bus.tx, req).unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
        let mut req = Request::new(Bytes::from_static(b"a"));
        req.metadata_mut()
            .insert(CLIENT_HEADER, "xyz".parse().unwrap());
        assert_eq!(
            subscribe(&bus.tx, req).unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
        let mut req = Request::new(Bytes::from_static(b"a"));
        req.metadata_mut()
            .insert(CLIENT_HEADER, client_id_header(ME).parse().unwrap());
        req.metadata_mut()
            .insert(SELF_HEADER, "maybe".parse().unwrap());
        assert_eq!(
            subscribe(&bus.tx, req).unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
    }

    #[test]
    fn g8_items_with_sender_round_trip_and_reject_garbage() {
        let k = Key::from_static("a/b");
        let item = encode_item_with_sender(k, Some(ME), b"xyz");
        assert_eq!(item.len(), 1 + 3 + 1 + 8 + 3);
        assert_eq!(
            decode_item_with_sender(&item),
            Some((k, Some(ME), &b"xyz"[..]))
        );
        let anon = encode_item_with_sender(k, None, b"");
        assert_eq!(&anon[..], b"\x03a/b\x00");
        assert_eq!(decode_item_with_sender(&anon), Some((k, None, &b""[..])));
        assert_eq!(
            decode_item_with_sender(b"\x03a/b"),
            None,
            "no sender length"
        );
        assert_eq!(
            decode_item_with_sender(b"\x03a/b\x05abcde"),
            None,
            "bad sender length"
        );
        assert_eq!(
            decode_item_with_sender(b"\x03a/b\x08abc"),
            None,
            "short sender"
        );
    }

    fn publish_as(bus: &BusService, sender: Option<SenderId>, k: &str, p: &'static str) {
        let mut req = Request::new(Bytes::from(p));
        req.metadata_mut().insert(KEY_HEADER, k.parse().unwrap());
        if let Some(id) = sender {
            req.metadata_mut()
                .insert(CLIENT_HEADER, client_id_header(id).parse().unwrap());
        }
        publish(&bus.tx, req).unwrap();
    }

    fn subscribe_as(
        bus: &BusService,
        me: Option<SenderId>,
        own: Option<&'static str>,
    ) -> KeyedStream {
        let mut req = Request::new(Bytes::from_static(b"t"));
        if let Some(id) = me {
            req.metadata_mut()
                .insert(CLIENT_HEADER, client_id_header(id).parse().unwrap());
        }
        if let Some(v) = own {
            req.metadata_mut().insert(SELF_HEADER, v.parse().unwrap());
        }
        subscribe(&bus.tx, req).unwrap().into_inner()
    }

    #[tokio::test]
    async fn g8_bus_refuses_self_delivery_and_fills_the_sender() {
        use tokio_stream::StreamExt;
        let bus = BusService::new(8);
        let mut named = subscribe_as(&bus, Some(ME), None);
        let mut refusing = subscribe_as(&bus, Some(ME), Some(SELF_REFUSE));
        let mut loopback = subscribe_as(&bus, Some(ME), Some(SELF_DELIVER));
        let mut legacy = subscribe_as(&bus, None, None);
        publish_as(&bus, Some(ME), "t", "mine");
        publish_as(&bus, Some(PEER), "t", "peer");
        publish_as(&bus, None, "t", "anon");
        let t = Key::from_static("t");
        for s in [&mut named, &mut refusing] {
            let a = s.next().await.unwrap().unwrap();
            let b = s.next().await.unwrap().unwrap();
            assert_eq!(
                decode_item_with_sender(&a),
                Some((t, Some(PEER), &b"peer"[..]))
            );
            assert_eq!(decode_item_with_sender(&b), Some((t, None, &b"anon"[..])));
        }
        let own = loopback.next().await.unwrap().unwrap();
        assert_eq!(
            decode_item_with_sender(&own),
            Some((t, Some(ME), &b"mine"[..]))
        );
        // A 0.2 subscriber gets every sample in the old item format.
        let old = legacy.next().await.unwrap().unwrap();
        assert_eq!(decode_item(&old), Some((t, &b"mine"[..])));
    }
}
