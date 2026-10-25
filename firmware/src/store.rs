//! Payload storage over any NOR flash.
//!
//! [`PayloadStore`] performs the loader's storage effects on the payload
//! partition, following the layout in [`crate::image`]: erase the header and
//! the image sectors, write chunks through a [`WordWriter`], and on activate
//! flush the last partial word and write the [`Header`]. It is generic over
//! `embedded_storage::nor_flash::NorFlash`, so the same code runs on
//! `esp_storage::FlashStorage` and on a RAM flash model in the unit tests.
//!
//! Reads respect a 4-byte read granularity (esp-storage's default): the
//! aligned prefix is read directly and a trailing partial word through a
//! small buffer.

use crate::image::{HEADER_LEN, Header, OutOfOrder, Region, SECTOR, WORD, WordWriter};
use embedded_storage::nor_flash::NorFlash;
use middleware::crc::Crc32;

/// Bytes read per step while verifying an image.
const VERIFY_STEP: usize = 256;

/// Why a storage operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreError {
    /// The flash driver reported an error.
    Flash,
    /// The image does not fit the partition.
    TooLarge,
    /// A write did not continue where the previous one ended.
    OutOfOrder,
    /// No valid header is present.
    NoImage,
    /// The image bytes do not match the header's CRC-32.
    Crc,
    /// The caller's buffer does not match the image length.
    Buffer,
}

impl From<OutOfOrder> for StoreError {
    fn from(_: OutOfOrder) -> Self {
        StoreError::OutOfOrder
    }
}

impl core::fmt::Display for StoreError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            StoreError::Flash => "flash error",
            StoreError::TooLarge => "image too large",
            StoreError::OutOfOrder => "write out of order",
            StoreError::NoImage => "no image installed",
            StoreError::Crc => "image CRC mismatch",
            StoreError::Buffer => "buffer size mismatch",
        };
        f.write_str(s)
    }
}

/// The payload partition on a flash device.
pub struct PayloadStore<F> {
    flash: F,
    base: u32,
    region: Region,
    limit: u32,
    writer: WordWriter,
}

impl<F: NorFlash> PayloadStore<F> {
    /// A store for the partition at absolute flash offset `base` (sector
    /// aligned) of `size` bytes. Images are limited to `limit` bytes as well
    /// as the partition capacity.
    pub fn new(flash: F, base: u32, size: u32, limit: u32) -> Result<Self, StoreError> {
        if !base.is_multiple_of(SECTOR) {
            return Err(StoreError::Flash);
        }
        Ok(PayloadStore { flash, base, region: Region::new(size), limit, writer: WordWriter::new() })
    }

    /// Largest image accepted.
    pub fn capacity(&self) -> u32 {
        self.region.capacity().min(self.limit)
    }

    /// Prepares for an image of `len` bytes: erases the header and image
    /// sectors.
    pub fn erase(&mut self, len: u32) -> Result<(), StoreError> {
        if len > self.capacity() {
            return Err(StoreError::TooLarge);
        }
        let (from, to) = self.region.erase_span(len).ok_or(StoreError::TooLarge)?;
        self.writer = WordWriter::new();
        self.flash.erase(self.base + from, self.base + to).map_err(|_| StoreError::Flash)
    }

    /// Writes image bytes at `offset` (in order).
    pub fn write(&mut self, offset: u32, data: &[u8]) -> Result<(), StoreError> {
        let end = offset.checked_add(data.len() as u32).ok_or(StoreError::TooLarge)?;
        if end > self.capacity() {
            return Err(StoreError::TooLarge);
        }
        let (next, plan) = self.writer.push(offset, data)?;
        let at = self.base + self.region.data_offset();
        if let Some((off, word)) = plan.head {
            self.flash.write(at + off, &word).map_err(|_| StoreError::Flash)?;
        }
        if let Some((off, bytes)) = plan.body {
            self.flash.write(at + off, bytes).map_err(|_| StoreError::Flash)?;
        }
        self.writer = next;
        Ok(())
    }

    /// Completes the image: flushes the last partial word and writes the
    /// header. `len` must equal the bytes written.
    pub fn activate(&mut self, len: u32, crc: u32) -> Result<(), StoreError> {
        if self.writer.position() != len {
            return Err(StoreError::OutOfOrder);
        }
        if let Some((off, word)) = self.writer.finish() {
            let at = self.base + self.region.data_offset();
            self.flash.write(at + off, &word).map_err(|_| StoreError::Flash)?;
        }
        self.writer = WordWriter::new();
        let header = Header { len, crc }.encode();
        let at = self.base + self.region.header_offset();
        self.flash.write(at, &header).map_err(|_| StoreError::Flash)
    }

    /// The installed image's header, if a valid one is present.
    pub fn header(&mut self) -> Option<Header> {
        let mut buf = [0u8; HEADER_LEN];
        let at = self.base + self.region.header_offset();
        self.flash.read(at, &mut buf).ok()?;
        Header::decode(&buf, self.capacity())
    }

    /// Reads image bytes `[offset, offset + buf.len())`, any length.
    fn read_image(&mut self, offset: u32, buf: &mut [u8]) -> Result<(), StoreError> {
        let at = self.base + self.region.data_offset() + offset;
        let word = WORD as usize;
        let aligned = buf.len() - buf.len() % word;
        let (body, tail) = buf.split_at_mut(aligned);
        if !body.is_empty() {
            self.flash.read(at, body).map_err(|_| StoreError::Flash)?;
        }
        if !tail.is_empty() {
            let mut w = [0u8; WORD as usize];
            self.flash.read(at + aligned as u32, &mut w).map_err(|_| StoreError::Flash)?;
            for (dst, src) in tail.iter_mut().zip(w) {
                *dst = src;
            }
        }
        Ok(())
    }

    /// Checks the installed image against its header CRC without loading it.
    pub fn verify(&mut self) -> Result<Header, StoreError> {
        let header = self.header().ok_or(StoreError::NoImage)?;
        let mut crc = Crc32::new();
        let mut buf = [0u8; VERIFY_STEP];
        let mut at = 0u32;
        // Bounded: the header length is at most the partition capacity.
        while at < header.len {
            let n = (header.len - at).min(VERIFY_STEP as u32);
            let part = buf.get_mut(..n as usize).ok_or(StoreError::Buffer)?;
            self.read_image(at, part)?;
            crc = crc.update(part);
            at += n;
        }
        if crc.finish() == header.crc { Ok(header) } else { Err(StoreError::Crc) }
    }

    /// Reads the installed image described by `header` into `buf` (exactly
    /// `header.len` bytes) and checks its CRC.
    pub fn load(&mut self, header: Header, buf: &mut [u8]) -> Result<(), StoreError> {
        if buf.len() != header.len as usize {
            return Err(StoreError::Buffer);
        }
        self.read_image(0, buf)?;
        if Crc32::checksum(buf) == header.crc { Ok(()) } else { Err(StoreError::Crc) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embedded_storage::nor_flash::{ErrorType, NorFlashErrorKind, ReadNorFlash};

    const SIZE: usize = 8 * SECTOR as usize;
    const BASE: u32 = 2 * SECTOR;

    /// RAM NOR flash: 4-byte reads/writes, 4 KiB erases, writes only clear
    /// bits.
    struct RamFlash {
        mem: [u8; SIZE + BASE as usize],
        erases: usize,
    }

    impl RamFlash {
        fn new() -> Self {
            RamFlash { mem: [0x00; SIZE + BASE as usize], erases: 0 }
        }
    }

    impl ErrorType for RamFlash {
        type Error = NorFlashErrorKind;
    }

    impl ReadNorFlash for RamFlash {
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

    impl NorFlash for RamFlash {
        const WRITE_SIZE: usize = 4;
        const ERASE_SIZE: usize = SECTOR as usize;
        fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
            if !offset.is_multiple_of(4) || !bytes.len().is_multiple_of(4) {
                return Err(NorFlashErrorKind::NotAligned);
            }
            let dst = self.mem.get_mut(offset as usize..offset as usize + bytes.len());
            for (d, s) in dst.ok_or(NorFlashErrorKind::OutOfBounds)?.iter_mut().zip(bytes) {
                *d &= *s;
            }
            Ok(())
        }
        fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
            if !from.is_multiple_of(SECTOR) || !to.is_multiple_of(SECTOR) {
                return Err(NorFlashErrorKind::NotAligned);
            }
            self.mem.get_mut(from as usize..to as usize).ok_or(NorFlashErrorKind::OutOfBounds)?.fill(0xFF);
            self.erases += 1;
            Ok(())
        }
    }

    fn install(store: &mut PayloadStore<RamFlash>, image: &[u8], chunk: usize) {
        store.erase(image.len() as u32).unwrap();
        for (i, part) in image.chunks(chunk).enumerate() {
            store.write((i * chunk) as u32, part).unwrap();
        }
        store.activate(image.len() as u32, Crc32::checksum(image)).unwrap();
    }

    #[test]
    fn installed_image_survives_reopen_and_verifies() {
        let image: [u8; 5003] = core::array::from_fn(|i| (i * 31 % 251) as u8);
        let mut store = PayloadStore::new(RamFlash::new(), BASE, SIZE as u32, u32::MAX).unwrap();
        assert_eq!(store.header(), None, "zeroed flash has no image");
        install(&mut store, &image, 7);
        // A "reboot": a new store over the same flash contents.
        let flash = store.flash;
        let mut store = PayloadStore::new(flash, BASE, SIZE as u32, u32::MAX).unwrap();
        let header = store.verify().unwrap();
        assert_eq!(header.len, 5003);
        let mut buf = [0u8; 5003];
        store.load(header, &mut buf).unwrap();
        assert_eq!(buf, image);
        assert!(store.flash.mem[..BASE as usize].iter().all(|&b| b == 0), "outside partition untouched");
    }

    #[test]
    fn interrupted_transfer_leaves_no_image() {
        let mut store = PayloadStore::new(RamFlash::new(), BASE, SIZE as u32, u32::MAX).unwrap();
        install(&mut store, &[1, 2, 3, 4, 5], 2);
        assert!(store.header().is_some());
        store.erase(100).unwrap();
        store.write(0, &[9; 50]).unwrap();
        assert_eq!(store.header(), None);
        assert_eq!(store.verify(), Err(StoreError::NoImage));
        assert_eq!(store.activate(100, 0), Err(StoreError::OutOfOrder));
    }

    #[test]
    fn corruption_is_detected() {
        let mut store = PayloadStore::new(RamFlash::new(), BASE, SIZE as u32, u32::MAX).unwrap();
        install(&mut store, &[7u8; 64], 64);
        store.flash.mem[(BASE + SECTOR + 10) as usize] = 0;
        assert_eq!(store.verify(), Err(StoreError::Crc));
        let header = store.header().unwrap();
        let mut buf = [0u8; 64];
        assert_eq!(store.load(header, &mut buf), Err(StoreError::Crc));
        assert_eq!(store.load(header, &mut buf[..63]), Err(StoreError::Buffer));
    }

    #[test]
    fn limits_and_ordering_are_enforced() {
        let mut store = PayloadStore::new(RamFlash::new(), BASE, SIZE as u32, 1000).unwrap();
        assert_eq!(store.capacity(), 1000);
        assert_eq!(store.erase(1001), Err(StoreError::TooLarge));
        store.erase(1000).unwrap();
        assert_eq!(store.flash.erases, 1);
        assert_eq!(store.write(4, &[0]), Err(StoreError::OutOfOrder));
        assert_eq!(store.write(999, &[0, 0]), Err(StoreError::TooLarge));
        assert!(PayloadStore::new(RamFlash::new(), 3, SIZE as u32, 10).is_err());
        let full = PayloadStore::new(RamFlash::new(), BASE, SIZE as u32, u32::MAX).unwrap();
        assert_eq!(full.capacity(), SIZE as u32 - SECTOR);
    }
}
