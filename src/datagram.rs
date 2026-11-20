//! The UDP seam (always built, `no_std`): [`Datagram`], one bound socket
//! that sends to and receives from [`SocketAddr`]s.
//!
//! The mDNS shells (`mdns::socket::MdnsSocket`, `mdns::announce::MdnsAnnouncer`)
//! are generic over it: a tokio socket on the host (feature `mdns`), an
//! embassy-net socket on a board (feature `mdns-embassy`), and an
//! in-memory fake in tests.
//!
//! # Truncation
//!
//! A datagram longer than the receive buffer is truncated to the buffer's
//! length (`n == buf.len()`), as a UDP socket does, never an error. A
//! caller that must tell an oversized datagram from one that fits passes
//! one byte more than the largest datagram it accepts and treats
//! `n == buf.len()` as oversized.

use core::net::SocketAddr;

#[cfg(test)]
pub(crate) mod fake;

/// One bound UDP socket.
pub trait Datagram {
    /// Binding-specific failure.
    type Error: core::fmt::Debug;

    /// Sends `buf` as one datagram to `to`. A dropped call may or may not
    /// have sent it, never twice.
    async fn send_to(&mut self, buf: &[u8], to: SocketAddr) -> Result<(), Self::Error>;

    /// Waits for the next datagram, copies it into `buf` (truncated to
    /// `buf.len()`, see the module docs) and returns its length and source.
    ///
    /// Must be cancel safe: dropping the future before it is ready consumes
    /// no datagram.
    async fn recv_from(&mut self, buf: &mut [u8]) -> Result<(usize, SocketAddr), Self::Error>;

    /// The next datagram if one is already queued, without waiting;
    /// `Ok(None)` when none is. Truncates like [`recv_from`](Self::recv_from).
    fn try_recv_from(&mut self, buf: &mut [u8])
    -> Result<Option<(usize, SocketAddr)>, Self::Error>;
}
