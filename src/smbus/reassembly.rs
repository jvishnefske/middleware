//! Reassembly of fragmented messages from several masters at once.
//!
//! A [`Reassembler`] holds up to `R` messages in progress, each in an
//! `M`-byte slot, keyed by **(sender address, kind, message id)**. Blocks of
//! different masters may interleave arbitrarily on a multi-master bus; each
//! master sends the fragments of one message in index order, so within a
//! key fragments must arrive in order:
//!
//! * the expected next index is appended; with the last flag the message is
//!   complete and its slot freed;
//! * index 0 (re)starts the message, replacing a partial one with the same
//!   key; a single-fragment message (index 0 with the last flag) completes
//!   at once without taking a slot;
//! * an index already appended is a duplicate (a block the master retried
//!   after the target had taken it) and is ignored;
//! * an index past the expected one means a fragment was lost (dropped for
//!   its PEC, for instance): the message can no longer complete and is
//!   discarded; so is one that would exceed `M` bytes;
//! * a non-zero index with no message in progress is an orphan (its start
//!   was lost or expired) and is ignored;
//! * a message whose first fragment arrived `timeout_ms` or more before
//!   `now_ms` (the caller's clock) is discarded as expired;
//! * when every slot is busy, a new message evicts the one that started
//!   longest ago.
//!
//! Every discard is counted in [`Discards`]; nothing incomplete is ever
//! returned. All operations are bounded by `R` slots and `M` bytes.
//!
//! The table is mutated in place rather than returned by value: it is fed
//! once per received block, and moving `R × M` bytes per block would
//! dominate the cost (the same trade-off as the serial `Splitter`). The
//! decisions themselves are the pure [`classify`].

use super::address::Address;
use super::wire::{Fragment, Kind};

/// What identifies a message: who sent it, its kind and its id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Head {
    /// The sending node.
    pub source: Address,
    /// The message kind.
    pub kind: Kind,
    /// The message id (see [`wire`](super::wire)).
    pub id: u8,
}

impl Head {
    /// The key of the message `frag` belongs to.
    pub const fn of(frag: &Fragment<'_>) -> Head {
        Head {
            source: frag.source,
            kind: frag.kind,
            id: frag.id,
        }
    }
}

/// Why a fragment or a partial message was discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Discard {
    /// A fragment already appended.
    Duplicate,
    /// A fragment arrived out of order: one before it is missing.
    Missing,
    /// The message would exceed the slot's `M` bytes.
    Oversized,
    /// A non-zero index with no message in progress.
    Orphan,
}

/// Counts of everything the reassembler discarded since creation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Discards {
    /// Duplicate fragments ignored.
    pub duplicates: u32,
    /// Messages abandoned for a missing fragment.
    pub missing: u32,
    /// Messages (or single fragments) over `M` bytes.
    pub oversized: u32,
    /// Fragments with no message in progress.
    pub orphans: u32,
    /// Partial messages that timed out.
    pub expired: u32,
    /// Partial messages evicted for a newer one when every slot was busy.
    pub evicted: u32,
}

impl Discards {
    const fn count(self, why: Discard) -> Discards {
        match why {
            Discard::Duplicate => Discards {
                duplicates: self.duplicates.saturating_add(1),
                ..self
            },
            Discard::Missing => Discards {
                missing: self.missing.saturating_add(1),
                ..self
            },
            Discard::Oversized => Discards {
                oversized: self.oversized.saturating_add(1),
                ..self
            },
            Discard::Orphan => Discards {
                orphans: self.orphans.saturating_add(1),
                ..self
            },
        }
    }
}

/// Where a message in progress stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    head: Head,
    next: u8,
    len: usize,
    started_ms: u64,
}

/// What to do with one fragment, given the message in progress with its
/// key (if any): the pure decision behind [`Reassembler::push`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// A whole message in one fragment: deliver it as is.
    Single,
    /// Start (or restart) a message in a slot with this fragment.
    Start,
    /// Append to the message in progress; `complete` if it was the last.
    Append {
        /// The fragment was the last one.
        complete: bool,
    },
    /// Ignore the fragment, keeping any message in progress.
    Ignore(Discard),
    /// Discard the fragment and the message in progress.
    Abandon(Discard),
}

/// Decides what `frag` means for `progress`, the message in progress with
/// the same key, for `M`-byte slots (pure).
pub const fn classify(progress: Option<&Progress>, frag: &Fragment<'_>, m: usize) -> Action {
    if frag.index == 0 {
        if frag.body.len() > m {
            return match progress {
                Some(_) => Action::Abandon(Discard::Oversized),
                None => Action::Ignore(Discard::Oversized),
            };
        }
        return if frag.last {
            Action::Single
        } else {
            Action::Start
        };
    }
    let Some(p) = progress else {
        return Action::Ignore(Discard::Orphan);
    };
    if frag.index < p.next {
        Action::Ignore(Discard::Duplicate)
    } else if frag.index > p.next {
        Action::Abandon(Discard::Missing)
    } else if p.len + frag.body.len() > m {
        Action::Abandon(Discard::Oversized)
    } else {
        Action::Append {
            complete: frag.last,
        }
    }
}

/// The outcome of [`Reassembler::push`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Push<'a> {
    /// The fragment was taken; the message is not complete yet.
    Pending,
    /// The message is complete: its key and body (valid until the next
    /// `push`).
    Complete {
        /// The message's key.
        head: Head,
        /// The whole body.
        body: &'a [u8],
    },
    /// The fragment was discarded (and counted).
    Discarded(Discard),
}

/// Up to `R` messages of at most `M` bytes in progress (see the module
/// docs).
pub struct Reassembler<const R: usize, const M: usize> {
    progress: [Option<Progress>; R],
    bufs: [[u8; M]; R],
    discards: Discards,
}

impl<const R: usize, const M: usize> Default for Reassembler<R, M> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const R: usize, const M: usize> Reassembler<R, M> {
    /// An empty table.
    pub const fn new() -> Self {
        Reassembler {
            progress: [None; R],
            bufs: [[0; M]; R],
            discards: Discards {
                duplicates: 0,
                missing: 0,
                oversized: 0,
                orphans: 0,
                expired: 0,
                evicted: 0,
            },
        }
    }

    /// What was discarded since creation.
    pub const fn discards(&self) -> Discards {
        self.discards
    }

    /// Messages in progress.
    pub fn in_progress(&self) -> usize {
        self.progress.iter().flatten().count()
    }

    /// Discards partial messages that started `timeout_ms` or more before
    /// `now_ms`, returning how many.
    pub fn expire(&mut self, now_ms: u64, timeout_ms: u32) -> usize {
        let mut n = 0;
        for p in self.progress.iter_mut() {
            if p.is_some_and(|p| now_ms.saturating_sub(p.started_ms) >= u64::from(timeout_ms)) {
                *p = None;
                n += 1;
            }
        }
        self.discards.expired = self
            .discards
            .expired
            .saturating_add(u32::try_from(n).unwrap_or(u32::MAX));
        n
    }

    /// Feeds one fragment received at `now_ms`; partial messages older than
    /// `timeout_ms` are expired first. `frag`'s body is copied, except for a
    /// single-fragment message, which is returned borrowing it.
    pub fn push<'a>(&'a mut self, frag: &Fragment<'a>, now_ms: u64, timeout_ms: u32) -> Push<'a> {
        self.expire(now_ms, timeout_ms);
        let head = Head::of(frag);
        let slot = self
            .progress
            .iter()
            .position(|p| p.is_some_and(|p| p.head == head));
        let current = slot
            .and_then(|i| self.progress.get(i))
            .and_then(Option::as_ref);
        match classify(current, frag, M) {
            Action::Single => {
                if let Some(p) = slot.and_then(|i| self.progress.get_mut(i)) {
                    *p = None;
                }
                Push::Complete {
                    head,
                    body: frag.body,
                }
            }
            Action::Start => {
                let i = slot.unwrap_or_else(|| self.free_slot());
                let (Some(p), Some(buf)) = (self.progress.get_mut(i), self.bufs.get_mut(i)) else {
                    // R == 0: no message longer than one fragment fits.
                    self.discards = self.discards.count(Discard::Oversized);
                    return Push::Discarded(Discard::Oversized);
                };
                if let Some(dst) = buf.get_mut(..frag.body.len()) {
                    dst.copy_from_slice(frag.body);
                }
                *p = Some(Progress {
                    head,
                    next: 1,
                    len: frag.body.len(),
                    started_ms: now_ms,
                });
                Push::Pending
            }
            Action::Append { complete } => {
                let Some(i) = slot else {
                    return Push::Pending;
                };
                let (Some(Some(p)), Some(buf)) = (self.progress.get_mut(i), self.bufs.get_mut(i))
                else {
                    return Push::Pending;
                };
                let end = p.len + frag.body.len();
                if let Some(dst) = buf.get_mut(p.len..end) {
                    dst.copy_from_slice(frag.body);
                }
                p.len = end;
                p.next = p.next.saturating_add(1);
                if !complete {
                    return Push::Pending;
                }
                if let Some(p) = self.progress.get_mut(i) {
                    *p = None;
                }
                Push::Complete {
                    head,
                    body: self
                        .bufs
                        .get(i)
                        .and_then(|b| b.get(..end))
                        .unwrap_or_default(),
                }
            }
            Action::Ignore(why) => {
                self.discards = self.discards.count(why);
                Push::Discarded(why)
            }
            Action::Abandon(why) => {
                if let Some(p) = slot.and_then(|i| self.progress.get_mut(i)) {
                    *p = None;
                }
                self.discards = self.discards.count(why);
                Push::Discarded(why)
            }
        }
    }

    /// A free slot, or the one whose message started longest ago (evicted
    /// and counted). `R` when there are no slots at all.
    fn free_slot(&mut self) -> usize {
        if let Some(i) = self.progress.iter().position(Option::is_none) {
            return i;
        }
        let oldest = (0..R).min_by_key(|&i| {
            self.progress
                .get(i)
                .and_then(|p| p.map(|p| p.started_ms))
                .unwrap_or(0)
        });
        match oldest {
            Some(i) => {
                self.discards.evicted = self.discards.evicted.saturating_add(1);
                i
            }
            None => R,
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    const T: u32 = 100;

    fn a(raw: u8) -> Address {
        Address::node(raw).unwrap()
    }

    fn f(source: u8, id: u8, index: u8, last: bool, body: &[u8]) -> Fragment<'_> {
        Fragment {
            kind: Kind::Publish,
            source: a(source),
            id,
            index,
            last,
            body,
        }
    }

    fn done(p: Push<'_>) -> Option<(u8, std::vec::Vec<u8>)> {
        match p {
            Push::Complete { head, body } => Some((head.source.get(), body.to_vec())),
            _ => None,
        }
    }

    #[test]
    fn m5_in_order_fragments_complete_and_free_the_slot() {
        let mut r = Reassembler::<2, 16>::new();
        assert_eq!(r.push(&f(0x10, 1, 0, false, b"abc"), 0, T), Push::Pending);
        assert_eq!(r.push(&f(0x10, 1, 1, false, b"def"), 1, T), Push::Pending);
        assert_eq!(r.in_progress(), 1);
        assert_eq!(
            done(r.push(&f(0x10, 1, 2, true, b"g"), 2, T)),
            Some((0x10, b"abcdefg".to_vec()))
        );
        assert_eq!(r.in_progress(), 0);
        // A single fragment completes without a slot.
        assert_eq!(
            done(r.push(&f(0x11, 9, 0, true, b"one"), 3, T)),
            Some((0x11, b"one".to_vec()))
        );
        assert_eq!(r.discards(), Discards::default());
    }

    #[test]
    fn m5_interleaved_masters_reassemble_independently() {
        let mut r = Reassembler::<4, 32>::new();
        // Three senders, and two message ids of one sender, interleaved.
        let script = [
            f(0x10, 1, 0, false, b"A0"),
            f(0x20, 1, 0, false, b"B0"),
            f(0x10, 2, 0, false, b"C0"),
            f(0x30, 1, 0, false, b"D0"),
            f(0x20, 1, 1, false, b"B1"),
            f(0x10, 1, 1, true, b"A1"),
            f(0x30, 1, 1, true, b"D1"),
            f(0x20, 1, 2, true, b"B2"),
            f(0x10, 2, 1, true, b"C1"),
        ];
        let mut got = std::vec::Vec::new();
        for (t, frag) in script.iter().enumerate() {
            if let Some(m) = done(r.push(frag, t as u64, T)) {
                got.push(m);
            }
        }
        assert_eq!(
            got,
            [
                (0x10, b"A0A1".to_vec()),
                (0x30, b"D0D1".to_vec()),
                (0x20, b"B0B1B2".to_vec()),
                (0x10, b"C0C1".to_vec()),
            ]
        );
        // Same id, different kind: separate messages.
        let req = Fragment {
            kind: Kind::Request,
            ..f(0x10, 5, 0, false, b"R")
        };
        r.push(&f(0x10, 5, 0, false, b"P"), 10, T);
        r.push(&req, 10, T);
        assert_eq!(r.in_progress(), 2);
    }

    #[test]
    fn m5_duplicates_are_ignored_and_gaps_abandon_the_message() {
        let mut r = Reassembler::<2, 16>::new();
        r.push(&f(0x10, 1, 0, false, b"ab"), 0, T);
        r.push(&f(0x10, 1, 1, false, b"cd"), 0, T);
        assert_eq!(
            r.push(&f(0x10, 1, 1, false, b"cd"), 0, T),
            Push::Discarded(Discard::Duplicate)
        );
        assert_eq!(
            done(r.push(&f(0x10, 1, 2, true, b"e"), 0, T)),
            Some((0x10, b"abcde".to_vec()))
        );
        // Fragment 1 lost: fragment 2 abandons the message, the rest are
        // orphans.
        r.push(&f(0x10, 2, 0, false, b"ab"), 0, T);
        assert_eq!(
            r.push(&f(0x10, 2, 2, false, b"ef"), 0, T),
            Push::Discarded(Discard::Missing)
        );
        assert_eq!(
            r.push(&f(0x10, 2, 3, true, b"g"), 0, T),
            Push::Discarded(Discard::Orphan)
        );
        assert_eq!(r.in_progress(), 0);
        let d = r.discards();
        assert_eq!((d.duplicates, d.missing, d.orphans), (1, 1, 1));
    }

    #[test]
    fn m5_index_zero_restarts_a_partial_message() {
        let mut r = Reassembler::<2, 16>::new();
        r.push(&f(0x10, 1, 0, false, b"old"), 0, T);
        r.push(&f(0x10, 1, 1, false, b"old"), 0, T);
        r.push(&f(0x10, 1, 0, false, b"new"), 0, T);
        assert_eq!(
            done(r.push(&f(0x10, 1, 1, true, b"!"), 0, T)),
            Some((0x10, b"new!".to_vec()))
        );
    }

    #[test]
    fn m5_oversized_messages_are_discarded() {
        let mut r = Reassembler::<2, 8>::new();
        assert_eq!(
            r.push(&f(0x10, 1, 0, true, &[0; 9]), 0, T),
            Push::Discarded(Discard::Oversized)
        );
        r.push(&f(0x10, 2, 0, false, &[0; 5]), 0, T);
        assert_eq!(
            r.push(&f(0x10, 2, 1, true, &[0; 4]), 0, T),
            Push::Discarded(Discard::Oversized)
        );
        assert_eq!(r.in_progress(), 0);
        assert_eq!(r.discards().oversized, 2);
        // Exactly M fits.
        r.push(&f(0x10, 3, 0, false, &[1; 5]), 0, T);
        assert!(done(r.push(&f(0x10, 3, 1, true, &[2; 3]), 0, T)).is_some());
    }

    #[test]
    fn m5_partial_messages_expire_on_the_callers_clock() {
        let mut r = Reassembler::<2, 16>::new();
        r.push(&f(0x10, 1, 0, false, b"ab"), 1_000, T);
        // Just inside the timeout: still in progress.
        assert_eq!(r.push(&f(0x20, 1, 0, false, b"x"), 1_099, T), Push::Pending);
        assert_eq!(r.in_progress(), 2);
        // At the timeout the first is gone; its next fragment is an orphan.
        assert_eq!(
            r.push(&f(0x10, 1, 1, true, b"c"), 1_100, T),
            Push::Discarded(Discard::Orphan)
        );
        assert_eq!(r.discards().expired, 1);
        assert_eq!(r.expire(5_000, T), 1);
        assert_eq!(r.in_progress(), 0);
    }

    #[test]
    fn m5_a_full_table_evicts_the_oldest_message() {
        let mut r = Reassembler::<2, 16>::new();
        r.push(&f(0x10, 1, 0, false, b"a"), 0, T);
        r.push(&f(0x20, 1, 0, false, b"b"), 5, T);
        r.push(&f(0x30, 1, 0, false, b"c"), 6, T);
        assert_eq!(r.discards().evicted, 1);
        assert_eq!(
            r.push(&f(0x10, 1, 1, true, b"a"), 7, T),
            Push::Discarded(Discard::Orphan)
        );
        assert!(done(r.push(&f(0x20, 1, 1, true, b"b"), 7, T)).is_some());
        assert!(done(r.push(&f(0x30, 1, 1, true, b"c"), 7, T)).is_some());
        // With no slots, single fragments still complete.
        let mut none = Reassembler::<0, 16>::new();
        assert!(done(none.push(&f(0x10, 1, 0, true, b"s"), 0, T)).is_some());
        assert!(matches!(
            none.push(&f(0x10, 2, 0, false, b"m"), 0, T),
            Push::Discarded(_)
        ));
    }

    #[test]
    fn m5_hostile_fragment_streams_never_panic_or_return_partials() {
        let mut r = Reassembler::<3, 40>::new();
        let mut x: u32 = 7;
        let body = [0xEEu8; 64];
        let mut complete = 0;
        for t in 0..50_000u64 {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let frag = Fragment {
                kind: if x & 1 == 0 {
                    Kind::Publish
                } else {
                    Kind::Reply
                },
                source: a(0x10 + (x >> 4) as u8 % 3),
                id: (x >> 8) as u8 % 2,
                index: (x >> 12) as u8 % 6,
                last: (x >> 16).is_multiple_of(4),
                body: &body[..(x >> 20) as usize % 20],
            };
            if let Push::Complete { body, .. } = r.push(&frag, t / 10, 50) {
                assert!(body.len() <= 40);
                complete += 1;
            }
        }
        assert!(complete > 0);
    }
}
