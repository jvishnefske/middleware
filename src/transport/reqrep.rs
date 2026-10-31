//! Request/response: [`Requester`] (one reply), [`Gather`] (every reply),
//! the pure [`Handler`] and its I/O shell [`Responder`].
//!
//! # Options and timeouts
//!
//! [`Requester::request_with`] and [`Gather::request_each`] take
//! [`RequestOptions`]: an optional `timeout` and a [`Target`]. The timeout
//! contract is the same for every backend:
//!
//! * `timeout: Some(d)` — the call returns within about `d` (plus the
//!   backend's scheduling slack), with the backend's timeout error if no
//!   reply came. A backend that can carry the remaining budget to the
//!   responder does so as [`RequestMeta::deadline_ms`].
//! * `timeout: None` — the backend's own default bound applies, and the
//!   backend documents it (Zenoh's query timeout, a gRPC channel deadline,
//!   the serial link's frame skip limit). A backend with **no** bound of its
//!   own says so on its `request_with`, and the caller bounds the call (e.g.
//!   with `select` against a timer; every call is cancel safe).
//!
//! [`Requester::request`] is `request_with` with [`RequestOptions::DEFAULT`].
//!
//! # Targets
//!
//! [`Target::Any`] lets the transport pick any responder for the key (the
//! first to answer, the one it routes to). [`Target::Node`] asks for the
//! responder on one node; a backend that cannot address nodes answers with
//! an error rather than silently asking someone else, and documents it.

use core::time::Duration;

use crate::key::Key;

use super::meta::{RequestMeta, SenderId};

/// One request as a [`Handler`] sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request<'a> {
    /// The key the request was sent to.
    pub key: Key<'a>,
    /// The request bytes.
    pub body: &'a [u8],
    /// Caller identity and deadline, as far as the transport knows them.
    pub meta: RequestMeta<'a>,
}

impl<'a> Request<'a> {
    /// A request with no metadata.
    pub const fn new(key: Key<'a>, body: &'a [u8]) -> Self {
        Request {
            key,
            body,
            meta: RequestMeta::NONE,
        }
    }

    /// The same request with `meta` attached.
    pub const fn with_meta(self, meta: RequestMeta<'a>) -> Self {
        Request { meta, ..self }
    }
}

/// Which responder a request is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Target {
    /// Any responder serving the key.
    #[default]
    Any,
    /// Only the responder on this node.
    Node(SenderId),
}

/// Per-call options of [`Requester::request_with`] and
/// [`Gather::request_each`]. Build with the const methods:
/// `RequestOptions::DEFAULT.with_timeout(Duration::from_millis(500))`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RequestOptions {
    /// How long to wait for replies; `None` uses the backend's documented
    /// default (see the module docs).
    pub timeout: Option<Duration>,
    /// Which responder(s) the request is for.
    pub target: Target,
}

impl RequestOptions {
    /// No timeout of the caller's, any responder.
    pub const DEFAULT: RequestOptions = RequestOptions {
        timeout: None,
        target: Target::Any,
    };

    /// The same options with `timeout`.
    pub const fn with_timeout(self, timeout: Duration) -> Self {
        RequestOptions {
            timeout: Some(timeout),
            ..self
        }
    }

    /// The same options with `target`.
    pub const fn with_target(self, target: Target) -> Self {
        RequestOptions { target, ..self }
    }

    /// The timeout in whole milliseconds, saturating at `u32::MAX`: the
    /// value a backend puts in [`RequestMeta::deadline_ms`] when it sends.
    pub const fn timeout_ms(&self) -> Option<u32> {
        match self.timeout {
            None => None,
            Some(d) => {
                let ms = d.as_millis();
                Some(if ms > u32::MAX as u128 {
                    u32::MAX
                } else {
                    ms as u32
                })
            }
        }
    }
}

/// Unary request/response: one request in, one response out.
///
/// The generalisation of a gRPC unary call and a Zenoh `get`.
pub trait Requester {
    /// Transport-specific failure.
    type Error: core::fmt::Debug;

    /// Sends `request` to `key` and writes the reply into `response`,
    /// returning the filled prefix. Honours `options` as stated in the
    /// module docs; a remote [`HandlerError`] is reported through
    /// `Self::Error`.
    async fn request_with<'b>(
        &mut self,
        key: Key<'_>,
        request: &[u8],
        response: &'b mut [u8],
        options: &RequestOptions,
    ) -> Result<&'b [u8], Self::Error>;

    /// [`request_with`](Requester::request_with) with
    /// [`RequestOptions::DEFAULT`].
    async fn request<'b>(
        &mut self,
        key: Key<'_>,
        request: &[u8],
        response: &'b mut [u8],
    ) -> Result<&'b [u8], Self::Error> {
        self.request_with(key, request, response, &RequestOptions::DEFAULT)
            .await
    }
}

/// One reply seen by [`Gather::request_each`]. `result` borrows the
/// caller's buffer and is only valid inside the callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reply<'a> {
    /// The responding node, if the transport identifies it.
    pub sender: Option<SenderId>,
    /// The response bytes, or the responder's error. A response that does
    /// not fit the caller's buffer is reported here as
    /// [`HandlerError::BufferTooSmall`] and does not end the gather.
    pub result: Result<&'a [u8], HandlerError>,
}

/// What [`Gather::request_each`] does after a reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// Keep collecting.
    Continue,
    /// Stop now; later replies are discarded.
    Stop,
}

/// Scatter/gather: one request to every responder of a key, each reply
/// handed to a callback. The generalisation of a Zenoh `get` with several
/// queryables and of a DNS-SD style "who serves this?".
///
/// The call is bounded by count and by time: it returns after
/// `max_replies` replies, when `on_reply` returns [`Flow::Stop`], when the
/// backend knows every responder has answered, or when `options.timeout`
/// (or the backend's documented default, see the module docs) elapses.
/// Running out of time after some replies is **not** an error: the call
/// returns how many replies it saw (possibly 0). [`Target::Node`] restricts
/// the gather to one node.
pub trait Gather {
    /// Transport-specific failure.
    type Error: core::fmt::Debug;

    /// Sends `request` to `key` and calls `on_reply` with each reply, copied
    /// into `buf`. Returns the number of replies passed to `on_reply`.
    async fn request_each<F>(
        &mut self,
        key: Key<'_>,
        request: &[u8],
        buf: &mut [u8],
        options: &RequestOptions,
        max_replies: usize,
        on_reply: F,
    ) -> Result<usize, Self::Error>
    where
        F: FnMut(Reply<'_>) -> Flow;
}

/// Why a [`Handler`] could not produce a response. Transports map these onto
/// their native status codes (gRPC status, Zenoh reply error, BLE error byte).
///
/// Non-exhaustive: match with a wildcard arm; [`HandlerError::code`] and
/// [`HandlerError::from_code`] are the stable wire form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum HandlerError {
    /// No handler exists for the key.
    NotFound,
    /// The request payload was malformed.
    InvalidRequest,
    /// The response did not fit the buffer.
    BufferTooSmall,
    /// The handler failed for an application reason.
    Internal,
    /// The request was well formed but refused by policy: a payload commit
    /// that is unsigned, badly signed or a rollback on a device with a
    /// pinned key, or a key claim on an already claimed device outside the
    /// re-key window ([`crate::loader::trust`]).
    Denied,
    /// The handler exists but cannot serve now (busy, starting, a
    /// dependency down); the caller may retry later.
    Unavailable,
    /// The caller's deadline ([`RequestMeta::deadline_ms`]) passed before
    /// the handler could answer.
    DeadlineExceeded,
}

impl HandlerError {
    /// Every variant, in code order.
    pub const ALL: [HandlerError; 7] = [
        HandlerError::NotFound,
        HandlerError::InvalidRequest,
        HandlerError::BufferTooSmall,
        HandlerError::Internal,
        HandlerError::Denied,
        HandlerError::Unavailable,
        HandlerError::DeadlineExceeded,
    ];

    /// One-byte wire code used by transports without a native status field.
    pub const fn code(self) -> u8 {
        match self {
            HandlerError::NotFound => 1,
            HandlerError::InvalidRequest => 2,
            HandlerError::BufferTooSmall => 3,
            HandlerError::Internal => 4,
            HandlerError::Denied => 5,
            HandlerError::Unavailable => 6,
            HandlerError::DeadlineExceeded => 7,
        }
    }

    /// Inverse of [`HandlerError::code`]; unknown codes map to `Internal`.
    pub const fn from_code(code: u8) -> Self {
        match code {
            1 => HandlerError::NotFound,
            2 => HandlerError::InvalidRequest,
            3 => HandlerError::BufferTooSmall,
            5 => HandlerError::Denied,
            6 => HandlerError::Unavailable,
            7 => HandlerError::DeadlineExceeded,
            _ => HandlerError::Internal,
        }
    }
}

/// Pure request handler: given the request, write the response into
/// `response` and return its length.
///
/// Closures implement it; [`handler_fn`] pins a closure's argument types
/// so they need no annotations.
pub trait Handler {
    /// Produces the response for one request.
    fn handle(&mut self, req: &Request<'_>, response: &mut [u8]) -> Result<usize, HandlerError>;
}

impl<F> Handler for F
where
    F: FnMut(&Request<'_>, &mut [u8]) -> Result<usize, HandlerError>,
{
    fn handle(&mut self, req: &Request<'_>, response: &mut [u8]) -> Result<usize, HandlerError> {
        self(req, response)
    }
}

/// Returns `f` unchanged, fixing its signature to the [`Handler`] shape so
/// a closure's arguments are inferred: `handler_fn(|req, out| ...)`.
pub const fn handler_fn<F>(f: F) -> F
where
    F: FnMut(&Request<'_>, &mut [u8]) -> Result<usize, HandlerError>,
{
    f
}

/// Server side of [`Requester`]: answers requests arriving on the keys it was
/// created for.
pub trait Responder {
    /// Transport-specific failure.
    type Error: core::fmt::Debug;

    /// Waits for exactly one request, answers it with `handler`, and returns.
    /// `scratch` holds the request and response; callers loop to keep serving.
    /// The handler receives whatever [`RequestMeta`] the transport knows.
    async fn serve_one<H: Handler>(
        &mut self,
        handler: &mut H,
        scratch: &mut [u8],
    ) -> Result<(), Self::Error>;
}

#[cfg(test)]
mod tests {
    use super::*;

    const K: Key<'static> = Key::from_static("a/b");

    #[test]
    fn handler_error_codes_round_trip() {
        for e in HandlerError::ALL {
            assert_eq!(HandlerError::from_code(e.code()), e);
        }
        assert_eq!(HandlerError::Denied.code(), 5);
        assert_eq!(HandlerError::Unavailable.code(), 6);
        assert_eq!(HandlerError::DeadlineExceeded.code(), 7);
        for unknown in [0, 8, 255] {
            assert_eq!(HandlerError::from_code(unknown), HandlerError::Internal);
        }
    }

    #[test]
    fn closures_are_handlers_and_see_meta() {
        let mut seen = None;
        let mut h = handler_fn(|req, out| {
            seen = Some((
                req.meta.sender,
                req.meta.principal.map(str::len),
                req.meta.deadline_ms,
            ));
            out[..req.body.len()].copy_from_slice(req.body);
            Ok(req.body.len())
        });
        let meta = RequestMeta::NONE
            .with_sender(SenderId([1; 8]))
            .with_principal("spiffe://x")
            .with_deadline_ms(9);
        let mut out = [0u8; 4];
        assert_eq!(
            h.handle(&Request::new(K, b"hi").with_meta(meta), &mut out),
            Ok(2)
        );
        assert_eq!(seen, Some((Some(SenderId([1; 8])), Some(10), Some(9))));
    }

    #[test]
    fn request_options_build_and_convert_timeouts() {
        let o = RequestOptions::DEFAULT;
        assert_eq!(o, RequestOptions::default());
        assert_eq!(
            (o.timeout, o.target, o.timeout_ms()),
            (None, Target::Any, None)
        );
        let o = o
            .with_timeout(Duration::from_millis(1500))
            .with_target(Target::Node(SenderId([2; 8])));
        assert_eq!(o.timeout_ms(), Some(1500));
        assert_eq!(o.target, Target::Node(SenderId([2; 8])));
        let huge = RequestOptions::DEFAULT.with_timeout(Duration::from_secs(u64::MAX));
        assert_eq!(huge.timeout_ms(), Some(u32::MAX));
    }

    /// A requester implementing only `request_with` gets `request`.
    struct Echo(Option<RequestOptions>);

    impl Requester for Echo {
        type Error = ();
        async fn request_with<'b>(
            &mut self,
            _key: Key<'_>,
            request: &[u8],
            response: &'b mut [u8],
            options: &RequestOptions,
        ) -> Result<&'b [u8], ()> {
            self.0 = Some(*options);
            let out = response.get_mut(..request.len()).ok_or(())?;
            out.copy_from_slice(request);
            Ok(out)
        }
    }

    #[test]
    fn request_is_request_with_defaults() {
        let mut e = Echo(None);
        let mut buf = [0u8; 4];
        let r = futures::executor::block_on(e.request(K, b"abc", &mut buf)).unwrap();
        assert_eq!(r, b"abc");
        assert_eq!(e.0, Some(RequestOptions::DEFAULT));
    }
}
