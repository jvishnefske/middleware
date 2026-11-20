//! The tokio instantiation of the mDNS seams (feature `mdns`, `std`):
//! [`TokioUdp`], a [`Datagram`] over a tokio UDP socket, and
//! [`TokioClock`], a [`Clock`] over tokio's monotonic time. The builders
//! (`MdnsSocket::builder`, `MdnsAnnouncer::builder`) make both.

use core::net::SocketAddr;
use std::io;

use tokio::net::UdpSocket;
use tokio::time::Instant;

use crate::clock::Clock;
use crate::datagram::Datagram;

/// A tokio UDP socket as a [`Datagram`]. Receives truncate as the OS does
/// (Linux and macOS cut a datagram to the buffer); `try_recv_from` maps
/// `WouldBlock` to `Ok(None)`.
#[derive(Debug)]
pub struct TokioUdp(pub(crate) UdpSocket);

impl TokioUdp {
    /// Wraps a bound tokio socket.
    pub fn new(socket: UdpSocket) -> Self {
        TokioUdp(socket)
    }

    /// The bound local address.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.0.local_addr()
    }

    /// The tokio socket.
    pub fn get_ref(&self) -> &UdpSocket {
        &self.0
    }
}

impl Datagram for TokioUdp {
    type Error = io::Error;

    async fn send_to(&mut self, buf: &[u8], to: SocketAddr) -> io::Result<()> {
        self.0.send_to(buf, to).await.map(drop)
    }

    async fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.0.recv_from(buf).await
    }

    fn try_recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        match self.0.try_recv_from(buf) {
            Ok(r) => Ok(Some(r)),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// Milliseconds since this clock was made, from tokio's monotonic
/// `Instant` (so tokio's paused test time applies).
#[derive(Debug, Clone, Copy)]
pub struct TokioClock(Instant);

impl TokioClock {
    /// A clock whose zero is now.
    pub fn new() -> Self {
        TokioClock(Instant::now())
    }
}

impl Default for TokioClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for TokioClock {
    fn now_ms(&mut self) -> u64 {
        u64::try_from(self.0.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    async fn sleep_until_ms(&mut self, deadline_ms: u64) {
        // A deadline past what an Instant can hold never comes.
        match self
            .0
            .checked_add(core::time::Duration::from_millis(deadline_ms))
        {
            Some(at) => tokio::time::sleep_until(at).await,
            None => core::future::pending().await,
        }
    }
}
