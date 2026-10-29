//! Service discovery, independent of how it travels.
//!
//! * [`record`] — what is discovered: a [`Service`] of a [`ServiceKind`]
//!   (a middleware key such as `cam/jpeg`) at an [`Endpoint`], with RFC
//!   6763 TXT attributes and a TTL; [`Event`]s (`Up`, `Down`); the mapping
//!   of kinds to DNS-SD types ([`record::dns_sd_type`]).
//! * [`Announce`] / [`Browse`] — the traits every discovery backend
//!   implements: mDNS (`mdns::socket::MdnsSocket`, feature `mdns`), unicast
//!   DNS-SD (feature `dns-sd`), pub/sub beacons over any transport
//!   ([`beacon::PubSubDiscovery`]), the in-process bus (feature `memory`).
//! * [`directory`] — [`Directory`], the pure fixed-capacity cache a browser
//!   keeps: expiry by TTL, goodbyes, refresh at 80 % of the TTL.
//! * [`beacon`] — the versioned `disc/<kind>` beacon format and
//!   [`PubSubDiscovery`](beacon::PubSubDiscovery).
//!
//! Everything here is always built, `no_std` and heap free.
//!
//! # TTL and goodbyes
//!
//! A [`Service`] is valid for `ttl_s` seconds after it was received; the
//! announcer re-announces before then. A service with TTL 0 is a goodbye,
//! reported by [`Browse`] as [`Event::Down`]; a [`Directory`] removes it at
//! once.
//!
//! # Cancel safety
//!
//! As for the transport traits ([`crate::transport`]): a dropped
//! [`Browse::next`] loses no event; a dropped [`Announce::announce`] or
//! [`Announce::withdraw`] may or may not have sent its message, never
//! twice; a dropped [`Browse::browse`] may or may not have sent its query.

pub mod beacon;
pub mod directory;
pub mod record;

pub use directory::{Change, Directory, ServiceRecord};
pub use record::{Endpoint, Event, RecordError, Service, ServiceKind};

use crate::key::KeyFilter;

/// Makes services known.
pub trait Announce {
    /// Backend-specific failure.
    type Error: core::fmt::Debug;

    /// Announces `service` now. Backends that need periodic re-announcement
    /// keep it and document how they are driven (e.g.
    /// [`PubSubDiscovery::tick`](beacon::PubSubDiscovery::tick)).
    /// Announcing the same kind and instance again replaces it.
    async fn announce(&mut self, service: &Service<'_>) -> Result<(), Self::Error>;

    /// Withdraws the service `instance` of `kind`: sends a goodbye (TTL 0)
    /// and stops re-announcing it.
    async fn withdraw(&mut self, kind: ServiceKind<'_>, instance: &str) -> Result<(), Self::Error>;
}

/// Finds services.
pub trait Browse {
    /// Backend-specific failure.
    type Error: core::fmt::Debug;

    /// Starts browsing the kinds matching `filter`, replacing any earlier
    /// filter; a backend that queries (mDNS, unicast DNS-SD) sends its
    /// query now and documents which filters it supports (DNS-SD types
    /// cannot express a prefix). Call again to re-query.
    async fn browse(&mut self, filter: KeyFilter<'_>) -> Result<(), Self::Error>;

    /// Waits for the next event of a browsed kind and copies it into `buf`.
    /// An announcement with TTL 0 is reported as [`Event::Down`].
    async fn next<'b>(&mut self, buf: &'b mut [u8]) -> Result<Event<'b>, Self::Error>;

    /// Returns the next event if one has already arrived, without waiting.
    fn try_next<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Event<'b>>, Self::Error>;
}
