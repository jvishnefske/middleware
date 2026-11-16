//! [`KvTable`]: a fixed-capacity, heap-free key-value store in RAM, and the
//! reference implementation of the [`kv`](super) contract.
//!
//! # Layout
//!
//! Up to `N` entries live in one `heapless::Vec`, kept sorted by the byte
//! order of their keys. Lookups are a binary search; an insert or a delete
//! shifts the entries after it (at most `N`). Each entry stores its key in
//! a [`KeyBuf<K>`] and its value in a `heapless::Vec<u8, V>`, so the table's
//! size is fixed at compile time: roughly `N * (K + V + 24)` bytes.
//!
//! # Behaviour
//!
//! * **Limits.** [`KvStoreSync::LIMITS`] is `K` and `V`: a key longer than
//!   `K` bytes or a value longer than `V` bytes is [`KvError::TooLarge`].
//!   `K` must be 1..=[`MAX_KEY_LEN`]; a table with any other `K` fails to
//!   compile when [`KvTable::new`] is instantiated.
//! * **Full.** With `N` entries present, a put of a new key is
//!   [`KvError::Full`]. Overwriting an existing key always has room, and a
//!   delete frees a slot.
//! * **Revisions.** One counter, starting at 1, advanced by every successful
//!   put and by every delete that removes an entry. A delete returns the
//!   revision the removed entry had (the revision of the put that last set
//!   it); the delete's own stamp is consumed but not returned. A delete of an
//!   absent key whose [`Expect`] holds ([`Expect::Any`] or
//!   [`Expect::Absent`]) writes nothing and returns `Ok(None)`. In the
//!   unreachable case that the 64-bit counter runs out, writes fail with
//!   [`TableError::RevisionsExhausted`] instead of reusing a revision.
//! * **Precedence of errors.** A put checks, in order: limits
//!   ([`KvError::TooLarge`]), the precondition ([`KvError::Conflict`]),
//!   room ([`KvError::Full`]). No error changes the table.
//! * **Scans** walk the sorted entries from the first key at or after the
//!   filter's key and strictly after `after`, stopping where keys no longer
//!   start with the filter's key bytes. The page limit is the whole table
//!   (`N` entries), so a scan ends early only when the callback returns
//!   [`Flow::Stop`]; [`Scan::complete`] is then `true` if no matching entry
//!   follows the last one visited. A value longer than the caller's buffer
//!   ends the scan with [`KvError::BufferTooSmall`]; the entries before it
//!   were visited, and a caller resumes after the last key it saw.
//! * **Durability: none.** The table lives in RAM and is lost on reset or
//!   power loss; a write is complete when the call returns, and there are no
//!   torn writes because nothing is persisted.

use crate::key::{AsKey, Key, KeyBuf, KeyFilter, MAX_KEY_LEN};

use super::{Entry, Expect, Flow, KvError, KvLimits, KvStoreSync, Revision, Scan, expect_holds};

/// Failure of a [`KvTable`] carried in [`KvError::Backend`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableError {
    /// The revision counter reached `u64::MAX`; no further write can be
    /// stamped without reusing a revision.
    RevisionsExhausted,
}

/// One stored entry.
#[derive(Debug, Clone)]
struct Slot<const K: usize, const V: usize> {
    key: KeyBuf<K>,
    value: heapless::Vec<u8, V>,
    revision: Revision,
}

impl<const K: usize, const V: usize> Slot<K, V> {
    fn key_bytes(&self) -> &[u8] {
        self.key.as_str().as_bytes()
    }
}

/// A sorted, fixed-capacity store of up to `N` entries with keys of at most
/// `K` bytes and values of at most `V` bytes. See the module docs.
#[derive(Debug, Clone)]
pub struct KvTable<const N: usize, const K: usize, const V: usize> {
    entries: heapless::Vec<Slot<K, V>, N>,
    /// The revision the next write will be stamped with.
    next: Revision,
}

impl<const N: usize, const K: usize, const V: usize> Default for KvTable<N, K, V> {
    fn default() -> Self {
        Self::new()
    }
}

/// The result of a binary search: the entry's index, or where it would go.
type Position = Result<usize, usize>;

impl<const N: usize, const K: usize, const V: usize> KvTable<N, K, V> {
    /// Compile-time check that `K` is a usable key capacity.
    const KEY_CAPACITY_OK: () = assert!(
        K >= 1 && K <= MAX_KEY_LEN,
        "KvTable key capacity K must be 1..=MAX_KEY_LEN"
    );

    /// An empty table whose first write gets [`Revision`] 1.
    pub const fn new() -> Self {
        let () = Self::KEY_CAPACITY_OK;
        KvTable {
            entries: heapless::Vec::new(),
            next: Revision(1),
        }
    }

    /// Entries stored.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True if no entry is stored.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Entries the table can hold (`N`).
    pub const fn capacity(&self) -> usize {
        N
    }

    /// The revision the next successful write will be stamped with.
    pub const fn next_revision(&self) -> Revision {
        self.next
    }

    fn find(&self, key: &[u8]) -> Position {
        self.entries.binary_search_by(|s| s.key_bytes().cmp(key))
    }

    fn current(&self, at: Position) -> Option<Revision> {
        match at {
            Ok(i) => self.entries.get(i).map(|s| s.revision),
            Err(_) => None,
        }
    }

    /// Takes the next revision, failing rather than wrapping.
    fn stamp(&self) -> Result<(Revision, Revision), KvError<TableError>> {
        let issued = self.next;
        let after = issued
            .0
            .checked_add(1)
            .ok_or(KvError::Backend(TableError::RevisionsExhausted))?;
        Ok((issued, Revision(after)))
    }

    /// Index of the first entry the scan may visit.
    fn scan_start(&self, filter: KeyFilter<'_>, after: Option<Key<'_>>) -> usize {
        let from_filter = match self.find(filter.key().as_bytes()) {
            Ok(i) | Err(i) => i,
        };
        let from_after = match after.map(|a| self.find(a.as_bytes())) {
            None => 0,
            Some(Ok(i)) => i.saturating_add(1),
            Some(Err(i)) => i,
        };
        from_filter.max(from_after)
    }

    /// True if an entry at or after index `from` matches `filter` (within
    /// the range of keys starting with the filter's key bytes).
    fn any_match_from(&self, filter: KeyFilter<'_>, from: usize) -> bool {
        let range = filter.key().as_bytes();
        self.entries
            .iter()
            .skip(from)
            .take_while(|s| s.key_bytes().starts_with(range))
            .any(|s| filter.matches(s.key.as_key()))
    }
}

impl<const N: usize, const K: usize, const V: usize> KvStoreSync for KvTable<N, K, V> {
    type Error = TableError;

    const LIMITS: KvLimits = KvLimits {
        max_key: K,
        max_value: V,
    };

    fn get<'b>(
        &mut self,
        key: Key<'_>,
        buf: &'b mut [u8],
    ) -> Result<Option<Entry<'b>>, KvError<TableError>> {
        let Ok(i) = self.find(key.as_bytes()) else {
            return Ok(None);
        };
        let Some(slot) = self.entries.get(i) else {
            return Ok(None);
        };
        let needed = slot.value.len();
        let out = buf
            .get_mut(..needed)
            .ok_or(KvError::BufferTooSmall { needed })?;
        out.copy_from_slice(&slot.value);
        Ok(Some(Entry {
            value: out,
            revision: slot.revision,
        }))
    }

    fn put(
        &mut self,
        key: Key<'_>,
        value: &[u8],
        expect: Expect,
    ) -> Result<Revision, KvError<TableError>> {
        if key.as_bytes().len() > K || value.len() > V {
            return Err(KvError::TooLarge);
        }
        let at = self.find(key.as_bytes());
        let current = self.current(at);
        if !expect_holds(expect, current) {
            return Err(KvError::Conflict { current });
        }
        if at.is_err() && self.entries.is_full() {
            return Err(KvError::Full);
        }
        let (revision, next) = self.stamp()?;
        let value = heapless::Vec::from_slice(value).map_err(|_| KvError::TooLarge)?;
        match at {
            Ok(i) => {
                let slot = self.entries.get_mut(i).ok_or(KvError::Full)?;
                slot.value = value;
                slot.revision = revision;
            }
            Err(i) => {
                let key = KeyBuf::from_key(key).map_err(|_| KvError::TooLarge)?;
                if i > self.entries.len() {
                    return Err(KvError::Full);
                }
                self.entries
                    .insert(
                        i,
                        Slot {
                            key,
                            value,
                            revision,
                        },
                    )
                    .map_err(|_| KvError::Full)?;
            }
        }
        self.next = next;
        Ok(revision)
    }

    fn delete(
        &mut self,
        key: Key<'_>,
        expect: Expect,
    ) -> Result<Option<Revision>, KvError<TableError>> {
        let at = self.find(key.as_bytes());
        let current = self.current(at);
        if !expect_holds(expect, current) {
            return Err(KvError::Conflict { current });
        }
        let (Ok(i), Some(removed)) = (at, current) else {
            return Ok(None);
        };
        if i >= self.entries.len() {
            return Ok(None);
        }
        let (_, next) = self.stamp()?;
        self.entries.remove(i);
        self.next = next;
        Ok(Some(removed))
    }

    fn scan<F>(
        &mut self,
        filter: KeyFilter<'_>,
        after: Option<Key<'_>>,
        buf: &mut [u8],
        mut on_entry: F,
    ) -> Result<Scan, KvError<TableError>>
    where
        F: FnMut(Key<'_>, Entry<'_>) -> Flow,
    {
        let range = filter.key().as_bytes();
        let start = self.scan_start(filter, after);
        let mut visited = 0;
        for (i, slot) in self.entries.iter().enumerate().skip(start) {
            if !slot.key_bytes().starts_with(range) {
                break;
            }
            let key = slot.key.as_key();
            if !filter.matches(key) {
                continue;
            }
            let needed = slot.value.len();
            let out = buf
                .get_mut(..needed)
                .ok_or(KvError::BufferTooSmall { needed })?;
            out.copy_from_slice(&slot.value);
            visited += 1;
            let entry = Entry {
                value: out,
                revision: slot.revision,
            };
            if on_entry(key, entry) == Flow::Stop {
                return Ok(Scan {
                    visited,
                    complete: !self.any_match_from(filter, i.saturating_add(1)),
                });
            }
        }
        Ok(Scan {
            visited,
            complete: true,
        })
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::borrow::ToOwned;
    use std::string::String;
    use std::vec::Vec;

    type Small = KvTable<4, 16, 8>;

    fn k(s: &str) -> Key<'_> {
        Key::new(s).unwrap()
    }

    fn keys<const N: usize, const K: usize, const V: usize>(
        t: &mut KvTable<N, K, V>,
        filter: KeyFilter<'_>,
        after: Option<Key<'_>>,
    ) -> Vec<String> {
        let mut seen = Vec::new();
        let mut buf = [0u8; 64];
        let scan = t
            .scan(filter, after, &mut buf, |key, _| {
                seen.push(key.as_str().into());
                Flow::Continue
            })
            .unwrap();
        assert!(scan.complete);
        assert_eq!(scan.visited, seen.len());
        seen
    }

    #[test]
    fn limits_follow_the_const_parameters() {
        assert_eq!(
            <Small as KvStoreSync>::LIMITS,
            KvLimits {
                max_key: 16,
                max_value: 8
            }
        );
        let t = Small::new();
        assert_eq!((t.len(), t.capacity(), t.is_empty()), (0, 4, true));
        assert_eq!(t.next_revision(), Revision(1));
    }

    #[test]
    fn entries_stay_sorted_by_key_bytes() {
        let mut t = KvTable::<8, 16, 8>::new();
        for key in ["b", "a/b", "a-b", "a0", "a"] {
            t.put(k(key), b"v", Expect::Any).unwrap();
        }
        let all = keys(&mut t, KeyFilter::Prefix(k("a")), None);
        // A prefix filter is segment-wise: "a-b" and "a0" are not under "a".
        assert_eq!(all, ["a", "a/b"]);
        // Storage is byte order, not segment order: '-' (0x2d) < '/' (0x2f)
        // < '0' (0x30).
        let sorted: Vec<String> = t
            .entries
            .iter()
            .map(|s| s.key.as_str().to_owned())
            .collect();
        assert_eq!(sorted, ["a", "a-b", "a/b", "a0", "b"]);
    }

    #[test]
    fn put_overwrite_keeps_one_entry_and_restamps() {
        let mut t = Small::new();
        assert_eq!(t.put(k("x"), b"1", Expect::Absent), Ok(Revision(1)));
        assert_eq!(
            t.put(k("x"), b"22", Expect::Exact(Revision(1))),
            Ok(Revision(2))
        );
        assert_eq!(t.len(), 1);
        let mut buf = [0u8; 8];
        assert_eq!(
            t.get(k("x"), &mut buf).unwrap(),
            Some(Entry {
                value: b"22",
                revision: Revision(2)
            })
        );
    }

    #[test]
    fn full_rejects_new_keys_but_not_updates() {
        let mut t = Small::new();
        for key in ["a", "b", "c", "d"] {
            t.put(k(key), b"v", Expect::Any).unwrap();
        }
        assert_eq!(t.put(k("e"), b"v", Expect::Any), Err(KvError::Full));
        assert_eq!(t.next_revision(), Revision(5));
        assert_eq!(t.put(k("a"), b"w", Expect::Any), Ok(Revision(5)));
        assert_eq!(t.delete(k("b"), Expect::Any), Ok(Some(Revision(2))));
        assert_eq!(t.put(k("e"), b"v", Expect::Any), Ok(Revision(7)));
    }

    #[test]
    fn too_large_is_checked_before_anything_else() {
        let mut t = Small::new();
        let long_key = "abcdefghijklmnopq"; // 17 > K = 16
        assert_eq!(t.put(k(long_key), b"", Expect::Any), Err(KvError::TooLarge));
        assert_eq!(t.put(k("a"), &[0; 9], Expect::Any), Err(KvError::TooLarge));
        assert_eq!(t.put(k("a"), &[0; 8], Expect::Any), Ok(Revision(1)));
        // Over the limit and a failing precondition: TooLarge wins.
        assert_eq!(
            t.put(k("a"), &[0; 9], Expect::Absent),
            Err(KvError::TooLarge)
        );
        // A key longer than K is simply absent.
        let mut buf = [0u8; 8];
        assert_eq!(t.get(k(long_key), &mut buf), Ok(None));
    }

    #[test]
    fn delete_semantics() {
        let mut t = Small::new();
        assert_eq!(t.delete(k("a"), Expect::Any), Ok(None));
        assert_eq!(t.delete(k("a"), Expect::Absent), Ok(None));
        assert_eq!(
            t.delete(k("a"), Expect::Exact(Revision(1))),
            Err(KvError::Conflict { current: None })
        );
        assert_eq!(t.next_revision(), Revision(1));
        t.put(k("a"), b"v", Expect::Any).unwrap();
        assert_eq!(
            t.delete(k("a"), Expect::Absent),
            Err(KvError::Conflict {
                current: Some(Revision(1))
            })
        );
        assert_eq!(
            t.delete(k("a"), Expect::Exact(Revision(1))),
            Ok(Some(Revision(1)))
        );
        assert_eq!(t.next_revision(), Revision(3));
        assert!(t.is_empty());
    }

    #[test]
    fn exhausted_revisions_fail_without_writing() {
        let mut t = Small::new();
        t.next = Revision(u64::MAX);
        assert_eq!(
            t.put(k("a"), b"v", Expect::Any),
            Err(KvError::Backend(TableError::RevisionsExhausted))
        );
        assert!(t.is_empty());
        t.next = Revision(u64::MAX - 1);
        assert_eq!(t.put(k("a"), b"v", Expect::Any), Ok(Revision(u64::MAX - 1)));
        assert_eq!(
            t.delete(k("a"), Expect::Any),
            Err(KvError::Backend(TableError::RevisionsExhausted))
        );
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn scan_range_filter_after_and_stop() {
        let mut t = KvTable::<8, 16, 8>::new();
        for key in ["app", "app-x", "app/x", "app/x/y", "apple", "b"] {
            t.put(k(key), key.as_bytes().get(..1).unwrap(), Expect::Any)
                .unwrap();
        }
        assert_eq!(
            keys(&mut t, KeyFilter::Prefix(k("app")), None),
            ["app", "app/x", "app/x/y"]
        );
        assert_eq!(keys(&mut t, KeyFilter::Exact(k("app/x")), None), ["app/x"]);
        assert_eq!(
            keys(&mut t, KeyFilter::Prefix(k("app")), Some(k("app"))),
            ["app/x", "app/x/y"]
        );
        // A cursor that is not stored, and one before the filter's range.
        assert_eq!(
            keys(&mut t, KeyFilter::Prefix(k("app")), Some(k("app/w"))),
            ["app/x", "app/x/y"]
        );
        assert_eq!(
            keys(&mut t, KeyFilter::Prefix(k("app")), Some(k("a"))),
            ["app", "app/x", "app/x/y"]
        );
        assert!(keys(&mut t, KeyFilter::Prefix(k("zz")), None).is_empty());
        let mut buf = [0u8; 8];
        let stop_first = t
            .scan(KeyFilter::Prefix(k("app")), None, &mut buf, |_, _| {
                Flow::Stop
            })
            .unwrap();
        assert_eq!(
            stop_first,
            Scan {
                visited: 1,
                complete: false
            }
        );
        // Stopping on the last match: nothing matching follows ("apple"
        // shares the bytes but not the segment).
        let stop_last = t
            .scan(
                KeyFilter::Prefix(k("app")),
                Some(k("app/x")),
                &mut buf,
                |_, _| Flow::Stop,
            )
            .unwrap();
        assert_eq!(
            stop_last,
            Scan {
                visited: 1,
                complete: true
            }
        );
    }

    #[test]
    fn scan_buffer_too_small_reports_the_size() {
        let mut t = Small::new();
        t.put(k("a"), b"1", Expect::Any).unwrap();
        t.put(k("b"), b"12345", Expect::Any).unwrap();
        let mut buf = [0u8; 2];
        let mut seen = 0;
        let r = t.scan(KeyFilter::Prefix(k("a")), None, &mut buf, |_, _| {
            seen += 1;
            Flow::Continue
        });
        assert_eq!(r.map(|s| s.visited), Ok(1));
        let r = t.scan(KeyFilter::Prefix(k("b")), None, &mut buf, |_, _| {
            Flow::Continue
        });
        assert_eq!(r, Err(KvError::BufferTooSmall { needed: 5 }));
        assert_eq!(seen, 1);
    }

    #[test]
    fn blocking_adapter_completes_on_first_poll() {
        use crate::kv::{Blocking, KvStore};
        let mut b = Blocking(Small::new());
        let r = futures::executor::block_on(b.put(k("a"), b"v", Expect::Any));
        assert_eq!(r, Ok(Revision(1)));
        assert_eq!(<Blocking<Small> as KvStore>::LIMITS.max_key, 16);
    }
}
