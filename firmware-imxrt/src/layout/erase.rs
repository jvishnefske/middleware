//! 4 KiB-sector views of block-erased flash, for the stores shared with
//! the ESP base firmware.
//!
//! `esp_base::trust_store` and `esp_base::store` erase 4 KiB sectors; the
//! HyperFlash erases 256 KiB blocks ([`BLOCK`]). Erasing a sector inside a
//! block would mean saving and rewriting the rest of the block, which is
//! neither bounded in RAM nor safe across a power cut, so the views do not
//! do that. Each one instead lays its store out so every erase the store
//! makes is a whole number of blocks:
//!
//! * [`SectorPerBlock`] spreads the logical sectors: logical sector `i` is
//!   the first 4 KiB of block `i` of the region (the rest of the block is
//!   unused). Erasing logical sector `i` erases block `i`. The trust
//!   store's two record sectors become two blocks.
//! * [`BlockErase`] maps offsets one to one and refuses any erase that
//!   does not start on a block boundary; an accepted erase covers every
//!   block the range touches. The payload store's erases start at a
//!   payload slot's start, so with one block per payload slot each erase
//!   is that slot's block.
//!
//! Both are windows: logical offset 0 is the region's start, so the stores
//! are opened at base 0. Reads and writes are passed through (split at
//! logical sector boundaries by [`SectorPerBlock`]); the flash's read and
//! write granules are the view's. Every loop is bounded by the length of
//! the request.

use super::{BLOCK, Region, SECTOR};
use embedded_storage::nor_flash::{
    ErrorType, NorFlash, NorFlashError, NorFlashErrorKind, ReadNorFlash,
};

/// Why a view refused or failed an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewError<E> {
    /// The flash driver failed.
    Flash(E),
    /// Outside the view.
    OutOfBounds,
    /// An erase that is not whole sectors ([`SectorPerBlock`]) or does not
    /// start on a block ([`BlockErase`]); or the flash's erase size is not
    /// [`BLOCK`].
    NotAligned,
}

impl<E: NorFlashError> NorFlashError for ViewError<E> {
    fn kind(&self) -> NorFlashErrorKind {
        match self {
            ViewError::Flash(e) => e.kind(),
            ViewError::OutOfBounds => NorFlashErrorKind::OutOfBounds,
            ViewError::NotAligned => NorFlashErrorKind::NotAligned,
        }
    }
}

/// The physical end of `[at, at + len)`, or `OutOfBounds`.
fn span<E>(at: u32, len: usize, size: u32) -> Result<u32, ViewError<E>> {
    let len = u32::try_from(len).map_err(|_| ViewError::OutOfBounds)?;
    match at.checked_add(len) {
        Some(end) if end <= size => Ok(end),
        _ => Err(ViewError::OutOfBounds),
    }
}

/// Logical 4 KiB sector `i` is the start of block `i` of the region. See
/// the module docs.
#[derive(Debug)]
pub struct SectorPerBlock<F> {
    flash: F,
    region: Region,
}

impl<F: NorFlash> SectorPerBlock<F> {
    /// The view of `region` (whole blocks) on `flash`, whose erase size
    /// must be [`BLOCK`].
    pub fn new(flash: F, region: Region) -> Result<Self, ViewError<F::Error>> {
        if F::ERASE_SIZE != BLOCK as usize || !region.is_aligned() {
            return Err(ViewError::NotAligned);
        }
        Ok(SectorPerBlock { flash, region })
    }

    /// The flash back.
    pub fn into_inner(self) -> F {
        self.flash
    }

    /// The logical size: one sector per block.
    pub const fn size(&self) -> u32 {
        self.region.block_count() * SECTOR
    }

    /// The physical offset of logical `at`.
    const fn phys(&self, at: u32) -> u32 {
        self.region.offset + (at / SECTOR) * BLOCK + at % SECTOR
    }

    /// Splits `[at, at + len)` at logical sector boundaries, calling `op`
    /// with the physical offset and the range of the buffer.
    fn split(
        &mut self,
        at: u32,
        len: usize,
        mut op: impl FnMut(&mut F, u32, core::ops::Range<usize>) -> Result<(), F::Error>,
    ) -> Result<(), ViewError<F::Error>> {
        span(at, len, self.size())?;
        let mut done = 0usize;
        // Bounded: one step per logical sector touched.
        while done < len {
            let here = at + done as u32;
            let room = (SECTOR - here % SECTOR) as usize;
            let n = room.min(len - done);
            let phys = self.phys(here);
            op(&mut self.flash, phys, done..done + n).map_err(ViewError::Flash)?;
            done += n;
        }
        Ok(())
    }
}

impl<F: NorFlash> ErrorType for SectorPerBlock<F> {
    type Error = ViewError<F::Error>;
}

impl<F: NorFlash> ReadNorFlash for SectorPerBlock<F> {
    const READ_SIZE: usize = F::READ_SIZE;

    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
        self.split(offset, bytes.len(), |f, at, r| match bytes.get_mut(r) {
            Some(b) => f.read(at, b),
            None => Ok(()),
        })
    }

    fn capacity(&self) -> usize {
        self.size() as usize
    }
}

impl<F: NorFlash> NorFlash for SectorPerBlock<F> {
    const WRITE_SIZE: usize = F::WRITE_SIZE;
    const ERASE_SIZE: usize = SECTOR as usize;

    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
        self.split(offset, bytes.len(), |f, at, r| match bytes.get(r) {
            Some(b) => f.write(at, b),
            None => Ok(()),
        })
    }

    /// Erases the block of every logical sector in `[from, to)`.
    fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
        if !from.is_multiple_of(SECTOR) || !to.is_multiple_of(SECTOR) || from > to {
            return Err(ViewError::NotAligned);
        }
        if to > self.size() {
            return Err(ViewError::OutOfBounds);
        }
        // Bounded: one block per logical sector, at most the region's.
        for sector in from / SECTOR..to / SECTOR {
            let at = self.region.offset + sector * BLOCK;
            self.flash.erase(at, at + BLOCK).map_err(ViewError::Flash)?;
        }
        Ok(())
    }
}

/// Offsets one to one; erases must start on a block. See the module docs.
#[derive(Debug)]
pub struct BlockErase<F> {
    flash: F,
    region: Region,
}

impl<F: NorFlash> BlockErase<F> {
    /// The view of `region` (whole blocks) on `flash`, whose erase size
    /// must be [`BLOCK`].
    pub fn new(flash: F, region: Region) -> Result<Self, ViewError<F::Error>> {
        if F::ERASE_SIZE != BLOCK as usize || !region.is_aligned() {
            return Err(ViewError::NotAligned);
        }
        Ok(BlockErase { flash, region })
    }

    /// The flash back.
    pub fn into_inner(self) -> F {
        self.flash
    }
}

impl<F: NorFlash> ErrorType for BlockErase<F> {
    type Error = ViewError<F::Error>;
}

impl<F: NorFlash> ReadNorFlash for BlockErase<F> {
    const READ_SIZE: usize = F::READ_SIZE;

    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
        span(offset, bytes.len(), self.region.size)?;
        self.flash
            .read(self.region.offset + offset, bytes)
            .map_err(ViewError::Flash)
    }

    fn capacity(&self) -> usize {
        self.region.size as usize
    }
}

impl<F: NorFlash> NorFlash for BlockErase<F> {
    const WRITE_SIZE: usize = F::WRITE_SIZE;
    const ERASE_SIZE: usize = SECTOR as usize;

    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
        span(offset, bytes.len(), self.region.size)?;
        self.flash
            .write(self.region.offset + offset, bytes)
            .map_err(ViewError::Flash)
    }

    /// Erases every block `[from, to)` touches; `from` must be a block
    /// boundary, so no data before it is lost.
    fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
        if !from.is_multiple_of(BLOCK) || !to.is_multiple_of(SECTOR) || from > to {
            return Err(ViewError::NotAligned);
        }
        if to > self.region.size {
            return Err(ViewError::OutOfBounds);
        }
        let end = to.div_ceil(BLOCK) * BLOCK;
        if from == end {
            return Ok(());
        }
        self.flash
            .erase(self.region.offset + from, self.region.offset + end)
            .map_err(ViewError::Flash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ab::test_flash::BlockFlash;
    use crate::layout::{PAYLOAD_CAPACITY, RT1050_EVKB};
    use esp_base::store::PayloadStore;
    use esp_base::trust_store::{PARTITION_LEN, Persisted, TrustStore};
    use middleware::loader::trust::{PublicKey, TrustState, Version};

    #[test]
    fn rt3_sector_per_block_spreads_sectors_and_erases_one_block_each() {
        let mut flash = BlockFlash::erased(RT1050_EVKB.payload.end() as usize);
        let trust = RT1050_EVKB.trust;
        {
            let mut v = SectorPerBlock::new(&mut flash, trust).unwrap();
            assert_eq!(v.capacity(), PARTITION_LEN as usize);
            // A write across the logical sector boundary lands in both
            // blocks.
            v.write(SECTOR - 4, &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
            let mut back = [0u8; 8];
            v.read(SECTOR - 4, &mut back).unwrap();
            assert_eq!(back, [1, 2, 3, 4, 5, 6, 7, 8]);
            assert_eq!(v.erase(0, 100), Err(ViewError::NotAligned));
            assert_eq!(v.erase(0, 3 * SECTOR), Err(ViewError::OutOfBounds));
            assert_eq!(
                v.read(2 * SECTOR - 4, &mut back),
                Err(ViewError::OutOfBounds)
            );
        }
        let o = trust.offset as usize;
        let b = BLOCK as usize;
        let s = SECTOR as usize;
        assert_eq!(flash.bytes(o + s - 4, 4), [1, 2, 3, 4]);
        assert_eq!(flash.bytes(o + b, 4), [5, 6, 7, 8]);
        let erases = flash.erases;
        {
            let mut v = SectorPerBlock::new(&mut flash, trust).unwrap();
            v.erase(SECTOR, 2 * SECTOR).unwrap();
        }
        assert_eq!(flash.erases, erases + 1, "one block");
        assert_eq!(flash.bytes(o + s - 4, 4), [1, 2, 3, 4], "block 0 kept");
        assert_eq!(flash.bytes(o + b, 4), [0xFF; 4]);
    }

    #[test]
    fn rt3_the_trust_store_saves_and_reopens_through_its_view() {
        let mut flash = BlockFlash::erased(RT1050_EVKB.payload.end() as usize);
        let key = PublicKey([7; 32]);
        let mut state = Persisted::default();
        for n in 1..=3u32 {
            let mut v = SectorPerBlock::new(&mut flash, RT1050_EVKB.trust).unwrap();
            let (mut store, now) = TrustStore::open(&mut v, 0, PARTITION_LEN).unwrap();
            assert_eq!(now, state);
            state = Persisted {
                trust: TrustState::Claimed {
                    key,
                    min_version: Version(n),
                },
                ..state
            };
            store.save(&mut v, &state).unwrap();
        }
        // Nothing outside the trust region was touched.
        let t = RT1050_EVKB.trust;
        let size = RT1050_EVKB.payload.end() as usize;
        let outside = flash
            .bytes(0, t.offset as usize)
            .into_iter()
            .chain(flash.bytes(t.end() as usize, size - t.end() as usize));
        assert!(outside.into_iter().all(|b| b == 0xFF));
    }

    #[test]
    fn rt3_block_erase_refuses_mid_block_erases_and_the_payload_store_works_through_it() {
        let mut flash = BlockFlash::erased(RT1050_EVKB.payload.end() as usize);
        let region = RT1050_EVKB.payload;
        {
            let mut v = BlockErase::new(&mut flash, region).unwrap();
            assert_eq!(v.erase(SECTOR, 2 * SECTOR), Err(ViewError::NotAligned));
            assert_eq!(v.erase(0, 3 * BLOCK), Err(ViewError::OutOfBounds));
            assert_eq!(v.erase(BLOCK, BLOCK), Ok(()));
        }
        assert_eq!(flash.erases, 0);
        let images: [&[u8]; 3] = [&[0x11; 1000], &[0x22; 70_000], &[0x33; 124 * 1024]];
        for image in images {
            let v = BlockErase::new(&mut flash, region).unwrap();
            let mut store = PayloadStore::new(v, 0, region.size, PAYLOAD_CAPACITY).unwrap();
            assert_eq!(store.capacity(), PAYLOAD_CAPACITY);
            let before = store.header();
            store.erase(image.len() as u32).unwrap();
            assert_eq!(store.header(), before, "the active slot is untouched");
            store.write(0, image).unwrap();
            store
                .activate(image.len() as u32, middleware::crc::Crc32::checksum(image))
                .unwrap();
            assert_eq!(store.header().map(|h| h.len), Some(image.len() as u32));
        }
        // Reopened: the last image is the active one.
        {
            let v = BlockErase::new(&mut flash, region).unwrap();
            let mut store = PayloadStore::new(v, 0, region.size, PAYLOAD_CAPACITY).unwrap();
            assert_eq!(store.header().map(|h| h.len), Some(124 * 1024));
            assert_eq!(store.verify().map(|h| h.len), Ok(124 * 1024));
        }
        assert_eq!(flash.erases, 3, "one block per transfer");
    }

    #[test]
    fn rt3_views_need_the_block_erase_size() {
        let mut flash = crate::ab::test_flash::SmallEraseFlash;
        assert!(SectorPerBlock::new(&mut flash, RT1050_EVKB.trust).is_err());
        assert!(BlockErase::new(&mut flash, RT1050_EVKB.payload).is_err());
    }
}
