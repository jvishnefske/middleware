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

use crate::key::Key;

/// A message received by a [`Subscriber`]. Both fields borrow the buffer
/// passed to [`Subscriber::recv`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample<'a> {
    /// The key the message was published on.
    pub key: Key<'a>,
    /// The raw payload.
    pub payload: &'a [u8],
}

/// Fire-and-forget delivery of a payload to everyone subscribed to a key.
pub trait Publisher {
    /// Transport-specific failure.
    type Error: core::fmt::Debug;

    /// Sends `payload` on `key`.
    async fn publish(&mut self, key: Key<'_>, payload: &[u8]) -> Result<(), Self::Error>;
}

/// Receives payloads published on the key(s) this subscriber was created for.
pub trait Subscriber {
    /// Transport-specific failure.
    type Error: core::fmt::Debug;

    /// Waits for the next sample and writes its key and payload into `buf`.
    async fn recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Sample<'b>, Self::Error>;
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
}

impl HandlerError {
    /// One-byte wire code used by transports without a native status field.
    pub const fn code(self) -> u8 {
        match self {
            HandlerError::NotFound => 1,
            HandlerError::InvalidRequest => 2,
            HandlerError::BufferTooSmall => 3,
            HandlerError::Internal => 4,
        }
    }

    /// Inverse of [`HandlerError::code`]; unknown codes map to `Internal`.
    pub const fn from_code(code: u8) -> Self {
        match code {
            1 => HandlerError::NotFound,
            2 => HandlerError::InvalidRequest,
            3 => HandlerError::BufferTooSmall,
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
    Some(Sample {
        key: Key::new(key).ok()?,
        payload: payload_buf,
    })
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
        ] {
            assert_eq!(HandlerError::from_code(e.code()), e);
        }
    }
}
