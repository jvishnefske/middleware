//! The flash the base firmware's store and trust record run on: a NOR
//! flash model whose sectors persist in files.
//!
//! [`SectorFlash`] implements `embedded_storage`'s `NorFlash` with the
//! rules of [`nor`] (the ESP and RP drivers' rules), so
//! `esp_base::store::PayloadStore` and `esp_base::trust_store::TrustStore`
//! run on it unchanged. It keeps the whole device in memory and makes it
//! durable through a [`Sectors`] backend, one sector at a time:
//! [`dir::DirSectors`] in the state directory on the node, [`mem::MemSectors`]
//! in tests.
//!
//! # Durability and power loss
//!
//! A backend replaces one sector atomically and durably ([`Sectors::store`]:
//! on files, write a temporary file, `fsync` it, rename it over the
//! sector's file, `fsync` the directory), and an erased sector is a missing
//! file. Writes are applied to the memory image at once and the sector they
//! touched is marked dirty; the dirty sector is stored before any other
//! sector is written or erased, and on [`Durable::sync`]. So at most one
//! sector is ever ahead of the disk, and what the disk holds after a crash
//! at any moment is the image after some prefix of the operations — the
//! power-loss model esp-base's store and trust record are proven against
//! (T8, E3), with the stronger property that no sector is ever torn. The
//! request service syncs after every storage action except a chunk write,
//! so a reply that confirms an install, an activation or a claim is only
//! sent once it is on disk, while the chunks of one sector share one
//! `fsync`.
//!
//! When a store or a sync fails the flash is *poisoned*: every later
//! operation fails (`NorFlashErrorKind::Other`), so the service answers
//! `Internal` and nothing runs from a memory image the disk may not hold.
//! Restarting the node re-reads the disk.

pub mod dir;
pub mod mem;
pub mod nor;

use embedded_storage::nor_flash::{ErrorType, NorFlash, NorFlashErrorKind, ReadNorFlash};
use std::io;

/// Erase granularity: esp-base's sector.
pub const SECTOR: u32 = esp_base::image::SECTOR;

/// One sector's bytes.
pub type SectorBytes = [u8; SECTOR as usize];

/// Where sectors persist.
pub trait Sectors {
    /// Every stored sector below `count`: its index and bytes. Sectors not
    /// returned are erased.
    fn load(&mut self, count: u32) -> io::Result<Vec<(u32, Box<SectorBytes>)>>;
    /// Replaces sector `index` atomically and durably: afterwards it reads
    /// `data`, or erased for `None`; on an error it reads as before.
    fn store(&mut self, index: u32, data: Option<&SectorBytes>) -> io::Result<()>;
}

/// Making written data durable.
pub trait Durable {
    /// Stores the dirty sector, if any.
    fn sync(&mut self) -> Result<(), NorFlashErrorKind>;
}

/// A NOR flash of `count` sectors over a [`Sectors`] backend (module docs).
#[derive(Debug)]
pub struct SectorFlash<S> {
    sectors: S,
    mem: Vec<u8>,
    dirty: Option<u32>,
    poisoned: bool,
}

impl<S: Sectors> SectorFlash<S> {
    /// Reads the stored sectors of a `count`-sector device.
    pub fn open(mut sectors: S, count: u32) -> io::Result<Self> {
        let size = (count as usize).checked_mul(SECTOR as usize).ok_or_else(|| io::Error::other("flash too large"))?;
        let mut mem = vec![0xFF; size];
        for (index, data) in sectors.load(count)? {
            let at = index as usize * SECTOR as usize;
            if let Some(dst) = mem.get_mut(at..at + SECTOR as usize) {
                dst.copy_from_slice(&data[..]);
            }
        }
        Ok(SectorFlash { sectors, mem, dirty: None, poisoned: false })
    }

    /// The backend (to inspect what is durable).
    pub fn sectors(&self) -> &S {
        &self.sectors
    }

    /// Whether a failed store has poisoned the flash.
    pub fn poisoned(&self) -> bool {
        self.poisoned
    }

    fn live(&self) -> Result<(), NorFlashErrorKind> {
        if self.poisoned { Err(NorFlashErrorKind::Other) } else { Ok(()) }
    }

    fn sector(&self, index: u32) -> Option<&SectorBytes> {
        let at = index as usize * SECTOR as usize;
        self.mem.get(at..at + SECTOR as usize).and_then(|s| s.try_into().ok())
    }

    /// Stores sector `index` from memory; poisons on failure.
    fn persist(&mut self, index: u32) -> Result<(), NorFlashErrorKind> {
        let data = self.sector(index).ok_or(NorFlashErrorKind::OutOfBounds)?;
        let data = (!nor::is_erased(data)).then(|| Box::new(*data));
        match self.sectors.store(index, data.as_deref()) {
            Ok(()) => Ok(()),
            Err(_) => {
                self.poisoned = true;
                Err(NorFlashErrorKind::Other)
            }
        }
    }

    /// Stores the dirty sector unless it is `keep`.
    fn flush_unless(&mut self, keep: Option<u32>) -> Result<(), NorFlashErrorKind> {
        match self.dirty {
            Some(d) if Some(d) != keep => {
                self.persist(d)?;
                self.dirty = None;
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

impl<S: Sectors> Durable for SectorFlash<S> {
    fn sync(&mut self) -> Result<(), NorFlashErrorKind> {
        self.live()?;
        self.flush_unless(None)
    }
}

impl<S> ErrorType for SectorFlash<S> {
    type Error = NorFlashErrorKind;
}

impl<S: Sectors> ReadNorFlash for SectorFlash<S> {
    const READ_SIZE: usize = nor::WORD as usize;

    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
        self.live()?;
        let r = nor::span(offset, bytes.len(), self.mem.len())?;
        bytes.copy_from_slice(self.mem.get(r).ok_or(NorFlashErrorKind::OutOfBounds)?);
        Ok(())
    }

    fn capacity(&self) -> usize {
        self.mem.len()
    }
}

impl<S: Sectors> NorFlash for SectorFlash<S> {
    const WRITE_SIZE: usize = nor::WORD as usize;
    const ERASE_SIZE: usize = SECTOR as usize;

    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
        self.live()?;
        let r = nor::span(offset, bytes.len(), self.mem.len())?;
        for index in nor::sectors_of(&r) {
            self.flush_unless(Some(index))?;
            let part = nor::within(&r, index);
            let src = bytes.get(part.start - r.start..part.end - r.start).ok_or(NorFlashErrorKind::OutOfBounds)?;
            let dst = self.mem.get_mut(part).ok_or(NorFlashErrorKind::OutOfBounds)?;
            nor::program(dst, src);
            self.dirty = Some(index);
        }
        Ok(())
    }

    fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
        self.live()?;
        let span = nor::erase_sectors(from, to, self.mem.len())?;
        self.flush_unless(None)?;
        for index in span {
            let at = index as usize * SECTOR as usize;
            if let Some(s) = self.mem.get_mut(at..at + SECTOR as usize) {
                s.fill(0xFF);
            }
            self.persist(index)?;
        }
        Ok(())
    }
}

/// The lent flash works as the flash: the store keeps its flash by value,
/// and tests lend theirs.
impl<F: Durable> Durable for &mut F {
    fn sync(&mut self) -> Result<(), NorFlashErrorKind> {
        (**self).sync()
    }
}

#[cfg(test)]
mod tests {
    use super::mem::{Disk, MemSectors};
    use super::*;

    fn open(disk: &mut Disk, count: u32) -> SectorFlash<MemSectors<'_>> {
        SectorFlash::open(MemSectors::new(disk), count).expect("open")
    }

    #[test]
    fn pn3_blank_flash_reads_erased_and_writes_follow_nor_rules() {
        let mut disk = Disk::default();
        let mut f = open(&mut disk, 2);
        assert_eq!(f.capacity(), 8192);
        let mut b = [0u8; 8];
        f.read(0, &mut b).expect("read");
        assert_eq!(b, [0xFF; 8]);
        f.write(0, &[0x0F, 0xF0, 0xFF, 0x00]).expect("write");
        f.write(0, &[0xFF, 0x00, 0x0F, 0xFF]).expect("write");
        f.read(0, &mut b[..4]).expect("read");
        assert_eq!(&b[..4], &[0x0F, 0x00, 0x0F, 0x00]);
        assert_eq!(f.write(1, &[0; 4]), Err(NorFlashErrorKind::NotAligned));
        assert_eq!(f.write(8192, &[0; 4]), Err(NorFlashErrorKind::OutOfBounds));
        assert_eq!(f.erase(0, 100), Err(NorFlashErrorKind::NotAligned));
        f.erase(0, 4096).expect("erase");
        f.read(0, &mut b).expect("read");
        assert_eq!(b, [0xFF; 8]);
    }

    #[test]
    fn pn3_one_dirty_sector_is_stored_before_another_is_touched_and_on_sync() {
        let mut disk = Disk::default();
        {
            let mut f = open(&mut disk, 3);
            f.write(0, &[1, 2, 3, 4]).expect("write");
            f.write(4, &[5, 6, 7, 8]).expect("write");
            // Same sector: nothing stored yet.
            assert_eq!(f.sectors().stores(), 0);
            // A write that crosses into sector 1 stores sector 0 first.
            f.write(4092, &[9; 8]).expect("write");
            assert_eq!(f.sectors().stores(), 1);
            assert_eq!(f.sectors().durable(0).map(|s| s[..8].to_vec()), Some(vec![1, 2, 3, 4, 5, 6, 7, 8]));
            assert_eq!(f.sectors().durable(1), None);
            f.sync().expect("sync");
            assert_eq!(f.sectors().stores(), 2);
            assert_eq!(f.sectors().durable(1).map(|s| s[..4].to_vec()), Some(vec![9; 4]));
            // A sync with nothing dirty stores nothing.
            f.sync().expect("sync");
            assert_eq!(f.sectors().stores(), 2);
            // Erasing stores the dirty sector first, then the erase.
            f.write(8192, &[0; 4]).expect("write");
            f.erase(0, 4096).expect("erase");
            assert_eq!(f.sectors().durable(2).map(|s| s[..4].to_vec()), Some(vec![0; 4]));
            assert_eq!(f.sectors().durable(0), None);
        }
        // Reopened: what was stored is what reads back.
        let mut f = open(&mut disk, 3);
        let mut b = [0u8; 8];
        f.read(4092, &mut b).expect("read");
        assert_eq!(b, [0xFF, 0xFF, 0xFF, 0xFF, 9, 9, 9, 9]);
    }

    #[test]
    fn pn3_a_failed_store_poisons_the_flash() {
        let mut disk = Disk::default();
        let mut f = SectorFlash::open(MemSectors::new(&mut disk).cut_after(0), 2).expect("open");
        f.write(0, &[0; 4]).expect("buffered");
        assert_eq!(f.sync(), Err(NorFlashErrorKind::Other));
        assert!(f.poisoned());
        let mut b = [0u8; 4];
        assert_eq!(f.read(0, &mut b), Err(NorFlashErrorKind::Other));
        assert_eq!(f.write(0, &[0; 4]), Err(NorFlashErrorKind::Other));
        assert_eq!(f.erase(0, 4096), Err(NorFlashErrorKind::Other));
        assert_eq!(f.sectors().durable(0), None);
    }
}
