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
//!
//! # Client identity
//!
//! Each transport sends its [`GrpcTransport::client_id`] in
//! [`CLIENT_HEADER`] on every publish and subscribe. Its subscribers receive
//! the publisher's id in [`SampleMeta::sender`] (`None` for publishers that
//! sent none) and, unless [`GrpcTransport::with_local_delivery`] is set, no
//! samples the transport (or a clone of it) published itself.
//! [`SampleMeta::seq`] is always `None`: the bus carries no sequence.

use super::GrpcError;
use super::bus::{
    CLIENT_HEADER, FILTER_HEADER, KEY_HEADER, PUBLISH_PATH, SELF_DELIVER, SELF_HEADER, SELF_REFUSE,
    SUBSCRIBE_PATH, client_id_header, decode_item_with_sender, filter_header,
};
use super::codec::RawCodec;
use super::{GrpcSubscriberKind, MAX_PAYLOAD};
use crate::dedup::fnv1a64;
use crate::key::{Key, KeyFilter, MAX_KEY_LEN};
use crate::transport::{
    Delivery, Limits, Overflow, Publisher, RequestOptions, Requester, Sample, SampleMeta, SenderId,
    Subscribe, Subscriber, Target, copy_into, sample_into,
};
use bytes::Bytes;
use core::pin::Pin;
use core::sync::atomic::{AtomicU64, Ordering};
use core::task::{Context, Poll, Waker};
use http::uri::PathAndQuery;
use tokio_stream::Stream;
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

/// A client id that differs between transports of one process (a
/// counter) and, with overwhelming probability, between processes and runs
/// (process id and wall-clock time, hashed with FNV-1a 64).
pub fn fresh_client_id() -> SenderId {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut seed = [0u8; 28];
    seed[..8].copy_from_slice(&n.to_le_bytes());
    seed[8..12].copy_from_slice(&std::process::id().to_le_bytes());
    seed[12..].copy_from_slice(&nanos.to_le_bytes());
    SenderId(fnv1a64(&seed).to_le_bytes())
}

/// gRPC client implementing [`Requester`], [`Publisher`] and [`Subscribe`].
///
/// Clones share the client id, so they count as one client for the
/// self-delivery rule (see the module docs).
#[derive(Debug, Clone)]
pub struct GrpcTransport<T = Channel> {
    grpc: Grpc<T>,
    client_id: SenderId,
    local_delivery: bool,
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
    /// Wraps an existing channel or intercepted service, with a
    /// [`fresh_client_id`] and no self-delivery.
    pub fn new(inner: T) -> Self {
        Self {
            grpc: Grpc::new(inner),
            client_id: fresh_client_id(),
            local_delivery: false,
        }
    }

    /// The transport identified as `id` from now on (for a stable identity
    /// across restarts, or to make two transports count as one client).
    pub fn with_client_id(self, id: SenderId) -> Self {
        Self {
            client_id: id,
            ..self
        }
    }

    /// Returns a transport whose subscriptions, opened from now on, also
    /// receive its own publications (`true`), or only other clients'
    /// (`false`, the default).
    pub fn with_local_delivery(self, local_delivery: bool) -> Self {
        Self {
            local_delivery,
            ..self
        }
    }

    /// The id sent in [`CLIENT_HEADER`] and reported to other clients'
    /// subscribers in [`SampleMeta::sender`].
    pub fn client_id(&self) -> SenderId {
        self.client_id
    }

    /// The [`CLIENT_HEADER`] metadata value of this transport.
    fn client_value(&self) -> Result<MetadataValue<tonic::metadata::Ascii>, GrpcError> {
        MetadataValue::try_from(client_id_header(self.client_id))
            .map_err(|_| GrpcError::InvalidHeader)
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
    type Subscriber<'a>
        = GrpcSubscriber
    where
        Self: 'a;
    type Error = GrpcError;

    /// Opens a `middleware.v1.Bus/Subscribe` stream for `filter` (the key in
    /// the body, the kind in [`FILTER_HEADER`], this client in
    /// [`CLIENT_HEADER`] and the self-delivery choice in [`SELF_HEADER`]);
    /// the server filters with [`KeyFilter::matches`].
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
        request
            .metadata_mut()
            .insert(CLIENT_HEADER, self.client_value()?);
        let own = if self.local_delivery {
            SELF_DELIVER
        } else {
            SELF_REFUSE
        };
        request
            .metadata_mut()
            .insert(SELF_HEADER, MetadataValue::from_static(own));
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

    /// A unary call on `/Service/Method`. `options.timeout` is sent as the
    /// `grpc-timeout` deadline and enforced locally: when it elapses the
    /// call is abandoned with status `DEADLINE_EXCEEDED`
    /// ([`HandlerError::DeadlineExceeded`](crate::HandlerError::DeadlineExceeded)
    /// through [`GrpcError::handler_error`]). Without one, the channel's own
    /// timeout (if configured) bounds the call. gRPC addresses services,
    /// not nodes: [`Target::Node`] is [`GrpcError::UnsupportedTarget`].
    async fn request_with<'b>(
        &mut self,
        key: Key<'_>,
        request: &[u8],
        response: &'b mut [u8],
        options: &RequestOptions,
    ) -> Result<&'b [u8], GrpcError> {
        if options.target != Target::Any {
            return Err(GrpcError::UnsupportedTarget);
        }
        let path = method_path(key)?;
        let mut call = tonic::Request::new(Bytes::copy_from_slice(request));
        let reply = match options.timeout {
            Some(d) => {
                call.set_timeout(d);
                tokio::time::timeout(d, self.unary(path, call))
                    .await
                    .map_err(|_| GrpcError::Status {
                        code: tonic::Code::DeadlineExceeded,
                        message: "client deadline elapsed".to_owned(),
                    })??
            }
            None => self.unary(path, call).await?,
        };
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
        request
            .metadata_mut()
            .insert(CLIENT_HEADER, self.client_value()?);
        self.unary(PathAndQuery::from_static(PUBLISH_PATH), request)
            .await?;
        Ok(())
    }
}

/// One `Subscribe` stream; implements [`Subscriber`]. Samples carry their
/// own key (a prefix subscription receives many) and the publisher's client
/// id as [`SampleMeta::sender`] when it sent one.
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

/// Copies one keyed stream item into `buf` as a [`Sample`] with the
/// publisher as its sender.
fn item_into<'b>(item: &[u8], buf: &'b mut [u8]) -> Result<Sample<'b>, GrpcError> {
    let (key, sender, payload) = decode_item_with_sender(item).ok_or(GrpcError::MalformedSample)?;
    let needed = key.as_bytes().len() + payload.len();
    let sample =
        sample_into(buf, key.as_str(), payload).ok_or(GrpcError::BufferTooSmall { needed })?;
    Ok(sample.with_meta(SampleMeta { sender, ..SampleMeta::NONE }))
}

impl Subscriber for GrpcSubscriber {
    type Error = GrpcError;

    /// The bus's broadcast channel drops the oldest samples for a lagging
    /// stream; one stream is ordered. No gap is reported (`gap: None`).
    const DELIVERY: Delivery = Delivery::new(Overflow::DropOldest, true);
    const LIMITS: Limits = Limits::new(MAX_KEY_LEN, MAX_PAYLOAD);

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

    #[tokio::test]
    async fn g8_fresh_client_ids_differ_and_ids_can_be_set() {
        let ids: std::collections::HashSet<_> = (0..64).map(|_| fresh_client_id().0).collect();
        assert_eq!(ids.len(), 64);
        let channel = tonic::transport::Endpoint::from_static("http://127.0.0.1:1").connect_lazy();
        let a = GrpcTransport::new(channel.clone());
        let b = GrpcTransport::new(channel);
        assert_ne!(a.client_id(), b.client_id());
        assert_eq!(
            a.clone().client_id(),
            a.client_id(),
            "clones are one client"
        );
        let fixed = SenderId([7; 8]);
        assert_eq!(b.with_client_id(fixed).client_id(), fixed);
    }

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
