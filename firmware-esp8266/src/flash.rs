//! NOR flash driver for the payload region (requirement E9).
//!
//! The ESP8266 HAL exposes raw SPI flash operations on absolute flash
//! addresses (`esp8266_hal::flash::ESPFlash`). This module puts a small seam
//! over them, [`RawFlash`], and implements the `embedded-storage` 0.3
//! [`NorFlash`] / [`ReadNorFlash`] traits on top in [`RegionFlash`], so that
//! `esp_base::store::PayloadStore` can run on the chip unchanged.
//!
//! [`RegionFlash`] confines every access to one fixed region:
//!
//! | constant          | value      | meaning                                  |
//! |-------------------|------------|------------------------------------------|
//! | [`REGION_BASE`]   | `0x100000` | absolute flash address of the region     |
//! | [`REGION_SIZE`]   | `0x40000`  | 256 KiB: header sector + 252 KiB image   |
//!
//! Offsets passed to the `NorFlash` methods are **region relative**
//! (`0..REGION_SIZE`); the driver adds [`REGION_BASE`]. The store is
//! therefore opened at base `0` over the whole region, and nothing outside
//! `0x100000..0x140000` can be touched (the firmware image occupies
//! `0x0..0x11000`, IROM mapped from `0x10000`).
//!
//! Geometry and rules:
//!
//! * `READ_SIZE` = 1: any offset and length (the HAL reads byte-wise around
//!   an aligned word body).
//! * `WRITE_SIZE` = 4: offset and length must be multiples of 4.
//! * `ERASE_SIZE` = 4096: `from` and `to` must be sector aligned. The HAL's
//!   multi-sector erase is broken (it advances the address by one byte per
//!   sector and only sets write-enable once), so [`RegionFlash`] calls
//!   [`RawFlash::erase_sector`] once per sector.
//! * Writes are split so that no single [`RawFlash::program`] call crosses
//!   a 256-byte page boundary: SPI page program wraps within a page, and the
//!   HAL programs 64-byte runs from wherever the write starts.
//! * Range or alignment violations return [`FlashError`] with the matching
//!   [`NorFlashErrorKind`] (`OutOfBounds`, `NotAligned`) before any device
//!   operation; a failing device operation is `Other`.
//!
//! Every loop is bounded by the request length: at most `REGION_SIZE /
//! PAGE` program calls per write and `REGION_SIZE / SECTOR` erase calls per
//! erase.
//!
//! The decision logic here is pure and host tested against a RAM NOR model
//! (`flash::ram`, test builds only); the HAL implementation of [`RawFlash`] lives
//! in the shell (`main.rs`).

#[cfg(test)]
pub mod ram;

use embedded_storage::nor_flash::{
    ErrorType, NorFlash, NorFlashError, NorFlashErrorKind, ReadNorFlash,
};

/// Absolute flash address of the payload region.
pub const REGION_BASE: u32 = 0x10_0000;

/// Size of the payload region in bytes (256 KiB).
pub const REGION_SIZE: u32 = 0x4_0000;

/// Erase granularity.
pub const SECTOR: u32 = 4096;

/// SPI page-program granularity; one program call stays inside a page.
pub const PAGE: u32 = 256;

/// Write granularity.
pub const WORD: u32 = 4;

/// A raw flash operation failed on the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceError;

/// Raw flash operations on absolute addresses: the seam over the HAL.
///
/// Implementations need no checking; [`RegionFlash`] only calls them with
/// in-region addresses, `program` never crossing a [`PAGE`] boundary, and
/// `erase_sector` with a [`SECTOR`]-aligned address.
pub trait RawFlash {
    /// Reads `buf.len()` bytes starting at `addr`.
    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), DeviceError>;
    /// Programs `data` at `addr` (clears bits; the range lies in one page).
    fn program(&mut self, addr: u32, data: &[u8]) -> Result<(), DeviceError>;
    /// Erases the 4 KiB sector starting at `addr` to `0xFF`.
    fn erase_sector(&mut self, addr: u32) -> Result<(), DeviceError>;
}

/// Why a [`RegionFlash`] operation was refused or failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashError {
    /// Offset or length not a multiple of the write or erase size, or
    /// `from > to`.
    NotAligned,
    /// The range leaves the region.
    OutOfBounds,
    /// The device reported a failure.
    Device,
}

impl NorFlashError for FlashError {
    fn kind(&self) -> NorFlashErrorKind {
        match self {
            FlashError::NotAligned => NorFlashErrorKind::NotAligned,
            FlashError::OutOfBounds => NorFlashErrorKind::OutOfBounds,
            FlashError::Device => NorFlashErrorKind::Other,
        }
    }
}

impl From<DeviceError> for FlashError {
    fn from(_: DeviceError) -> Self {
        FlashError::Device
    }
}

/// `embedded-storage` NOR flash over the fixed payload region of a
/// [`RawFlash`]. Offsets are relative to [`REGION_BASE`].
pub struct RegionFlash<R> {
    raw: R,
}

impl<R: RawFlash> RegionFlash<R> {
    /// Wraps the raw device.
    pub const fn new(raw: R) -> Self {
        RegionFlash { raw }
    }

    /// The raw device.
    pub const fn raw(&self) -> &R {
        &self.raw
    }

    /// The raw device, mutably.
    pub fn raw_mut(&mut self) -> &mut R {
        &mut self.raw
    }

    /// Returns the raw device.
    pub fn into_inner(self) -> R {
        self.raw
    }
}

/// Checks `[offset, offset + len)` lies in the region; returns the end.
fn in_region(offset: u32, len: usize) -> Result<u32, FlashError> {
    let len = u32::try_from(len).map_err(|_| FlashError::OutOfBounds)?;
    let end = offset.checked_add(len).ok_or(FlashError::OutOfBounds)?;
    if end > REGION_SIZE {
        Err(FlashError::OutOfBounds)
    } else {
        Ok(end)
    }
}

/// Bytes from `offset` to the end of its page.
const fn page_room(offset: u32) -> u32 {
    PAGE - offset % PAGE
}

impl<R> ErrorType for RegionFlash<R> {
    type Error = FlashError;
}

impl<R: RawFlash> ReadNorFlash for RegionFlash<R> {
    const READ_SIZE: usize = 1;

    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
        in_region(offset, bytes.len())?;
        if bytes.is_empty() {
            return Ok(());
        }
        Ok(self.raw.read(REGION_BASE + offset, bytes)?)
    }

    fn capacity(&self) -> usize {
        REGION_SIZE as usize
    }
}

impl<R: RawFlash> NorFlash for RegionFlash<R> {
    const WRITE_SIZE: usize = WORD as usize;
    const ERASE_SIZE: usize = SECTOR as usize;

    fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
        if from > to || !from.is_multiple_of(SECTOR) || !to.is_multiple_of(SECTOR) {
            return Err(FlashError::NotAligned);
        }
        if to > REGION_SIZE {
            return Err(FlashError::OutOfBounds);
        }
        // Bounded: at most REGION_SIZE / SECTOR iterations.
        for sector in (from..to).step_by(SECTOR as usize) {
            self.raw.erase_sector(REGION_BASE + sector)?;
        }
        Ok(())
    }

    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
        if !offset.is_multiple_of(WORD) || !bytes.len().is_multiple_of(WORD as usize) {
            return Err(FlashError::NotAligned);
        }
        in_region(offset, bytes.len())?;
        let mut at = offset;
        let mut rest = bytes;
        // Bounded: each iteration consumes at least one byte (page_room >= 1)
        // and at most one page, so it runs at most len / PAGE + 1 times.
        while !rest.is_empty() {
            let n = rest.len().min(page_room(at) as usize);
            let (run, tail) = rest.split_at(n);
            self.raw.program(REGION_BASE + at, run)?;
            at += n as u32;
            rest = tail;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::ram::{Op, RamFlash};
    use super::*;

    fn flash() -> RegionFlash<RamFlash> {
        RegionFlash::new(RamFlash::new())
    }

    #[test]
    fn erased_reads_ff_and_writes_and_like_nor() {
        let mut f = flash();
        f.erase(0, SECTOR).unwrap();
        let mut buf = [0u8; 8];
        f.read(0, &mut buf).unwrap();
        assert_eq!(buf, [0xFF; 8]);
        f.write(0, &[0xF0, 0x0F, 0xAA, 0x55]).unwrap();
        f.write(0, &[0x3C, 0xFF, 0xFF, 0x00]).unwrap();
        f.read(0, &mut buf[..4]).unwrap();
        assert_eq!(&buf[..4], &[0x30, 0x0F, 0xAA, 0x00]);
        // READ_SIZE is 1: odd offsets and lengths are fine.
        let mut one = [0u8; 3];
        f.read(1, &mut one).unwrap();
        assert_eq!(one, [0x0F, 0xAA, 0x00]);
    }

    #[test]
    fn addresses_are_region_relative() {
        let mut f = flash();
        f.erase(SECTOR, 2 * SECTOR).unwrap();
        f.write(SECTOR + 8, &[1, 2, 3, 4]).unwrap();
        let raw = f.into_inner();
        assert_eq!(raw.ops()[0], Op::Erase(REGION_BASE + SECTOR));
        assert_eq!(raw.ops()[1], Op::Program(REGION_BASE + SECTOR + 8, 4));
        assert_eq!(raw.byte(REGION_BASE + SECTOR + 8), 1);
    }

    #[test]
    fn erase_issues_one_call_per_sector() {
        let mut f = flash();
        f.erase(2 * SECTOR, 5 * SECTOR).unwrap();
        f.erase(SECTOR, SECTOR).unwrap();
        let ops = f.into_inner();
        let expected = [2, 3, 4].map(|s| Op::Erase(REGION_BASE + s * SECTOR));
        assert_eq!(ops.ops(), &expected);
    }

    #[test]
    fn whole_region_erases() {
        let mut f = flash();
        f.erase(0, REGION_SIZE).unwrap();
        assert_eq!(f.into_inner().ops().len(), (REGION_SIZE / SECTOR) as usize);
    }

    #[test]
    fn writes_split_at_page_boundaries() {
        let mut f = flash();
        f.erase(0, SECTOR).unwrap();
        let data: [u8; 600] = core::array::from_fn(|i| (i % 251) as u8);
        f.write(PAGE - 8, &data).unwrap();
        let raw = f.into_inner();
        let programs: Vec<Op> = raw
            .ops()
            .iter()
            .copied()
            .filter(|o| matches!(o, Op::Program(..)))
            .collect();
        let base = REGION_BASE;
        assert_eq!(
            programs,
            [
                Op::Program(base + PAGE - 8, 8),
                Op::Program(base + PAGE, 256),
                Op::Program(base + 2 * PAGE, 256),
                Op::Program(base + 3 * PAGE, 80),
            ]
        );
        for (i, &b) in data.iter().enumerate() {
            assert_eq!(raw.byte(base + PAGE - 8 + i as u32), b);
        }
    }

    #[test]
    fn aligned_page_sized_write_is_one_program() {
        let mut f = flash();
        f.erase(0, SECTOR).unwrap();
        f.write(PAGE, &[0u8; 256]).unwrap();
        let raw = f.into_inner();
        assert_eq!(
            raw.ops().last(),
            Some(&Op::Program(REGION_BASE + PAGE, 256))
        );
    }

    #[test]
    fn misaligned_requests_are_refused_without_device_access() {
        let mut f = flash();
        assert_eq!(f.write(2, &[0; 4]), Err(FlashError::NotAligned));
        assert_eq!(f.write(0, &[0; 3]), Err(FlashError::NotAligned));
        assert_eq!(f.erase(1, SECTOR), Err(FlashError::NotAligned));
        assert_eq!(f.erase(0, SECTOR + 4), Err(FlashError::NotAligned));
        assert_eq!(f.erase(2 * SECTOR, SECTOR), Err(FlashError::NotAligned));
        assert_eq!(FlashError::NotAligned.kind(), NorFlashErrorKind::NotAligned);
        assert!(f.into_inner().ops().is_empty());
    }

    #[test]
    fn out_of_region_requests_are_refused() {
        let mut f = flash();
        let mut buf = [0u8; 8];
        assert_eq!(
            f.read(REGION_SIZE - 4, &mut buf),
            Err(FlashError::OutOfBounds)
        );
        assert_eq!(
            f.read(u32::MAX, &mut buf[..1]),
            Err(FlashError::OutOfBounds)
        );
        assert_eq!(f.write(REGION_SIZE, &[0; 4]), Err(FlashError::OutOfBounds));
        assert_eq!(f.write(u32::MAX - 3, &[0; 8]), Err(FlashError::OutOfBounds));
        assert_eq!(
            f.erase(0, REGION_SIZE + SECTOR),
            Err(FlashError::OutOfBounds)
        );
        assert_eq!(
            FlashError::OutOfBounds.kind(),
            NorFlashErrorKind::OutOfBounds
        );
        assert_eq!(f.capacity(), REGION_SIZE as usize);
        // The last word and an empty read at the end are in range.
        f.read(REGION_SIZE - 4, &mut buf[..4]).unwrap();
        f.read(REGION_SIZE, &mut []).unwrap();
        assert!(
            f.into_inner()
                .ops()
                .iter()
                .all(|o| matches!(o, Op::Read(..)))
        );
    }

    #[test]
    fn device_failures_surface_as_other() {
        let mut f = RegionFlash::new(RamFlash::failing());
        assert_eq!(f.erase(0, SECTOR), Err(FlashError::Device));
        assert_eq!(f.write(0, &[0; 4]), Err(FlashError::Device));
        assert_eq!(f.read(0, &mut [0; 4]), Err(FlashError::Device));
        assert_eq!(FlashError::Device.kind(), NorFlashErrorKind::Other);
    }
}
