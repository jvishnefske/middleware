//! NOR flash adapters: one sector per erase call ([`Sectored`]), and reads
//! through an untranslated window ([`Window`], RP2350).
//!
//! `embassy_rp::flash::Flash::blocking_erase` erases its whole range in one
//! call that runs from RAM with interrupts disabled and the XIP cache off.
//! A loader `begin` for a 124 KiB image erases 32 sectors (about 1.5 s on
//! the Pico's W25Q16), which would hold off USB and the time driver for
//! the whole span. [`Sectored`] splits every erase into one call per 4 KiB
//! sector (about 45 ms each), so interrupts are serviced between sectors;
//! reads and writes pass through unchanged (a write is at most one loader
//! chunk). The loop runs at most `capacity / SECTOR` times.
//!
//! On the RP2350 the boot ROM maps the booted partition at `0x1000_0000`
//! (QMI address translation, window 0) and faults any access past its end,
//! while the ROM's erase and program functions take physical flash offsets.
//! `embassy_rp::flash::Flash::blocking_read` reads at `0x1000_0000 +
//! offset`, so under a partition table it would read the running slot, not
//! the payload store. The shell maps the whole flash, untranslated, at
//! window 1 (`0x1040_0000`, QMI `ATRANS1`) and [`Window`] reads at
//! `read_base + offset` there, so every offset this firmware uses (payload
//! store, trust record, both firmware slots, the update record) is a
//! physical offset for reads, erases and writes alike.

use embedded_storage::nor_flash::{ErrorType, NorFlash, NorFlashError, NorFlashErrorKind, ReadNorFlash};
use esp_base::image::SECTOR;

/// `F` with reads shifted by `read_base` and every access bounded by
/// `size` (the physical flash); erases and writes pass through unchanged.
pub struct Window<F> {
    inner: F,
    read_base: u32,
    size: u32,
}

impl<F> Window<F> {
    /// `inner`, reading at `read_base + offset`, for a `size`-byte flash.
    pub const fn new(inner: F, read_base: u32, size: u32) -> Self {
        Window { inner, read_base, size }
    }

    /// The wrapped driver.
    pub fn inner(&mut self) -> &mut F {
        &mut self.inner
    }

    fn check(&self, offset: u32, len: usize) -> Result<(), NorFlashErrorKind> {
        let end = u32::try_from(len).ok().and_then(|n| offset.checked_add(n));
        match end {
            Some(end) if end <= self.size => Ok(()),
            _ => Err(NorFlashErrorKind::OutOfBounds),
        }
    }
}

/// A [`Window`] failure: out of the physical flash, or the driver's error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowError<E> {
    /// The access does not fit the physical flash.
    OutOfBounds,
    /// The driver failed.
    Flash(E),
}

impl<E: NorFlashError> NorFlashError for WindowError<E> {
    fn kind(&self) -> NorFlashErrorKind {
        match self {
            WindowError::OutOfBounds => NorFlashErrorKind::OutOfBounds,
            WindowError::Flash(e) => e.kind(),
        }
    }
}

impl<F: ErrorType> ErrorType for Window<F> {
    type Error = WindowError<F::Error>;
}

impl<F: ReadNorFlash> ReadNorFlash for Window<F> {
    const READ_SIZE: usize = F::READ_SIZE;

    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
        self.check(offset, bytes.len()).map_err(|_| WindowError::OutOfBounds)?;
        let at = offset.checked_add(self.read_base).ok_or(WindowError::OutOfBounds)?;
        self.inner.read(at, bytes).map_err(WindowError::Flash)
    }

    fn capacity(&self) -> usize {
        self.size as usize
    }
}

impl<F: NorFlash> NorFlash for Window<F> {
    const WRITE_SIZE: usize = F::WRITE_SIZE;
    const ERASE_SIZE: usize = F::ERASE_SIZE;

    fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
        let len = to.checked_sub(from).ok_or(WindowError::OutOfBounds)?;
        self.check(from, len as usize).map_err(|_| WindowError::OutOfBounds)?;
        self.inner.erase(from, to).map_err(WindowError::Flash)
    }

    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
        self.check(offset, bytes.len()).map_err(|_| WindowError::OutOfBounds)?;
        self.inner.write(offset, bytes).map_err(WindowError::Flash)
    }
}

/// `F`, with erases split into single sectors.
pub struct Sectored<F>(pub F);

impl<F> Sectored<F> {
    /// The wrapped driver.
    pub fn inner(&mut self) -> &mut F {
        &mut self.0
    }
}

impl<F: ErrorType> ErrorType for Sectored<F> {
    type Error = F::Error;
}

impl<F: ReadNorFlash> ReadNorFlash for Sectored<F> {
    const READ_SIZE: usize = F::READ_SIZE;

    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
        self.0.read(offset, bytes)
    }

    fn capacity(&self) -> usize {
        self.0.capacity()
    }
}

impl<F: NorFlash> NorFlash for Sectored<F> {
    const WRITE_SIZE: usize = F::WRITE_SIZE;
    const ERASE_SIZE: usize = F::ERASE_SIZE;

    fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
        if from > to || !from.is_multiple_of(SECTOR) || !to.is_multiple_of(SECTOR) {
            // Let the driver report its own error for a bad range.
            return self.0.erase(from, to);
        }
        let mut at = from;
        while at < to {
            self.0.erase(at, at + SECTOR)?;
            at += SECTOR;
        }
        Ok(())
    }

    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
        self.0.write(offset, bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ram_flash::RamFlash;

    #[test]
    fn v5_erases_one_sector_per_driver_call() {
        let mut f = Sectored(RamFlash::<{ 8 * SECTOR as usize }>::new());
        f.erase(SECTOR, 5 * SECTOR).unwrap();
        assert_eq!(f.inner().erase_calls, 4);
        assert!(f.inner().mem[SECTOR as usize..5 * SECTOR as usize].iter().all(|&b| b == 0xFF));
        assert!(f.inner().mem[..SECTOR as usize].iter().all(|&b| b == 0));
        // Empty range: nothing erased.
        f.erase(2 * SECTOR, 2 * SECTOR).unwrap();
        assert_eq!(f.inner().erase_calls, 4);
        // A bad range reaches the driver, which refuses it.
        assert!(f.erase(1, SECTOR).is_err());
        assert!(f.erase(0, 9 * SECTOR).is_err());
        // Reads and writes pass through.
        f.write(SECTOR, &[1, 2, 3, 4]).unwrap();
        let mut b = [0u8; 4];
        f.read(SECTOR, &mut b).unwrap();
        assert_eq!(b, [1, 2, 3, 4]);
        assert_eq!(f.capacity(), 8 * SECTOR as usize);
    }

    #[test]
    fn y12_window_reads_through_the_untranslated_alias_and_writes_physically() {
        // A 4-sector flash whose untranslated alias is the upper half of an
        // 8-sector model: reads see the alias, writes go to the low half.
        const N: usize = 4 * SECTOR as usize;
        let mut f = Window::new(RamFlash::<{ 2 * N }>::new(), N as u32, N as u32);
        f.inner().mem[N + 8..N + 12].copy_from_slice(&[9, 8, 7, 6]);
        let mut b = [0u8; 4];
        f.read(8, &mut b).unwrap();
        assert_eq!(b, [9, 8, 7, 6], "read at read_base + offset");
        f.erase(0, SECTOR).unwrap();
        f.write(8, &[1, 2, 3, 4]).unwrap();
        assert_eq!(&f.inner().mem[8..12], &[1, 2, 3, 4], "written at offset");
        assert_eq!(&f.inner().mem[N + 8..N + 12], &[9, 8, 7, 6]);
        // Nothing past the physical flash, in any direction.
        assert_eq!(f.read(N as u32 - 2, &mut b), Err(WindowError::OutOfBounds));
        assert_eq!(f.write(N as u32, &[0; 4]), Err(WindowError::OutOfBounds));
        assert_eq!(f.erase(N as u32, N as u32 + SECTOR), Err(WindowError::OutOfBounds));
        assert_eq!(f.erase(SECTOR, 0), Err(WindowError::OutOfBounds));
        assert_eq!(f.read(u32::MAX, &mut b), Err(WindowError::OutOfBounds));
        assert_eq!(f.capacity(), N);
        assert_eq!(WindowError::<NorFlashErrorKind>::OutOfBounds.kind(), NorFlashErrorKind::OutOfBounds);
    }
}
