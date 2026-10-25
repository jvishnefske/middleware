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
//! * [`Publisher::publish`] is a Zenoh `put` of the payload on the key.
//! * [`ZenohTransport::subscriber`] declares a Zenoh subscriber on one key and
//!   returns a [`ZenohSubscriber`]. Its [`Subscriber::recv`] waits for the next
//!   sample and writes the sample's key followed by its payload into the
//!   caller's buffer (via [`sample_into`]). A buffer that cannot hold both is
//!   [`ZenohError::BufferTooSmall`]; the sample is dropped, never truncated.
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

use crate::key::{Key, KeyError};
use crate::transport::{
    Handler, HandlerError, Publisher, Requester, Responder, Sample, Subscriber, copy_into,
    sample_into,
};

/// Request timeout used by [`ZenohTransport::new`].
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

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
}

impl ZenohTransport {
    /// Wraps an already opened session, using [`DEFAULT_TIMEOUT`] for
    /// requests.
    pub fn new(session: Session) -> Self {
        Self { session, timeout: DEFAULT_TIMEOUT }
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

    /// The underlying session.
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Declares a subscriber on `key`.
    pub async fn subscriber(&self, key: Key<'_>) -> Result<ZenohSubscriber, ZenohError> {
        let inner = self.session.declare_subscriber(key.as_str()).await?;
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

    async fn publish(&mut self, key: Key<'_>, payload: &[u8]) -> Result<(), ZenohError> {
        self.session.put(key.as_str(), payload).await?;
        Ok(())
    }
}

/// Decodes a reply-error payload into the handler error it carries.
fn decode_reply_error(payload: &[u8]) -> HandlerError {
    HandlerError::from_code(payload.first().copied().unwrap_or(0))
}

/// Copies `src` into `dst`, reporting the sizes on failure.
fn copy_checked<'b>(dst: &'b mut [u8], src: &[u8]) -> Result<&'b mut [u8], ZenohError> {
    let capacity = dst.len();
    copy_into(dst, src).ok_or(ZenohError::BufferTooSmall { needed: src.len(), capacity })
}

impl Requester for ZenohTransport {
    type Error = ZenohError;

    async fn request<'b>(
        &mut self,
        key: Key<'_>,
        request: &[u8],
        response: &'b mut [u8],
    ) -> Result<&'b [u8], ZenohError> {
        let replies = self
            .session
            .get(key.as_str())
            .payload(request)
            .timeout(self.timeout)
            .await?;
        let mut last_error = None;
        // The channel closes once every responder has answered or the
        // timeout elapses, so this loop is bounded by `self.timeout`.
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

/// Receives samples published on the one key it was declared for.
#[derive(Debug)]
pub struct ZenohSubscriber {
    inner: ::zenoh::pubsub::Subscriber<FifoChannelHandler<::zenoh::sample::Sample>>,
}

impl Subscriber for ZenohSubscriber {
    type Error = ZenohError;

    async fn recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Sample<'b>, ZenohError> {
        let sample = self.inner.recv_async().await.map_err(|_| ZenohError::Closed)?;
        let key = sample.key_expr().as_str();
        let payload = sample.payload().to_bytes();
        let needed = key.len() + payload.len();
        if needed > buf.len() {
            return Err(ZenohError::BufferTooSmall { needed, capacity: buf.len() });
        }
        Key::new(key).map_err(ZenohError::InvalidKey)?;
        // Size and key validity were checked above, so `None` is unreachable;
        // it is still mapped to an error rather than unwrapped.
        sample_into(buf, key, &payload).ok_or(ZenohError::BufferTooSmall {
            needed,
            capacity: needed,
        })
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
    let n = handler.handle(key, req, resp)?;
    resp.get(..n).ok_or(HandlerError::BufferTooSmall)
}

impl Responder for ZenohResponder<'_> {
    type Error = ZenohError;

    async fn serve_one<H: Handler>(
        &mut self,
        handler: &mut H,
        scratch: &mut [u8],
    ) -> Result<(), ZenohError> {
        let query = self.inner.recv_async().await.map_err(|_| ZenohError::Closed)?;
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
        assert_eq!(decode_reply_error(&[HandlerError::NotFound.code()]), HandlerError::NotFound);
        assert_eq!(decode_reply_error(&[]), HandlerError::Internal);
    }

    #[test]
    fn run_handler_splits_scratch_and_reports_errors() {
        let mut echo = |_: Key<'_>, req: &[u8], resp: &mut [u8]| {
            copy_into(resp, req).map(|s| s.len()).ok_or(HandlerError::BufferTooSmall)
        };
        let key = Key::from_static("a/b");
        let mut scratch = [0u8; 8];
        assert_eq!(run_handler(&mut echo, key, b"abc", &mut scratch), Ok(&b"abc"[..]));
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
        assert!(SessionConfig::new().connect("tcp/127.0.0.1:7447").build().is_ok());
    }
}
