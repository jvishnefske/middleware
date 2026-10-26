//! Client side: [`GrpcTransport`] (requests, publishes and subscriptions)
//! and [`GrpcSubscriber`] (a `Subscribe` stream).
//!
//! # Cancel safety
//!
//! * [`Subscriber::recv`] waits on tonic's `Streaming`, which keeps partly
//!   received messages in itself; dropping the wait loses nothing.
//! * [`Requester::request`] and [`Publisher::publish`] are single unary
//!   calls: dropping one cancels the HTTP/2 stream. A dropped publish may or
//!   may not have reached the bus, and is never retried.
//! * [`Subscribe::subscribe`] dropped before it returns abandons the stream.
//!
//! The transport is generic over any tonic
//! [`GrpcService`], so the same code runs over a
//! bare [`Channel`] or a channel wrapped in an interceptor (for example
//! `istio::MeshInterceptor`, which adds trace headers). Each call copies the
//! caller's bytes into an owned [`Bytes`] at the boundary and copies the
//! reply back into the caller's buffer, refusing to truncate.

use super::GrpcError;
use super::bus::{
    FILTER_HEADER, KEY_HEADER, PUBLISH_PATH, SUBSCRIBE_PATH, decode_item, filter_header,
};
use super::codec::RawCodec;
use super::{GrpcSubscriberKind, MAX_PAYLOAD};
use crate::key::{Key, KeyFilter, MAX_KEY_LEN};
use crate::transport::{
    Limits, Publisher, Requester, Sample, Subscribe, Subscriber, copy_into, sample_into,
};
use bytes::Bytes;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use tokio_stream::Stream;
use http::uri::PathAndQuery;
use tonic::Streaming;
use tonic::body::Body;
use tonic::client::{Grpc, GrpcService};
use tonic::codegen::{Body as HttpBody, StdError};
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;

/// The HTTP/2 path `/Service/Method` for a two-segment key, or
/// [`GrpcError::NotServiceMethod`] for any other key.
pub fn method_path(key: Key<'_>) -> Result<PathAndQuery, GrpcError> {
    let (service, method) = key.service_method().ok_or(GrpcError::NotServiceMethod)?;
    PathAndQuery::try_from(format!("/{service}/{method}")).map_err(|_| GrpcError::InvalidHeader)
}

/// gRPC client implementing [`Requester`], [`Publisher`] and [`Subscribe`].
#[derive(Debug, Clone)]
pub struct GrpcTransport<T = Channel> {
    grpc: Grpc<T>,
}

impl GrpcTransport<Channel> {
    /// Connects to `uri` (for example `http://127.0.0.1:50051`).
    pub async fn connect(uri: impl Into<Bytes>) -> Result<Self, GrpcError> {
        let channel = tonic::transport::Endpoint::from_shared(uri)?
            .connect()
            .await?;
        Ok(Self::new(channel))
    }
}

impl<T> GrpcTransport<T>
where
    T: GrpcService<Body>,
    T::Error: Into<StdError>,
    T::ResponseBody: HttpBody<Data = Bytes> + Send + 'static,
    <T::ResponseBody as HttpBody>::Error: Into<StdError>,
{
    /// Wraps an existing channel or intercepted service.
    pub fn new(inner: T) -> Self {
        Self {
            grpc: Grpc::new(inner),
        }
    }

    async fn ready(&mut self) -> Result<(), GrpcError> {
        self.grpc
            .ready()
            .await
            .map_err(|e| GrpcError::NotReady(e.into().to_string()))
    }

    async fn unary(
        &mut self,
        path: PathAndQuery,
        request: tonic::Request<Bytes>,
    ) -> Result<Bytes, GrpcError> {
        self.ready().await?;
        let resp = self.grpc.unary(request, path, RawCodec).await?;
        Ok(resp.into_inner())
    }

}

impl<T> Subscribe for GrpcTransport<T>
where
    T: GrpcService<Body>,
    T::Error: Into<StdError>,
    T::ResponseBody: HttpBody<Data = Bytes> + Send + 'static,
    <T::ResponseBody as HttpBody>::Error: Into<StdError>,
{
    type Subscriber<'a> = GrpcSubscriber
    where
        Self: 'a;
    type Error = GrpcError;

    /// Opens a `middleware.v1.Bus/Subscribe` stream for `filter` (the key in
    /// the body, the kind in [`FILTER_HEADER`]); the server filters with
    /// [`KeyFilter::matches`].
    ///
    /// Returns once the server has accepted the subscription, so samples
    /// published afterwards are delivered.
    async fn subscribe(&mut self, filter: KeyFilter<'_>) -> Result<GrpcSubscriber, GrpcError> {
        self.ready().await?;
        let key = filter.key();
        let mut request = tonic::Request::new(Bytes::copy_from_slice(key.as_bytes()));
        request.metadata_mut().insert(
            FILTER_HEADER,
            MetadataValue::from_static(filter_header(filter)),
        );
        let resp = self
            .grpc
            .server_streaming(request, PathAndQuery::from_static(SUBSCRIBE_PATH), RawCodec)
            .await?;
        let kind = match filter {
            KeyFilter::Exact(_) => GrpcSubscriberKind::Exact,
            KeyFilter::Prefix(_) => GrpcSubscriberKind::Prefix,
        };
        Ok(GrpcSubscriber {
            key: key.as_str().to_owned(),
            kind,
            stream: resp.into_inner(),
        })
    }
}

impl<T> Requester for GrpcTransport<T>
where
    T: GrpcService<Body>,
    T::Error: Into<StdError>,
    T::ResponseBody: HttpBody<Data = Bytes> + Send + 'static,
    <T::ResponseBody as HttpBody>::Error: Into<StdError>,
{
    type Error = GrpcError;

    async fn request<'b>(
        &mut self,
        key: Key<'_>,
        request: &[u8],
        response: &'b mut [u8],
    ) -> Result<&'b [u8], GrpcError> {
        let path = method_path(key)?;
        let reply = self
            .unary(path, tonic::Request::new(Bytes::copy_from_slice(request)))
            .await?;
        let out = copy_into(response, &reply).ok_or(GrpcError::BufferTooSmall {
            needed: reply.len(),
        })?;
        Ok(out)
    }
}

impl<T> Publisher for GrpcTransport<T>
where
    T: GrpcService<Body>,
    T::Error: Into<StdError>,
    T::ResponseBody: HttpBody<Data = Bytes> + Send + 'static,
    <T::ResponseBody as HttpBody>::Error: Into<StdError>,
{
    type Error = GrpcError;

    /// Keys up to [`MAX_KEY_LEN`]; payloads up to [`MAX_PAYLOAD`], so a
    /// publish body and its keyed stream item both stay within tonic's
    /// default 4 MiB decoding limit.
    const LIMITS: Limits = Limits::new(MAX_KEY_LEN, MAX_PAYLOAD);

    async fn publish(&mut self, key: Key<'_>, payload: &[u8]) -> Result<(), GrpcError> {
        let mut request = tonic::Request::new(Bytes::copy_from_slice(payload));
        let value = MetadataValue::try_from(key.as_str()).map_err(|_| GrpcError::InvalidHeader)?;
        request.metadata_mut().insert(KEY_HEADER, value);
        self.unary(PathAndQuery::from_static(PUBLISH_PATH), request)
            .await?;
        Ok(())
    }
}

/// One `Subscribe` stream; implements [`Subscriber`]. Samples carry their
/// own key (a prefix subscription receives many) and no sender metadata:
/// the bus does not identify publishers.
#[derive(Debug)]
pub struct GrpcSubscriber {
    key: String,
    kind: GrpcSubscriberKind,
    stream: Streaming<Bytes>,
}

impl GrpcSubscriber {
    /// The key of the filter this subscription was opened with.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Whether the filter is exact or a prefix.
    pub fn kind(&self) -> GrpcSubscriberKind {
        self.kind
    }
}

/// Copies one keyed stream item into `buf` as a [`Sample`].
fn item_into<'b>(item: &[u8], buf: &'b mut [u8]) -> Result<Sample<'b>, GrpcError> {
    let (key, payload) = decode_item(item).ok_or(GrpcError::MalformedSample)?;
    let needed = key.as_bytes().len() + payload.len();
    sample_into(buf, key.as_str(), payload).ok_or(GrpcError::BufferTooSmall { needed })
}

impl Subscriber for GrpcSubscriber {
    type Error = GrpcError;

    /// Waits for the next stream item. Cancel safe (see the module docs). A
    /// buffer too small for key plus payload is
    /// [`GrpcError::BufferTooSmall`] and drops that sample.
    async fn recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Sample<'b>, GrpcError> {
        let item = self.stream.message().await?.ok_or(GrpcError::StreamEnded)?;
        item_into(&item, buf)
    }

    /// Polls the stream once with a no-op waker: an item already received by
    /// the connection is returned, otherwise `Ok(None)`. Partial messages
    /// stay buffered in the stream.
    fn try_recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Sample<'b>>, GrpcError> {
        let mut cx = Context::from_waker(Waker::noop());
        match Pin::new(&mut self.stream).poll_next(&mut cx) {
            Poll::Pending => Ok(None),
            Poll::Ready(None) => Err(GrpcError::StreamEnded),
            Poll::Ready(Some(Err(status))) => Err(status.into()),
            Poll::Ready(Some(Ok(item))) => item_into(&item, buf).map(Some),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_path_needs_two_segments() {
        let p = method_path(Key::from_static("pkg.Svc/Call")).unwrap();
        assert_eq!(p.as_str(), "/pkg.Svc/Call");
        assert!(matches!(
            method_path(Key::from_static("a")),
            Err(GrpcError::NotServiceMethod)
        ));
        assert!(matches!(
            method_path(Key::from_static("a/b/c")),
            Err(GrpcError::NotServiceMethod)
        ));
    }
}
