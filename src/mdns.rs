//! Service discovery over multicast DNS (mDNS, RFC 6762) and DNS-based
//! service discovery (DNS-SD, RFC 6763).
//!
//! * [`wire`] — the DNS message format, now [`crate::dns::wire`] and
//!   re-exported here under its old path (always built, `no_std`, no heap):
//!   [`encode_query`](wire::encode_query), [`Message::parse`](wire::Message::parse),
//!   and [`Response::instances`](wire::Response::instances), which joins the
//!   PTR, SRV, TXT and A/AAAA records of one response into
//!   [`Instance`]s.
//! * `socket` (feature `mdns`, `std`) — `MdnsSocket`, a querier over a
//!   tokio UDP socket. It implements [`crate::discovery::Browse`]: browsing
//!   a [`ServiceKind`](crate::discovery::ServiceKind) queries its DNS-SD
//!   type ([`dns_sd_type`](crate::discovery::record::dns_sd_type) in
//!   [`DOMAIN`]) and turns answers into discovery events. Its raw
//!   `query` / `recv_response` methods hand back whole [`Response`]s.
//! * `announce` (feature `mdns`, `std`) — `MdnsAnnouncer`, a responder
//!   implementing [`crate::discovery::Announce`]: RFC 6762 probing with
//!   conflict renames, announcements, answers to PTR/SRV/TXT/A/AAAA
//!   queries and goodbyes, decided by the pure `announce::table` core.
//! * `origin` (feature `mdns`) — which sources both accept (on-link only
//!   unless a destination was set explicitly), a pure policy.
//!
//! The 0.4 `ServiceDiscovery` trait is gone: browsing is
//! [`crate::discovery::Browse`] for every discovery backend.
//!
//! # Resolution
//!
//! Resolution is per response: [`Response::instances`] reports an instance
//! as [`Instance::Resolved`] when the same response carries its SRV record
//! (responders put SRV, TXT and addresses in the additional section, RFC 6763
//! §12), [`Instance::Unresolved`] when it carries only the PTR, and
//! [`Instance::Gone`] for a goodbye (PTR with TTL 0). There is no cache:
//! a caller that wants one keeps a [`crate::discovery::Directory`].
//!
//! Only service types of the form `_service._tcp.domain` or
//! `_service._udp.domain` are browsed; subtypes (`_sub`) and the
//! `_services._dns-sd._udp` meta-query are not supported.

use core::net::Ipv4Addr;

#[cfg(feature = "mdns-core")]
pub mod announce;
#[cfg(feature = "mdns-core")]
mod error;
#[cfg(feature = "mdns")]
pub mod host;
#[cfg(feature = "mdns-core")]
pub mod origin;
#[cfg(feature = "mdns-core")]
mod race;
#[cfg(feature = "mdns-core")]
pub mod socket;
pub use crate::dns::wire;

pub use wire::{Instance, Message, Name, Response, ServiceInstance, ServiceType, Txt, WireError};

/// The socket error [`MdnsError`](socket::MdnsError) carries when none is
/// named: `std::io::Error`, the tokio shells' (without `std`, `Infallible`).
#[cfg(all(feature = "mdns-core", feature = "std"))]
pub(crate) type DefaultIo = std::io::Error;
#[cfg(all(feature = "mdns-core", not(feature = "std")))]
pub(crate) type DefaultIo = core::convert::Infallible;
/// The socket of the bare `MdnsSocket` / `MdnsAnnouncer`: tokio's with
/// `mdns`, else none (every other instantiation names its socket).
#[cfg(feature = "mdns")]
pub(crate) type DefaultSocket = host::TokioUdp;
#[cfg(all(feature = "mdns-core", not(feature = "mdns")))]
pub(crate) type DefaultSocket = ();
/// The clock of the bare `MdnsAnnouncer`, as for [`DefaultSocket`].
#[cfg(feature = "mdns")]
pub(crate) type DefaultClock = host::TokioClock;
#[cfg(all(feature = "mdns-core", not(feature = "mdns")))]
pub(crate) type DefaultClock = ();

/// UDP port of mDNS.
pub const MDNS_PORT: u16 = 5353;
/// IPv4 mDNS multicast group.
pub const MDNS_GROUP_V4: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
/// The mDNS domain, appended to a service kind's DNS-SD type.
pub const DOMAIN: &str = "local";
/// Receive and transmit buffer of the std shells: one byte over
/// [`wire::MAX_MESSAGE_LEN`], so a datagram of this length or more (cut
/// by the socket) is detected as oversized and skipped.
pub const STD_BUF: usize = wire::MAX_MESSAGE_LEN + 1;
/// Buffer for a microcontroller's shells: an Ethernet MTU. Datagrams of
/// 1500 bytes or more are skipped.
pub const MCU_BUF: usize = 1500;
/// Longest DNS-SD type text of a kind in [`DOMAIN`]: the type, a dot, the
/// domain.
#[cfg(feature = "mdns-core")]
pub(crate) const MAX_TYPE_TEXT: usize =
    crate::discovery::record::MAX_DNS_SD_TYPE_LEN + 1 + DOMAIN.len();
