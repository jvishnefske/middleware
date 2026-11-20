//! The embassy instantiation of the mDNS seams (feature `mdns-embassy`,
//! `no_std`): [`EmbassyUdp`], a [`Datagram`] over an embassy-net UDP
//! socket, [`join_group`] for the stack's mDNS group membership, and
//! [`EmbassyClock`], a [`Clock`] over `embassy_time`.
//!
//! # A responder on a board
//!
//! ```ignore
//! let socket = UdpSocket::new(stack, &mut rx_meta, &mut rx, &mut tx_meta, &mut tx);
//! let udp = EmbassyUdp::bind(socket, MDNS_PORT)?;
//! join_group(stack)?;
//! let params = AnnouncerParams {
//!     destination: SocketAddr::V4(SocketAddrV4::new(MDNS_GROUP_V4, MDNS_PORT)),
//!     policy: OriginPolicy::OnLink,
//!     // The LAN's subnet: without std the routes are not read.
//!     on_link: OnLink::new().with(subnet),
//! };
//! let mut announcer: MdnsAnnouncer<EmbassyUdp, EmbassyClock, MCU_BUF> =
//!     MdnsAnnouncer::new(udp, EmbassyClock, AnnounceTable::new("rt1050.local")?, params);
//! ```
//!
//! * Give the socket's receive buffer room for at least two
//!   [`MCU_BUF`](super::MCU_BUF) datagrams and four `PacketMetadata`.
//! * smoltcp hands a datagram to the first UDP socket bound to its port
//!   only, so a board has one socket on 5353; a browser on the same board
//!   is a one-shot querier on another port (responders answer it by
//!   unicast).
//! * Before the stack has an IPv4 address, sends fail with
//!   `EmbassyError::Send(SendError::NoRoute)`: retry `announce` after
//!   link-up.
//! * Only IPv4: a V6 destination is [`EmbassyError::Ipv6Unsupported`].

use core::net::SocketAddr;
use core::pin::pin;
use core::task::{Context, Poll, Waker};

use embassy_net::udp::{BindError, SendError, UdpMetadata, UdpSocket};
use embassy_net::{IpEndpoint, MulticastError, Stack};

use super::MDNS_GROUP_V4;
use crate::clock::Clock;
use crate::datagram::Datagram;

/// Failure of an [`EmbassyUdp`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbassyError {
    /// embassy-net refused the send (`NoRoute` before an address is
    /// configured, `PacketTooLarge` beyond the transmit buffer).
    Send(SendError),
    /// The destination is IPv6; the seam speaks IPv4 only.
    Ipv6Unsupported,
}

impl core::fmt::Display for EmbassyError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            EmbassyError::Send(e) => write!(f, "udp send failed: {e:?}"),
            EmbassyError::Ipv6Unsupported => f.write_str("IPv6 destinations are not supported"),
        }
    }
}

impl core::error::Error for EmbassyError {}

/// The smoltcp endpoint of `to`; IPv4 only.
fn to_endpoint(to: SocketAddr) -> Result<IpEndpoint, EmbassyError> {
    match to {
        SocketAddr::V4(v4) => Ok(IpEndpoint::from(v4)),
        SocketAddr::V6(_) => Err(EmbassyError::Ipv6Unsupported),
    }
}

/// The source of a received datagram.
fn from_meta(meta: UdpMetadata) -> SocketAddr {
    SocketAddr::from(meta.endpoint)
}

/// Copies `data` into `buf`, truncated to it, as a UDP socket does.
fn truncate_into(data: &[u8], buf: &mut [u8]) -> usize {
    let n = data.len().min(buf.len());
    if let (Some(dst), Some(src)) = (buf.get_mut(..n), data.get(..n)) {
        dst.copy_from_slice(src);
    }
    n
}

/// An embassy-net UDP socket as a [`Datagram`]: bound, hop limit 255 (RFC
/// 6762 §11), IPv4 only. Receives go through `recv_from_with`, so a
/// datagram longer than the buffer is truncated like UDP (smoltcp's
/// `recv_slice` would drop it as `Truncated`).
pub struct EmbassyUdp<'a>(UdpSocket<'a>);

impl core::fmt::Debug for EmbassyUdp<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("EmbassyUdp")
            .field(&self.local_port())
            .finish()
    }
}

impl<'a> EmbassyUdp<'a> {
    /// Binds `socket` to `port` on every address (5353 for a responder or
    /// a group member, 0 for an ephemeral one-shot querier) and sets the
    /// hop limit to 255. Join the group with [`join_group`].
    pub fn bind(mut socket: UdpSocket<'a>, port: u16) -> Result<Self, BindError> {
        socket.bind(port)?;
        socket.set_hop_limit(Some(255));
        Ok(EmbassyUdp(socket))
    }

    /// The bound local port.
    pub fn local_port(&self) -> u16 {
        self.0.endpoint().port
    }

    /// The embassy-net socket.
    pub fn get_ref(&self) -> &UdpSocket<'a> {
        &self.0
    }
}

/// Joins the IPv4 mDNS group (224.0.0.251) on `stack`'s interface, so
/// sockets on port 5353 hear queries and announcements. Membership is per
/// interface in embassy-net, not per socket.
pub fn join_group(stack: Stack<'_>) -> Result<(), MulticastError> {
    stack.join_multicast_group(MDNS_GROUP_V4)
}

impl Datagram for EmbassyUdp<'_> {
    type Error = EmbassyError;

    async fn send_to(&mut self, buf: &[u8], to: SocketAddr) -> Result<(), EmbassyError> {
        let to = to_endpoint(to)?;
        self.0.send_to(buf, to).await.map_err(EmbassyError::Send)
    }

    async fn recv_from(&mut self, buf: &mut [u8]) -> Result<(usize, SocketAddr), EmbassyError> {
        Ok(self
            .0
            .recv_from_with(|data, meta| (truncate_into(data, buf), from_meta(meta)))
            .await)
    }

    fn try_recv_from(
        &mut self,
        buf: &mut [u8],
    ) -> Result<Option<(usize, SocketAddr)>, EmbassyError> {
        let recv = pin!(
            self.0
                .recv_from_with(|data, meta| (truncate_into(data, buf), from_meta(meta)))
        );
        match recv.poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(r) => Ok(Some(r)),
            Poll::Pending => Ok(None),
        }
    }
}

/// `embassy_time`'s monotonic milliseconds since boot. Needs the
/// firmware's time driver.
#[derive(Debug, Clone, Copy, Default)]
pub struct EmbassyClock;

impl Clock for EmbassyClock {
    fn now_ms(&mut self) -> u64 {
        embassy_time::Instant::now().as_millis()
    }

    async fn sleep_until_ms(&mut self, deadline_ms: u64) {
        // A deadline past what an Instant can hold never comes.
        match ceil_ticks(deadline_ms, embassy_time::TICK_HZ) {
            Some(t) => embassy_time::Timer::at(embassy_time::Instant::from_ticks(t)).await,
            None => core::future::pending().await,
        }
    }
}

/// The first tick of a `tick_hz` clock at or after `ms` milliseconds:
/// rounded up, so a timer never fires before its deadline (at a tick rate
/// that is not a multiple of 1000, rounding down fires up to a tick
/// early). `None` when it does not fit a `u64`.
fn ceil_ticks(ms: u64, tick_hz: u64) -> Option<u64> {
    let ticks = (u128::from(ms) * u128::from(tick_hz)).div_ceil(1000);
    u64::try_from(ticks).ok()
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use core::net::SocketAddrV4;
    use std::string::ToString;

    #[test]
    fn v4_endpoints_round_trip() {
        let a: SocketAddr = "224.0.0.251:5353".parse().unwrap();
        let ep = to_endpoint(a).unwrap();
        assert_eq!(ep.port, 5353);
        assert_eq!(from_meta(UdpMetadata::from(ep)), a);
        let b = SocketAddrV4::new("192.168.1.20".parse().unwrap(), 40000);
        assert_eq!(from_meta(UdpMetadata::from(b)), SocketAddr::V4(b));
    }

    #[test]
    fn v6_destinations_are_unsupported() {
        let a: SocketAddr = "[ff02::fb]:5353".parse().unwrap();
        assert_eq!(to_endpoint(a), Err(EmbassyError::Ipv6Unsupported));
    }

    #[test]
    fn errors_have_text() {
        assert_eq!(
            EmbassyError::Send(SendError::NoRoute).to_string(),
            "udp send failed: NoRoute"
        );
        assert_eq!(
            EmbassyError::Ipv6Unsupported.to_string(),
            "IPv6 destinations are not supported"
        );
    }

    #[test]
    fn deadlines_round_up_to_the_next_tick() {
        // 32768 Hz: 1 ms is 32.768 ticks, so the timer waits 33.
        assert_eq!(ceil_ticks(1, 32_768), Some(33));
        assert_eq!(ceil_ticks(3, 32_768), Some(99));
        assert_eq!(ceil_ticks(1000, 32_768), Some(32_768));
        // Whole ticks are exact.
        assert_eq!(ceil_ticks(7, 24_000_000), Some(168_000));
        assert_eq!(ceil_ticks(5, 1000), Some(5));
        assert_eq!(ceil_ticks(0, 32_768), Some(0));
        // Past what an Instant holds: never.
        assert_eq!(ceil_ticks(u64::MAX, 24_000_000), None);
    }

    #[test]
    fn receives_truncate_to_the_buffer() {
        let mut buf = [0u8; 3];
        assert_eq!(truncate_into(&[1, 2, 3, 4], &mut buf), 3);
        assert_eq!(buf, [1, 2, 3]);
        let mut big = [0u8; 8];
        assert_eq!(truncate_into(&[5, 6], &mut big), 2);
        assert_eq!(&big[..2], &[5, 6]);
    }
}
