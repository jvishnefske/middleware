//! Zenoh backend (feature `zenoh`).
//!
//! [`ZenohTransport`] wraps an opened [`::zenoh::Session`] and implements the
//! byte-level traits of [`crate::transport`] on top of it.
//!
//! # Key mapping
//!
//! A [`Key`] is used verbatim as a Zenoh key expression: `sensor/temperature`
//! becomes the key expression `sensor/temperature`. Keys never contain
//! wildcards (see [`crate::key`]), so every key expression this module
//! declares or targets is a single concrete resource.
//!
//! # Behaviour
//!
//! * [`Publisher::publish`] is a Zenoh `put` of the payload on the key, with
//!   a 12-byte attachment `"mw" 1 [sender id: 8] [seq]` identifying this
//!   transport ([`ZenohTransport::sender_id`]) and numbering its publications
//!   (a wrapping `u8` per transport value).
//! * [`Subscribe::subscribe`] declares a Zenoh subscriber and returns a
//!   [`ZenohSubscriber`]. [`KeyFilter::Exact`] maps to the key expression
//!   `k`; [`KeyFilter::Prefix`] maps to the single key expression `k/**`,
//!   which in Zenoh matches `k` itself as well as every key below it (`**`
//!   matches zero or more chunks), and never `kx`.
//!   [`ZenohTransport::subscriber`] is the exact-key shorthand.
//! * [`Subscriber::recv`] waits for the next sample and writes the sample's
//!   key followed by its payload into the caller's buffer (via
//!   [`sample_into`]); [`Subscriber::try_recv`] does the same if a sample is
//!   already queued and returns `None` otherwise. A buffer that cannot hold
//!   both is [`ZenohError::BufferTooSmall`]; the sample is dropped, never
//!   truncated. When the attachment is present, [`SampleMeta`] carries the
//!   publisher's sender id and sequence number; samples from other Zenoh
//!   publishers have empty metadata.
//! * **No self-delivery:** subscribers are declared with
//!   `allowed_origin(Locality::Remote)`, so a transport never receives its
//!   own session's publications. [`ZenohTransport::with_local_delivery`]
//!   turns this off (needed for a single isolated session talking to
//!   itself, as in tests). Clones of a transport share one session and so
//!   one origin.
//! * [`Requester::request`] is a Zenoh `get` on the key carrying the request as
//!   the query payload, bounded by the transport's timeout
//!   ([`DEFAULT_TIMEOUT`] unless changed with [`ZenohTransport::with_timeout`]).
//!   The first successful reply is copied into the response buffer. If only
//!   error replies arrive, the last one becomes [`ZenohError::Handler`],
//!   carrying the [`HandlerError`] decoded from the first payload byte
//!   ([`HandlerError::from_code`]; an empty payload decodes as
//!   [`HandlerError::Internal`]). No reply at all within the timeout is
//!   [`ZenohError::NoReply`].
//! * [`ZenohTransport::responder`] declares a Zenoh queryable on one key and
//!   returns a [`ZenohResponder`]. Its [`Responder::serve_one`] receives one
//!   query, copies the query payload to the front of `scratch`, runs the pure
//!   [`Handler`] with the responder's key and the remainder of `scratch` as
//!   the response buffer, then replies on the responder's key with the
//!   response, or sends a Zenoh reply error whose payload is the single byte
//!   [`HandlerError::code`]. A request that does not fit `scratch` is answered
//!   with [`HandlerError::BufferTooSmall`] without calling the handler. In
//!   every one of these cases the query has been answered, so `serve_one`
//!   returns `Ok(())`; it fails only when Zenoh itself fails.
//!
//! # Cancel safety
//!
//! Zenoh's `put`, `reply` and declaration futures are ready on first poll
//! (they resolve synchronously), so the only suspension points are channel
//! receives:
//!
//! * [`Subscriber::recv`] and [`Responder::serve_one`] wait on a FIFO
//!   channel whose receive future leaves the item in the queue when dropped,
//!   so cancelling loses no sample or query. Once an item is dequeued the
//!   rest of the call completes without suspending.
//! * [`Requester::request`] owns the reply channel of its own `get`;
//!   dropping it abandons that query and its late replies go nowhere.
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

use std::time::Duration;

use ::zenoh::Session;
use ::zenoh::handlers::FifoChannelHandler;
use ::zenoh::query::Query;

use ::zenoh::sample::Locality;

use crate::dedup::{Sequence, fnv1a64};
use crate::key::{Key, KeyError, KeyFilter, MAX_KEY_LEN};
use crate::transport::{
    Delivery, Handler, HandlerError, Limits, Overflow, Publisher, Request, RequestMeta,
    RequestOptions, Requester, Responder, Sample, SampleMeta, SenderId, Subscribe, Subscriber,
    Target, copy_into, sample_into,
};

/// Request timeout used by [`ZenohTransport::new`].
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Largest payload [`ZenohTransport`] advertises in [`Publisher::LIMITS`]:
/// Zenoh's default `transport/link/rx/max_message_size` (1 GiB) minus 1 KiB
/// for the key, the attachment and Zenoh's own headers.
pub const MAX_PAYLOAD: usize = (1 << 30) - 1024;

/// Leading bytes of the attachment [`ZenohTransport`] puts on every
/// publication: `"mw"` and format version 1.
const ATTACHMENT_TAG: [u8; 3] = [b'm', b'w', 1];

/// Length of the publication attachment: tag, sender id, sequence number.
const ATTACHMENT_LEN: usize = ATTACHMENT_TAG.len() + 8 + 1;

/// The [`SenderId`] of a Zenoh session: the 64-bit FNV-1a hash of its
/// 16-byte Zenoh id (little-endian), so distinct sessions get distinct ids
/// with overwhelming probability.
pub fn sender_id_of(zid: ::zenoh::session::ZenohId) -> SenderId {
    SenderId(fnv1a64(&zid.to_le_bytes()).to_le_bytes())
}

/// The publication attachment for `sender` and `seq`.
fn encode_attachment(sender: SenderId, seq: Sequence) -> [u8; ATTACHMENT_LEN] {
    let mut out = [0u8; ATTACHMENT_LEN];
    let (tag, rest) = out.split_at_mut(ATTACHMENT_TAG.len());
    tag.copy_from_slice(&ATTACHMENT_TAG);
    let (id, last) = rest.split_at_mut(8);
    id.copy_from_slice(&sender.0);
    if let Some(b) = last.first_mut() {
        *b = seq.get();
    }
    out
}

/// Reads sender and sequence from a publication attachment; anything that is
/// not exactly our format yields empty metadata.
fn decode_attachment(bytes: &[u8]) -> SampleMeta {
    let parsed = (bytes.len() == ATTACHMENT_LEN)
        .then_some(bytes)
        .and_then(|b| b.strip_prefix(&ATTACHMENT_TAG[..]))
        .and_then(|rest| {
            let (id, seq) = rest.split_first_chunk::<8>()?;
            Some((SenderId(*id), *seq.first()?))
        });
    match parsed {
        Some((sender, seq)) => SampleMeta::from_sender(sender).with_seq(seq),
        None => SampleMeta::default(),
    }
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
    /// A request asked for [`Target::Node`]; not supported yet.
    UnsupportedTarget,
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
            ZenohError::UnsupportedTarget => f.write_str("zenoh requests cannot address a node yet"),
            ZenohError::Closed => f.write_str("zenoh channel closed"),
            ZenohError::InvalidEndpoint => f.write_str("invalid endpoint string"),
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

/// Publisher and requester over one Zenoh session; also the factory for
/// [`ZenohSubscriber`] and [`ZenohResponder`].
#[derive(Debug, Clone)]
pub struct ZenohTransport {
    session: Session,
    timeout: Duration,
    sender: SenderId,
    seq: Sequence,
    local_delivery: bool,
}

impl ZenohTransport {
    /// Wraps an already opened session, using [`DEFAULT_TIMEOUT`] for
    /// requests and no self-delivery.
    pub fn new(session: Session) -> Self {
        let sender = sender_id_of(session.zid());
        Self {
            session,
            timeout: DEFAULT_TIMEOUT,
            sender,
            seq: Sequence::default(),
            local_delivery: false,
        }
    }

    /// Opens a session from `config` and wraps it.
    pub async fn open(config: ::zenoh::Config) -> Result<Self, ZenohError> {
        let session = ::zenoh::open(config).await?;
        Ok(Self::new(session))
    }

    /// Returns a transport whose requests time out after `timeout`.
    pub fn with_timeout(self, timeout: Duration) -> Self {
        Self { timeout, ..self }
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

    /// The id this transport stamps on its publications and that remote
    /// subscribers report in [`SampleMeta::sender`].
    pub fn sender_id(&self) -> SenderId {
        self.sender
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
            .allowed_origin(origin)
            .await?;
        Ok(ZenohSubscriber { inner })
    }

    /// Declares a queryable on `key` that answers with a [`Handler`].
    pub async fn responder<'k>(&self, key: Key<'k>) -> Result<ZenohResponder<'k>, ZenohError> {
        let inner = self.session.declare_queryable(key.as_str()).await?;
        Ok(ZenohResponder { key, inner })
    }
}

impl Publisher for ZenohTransport {
    type Error = ZenohError;

    /// Keys up to [`MAX_KEY_LEN`], payloads up to [`MAX_PAYLOAD`].
    const LIMITS: Limits = Limits::new(MAX_KEY_LEN, MAX_PAYLOAD);

    /// A Zenoh `put` carrying the sender/sequence attachment. The sequence
    /// number advances even if Zenoh rejects the put.
    async fn publish(&mut self, key: Key<'_>, payload: &[u8]) -> Result<(), ZenohError> {
        let attachment = encode_attachment(self.sender, self.seq);
        self.seq = self.seq.next();
        self.session
            .put(key.as_str(), payload)
            .attachment(attachment.to_vec())
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

/// Decodes a reply-error payload into the handler error it carries.
fn decode_reply_error(payload: &[u8]) -> HandlerError {
    HandlerError::from_code(payload.first().copied().unwrap_or(0))
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

    /// A Zenoh `get`: the first successful reply wins. Zenoh enforces the
    /// query timeout: `options.timeout`, or the transport's
    /// ([`DEFAULT_TIMEOUT`] unless set with `with_timeout`); no reply in
    /// time is [`ZenohError::NoReply`]. [`Target::Node`] is
    /// [`ZenohError::UnsupportedTarget`].
    async fn request_with<'b>(
        &mut self,
        key: Key<'_>,
        request: &[u8],
        response: &'b mut [u8],
        options: &RequestOptions,
    ) -> Result<&'b [u8], ZenohError> {
        if options.target != Target::Any {
            return Err(ZenohError::UnsupportedTarget);
        }
        let timeout = options.timeout.unwrap_or(self.timeout);
        let replies = self
            .session
            .get(key.as_str())
            .payload(request)
            .timeout(timeout)
            .await?;
        let mut last_error = None;
        // The channel closes once every responder has answered or the
        // timeout elapses, so this loop is bounded by `timeout`.
        while let Ok(reply) = replies.recv_async().await {
            match reply.result() {
                Ok(sample) => {
                    let bytes = sample.payload().to_bytes();
                    return copy_checked(response, &bytes).map(|r| &*r);
                }
                Err(err) => last_error = Some(decode_reply_error(&err.payload().to_bytes())),
            }
        }
        Err(last_error.map_or(ZenohError::NoReply, ZenohError::Handler))
    }
}

/// Receives samples matching the filter it was declared for.
#[derive(Debug)]
pub struct ZenohSubscriber {
    inner: ::zenoh::pubsub::Subscriber<FifoChannelHandler<::zenoh::sample::Sample>>,
}

/// Copies a received Zenoh sample into `buf` as a [`Sample`] with metadata.
fn into_sample<'b>(
    sample: &::zenoh::sample::Sample,
    buf: &'b mut [u8],
) -> Result<Sample<'b>, ZenohError> {
    let key = sample.key_expr().as_str();
    let payload = sample.payload().to_bytes();
    let needed = key.len() + payload.len();
    if needed > buf.len() {
        return Err(ZenohError::BufferTooSmall {
            needed,
            capacity: buf.len(),
        });
    }
    Key::new(key).map_err(ZenohError::InvalidKey)?;
    let meta = sample
        .attachment()
        .map_or_else(SampleMeta::default, |a| decode_attachment(&a.to_bytes()));
    // Size and key validity were checked above, so `None` is unreachable;
    // it is still mapped to an error rather than unwrapped.
    sample_into(buf, key, &payload)
        .map(|s| s.with_meta(meta))
        .ok_or(ZenohError::BufferTooSmall {
            needed,
            capacity: needed,
        })
}

impl Subscriber for ZenohSubscriber {
    type Error = ZenohError;

    /// The subscriber's FIFO channel is bounded; when it is full Zenoh
    /// stops taking samples for it and the publisher's congestion control
    /// (drop, the default for `put`) discards the newest. Samples of one
    /// publisher arrive in order. No gap is reported (`gap: None`).
    const DELIVERY: Delivery = Delivery::new(Overflow::DropNewest, true);
    const LIMITS: Limits = Limits::new(MAX_KEY_LEN, MAX_PAYLOAD);

    /// Waits on the subscriber's FIFO channel. Cancel safe: a dropped wait
    /// leaves queued samples in the channel.
    async fn recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Sample<'b>, ZenohError> {
        let sample = self
            .inner
            .recv_async()
            .await
            .map_err(|_| ZenohError::Closed)?;
        into_sample(&sample, buf)
    }

    /// Dequeues a sample if one is queued; `Ok(None)` when the channel is
    /// empty, [`ZenohError::Closed`] when it is empty and closed.
    fn try_recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Sample<'b>>, ZenohError> {
        match self.inner.try_recv() {
            Ok(Some(sample)) => into_sample(&sample, buf).map(Some),
            Ok(None) => Ok(None),
            Err(_) => Err(ZenohError::Closed),
        }
    }
}

/// Answers queries on the one key it was declared for by running a pure
/// [`Handler`].
#[derive(Debug)]
pub struct ZenohResponder<'k> {
    key: Key<'k>,
    inner: ::zenoh::query::Queryable<FifoChannelHandler<Query>>,
}

impl ZenohResponder<'_> {
    /// The key this responder serves.
    pub fn key(&self) -> Key<'_> {
        self.key
    }
}

/// Runs `handler` with the request at the front of `scratch` and the rest as
/// the response buffer. Returns the response slice or the handler error.
fn run_handler<'s, H: Handler>(
    handler: &mut H,
    key: Key<'_>,
    request: &[u8],
    scratch: &'s mut [u8],
) -> Result<&'s [u8], HandlerError> {
    let request_len = request.len();
    copy_into(scratch, request).ok_or(HandlerError::BufferTooSmall)?;
    let (req, resp) = scratch.split_at_mut(request_len);
    // Caller identity and deadline are not carried over Zenoh queries yet.
    let n = handler.handle(&Request::new(key, req).with_meta(RequestMeta::NONE), resp)?;
    resp.get(..n).ok_or(HandlerError::BufferTooSmall)
}

impl Responder for ZenohResponder<'_> {
    type Error = ZenohError;

    async fn serve_one<H: Handler>(
        &mut self,
        handler: &mut H,
        scratch: &mut [u8],
    ) -> Result<(), ZenohError> {
        let query = self
            .inner
            .recv_async()
            .await
            .map_err(|_| ZenohError::Closed)?;
        let request = query.payload().map(|p| p.to_bytes()).unwrap_or_default();
        match run_handler(handler, self.key, &request, scratch) {
            Ok(response) => query.reply(self.key.as_str(), response).await?,
            Err(e) => query.reply_err([e.code()]).await?,
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_error_payload_decodes_code_byte() {
        for e in HandlerError::ALL {
            assert_eq!(decode_reply_error(&[e.code()]), e);
        }
        assert_eq!(decode_reply_error(&[]), HandlerError::Internal);
    }

    #[test]
    fn run_handler_splits_scratch_and_reports_errors() {
        let mut echo = crate::transport::handler_fn(|req, resp| {
            copy_into(resp, req.body)
                .map(|s| s.len())
                .ok_or(HandlerError::BufferTooSmall)
        });
        let key = Key::from_static("a/b");
        let mut scratch = [0u8; 8];
        assert_eq!(
            run_handler(&mut echo, key, b"abc", &mut scratch),
            Ok(&b"abc"[..])
        );
        assert_eq!(
            run_handler(&mut echo, key, b"abcde", &mut scratch),
            Err(HandlerError::BufferTooSmall)
        );
        assert_eq!(
            run_handler(&mut echo, key, b"too long for scratch", &mut scratch),
            Err(HandlerError::BufferTooSmall)
        );
    }

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
    fn attachment_round_trips_and_foreign_ones_are_ignored() {
        let id = SenderId([1, 2, 3, 4, 5, 6, 7, 8]);
        let a = encode_attachment(id, Sequence::new(9));
        assert_eq!(
            decode_attachment(&a),
            SampleMeta::from_sender(id).with_seq(9)
        );
        assert_eq!(decode_attachment(&a[..11]), SampleMeta::default());
        assert_eq!(decode_attachment(b"xx\x01123456789"), SampleMeta::default());
        assert_eq!(decode_attachment(b""), SampleMeta::default());
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
