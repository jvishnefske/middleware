//! DNS: the message codec shared by multicast DNS ([`crate::mdns`]) and
//! unicast DNS-SD.
//!
//! * [`wire`] — the DNS message format (always built, `no_std`, no heap):
//!   parsing with bounded work ([`wire::Message::parse`],
//!   [`wire::MAX_RECORDS`]), the DNS-SD join of PTR, SRV, TXT and address
//!   records ([`wire::Response::instances`]), and encoding of queries and
//!   responses ([`wire::MessageWriter`], [`wire::encode_query_for`]).
//! * `unicast` (feature `dns-sd`, `std`) — DNS-SD over a unicast resolver
//!   (SRV/TXT/A/AAAA lookups and browsing). Reserved for the 0.5 backend
//!   work: the module exists so the feature is stable, and is empty today.

pub mod wire;

#[cfg(feature = "dns-sd")]
pub mod unicast;
