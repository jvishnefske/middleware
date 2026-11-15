//! A durable key-value store on the local filesystem (feature `kv-file`,
//! `std`, host only).
//!
//! [`FileKv`] keeps one append-only log file. Every put, delete and
//! revision checkpoint is appended as a length-prefixed, CRC-32 checked
//! record (the byte layout and the replay rules are in [`log`]). Opening
//! replays the log into an in-memory index of every live entry (key,
//! revision, value; see the private `index` submodule), so reads never touch the disk and
//! resident memory is about the size of the live data. [`FileKv::compact`]
//! rewrites the log to just the live entries.
//!
//! The pure parts, record encoding, log parsing with torn-tail detection,
//! replay, and write planning, live in the [`log`] and `index`
//! submodules and are tested without I/O. This module is the thin shell
//! that reads, appends, syncs, truncates and renames.
//!
//! # Limits
//!
//! Keys up to [`MAX_KEY_LEN`] bytes (any valid [`Key`]), values up to
//! [`MAX_VALUE`] (1 MiB). A scan visits at most [`SCAN_PAGE_LIMIT`] entries
//! per call. [`KvError::Full`] is returned only if the revision counter
//! reaches `u64::MAX`; the number of entries is bounded by memory and disk.
//!
//! # Revisions
//!
//! Revisions start at 1 and strictly increase. They are never reused, also
//! across reopen and compaction: the counter is recovered from the highest
//! revision in the log, and compaction writes a checkpoint record carrying
//! it, so deleting the highest-revision key and compacting does not lower
//! it. In [`Durability::Batched`] mode revisions are additionally reserved
//! in blocks of [`RESERVE_BLOCK`] by a synced checkpoint before any of them
//! is issued, so writes lost to power loss can never have their revisions
//! handed out again. The log cannot tell a clean close from power loss,
//! so every reopen of a log written in batched mode (until it is
//! compacted) resumes after the last reserved block, leaving a gap:
//! revisions are increasing, not dense. [`Durability::Sync`] numbers
//! writes consecutively, also across reopen and compaction.
//!
//! # Durability
//!
//! * [`Durability::Sync`] (the default): a put or delete returns `Ok` only
//!   after its record is written and `sync_data` has returned, so every
//!   acknowledged write survives power loss. A crash during a write leaves
//!   at most a torn final record of a write that was never acknowledged; the
//!   next [`FileKv::open`] truncates it away and the store is exactly the
//!   acknowledged state.
//! * [`Durability::Batched`]: each record is handed to the OS before the
//!   write returns, so it survives a crash of the process, but it reaches
//!   stable storage only on [`FileKv::flush`], [`FileKv::compact`], or the
//!   synced reservation checkpoint that starts each block of
//!   [`RESERVE_BLOCK`] revisions. Power loss can lose any writes after the
//!   last such sync. Usually that leaves a torn tail that open discards;
//!   if the OS persisted later pages before earlier ones, open finds damage
//!   followed by intact records and reports [`FileKvError::Corrupt`] under
//!   the default [`Recovery::Strict`]. [`Recovery::TruncateAtFirstBad`]
//!   then recovers the prefix up to the damage.
//!
//! Creating a log syncs the new file and its directory. Compaction is
//! atomic: the new log is written to `<name>.compact` in the same
//! directory, synced, renamed over the log, and the directory is synced;
//! a crash at any point leaves either the old log or the new one, each
//! complete. A stale `<name>.compact` from an interrupted compaction is
//! removed by the next compaction and is otherwise ignored.
//!
//! # Corruption
//!
//! A damaged record that is followed by an intact one cannot be a torn
//! append (appends only happen at the end), so open reports it as
//! [`FileKvError::Corrupt`] with its byte offset instead of dropping data,
//! unless [`Recovery::TruncateAtFirstBad`] was chosen. Damage confined to
//! the final record is indistinguishable from a torn append and is
//! discarded; that loses the last write if it was bit rot rather than a
//! crash.
//!
//! # Errors
//!
//! A failed operation leaves the store unchanged: the index is updated
//! only after the append (and, in sync mode, the sync) succeeded, and a
//! failed append is cut back off the file with `set_len`. If that cut-back
//! itself fails, or a [`FileKv::flush`] sync fails (after which the OS may
//! have dropped the unsynced pages), the on-disk state is unknown and the
//! store is poisoned: writes, flushes and compaction return
//! [`FileKvError::Poisoned`] while reads still serve the last acknowledged
//! state. Reopen to recover; an unacknowledged write may or may not be
//! present then.
//!
//! # Concurrency
//!
//! One `FileKv` must own a log at a time. Nothing locks the file (the
//! crate's minimum Rust predates `File::lock`), so two stores, in one
//! process or several, appending to the same path corrupt it.

mod index;
pub mod log;

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::vec::Vec;

use crate::key::{Key, KeyFilter, MAX_KEY_LEN};
use crate::kv::{Entry, Expect, Flow, KvError, KvLimits, KvStoreSync, Revision, Scan};

use index::{Index, Refusal};
pub use log::{Corruption, MAX_VALUE, Recovery};
use log::{ParseError, Record};

/// The most entries one [`KvStoreSync::scan`] call visits. A scan that
/// stops here returns `complete: false`; resume from the last key seen.
pub const SCAN_PAGE_LIMIT: usize = 256;

/// Revisions reserved per synced checkpoint in [`Durability::Batched`]
/// mode.
pub const RESERVE_BLOCK: u64 = 1024;

/// When a write reaches stable storage; see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Durability {
    /// `sync_data` after every write (the default).
    #[default]
    Sync,
    /// Sync only on [`FileKv::flush`], [`FileKv::compact`] and revision
    /// reservations.
    Batched,
}

/// How [`FileKv::open`] behaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Options {
    pub durability: Durability,
    pub recovery: Recovery,
}

impl Options {
    /// The defaults: [`Durability::Sync`], [`Recovery::Strict`].
    pub const fn new() -> Self {
        Options {
            durability: Durability::Sync,
            recovery: Recovery::Strict,
        }
    }

    /// These options with `durability`.
    pub const fn durability(self, durability: Durability) -> Self {
        Options { durability, ..self }
    }

    /// These options with `recovery`.
    pub const fn recovery(self, recovery: Recovery) -> Self {
        Options { recovery, ..self }
    }
}

/// The step during which an I/O error happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoOp {
    /// Opening or creating the log.
    Open,
    /// Reading the log during open.
    Read,
    /// Appending a record.
    Append,
    /// Syncing the log.
    Sync,
    /// Truncating a torn tail during open.
    Truncate,
    /// Writing, syncing or renaming the compacted log.
    Compact,
    /// Syncing the log's directory.
    SyncDir,
}

/// Failure of a [`FileKv`] operation, carried in [`KvError::Backend`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FileKvError {
    /// An I/O call failed.
    Io { op: IoOp, kind: io::ErrorKind },
    /// The file is not a `FileKv` log (wrong magic).
    NotALog,
    /// The log was written by an unknown format version.
    UnsupportedVersion(u32),
    /// The log is damaged at this byte offset and the damage is not a torn
    /// tail; see the module docs.
    Corrupt { offset: u64, why: Corruption },
    /// The path has no file name.
    InvalidPath,
    /// An earlier failure left the on-disk state unknown; reopen.
    Poisoned,
}

impl core::fmt::Display for FileKvError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FileKvError::Io { op, kind } => write!(f, "kv log I/O failed during {op:?}: {kind}"),
            FileKvError::NotALog => f.write_str("not a kv log file"),
            FileKvError::UnsupportedVersion(v) => write!(f, "unsupported kv log version {v}"),
            FileKvError::Corrupt { offset, why } => {
                write!(f, "kv log corrupt at byte {offset}: {why:?}")
            }
            FileKvError::InvalidPath => f.write_str("kv log path has no file name"),
            FileKvError::Poisoned => f.write_str("kv store poisoned by an earlier failure"),
        }
    }
}

impl std::error::Error for FileKvError {}

fn io_err(op: IoOp) -> impl Fn(io::Error) -> FileKvError {
    move |e| FileKvError::Io { op, kind: e.kind() }
}

/// A durable key-value store in one append-only log file; see the module
/// docs for format, durability and recovery.
#[derive(Debug)]
pub struct FileKv {
    path: PathBuf,
    compact_path: PathBuf,
    /// Opened for reading and appending.
    file: File,
    /// Bytes of the log known to be valid; appends start here.
    len: u64,
    index: Index,
    durability: Durability,
    /// Batched mode: the highest revision covered by a synced checkpoint.
    reserved: u64,
    poisoned: bool,
}

impl FileKv {
    /// Opens the log at `path`, creating it (but not its directory) if it
    /// does not exist, and replays it. A torn final record is truncated
    /// away and the truncation synced. Damage before an intact record is
    /// [`FileKvError::Corrupt`] unless `options.recovery` is
    /// [`Recovery::TruncateAtFirstBad`], which truncates at the damage.
    pub fn open(path: impl AsRef<Path>, options: Options) -> Result<Self, FileKvError> {
        let path = path.as_ref().to_path_buf();
        let name = path.file_name().ok_or(FileKvError::InvalidPath)?;
        let mut compact_name = name.to_os_string();
        compact_name.push(".compact");
        let compact_path = path.with_file_name(compact_name);

        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(&path)
            .map_err(io_err(IoOp::Open))?;
        let mut image = Vec::new();
        file.read_to_end(&mut image).map_err(io_err(IoOp::Read))?;

        let parsed = log::parse(&image, options.recovery).map_err(|e| match e {
            ParseError::NotALog => FileKvError::NotALog,
            ParseError::UnsupportedVersion(v) => FileKvError::UnsupportedVersion(v),
            ParseError::Corrupt { offset, why } => FileKvError::Corrupt { offset, why },
        })?;
        let index = Index::replay(&parsed.records).map_err(|e| FileKvError::Corrupt {
            offset: e.offset,
            why: e.why,
        })?;

        let len = if !parsed.header_present {
            // New (or torn while being created): start a fresh log.
            file.set_len(0).map_err(io_err(IoOp::Truncate))?;
            file.write_all(&log::header())
                .map_err(io_err(IoOp::Append))?;
            file.sync_all().map_err(io_err(IoOp::Sync))?;
            sync_dir(&path)?;
            log::HEADER_LEN as u64
        } else {
            let keep = parsed.keep as u64;
            if parsed.keep < image.len() {
                file.set_len(keep).map_err(io_err(IoOp::Truncate))?;
                file.sync_data().map_err(io_err(IoOp::Sync))?;
            }
            keep
        };

        let reserved = index.issued();
        Ok(FileKv {
            path,
            compact_path,
            file,
            len,
            index,
            durability: options.durability,
            reserved,
            poisoned: false,
        })
    }

    /// The log's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Current length of the log file in bytes.
    pub fn log_len(&self) -> u64 {
        self.len
    }

    /// Number of live entries.
    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// `true` if the store holds no entries.
    pub fn is_empty(&self) -> bool {
        self.index.len() == 0
    }

    /// Makes every acknowledged write durable (`sync_data`). A no-op for
    /// durability in [`Durability::Sync`] mode, where writes already are.
    /// A failure poisons the store (see the module docs).
    pub fn flush(&mut self) -> Result<(), FileKvError> {
        self.check_poisoned()?;
        self.file.sync_data().map_err(|e| {
            self.poisoned = true;
            io_err(IoOp::Sync)(e)
        })
    }

    /// Rewrites the log to hold only the live entries (each with its
    /// revision) and a checkpoint of the revision counter, atomically:
    /// write `<name>.compact`, sync it, rename it over the log, sync the
    /// directory. On an error before the rename the old log stays in use
    /// and the temporary file is removed. A failure of the final directory
    /// sync is returned, but the store already uses the new log, whose
    /// content is the same state.
    pub fn compact(&mut self) -> Result<(), FileKvError> {
        self.check_poisoned()?;
        let compact = io_err(IoOp::Compact);
        match fs::remove_file(&self.compact_path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(compact(e)),
        }
        let written = self.write_compacted();
        let (file, len) = match written {
            Ok(done) => done,
            Err(e) => {
                // Best effort: the old log is intact and still in use.
                let _ = fs::remove_file(&self.compact_path);
                return Err(e);
            }
        };
        if let Err(e) = fs::rename(&self.compact_path, &self.path) {
            let _ = fs::remove_file(&self.compact_path);
            return Err(compact(e));
        }
        self.file = file;
        self.len = len;
        self.reserved = self.index.issued();
        sync_dir(&self.path)
    }

    /// Writes and syncs the compacted log to the temporary path; returns its
    /// handle (positioned for appends) and length.
    fn write_compacted(&self) -> Result<(File, u64), FileKvError> {
        let compact = io_err(IoOp::Compact);
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create_new(true)
            .open(&self.compact_path)
            .map_err(&compact)?;
        let mut out = BufWriter::new(file);
        out.write_all(&log::header()).map_err(&compact)?;
        let mut len = log::HEADER_LEN as u64;
        let mut frame = Vec::new();
        for record in self.index.compacted() {
            frame.clear();
            log::encode(&record, &mut frame).map_err(|_| FileKvError::Corrupt {
                offset: len,
                why: Corruption::Layout,
            })?;
            out.write_all(&frame).map_err(&compact)?;
            len += frame.len() as u64;
        }
        let file = out.into_inner().map_err(|e| compact(e.into_error()))?;
        file.sync_all().map_err(&compact)?;
        Ok((file, len))
    }

    fn check_poisoned(&self) -> Result<(), FileKvError> {
        if self.poisoned {
            Err(FileKvError::Poisoned)
        } else {
            Ok(())
        }
    }

    /// Appends `record` (preceded by a reservation checkpoint when batched
    /// mode needs one), syncs as the durability mode requires, and only
    /// then commits it to the index. On failure the appended bytes are cut
    /// back off and the index is untouched.
    fn append(&mut self, record: &Record<'_>) -> Result<(), FileKvError> {
        self.check_poisoned()?;
        let revision = record.revision();
        let reserve = match self.durability {
            Durability::Batched if revision > self.reserved => {
                Some(revision.saturating_add(RESERVE_BLOCK - 1))
            }
            _ => None,
        };
        let mut bytes = Vec::new();
        if let Some(upto) = reserve {
            log::encode(&Record::Checkpoint { revision: upto }, &mut bytes).map_err(|_| {
                FileKvError::Corrupt {
                    offset: self.len,
                    why: Corruption::Layout,
                }
            })?;
        }
        log::encode(record, &mut bytes).map_err(|_| FileKvError::Corrupt {
            offset: self.len,
            why: Corruption::Layout,
        })?;
        let sync = self.durability == Durability::Sync || reserve.is_some();

        let written = self
            .file
            .write_all(&bytes)
            .map_err(io_err(IoOp::Append))
            .and_then(|()| {
                if sync {
                    self.file.sync_data().map_err(io_err(IoOp::Sync))
                } else {
                    Ok(())
                }
            });
        match written {
            Ok(()) => {
                self.len += bytes.len() as u64;
                self.index.commit(record);
                if let Some(upto) = reserve {
                    self.reserved = upto;
                }
                Ok(())
            }
            Err(e) => {
                let undone = self
                    .file
                    .set_len(self.len)
                    .and_then(|()| self.file.sync_data());
                if undone.is_err() {
                    self.poisoned = true;
                }
                Err(e)
            }
        }
    }
}

/// Syncs the directory holding `path`, so a created or renamed entry is
/// durable.
fn sync_dir(path: &Path) -> Result<(), FileKvError> {
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(io_err(IoOp::SyncDir))
}

fn refused(r: Refusal) -> KvError<FileKvError> {
    match r {
        Refusal::Conflict { current } => KvError::Conflict { current },
        Refusal::TooLarge => KvError::TooLarge,
        Refusal::Full => KvError::Full,
    }
}

impl KvStoreSync for FileKv {
    type Error = FileKvError;

    const LIMITS: KvLimits = KvLimits {
        max_key: MAX_KEY_LEN,
        max_value: MAX_VALUE,
    };

    fn get<'b>(
        &mut self,
        key: Key<'_>,
        buf: &'b mut [u8],
    ) -> Result<Option<Entry<'b>>, KvError<FileKvError>> {
        let Some((revision, value)) = self.index.get(key) else {
            return Ok(None);
        };
        let dst = buf.get_mut(..value.len()).ok_or(KvError::BufferTooSmall {
            needed: value.len(),
        })?;
        dst.copy_from_slice(value);
        Ok(Some(Entry {
            value: dst,
            revision,
        }))
    }

    fn put(
        &mut self,
        key: Key<'_>,
        value: &[u8],
        expect: Expect,
    ) -> Result<Revision, KvError<FileKvError>> {
        self.check_poisoned().map_err(KvError::Backend)?;
        let record = self.index.plan_put(key, value, expect).map_err(refused)?;
        self.append(&record).map_err(KvError::Backend)?;
        Ok(Revision(record.revision()))
    }

    fn delete(
        &mut self,
        key: Key<'_>,
        expect: Expect,
    ) -> Result<Option<Revision>, KvError<FileKvError>> {
        self.check_poisoned().map_err(KvError::Backend)?;
        let Some((record, removed)) = self.index.plan_delete(key, expect).map_err(refused)? else {
            return Ok(None);
        };
        self.append(&record).map_err(KvError::Backend)?;
        Ok(Some(removed))
    }

    fn scan<F>(
        &mut self,
        filter: KeyFilter<'_>,
        after: Option<Key<'_>>,
        buf: &mut [u8],
        mut on_entry: F,
    ) -> Result<Scan, KvError<FileKvError>>
    where
        F: FnMut(Key<'_>, Entry<'_>) -> Flow,
    {
        let mut entries = self.index.select(filter, after);
        let mut visited = 0;
        // Bounded by SCAN_PAGE_LIMIT callbacks.
        while visited < SCAN_PAGE_LIMIT {
            let Some((key, revision, value)) = entries.next() else {
                return Ok(Scan {
                    visited,
                    complete: true,
                });
            };
            let dst = buf.get_mut(..value.len()).ok_or(KvError::BufferTooSmall {
                needed: value.len(),
            })?;
            dst.copy_from_slice(value);
            visited += 1;
            if on_entry(
                key,
                Entry {
                    value: dst,
                    revision,
                },
            ) == Flow::Stop
            {
                break;
            }
        }
        Ok(Scan {
            visited,
            complete: entries.next().is_none(),
        })
    }
}

#[cfg(test)]
mod tests;
