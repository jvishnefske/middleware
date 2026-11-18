//! [`DirSectors`]: sectors as files in the node's state directory.
//!
//! Layout under the state directory:
//!
//! * `lock` — held with an exclusive advisory lock (`File::try_lock`)
//!   while the node runs, so two processes never share one flash;
//! * `flash/sector-NNNN` — sector `NNNN` (decimal, four digits or more),
//!   exactly 4096 bytes; a missing file is an erased sector;
//! * `flash/.tmp-sector-NNNN` — a store in progress, removed at open.
//!
//! A store writes the temporary file, `fsync`s it, renames it over the
//! sector's file and `fsync`s the directory; an erase removes the file and
//! `fsync`s the directory. A crash therefore leaves each sector's file
//! wholly old or wholly new. A sector file of the wrong length was not
//! written by the node; opening refuses it (naming the file) rather than
//! guessing what the flash held.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::{SECTOR, SectorBytes, Sectors};

/// The flash's subdirectory of the state directory.
pub const FLASH_DIR: &str = "flash";
/// The lock file in the state directory.
pub const LOCK_FILE: &str = "lock";
const PREFIX: &str = "sector-";
const TMP_PREFIX: &str = ".tmp-";

/// The file name of sector `index`.
pub fn file_name(index: u32) -> String {
    format!("{PREFIX}{index:04}")
}

/// The sector a file name names, if it is one (four or more decimal digits,
/// no sign, the canonical form [`file_name`] writes).
pub fn parse_file_name(name: &str) -> Option<u32> {
    let digits = name.strip_prefix(PREFIX)?;
    if digits.len() < 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let index = digits.parse::<u32>().ok()?;
    (file_name(index) == name).then_some(index)
}

/// Sectors in `<state>/flash/` (module docs).
#[derive(Debug)]
pub struct DirSectors {
    dir: PathBuf,
    _lock: File,
}

impl DirSectors {
    /// Creates `<state>/flash/` if needed, takes the lock (failing if
    /// another process holds it) and removes leftover temporary files.
    pub fn open(state: &Path) -> io::Result<Self> {
        let dir = state.join(FLASH_DIR);
        fs::create_dir_all(&dir)?;
        let lock_path = state.join(LOCK_FILE);
        let lock = OpenOptions::new().create(true).truncate(false).write(true).open(&lock_path)?;
        lock.try_lock().map_err(|e| match e {
            TryLockError::WouldBlock => io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("{}: another mw-posix-node holds this state directory", lock_path.display()),
            ),
            TryLockError::Error(e) => e,
        })?;
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            if entry.file_name().to_str().is_some_and(|n| n.starts_with(TMP_PREFIX)) {
                fs::remove_file(entry.path())?;
            }
        }
        sync_dir(&dir)?;
        Ok(DirSectors { dir, _lock: lock })
    }

    /// The flash directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

impl Sectors for DirSectors {
    fn load(&mut self, count: u32) -> io::Result<Vec<(u32, Box<SectorBytes>)>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let Some(index) = entry.file_name().to_str().and_then(parse_file_name) else { continue };
            if index >= count {
                continue;
            }
            let path = entry.path();
            let bytes = fs::read(&path)?;
            let sector: Box<SectorBytes> = bytes.into_boxed_slice().try_into().map_err(|b: Box<[u8]>| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: {} bytes, a sector holds {SECTOR}", path.display(), b.len()),
                )
            })?;
            out.push((index, sector));
        }
        out.sort_by_key(|(i, _)| *i);
        Ok(out)
    }

    fn store(&mut self, index: u32, data: Option<&SectorBytes>) -> io::Result<()> {
        let path = self.dir.join(file_name(index));
        match data {
            Some(bytes) => {
                let tmp = self.dir.join(format!("{TMP_PREFIX}{}", file_name(index)));
                let mut f = File::create(&tmp)?;
                f.write_all(bytes)?;
                f.sync_all()?;
                drop(f);
                fs::rename(&tmp, &path)?;
            }
            None => match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(e),
            },
        }
        sync_dir(&self.dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flash::SectorFlash;
    use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};

    /// A fresh directory under the system's temporary directory, one per
    /// test and process.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mw-posix-node-unit-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn pn4_file_names_are_canonical() {
        assert_eq!(file_name(7), "sector-0007");
        assert_eq!(file_name(12345), "sector-12345");
        assert_eq!(parse_file_name("sector-0007"), Some(7));
        assert_eq!(parse_file_name("sector-12345"), Some(12345));
        for bad in ["sector-7", "sector-+007", "sector-00007", "sector-00a7", ".tmp-sector-0007", "lock"] {
            assert_eq!(parse_file_name(bad), None, "{bad}");
        }
    }

    #[test]
    fn pn4_sectors_persist_as_whole_files_and_erased_is_missing() {
        let state = scratch("persist");
        {
            let mut f = SectorFlash::open(DirSectors::open(&state).expect("open"), 4).expect("flash");
            f.write(4096, &[1, 2, 3, 4]).expect("write");
            f.write(3 * 4096, &[5, 6, 7, 8]).expect("write");
            crate::flash::Durable::sync(&mut f).expect("sync");
        }
        let flash = state.join(FLASH_DIR);
        let mut names: Vec<String> = fs::read_dir(&flash)
            .expect("dir")
            .map(|e| e.expect("entry").file_name().into_string().expect("utf-8"))
            .collect();
        names.sort();
        assert_eq!(names, ["sector-0001", "sector-0003"]);
        assert_eq!(fs::metadata(flash.join("sector-0001")).expect("meta").len(), 4096);
        // A crash between writing a temporary file and renaming it leaves
        // the old sector; the leftover is removed at the next open.
        fs::write(flash.join(".tmp-sector-0001"), [0u8; 10]).expect("tmp");
        {
            let mut f = SectorFlash::open(DirSectors::open(&state).expect("reopen"), 4).expect("flash");
            let mut b = [0u8; 4];
            f.read(4096, &mut b).expect("read");
            assert_eq!(b, [1, 2, 3, 4]);
            f.read(0, &mut b).expect("read");
            assert_eq!(b, [0xFF; 4]);
            assert!(!flash.join(".tmp-sector-0001").exists());
            // Erasing removes the file.
            f.erase(4096, 8192).expect("erase");
            assert!(!flash.join("sector-0001").exists());
        }
        // A sector file of the wrong length is refused, naming it.
        fs::write(flash.join("sector-0002"), [0u8; 100]).expect("bad");
        let e = SectorFlash::open(DirSectors::open(&state).expect("reopen"), 4).expect_err("bad length");
        assert!(e.to_string().contains("sector-0002"), "{e}");
        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn pn4_one_process_holds_the_state_directory() {
        let state = scratch("lock");
        let held = DirSectors::open(&state).expect("open");
        let e = DirSectors::open(&state).expect_err("locked");
        assert_eq!(e.kind(), io::ErrorKind::WouldBlock);
        assert!(e.to_string().contains("another mw-posix-node"), "{e}");
        drop(held);
        DirSectors::open(&state).expect("free again");
        let _ = fs::remove_dir_all(&state);
    }
}
