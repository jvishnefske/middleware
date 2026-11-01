//! RAM NOR flash model for the host unit tests (test builds only).
//!
//! [`RamFlash`] behaves like the QSPI flash behind
//! `embassy_rp::flash::Flash`, with the stricter 4-byte read and write
//! alignment of the ESP model so the store's word handling is exercised:
//! 4 KiB sector erases to `0xFF`, writes that only clear bits, and contents
//! that start zeroed (neither erased nor valid).
//!
//! Power loss is injected with [`RamFlash::cut_at`]: the erase or write
//! with that index (counting from 0 after the call) is torn, half of its
//! bytes applied, and it and every later erase or write fail, until
//! [`RamFlash::restore`]. (esp-base has the same model, private to its
//! tests.)

use embedded_storage::nor_flash::{ErrorType, NorFlash, NorFlashErrorKind, ReadNorFlash};
use esp_base::image::SECTOR;

/// `N` bytes of RAM NOR flash.
pub struct RamFlash<const N: usize> {
    /// The contents.
    pub mem: [u8; N],
    /// Erase calls made (including torn ones).
    pub erase_calls: usize,
    ops: usize,
    cut: Option<usize>,
}

impl<const N: usize> RamFlash<N> {
    /// Zeroed flash with no fault armed.
    pub fn new() -> Self {
        RamFlash { mem: [0x00; N], erase_calls: 0, ops: 0, cut: None }
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

    /// How much of an operation of `len` bytes takes effect: all, or the
    /// torn half (`Err`).
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
        N
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
        if !from.is_multiple_of(SECTOR) || !to.is_multiple_of(SECTOR) || from > to {
            return Err(NorFlashErrorKind::NotAligned);
        }
        let len = self.mem.get(from as usize..to as usize).ok_or(NorFlashErrorKind::OutOfBounds)?.len();
        self.erase_calls += 1;
        let admitted = self.admit(len);
        let n = admitted.unwrap_or_else(|n| n);
        if let Some(part) = self.mem.get_mut(from as usize..from as usize + n) {
            part.fill(0xFF);
        }
        admitted.map(|_| ()).map_err(|_| NorFlashErrorKind::Other)
    }
}
