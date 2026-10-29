//! Service discovery over multicast DNS (mDNS, RFC 6762) and DNS-based
//! service discovery (DNS-SD, RFC 6763).
//!
//! * [`ServiceDiscovery`] — the trait: [`browse`](ServiceDiscovery::browse)
//!   asks the network for instances of a [`ServiceType`], and
//!   [`recv`](ServiceDiscovery::recv) / [`try_recv`](ServiceDiscovery::try_recv)
//!   hand back the answers as a [`Response`] borrowing the caller's buffer.
//! * [`wire`] — the DNS message format (always built, `no_std`, no heap):
//!   [`encode_query`](wire::encode_query), [`Message::parse`](wire::Message::parse),
//!   and [`Response::instances`](wire::Response::instances), which joins the
//!   PTR, SRV, TXT and A/AAAA records of one response into
//!   [`Instance`]s.
//! * `socket` (feature `mdns`, `std`) — `MdnsSocket`, the trait over a tokio
//!   UDP socket.
//!
//! # Resolution
//!
//! Resolution is per response: [`Response::instances`] reports an instance
//! as [`Instance::Resolved`] when the same response carries its SRV record
//! (responders put SRV, TXT and addresses in the additional section, RFC 6763
//! §12), [`Instance::Unresolved`] when it carries only the PTR, and
//! [`Instance::Gone`] for a goodbye (PTR with TTL 0). There is no cache:
//! a caller that wants one keeps it, keyed by instance name.
//!
//! Only service types of the form `_service._tcp.domain` or
//! `_service._udp.domain` are browsed; subtypes (`_sub`) and the
//! `_services._dns-sd._udp` meta-query are not supported.

#[cfg(feature = "mdns")]
pub mod socket;
pub mod wire;

pub use wire::{Instance, Message, Name, Response, ServiceInstance, ServiceType, Txt, WireError};

/// Finds instances of a DNS-SD service.
///
/// Implementations send queries and receive whole DNS messages; the decoding
/// is the pure [`wire`] module, so the trait has the same shape on a host
/// socket and on a microcontroller's UDP stack.
///
/// # Cancel safety
///
/// * [`recv`](ServiceDiscovery::recv): dropping the future before it is
///   ready loses no response; the next `recv` or `try_recv` returns it.
/// * [`browse`](ServiceDiscovery::browse): dropping may or may not have sent
///   the query; it is never sent twice.
pub trait ServiceDiscovery {
    /// Transport-specific failure.
    type Error: core::fmt::Debug;

    /// Sends one query for instances of `service`. Answers arrive through
    /// [`recv`](ServiceDiscovery::recv); call again to re-query.
    async fn browse(&mut self, service: ServiceType<'_>) -> Result<(), Self::Error>;

    /// Waits for the next mDNS response and copies it into `buf`. Queries
    /// (including this node's own) are skipped. A message that does not fit
    /// `buf` is an error, never a truncation.
    async fn recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Response<'b>, Self::Error>;

    /// Returns the next response if one has already arrived, without
    /// waiting.
    fn try_recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Response<'b>>, Self::Error>;
}
