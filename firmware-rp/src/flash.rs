//! A NOR flash adapter that erases one sector per driver call.
//!
//! `embassy_rp::flash::Flash::blocking_erase` erases its whole range in one
//! call that runs from RAM with interrupts disabled and the XIP cache off.
//! A loader `begin` for a 124 KiB image erases 32 sectors (about 1.5 s on
//! the Pico's W25Q16), which would hold off USB and the time driver for
//! the whole span. [`Sectored`] splits every erase into one call per 4 KiB
//! sector (about 45 ms each), so interrupts are serviced between sectors;
//! reads and writes pass through unchanged (a write is at most one loader
//! chunk). The loop runs at most `capacity / SECTOR` times.

use embedded_storage::nor_flash::{ErrorType, NorFlash, ReadNorFlash};
use esp_base::image::SECTOR;

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
}
