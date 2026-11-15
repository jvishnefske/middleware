//! The shared key-value conformance cases (`tests/kv_conformance.rs`)
//! against [`FileKv`] behind [`Blocking`], each store in a fresh log file
//! under a unique temporary directory.
//!
//! * `file_sync`: [`Durability::Sync`], every case including
//!   `revisions_survive_reopen` (reopen drops the store and opens the same
//!   log again).
//! * `file_batched`: [`Durability::Batched`], every case except the reopen
//!   one: a batched log reserves revisions in blocks, so a reopen resumes
//!   after the reserved block instead of at the next number, which that
//!   case (consecutive numbering) does not allow. The batched reopen rules
//!   are tested in the crate's `kv::file` unit tests.
//!
//! `FileKv` is never `Full` (only an exhausted `u64` counter is), so the
//! capacity case is skipped.
#![cfg(feature = "kv-file")]

mod kv_conformance;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use kv_conformance::KvFixture;
use middleware::kv::Blocking;
use middleware::kv::file::{Durability, FileKv, Options};

/// Creates each store in its own new directory; removes them all on drop.
struct Files {
    options: Options,
    dirs: Vec<PathBuf>,
}

impl Files {
    fn new(durability: Durability) -> Self {
        Files {
            options: Options::new().durability(durability),
            dirs: Vec::new(),
        }
    }
}

impl Drop for Files {
    fn drop(&mut self) {
        for dir in &self.dirs {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

impl KvFixture for Files {
    type Store = Blocking<FileKv>;

    fn fresh(&mut self) -> Blocking<FileKv> {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("mw-kv-file-conformance-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let log = dir.join("store.kv");
        self.dirs.push(dir);
        Blocking(FileKv::open(log, self.options).expect("open fresh log"))
    }

    fn reopen(&mut self, store: Blocking<FileKv>) -> Option<Blocking<FileKv>> {
        let path = store.0.path().to_path_buf();
        drop(store);
        Some(Blocking(
            FileKv::open(path, self.options).expect("reopen log"),
        ))
    }
}

kv_cases!(file_sync, Files::new(Durability::Sync));

kv_cases!(@cases file_batched, Files::new(Durability::Batched),
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
);
