//! ABI 6: bridging publications between a device's links.
//!
//! A device with more than one network link (the Colorlight 5A-75B has two
//! Ethernet ports, each its own middleware link with its own address) can
//! forward publications from one link to another without running the
//! payload per message. The payload only configures the forwarding, once,
//! usually from `init`; the device matches every incoming publication
//! against its rule table and re-emits matches on the destination link with
//! the key and payload unchanged (in gateware, at line rate).
//!
//! # `route_bridge(src_link, key_ptr, key_len, dst_link) -> handle`
//!
//! * `src_link`, `dst_link`: link numbers `0..MAX_LINKS`, different from
//!   each other; otherwise `InvalidArgument`. A link the device does not
//!   have is `Unsupported`.
//! * the key bytes are either an exact [`Key`] (`demo/imu`) or a segment
//!   prefix ending in `/**` (`demo/**` matches `demo`, `demo/a`,
//!   `demo/a/b`, but not `demox`), the rules of [`Filter::Prefix`]. A
//!   malformed key, or one under `sys` (exact or prefix), is `InvalidKey`;
//!   a pointer pair outside memory is `OutOfBounds` (checked after the
//!   link numbers, before the key).
//! * returns a handle in `BRIDGE_BASE..BRIDGE_BASE + MAX_BRIDGES`.
//!   Bridging the same `(src, filter, dst)` again returns the same handle;
//!   a full table is `NoResources`.
//!
//! Only publications are bridged: requests and responses never cross links.
//! Bridging does not consume: a publication that matches a rule and a
//! subscription is both forwarded and delivered to `on_message`. A
//! publication matching several rules with the same destination is sent
//! there once.
//!
//! The handle works with the ABI 4 route imports: `route_stop(handle)`
//! removes the rule (`Busy` if it is not installed) and
//! `route_stat(handle, stat)` reads its counters: `Frames` (and `Chunks`)
//! count forwarded publications, `FramesDropped` (and `ChunksDropped`)
//! those the destination link could not send. Bridges raise no `on_irq`
//! events.
//!
//! # Links and the other imports
//!
//! On a multi-link device, `publish` sends on every link and `on_message`
//! delivers matching publications from any link; payloads that need to
//! pick a link use a bridge. A single-link device implements ABI 6 with
//! `route_bridge` returning `Unsupported`.
//!
//! Every rule is pure; `wasm-host` applies [`parse`] at the import
//! boundary and the devices keep a [`BridgeTable`].

use super::AbiError;
use crate::key::{Filter, Key, KeyBuf, KeyFilter, KeyFilterBuf, MAX_KEY_LEN};

/// Highest number of links a bridge can name (`0..MAX_LINKS`).
pub const MAX_LINKS: usize = 8;

/// Rules one device keeps (the gateware's rule table has 16 entries).
pub const MAX_BRIDGES: usize = 16;

/// First bridge handle: bridges and ABI 4 routes share `route_stop` and
/// `route_stat`, so bridge handles start above every route handle.
pub const BRIDGE_BASE: u8 = 16;

/// The suffix that makes a bridge key a segment prefix.
pub const PREFIX_SUFFIX: &str = "/**";

/// One forwarding rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bridge<'a> {
    /// The link publications arrive on.
    pub src: u8,
    /// Which keys are forwarded.
    pub filter: KeyFilter<'a>,
    /// The link they are re-emitted on.
    pub dst: u8,
}

/// Parses a link number argument.
pub fn link(v: i32) -> Result<u8, AbiError> {
    u8::try_from(v)
        .ok()
        .filter(|&l| usize::from(l) < MAX_LINKS)
        .ok_or(AbiError::InvalidArgument)
}

/// Checks the link pair of `route_bridge` (before any pointer).
pub fn links(src: i32, dst: i32) -> Result<(u8, u8), AbiError> {
    let (s, d) = (link(src)?, link(dst)?);
    if s == d {
        return Err(AbiError::InvalidArgument);
    }
    Ok((s, d))
}

/// Parses the key bytes of `route_bridge` into a filter: exact, or a
/// segment prefix written with a trailing `/**`. Keys under `sys` are
/// refused.
pub fn filter(bytes: &[u8]) -> Result<KeyFilter<'_>, AbiError> {
    let s = core::str::from_utf8(bytes).map_err(|_| AbiError::InvalidKey)?;
    let (body, prefix) = match s.strip_suffix(PREFIX_SUFFIX) {
        Some(p) => (p, true),
        None => (s, false),
    };
    let key = Key::new(body).map_err(|_| AbiError::InvalidKey)?;
    let b = key.as_bytes();
    if b == b"sys" || b.starts_with(b"sys/") {
        return Err(AbiError::InvalidKey);
    }
    Ok(if prefix {
        Filter::Prefix(key)
    } else {
        Filter::Exact(key)
    })
}

/// Parses a whole `route_bridge` call whose key bytes are already read.
pub fn parse(src: i32, key: &[u8], dst: i32) -> Result<Bridge<'_>, AbiError> {
    let (src, dst) = links(src, dst)?;
    Ok(Bridge {
        src,
        filter: filter(key)?,
        dst,
    })
}

/// Parses a bridge handle (`BRIDGE_BASE..BRIDGE_BASE + MAX_BRIDGES`) into
/// a rule index; `None` for any other value.
pub fn handle_index(handle: u8) -> Option<usize> {
    let i = usize::from(handle.checked_sub(BRIDGE_BASE)?);
    (i < MAX_BRIDGES).then_some(i)
}

/// The handle of rule `index`.
pub const fn handle_of(index: usize) -> u8 {
    BRIDGE_BASE + index as u8
}

/// Counters of one rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BridgeStats {
    /// Publications forwarded.
    pub forwarded: u32,
    /// Publications the destination link could not send.
    pub dropped: u32,
}

/// An installed rule, owning its key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rule {
    /// Source link.
    pub src: u8,
    /// Which keys.
    pub filter: KeyFilterBuf<MAX_KEY_LEN>,
    /// Destination link.
    pub dst: u8,
    /// Counters.
    pub stats: BridgeStats,
}

/// A device's bridge rule table: [`MAX_BRIDGES`] slots, a handle per
/// slot. The gateware's table and the simulators follow these semantics.
#[derive(Debug, Clone, Default)]
pub struct BridgeTable {
    rules: [Option<Rule>; MAX_BRIDGES],
}

/// Where one incoming publication goes: a destination bitmask (bit `n` =
/// link `n`) and, for counting, the rules that matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Forward {
    /// Destination links, one bit each.
    pub links: u8,
    /// Rules that matched, one bit per index.
    pub rules: u16,
}

impl BridgeTable {
    /// An empty table.
    pub const fn new() -> Self {
        BridgeTable {
            rules: [None; MAX_BRIDGES],
        }
    }

    /// Installs `b` (or finds the identical rule) and returns its handle.
    pub fn add(&mut self, b: &Bridge<'_>) -> Result<u8, AbiError> {
        let filter = b.filter.to_buf().map_err(|_| AbiError::InvalidKey)?;
        if let Some(i) = self
            .rules
            .iter()
            .position(|r| matches!(r, Some(r) if r.src == b.src && r.dst == b.dst && r.filter == filter))
        {
            return Ok(handle_of(i));
        }
        let i = self
            .rules
            .iter()
            .position(Option::is_none)
            .ok_or(AbiError::NoResources)?;
        if let Some(slot) = self.rules.get_mut(i) {
            *slot = Some(Rule {
                src: b.src,
                filter,
                dst: b.dst,
                stats: BridgeStats::default(),
            });
        }
        Ok(handle_of(i))
    }

    /// Removes the rule behind `handle`; `Busy` if none is installed there.
    pub fn remove(&mut self, handle: u8) -> Result<(), AbiError> {
        let slot = handle_index(handle)
            .and_then(|i| self.rules.get_mut(i))
            .ok_or(AbiError::Busy)?;
        slot.take().map(drop).ok_or(AbiError::Busy)
    }

    /// The rule behind `handle`.
    pub fn get(&self, handle: u8) -> Option<&Rule> {
        self.rules.get(handle_index(handle)?)?.as_ref()
    }

    /// Removes every rule (payload stop).
    pub fn clear(&mut self) {
        self.rules = [None; MAX_BRIDGES];
    }

    /// Installed rules with their handles.
    pub fn iter(&self) -> impl Iterator<Item = (u8, &Rule)> {
        self.rules
            .iter()
            .enumerate()
            .filter_map(|(i, r)| Some((handle_of(i), r.as_ref()?)))
    }

    /// Matches a publication on `key` that arrived on link `src`.
    pub fn route(&self, src: u8, key: Key<'_>) -> Forward {
        let mut f = Forward::default();
        for (i, r) in self.rules.iter().enumerate() {
            if let Some(r) = r
                && r.src == src
                && r.filter.matches(key)
            {
                f.links |= 1u8.checked_shl(u32::from(r.dst)).unwrap_or(0);
                f.rules |= 1u16.checked_shl(i as u32).unwrap_or(0);
            }
        }
        f
    }

    /// Counts the outcome of a [`Forward`]: each matched rule's publication
    /// was sent (`sent` has its destination bit) or dropped.
    pub fn account(&mut self, f: Forward, sent: u8) {
        for (i, r) in self.rules.iter_mut().enumerate() {
            if let Some(r) = r
                && f.rules & 1u16.checked_shl(i as u32).unwrap_or(0) != 0
            {
                let ok = sent & 1u8.checked_shl(u32::from(r.dst)).unwrap_or(0) != 0;
                let c = if ok {
                    &mut r.stats.forwarded
                } else {
                    &mut r.stats.dropped
                };
                *c = c.saturating_add(1);
            }
        }
    }
}

/// A key's bytes as [`KeyBuf`] (convenience for tables keyed by owned keys).
pub fn owned(key: Key<'_>) -> Result<KeyBuf<MAX_KEY_LEN>, AbiError> {
    KeyBuf::from_key(key).map_err(|_| AbiError::InvalidKey)
}

#[cfg(test)]
mod tests {
    use super::*;

    const K: fn(&'static str) -> Key<'static> = Key::from_static;

    #[test]
    fn cl2_links_are_checked_first() {
        assert_eq!(links(0, 1), Ok((0, 1)));
        assert_eq!(links(1, 1), Err(AbiError::InvalidArgument));
        assert_eq!(links(-1, 1), Err(AbiError::InvalidArgument));
        assert_eq!(links(0, MAX_LINKS as i32), Err(AbiError::InvalidArgument));
        assert_eq!(parse(2, b"\xff", 2), Err(AbiError::InvalidArgument));
    }

    #[test]
    fn cl2_exact_and_prefix_keys() {
        assert_eq!(filter(b"demo/imu"), Ok(Filter::Exact(K("demo/imu"))));
        assert_eq!(filter(b"demo/**"), Ok(Filter::Prefix(K("demo"))));
        assert_eq!(filter(b"a/b/**"), Ok(Filter::Prefix(K("a/b"))));
        for bad in [
            &b"**"[..],
            b"/**",
            b"demo/*",
            b"demo/**/x",
            b"de mo",
            b"",
            b"\xff",
            b"sys",
            b"sys/log",
            b"sys/**",
        ] {
            assert_eq!(filter(bad), Err(AbiError::InvalidKey), "{bad:?}");
        }
        // `system` is not under `sys`.
        assert_eq!(filter(b"system/**"), Ok(Filter::Prefix(K("system"))));
    }

    #[test]
    fn cl2_table_matches_segment_prefixes_and_dedupes() {
        let mut t = BridgeTable::new();
        let a = t.add(&parse(0, b"demo/**", 1).unwrap()).unwrap();
        assert_eq!(a, BRIDGE_BASE);
        assert_eq!(t.add(&parse(0, b"demo/**", 1).unwrap()), Ok(a));
        let b = t.add(&parse(0, b"demo/x", 1).unwrap()).unwrap();
        let c = t.add(&parse(1, b"back", 0).unwrap()).unwrap();
        assert_eq!((b, c), (BRIDGE_BASE + 1, BRIDGE_BASE + 2));

        let f = t.route(0, K("demo/x"));
        assert_eq!(f, Forward { links: 0b10, rules: 0b011 });
        assert_eq!(t.route(0, K("demo")).rules, 0b001);
        assert_eq!(t.route(0, K("demox")), Forward::default());
        assert_eq!(t.route(1, K("demo/x")), Forward::default());
        assert_eq!(t.route(1, K("back")).links, 0b01);

        t.account(f, 0b10);
        t.account(t.route(0, K("demo/y")), 0);
        assert_eq!(t.get(a).unwrap().stats, BridgeStats { forwarded: 1, dropped: 1 });
        assert_eq!(t.get(b).unwrap().stats, BridgeStats { forwarded: 1, dropped: 0 });

        assert_eq!(t.remove(b), Ok(()));
        assert_eq!(t.remove(b), Err(AbiError::Busy));
        assert_eq!(t.remove(3), Err(AbiError::Busy));
        assert_eq!(t.route(0, K("demo/x")).rules, 0b001);
        // The freed slot is reused.
        assert_eq!(t.add(&parse(0, b"z", 1).unwrap()), Ok(b));
    }

    #[test]
    fn cl2_a_full_table_is_no_resources() {
        let mut t = BridgeTable::new();
        let keys = [
            "k0", "k1", "k2", "k3", "k4", "k5", "k6", "k7", "k8", "k9", "ka", "kb", "kc", "kd",
            "ke", "kf",
        ];
        for (i, k) in keys.iter().enumerate() {
            let h = t.add(&parse(0, k.as_bytes(), 1).unwrap()).unwrap();
            assert_eq!(handle_index(h), Some(i));
        }
        assert_eq!(t.add(&parse(0, b"kg", 1).unwrap()), Err(AbiError::NoResources));
        assert_eq!(t.iter().count(), MAX_BRIDGES);
        t.clear();
        assert_eq!(t.iter().count(), 0);
        assert_eq!(handle_index(BRIDGE_BASE + MAX_BRIDGES as u8), None);
        assert_eq!(handle_index(0), None);
    }
}
