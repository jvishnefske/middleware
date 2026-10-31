//! Byte-level transport traits shared by every backend.
//!
//! All methods borrow caller-owned buffers instead of returning owned data,
//! so a `no_std` implementation never allocates and a `std` implementation
//! copies into the buffer at the boundary. A payload or key that does not fit
//! is an error, never a truncation.
//!
//! The contract is split over three submodules, all re-exported here so the
//! paths are `middleware::transport::X`:
//!
//! * [`meta`] — [`SenderId`], [`SampleMeta`] (sender, sequence, gap) and
//!   [`RequestMeta`] (sender, authenticated principal, deadline).
//! * [`pubsub`] — [`Publisher`], [`Subscriber`] (with its required
//!   [`Delivery`] and [`Limits`] declarations), [`Subscribe`] and [`Split`].
//! * [`reqrep`] — [`Requester`] (with [`RequestOptions`]: timeout and
//!   [`Target`]), [`Gather`] (every reply, bounded by count and time), the
//!   pure [`Handler`] over a [`Request`], [`HandlerError`] and
//!   [`Responder`].
//! * `memory` (feature `memory`, `std`) — an in-process backend implementing
//!   all of it, for tests and examples.
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
//! - [`Requester::request_with`] (and so [`Requester::request`]): dropping
//!   abandons the request; a late response is discarded by the next call
//!   (correlation ids or equivalent). Dropping is how a caller bounds a
//!   backend that has no timeout of its own.
//! - [`Gather::request_each`]: dropping abandons the gather; replies that
//!   arrive later are discarded by the next call. Replies already passed to
//!   the callback stay delivered.
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
//! # Timeouts
//!
//! `request_with` and `request_each` honour [`RequestOptions::timeout`];
//! with `None`, each backend applies and documents its own bound, or states
//! that it has none and the caller bounds the call (see [`reqrep`]).
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
//! sending; [`Subscriber::LIMITS`] states the largest sample a subscriber
//! can hand out. A type implementing both names them as
//! `<T as Publisher>::LIMITS` and `<T as Subscriber>::LIMITS`.

pub mod meta;
pub mod pubsub;
pub mod reqrep;

#[cfg(feature = "memory")]
pub mod memory;

pub use meta::{RequestMeta, SampleMeta, SenderId};
pub use pubsub::{Delivery, Limits, Overflow, Publisher, Sample, Split, Subscribe, Subscriber};
pub use reqrep::{
    Flow, Gather, Handler, HandlerError, Reply, Request, RequestOptions, Requester, Responder,
    Target, handler_fn,
};

use crate::key::Key;

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
        assert_eq!(s.meta, SampleMeta::NONE);
    }

    #[test]
    fn sample_into_refuses_to_truncate() {
        let mut buf = [0u8; 5];
        assert!(sample_into(&mut buf, "a/b", b"xyz").is_none());
        assert!(sample_into(&mut buf, "a b", b"").is_none());
    }
}
