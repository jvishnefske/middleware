//! RAM NOR flash model for the host unit tests (test builds only).
//!
//! [`RamFlash`] behaves like the ESP SPI flash as `esp-storage` exposes
//! it: 4-byte aligned reads and writes, 4 KiB sector erases to `0xFF`, and
//! writes that can only clear bits. It starts zeroed (neither erased nor
//! valid), like a region holding someone else's data.
//!
//! Power loss is injected with [`RamFlash::cut_at`]: the erase or write
//! with that index (counting from 0 after the call) is torn, half of its
//! bytes applied, and it and every later erase or write fail, until
//! [`RamFlash::restore`].

use crate::image::SECTOR;
use embedded_storage::nor_flash::{ErrorType, NorFlash, NorFlashErrorKind, ReadNorFlash};

/// `N` bytes of RAM NOR flash.
pub struct RamFlash<const N: usize> {
    /// The contents.
    pub mem: [u8; N],
    /// Erase operations performed (including torn ones).
    pub erases: usize,
    ops: usize,
    cut: Option<usize>,
}

impl<const N: usize> RamFlash<N> {
    /// Zeroed flash with no fault armed.
    pub fn new() -> Self {
        RamFlash { mem: [0x00; N], erases: 0, ops: 0, cut: None }
    }

    /// Arms power loss at the `op`-th erase or write from now.
    pub fn cut_at(&mut self, op: usize) {
        self.ops = 0;
        self.cut = Some(op);
    }

    /// Power is back: no more faults.
    pub fn restore(&mut self) {
        self.cut = None;
    }

    /// How much of an operation of `len` bytes takes effect: all, half
    /// (torn, then fail), or none (fail).
    fn admit(&mut self, len: usize) -> Result<usize, usize> {
        let op = self.ops;
        self.ops += 1;
        match self.cut {
            Some(cut) if op == cut => Err(len / 2),
            Some(cut) if op > cut => Err(0),
            _ => Ok(len),
        }
    }
}

impl<const N: usize> ErrorType for RamFlash<N> {
    type Error = NorFlashErrorKind;
}

impl<const N: usize> ReadNorFlash for RamFlash<N> {
    const READ_SIZE: usize = 4;
    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
        if !offset.is_multiple_of(4) || !bytes.len().is_multiple_of(4) {
            return Err(NorFlashErrorKind::NotAligned);
        }
        let src = self.mem.get(offset as usize..offset as usize + bytes.len());
        bytes.copy_from_slice(src.ok_or(NorFlashErrorKind::OutOfBounds)?);
        Ok(())
    }
    fn capacity(&self) -> usize {
        self.mem.len()
    }
}

impl<const N: usize> NorFlash for RamFlash<N> {
    const WRITE_SIZE: usize = 4;
    const ERASE_SIZE: usize = SECTOR as usize;
    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
        if !offset.is_multiple_of(4) || !bytes.len().is_multiple_of(4) {
            return Err(NorFlashErrorKind::NotAligned);
        }
        let admitted = self.admit(bytes.len());
        let n = admitted.unwrap_or_else(|n| n);
        let dst = self.mem.get_mut(offset as usize..offset as usize + bytes.len());
        for (d, s) in dst.ok_or(NorFlashErrorKind::OutOfBounds)?.iter_mut().zip(bytes).take(n) {
            *d &= *s;
        }
        admitted.map(|_| ()).map_err(|_| NorFlashErrorKind::Other)
    }
    fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
        if !from.is_multiple_of(SECTOR) || !to.is_multiple_of(SECTOR) {
            return Err(NorFlashErrorKind::NotAligned);
        }
        let span = self.mem.get_mut(from as usize..to as usize).ok_or(NorFlashErrorKind::OutOfBounds)?;
        let len = span.len();
        let admitted = self.admit(len);
        let n = admitted.unwrap_or_else(|n| n);
        if let Some(part) = self.mem.get_mut(from as usize..from as usize + n) {
            part.fill(0xFF);
        }
        self.erases += 1;
        admitted.map(|_| ()).map_err(|_| NorFlashErrorKind::Other)
    }
}
