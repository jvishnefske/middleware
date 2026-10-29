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

#[cfg(feature = "mdns")]
pub mod socket;
pub use crate::dns::wire;

pub use wire::{Instance, Message, Name, Response, ServiceInstance, ServiceType, Txt, WireError};

/// UDP port of mDNS.
pub const MDNS_PORT: u16 = 5353;
/// IPv4 mDNS multicast group.
pub const MDNS_GROUP_V4: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
/// The mDNS domain, appended to a service kind's DNS-SD type.
pub const DOMAIN: &str = "local";
