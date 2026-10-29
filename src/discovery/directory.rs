//! [`Directory`]: a fixed-capacity cache of discovered services, as a pure
//! value.
//!
//! The directory is the functional core behind a browser: the imperative
//! shell feeds it each [`Event`] from a [`Browse`](super::Browse) with the
//! current time, calls [`Directory::expire`] when
//! [`Directory::next_expiry`] comes due, and re-queries at
//! [`Directory::next_refresh`] (80 % of the soonest TTL, as RFC 6762 §5.2
//! has mDNS queriers do). Every transition takes the directory by value and
//! returns the new one with what changed ([`Change`]); nothing is
//! allocated.
//!
//! # Capacity
//!
//! `N` records, each stored in a [`ServiceRecord`] with bounded fields:
//!
//! | field     | bound                      |
//! |-----------|----------------------------|
//! | kind      | [`MAX_KIND_LEN`] bytes     |
//! | instance  | [`MAX_INSTANCE_LEN`] bytes (a DNS label) |
//! | host name | [`MAX_HOST_LEN`] bytes     |
//! | locator   | [`MAX_LOCATOR_LEN`] bytes  |
//! | TXT       | [`MAX_TXT_LEN`] bytes      |
//!
//! A record with a longer field is [`Change::Rejected`]; a new record when
//! all `N` slots are taken is [`Change::Full`] (existing records are never
//! evicted for a newcomer: they expire on their own TTL). Times are
//! milliseconds on any monotonic clock the caller chooses.

use core::net::SocketAddr;

use heapless::{String, Vec};

use crate::key::KeyBuf;
use crate::transport::SenderId;

use super::record::{Endpoint, Event, Service, ServiceKind};

/// Longest kind stored.
pub const MAX_KIND_LEN: usize = 64;
/// Longest instance name stored (a DNS label).
pub const MAX_INSTANCE_LEN: usize = 63;
/// Longest [`Endpoint::Host`] name stored.
pub const MAX_HOST_LEN: usize = 64;
/// Longest [`Endpoint::Locator`] stored.
pub const MAX_LOCATOR_LEN: usize = 96;
/// Most TXT bytes stored.
pub const MAX_TXT_LEN: usize = 192;

/// Percentage of a TTL after which a record should be re-queried.
pub const REFRESH_PERCENT: u64 = 80;

#[derive(Debug, Clone, PartialEq, Eq)]
enum OwnedEndpoint {
    Socket(SocketAddr),
    Host {
        name: String<MAX_HOST_LEN>,
        port: u16,
    },
    Locator(String<MAX_LOCATOR_LEN>),
    Local,
}

/// An owned copy of a [`Service`] within the bounds in the module docs.
/// Also what announcers keep to re-announce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceRecord {
    kind: KeyBuf<MAX_KIND_LEN>,
    instance: String<MAX_INSTANCE_LEN>,
    node: Option<SenderId>,
    endpoint: OwnedEndpoint,
    txt: Vec<u8, MAX_TXT_LEN>,
    ttl_s: u32,
}

fn string<const M: usize>(s: &str) -> Option<String<M>> {
    String::try_from(s).ok()
}

impl ServiceRecord {
    /// Copies `service`; `None` if a field is over its bound.
    pub fn new(service: &Service<'_>) -> Option<Self> {
        let endpoint = match service.endpoint {
            Endpoint::Socket(a) => OwnedEndpoint::Socket(a),
            Endpoint::Host { name, port } => OwnedEndpoint::Host {
                name: string(name)?,
                port,
            },
            Endpoint::Locator(l) => OwnedEndpoint::Locator(string(l)?),
            Endpoint::Local => OwnedEndpoint::Local,
        };
        Some(ServiceRecord {
            kind: KeyBuf::from_key(service.kind.key()).ok()?,
            instance: string(service.instance)?,
            node: service.node,
            endpoint,
            txt: Vec::from_slice(service.txt).ok()?,
            ttl_s: service.ttl_s,
        })
    }

    /// The record as a [`Service`] borrowing it.
    pub fn service(&self) -> Service<'_> {
        use crate::key::AsKey;
        let endpoint = match &self.endpoint {
            OwnedEndpoint::Socket(a) => Endpoint::Socket(*a),
            OwnedEndpoint::Host { name, port } => Endpoint::Host { name, port: *port },
            OwnedEndpoint::Locator(l) => Endpoint::Locator(l),
            OwnedEndpoint::Local => Endpoint::Local,
        };
        Service {
            kind: ServiceKind::new(self.kind.as_key()),
            instance: &self.instance,
            node: self.node,
            endpoint,
            txt: &self.txt,
            ttl_s: self.ttl_s,
        }
    }

    /// True if this is the record of `kind` / `instance`.
    pub fn is(&self, kind: ServiceKind<'_>, instance: &str) -> bool {
        self.kind == kind.key() && self.instance.as_str() == instance
    }

    /// The TTL in seconds.
    pub const fn ttl_s(&self) -> u32 {
        self.ttl_s
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    record: ServiceRecord,
    seen_ms: u64,
}

impl Entry {
    fn expires_ms(&self) -> u64 {
        self.seen_ms
            .saturating_add(u64::from(self.record.ttl_s) * 1000)
    }

    fn refresh_ms(&self) -> u64 {
        self.seen_ms
            .saturating_add(u64::from(self.record.ttl_s) * 10 * REFRESH_PERCENT)
    }
}

/// What [`Directory::observe`] did with an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// A new record was stored.
    Added,
    /// A known record was announced again unchanged; its TTL restarts.
    Refreshed,
    /// A known record was announced with a different node, endpoint, TXT
    /// or TTL, and replaced.
    Updated,
    /// A goodbye removed a known record.
    Removed,
    /// A goodbye for an unknown record: nothing to do.
    Ignored,
    /// A new record, but all `N` slots are taken; not stored.
    Full,
    /// A field is over its bound (module docs); not stored.
    Rejected,
}

/// Up to `N` discovered services; see the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Directory<const N: usize> {
    entries: [Option<Entry>; N],
}

impl<const N: usize> Default for Directory<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Directory<N> {
    /// Most records held.
    pub const CAPACITY: usize = N;

    /// An empty directory.
    pub const fn new() -> Self {
        Directory {
            entries: [const { None }; N],
        }
    }

    /// Applies `event` seen at `now_ms`. An [`Event::Up`] with TTL 0 is a
    /// goodbye, like [`Event::Down`]: the record goes at once.
    pub fn observe(self, event: &Event<'_>, now_ms: u64) -> (Self, Change) {
        match event {
            Event::Up(s) if s.ttl_s > 0 => self.up(s, now_ms),
            Event::Up(s) => self.down(s.kind, s.instance),
            Event::Down { kind, instance } => self.down(*kind, instance),
        }
    }

    fn position(&self, kind: ServiceKind<'_>, instance: &str) -> Option<usize> {
        self.entries
            .iter()
            .position(|e| matches!(e, Some(e) if e.record.is(kind, instance)))
    }

    fn up(self, service: &Service<'_>, now_ms: u64) -> (Self, Change) {
        let Some(record) = ServiceRecord::new(service) else {
            return (self, Change::Rejected);
        };
        let found = self.position(service.kind, service.instance);
        let mut entries = self.entries;
        let (slot, change) = match found {
            Some(i) if entries[i].as_ref().is_some_and(|e| e.record == record) => {
                (i, Change::Refreshed)
            }
            Some(i) => (i, Change::Updated),
            None => match entries.iter().position(Option::is_none) {
                Some(i) => (i, Change::Added),
                None => return (Directory { entries }, Change::Full),
            },
        };
        entries[slot] = Some(Entry {
            record,
            seen_ms: now_ms,
        });
        (Directory { entries }, change)
    }

    fn down(self, kind: ServiceKind<'_>, instance: &str) -> (Self, Change) {
        match self.position(kind, instance) {
            Some(i) => {
                let mut entries = self.entries;
                entries[i] = None;
                (Directory { entries }, Change::Removed)
            }
            None => (self, Change::Ignored),
        }
    }

    /// Removes every record whose TTL has run out at `now_ms` and returns
    /// how many went. Use [`Directory::expired`] first to learn which.
    pub fn expire(self, now_ms: u64) -> (Self, usize) {
        let mut entries = self.entries;
        let mut removed = 0;
        for e in entries.iter_mut() {
            if e.as_ref().is_some_and(|e| e.expires_ms() <= now_ms) {
                *e = None;
                removed += 1;
            }
        }
        (Directory { entries }, removed)
    }

    /// The records [`Directory::expire`] would remove at `now_ms`.
    pub fn expired(&self, now_ms: u64) -> impl Iterator<Item = Service<'_>> {
        self.entries
            .iter()
            .flatten()
            .filter(move |e| e.expires_ms() <= now_ms)
            .map(|e| e.record.service())
    }

    /// Every record, in slot order.
    pub fn iter(&self) -> impl Iterator<Item = Service<'_>> {
        self.entries.iter().flatten().map(|e| e.record.service())
    }

    /// The records of `kind`.
    pub fn lookup<'s>(&'s self, kind: ServiceKind<'s>) -> impl Iterator<Item = Service<'s>> + 's {
        self.iter().filter(move |s| s.kind == kind)
    }

    /// The record of `kind` / `instance`, if held.
    pub fn get(&self, kind: ServiceKind<'_>, instance: &str) -> Option<Service<'_>> {
        let i = self.position(kind, instance)?;
        self.entries.get(i)?.as_ref().map(|e| e.record.service())
    }

    /// Records held.
    pub fn len(&self) -> usize {
        self.entries.iter().flatten().count()
    }

    /// True when no record is held.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// When to re-query: the time at which the record seen longest ago,
    /// relative to its TTL, reaches [`REFRESH_PERCENT`] (80 %) of it, but
    /// not before `now_ms` (an overdue refresh is due now). `None` when
    /// empty.
    pub fn next_refresh(&self, now_ms: u64) -> Option<u64> {
        let t = self.entries.iter().flatten().map(Entry::refresh_ms).min()?;
        Some(t.max(now_ms))
    }

    /// When the next record expires (call [`Directory::expire`] then), but
    /// not before `now_ms`. `None` when empty.
    pub fn next_expiry(&self, now_ms: u64) -> Option<u64> {
        let t = self.entries.iter().flatten().map(Entry::expires_ms).min()?;
        Some(t.max(now_ms))
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::string::ToString;

    const CAM: ServiceKind<'static> = ServiceKind::from_static("cam/jpeg");
    const BUS: ServiceKind<'static> = ServiceKind::from_static("mw/bus");

    fn svc(kind: ServiceKind<'static>, instance: &'static str, ttl_s: u32) -> Service<'static> {
        Service {
            kind,
            instance,
            node: None,
            endpoint: Endpoint::Local,
            txt: b"",
            ttl_s,
        }
    }

    #[test]
    fn up_refresh_update_and_goodbye() {
        let d = Directory::<4>::new();
        let (d, c) = d.observe(&Event::Up(svc(CAM, "a", 10)), 0);
        assert_eq!((c, d.len()), (Change::Added, 1));
        let (d, c) = d.observe(&Event::Up(svc(CAM, "a", 10)), 5_000);
        assert_eq!(c, Change::Refreshed);
        let moved = Service {
            endpoint: Endpoint::Locator("tcp/10.0.0.2:7447"),
            ..svc(CAM, "a", 10)
        };
        let (d, c) = d.observe(&Event::Up(moved), 6_000);
        assert_eq!(c, Change::Updated);
        assert_eq!(d.get(CAM, "a"), Some(moved));
        let (d, _) = d.observe(&Event::Up(svc(BUS, "a", 10)), 6_000);
        assert_eq!(d.lookup(CAM).count(), 1);
        assert_eq!(d.lookup(BUS).count(), 1);
        // A goodbye removes the record at once, whichever form it takes.
        let (d, c) = d.observe(
            &Event::Down {
                kind: CAM,
                instance: "a",
            },
            6_001,
        );
        assert_eq!((c, d.get(CAM, "a")), (Change::Removed, None));
        let (d, c) = d.observe(&Event::Up(svc(BUS, "a", 0)), 6_002);
        assert_eq!((c, d.is_empty()), (Change::Removed, true));
        let (_, c) = d.observe(
            &Event::Down {
                kind: CAM,
                instance: "zz",
            },
            6_003,
        );
        assert_eq!(c, Change::Ignored);
    }

    #[test]
    fn records_expire_on_their_ttl_and_refresh_at_80_percent() {
        let d = Directory::<4>::new();
        let (d, _) = d.observe(&Event::Up(svc(CAM, "short", 10)), 1_000);
        let (d, _) = d.observe(&Event::Up(svc(CAM, "long", 100)), 1_000);
        assert_eq!(d.next_refresh(0), Some(9_000));
        assert_eq!(d.next_refresh(9_500), Some(9_500), "overdue is now");
        assert_eq!(d.next_expiry(0), Some(11_000));
        let (d, n) = d.clone().expire(10_999);
        assert_eq!(n, 0);
        assert_eq!(
            d.expired(11_000)
                .map(|s| s.instance)
                .collect::<std::vec::Vec<_>>(),
            ["short"]
        );
        let (d, n) = d.expire(11_000);
        assert_eq!((n, d.len()), (1, 1));
        assert_eq!(d.next_refresh(0), Some(81_000));
        let (d, n) = d.expire(u64::MAX);
        assert_eq!((n, d.next_refresh(0), d.next_expiry(0)), (1, None, None));
    }

    #[test]
    fn capacity_and_bounds_are_enforced() {
        let d = Directory::<1>::new();
        assert_eq!(Directory::<1>::CAPACITY, 1);
        let (d, _) = d.observe(&Event::Up(svc(CAM, "a", 10)), 0);
        let (d, c) = d.observe(&Event::Up(svc(CAM, "b", 10)), 0);
        assert_eq!((c, d.len()), (Change::Full, 1));
        let long = "x".repeat(64).to_string();
        let long: &'static str = std::boxed::Box::leak(long.into_boxed_str());
        let (_, c) = Directory::<1>::new().observe(&Event::Up(svc(CAM, long, 10)), 0);
        assert_eq!(c, Change::Rejected);
        let txt: &'static [u8] =
            std::boxed::Box::leak(std::vec![1u8; MAX_TXT_LEN + 1].into_boxed_slice());
        let (_, c) = Directory::<1>::new().observe(
            &Event::Up(Service {
                txt,
                ..svc(CAM, "a", 1)
            }),
            0,
        );
        assert_eq!(c, Change::Rejected);
    }
}
