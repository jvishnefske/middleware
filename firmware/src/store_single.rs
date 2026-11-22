//! Single-copy payload storage over any NOR flash.
//!
//! [`SingleStore`] keeps one payload in one region, with no second slot and
//! no generations. The region starts with the [`Header`] (see
//! [`crate::image`]) and the module bytes follow it at [`HEADER_LEN`]:
//!
//! * [`SingleStore::new`] reads the header. A header that does not decode,
//!   or whose module does not match its CRC-32, means the store is empty.
//! * [`SingleStore::erase`] invalidates the store and erases the header
//!   sector first, then the module sectors the new payload covers. After
//!   it there is no valid payload, so there is no fallback to an old one.
//! * [`SingleStore::write`] writes module bytes in order through a
//!   [`WordWriter`].
//! * [`SingleStore::commit`] flushes the last partial word, checks the
//!   module against the expected CRC-32 (writing no header on a mismatch),
//!   then writes the header fields and last of all the magic word, which
//!   sits at offset 0 of [`Header::encode`]. A power cut before that final
//!   word leaves no valid header, so the store is empty and never a partial
//!   module.
//!
//! Reads respect a 4-byte read granularity as in [`crate::store`].

use crate::image::{Generation, HEADER_LEN, Header, SECTOR, WORD, WordWriter};
use crate::store::StoreError;
use embedded_storage::nor_flash::NorFlash;
use middleware::crc::Crc32;

/// Bytes read per step while verifying a module.
const VERIFY_STEP: usize = 256;

/// [`HEADER_LEN`] as a flash offset.
const HEADER_LEN_U32: u32 = HEADER_LEN as u32;

/// A single payload region on a flash device.
pub struct SingleStore<F> {
    flash: F,
    base: u32,
    size: u32,
    /// The verified header of the stored payload, if there is one.
    header: Option<Header>,
    /// A transfer in progress: the next expected module offset.
    transfer: Option<WordWriter>,
}

impl<F: NorFlash> SingleStore<F> {
    /// A store for the region at absolute flash offset `base` (sector
    /// aligned) of `size` bytes (a whole number of sectors, at least one).
    /// Scans the header; an undecodable or CRC-mismatched payload counts as
    /// empty.
    pub fn new(flash: F, base: u32, size: u32) -> Result<Self, StoreError> {
        if !base.is_multiple_of(SECTOR)
            || size < SECTOR
            || !size.is_multiple_of(SECTOR)
            || base.checked_add(size).is_none()
        {
            return Err(StoreError::Flash);
        }
        let mut store = SingleStore {
            flash,
            base,
            size,
            header: None,
            transfer: None,
        };
        store.header = store.scan();
        Ok(store)
    }

    /// Largest module accepted: the region less the header.
    pub fn capacity(&self) -> u32 {
        self.size.saturating_sub(HEADER_LEN_U32)
    }

    /// The verified header of the stored payload, if there is one.
    pub fn header(&self) -> Option<Header> {
        self.header
    }

    /// Absolute flash offset of module byte 0.
    fn module_addr(&self) -> u32 {
        self.base + HEADER_LEN_U32
    }

    /// The header at boot, if it decodes and its module matches its CRC.
    fn scan(&mut self) -> Option<Header> {
        let mut buf = [0u8; HEADER_LEN];
        self.flash.read(self.base, &mut buf).ok()?;
        let header = Header::decode(&buf, self.capacity())?;
        (self.crc_of(header.len).ok()? == header.crc).then_some(header)
    }

    /// Starts a transfer of a module of `len` bytes: invalidates the store,
    /// then erases the header sector and any module sectors beyond it.
    /// Any earlier transfer is dropped.
    pub fn erase(&mut self, len: u32) -> Result<(), StoreError> {
        self.transfer = None;
        self.header = None;
        if len > self.capacity() {
            return Err(StoreError::TooLarge);
        }
        // The header sector holds the first module bytes too, so this
        // erase covers everything before the module sectors.
        let sectors = (HEADER_LEN_U32 + len).div_ceil(SECTOR);
        self.flash
            .erase(self.base, self.base + SECTOR)
            .map_err(|_| StoreError::Flash)?;
        if sectors > 1 {
            self.flash
                .erase(self.base + SECTOR, self.base + sectors * SECTOR)
                .map_err(|_| StoreError::Flash)?;
        }
        self.transfer = Some(WordWriter::new());
        Ok(())
    }

    /// Writes module bytes at `offset` (in order) into the transfer.
    pub fn write(&mut self, offset: u32, data: &[u8]) -> Result<(), StoreError> {
        let writer = self.transfer.ok_or(StoreError::OutOfOrder)?;
        let len = u32::try_from(data.len()).map_err(|_| StoreError::TooLarge)?;
        let end = offset.checked_add(len).ok_or(StoreError::TooLarge)?;
        if end > self.capacity() {
            return Err(StoreError::TooLarge);
        }
        let (next, plan) = writer.push(offset, data)?;
        let at = self.module_addr();
        if let Some((off, word)) = plan.head {
            self.flash
                .write(at + off, &word)
                .map_err(|_| StoreError::Flash)?;
        }
        if let Some((off, bytes)) = plan.body {
            self.flash
                .write(at + off, bytes)
                .map_err(|_| StoreError::Flash)?;
        }
        self.transfer = Some(next);
        Ok(())
    }

    /// Completes the transfer: flushes the last partial word, checks the
    /// first `len` module bytes against `crc`, and writes the header. On a
    /// CRC mismatch no header is written and the store stays empty. The
    /// header fields are written first and the magic word last, so the
    /// payload is valid only once every word is on flash. `len` must equal
    /// the bytes written and be non-zero. The transfer ends whatever the
    /// result.
    pub fn commit(&mut self, len: u32, crc: u32) -> Result<(), StoreError> {
        let writer = self.transfer.take().ok_or(StoreError::OutOfOrder)?;
        if writer.position() != len || len == 0 {
            return Err(StoreError::OutOfOrder);
        }
        if let Some((off, word)) = writer.finish() {
            self.flash
                .write(self.module_addr() + off, &word)
                .map_err(|_| StoreError::Flash)?;
        }
        if self.crc_of(len)? != crc {
            return Err(StoreError::Crc);
        }
        let header = Header {
            len,
            crc,
            generation: Generation::FIRST,
        };
        // `encode` puts the magic in the first word, so it is written last.
        let bytes = header.encode();
        let (magic, fields) = bytes.split_at(WORD as usize);
        self.flash
            .write(self.base + WORD, fields)
            .map_err(|_| StoreError::Flash)?;
        self.flash
            .write(self.base, magic)
            .map_err(|_| StoreError::Flash)?;
        self.header = Some(header);
        Ok(())
    }

    /// Checks the stored module against its header CRC without loading it.
    pub fn verify(&mut self) -> Result<Header, StoreError> {
        let header = self.header.ok_or(StoreError::NoImage)?;
        if self.crc_of(header.len)? == header.crc {
            Ok(header)
        } else {
            Err(StoreError::Crc)
        }
    }

    /// Reads the stored module described by `header` into `buf` (exactly
    /// `header.len` bytes) and checks its CRC.
    pub fn load(&mut self, header: Header, buf: &mut [u8]) -> Result<(), StoreError> {
        if self.header.is_none() {
            return Err(StoreError::NoImage);
        }
        if header.len > self.capacity() || buf.len() != header.len as usize {
            return Err(StoreError::Buffer);
        }
        self.read_module(0, buf)?;
        if Crc32::checksum(buf) == header.crc {
            Ok(())
        } else {
            Err(StoreError::Crc)
        }
    }

    /// The flash device, for other partitions on it. Callers must not write
    /// inside this store's region.
    pub fn flash(&mut self) -> &mut F {
        &mut self.flash
    }

    /// Reads module bytes `[offset, offset + buf.len())`, any length.
    fn read_module(&mut self, offset: u32, buf: &mut [u8]) -> Result<(), StoreError> {
        let at = self.module_addr() + offset;
        let word = WORD as usize;
        let aligned = buf.len() - buf.len() % word;
        let (body, tail) = buf.split_at_mut(aligned);
        if !body.is_empty() {
            self.flash.read(at, body).map_err(|_| StoreError::Flash)?;
        }
        if !tail.is_empty() {
            let mut w = [0u8; WORD as usize];
            self.flash
                .read(at + aligned as u32, &mut w)
                .map_err(|_| StoreError::Flash)?;
            for (dst, src) in tail.iter_mut().zip(w) {
                *dst = src;
            }
        }
        Ok(())
    }

    /// CRC-32 of the first `len` module bytes, read in bounded steps.
    fn crc_of(&mut self, len: u32) -> Result<u32, StoreError> {
        let mut crc = Crc32::new();
        let mut buf = [0u8; VERIFY_STEP];
        let mut at = 0u32;
        // Bounded: callers pass a length within the capacity.
        while at < len {
            let n = (len - at).min(VERIFY_STEP as u32);
            let part = buf.get_mut(..n as usize).ok_or(StoreError::Buffer)?;
            self.read_module(at, part)?;
            crc = crc.update(part);
            at += n;
        }
        Ok(crc.finish())
    }
}

#[cfg(test)]
mod tests {
    // Host-only test code: std collections for test images.
    extern crate std;

    use super::*;
    use crate::image::MAGIC;
    use std::{vec, vec::Vec};

    const SIZE: usize = 8 * SECTOR as usize;
    const BASE: u32 = 2 * SECTOR;
    const CAPACITY: u32 = SIZE as u32 - HEADER_LEN as u32;

    /// The RAM flash model, sized for this region plus the bytes before it.
    type RamFlash = crate::ram_flash::RamFlash<{ SIZE + BASE as usize }>;

    fn open(flash: RamFlash) -> SingleStore<RamFlash> {
        SingleStore::new(flash, BASE, SIZE as u32).unwrap()
    }

    /// A "reboot": a new store over the same flash contents.
    fn reboot(store: SingleStore<RamFlash>) -> SingleStore<RamFlash> {
        open(store.flash)
    }

    fn try_install(
        store: &mut SingleStore<RamFlash>,
        image: &[u8],
        chunk: usize,
    ) -> Result<(), StoreError> {
        let len = image.len() as u32;
        store.erase(len)?;
        for (i, part) in image.chunks(chunk).enumerate() {
            store.write((i * chunk) as u32, part)?;
        }
        store.commit(len, Crc32::checksum(image))
    }

    fn install(store: &mut SingleStore<RamFlash>, image: &[u8], chunk: usize) {
        try_install(store, image, chunk).unwrap();
    }

    /// The stored bytes, after verifying them.
    fn contents(store: &mut SingleStore<RamFlash>) -> Option<Vec<u8>> {
        let header = store.verify().ok()?;
        let mut buf = vec![0u8; header.len as usize];
        store.load(header, &mut buf).unwrap();
        Some(buf)
    }

    fn pattern(len: usize, seed: usize) -> Vec<u8> {
        (0..len).map(|i| ((i + seed) * 31 % 251) as u8).collect()
    }

    #[test]
    fn installed_payload_survives_reopen_and_loads_exactly() {
        let image = pattern(5003, 1);
        let mut store = open(RamFlash::new());
        assert_eq!(store.header(), None, "zeroed flash has no payload");
        assert_eq!(store.capacity(), CAPACITY);
        install(&mut store, &image, 7);
        assert_eq!(
            store.header().map(|h| (h.len, h.generation)),
            Some((5003, Generation::FIRST))
        );
        let mut store = reboot(store);
        let header = store.verify().unwrap();
        assert_eq!(header.len, 5003);
        let mut buf = vec![0u8; 5003];
        store.load(header, &mut buf).unwrap();
        assert_eq!(buf, image);
        assert!(
            store.flash.mem[..BASE as usize].iter().all(|&b| b == 0),
            "outside region untouched"
        );
    }

    #[test]
    fn erase_then_chunked_writes_of_any_size_commit() {
        let mut store = open(RamFlash::new());
        for chunk in [1, 2, 3, 4, 5, 7, 16, 61, 256, 1000] {
            // Each round reinstalls over the previous payload.
            let image = pattern(2050, chunk);
            try_install(&mut store, &image, chunk).unwrap();
            store = reboot(store);
            assert_eq!(contents(&mut store), Some(image), "chunk {chunk}");
        }
    }

    #[test]
    fn wrong_crc_commit_leaves_the_store_empty() {
        let mut store = open(RamFlash::new());
        let image = pattern(900, 5);
        store.erase(900).unwrap();
        store.write(0, &image).unwrap();
        assert_eq!(
            store.commit(900, Crc32::checksum(&image) ^ 1),
            Err(StoreError::Crc)
        );
        assert_eq!(store.header(), None);
        let mut store = reboot(store);
        assert_eq!(store.header(), None);
        assert_eq!(store.verify(), Err(StoreError::NoImage));
        assert_eq!(contents(&mut store), None);
        assert!(
            store.flash.mem[BASE as usize..BASE as usize + HEADER_LEN]
                .iter()
                .all(|&b| b == 0xFF),
            "no header written for the rejected module"
        );
    }

    #[test]
    fn invalid_payload_empties_store_and_erase_drops_the_previous_one() {
        let mut store = open(RamFlash::new());
        install(&mut store, &pattern(300, 1), 64);
        assert!(contents(&mut store).is_some());
        let bad = pattern(400, 2);
        store.erase(400).unwrap();
        assert_eq!(store.header(), None, "erase invalidates at once");
        assert_eq!(contents(&mut store), None);
        store.write(0, &bad).unwrap();
        assert_eq!(
            store.commit(400, Crc32::checksum(&bad) ^ 0x10),
            Err(StoreError::Crc)
        );
        let mut store = reboot(store);
        assert_eq!(store.header(), None);
        assert_eq!(contents(&mut store), None);
        // An abandoned transfer also leaves nothing behind.
        install(&mut store, &pattern(64, 3), 64);
        store.erase(10).unwrap();
        let store = reboot(store);
        assert_eq!(store.header(), None);
    }

    /// Installs `prior` (if any), then installs `new` with power lost at
    /// every erase or write in turn. After each recovery the store is empty
    /// or holds exactly `new`, never a partial or the old payload, and a
    /// further install still works.
    fn power_loss_sweep(prior: Option<&[u8]>, new: &[u8], chunk: usize) {
        for cut in 0..10_000 {
            let mut store = open(RamFlash::new());
            if let Some(prior) = prior {
                install(&mut store, prior, chunk);
            }
            store.flash.cut_at(cut);
            let mut store = reboot(store);
            let done = try_install(&mut store, new, chunk).is_ok();
            store.flash.restore();
            let mut store = reboot(store);
            let got = contents(&mut store);
            assert!(
                got.is_none() || got.as_deref() == Some(new),
                "cut {cut}: store holds neither empty nor the new payload"
            );
            if done {
                assert_eq!(got.as_deref(), Some(new), "cut {cut}: completed install");
                assert!(cut > 2, "the sweep cut the erase and header writes");
                return;
            }
            let after = pattern(222, cut);
            install(&mut store, &after, chunk);
            let mut store = reboot(store);
            assert_eq!(
                contents(&mut store),
                Some(after),
                "cut {cut}: install after recovery"
            );
        }
        panic!("install never completed");
    }

    #[test]
    fn power_loss_at_any_flash_operation_leaves_empty_or_new() {
        // Empty flash: the first install.
        power_loss_sweep(None, &pattern(410, 2), 7);
        // Over a prior payload: the old one must not survive a cut.
        power_loss_sweep(Some(&pattern(300, 1)), &pattern(410, 2), 7);
        // Multi-sector module in chunks wider than a word.
        power_loss_sweep(Some(&pattern(5003, 3)), &pattern(6000, 5), 256);
    }

    #[test]
    fn capacity_limits_are_enforced() {
        let mut store = open(RamFlash::new());
        assert_eq!(store.erase(CAPACITY + 1), Err(StoreError::TooLarge));
        store.erase(CAPACITY).unwrap();
        let full = pattern(CAPACITY as usize, 7);
        assert_eq!(store.write(CAPACITY, &[0]), Err(StoreError::TooLarge));
        store.write(0, &full).unwrap();
        assert_eq!(store.write(CAPACITY, &[0]), Err(StoreError::TooLarge));
        store.commit(CAPACITY, Crc32::checksum(&full)).unwrap();
        let mut store = reboot(store);
        assert_eq!(contents(&mut store), Some(full));
        // Erasing without a transfer is never a write target either.
        assert_eq!(store.write(0, &[0]), Err(StoreError::OutOfOrder));
    }

    #[test]
    fn out_of_order_writes_and_wrong_lengths_are_rejected() {
        let mut store = open(RamFlash::new());
        assert_eq!(
            store.write(0, &[0]),
            Err(StoreError::OutOfOrder),
            "no transfer"
        );
        store.erase(8).unwrap();
        assert_eq!(store.write(4, &[1]), Err(StoreError::OutOfOrder));
        store.write(0, &[1, 2, 3]).unwrap();
        assert_eq!(store.write(2, &[9]), Err(StoreError::OutOfOrder));
        store.write(3, &[4, 5, 6, 7, 8]).unwrap();
        assert_eq!(
            store.commit(4, Crc32::checksum(&[1, 2, 3, 4])),
            Err(StoreError::OutOfOrder),
            "declared length differs from bytes written"
        );
        assert_eq!(store.header(), None);
        assert_eq!(
            store.commit(8, 0),
            Err(StoreError::OutOfOrder),
            "transfer ended"
        );
    }

    #[test]
    fn torn_or_corrupt_header_and_module_read_as_empty() {
        let mut store = open(RamFlash::new());
        install(&mut store, &pattern(300, 1), 64);
        // A module byte flipped: the header decodes but the CRC does not match.
        store.flash.mem[(BASE + HEADER_LEN as u32 + 10) as usize] ^= 0xFF;
        let mut store = reboot(store);
        assert_eq!(store.header(), None);
        assert_eq!(store.verify(), Err(StoreError::NoImage));
        let h = Header {
            len: 300,
            crc: 0,
            generation: Generation::FIRST,
        };
        assert_eq!(store.load(h, &mut [0u8; 300]), Err(StoreError::NoImage));
        // A torn header: fields present, magic still erased.
        let mut store = open(RamFlash::new());
        install(&mut store, &pattern(300, 1), 64);
        store.flash.mem[BASE as usize..BASE as usize + 4].fill(0xFF);
        assert_eq!(reboot(store).header(), None);
        // A header claiming a length beyond the capacity.
        let mut store = open(RamFlash::new());
        store.flash.mem[BASE as usize..BASE as usize + 4].copy_from_slice(&MAGIC.to_le_bytes());
        store.flash.mem[BASE as usize + 4..BASE as usize + 8]
            .copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(reboot(store).header(), None);
    }

    #[test]
    fn bad_geometry_is_rejected() {
        assert!(SingleStore::new(RamFlash::new(), 3, SIZE as u32).is_err());
        assert!(SingleStore::new(RamFlash::new(), BASE, 100).is_err());
        assert!(SingleStore::new(RamFlash::new(), BASE, SECTOR + 1).is_err());
        assert!(SingleStore::new(RamFlash::new(), BASE, 0).is_err());
        assert!(SingleStore::new(RamFlash::new(), BASE, SECTOR).is_ok());
    }
}
