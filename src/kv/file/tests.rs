//! Tests of [`FileKv`] against real files in fresh temporary directories.

use std::format;
use std::string::String;
use std::sync::atomic::{AtomicU32, Ordering};
use std::vec;
use std::vec::Vec;

use super::*;
use crate::key::Filter;

/// A fresh directory under the system temp dir, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("mw-kv-file-{}-{}-{}", std::process::id(), name, n));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn log(&self) -> PathBuf {
        self.0.join("store.kv")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn k(s: &'static str) -> Key<'static> {
    Key::from_static(s)
}

fn open(path: &Path) -> FileKv {
    FileKv::open(path, Options::new()).unwrap()
}

fn batched(path: &Path) -> FileKv {
    FileKv::open(path, Options::new().durability(Durability::Batched)).unwrap()
}

/// Every entry in key order, paging through the store.
fn dump(kv: &mut FileKv) -> Vec<(String, Vec<u8>, u64)> {
    let mut out: Vec<(String, Vec<u8>, u64)> = Vec::new();
    let mut buf = vec![0u8; MAX_VALUE];
    let mut cursor: Option<String> = None;
    for _ in 0..10_000 {
        let after_owned = cursor.clone();
        let after = after_owned.as_deref().map(|s| Key::new(s).unwrap());
        let mut last = None;
        let scan = kv
            .scan(Filter::Prefix(k("a")), after, &mut buf, |key, e| {
                out.push((key.as_str().into(), e.value.to_vec(), e.revision.0));
                last = Some(String::from(key.as_str()));
                Flow::Continue
            })
            .unwrap();
        // The tests store only keys under "a" plus "b..." keys checked apart.
        if scan.complete {
            break;
        }
        cursor = last;
    }
    let mut buf2 = vec![0u8; MAX_VALUE];
    kv.scan(Filter::Prefix(k("b")), None, &mut buf2, |key, e| {
        out.push((key.as_str().into(), e.value.to_vec(), e.revision.0));
        Flow::Continue
    })
    .unwrap();
    out
}

fn put(kv: &mut FileKv, key: &'static str, value: &[u8]) -> u64 {
    kv.put(k(key), value, Expect::Any).unwrap().0
}

fn get(kv: &mut FileKv, key: &'static str) -> Option<(Vec<u8>, u64)> {
    let mut buf = vec![0u8; MAX_VALUE];
    kv.get(k(key), &mut buf)
        .unwrap()
        .map(|e| (e.value.to_vec(), e.revision.0))
}

// --- Persistence and revisions -------------------------------------------

#[test]
fn entries_persist_across_reopen() {
    let dir = TempDir::new("persist");
    let mut kv = open(&dir.log());
    assert_eq!(put(&mut kv, "a/1", b"one"), 1);
    assert_eq!(put(&mut kv, "a/2", b"two"), 2);
    assert_eq!(put(&mut kv, "a/1", b"uno"), 3);
    assert_eq!(kv.delete(k("a/2"), Expect::Any).unwrap(), Some(Revision(2)));
    put(&mut kv, "b", b"");
    let before = dump(&mut kv);
    drop(kv);

    let mut kv = open(&dir.log());
    assert_eq!(dump(&mut kv), before);
    assert_eq!(get(&mut kv, "a/1"), Some((b"uno".to_vec(), 3)));
    assert_eq!(get(&mut kv, "a/2"), None);
    assert_eq!(kv.len(), 2);
}

#[test]
fn new_log_has_a_header_and_reopens_empty() {
    let dir = TempDir::new("fresh");
    let kv = open(&dir.log());
    assert_eq!(kv.log_len(), log::HEADER_LEN as u64);
    assert!(kv.is_empty());
    drop(kv);
    assert_eq!(fs::read(dir.log()).unwrap(), log::header().to_vec());
    let mut kv = open(&dir.log());
    assert_eq!(put(&mut kv, "a", b"x"), 1);
}

#[test]
fn revision_counter_survives_reopen() {
    let dir = TempDir::new("rev-reopen");
    let mut kv = open(&dir.log());
    put(&mut kv, "a/x", b"1");
    put(&mut kv, "a/y", b"2");
    drop(kv);
    let mut kv = open(&dir.log());
    assert_eq!(put(&mut kv, "a/z", b"3"), 3);
}

#[test]
fn revision_counter_survives_deleting_the_highest_key_and_reopen() {
    let dir = TempDir::new("rev-delete");
    let mut kv = open(&dir.log());
    put(&mut kv, "a/x", b"1");
    put(&mut kv, "a/top", b"2");
    kv.delete(k("a/top"), Expect::Any).unwrap();
    drop(kv);
    let mut kv = open(&dir.log());
    assert_eq!(put(&mut kv, "a/next", b""), 4);
}

#[test]
fn revision_counter_survives_compaction_after_deleting_the_highest_key() {
    let dir = TempDir::new("rev-compact");
    let mut kv = open(&dir.log());
    put(&mut kv, "a/x", b"1");
    put(&mut kv, "a/top", b"2");
    kv.delete(k("a/top"), Expect::Any).unwrap();
    kv.compact().unwrap();
    assert_eq!(put(&mut kv, "a/after", b""), 4);
    kv.delete(k("a/after"), Expect::Any).unwrap();
    kv.compact().unwrap();
    drop(kv);
    let mut kv = open(&dir.log());
    assert_eq!(put(&mut kv, "a/again", b""), 6);
}

#[test]
fn compaction_of_an_emptied_store_keeps_the_counter() {
    let dir = TempDir::new("compact-empty");
    let mut kv = open(&dir.log());
    put(&mut kv, "a", b"1");
    kv.delete(k("a"), Expect::Any).unwrap();
    kv.compact().unwrap();
    drop(kv);
    let mut kv = open(&dir.log());
    assert!(kv.is_empty());
    assert_eq!(put(&mut kv, "a", b""), 3);
}

// --- Conditional writes and errors leave the store unchanged ---------------

#[test]
fn expect_conflicts_write_nothing() {
    let dir = TempDir::new("expect");
    let mut kv = open(&dir.log());
    let r1 = kv.put(k("a"), b"1", Expect::Absent).unwrap();
    let len = kv.log_len();
    assert_eq!(
        kv.put(k("a"), b"2", Expect::Absent),
        Err(KvError::Conflict { current: Some(r1) })
    );
    assert_eq!(
        kv.put(k("a"), b"2", Expect::Exact(Revision(9))),
        Err(KvError::Conflict { current: Some(r1) })
    );
    assert_eq!(
        kv.delete(k("b"), Expect::Exact(Revision(1))),
        Err(KvError::Conflict { current: None })
    );
    assert_eq!(kv.delete(k("b"), Expect::Any), Ok(None));
    assert_eq!(kv.log_len(), len);
    let r2 = kv.put(k("a"), b"2", Expect::Exact(r1)).unwrap();
    assert_eq!(r2, Revision(2));
    assert_eq!(kv.delete(k("a"), Expect::Exact(r2)), Ok(Some(r2)));
}

#[test]
fn limits_and_small_buffers_are_refused() {
    let dir = TempDir::new("limits");
    let mut kv = open(&dir.log());
    assert_eq!(FileKv::LIMITS.max_key, MAX_KEY_LEN);
    assert_eq!(FileKv::LIMITS.max_value, 1 << 20);
    let big = vec![1u8; MAX_VALUE + 1];
    assert_eq!(kv.put(k("a"), &big, Expect::Any), Err(KvError::TooLarge));
    let max = vec![2u8; MAX_VALUE];
    assert_eq!(put(&mut kv, "a", &max), 1);
    let mut small = [0u8; 4];
    assert_eq!(
        kv.get(k("a"), &mut small),
        Err(KvError::BufferTooSmall { needed: MAX_VALUE })
    );
    drop(kv);
    let mut kv = open(&dir.log());
    assert_eq!(get(&mut kv, "a"), Some((max, 1)));
}

// --- Scans --------------------------------------------------------------

#[test]
fn scan_pages_in_key_order_with_page_limit() {
    let dir = TempDir::new("scan");
    let mut kv = batched(&dir.log());
    let total = SCAN_PAGE_LIMIT + 10;
    let names: Vec<String> = (0..total).map(|i| format!("a/{i:04}")).collect();
    for (i, name) in names.iter().enumerate().rev() {
        let v = [i as u8];
        kv.put(Key::new(name).unwrap(), &v, Expect::Any).unwrap();
    }
    let mut buf = [0u8; 8];
    let mut seen: Vec<String> = Vec::new();
    let first = kv
        .scan(Filter::Prefix(k("a")), None, &mut buf, |key, _| {
            seen.push(key.as_str().into());
            Flow::Continue
        })
        .unwrap();
    assert_eq!(
        first,
        Scan {
            visited: SCAN_PAGE_LIMIT,
            complete: false
        }
    );
    let cursor = seen.last().unwrap().clone();
    let second = kv
        .scan(
            Filter::Prefix(k("a")),
            Some(Key::new(&cursor).unwrap()),
            &mut buf,
            |key, _| {
                seen.push(key.as_str().into());
                Flow::Continue
            },
        )
        .unwrap();
    assert_eq!(
        second,
        Scan {
            visited: 10,
            complete: true
        }
    );
    assert_eq!(seen, names);

    let mut n = 0;
    let stopped = kv
        .scan(Filter::Prefix(k("a")), None, &mut buf, |_, _| {
            n += 1;
            if n == 3 { Flow::Stop } else { Flow::Continue }
        })
        .unwrap();
    assert_eq!(
        stopped,
        Scan {
            visited: 3,
            complete: false
        }
    );
}

// --- Crash recovery -------------------------------------------------------

#[test]
fn every_truncation_of_the_last_record_reopens_to_the_prior_state() {
    let dir = TempDir::new("torn");
    let mut kv = open(&dir.log());
    put(&mut kv, "a/1", b"one");
    put(&mut kv, "a/2", b"two");
    kv.delete(k("a/1"), Expect::Any).unwrap();
    let prior = dump(&mut kv);
    let prior_len = kv.log_len();
    put(&mut kv, "a/3", b"the last write, torn by a crash");
    drop(kv);
    let full = fs::read(dir.log()).unwrap();

    for cut in prior_len as usize..full.len() {
        fs::write(dir.log(), &full[..cut]).unwrap();
        let mut kv = open(&dir.log());
        assert_eq!(dump(&mut kv), prior, "cut at {cut}");
        assert_eq!(kv.log_len(), prior_len, "cut at {cut}");
        // The torn tail was truncated on disk, and the store keeps working.
        assert_eq!(fs::metadata(dir.log()).unwrap().len(), prior_len);
        assert_eq!(put(&mut kv, "a/3", b"again"), 4, "cut at {cut}");
        drop(kv);
        let mut kv = open(&dir.log());
        assert_eq!(get(&mut kv, "a/3"), Some((b"again".to_vec(), 4)));
    }
}

#[test]
fn torn_header_of_a_new_log_reopens_empty() {
    let dir = TempDir::new("torn-header");
    for n in 0..log::HEADER_LEN {
        fs::write(dir.log(), &log::header()[..n]).unwrap();
        let mut kv = open(&dir.log());
        assert!(kv.is_empty());
        assert_eq!(put(&mut kv, "a", b"x"), 1);
        drop(kv);
        assert_eq!(get(&mut open(&dir.log()), "a"), Some((b"x".to_vec(), 1)));
    }
}

#[test]
fn crc_corruption_in_the_middle_is_an_error() {
    let dir = TempDir::new("corrupt");
    let mut kv = open(&dir.log());
    put(&mut kv, "a/1", b"one");
    let second = kv.log_len();
    put(&mut kv, "a/2", b"two");
    put(&mut kv, "a/3", b"three");
    drop(kv);
    let mut bytes = fs::read(dir.log()).unwrap();
    let good = bytes.clone();
    // A value byte of the second record: its CRC fails, the third is intact.
    let at = second as usize + 8 + 11 + 3;
    bytes[at] ^= 0x40;
    fs::write(dir.log(), &bytes).unwrap();
    assert_eq!(
        FileKv::open(dir.log(), Options::new()).unwrap_err(),
        FileKvError::Corrupt {
            offset: second,
            why: Corruption::Checksum
        }
    );
    // The failed open changed nothing on disk.
    assert_eq!(fs::read(dir.log()).unwrap(), bytes);

    // Explicit recovery keeps the prefix before the damage.
    let mut kv = FileKv::open(
        dir.log(),
        Options::new().recovery(Recovery::TruncateAtFirstBad),
    )
    .unwrap();
    assert_eq!(get(&mut kv, "a/1"), Some((b"one".to_vec(), 1)));
    assert_eq!(get(&mut kv, "a/2"), None);
    assert_eq!(get(&mut kv, "a/3"), None);
    assert_eq!(kv.log_len(), second);
    drop(kv);
    assert_eq!(
        fs::read(dir.log()).unwrap(),
        good[..second as usize].to_vec()
    );
}

#[test]
fn foreign_file_is_not_a_log() {
    let dir = TempDir::new("foreign");
    fs::write(dir.log(), b"#!/bin/sh\necho hi\n").unwrap();
    assert_eq!(
        FileKv::open(dir.log(), Options::new()).unwrap_err(),
        FileKvError::NotALog
    );
}

// --- Compaction -----------------------------------------------------------

#[test]
fn compaction_shrinks_the_log_and_preserves_entries_and_revisions() {
    let dir = TempDir::new("compact");
    let mut kv = open(&dir.log());
    for round in 0..20u8 {
        put(&mut kv, "a/x", &[round; 100]);
        put(&mut kv, "a/y", &[round; 50]);
        put(&mut kv, "a/gone", b"tmp");
        kv.delete(k("a/gone"), Expect::Any).unwrap();
    }
    let before = dump(&mut kv);
    let len_before = kv.log_len();
    kv.compact().unwrap();
    assert!(
        kv.log_len() < len_before / 10,
        "{} vs {len_before}",
        kv.log_len()
    );
    assert_eq!(fs::metadata(dir.log()).unwrap().len(), kv.log_len());
    assert!(!dir.0.join("store.kv.compact").exists());
    assert_eq!(dump(&mut kv), before);

    // Writes continue on the new log and everything survives reopen.
    assert_eq!(put(&mut kv, "a/z", b"z"), 81);
    drop(kv);
    let mut kv = open(&dir.log());
    let mut expected = before.clone();
    expected.insert(2, ("a/z".into(), b"z".to_vec(), 81));
    assert_eq!(dump(&mut kv), expected);
}

#[test]
fn stale_compaction_file_is_replaced() {
    let dir = TempDir::new("stale");
    fs::write(dir.0.join("store.kv.compact"), b"left by a crash").unwrap();
    let mut kv = open(&dir.log());
    put(&mut kv, "a", b"1");
    kv.compact().unwrap();
    drop(kv);
    assert_eq!(get(&mut open(&dir.log()), "a"), Some((b"1".to_vec(), 1)));
}

// --- Durability modes -------------------------------------------------------

#[test]
fn batched_mode_flushes_and_persists() {
    let dir = TempDir::new("batched");
    let mut kv = batched(&dir.log());
    for i in 0..10u8 {
        assert_eq!(put(&mut kv, "a/k", &[i]), u64::from(i) + 1);
    }
    kv.flush().unwrap();
    let before = dump(&mut kv);
    drop(kv);
    let mut kv = batched(&dir.log());
    assert_eq!(dump(&mut kv), before);
    // Even a clean reopen resumes after the reserved block.
    assert_eq!(put(&mut kv, "a/k", b""), RESERVE_BLOCK + 1);
}

#[test]
fn batched_mode_reserves_revisions_so_lost_writes_are_not_reissued() {
    let dir = TempDir::new("reserve");
    let mut kv = batched(&dir.log());
    put(&mut kv, "a/1", b"durable");
    kv.flush().unwrap();
    let synced = kv.log_len();
    put(&mut kv, "a/2", b"lost");
    put(&mut kv, "a/3", b"lost");
    drop(kv);
    // Simulate power loss: everything after the flush is gone.
    let bytes = fs::read(dir.log()).unwrap();
    fs::write(dir.log(), &bytes[..synced as usize]).unwrap();

    let mut kv = batched(&dir.log());
    assert_eq!(get(&mut kv, "a/2"), None);
    // Revisions 2 and 3 were acknowledged before the loss; the counter
    // resumes past the whole reserved block instead.
    assert_eq!(put(&mut kv, "a/4", b""), RESERVE_BLOCK + 1);
}

#[test]
fn sync_mode_writes_no_reservations() {
    let dir = TempDir::new("sync-no-reserve");
    let mut kv = open(&dir.log());
    put(&mut kv, "a", b"x");
    let one = kv.log_len() - log::HEADER_LEN as u64;
    put(&mut kv, "b", b"y");
    assert_eq!(kv.log_len() - log::HEADER_LEN as u64, 2 * one);
    drop(kv);
    assert_eq!(put(&mut open(&dir.log()), "a", b""), 3);
}
