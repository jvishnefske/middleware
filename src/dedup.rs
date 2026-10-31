//! Duplicate suppression for broadcast transports.
//!
//! On a broadcast medium (BLE advertising, ESP-NOW) every node repeats its
//! messages, and every node's sequence starts at 0. [`SeenTable`] therefore
//! remembers the last sequence number per **(sender, key)** pair, not per
//! key, so two senders on one key never suppress each other. The table is a
//! fixed-size value (no heap); when full, the least recently updated entry is
//! replaced. Transitions are immutable: [`SeenTable::observe`] returns a new
//! table.
//!
//! The same table computes sample gaps: [`SeenTable::check`] classifies a
//! sequence number as first, next (with how many were lost), repeat or
//! behind ([`SeqCheck`]), which backends report as
//! [`SampleMeta::gap`](crate::transport::SampleMeta::gap).
//!
//! Also home to [`Sequence`] (a wrapping per-sender counter) and the FNV-1a
//! hashes the backends use for key ids.

use crate::key::Key;
use crate::transport::SenderId;

/// 32-bit FNV-1a over `bytes`.
pub const fn fnv1a32(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u32;
        hash = hash.wrapping_mul(0x0100_0193);
        i += 1;
    }
    hash
}

/// 64-bit FNV-1a over `bytes`.
pub const fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        i += 1;
    }
    hash
}

/// 16-bit id of `key`: its 64-bit FNV-1a folded to 16 bits. Shared by the
/// BLE advertising and ESP-NOW backends, so a key has the same id on both.
pub const fn short_id(key: Key<'_>) -> u16 {
    let h = fnv1a64(key.as_bytes());
    (h ^ (h >> 16) ^ (h >> 32) ^ (h >> 48)) as u16
}

/// A wrapping 8-bit message sequence number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Sequence(u8);

impl Sequence {
    /// A sequence starting at `value`.
    pub const fn new(value: u8) -> Self {
        Sequence(value)
    }

    /// The raw value.
    pub const fn get(self) -> u8 {
        self.0
    }

    /// The following sequence number.
    pub const fn next(self) -> Self {
        Sequence(self.0.wrapping_add(1))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    sender: SenderId,
    key: u32,
    seq: u8,
    stamp: u32,
}

/// How a sequence number relates to the last one seen from the same
/// (sender, stream), as decided by [`SeenTable::check`].
///
/// With 8-bit wrapping sequences, `delta = seq - last` (mod 256) is read as:
/// 0 a repeat, 1..=128 forwards with `delta - 1` messages missing in
/// between, 129..=255 backwards (a late, reordered message or a restarted
/// sender). A jump of more than 128 cannot be told from going backwards, so
/// losses that large are reported as [`SeqCheck::Behind`] (gap unknown).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeqCheck {
    /// Nothing was remembered for this pair: the gap is unknown.
    First,
    /// The message follows the last one; `lost` messages are missing in
    /// between (0 when it is the very next).
    Next {
        /// Messages skipped, 0..=127.
        lost: u16,
    },
    /// The same sequence number as last time: a repeat.
    Repeat,
    /// The sequence went backwards: reordered, or the sender restarted. The
    /// table resynchronises to it.
    Behind,
}

impl SeqCheck {
    /// Classifies `seq` against the `last` one seen (`None`: nothing seen).
    pub const fn classify(last: Option<u8>, seq: u8) -> SeqCheck {
        let Some(last) = last else {
            return SeqCheck::First;
        };
        match seq.wrapping_sub(last) {
            0 => SeqCheck::Repeat,
            d @ 1..=128 => SeqCheck::Next { lost: d as u16 - 1 },
            _ => SeqCheck::Behind,
        }
    }

    /// The value for [`SampleMeta::gap`](crate::transport::SampleMeta::gap):
    /// `Some(lost)` for [`SeqCheck::Next`], `None` otherwise.
    pub const fn gap(self) -> Option<u16> {
        match self {
            SeqCheck::Next { lost } => Some(lost),
            _ => None,
        }
    }

    /// False only for a [`SeqCheck::Repeat`]: what deduplication keeps.
    pub const fn is_fresh(self) -> bool {
        !matches!(self, SeqCheck::Repeat)
    }
}

/// Last sequence number seen per (sender, stream), for up to `K` pairs.
///
/// A stream is normally a key ([`SeenTable::observe`], [`SeenTable::check`]);
/// [`SeenTable::check_stream`] takes any 32-bit stream id instead, for a
/// sender that numbers all its keys with one counter (pass a constant) or
/// streams that are not keys.
///
/// A gap is only meaningful when the receiver sees every message the
/// sender numbered in that stream: a sender with one counter over all keys,
/// seen through a subscription to some of its keys, looks lossy. Senders
/// that want receivers to compute gaps per key number each key separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeenTable<const K: usize> {
    entries: [Option<Entry>; K],
    clock: u32,
}

impl<const K: usize> Default for SeenTable<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const K: usize> SeenTable<K> {
    /// A table that has seen nothing.
    pub const fn new() -> Self {
        SeenTable {
            entries: [None; K],
            clock: 0,
        }
    }

    /// Records `seq` from `sender` on `key`. Returns the new table and whether
    /// the message is fresh (not the same sequence as last time for that
    /// pair). A full table replaces its least recently updated entry.
    pub fn observe(self, sender: SenderId, key: Key<'_>, seq: u8) -> (Self, bool) {
        let (table, check) = self.check(sender, key, seq);
        (table, check.is_fresh())
    }

    /// Records `seq` from `sender` on `key` and classifies it against the
    /// previous one ([`SeqCheck`]): the gap helper for backends with 8-bit
    /// sequences. A full table replaces its least recently updated entry.
    pub fn check(self, sender: SenderId, key: Key<'_>, seq: u8) -> (Self, SeqCheck) {
        self.check_stream(sender, fnv1a32(key.as_bytes()), seq)
    }

    /// [`SeenTable::check`] for a caller-chosen 32-bit `stream` id.
    pub fn check_stream(self, sender: SenderId, stream: u32, seq: u8) -> (Self, SeqCheck) {
        let clock = self.clock.wrapping_add(1);
        let mut entries = self.entries;
        let slot = entries
            .iter()
            .position(|e| matches!(e, Some(e) if e.sender == sender && e.key == stream))
            .or_else(|| entries.iter().position(Option::is_none))
            .or_else(|| {
                // Oldest by wrapping age relative to `clock`.
                (0..K).max_by_key(|&i| entries[i].map_or(0, |e| clock.wrapping_sub(e.stamp)))
            });
        let Some(slot) = slot else {
            // K == 0: nothing can be remembered, so everything is first.
            return (SeenTable { entries, clock }, SeqCheck::First);
        };
        let last = match entries[slot] {
            Some(e) if e.sender == sender && e.key == stream => Some(e.seq),
            _ => None,
        };
        entries[slot] = Some(Entry {
            sender,
            key: stream,
            seq,
            stamp: clock,
        });
        (SeenTable { entries, clock }, SeqCheck::classify(last, seq))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: SenderId = SenderId([1; 8]);
    const B: SenderId = SenderId([2; 8]);
    const K1: Key<'static> = Key::from_static("app/one");
    const K2: Key<'static> = Key::from_static("app/two");

    #[test]
    fn fnv_reference_vectors() {
        assert_eq!(fnv1a32(b""), 0x811c_9dc5);
        assert_eq!(fnv1a32(b"a"), 0xe40c_292c);
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn repeats_are_dropped_per_sender_and_key() {
        let t = SeenTable::<4>::new();
        let (t, fresh) = t.observe(A, K1, 0);
        assert!(fresh);
        let (t, fresh) = t.observe(A, K1, 0);
        assert!(!fresh, "same sender, key and seq is a repeat");
        // Another sender starting at the same seq is not suppressed.
        let (t, fresh) = t.observe(B, K1, 0);
        assert!(fresh);
        let (t, fresh) = t.observe(A, K2, 0);
        assert!(fresh, "other key");
        let (_, fresh) = t.observe(A, K1, 1);
        assert!(fresh, "next seq");
    }

    #[test]
    fn full_table_replaces_least_recent() {
        let t = SeenTable::<2>::new();
        let (t, _) = t.observe(A, K1, 5);
        let (t, _) = t.observe(B, K1, 5);
        // Touch A so B is the least recent, then a third pair evicts B.
        let (t, _) = t.observe(A, K1, 6);
        let (t, _) = t.observe(A, K2, 1);
        let (t, fresh) = t.observe(A, K1, 6);
        assert!(!fresh, "A/K1 survived");
        let (_, fresh) = t.observe(B, K1, 5);
        assert!(fresh, "B/K1 was evicted, so its repeat looks fresh");
    }

    #[test]
    fn limits_fit_independent_and_shared_budgets() {
        use crate::transport::Limits;
        let independent = Limits::new(64, 21);
        assert!(independent.fits(64, 21));
        assert!(!independent.fits(10, 22), "payload cap applies on its own");
        let shared = Limits::shared(64, 243);
        assert_eq!(shared.max_payload, 179);
        assert!(shared.fits(64, 179));
        assert!(shared.fits(10, 233), "a shorter key frees payload room");
        assert!(!shared.fits(10, 234));
        assert!(!shared.fits(65, 0));
        assert!(Limits::UNBOUNDED.fits(128, usize::MAX - 128));
    }

    #[test]
    fn gaps_are_computed_from_wrapping_sequences() {
        let t = SeenTable::<4>::new();
        let (t, c) = t.check(A, K1, 250);
        assert_eq!((c, c.gap()), (SeqCheck::First, None));
        let (t, c) = t.check(A, K1, 251);
        assert_eq!(c.gap(), Some(0));
        // 252..=255 and 0..=1 lost across the wrap.
        let (t, c) = t.check(A, K1, 2);
        assert_eq!(c, SeqCheck::Next { lost: 6 });
        let (t, c) = t.check(A, K1, 2);
        assert_eq!((c, c.is_fresh(), c.gap()), (SeqCheck::Repeat, false, None));
        let (t, c) = t.check(A, K1, 1);
        assert_eq!(c, SeqCheck::Behind, "late or restarted");
        let (t, c) = t.check(A, K1, 2);
        assert_eq!(c.gap(), Some(0), "resynchronised to the late one");
        // Other sender and other stream are independent.
        let (t, c) = t.check(B, K1, 9);
        assert_eq!(c, SeqCheck::First);
        let (_, c) = t.check_stream(A, 0, 3);
        assert_eq!(c, SeqCheck::First);
        assert_eq!(
            SeqCheck::classify(Some(0), 128),
            SeqCheck::Next { lost: 127 }
        );
        assert_eq!(SeqCheck::classify(Some(0), 129), SeqCheck::Behind);
    }

    #[test]
    fn sequence_wraps() {
        assert_eq!(Sequence::new(255).next().get(), 0);
        assert_eq!(Sequence::default().get(), 0);
    }

    #[test]
    fn zero_sized_table_treats_everything_as_fresh() {
        let (t, a) = SeenTable::<0>::new().observe(A, K1, 0);
        let (_, b) = t.observe(A, K1, 0);
        assert!(a && b);
    }
}
