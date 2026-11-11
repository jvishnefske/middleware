//! Client side: [`GrpcTransport`] (requests, publishes and subscriptions)
//! and [`GrpcSubscriber`] (a `Subscribe` stream).
//!
//! # Cancel safety
//!
//! * [`Subscriber::recv`] waits on the subscriber's local queue (below),
//!   a tokio `broadcast` receiver whose `recv` is cancel safe; dropping
//!   the wait loses nothing.
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
//! Requests carry the id too, so a [`GrpcResponder`](super::GrpcResponder)
//! hands it to the handler as `RequestMeta::sender`.
//!
//! # Subscriber queue and gaps
//!
//! [`Subscribe::subscribe`] spawns a task (on the current tokio runtime)
//! that reads the `Subscribe` stream as fast as it arrives into a local
//! queue of [`GrpcTransport::queue_depth`] samples, a tokio `broadcast`
//! channel: when the application falls behind, the **oldest** queued
//! samples are dropped (`Overflow::DropOldest`), deterministically, rather
//! than whatever HTTP/2 flow control and the bus's own buffer happen to
//! hold. The task ends when the stream does or the subscriber is dropped.
//!
//! Each delivered sample's [`SampleMeta::gap`] is `Some(n)`: the samples
//! dropped from the local queue immediately before it plus those the bus
//! reported lost to its broadcast lag ([`bus`](super::bus), "Gaps"). The
//! bus numbers no samples per sender, so `n` counts every sample the
//! stream lost, of any sender; it is exact for a single publisher.

use super::GrpcError;
use super::bus::{
    CLIENT_HEADER, DEFAULT_CAPACITY, FILTER_HEADER, GAP_HEADER, GAP_REPORT, KEY_HEADER,
    PUBLISH_PATH, SELF_DELIVER, SELF_HEADER, SELF_REFUSE, SUBSCRIBE_PATH, client_id_header,
    decode_item_with_gap, filter_header,
};
use super::codec::RawCodec;
use super::timeout::{GRPC_TIMEOUT_HEADER, GrpcTimeout};
use super::{GrpcSubscriberKind, MAX_PAYLOAD};
use crate::dedup::fnv1a64;
use crate::key::{Key, KeyFilter, MAX_KEY_LEN};
use crate::transport::{
    Delivery, Limits, Overflow, Publisher, RequestOptions, Requester, Sample, SampleMeta, SenderId,
    Subscribe, Subscriber, Target, copy_into, sample_into,
};
use bytes::Bytes;
use core::sync::atomic::{AtomicU64, Ordering};
use core::time::Duration;
use http::uri::PathAndQuery;
use tokio::sync::{broadcast, oneshot};
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
    queue_depth: usize,
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
            queue_depth: DEFAULT_CAPACITY,
        }
    }

    /// Returns a transport whose subscriptions, opened from now on, queue
    /// up to `depth` samples (see the module docs). The broadcast queue
    /// rounds up to a power of two, at least 1; [`queue_depth`](Self::queue_depth)
    /// reports the effective value. The default is the bus's
    /// [`DEFAULT_CAPACITY`] (64).
    pub fn with_queue_depth(self, depth: usize) -> Self {
        Self {
            queue_depth: depth.max(1).checked_next_power_of_two().unwrap_or(1 << 30),
            ..self
        }
    }

    /// Samples a subscription queues before the oldest is dropped.
    pub fn queue_depth(&self) -> usize {
        self.queue_depth
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

    /// One unary request call on `path` with this client's id and, with a
    /// `timeout`, its `grpc-timeout` and local deadline; the reply owned.
    /// The shared core of [`Requester::request_with`] and the mesh
    /// gather.
    pub(crate) async fn call(
        &mut self,
        path: PathAndQuery,
        body: Bytes,
        timeout: Option<Duration>,
    ) -> Result<Bytes, GrpcError> {
        let mut call = tonic::Request::new(body);
        call.metadata_mut()
            .insert(CLIENT_HEADER, self.client_value()?);
        let Some(d) = timeout else {
            return self.unary(path, call).await;
        };
        let wire = MetadataValue::try_from(GrpcTimeout::from_duration(d).to_string())
            .map_err(|_| GrpcError::InvalidHeader)?;
        call.metadata_mut().insert(GRPC_TIMEOUT_HEADER, wire);
        let deadline_exceeded = || GrpcError::Status {
            code: tonic::Code::DeadlineExceeded,
            message: "client deadline elapsed".to_owned(),
        };
        let start = tokio::time::Instant::now();
        match tokio::time::timeout(d, self.unary(path, call)).await {
            Err(_) => Err(deadline_exceeded()),
            // A tonic server enforcing `grpc-timeout` itself answers
            // CANCELLED when it expires; past our deadline that is ours.
            Ok(Err(GrpcError::Status {
                code: tonic::Code::Cancelled,
                ..
            })) if start.elapsed() >= d => Err(deadline_exceeded()),
            Ok(reply) => reply,
        }
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
        request
            .metadata_mut()
            .insert(GAP_HEADER, MetadataValue::from_static(GAP_REPORT));
        let resp = self
            .grpc
            .server_streaming(request, PathAndQuery::from_static(SUBSCRIBE_PATH), RawCodec)
            .await?;
        let kind = match filter {
            KeyFilter::Exact(_) => GrpcSubscriberKind::Exact,
            KeyFilter::Prefix(_) => GrpcSubscriberKind::Prefix,
        };
        let (tx, rx) = broadcast::channel(self.queue_depth);
        let (stop, stopped) = oneshot::channel();
        tokio::spawn(pump(resp.into_inner(), tx, stopped));
        Ok(GrpcSubscriber {
            key: key.as_str().to_owned(),
            kind,
            queue: rx,
            pending: None,
            lost: 0,
            _stop: stop,
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

    /// A unary call on `/Service/Method` carrying this client's id in
    /// [`CLIENT_HEADER`]. `options.timeout` is sent as the `grpc-timeout`
    /// deadline ([`GrpcTimeout`]) and enforced locally: when it elapses
    /// the call is abandoned with status `DEADLINE_EXCEEDED`
    /// ([`HandlerError::DeadlineExceeded`](crate::HandlerError::DeadlineExceeded)
    /// through [`GrpcError::handler_error`]); a `CANCELLED` from a server
    /// that enforced the header itself, arriving after the deadline, is
    /// reported the same way. Without one, the channel's own timeout (if
    /// configured) bounds the call; a plain channel has none, so the caller
    /// bounds it. One channel addresses one service, not nodes:
    /// [`Target::Node`] is [`GrpcError::UnsupportedTarget`] (the mesh
    /// client, `grpc::mesh::MeshClient` with feature `istio`, routes by
    /// node).
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
        let reply = self
            .call(path, Bytes::copy_from_slice(request), options.timeout)
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
        request
            .metadata_mut()
            .insert(CLIENT_HEADER, self.client_value()?);
        self.unary(PathAndQuery::from_static(PUBLISH_PATH), request)
            .await?;
        Ok(())
    }
}

/// What the pump task forwards from the stream to the local queue.
#[derive(Debug, Clone)]
enum Pumped {
    /// A raw stream item, decoded by the subscriber.
    Item(Bytes),
    /// The stream failed with this status.
    Failed(tonic::Code, String),
}

/// Reads `stream` into `queue` until the stream ends or fails, or the
/// subscriber drops its end of `stop`. Dropping `queue` (on return) tells
/// the subscriber the stream is over once it has drained the queue.
async fn pump(
    mut stream: Streaming<Bytes>,
    queue: broadcast::Sender<Pumped>,
    mut stop: oneshot::Receiver<()>,
) {
    // One iteration per stream message; the loop lives as long as the
    // subscription.
    loop {
        let message = tokio::select! {
            _ = &mut stop => return,
            m = stream.message() => m,
        };
        let pumped = match message {
            Ok(Some(item)) => Pumped::Item(item),
            Ok(None) => return,
            Err(status) => Pumped::Failed(status.code(), status.message().to_owned()),
        };
        let failed = matches!(pumped, Pumped::Failed(..));
        // An error means the subscriber is gone.
        if queue.send(pumped).is_err() || failed {
            return;
        }
    }
}

/// One `Subscribe` stream; implements [`Subscriber`]. Samples carry their
/// own key (a prefix subscription receives many), the publisher's client
/// id as [`SampleMeta::sender`] when it sent one, and the gap (module
/// docs).
#[derive(Debug)]
pub struct GrpcSubscriber {
    key: String,
    kind: GrpcSubscriberKind,
    queue: broadcast::Receiver<Pumped>,
    /// A sample that did not fit the caller's buffer, kept for the retry.
    pending: Option<Pumped>,
    /// Samples dropped from the local queue since the last delivery.
    lost: u16,
    /// Dropped with the subscriber, which stops the pump task.
    _stop: oneshot::Sender<()>,
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

    /// Notes `n` samples dropped from the local queue.
    fn lagged(&mut self, n: u64) {
        let n = u16::try_from(n).unwrap_or(u16::MAX);
        self.lost = self.lost.saturating_add(n);
    }

    /// Copies `pumped` into `buf` as a [`Sample`]; a sample too large for
    /// `buf` is kept for the next call.
    fn deliver<'b>(&mut self, pumped: Pumped, buf: &'b mut [u8]) -> Result<Sample<'b>, GrpcError> {
        let item = match &pumped {
            Pumped::Item(item) => item.clone(),
            Pumped::Failed(code, message) => {
                return Err(GrpcError::Status {
                    code: *code,
                    message: message.clone(),
                });
            }
        };
        let decoded = decode_item_with_gap(&item).ok_or(GrpcError::MalformedSample)?;
        let needed = decoded.key.as_bytes().len() + decoded.payload.len();
        if buf.len() < needed {
            self.pending = Some(pumped);
            return Err(GrpcError::BufferTooSmall { needed });
        }
        let sample = sample_into(buf, decoded.key.as_str(), decoded.payload)
            .ok_or(GrpcError::MalformedSample)?;
        let gap = self.lost.saturating_add(decoded.gap);
        self.lost = 0;
        Ok(sample.with_meta(
            SampleMeta {
                sender: decoded.sender,
                ..SampleMeta::NONE
            }
            .with_gap(Some(gap)),
        ))
    }
}

impl Subscriber for GrpcSubscriber {
    type Error = GrpcError;

    /// The local queue drops the oldest samples when the application falls
    /// behind, and the bus's broadcast channel does the same for a lagging
    /// stream; one stream is ordered. Both losses are reported in
    /// [`SampleMeta::gap`] (module docs).
    const DELIVERY: Delivery = Delivery::new(Overflow::DropOldest, true);
    const LIMITS: Limits = Limits::new(MAX_KEY_LEN, MAX_PAYLOAD);

    /// Waits for the next sample. Cancel safe (see the module docs). A
    /// buffer too small for key plus payload is
    /// [`GrpcError::BufferTooSmall`] and keeps the sample for a retry with
    /// a larger buffer. After the stream ends (or its failure was
    /// reported) every call is [`GrpcError::StreamEnded`].
    async fn recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Sample<'b>, GrpcError> {
        if let Some(p) = self.pending.take() {
            return self.deliver(p, buf);
        }
        // Each lag notice moves the receiver to the oldest queued sample,
        // so the next `recv` returns one unless the producer overtakes it
        // again.
        loop {
            match self.queue.recv().await {
                Ok(p) => return self.deliver(p, buf),
                Err(broadcast::error::RecvError::Lagged(n)) => self.lagged(n),
                Err(broadcast::error::RecvError::Closed) => return Err(GrpcError::StreamEnded),
            }
        }
    }

    /// Returns a sample already in the local queue, otherwise `Ok(None)`;
    /// never waits.
    fn try_recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Sample<'b>>, GrpcError> {
        if let Some(p) = self.pending.take() {
            return self.deliver(p, buf).map(Some);
        }
        // As in `recv`: a lag notice is followed by a queued sample.
        loop {
            match self.queue.try_recv() {
                Ok(p) => return self.deliver(p, buf).map(Some),
                Err(broadcast::error::TryRecvError::Lagged(n)) => self.lagged(n),
                Err(broadcast::error::TryRecvError::Empty) => return Ok(None),
                Err(broadcast::error::TryRecvError::Closed) => return Err(GrpcError::StreamEnded),
            }
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
