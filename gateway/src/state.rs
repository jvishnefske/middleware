//! The gateway's durable state: the highest accepted signed start/stop
//! counter ([`crate::update`], "The counter floor is persisted and fails
//! closed").
//!
//! * [`floor_at_start`] (pure) decides the floor a starting gateway uses
//!   from what [`ControlStore::load`] found and the clock: the larger of
//!   the persisted counter and the clock, the clock counting only at or
//!   after [`MIN_CLOCK_MS`] (2026-01-01T00:00:00Z, before this code
//!   existed, so an unsynced clock near 1970 or a stale image time never
//!   counts). No floor (`None`) when neither is usable, or when the state
//!   file exists but cannot be read: then start/stop are refused.
//! * [`ControlStore`] (I/O) keeps the counter in `<dir>/control-counter`
//!   (`MW_GATEWAY_STATE`, default `/var/lib/mw-gateway`) as one decimal
//!   line. [`ControlStore::save`] writes a temporary file, fsyncs it,
//!   renames it over the old one and fsyncs the directory, so after a
//!   crash the file holds either the old or the new counter, never a torn
//!   one. It refuses to lower the stored value. Saves within one process
//!   are serialized: mw-posix-node's Zenoh path shares the store.
//!
//! One counter for the port, not one per device: a counter accepted for
//! one device only raises the bar for the next, and signed requests name
//! their device's MAC, so a captured one never works on another device.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use middleware::loader::trust::ControlCounter;

/// The earliest clock reading trusted as wall time: 2026-01-01T00:00:00Z
/// in Unix milliseconds.
pub const MIN_CLOCK_MS: u64 = 1_767_225_600_000;

/// The default state directory.
pub const DEFAULT_STATE_DIR: &str = "/var/lib/mw-gateway";

/// The state file's name inside the directory.
pub const COUNTER_FILE: &str = "control-counter";

/// What loading the persisted counter found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Persisted {
    /// No state file: nothing was ever accepted here.
    Nothing,
    /// The stored counter.
    At(ControlCounter),
    /// The file exists but cannot be read or parsed: the floor is unknown.
    Unreadable,
}

/// The floor a starting gateway uses (module docs). Pure.
pub fn floor_at_start(persisted: Persisted, clock_ms: Option<u64>) -> Option<ControlCounter> {
    let clock = clock_ms
        .filter(|ms| *ms >= MIN_CLOCK_MS)
        .map(ControlCounter);
    match persisted {
        Persisted::Unreadable => None,
        Persisted::Nothing => clock,
        Persisted::At(c) => Some(clock.map_or(c, |k| k.max(c))),
    }
}

/// Parses the file's contents: one decimal counter, optional newline.
/// Pure.
pub fn parse_counter(text: &str) -> Option<ControlCounter> {
    text.trim().parse::<u64>().ok().map(ControlCounter)
}

/// The persisted counter (module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlStore {
    dir: PathBuf,
}

impl ControlStore {
    /// The store in `dir` (created on the first save if missing).
    pub fn in_dir(dir: impl Into<PathBuf>) -> Self {
        ControlStore { dir: dir.into() }
    }

    /// The state file.
    pub fn path(&self) -> PathBuf {
        self.dir.join(COUNTER_FILE)
    }

    /// Reads the stored counter.
    pub fn load(&self) -> Persisted {
        match fs::read_to_string(self.path()) {
            Ok(text) => parse_counter(&text).map_or(Persisted::Unreadable, Persisted::At),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Persisted::Nothing,
            Err(_) => Persisted::Unreadable,
        }
    }

    /// Stores `counter` durably (module docs). A counter at or below the
    /// stored one is an error (`InvalidInput`), as is any I/O failure;
    /// the caller then must not act on the request.
    pub fn save(&self, counter: ControlCounter) -> io::Result<()> {
        // One save at a time in this process: mw-posix-node's device thread
        // saves Zenoh starts/stops into the store its gateway uses, and the
        // check below and the rename must not interleave.
        let _one = SAVING
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match self.load() {
            Persisted::At(c) if c >= counter => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "counter not above the stored one",
                ));
            }
            Persisted::Unreadable => {
                return Err(io::Error::other("state file unreadable"));
            }
            _ => {}
        }
        fs::create_dir_all(&self.dir)?;
        let tmp = self.dir.join(format!("{COUNTER_FILE}.tmp"));
        {
            let mut f = fs::File::create(&tmp)?;
            writeln!(f, "{}", counter.0)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, self.path())?;
        sync_dir(&self.dir)
    }
}

/// Serializes [`ControlStore::save`] within the process.
static SAVING: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn sync_dir(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory under the system temp dir.
    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mw-gateway-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn rg27_the_floor_is_the_persisted_counter_or_a_sane_clock() {
        let c = ControlCounter(MIN_CLOCK_MS + 10_000);
        // A backwards clock (unsynced, stale) never lowers a persisted floor.
        assert_eq!(
            floor_at_start(Persisted::At(c), Some(MIN_CLOCK_MS + 5)),
            Some(c)
        );
        assert_eq!(floor_at_start(Persisted::At(c), Some(1_000)), Some(c));
        assert_eq!(floor_at_start(Persisted::At(c), None), Some(c));
        // A later clock raises it.
        let later = MIN_CLOCK_MS + 20_000;
        assert_eq!(
            floor_at_start(Persisted::At(c), Some(later)),
            Some(ControlCounter(later))
        );
        // No state: only a sane clock gives a floor; 1970 or no clock: none.
        assert_eq!(
            floor_at_start(Persisted::Nothing, Some(later)),
            Some(ControlCounter(later))
        );
        assert_eq!(
            floor_at_start(Persisted::Nothing, Some(MIN_CLOCK_MS - 1)),
            None
        );
        assert_eq!(floor_at_start(Persisted::Nothing, Some(0)), None);
        assert_eq!(floor_at_start(Persisted::Nothing, None), None);
        // An unreadable state file: unknown, whatever the clock says.
        assert_eq!(floor_at_start(Persisted::Unreadable, Some(later)), None);
        assert_eq!(parse_counter("42\n"), Some(ControlCounter(42)));
        assert_eq!(parse_counter("4x"), None);
    }

    #[test]
    fn rg27_a_saved_counter_survives_a_restart_and_beats_a_backwards_clock() {
        let dir = scratch("restart");
        let store = ControlStore::in_dir(&dir);
        assert_eq!(store.load(), Persisted::Nothing);
        let accepted = ControlCounter(MIN_CLOCK_MS + 99_000);
        store.save(accepted).unwrap();
        // Never lowered.
        assert!(store.save(ControlCounter(MIN_CLOCK_MS)).is_err());
        assert!(store.save(accepted).is_err());
        // "Restart": a new store reads it back; a clock behind it (no NTP
        // yet) does not lower the floor, so the accepted request (and
        // anything older) stays refused.
        let again = ControlStore::in_dir(&dir);
        assert_eq!(again.load(), Persisted::At(accepted));
        assert_eq!(floor_at_start(again.load(), Some(1_000)), Some(accepted));
        let gate = crate::update::UpdateGate::new(
            crate::update::Updaters::none(),
            floor_at_start(again.load(), Some(1_000)),
        );
        assert_eq!(gate.control_counter(), Some(accepted));
        // A corrupt file is unreadable: no floor, and no save over it.
        fs::write(again.path(), "garbage").unwrap();
        assert_eq!(again.load(), Persisted::Unreadable);
        assert!(again.save(ControlCounter(u64::MAX)).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rg27_a_write_failure_is_an_error() {
        let dir = scratch("readonly");
        fs::create_dir_all(&dir).unwrap();
        // The state "directory" is a regular file: nothing can be written.
        let file = dir.join("not-a-dir");
        fs::write(&file, b"x").unwrap();
        let store = ControlStore::in_dir(&file);
        assert!(store.save(ControlCounter(MIN_CLOCK_MS + 1)).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    /// The gateway's task and mw-posix-node's device thread save into one
    /// store: of two saves of the same counter at once, one succeeds.
    #[test]
    fn rg27_concurrent_saves_of_one_counter_accept_it_once() {
        for round in 0..50u64 {
            let dir = scratch(&format!("race-{round}"));
            let store = ControlStore::in_dir(&dir);
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
            let oks: usize = (0..4)
                .map(|_| {
                    let (s, b) = (store.clone(), barrier.clone());
                    std::thread::spawn(move || {
                        b.wait();
                        s.save(ControlCounter(7)).is_ok()
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|t| usize::from(t.join().expect("thread")))
                .sum();
            assert_eq!(oks, 1, "round {round}");
            let _ = fs::remove_dir_all(&dir);
        }
    }
}
