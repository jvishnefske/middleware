//! Byte-level transport traits shared by every backend.
//!
//! All methods borrow caller-owned buffers instead of returning owned data,
//! so a `no_std` implementation never allocates and a `std` implementation
//! copies into the buffer at the boundary. A payload or key that does not fit
//! is an error, never a truncation.
//!
//! Server-side request handling is split into a pure [`Handler`] (decision
//! logic, synchronous, unit-testable) and a [`Responder`] (the I/O shell that
//! receives one request, runs the handler and sends the reply).
//!
//! # Cancel safety
//!
//! Every async method may be raced in a `select` and dropped before it
//! completes. Implementations must guarantee:
//!
//! - [`Subscriber::recv`]: dropping the future before it is ready loses no
//!   sample; the next `recv` or [`Subscriber::try_recv`] returns it.
//! - [`Responder::serve_one`]: partial request state (e.g. fragment
//!   reassembly) is kept in `self`, so a dropped call resumes where it left
//!   off on the next call.
//! - [`Requester::request`]: dropping abandons the request; a late response
//!   is discarded by the next call (correlation ids or equivalent).
//! - [`Publisher::publish`]: dropping may or may not have sent the message
//!   (on fragmenting transports, possibly only part of it); it never sends it
//!   twice. A transport that numbers messages advances its sequence **before**
//!   the first await, so a dropped or failed send never makes the next
//!   message look like a repeat to receivers that deduplicate by sequence.
//! - [`Responder::serve_one`] dropped while its reply is being written: the
//!   rest of that reply may be lost (the requester discards it as stale) or
//!   completed by the next call; the transport documents which.
//!
//! These rules assume the underlying I/O futures (a stream `read`, a radio
//! `receive`) are themselves cancel-safe; implementations of
//! [`Subscriber::try_recv`] that poll an await-only stream once with a no-op
//! waker rely on the same assumption and say so.
//!
//! # Buffer too small
//!
//! When the caller's buffer cannot hold the next sample, `recv` / `try_recv`
//! return the transport's buffer-too-small error. Transports should keep the
//! sample so a retry with a larger buffer returns it; each transport documents
//! whether it does (some radios cannot re-read a frame and drop it).
//!
//! # Limits
//!
//! [`Publisher::LIMITS`] states the largest key and payload the transport
//! carries, so callers can size buffers and reject oversized messages before
//! sending.

use crate::key::{Key, KeyFilter};

/// Identifies the sender of a sample as far as the transport knows it (a MAC
/// address, a Zenoh session id, ...), zero-padded to 8 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct SenderId(pub [u8; 8]);

impl SenderId {
    /// The id of a node with this 48-bit hardware address.
    pub const fn from_mac(mac: [u8; 6]) -> Self {
        SenderId([mac[0], mac[1], mac[2], mac[3], mac[4], mac[5], 0, 0])
    }
}

/// What the transport knows about where a sample came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SampleMeta {
    /// The sender, if the transport identifies senders.
    pub sender: Option<SenderId>,
    /// The sender's sequence number for this message, if the transport
    /// carries one. Deliberately 8 bits: it is what the radio frames carry
    /// (one byte per frame matters at 31-byte advertising payloads), and
    /// it only has to distinguish repeats inside a short dedup window
    /// ([`crate::dedup`]), not order a long history. It wraps after 255;
    /// compare with wrapping arithmetic, never with `<`.
    pub seq: Option<u8>,
}

/// A message received by a [`Subscriber`]. `key` and `payload` borrow the
/// buffer passed to [`Subscriber::recv`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample<'a> {
    /// The key the message was published on.
    pub key: Key<'a>,
    /// The raw payload.
    pub payload: &'a [u8],
    /// Sender and sequence, when known.
    pub meta: SampleMeta,
}

impl<'a> Sample<'a> {
    /// A sample with no sender information.
    pub const fn new(key: Key<'a>, payload: &'a [u8]) -> Self {
        Sample { key, payload, meta: SampleMeta { sender: None, seq: None } }
    }

    /// The same sample with `meta` attached.
    pub const fn with_meta(self, meta: SampleMeta) -> Self {
        Sample { meta, ..self }
    }
}

/// Size limits of a transport.
///
/// `max_key` bounds the key and `max_payload` is the largest payload that
/// fits with **any** allowed key, so it is always safe for sizing buffers.
/// When key and payload share one frame ([`Limits::shared`]), a shorter key
/// leaves room for a longer payload, up to `max_message` bytes together; use
/// [`Limits::fits`] to check an actual message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Longest key, in bytes.
    pub max_key: usize,
    /// Largest payload that fits with any allowed key.
    pub max_payload: usize,
    /// Largest key length plus payload length together.
    pub max_message: usize,
    /// True if key and payload share `max_message` (a shorter key frees
    /// payload room); false if the payload cap is independent of the key.
    pub shared: bool,
}

impl Limits {
    /// No transport-specific limit beyond [`crate::key::MAX_KEY_LEN`].
    pub const UNBOUNDED: Limits = Limits::new(crate::key::MAX_KEY_LEN, usize::MAX);

    /// Independent limits: the key does not use payload room (e.g. it is
    /// hashed into a header or a UUID).
    pub const fn new(max_key: usize, max_payload: usize) -> Limits {
        Limits { max_key, max_payload, max_message: max_key.saturating_add(max_payload), shared: false }
    }

    /// Key and payload share `max_message` bytes; `max_payload` is what is
    /// left with the longest key.
    pub const fn shared(max_key: usize, max_message: usize) -> Limits {
        Limits { max_key, max_payload: max_message.saturating_sub(max_key), max_message, shared: true }
    }

    /// True if a message with these key and payload lengths fits.
    pub const fn fits(&self, key_len: usize, payload_len: usize) -> bool {
        if key_len > self.max_key {
            return false;
        }
        if self.shared {
            payload_len <= self.max_message.saturating_sub(key_len)
        } else {
            payload_len <= self.max_payload
        }
    }
}

/// Fire-and-forget delivery of a payload to everyone subscribed to a key.
pub trait Publisher {
    /// Transport-specific failure.
    type Error: core::fmt::Debug;

    /// What this transport can carry.
    const LIMITS: Limits = Limits::UNBOUNDED;

    /// Sends `payload` on `key`.
    async fn publish(&mut self, key: Key<'_>, payload: &[u8]) -> Result<(), Self::Error>;
}

/// Receives payloads published on the key(s) this subscriber was created for.
pub trait Subscriber {
    /// Transport-specific failure.
    type Error: core::fmt::Debug;

    /// Waits for the next sample and writes its key and payload into `buf`.
    /// Cancel-safe (see the module docs).
    async fn recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Sample<'b>, Self::Error>;

    /// Returns the next sample if one is already available, without waiting.
    /// For tick-driven callers that cannot await.
    fn try_recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Sample<'b>>, Self::Error>;
}

/// Creates subscriptions at runtime.
///
/// The subscriber may borrow the transport (`Subscriber<'a>` with
/// `Self: 'a`): a single-stream transport (a serial link, one radio) hands
/// out a filtered view of itself, valid while it is borrowed, while
/// multiplexing transports (Zenoh, gRPC bus, in-process buses) hand out
/// independent owned subscribers and ignore the lifetime.
pub trait Subscribe {
    /// The subscriber handed out, possibly borrowing `self`.
    type Subscriber<'a>: Subscriber
    where
        Self: 'a;
    /// Transport-specific failure.
    type Error: core::fmt::Debug;

    /// Starts receiving samples whose key matches `filter`. The filter is
    /// copied if the subscriber needs to keep it.
    async fn subscribe<'a>(
        &'a mut self,
        filter: KeyFilter<'_>,
    ) -> Result<Self::Subscriber<'a>, Self::Error>;
}

/// Unary request/response: one request in, one response out.
///
/// The generalisation of a gRPC unary call and a Zenoh `get`.
pub trait Requester {
    /// Transport-specific failure.
    type Error: core::fmt::Debug;

    /// Sends `request` to `key` and writes the reply into `response`,
    /// returning the filled prefix.
    async fn request<'b>(
        &mut self,
        key: Key<'_>,
        request: &[u8],
        response: &'b mut [u8],
    ) -> Result<&'b [u8], Self::Error>;
}

/// Why a [`Handler`] could not produce a response. Transports map these onto
/// their native status codes (gRPC status, Zenoh reply error, BLE error byte).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}

impl HandlerError {
    /// One-byte wire code used by transports without a native status field.
    pub const fn code(self) -> u8 {
        match self {
            HandlerError::NotFound => 1,
            HandlerError::InvalidRequest => 2,
            HandlerError::BufferTooSmall => 3,
            HandlerError::Internal => 4,
            HandlerError::Denied => 5,
        }
    }

    /// Inverse of [`HandlerError::code`]; unknown codes map to `Internal`.
    pub const fn from_code(code: u8) -> Self {
        match code {
            1 => HandlerError::NotFound,
            2 => HandlerError::InvalidRequest,
            3 => HandlerError::BufferTooSmall,
            5 => HandlerError::Denied,
            _ => HandlerError::Internal,
        }
    }
}

/// Pure request handler: given the key and request bytes, write the response
/// into `response` and return its length.
pub trait Handler {
    /// Produces the response for one request.
    fn handle(
        &mut self,
        key: Key<'_>,
        request: &[u8],
        response: &mut [u8],
    ) -> Result<usize, HandlerError>;
}

impl<F> Handler for F
where
    F: FnMut(Key<'_>, &[u8], &mut [u8]) -> Result<usize, HandlerError>,
{
    fn handle(
        &mut self,
        key: Key<'_>,
        request: &[u8],
        response: &mut [u8],
    ) -> Result<usize, HandlerError> {
        self(key, request, response)
    }
}

/// Server side of [`Requester`]: answers requests arriving on the keys it was
/// created for.
pub trait Responder {
    /// Transport-specific failure.
    type Error: core::fmt::Debug;

    /// Waits for exactly one request, answers it with `handler`, and returns.
    /// `scratch` holds the request and response; callers loop to keep serving.
    async fn serve_one<H: Handler>(
        &mut self,
        handler: &mut H,
        scratch: &mut [u8],
    ) -> Result<(), Self::Error>;
}

/// Copies `src` into the front of `dst`, failing instead of truncating.
/// Shared by backends that receive owned data and must hand out borrows.
pub fn copy_into<'b>(dst: &'b mut [u8], src: &[u8]) -> Option<&'b mut [u8]> {
    let slot = dst.get_mut(..src.len())?;
    slot.copy_from_slice(src);
    Some(slot)
}

/// Writes `key` then `payload` into `buf` and returns them as a [`Sample`].
/// Returns `None` when `buf` is too small or the key text is not a valid
/// [`Key`]. Backends that receive owned key strings use this to satisfy
/// [`Subscriber::recv`].
pub fn sample_into<'b>(buf: &'b mut [u8], key: &str, payload: &[u8]) -> Option<Sample<'b>> {
    let (key_buf, rest) = buf.split_at_mut_checked(key.len())?;
    key_buf.copy_from_slice(key.as_bytes());
    let payload_buf = rest.get_mut(..payload.len())?;
    payload_buf.copy_from_slice(payload);
    let key = core::str::from_utf8(key_buf).ok()?;
    Some(Sample::new(Key::new(key).ok()?, payload_buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_into_places_key_then_payload() {
        let mut buf = [0u8; 16];
        let s = sample_into(&mut buf, "a/b", b"xyz").unwrap();
        assert_eq!(s.key.as_str(), "a/b");
        assert_eq!(s.payload, b"xyz");
    }

    #[test]
    fn sample_into_refuses_to_truncate() {
        let mut buf = [0u8; 5];
        assert!(sample_into(&mut buf, "a/b", b"xyz").is_none());
        assert!(sample_into(&mut buf, "a b", b"").is_none());
    }

    #[test]
    fn handler_error_codes_round_trip() {
        for e in [
            HandlerError::NotFound,
            HandlerError::InvalidRequest,
            HandlerError::BufferTooSmall,
            HandlerError::Internal,
            HandlerError::Denied,
        ] {
            assert_eq!(HandlerError::from_code(e.code()), e);
        }
        assert_eq!(HandlerError::Denied.code(), 5);
        assert_eq!(HandlerError::from_code(6), HandlerError::Internal);
    }
}
