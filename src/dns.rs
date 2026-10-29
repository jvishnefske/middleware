//! DNS: the message codec shared by multicast DNS ([`crate::mdns`]) and
//! unicast DNS-SD.
//!
//! * [`wire`] — the DNS message format (always built, `no_std`, no heap):
//!   parsing with bounded work ([`wire::Message::parse`],
//!   [`wire::MAX_RECORDS`]), the DNS-SD join of PTR, SRV, TXT and address
//!   records ([`wire::Response::instances`]), and encoding of queries and
//!   responses ([`wire::MessageWriter`], [`wire::encode_query_for`]).
//! * `unicast` (feature `dns-sd`, `std`) — DNS-SD over a unicast resolver:
//!   `UnicastDnsSd` browses Kubernetes Services by their SRV records
//!   (SRV, then A/AAAA for targets without glue) and implements
//!   [`Browse`](crate::discovery::Browse).

pub mod wire;

#[cfg(feature = "dns-sd")]
pub mod unicast;
