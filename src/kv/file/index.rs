//! The in-memory state of [`FileKv`](super::FileKv), as pure functions:
//! replay of logged records into an index, planning of writes (the
//! [`Expect`] check, limits, the next revision), and the ordered selection
//! a scan walks. No I/O happens here; the shell appends what a plan returns
//! and only then commits it.
//!
//! Every live entry is held in memory (key, revision and value), so reads
//! never touch the file and the resident size is about the live data size.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::vec::Vec;

use super::log::{Corruption, Located, MAX_VALUE, Record};
use crate::key::{AsKey, Key, KeyBuf, KeyFilter, MAX_KEY_LEN};
use crate::kv::{Expect, Revision, expect_holds};

/// A stored key, ordered by its bytes (the scan order of the contract).
#[derive(Clone, Copy, PartialEq, Eq)]
struct StoredKey(KeyBuf<MAX_KEY_LEN>);

impl StoredKey {
    /// `None` only if `key` is longer than [`MAX_KEY_LEN`], which a
    /// validated key never is.
    fn new(key: Key<'_>) -> Option<Self> {
        KeyBuf::from_key(key).ok().map(StoredKey)
    }
}

impl Ord for StoredKey {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.0.as_str().as_bytes().cmp(other.0.as_str().as_bytes())
    }
}

impl PartialOrd for StoredKey {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// A live entry.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stored {
    revision: Revision,
    value: Vec<u8>,
}

/// Why a planned write is refused. The index is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The [`Expect`] did not hold; the key's current revision.
    Conflict { current: Option<Revision> },
    /// The value is longer than [`MAX_VALUE`].
    TooLarge,
    /// The revision counter is exhausted (`u64::MAX` was issued).
    Full,
}

/// A replayed record that breaks the revision order, at its file offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayError {
    pub offset: u64,
    pub why: Corruption,
}

/// Live entries plus the revision counter.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Index {
    entries: BTreeMap<StoredKey, Stored>,
    /// The highest revision that may have been issued: every new write is
    /// stamped above it.
    issued: u64,
    /// The revision of the last replayed or committed put or delete, for
    /// the replay order check.
    last_write: u64,
}

impl core::fmt::Debug for Index {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Index")
            .field("entries", &self.entries.len())
            .field("issued", &self.issued)
            .finish()
    }
}

impl Index {
    /// An empty store whose first write gets revision 1.
    pub fn new() -> Self {
        Self::default()
    }

    /// Folds `records` (in file order) into an index. A put or delete must
    /// carry a revision above the previous put or delete; a checkpoint
    /// raises the counter to at least its revision. A delete of an absent
    /// key is accepted and changes nothing but the counter.
    pub fn replay(records: &[Located<'_>]) -> Result<Self, ReplayError> {
        records.iter().try_fold(Self::new(), |index, located| {
            index.replayed(&located.record).map_err(|why| ReplayError {
                offset: located.offset,
                why,
            })
        })
    }

    /// The state after replaying one record.
    fn replayed(mut self, record: &Record<'_>) -> Result<Self, Corruption> {
        match *record {
            Record::Checkpoint { revision } => {
                self.issued = self.issued.max(revision);
                Ok(self)
            }
            Record::Put { revision, .. } | Record::Delete { revision, .. } => {
                if revision == 0 || revision <= self.last_write {
                    return Err(Corruption::RevisionOrder);
                }
                self.apply(record).ok_or(Corruption::Key)?;
                self.issued = self.issued.max(revision);
                Ok(self)
            }
        }
    }

    /// Applies a put or delete to the entries and `last_write`. `None` only
    /// for an over-long key (unreachable for a validated [`Key`]); the index
    /// is then unchanged.
    fn apply(&mut self, record: &Record<'_>) -> Option<()> {
        match *record {
            Record::Put {
                revision,
                key,
                value,
            } => {
                let key = StoredKey::new(key)?;
                self.entries.insert(
                    key,
                    Stored {
                        revision: Revision(revision),
                        value: value.to_vec(),
                    },
                );
                self.last_write = revision;
            }
            Record::Delete { revision, key } => {
                let key = StoredKey::new(key)?;
                self.entries.remove(&key);
                self.last_write = revision;
            }
            Record::Checkpoint { .. } => {}
        }
        Some(())
    }

    /// Records a planned put or delete after the shell made it durable.
    /// Live writes stamp the counter exactly, so a later reservation
    /// checkpoint does not move it. Checkpoints are ignored here.
    pub fn commit(&mut self, record: &Record<'_>) {
        if let Record::Put { revision, .. } | Record::Delete { revision, .. } = *record
            && self.apply(record).is_some()
        {
            self.issued = self.issued.max(revision);
        }
    }

    /// The highest revision that may have been issued (0 if none).
    pub fn issued(&self) -> u64 {
        self.issued
    }

    /// Number of live entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `key`'s value and revision, if present.
    pub fn get(&self, key: Key<'_>) -> Option<(Revision, &[u8])> {
        let key = StoredKey::new(key)?;
        self.entries
            .get(&key)
            .map(|s| (s.revision, s.value.as_slice()))
    }

    fn current(&self, key: Key<'_>) -> Option<Revision> {
        self.get(key).map(|(r, _)| r)
    }

    fn next_revision(&self) -> Result<u64, Refusal> {
        self.issued.checked_add(1).ok_or(Refusal::Full)
    }

    /// The record that writes `value` at `key`, if `expect` holds.
    pub fn plan_put<'a>(
        &self,
        key: Key<'a>,
        value: &'a [u8],
        expect: Expect,
    ) -> Result<Record<'a>, Refusal> {
        if value.len() > MAX_VALUE {
            return Err(Refusal::TooLarge);
        }
        let current = self.current(key);
        if !expect_holds(expect, current) {
            return Err(Refusal::Conflict { current });
        }
        Ok(Record::Put {
            revision: self.next_revision()?,
            key,
            value,
        })
    }

    /// The record that removes `key`, if `expect` holds, and the revision
    /// being removed. `Ok(None)`: the key is absent and `expect` allows
    /// that, so nothing is written.
    pub fn plan_delete<'a>(
        &self,
        key: Key<'a>,
        expect: Expect,
    ) -> Result<Option<(Record<'a>, Revision)>, Refusal> {
        let current = self.current(key);
        if !expect_holds(expect, current) {
            return Err(Refusal::Conflict { current });
        }
        match current {
            None => Ok(None),
            Some(removed) => Ok(Some((
                Record::Delete {
                    revision: self.next_revision()?,
                    key,
                },
                removed,
            ))),
        }
    }

    /// The records of a compacted log: one put per live entry in ascending
    /// revision order (so replay's order check holds), then a checkpoint at
    /// [`Index::issued`] so the counter survives even when the
    /// highest-revision key was deleted. Replaying them yields an index with
    /// the same entries and counter.
    pub fn compacted(&self) -> Vec<Record<'_>> {
        let mut puts: Vec<Record<'_>> = self
            .entries
            .iter()
            .map(|(k, s)| Record::Put {
                revision: s.revision.0,
                key: k.0.as_key(),
                value: s.value.as_slice(),
            })
            .collect();
        puts.sort_by_key(Record::revision);
        if self.issued > 0 {
            puts.push(Record::Checkpoint {
                revision: self.issued,
            });
        }
        puts
    }

    /// The entries matching `filter` with keys strictly after `after`, in
    /// ascending byte order of their keys.
    pub fn select<'s>(
        &'s self,
        filter: KeyFilter<'s>,
        after: Option<Key<'_>>,
    ) -> impl Iterator<Item = (Key<'s>, Revision, &'s [u8])> + 's {
        let base = filter.key();
        // Every key the filter matches starts with its key's bytes, and such
        // keys are contiguous in byte order: start at the filter key (or
        // just after the cursor, whichever is later) and stop at the first
        // key without that prefix.
        let lower = match (after.and_then(StoredKey::new), StoredKey::new(base)) {
            (Some(a), Some(b)) if a >= b => Bound::Excluded(a),
            (_, Some(b)) => Bound::Included(b),
            (Some(a), None) => Bound::Excluded(a),
            (None, None) => Bound::Unbounded,
        };
        self.entries
            .range((lower, Bound::Unbounded))
            .map(|(k, s)| (k.0.as_key(), s.revision, s.value.as_slice()))
            .take_while(move |(k, _, _)| k.as_bytes().starts_with(base.as_bytes()))
            .filter(move |(k, _, _)| filter.matches(*k))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::Filter;

    fn k(s: &'static str) -> Key<'static> {
        Key::from_static(s)
    }

    fn located<'a>(records: &[Record<'a>]) -> Vec<Located<'a>> {
        records
            .iter()
            .enumerate()
            .map(|(i, r)| Located {
                offset: i as u64,
                record: *r,
            })
            .collect()
    }

    fn put(index: &mut Index, key: &'static str, value: &'static [u8]) -> u64 {
        let r = index.plan_put(k(key), value, Expect::Any).unwrap();
        index.commit(&r);
        r.revision()
    }

    #[test]
    fn revisions_start_at_one_and_increase() {
        let mut ix = Index::new();
        assert_eq!(put(&mut ix, "a", b"1"), 1);
        assert_eq!(put(&mut ix, "b", b"2"), 2);
        assert_eq!(put(&mut ix, "a", b"3"), 3);
        let (r, _) = ix.plan_delete(k("b"), Expect::Any).unwrap().unwrap();
        ix.commit(&r);
        assert_eq!(
            r,
            Record::Delete {
                revision: 4,
                key: k("b")
            }
        );
        assert_eq!(ix.get(k("a")), Some((Revision(3), &b"3"[..])));
        assert_eq!(ix.get(k("b")), None);
        assert_eq!(ix.issued(), 4);
    }

    #[test]
    fn plans_check_expect_and_limits_without_changing_state() {
        let mut ix = Index::new();
        put(&mut ix, "a", b"1");
        let before = ix.clone();
        assert_eq!(
            ix.plan_put(k("a"), b"x", Expect::Absent),
            Err(Refusal::Conflict {
                current: Some(Revision(1))
            })
        );
        assert_eq!(
            ix.plan_put(k("b"), b"x", Expect::Exact(Revision(1))),
            Err(Refusal::Conflict { current: None })
        );
        assert_eq!(
            ix.plan_delete(k("a"), Expect::Exact(Revision(2))),
            Err(Refusal::Conflict {
                current: Some(Revision(1))
            })
        );
        assert_eq!(ix.plan_delete(k("zz"), Expect::Any), Ok(None));
        assert_eq!(ix.plan_delete(k("zz"), Expect::Absent), Ok(None));
        let big = vec![0u8; MAX_VALUE + 1];
        assert_eq!(
            ix.plan_put(k("a"), &big, Expect::Any),
            Err(Refusal::TooLarge)
        );
        assert_eq!(ix, before);
    }

    #[test]
    fn exhausted_counter_is_full() {
        let ix = Index::replay(&located(&[Record::Checkpoint { revision: u64::MAX }])).unwrap();
        assert_eq!(ix.plan_put(k("a"), b"", Expect::Any), Err(Refusal::Full));
    }

    #[test]
    fn replay_folds_records_and_checkpoints_raise_the_counter() {
        let ix = Index::replay(&located(&[
            Record::Put {
                revision: 1,
                key: k("a"),
                value: b"x",
            },
            Record::Put {
                revision: 2,
                key: k("b"),
                value: b"y",
            },
            Record::Delete {
                revision: 3,
                key: k("a"),
            },
            Record::Checkpoint { revision: 10 },
            Record::Delete {
                revision: 4,
                key: k("nothing"),
            },
        ]))
        .unwrap();
        assert_eq!(ix.len(), 1);
        assert_eq!(ix.get(k("b")), Some((Revision(2), &b"y"[..])));
        assert_eq!(ix.issued(), 10);
        assert_eq!(
            ix.plan_put(k("c"), b"", Expect::Any).unwrap(),
            Record::Put {
                revision: 11,
                key: k("c"),
                value: b""
            }
        );
    }

    #[test]
    fn replay_rejects_out_of_order_revisions() {
        let err = Index::replay(&located(&[
            Record::Put {
                revision: 2,
                key: k("a"),
                value: b"",
            },
            Record::Put {
                revision: 2,
                key: k("b"),
                value: b"",
            },
        ]));
        assert_eq!(
            err,
            Err(ReplayError {
                offset: 1,
                why: Corruption::RevisionOrder
            })
        );
        let zero = Index::replay(&located(&[Record::Delete {
            revision: 0,
            key: k("a"),
        }]));
        assert!(zero.is_err());
    }

    #[test]
    fn compaction_records_replay_to_the_same_index() {
        let mut ix = Index::new();
        put(&mut ix, "z", b"1");
        put(&mut ix, "a", b"2");
        put(&mut ix, "m", b"3");
        put(&mut ix, "z", b"4");
        put(&mut ix, "top", b"5");
        let (r, _) = ix.plan_delete(k("top"), Expect::Any).unwrap().unwrap();
        ix.commit(&r);
        let recs = ix.compacted();
        assert_eq!(recs.len(), 4);
        assert_eq!(recs.last(), Some(&Record::Checkpoint { revision: 6 }));
        let again = Index::replay(&located(&recs)).unwrap();
        assert_eq!(again.get(k("z")), ix.get(k("z")));
        assert_eq!(again.get(k("a")), ix.get(k("a")));
        assert_eq!(again.get(k("m")), ix.get(k("m")));
        assert_eq!(again.len(), ix.len());
        assert_eq!(again.issued(), 6);
        assert!(Index::new().compacted().is_empty());
    }

    #[test]
    fn select_is_ordered_filtered_and_resumes_after_cursor() {
        let mut ix = Index::new();
        for key in ["app/x", "app", "apple", "app/a/b", "b", "ap", "app/z"] {
            put(&mut ix, key, b"");
        }
        let keys = |f: KeyFilter<'_>, after: Option<Key<'_>>| -> Vec<std::string::String> {
            ix.select(f, after)
                .map(|(k, _, _)| k.as_str().into())
                .collect()
        };
        assert_eq!(
            keys(Filter::Prefix(k("app")), None),
            ["app", "app/a/b", "app/x", "app/z"]
        );
        assert_eq!(
            keys(Filter::Prefix(k("app")), Some(k("app/a/b"))),
            ["app/x", "app/z"]
        );
        assert_eq!(keys(Filter::Prefix(k("app")), Some(k("a"))).len(), 4);
        assert!(keys(Filter::Prefix(k("app")), Some(k("zzz"))).is_empty());
        assert_eq!(keys(Filter::Exact(k("app")), None), ["app"]);
        assert!(keys(Filter::Exact(k("app")), Some(k("app"))).is_empty());
        assert_eq!(keys(Filter::Prefix(k("b")), None), ["b"]);
    }
}
