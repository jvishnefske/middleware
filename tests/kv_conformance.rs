//! Conformance cases for the key-value contract (`middleware::kv`).
//!
//! Generic async cases over [`KvStore`], written once and run against
//! every store. A store plugs in by implementing [`KvFixture`] and calling
//! every case from its own test file, usually through [`kv_cases!`]:
//!
//! ```ignore
//! mod kv_conformance;
//! use kv_conformance::KvFixture;
//!
//! struct Files { dir: TempDir }
//! impl KvFixture for Files {
//!     type Store = Blocking<FileKv>;
//!     fn fresh(&mut self) -> Self::Store { ... }        // empty store
//!     fn capacity(&self) -> Option<usize> { None }     // never Full
//!     fn reopen(&mut self, s: Self::Store) -> Option<Self::Store> { ... } // durable
//! }
//!
//! kv_cases!(file, Files::new()); // tests file::revisions_start_at_one_and_increase, ...
//! ```
//!
//! `mod kv_conformance;` from a test crate root resolves to this file.
//! `tests/kv.rs` is the reference: every case against
//! `Blocking<KvTable>` and against `RemoteKv` talking to a `KvHandler`
//! over the in-process bus.
//!
//! # What the cases assume
//!
//! * Keys up to 12 bytes and values up to 16 bytes fit (the limit cases
//!   use [`KvFixture::limits`] instead).
//! * The store numbers its writes consecutively: the first write is
//!   revision 1 and each successful write (put, or a delete that removed
//!   an entry) takes the next number. The contract only requires strictly
//!   increasing revisions; consecutive numbering is what lets a case see
//!   that a failed or empty write consumed none. A store that skips numbers
//!   documents why in its runner and leaves those cases out.
//! * A delete returns the revision the removed entry had (the put that
//!   last set it), and consumes a revision of its own.
//!
//! Cases panic on failure, like any test.
#![allow(dead_code, async_fn_in_trait)]

use std::collections::BTreeSet;

use middleware::key::{Key, KeyFilter, MAX_KEY_LEN};
use middleware::kv::{Expect, Flow, KvError, KvLimits, KvStore, Revision, Scan};

/// How a store under test is made.
pub trait KvFixture {
    /// The store under test.
    type Store: KvStore;

    /// A new, empty store whose first write will be revision 1.
    fn fresh(&mut self) -> Self::Store;

    /// The limits the store enforces end to end. Defaults to
    /// `Store::LIMITS`; a client of a remote store returns the served
    /// store's limits, so the cases check that its `TooLarge` comes back.
    fn limits(&self) -> KvLimits {
        <Self::Store as KvStore>::LIMITS
    }

    /// Entries the store holds before a new key is `Full`, if a case can
    /// reach that; `None` skips the `Full` case.
    fn capacity(&self) -> Option<usize> {
        None
    }

    /// For a durable store: closes `store` and opens it again from what it
    /// persisted. `None` (the default) skips the durability case.
    fn reopen(&mut self, store: Self::Store) -> Option<Self::Store> {
        drop(store);
        None
    }
}

/// Generates `mod $module` with one `#[tokio::test]` per case, each on a
/// new fixture from `$fixture` (evaluated inside the module, which sees
/// the invoking scope through `use super::*`). The runner must declare
/// `mod kv_conformance;` at its crate root.
#[macro_export]
macro_rules! kv_cases {
    ($module:ident, $fixture:expr) => {
        $crate::kv_cases!(@cases $module, $fixture,
            revisions_start_at_one_and_increase,
            get_reads_value_and_revision,
            expect_semantics_and_conflict_revision,
            errors_leave_the_store_unchanged,
            buffer_too_small_reports_needed_size,
            too_large_is_refused,
            full_at_capacity,
            delete_of_absent_key_writes_nothing,
            scan_visits_keys_in_byte_order,
            scan_prefix_filter_is_segment_wise,
            scan_after_is_exclusive,
            scan_stops_on_flow_stop,
            scan_paging_visits_every_entry_once,
            scan_buffer_too_small_reports_needed_size,
            revisions_survive_reopen,
        );
    };
    (@cases $module:ident, $fixture:expr, $($case:ident),+ $(,)?) => {
        mod $module {
            #[allow(unused_imports)]
            use super::*;
            $(
                #[tokio::test]
                async fn $case() {
                    $crate::kv_conformance::$case(&mut $fixture).await;
                }
            )+
        }
    };
}

fn k(s: &str) -> Key<'_> {
    Key::new(s).expect("valid test key")
}

/// Reads `key` as `(value, revision)`.
async fn read<S: KvStore>(s: &mut S, key: &str) -> Option<(Vec<u8>, u64)> {
    let mut buf = vec![0u8; 4096];
    s.get(k(key), &mut buf)
        .await
        .expect("get")
        .map(|e| (e.value.to_vec(), e.revision.0))
}

/// Every entry under `prefix` as `(key, value, revision)`, in scan order,
/// and how the scan ended.
async fn scan_all<S: KvStore>(
    s: &mut S,
    filter: KeyFilter<'_>,
    after: Option<Key<'_>>,
) -> (Vec<(String, Vec<u8>, u64)>, Scan) {
    let mut seen = Vec::new();
    let mut buf = [0u8; 64];
    let scan = s
        .scan(filter, after, &mut buf, |key, e| {
            seen.push((key.as_str().to_owned(), e.value.to_vec(), e.revision.0));
            Flow::Continue
        })
        .await
        .expect("scan");
    (seen, scan)
}

fn keys(seen: &[(String, Vec<u8>, u64)]) -> Vec<&str> {
    seen.iter().map(|(k, _, _)| k.as_str()).collect()
}

async fn put<S: KvStore>(s: &mut S, key: &str, value: &[u8]) -> u64 {
    s.put(k(key), value, Expect::Any).await.expect("put").0
}

/// The first write is revision 1; puts (new and overwriting) and deletes
/// each take the next revision; a delete returns the removed entry's
/// revision; an entry's revision is the write that last set it.
pub async fn revisions_start_at_one_and_increase<F: KvFixture>(f: &mut F) {
    let mut s = f.fresh();
    assert_eq!(put(&mut s, "a", b"1").await, 1);
    assert_eq!(put(&mut s, "b", b"2").await, 2);
    assert_eq!(put(&mut s, "a", b"3").await, 3);
    assert_eq!(read(&mut s, "a").await, Some((b"3".to_vec(), 3)));
    assert_eq!(read(&mut s, "b").await, Some((b"2".to_vec(), 2)));
    let removed = s.delete(k("b"), Expect::Any).await.expect("delete");
    assert_eq!(
        removed,
        Some(Revision(2)),
        "delete returns the removed revision"
    );
    // The delete took revision 4.
    assert_eq!(put(&mut s, "c", b"").await, 5);
    assert_eq!(put(&mut s, "b", b"again").await, 6);
    assert_eq!(read(&mut s, "b").await, Some((b"again".to_vec(), 6)));
}

/// A get copies the value into the caller's buffer with its revision; an
/// absent key is `Ok(None)`; empty values are values.
pub async fn get_reads_value_and_revision<F: KvFixture>(f: &mut F) {
    let mut s = f.fresh();
    assert_eq!(read(&mut s, "x").await, None);
    put(&mut s, "x", b"hello").await;
    put(&mut s, "e", b"").await;
    let mut buf = [0u8; 8];
    let e = s
        .get(k("x"), &mut buf)
        .await
        .expect("get")
        .expect("present");
    assert_eq!((e.value, e.revision), (&b"hello"[..], Revision(1)));
    assert_eq!(read(&mut s, "e").await, Some((Vec::new(), 2)));
    assert_eq!(read(&mut s, "x/sub").await, None);
}

/// `Any` always writes; `Absent` only for an absent key; `Exact(r)` only
/// at revision `r`. A mismatch is `Conflict` with the current revision
/// (`None` if absent), for puts and deletes alike.
pub async fn expect_semantics_and_conflict_revision<F: KvFixture>(f: &mut F) {
    let mut s = f.fresh();
    let conflict = |current: Option<u64>| move |e: &KvError<_>| matches!(e, KvError::Conflict { current: c } if *c == current.map(Revision));
    assert_eq!(
        s.put(k("a"), b"1", Expect::Absent)
            .await
            .expect("absent ok"),
        Revision(1)
    );
    let e = s
        .put(k("a"), b"x", Expect::Absent)
        .await
        .expect_err("present");
    assert!(conflict(Some(1))(&e), "{e:?}");
    let e = s
        .put(k("b"), b"x", Expect::Exact(Revision(1)))
        .await
        .expect_err("absent");
    assert!(conflict(None)(&e), "{e:?}");
    let e = s
        .put(k("a"), b"x", Expect::Exact(Revision(9)))
        .await
        .expect_err("stale");
    assert!(conflict(Some(1))(&e), "{e:?}");
    let e = s
        .put(k("a"), b"x", Expect::Exact(Revision(0)))
        .await
        .expect_err("rev 0");
    assert!(conflict(Some(1))(&e), "{e:?}");
    assert_eq!(
        s.put(k("a"), b"2", Expect::Exact(Revision(1)))
            .await
            .expect("cas"),
        Revision(2)
    );
    assert_eq!(
        s.put(k("a"), b"3", Expect::Any).await.expect("any"),
        Revision(3)
    );
    // Deletes.
    let e = s
        .delete(k("a"), Expect::Exact(Revision(2)))
        .await
        .expect_err("stale");
    assert!(conflict(Some(3))(&e), "{e:?}");
    let e = s.delete(k("a"), Expect::Absent).await.expect_err("present");
    assert!(conflict(Some(3))(&e), "{e:?}");
    let e = s
        .delete(k("b"), Expect::Exact(Revision(3)))
        .await
        .expect_err("absent");
    assert!(conflict(None)(&e), "{e:?}");
    assert_eq!(
        s.delete(k("a"), Expect::Exact(Revision(3)))
            .await
            .expect("cas delete"),
        Some(Revision(3))
    );
    assert_eq!(read(&mut s, "a").await, None);
}

/// No error changes the store: after a conflict, a too-large write and a
/// too-small read, every entry reads back as before and the next write
/// takes the next revision (none was consumed).
pub async fn errors_leave_the_store_unchanged<F: KvFixture>(f: &mut F) {
    let limits = f.limits();
    let mut s = f.fresh();
    put(&mut s, "a", b"keep").await;
    put(&mut s, "b", b"also").await;
    let _ = s
        .put(k("a"), b"x", Expect::Absent)
        .await
        .expect_err("conflict");
    let _ = s
        .put(k("c"), b"x", Expect::Exact(Revision(1)))
        .await
        .expect_err("conflict");
    let _ = s
        .delete(k("b"), Expect::Exact(Revision(1)))
        .await
        .expect_err("conflict");
    let big = vec![0u8; limits.max_value + 1];
    let _ = s
        .put(k("a"), &big, Expect::Any)
        .await
        .expect_err("too large");
    let mut tiny = [0u8; 1];
    let _ = s
        .get(k("a"), &mut tiny)
        .await
        .expect_err("buffer too small");
    assert_eq!(read(&mut s, "a").await, Some((b"keep".to_vec(), 1)));
    assert_eq!(read(&mut s, "b").await, Some((b"also".to_vec(), 2)));
    assert_eq!(read(&mut s, "c").await, None);
    assert_eq!(
        put(&mut s, "d", b"").await,
        3,
        "an error consumed a revision"
    );
}

/// A value longer than the caller's buffer is `BufferTooSmall` with the
/// value's length; a buffer of exactly that length then reads it.
pub async fn buffer_too_small_reports_needed_size<F: KvFixture>(f: &mut F) {
    let mut s = f.fresh();
    put(&mut s, "v", b"0123456789").await;
    let mut buf = [0u8; 10];
    let e = s.get(k("v"), &mut buf[..9]).await.expect_err("too small");
    assert!(matches!(e, KvError::BufferTooSmall { needed: 10 }), "{e:?}");
    let e = s.get(k("v"), &mut []).await.expect_err("empty buffer");
    assert!(matches!(e, KvError::BufferTooSmall { needed: 10 }), "{e:?}");
    let got = s
        .get(k("v"), &mut buf)
        .await
        .expect("fits")
        .expect("present");
    assert_eq!((got.value, got.revision), (&b"0123456789"[..], Revision(1)));
}

/// A value of `limits().max_value` bytes is stored; one byte more is
/// `TooLarge`; a key over `limits().max_key` (when that is below
/// `MAX_KEY_LEN`) is `TooLarge`. Neither consumes a revision.
pub async fn too_large_is_refused<F: KvFixture>(f: &mut F) {
    let limits = f.limits();
    let mut s = f.fresh();
    let max = vec![0xA5u8; limits.max_value];
    assert_eq!(
        s.put(k("m"), &max, Expect::Any)
            .await
            .expect("at the limit"),
        Revision(1)
    );
    let mut buf = vec![0u8; limits.max_value];
    let e = s
        .get(k("m"), &mut buf)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(e.value, &max[..]);
    let over = vec![0u8; limits.max_value + 1];
    let e = s
        .put(k("m"), &over, Expect::Any)
        .await
        .expect_err("over the limit");
    assert!(matches!(e, KvError::TooLarge), "{e:?}");
    let e = s
        .put(k("n"), &over, Expect::Any)
        .await
        .expect_err("over the limit");
    assert!(matches!(e, KvError::TooLarge), "{e:?}");
    if limits.max_key < MAX_KEY_LEN {
        let at = "k".repeat(limits.max_key);
        let long = "k".repeat(limits.max_key + 1);
        assert_eq!(put(&mut s, &at, b"").await, 2);
        let e = s
            .put(k(&long), b"", Expect::Any)
            .await
            .expect_err("long key");
        assert!(matches!(e, KvError::TooLarge), "{e:?}");
    }
    let next = s.put(k("z"), b"", Expect::Any).await.expect("put").0;
    let expected = if limits.max_key < MAX_KEY_LEN { 3 } else { 2 };
    assert_eq!(next, expected, "a refused write consumed a revision");
    assert_eq!(read(&mut s, "n").await, None);
}

/// At `capacity()` entries a new key is `Full` (store unchanged, no
/// revision consumed), while overwriting an existing key still works and
/// a delete makes room. Skipped when the fixture has no capacity.
pub async fn full_at_capacity<F: KvFixture>(f: &mut F) {
    let Some(capacity) = f.capacity() else {
        return;
    };
    let mut s = f.fresh();
    for i in 0..capacity {
        put(&mut s, &format!("f/{i:04}"), b"v").await;
    }
    let rev = capacity as u64;
    let e = s.put(k("new"), b"v", Expect::Any).await.expect_err("full");
    assert!(matches!(e, KvError::Full), "{e:?}");
    let e = s
        .put(k("new"), b"v", Expect::Absent)
        .await
        .expect_err("full");
    assert!(matches!(e, KvError::Full), "{e:?}");
    assert_eq!(read(&mut s, "new").await, None);
    assert_eq!(put(&mut s, "f/0000", b"w").await, rev + 1);
    let removed = s.delete(k("f/0001"), Expect::Any).await.expect("delete");
    assert_eq!(removed, Some(Revision(2)));
    assert_eq!(put(&mut s, "new", b"v").await, rev + 3);
    let (seen, _) = scan_all(&mut s, KeyFilter::Prefix(k("f")), None).await;
    assert_eq!(seen.len(), capacity - 1);
}

/// Deleting an absent key under `Any` (or `Absent`) is `Ok(None)` and
/// writes nothing: the next write takes the next revision.
pub async fn delete_of_absent_key_writes_nothing<F: KvFixture>(f: &mut F) {
    let mut s = f.fresh();
    assert_eq!(
        s.delete(k("nope"), Expect::Any).await.expect("delete"),
        None
    );
    assert_eq!(put(&mut s, "a", b"").await, 1);
    assert_eq!(
        s.delete(k("nope"), Expect::Any).await.expect("delete"),
        None
    );
    assert_eq!(
        s.delete(k("nope"), Expect::Absent).await.expect("delete"),
        None
    );
    s.delete(k("a"), Expect::Any).await.expect("delete");
    assert_eq!(s.delete(k("a"), Expect::Any).await.expect("again"), None);
    assert_eq!(put(&mut s, "b", b"").await, 3);
}

/// A scan visits entries in ascending byte order of their keys, with each
/// value and revision, and reports `complete`.
pub async fn scan_visits_keys_in_byte_order<F: KvFixture>(f: &mut F) {
    let mut s = f.fresh();
    // Inserted out of order; '-' < '/' < '0' < 'B' < 'a' in ASCII.
    let inserted = ["s/b", "s/a0", "s", "s/a/b", "s/B", "s/a-b", "s/a"];
    for (i, key) in inserted.iter().enumerate() {
        put(&mut s, key, &[i as u8]).await;
    }
    put(&mut s, "t", b"other").await;
    let (seen, scan) = scan_all(&mut s, KeyFilter::Prefix(k("s")), None).await;
    let mut sorted = inserted.to_vec();
    sorted.sort_unstable();
    assert_eq!(keys(&seen), sorted);
    assert_eq!(
        keys(&seen),
        ["s", "s/B", "s/a", "s/a-b", "s/a/b", "s/a0", "s/b"]
    );
    for (key, value, revision) in &seen {
        let i = inserted.iter().position(|x| x == key).expect("inserted");
        assert_eq!(
            (value.as_slice(), *revision),
            (&[i as u8][..], i as u64 + 1),
            "{key}"
        );
    }
    assert_eq!(
        scan,
        Scan {
            visited: inserted.len(),
            complete: true
        }
    );
}

/// `Prefix(p)` selects `p` and keys below it segment-wise (not `p-x` or
/// `px`); `Exact(k)` selects only `k`; no match is an empty, complete scan.
pub async fn scan_prefix_filter_is_segment_wise<F: KvFixture>(f: &mut F) {
    let mut s = f.fresh();
    for key in ["ap", "app", "app-x", "app/x", "app/x/y", "apple", "b/app"] {
        put(&mut s, key, b"").await;
    }
    let (seen, scan) = scan_all(&mut s, KeyFilter::Prefix(k("app")), None).await;
    assert_eq!(keys(&seen), ["app", "app/x", "app/x/y"]);
    assert!(scan.complete);
    let (seen, _) = scan_all(&mut s, KeyFilter::Prefix(k("app/x")), None).await;
    assert_eq!(keys(&seen), ["app/x", "app/x/y"]);
    let (seen, _) = scan_all(&mut s, KeyFilter::Exact(k("app/x")), None).await;
    assert_eq!(keys(&seen), ["app/x"]);
    let (seen, scan) = scan_all(&mut s, KeyFilter::Exact(k("app/q")), None).await;
    assert!(seen.is_empty());
    assert_eq!(
        scan,
        Scan {
            visited: 0,
            complete: true
        }
    );
    let (seen, scan) = scan_all(&mut s, KeyFilter::Prefix(k("zz")), None).await;
    assert_eq!((seen.len(), scan.complete), (0, true));
}

/// The `after` cursor is exclusive, need not be a stored key, may sort
/// before the filter's range, and past the end gives an empty, complete
/// scan.
pub async fn scan_after_is_exclusive<F: KvFixture>(f: &mut F) {
    let mut s = f.fresh();
    for key in ["p", "p/a", "p/b", "p/c", "q"] {
        put(&mut s, key, b"").await;
    }
    let pf = KeyFilter::Prefix(k("p"));
    let (seen, _) = scan_all(&mut s, pf, Some(k("p/a"))).await;
    assert_eq!(keys(&seen), ["p/b", "p/c"]);
    let (seen, _) = scan_all(&mut s, pf, Some(k("p"))).await;
    assert_eq!(keys(&seen), ["p/a", "p/b", "p/c"]);
    let (seen, _) = scan_all(&mut s, pf, Some(k("p/a0"))).await;
    assert_eq!(keys(&seen), ["p/b", "p/c"], "cursor between stored keys");
    let (seen, _) = scan_all(&mut s, pf, Some(k("a"))).await;
    assert_eq!(
        keys(&seen),
        ["p", "p/a", "p/b", "p/c"],
        "cursor before the range"
    );
    let (seen, scan) = scan_all(&mut s, pf, Some(k("p/c"))).await;
    assert_eq!((seen.len(), scan.complete), (0, true));
    let (seen, _) = scan_all(&mut s, KeyFilter::Exact(k("p/b")), Some(k("p/b"))).await;
    assert!(seen.is_empty());
}

/// `Flow::Stop` ends the scan after that entry: `visited` counts the
/// entries passed to the callback and `complete` is false when matching
/// entries remain.
pub async fn scan_stops_on_flow_stop<F: KvFixture>(f: &mut F) {
    let mut s = f.fresh();
    for i in 0..5 {
        put(&mut s, &format!("r/{i}"), b"").await;
    }
    let mut calls = 0;
    let mut buf = [0u8; 8];
    let scan = s
        .scan(KeyFilter::Prefix(k("r")), None, &mut buf, |_, _| {
            calls += 1;
            if calls == 2 {
                Flow::Stop
            } else {
                Flow::Continue
            }
        })
        .await
        .expect("scan");
    assert_eq!(calls, 2);
    assert_eq!(
        scan,
        Scan {
            visited: 2,
            complete: false
        }
    );
}

/// Paging: a caller that stops every few entries and resumes after the
/// last key it saw visits every matching entry exactly once, in order,
/// even across the store's own page boundaries.
pub async fn scan_paging_visits_every_entry_once<F: KvFixture>(f: &mut F) {
    let mut s = f.fresh();
    let total = 40;
    let mut expected = Vec::new();
    for i in 0..total {
        let key = format!("pg/{:03}", (i * 17) % total);
        put(&mut s, &key, key.as_bytes()).await;
        expected.push(key);
    }
    put(&mut s, "pg0", b"outside").await;
    expected.sort();
    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut buf = [0u8; 16];
    // Each round visits at least one entry, so `total + 1` rounds bound it.
    for _ in 0..=total {
        let mut in_round = 0;
        let after = cursor.as_deref().map(k);
        let scan = s
            .scan(KeyFilter::Prefix(k("pg")), after, &mut buf, |key, e| {
                assert_eq!(e.value, key.as_bytes());
                seen.push(key.as_str().to_owned());
                in_round += 1;
                if in_round == 7 {
                    Flow::Stop
                } else {
                    Flow::Continue
                }
            })
            .await
            .expect("scan");
        assert_eq!(scan.visited, in_round);
        cursor = seen.last().cloned();
        if scan.complete {
            break;
        }
        assert!(in_round > 0, "an incomplete scan visited nothing");
    }
    assert_eq!(seen, expected);
    let distinct: BTreeSet<_> = seen.iter().collect();
    assert_eq!(distinct.len(), total);
}

/// A scan reaching a value longer than the caller's buffer is
/// `BufferTooSmall` with the value's length; the entries before it were
/// visited, and resuming after them with a larger buffer continues.
pub async fn scan_buffer_too_small_reports_needed_size<F: KvFixture>(f: &mut F) {
    let mut s = f.fresh();
    put(&mut s, "w/a", b"1").await;
    put(&mut s, "w/b", b"123456").await;
    put(&mut s, "w/c", b"2").await;
    let mut small = [0u8; 4];
    let mut seen = Vec::new();
    let e = s
        .scan(KeyFilter::Prefix(k("w")), None, &mut small, |key, _| {
            seen.push(key.as_str().to_owned());
            Flow::Continue
        })
        .await
        .expect_err("value too long");
    assert!(matches!(e, KvError::BufferTooSmall { needed: 6 }), "{e:?}");
    assert_eq!(seen, ["w/a"]);
    let (rest, scan) = scan_all(&mut s, KeyFilter::Prefix(k("w")), Some(k("w/a"))).await;
    assert_eq!(keys(&rest), ["w/b", "w/c"]);
    assert!(scan.complete);
}

/// Durable stores only: after a reopen every entry reads back with its
/// revision, deleted keys stay deleted, and new writes continue the
/// counter (revisions are never reused across a restart).
pub async fn revisions_survive_reopen<F: KvFixture>(f: &mut F) {
    let mut s = f.fresh();
    put(&mut s, "d/a", b"1").await;
    put(&mut s, "d/b", b"2").await;
    put(&mut s, "d/a", b"3").await;
    s.delete(k("d/b"), Expect::Any).await.expect("delete");
    let Some(mut s) = f.reopen(s) else {
        return;
    };
    assert_eq!(read(&mut s, "d/a").await, Some((b"3".to_vec(), 3)));
    assert_eq!(read(&mut s, "d/b").await, None);
    assert_eq!(
        put(&mut s, "d/c", b"").await,
        5,
        "revision reused or skipped after reopen"
    );
    let (seen, _) = scan_all(&mut s, KeyFilter::Prefix(k("d")), None).await;
    assert_eq!(keys(&seen), ["d/a", "d/c"]);
}
