//! The generic `middleware.v1.Bus` service: publish/subscribe over gRPC.
//!
//! Wire contract (bodies are raw bytes, see [`RawCodec`]):
//!
//! | method                         | kind             | request                              | response               |
//! |--------------------------------|------------------|--------------------------------------|------------------------|
//! | `/middleware.v1.Bus/Publish`   | unary            | body = payload, [`KEY_HEADER`] = key | empty                  |
//! | `/middleware.v1.Bus/Subscribe` | server streaming | body = key text                      | stream of payloads     |
//!
//! [`BusService`] fans published samples out with a
//! [`tokio::sync::broadcast`] channel; each `Subscribe` stream keeps the
//! samples whose key equals its own. Publishing with no subscribers
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
use crate::key::Key;
use bytes::Bytes;
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
    let text = core::str::from_utf8(bytes).map_err(|_| Status::invalid_argument("key is not UTF-8"))?;
    Key::new(text).map_err(|_| Status::invalid_argument("invalid key"))?;
    Ok(text)
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
            PUBLISH_PATH => Grpc::new(RawCodec).unary(PublishMethod { tx: self.tx }, req).await,
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

fn publish(tx: &broadcast::Sender<Published>, request: Request<Bytes>) -> Result<Response<Bytes>, Status> {
    let key = request
        .metadata()
        .get(KEY_HEADER)
        .ok_or_else(|| Status::invalid_argument("missing mw-key metadata"))?;
    let key = Bytes::copy_from_slice(parse_key(key.as_bytes())?.as_bytes());
    // A send error only means nobody is subscribed: fire and forget.
    let _ = tx.send(Published { key, payload: request.into_inner() });
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
        let key = request.into_inner();
        let result = match parse_key(&key) {
            Ok(_) => Ok(Response::new(KeyedStream { key, inner: BroadcastStream::new(self.tx.subscribe()) })),
            Err(status) => Err(status),
        };
        core::future::ready(result)
    }
}

/// Broadcast receiver filtered to one key, yielding payloads.
pub struct KeyedStream {
    key: Bytes,
    inner: BroadcastStream<Published>,
}

impl core::fmt::Debug for KeyedStream {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("KeyedStream").field("key", &self.key).finish_non_exhaustive()
    }
}

impl Stream for KeyedStream {
    type Item = Result<Bytes, Status>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // Each iteration consumes one ready item; the loop ends when the
        // channel is pending or closed, so it is bounded by the backlog.
        loop {
            match Pin::new(&mut self.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(p))) if p.key == self.key => return Poll::Ready(Some(Ok(p.payload))),
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
        req.metadata_mut().insert(KEY_HEADER, "a b".parse().unwrap());
        assert_eq!(publish(&bus.tx, req).unwrap_err().code(), tonic::Code::InvalidArgument);

        let mut req = Request::new(Bytes::from_static(b"x"));
        req.metadata_mut().insert(KEY_HEADER, "a/b".parse().unwrap());
        assert!(publish(&bus.tx, req).is_ok(), "no subscribers is not an error");
    }

    #[tokio::test]
    async fn keyed_stream_filters_by_key() {
        use tokio_stream::StreamExt;
        let bus = BusService::new(8);
        let mut s = KeyedStream { key: Bytes::from_static(b"a/b"), inner: BroadcastStream::new(bus.tx.subscribe()) };
        for (k, p) in [("x/y", "1"), ("a/b", "2")] {
            let mut req = Request::new(Bytes::from(p));
            req.metadata_mut().insert(KEY_HEADER, k.parse().unwrap());
            publish(&bus.tx, req).unwrap();
        }
        assert_eq!(s.next().await.unwrap().unwrap(), Bytes::from_static(b"2"));
    }
}
