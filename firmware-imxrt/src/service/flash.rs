//! [`RtFlash`]: the one view of the HyperFlash the request service's
//! stores share, with every erase a whole number of 256 KiB blocks.
//!
//! `esp_base::store::PayloadStore` owns its flash and lends it to the trust
//! record (`PayloadStore::flash`, and the closure of
//! `PayloadStore::activate_with` that saves a raised `min_version` before
//! the slot header). On the RP base firmware both stores sit on the same
//! 4 KiB-erase flash. On the RT1050 the trust record needs
//! [`SectorPerBlock`] and the payload store [`BlockErase`], two different
//! views, and the A/B updater needs the raw flash. [`RtFlash`] owns the
//! flash and presents one 4 KiB-sector address space with two windows, at
//! the flash map's own offsets:
//!
//! * the trust window, `[trust.offset, trust.offset + 8 KiB)`: the trust
//!   store's two 4 KiB sectors, each the first 4 KiB of one block of the
//!   trust region (through [`SectorPerBlock`]);
//! * the payload window, the payload region one to one (through
//!   [`BlockErase`]: an erase must start on a block boundary of the region
//!   and erases every block it touches).
//!
//! Anything outside the windows (boot block, update record, firmware
//! slots) is `OutOfBounds`, so neither store can reach the A/B regions;
//! the updater gets the raw flash through [`RtFlash::raw`]. An operation
//! must lie inside one window. The views are built per operation over
//! `&mut F` (no I/O, a few comparisons), so the flash has one owner.
//!
//! The flash's erase size must be [`BLOCK`]; that is checked when the
//! firmware is compiled ([`RtFlash::new`]), not at run time.

use crate::layout::erase::ViewError;
use crate::layout::{BLOCK, BlockErase, FlashMap, Region, SECTOR, SectorPerBlock};
use embedded_storage::nor_flash::{ErrorType, NorFlash, ReadNorFlash};
use esp_base::trust_store::PARTITION_LEN;

/// Where a logical span lies: the window and the offset inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Window {
    Trust(u32),
    Payload(u32),
}

/// The trust and payload windows over a block-erased flash. See the
/// module docs.
#[derive(Debug)]
pub struct RtFlash<F> {
    flash: F,
    trust: Region,
    payload: Region,
}

impl<F: NorFlash> RtFlash<F> {
    /// The windows of `map`'s trust and payload regions over `flash`. The
    /// map must have passed [`FlashMap::check`] (whole blocks); a flash
    /// whose erase size is not [`BLOCK`] does not compile.
    pub fn new(flash: F, map: &FlashMap) -> Self {
        const {
            assert!(
                F::ERASE_SIZE == BLOCK as usize,
                "the request service needs a flash with 256 KiB erase blocks"
            )
        };
        RtFlash {
            flash,
            trust: map.trust,
            payload: map.payload,
        }
    }

    /// The raw flash (offsets from the start of the HyperFlash), for the
    /// A/B updater.
    pub fn raw(&mut self) -> &mut F {
        &mut self.flash
    }

    /// The flash back.
    pub fn into_inner(self) -> F {
        self.flash
    }

    /// Finds the window holding all of `[at, at + len)`.
    fn locate(&self, at: u32, len: usize) -> Result<Window, ViewError<F::Error>> {
        let len = u32::try_from(len).map_err(|_| ViewError::OutOfBounds)?;
        let end = at.checked_add(len).ok_or(ViewError::OutOfBounds)?;
        let inside = |base: u32, size: u32| at >= base && end <= base.saturating_add(size);
        if inside(self.trust.offset, PARTITION_LEN) {
            Ok(Window::Trust(at - self.trust.offset))
        } else if inside(self.payload.offset, self.payload.size) {
            Ok(Window::Payload(at - self.payload.offset))
        } else {
            Err(ViewError::OutOfBounds)
        }
    }

    fn trust_view(&mut self) -> Result<SectorPerBlock<&mut F>, ViewError<F::Error>> {
        SectorPerBlock::new(&mut self.flash, self.trust)
    }

    fn payload_view(&mut self) -> Result<BlockErase<&mut F>, ViewError<F::Error>> {
        BlockErase::new(&mut self.flash, self.payload)
    }
}

impl<F: NorFlash> ErrorType for RtFlash<F> {
    type Error = ViewError<F::Error>;
}

impl<F: NorFlash> ReadNorFlash for RtFlash<F> {
    const READ_SIZE: usize = F::READ_SIZE;

    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
        match self.locate(offset, bytes.len())? {
            Window::Trust(at) => self.trust_view()?.read(at, bytes),
            Window::Payload(at) => self.payload_view()?.read(at, bytes),
        }
    }

    /// One past the highest logical offset of either window.
    fn capacity(&self) -> usize {
        let trust_end = self.trust.offset.saturating_add(PARTITION_LEN);
        trust_end.max(self.payload.end()) as usize
    }
}

impl<F: NorFlash> NorFlash for RtFlash<F> {
    const WRITE_SIZE: usize = F::WRITE_SIZE;
    const ERASE_SIZE: usize = SECTOR as usize;

    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
        match self.locate(offset, bytes.len())? {
            Window::Trust(at) => self.trust_view()?.write(at, bytes),
            Window::Payload(at) => self.payload_view()?.write(at, bytes),
        }
    }

    /// Erases through the window's view: one block per trust sector, or
    /// the payload blocks the span covers (starting on a block).
    fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
        let len = to.checked_sub(from).ok_or(ViewError::NotAligned)?;
        match self.locate(from, len as usize)? {
            Window::Trust(at) => self.trust_view()?.erase(at, at + len),
            Window::Payload(at) => self.payload_view()?.erase(at, at + len),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ab::test_flash::{BlockFlash, Op};
    use crate::layout::RT1050_EVKB;

    const MAP: FlashMap = RT1050_EVKB;

    #[test]
    fn rt12_the_windows_reach_only_the_trust_and_payload_regions() {
        let mut f = BlockFlash::erased(MAP.slot_b.end() as usize);
        let mut v = RtFlash::new(&mut f, &MAP);
        let t = MAP.trust.offset;
        let p = MAP.payload.offset;
        // The trust window's second sector is the trust region's second
        // block.
        v.write(t + SECTOR, &[1, 2]).unwrap();
        v.write(p + 4, &[3, 4]).unwrap();
        let mut b = [0u8; 2];
        v.read(t + SECTOR, &mut b).unwrap();
        assert_eq!(b, [1, 2]);
        for at in [
            0,
            MAP.record.offset,
            MAP.slot_a.offset,
            MAP.slot_b.offset,
            t + PARTITION_LEN,
        ] {
            assert_eq!(v.write(at, &[0, 0]), Err(ViewError::OutOfBounds), "{at:#x}");
            assert_eq!(v.read(at, &mut b), Err(ViewError::OutOfBounds), "{at:#x}");
        }
        // A span across the end of a window is refused.
        assert_eq!(
            v.read(t + PARTITION_LEN - 1, &mut b),
            Err(ViewError::OutOfBounds)
        );
        assert_eq!(
            v.erase(p + SECTOR, p + 2 * SECTOR),
            Err(ViewError::NotAligned)
        );
        v.erase(t + SECTOR, t + 2 * SECTOR).unwrap();
        v.erase(p, p + 3 * SECTOR).unwrap();
        assert_eq!(v.capacity(), MAP.payload.end() as usize);
        assert_eq!(f.bytes((t + BLOCK) as usize, 2), [0xFF, 0xFF]);
        assert_eq!(
            f.log[2..],
            [Op::Erase(t + BLOCK, t + 2 * BLOCK), Op::Erase(p, p + BLOCK)],
            "whole blocks"
        );
    }
}
