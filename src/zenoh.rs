//! Zenoh backend (feature `zenoh`).
//!
//! [`ZenohTransport`] wraps an opened [`::zenoh::Session`] and implements the
//! byte-level traits of [`crate::transport`] on top of it: [`Publisher`],
//! [`Subscribe`], [`Requester`] and [`Gather`]. It creates
//! [`ZenohSubscriber`]s, [`ZenohResponder`]s and a [`ZenohDiscovery`]
//! ([`Announce`](crate::discovery::Announce) and
//! [`Browse`](crate::discovery::Browse) over liveliness tokens).
//!
//! Submodules: [`wire`] (pure attachment and key encodings),
//! [`subscriber`], [`responder`], [`discovery`].
//!
//! # Key mapping
//!
//! A [`Key`] is used verbatim as a Zenoh key expression: `sensor/temperature`
//! becomes the key expression `sensor/temperature`. Keys never contain
//! wildcards (see [`crate::key`]), so every key expression this module
//! declares or targets for a key is a single concrete resource. The
//! backend's own key expressions start with the verbatim chunk `@mw`
//! ([`wire`]), which application keys and wildcards never match.
//!
//! # Identity
//!
//! A session's [`SenderId`] is the 64-bit FNV-1a hash of its Zenoh id
//! ([`sender_id_of`]); every clone of a transport, and every responder and
//! discovery handle made from it, has the same one. It is carried in
//! attachments ([`wire`]), not taken from Zenoh's source info, which is
//! unstable API in Zenoh 1.x.
//!
//! # Publish / subscribe
//!
//! * [`Publisher::publish`] is a Zenoh `put` of the payload on the key with
//!   congestion control **Block** and a 16-byte attachment (sender id,
//!   sequence, stream id; [`wire`]). Sequences count per key per transport
//!   value: each key published through one [`ZenohTransport`] value is its
//!   own stream numbered from 0, and a clone starts new streams (it gets a
//!   fresh origin), so a subscriber to any subset of keys sees exact gaps.
//!   The publisher keeps one 8-bit counter per key it has published on.
//! * [`Subscribe::subscribe`] declares a Zenoh subscriber with a FIFO of
//!   [`ZenohTransport::queue_depth`] samples (Zenoh's default 256, set with
//!   [`ZenohTransport::with_queue_depth`]) and returns a
//!   [`ZenohSubscriber`]. [`KeyFilter::Exact`] maps to the key expression
//!   `k`; [`KeyFilter::Prefix`] maps to `k/**`, which in Zenoh matches `k`
//!   itself as well as every key below it, and never `kx`.
//!   [`ZenohTransport::subscriber`] is the exact-key shorthand.
//! * **Delivery is [`Overflow::Block`]** with per-sender order (see
//!   [`subscriber`] for the evidence and caveats): a full FIFO blocks
//!   Zenoh's delivery, the link stops being read, and once the publisher's
//!   transmission queue is full its `put` waits.
//! * **No self-delivery:** subscribers are declared with
//!   `allowed_origin(Locality::Remote)`, so a transport never receives its
//!   own session's publications. [`ZenohTransport::with_local_delivery`]
//!   turns this off (needed for a single isolated session talking to
//!   itself, as in tests). With local delivery a full FIFO blocks the
//!   publishing thread itself, so the subscriber must be drained from
//!   another task.
//!
//! # Request / response
//!
//! * [`Requester::request_with`] is a Zenoh `get` with no consolidation,
//!   the request as the query payload and a query attachment carrying this
//!   transport's sender id and the timeout in milliseconds. The timeout is
//!   `options.timeout`, or the transport's ([`DEFAULT_TIMEOUT`] unless
//!   changed with [`ZenohTransport::with_timeout`]); Zenoh enforces it as
//!   the query timeout. [`Target::Any`] queries the key (Zenoh picks the
//!   best matching queryable); [`Target::Node`] queries that node's
//!   per-node key `@mw/node/<zid16>/<key>`, which only its responder
//!   declares. The first successful reply is copied into the response
//!   buffer. If only error replies arrive, the last one becomes
//!   [`ZenohError::Handler`]. Zenoh's own timeout error (a reply error
//!   with the payload `Timeout`, sent by the querying session or by the
//!   peer that routed the query) is not a responder's reply and is skipped
//!   by both calls. No reply at all is [`ZenohError::NoReply`]:
//!   at once when no queryable matches (Zenoh finalises such a query
//!   immediately, e.g. a `Target::Node` with no responder on that node),
//!   otherwise when the timeout elapses.
//! * [`Gather::request_each`] is the same query with `QueryTarget::All`:
//!   every responder of the key answers, and each reply is passed to the
//!   callback with the replier's [`SenderId`] (from the reply attachment,
//!   or the reply-error payload) and its result. It stops after
//!   `max_replies`, on [`Flow::Stop`], when Zenoh finalises the query
//!   (every matching queryable answered) or at the timeout.
//! * [`ZenohTransport::responder`] declares two queryables feeding one FIFO
//!   ([`responder`]): one on the key and one on the per-node key. The
//!   handler sees the requester's sender id and the deadline it carried.
//!
//! # Cancel safety
//!
//! Zenoh's `put`, `get`, `reply` and declaration futures are ready on first
//! poll (they resolve synchronously; a `put` may block the thread under
//! congestion, see above), so the only suspension points are channel
//! receives:
//!
//! * [`Subscriber::recv`], [`Responder::serve_one`] and
//!   [`Browse::next`](crate::discovery::Browse::next) wait on FIFO channels
//!   whose receive future leaves the item in the queue when dropped, so
//!   cancelling loses no sample, query or event.
//! * [`Requester::request_with`] and [`Gather::request_each`] own the reply
//!   channel of their own `get`; dropping them abandons that query and its
//!   late replies go nowhere.
//! * [`Publisher::publish`] completes on its first poll: once polled it has
//!   been sent exactly once.
//!
//! # Limits
//!
//! [`ZenohTransport`]'s [`Publisher::LIMITS`] allow keys up to
//! [`MAX_KEY_LEN`] and payloads up to [`MAX_PAYLOAD`], which leaves 1 KiB of
//! Zenoh's default 1 GiB `max_message_size` for key, attachment and headers.
//!
//! # Session configuration
//!
//! [`SessionConfig`] builds a [`::zenoh::Config`] with multicast scouting and
//! gossip disabled and only the explicitly listed listen and connect
//! endpoints, so opening a session generates no discovery traffic. With no
//! endpoints at all the session is isolated; Zenoh still delivers puts and
//! queries between declarations inside that one session.

pub mod discovery;
pub mod responder;
pub mod subscriber;
pub mod wire;

pub use discovery::ZenohDiscovery;
pub use responder::ZenohResponder;
pub use subscriber::ZenohSubscriber;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use ::zenoh::Session;
use ::zenoh::handlers::FifoChannelHandler;
use ::zenoh::qos::CongestionControl;
use ::zenoh::query::{ConsolidationMode, QueryTarget, Reply as ZenohReply};
use ::zenoh::sample::Locality;

use crate::dedup::{Sequence, fnv1a64};
use crate::key::{Key, KeyError, KeyFilter, MAX_KEY_LEN};
use crate::transport::{
    Flow, Gather, HandlerError, Limits, Publisher, Reply, RequestOptions, Requester, SenderId,
    Subscribe, Target, copy_into,
};

#[cfg(doc)]
use crate::transport::{Overflow, Responder, Subscriber};

/// Request timeout used by [`ZenohTransport::new`].
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Subscriber and responder FIFO depth used by [`ZenohTransport::new`]:
/// Zenoh's own default channel capacity.
pub const DEFAULT_QUEUE_DEPTH: usize = 256;

/// Largest payload [`ZenohTransport`] advertises in [`Publisher::LIMITS`]:
/// Zenoh's default `transport/link/rx/max_message_size` (1 GiB) minus 1 KiB
/// for the key, the attachment and Zenoh's own headers.
pub const MAX_PAYLOAD: usize = (1 << 30) - 1024;

/// The [`SenderId`] of a Zenoh session: the 64-bit FNV-1a hash of its
/// 16-byte Zenoh id (little-endian), so distinct sessions get distinct ids
/// with overwhelming probability.
pub fn sender_id_of(zid: ::zenoh::session::ZenohId) -> SenderId {
    SenderId(fnv1a64(&zid.to_le_bytes()).to_le_bytes())
}

/// The key expression of node `id`'s queryable for `key`
/// (`@mw/node/<zid16>/<key>`), which [`Target::Node`] requests query.
pub fn node_key(id: SenderId, key: Key<'_>) -> String {
    wire::node_key(id, key)
}

/// The Zenoh key expression selecting what `filter` selects.
fn key_expr(filter: KeyFilter<'_>) -> String {
    match filter {
        KeyFilter::Exact(k) => k.as_str().to_owned(),
        KeyFilter::Prefix(k) => format!("{k}/**"),
    }
}

/// Failure of a Zenoh transport operation.
#[derive(Debug)]
#[non_exhaustive]
pub enum ZenohError {
    /// Zenoh rejected or failed an operation (config, open, declare, put,
    /// get, reply).
    Zenoh(::zenoh::Error),
    /// The received data needs `needed` bytes but the buffer holds only
    /// `capacity`. Nothing was written past the buffer and nothing truncated.
    BufferTooSmall {
        /// Bytes required.
        needed: usize,
        /// Bytes available.
        capacity: usize,
    },
    /// A received sample's key expression is not a valid [`Key`].
    InvalidKey(KeyError),
    /// The responder answered with a reply error carrying this code.
    Handler(HandlerError),
    /// No reply arrived before the query completed or timed out.
    NoReply,
    /// The subscriber or queryable channel was closed (session closed or
    /// declaration undeclared).
    Closed,
    /// An endpoint string given to [`SessionConfig`] contains a character
    /// that cannot be embedded in the configuration (`"` or `\`).
    InvalidEndpoint,
    /// A service instance name is not a single plain Zenoh key chunk
    /// (empty, contains `/`, `*`, `$`, `?` or `#`, or starts with `@`).
    InvalidInstance,
    /// A service record could not be encoded, or the record fetched for a
    /// liveliness token could not be decoded.
    Record(crate::discovery::beacon::BeaconError),
    /// [`Browse::next`](crate::discovery::Browse::next) or `try_next` was
    /// called before [`Browse::browse`](crate::discovery::Browse::browse).
    NotBrowsing,
}

impl core::fmt::Display for ZenohError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ZenohError::Zenoh(e) => write!(f, "zenoh error: {e}"),
            ZenohError::BufferTooSmall { needed, capacity } => {
                write!(f, "buffer too small: need {needed} bytes, have {capacity}")
            }
            ZenohError::InvalidKey(e) => write!(f, "received invalid key: {e:?}"),
            ZenohError::Handler(e) => write!(f, "responder returned error: {e:?}"),
            ZenohError::NoReply => f.write_str("no reply received"),
            ZenohError::Closed => f.write_str("zenoh channel closed"),
            ZenohError::InvalidEndpoint => f.write_str("invalid endpoint string"),
            ZenohError::InvalidInstance => f.write_str("instance is not a plain key chunk"),
            ZenohError::Record(e) => write!(f, "service record: {e:?}"),
            ZenohError::NotBrowsing => f.write_str("browse was not called"),
        }
    }
}

impl std::error::Error for ZenohError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ZenohError::Zenoh(e) => Some(e.as_ref()),
            _ => None,
        }
    }
}

impl From<::zenoh::Error> for ZenohError {
    fn from(e: ::zenoh::Error) -> Self {
        ZenohError::Zenoh(e)
    }
}

/// Builder for a discovery-free [`::zenoh::Config`].
///
/// The built config is in `peer` mode with multicast scouting and gossip
/// disabled, listening and connecting only on the endpoints added here
/// (for example `tcp/127.0.0.1:7447`). With none added the session neither
/// listens nor connects.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionConfig {
    listen: Vec<String>,
    connect: Vec<String>,
}

impl SessionConfig {
    /// An isolated configuration: no endpoints, no scouting.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an endpoint to listen on.
    pub fn listen(mut self, endpoint: impl Into<String>) -> Self {
        self.listen.push(endpoint.into());
        self
    }

    /// Adds an endpoint to connect to.
    pub fn connect(mut self, endpoint: impl Into<String>) -> Self {
        self.connect.push(endpoint.into());
        self
    }

    /// Produces the Zenoh configuration.
    pub fn build(&self) -> Result<::zenoh::Config, ZenohError> {
        let mut config = ::zenoh::Config::default();
        config.insert_json5("mode", r#""peer""#)?;
        config.insert_json5("scouting/multicast/enabled", "false")?;
        config.insert_json5("scouting/gossip/enabled", "false")?;
        config.insert_json5("listen/endpoints", &json_array(&self.listen)?)?;
        config.insert_json5("connect/endpoints", &json_array(&self.connect)?)?;
        Ok(config)
    }
}

/// Renders endpoint strings as a JSON5 array, refusing characters that would
/// need escaping.
fn json_array(items: &[String]) -> Result<String, ZenohError> {
    if items.iter().any(|s| s.contains(['"', '\\'])) {
        return Err(ZenohError::InvalidEndpoint);
    }
    let quoted: Vec<String> = items.iter().map(|s| format!("\"{s}\"")).collect();
    Ok(format!("[{}]", quoted.join(",")))
}

/// Source of per-value publication origins (see [`wire::stream_id`]).
static NEXT_ORIGIN: AtomicU32 = AtomicU32::new(0);

fn next_origin() -> u32 {
    NEXT_ORIGIN.fetch_add(1, Ordering::Relaxed)
}

/// Publisher, requester and gatherer over one Zenoh session; also the
/// factory for [`ZenohSubscriber`], [`ZenohResponder`] and
/// [`ZenohDiscovery`].
///
/// Cloning shares the session (and so the sender id) but starts fresh
/// publication streams: the clone has its own origin and sequence counters.
#[derive(Debug)]
pub struct ZenohTransport {
    session: Session,
    timeout: Duration,
    queue_depth: usize,
    sender: SenderId,
    origin: u32,
    streams: BTreeMap<String, Sequence>,
    local_delivery: bool,
}

impl Clone for ZenohTransport {
    fn clone(&self) -> Self {
        Self {
            session: self.session.clone(),
            timeout: self.timeout,
            queue_depth: self.queue_depth,
            sender: self.sender,
            origin: next_origin(),
            streams: BTreeMap::new(),
            local_delivery: self.local_delivery,
        }
    }
}

impl ZenohTransport {
    /// Wraps an already opened session, using [`DEFAULT_TIMEOUT`] for
    /// requests, [`DEFAULT_QUEUE_DEPTH`] for FIFOs and no self-delivery.
    pub fn new(session: Session) -> Self {
        let sender = sender_id_of(session.zid());
        Self {
            session,
            timeout: DEFAULT_TIMEOUT,
            queue_depth: DEFAULT_QUEUE_DEPTH,
            sender,
            origin: next_origin(),
            streams: BTreeMap::new(),
            local_delivery: false,
        }
    }

    /// Opens a session from `config` and wraps it.
    pub async fn open(config: ::zenoh::Config) -> Result<Self, ZenohError> {
        let session = ::zenoh::open(config).await?;
        Ok(Self::new(session))
    }

    /// Returns a transport whose requests without their own timeout time
    /// out after `timeout`.
    pub fn with_timeout(self, timeout: Duration) -> Self {
        Self { timeout, ..self }
    }

    /// Returns a transport whose subscribers and responders, declared from
    /// now on, queue up to `depth` samples or queries (at least 1).
    pub fn with_queue_depth(self, depth: usize) -> Self {
        Self {
            queue_depth: depth.max(1),
            ..self
        }
    }

    /// Returns a transport whose subscribers, declared from now on, also
    /// receive publications made through this same session (`true`), or only
    /// publications from other sessions (`false`, the default).
    pub fn with_local_delivery(self, local_delivery: bool) -> Self {
        Self {
            local_delivery,
            ..self
        }
    }

    /// The id this transport stamps on its publications, requests and
    /// replies, and that [`Target::Node`] addresses.
    pub fn sender_id(&self) -> SenderId {
        self.sender
    }

    /// The default request timeout.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// The FIFO depth of subscribers and responders declared from now on.
    pub fn queue_depth(&self) -> usize {
        self.queue_depth
    }

    /// The underlying session.
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Declares a subscriber on exactly `key`; shorthand for
    /// [`Subscribe::subscribe`] with [`KeyFilter::Exact`].
    pub async fn subscriber(&self, key: Key<'_>) -> Result<ZenohSubscriber, ZenohError> {
        self.declare(KeyFilter::Exact(key)).await
    }

    async fn declare(&self, filter: KeyFilter<'_>) -> Result<ZenohSubscriber, ZenohError> {
        let origin = if self.local_delivery {
            Locality::Any
        } else {
            Locality::Remote
        };
        let inner = self
            .session
            .declare_subscriber(key_expr(filter))
            .with(::zenoh::handlers::FifoChannel::new(self.queue_depth))
            .allowed_origin(origin)
            .await?;
        Ok(ZenohSubscriber::new(inner))
    }

    /// Declares the queryables of a responder on `key` (the key and its
    /// per-node key) that answers with a [`Handler`](crate::transport::Handler).
    pub async fn responder<'k>(&self, key: Key<'k>) -> Result<ZenohResponder<'k>, ZenohError> {
        ZenohResponder::declare(&self.session, self.sender, key, self.queue_depth).await
    }

    /// A discovery handle on this session, fetching records with this
    /// transport's timeout.
    pub fn discovery(&self) -> ZenohDiscovery {
        ZenohDiscovery::new(self.session.clone()).with_timeout(self.timeout)
    }

    /// Sends the query for `key` per `options` with `target` routing.
    fn query(
        &self,
        key: Key<'_>,
        request: &[u8],
        options: &RequestOptions,
        target: QueryTarget,
    ) -> Result<FifoChannelHandler<ZenohReply>, ZenohError> {
        use ::zenoh::Wait;
        let timeout = options.timeout.unwrap_or(self.timeout);
        let deadline_ms = options
            .with_timeout(timeout)
            .timeout_ms()
            .unwrap_or(u32::MAX);
        let selector = match options.target {
            Target::Any => key.as_str().to_owned(),
            Target::Node(id) => wire::node_key(id, key),
        };
        let attachment = wire::encode_query(self.sender, deadline_ms);
        let replies = self
            .session
            .get(selector)
            .payload(request)
            .attachment(attachment.to_vec())
            .target(target)
            .consolidation(ConsolidationMode::None)
            .timeout(timeout)
            .wait()?;
        Ok(replies)
    }
}

impl Publisher for ZenohTransport {
    type Error = ZenohError;

    /// Keys up to [`MAX_KEY_LEN`], payloads up to [`MAX_PAYLOAD`].
    const LIMITS: Limits = Limits::new(MAX_KEY_LEN, MAX_PAYLOAD);

    /// A Zenoh `put` with congestion control Block, carrying the sender,
    /// the key's next sequence number and its stream id. The key's
    /// sequence advances even if Zenoh rejects the put.
    async fn publish(&mut self, key: Key<'_>, payload: &[u8]) -> Result<(), ZenohError> {
        let seq = self.streams.get(key.as_str()).copied().unwrap_or_default();
        self.streams.insert(key.as_str().to_owned(), seq.next());
        let attachment = wire::encode_pub(self.sender, seq, wire::stream_id(self.origin, key));
        self.session
            .put(key.as_str(), payload)
            .attachment(attachment.to_vec())
            .congestion_control(CongestionControl::Block)
            .await?;
        Ok(())
    }
}

impl Subscribe for ZenohTransport {
    type Subscriber<'a> = ZenohSubscriber;
    type Error = ZenohError;

    /// Declares a subscriber for `filter` (see the module docs for the key
    /// expressions and the self-delivery rule).
    async fn subscribe(&mut self, filter: KeyFilter<'_>) -> Result<ZenohSubscriber, ZenohError> {
        self.declare(filter).await
    }
}

/// Copies `src` into `dst`, reporting the sizes on failure.
fn copy_checked<'b>(dst: &'b mut [u8], src: &[u8]) -> Result<&'b mut [u8], ZenohError> {
    let capacity = dst.len();
    copy_into(dst, src).ok_or(ZenohError::BufferTooSmall {
        needed: src.len(),
        capacity,
    })
}

impl Requester for ZenohTransport {
    type Error = ZenohError;

    /// A Zenoh `get` (see the module docs): the first successful reply
    /// wins; the timeout is `options.timeout` or the transport's
    /// ([`DEFAULT_TIMEOUT`] unless set with `with_timeout`), enforced by
    /// Zenoh. No reply is [`ZenohError::NoReply`]; error replies only, the
    /// last as [`ZenohError::Handler`].
    async fn request_with<'b>(
        &mut self,
        key: Key<'_>,
        request: &[u8],
        response: &'b mut [u8],
        options: &RequestOptions,
    ) -> Result<&'b [u8], ZenohError> {
        let replies = self.query(key, request, options, QueryTarget::BestMatching)?;
        let mut last_error = None;
        // The channel closes once every responder has answered or the
        // timeout elapses, so this loop is bounded by the timeout.
        while let Ok(reply) = replies.recv_async().await {
            match reply.result() {
                Ok(sample) => {
                    let bytes = sample.payload().to_bytes();
                    return copy_checked(response, &bytes).map(|r| &*r);
                }
                Err(err) if is_zenoh_error(err) => {}
                Err(err) => {
                    last_error = Some(wire::decode_reply_error(&err.payload().to_bytes()).0)
                }
            }
        }
        Err(last_error.map_or(ZenohError::NoReply, ZenohError::Handler))
    }
}

/// True for the error Zenoh itself reports when a query times out, rather
/// than one a responder sent ([`wire::is_timeout_error`]).
fn is_zenoh_error(err: &::zenoh::query::ReplyError) -> bool {
    wire::is_timeout_error(&err.payload().to_bytes())
}

/// The sender and result of one Zenoh reply, the response copied into
/// `buf` (a response that does not fit is [`HandlerError::BufferTooSmall`]).
fn gathered<'b>(reply: &ZenohReply, buf: &'b mut [u8]) -> Reply<'b> {
    match reply.result() {
        Ok(sample) => {
            let sender = sample
                .attachment()
                .and_then(|a| wire::decode_reply(&a.to_bytes()));
            let bytes = sample.payload().to_bytes();
            let result = copy_into(buf, &bytes)
                .map(|r| &*r)
                .ok_or(HandlerError::BufferTooSmall);
            Reply { sender, result }
        }
        Err(err) => {
            let (error, sender) = wire::decode_reply_error(&err.payload().to_bytes());
            Reply {
                sender,
                result: Err(error),
            }
        }
    }
}

impl Gather for ZenohTransport {
    type Error = ZenohError;

    /// A Zenoh `get` with `QueryTarget::All` and no consolidation (see the
    /// module docs). Every reply, successful or an error, counts and is
    /// passed on with its replier's [`SenderId`]. Returns when
    /// `max_replies` were seen, on [`Flow::Stop`], when Zenoh finalises the
    /// query, or at the timeout; running out of time is not an error.
    async fn request_each<F>(
        &mut self,
        key: Key<'_>,
        request: &[u8],
        buf: &mut [u8],
        options: &RequestOptions,
        max_replies: usize,
        mut on_reply: F,
    ) -> Result<usize, ZenohError>
    where
        F: FnMut(Reply<'_>) -> Flow,
    {
        if max_replies == 0 {
            return Ok(0);
        }
        let replies = self.query(key, request, options, QueryTarget::All)?;
        let mut seen = 0;
        // Bounded by `max_replies` and by the query timeout, after which
        // Zenoh closes the channel.
        while seen < max_replies {
            let Ok(reply) = replies.recv_async().await else {
                break;
            };
            if reply.result().is_err_and(is_zenoh_error) {
                continue;
            }
            seen += 1;
            if on_reply(gathered(&reply, buf)) == Flow::Stop {
                break;
            }
        }
        Ok(seen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_rejects_unescapable_endpoints() {
        assert!(SessionConfig::new().listen("tcp/\"x").build().is_err());
        assert!(
            SessionConfig::new()
                .connect("tcp/127.0.0.1:7447")
                .build()
                .is_ok()
        );
    }

    #[test]
    fn filters_map_to_key_expressions() {
        let k = Key::from_static("app");
        assert_eq!(key_expr(KeyFilter::Exact(k)), "app");
        assert_eq!(key_expr(KeyFilter::Prefix(k)), "app/**");
        assert_eq!(
            <ZenohTransport as Publisher>::LIMITS,
            Limits::new(MAX_KEY_LEN, MAX_PAYLOAD)
        );
    }
}
