//! Payload storage over any NOR flash, in two slots.
//!
//! [`PayloadStore`] performs the loader's storage effects on the payload
//! partition, following the two-slot layout in [`crate::image`]. It is
//! generic over `embedded_storage::nor_flash::NorFlash`, so the same code
//! runs on `esp_storage::FlashStorage`, on the ESP8266 region driver, and on
//! a RAM flash model in the unit tests.
//!
//! * [`PayloadStore::new`] scans both slots: a slot is valid when its
//!   [`Header`] decodes and its image matches the header CRC-32, and the
//!   valid slot with the newer [`Generation`] becomes the *active* slot
//!   ([`crate::image::newest`]). With no valid slot the store is empty.
//! * [`PayloadStore::erase`] starts a transfer into the *inactive* slot (the
//!   one that is not active; slot A when the store is empty): it erases that
//!   slot's header sector and the image sectors. [`PayloadStore::write`]
//!   writes chunks through a [`WordWriter`].
//! * [`PayloadStore::activate`] flushes the last partial word, re-reads the
//!   new image and checks it against the expected CRC-32, then writes the
//!   slot's header (fields first, magic last) with the next generation, and
//!   only then makes that slot active.
//!
//! The active slot is never erased or written while it is active, so a
//! transfer that is abandoned, rejected, fails on a flash error, or is cut by
//! power loss leaves the previous image installed; after a reboot the store
//! holds either the previous image or, if the header write completed, the
//! new one. [`PayloadStore::header`], [`PayloadStore::verify`] and
//! [`PayloadStore::load`] always describe the active slot. The next
//! generation is one past the newest generation of any header that decodes
//! (even one whose image failed its CRC), so a new image always outranks
//! whatever is left on flash.
//!
//! Reads respect a 4-byte read granularity (esp-storage's default): the
//! aligned prefix is read directly and a trailing partial word through a
//! small buffer.

use crate::image::{
    Generation, HEADER_LEN, Header, OutOfOrder, Region, SECTOR, Slot, WORD, WordWriter, newest,
};
use embedded_storage::nor_flash::NorFlash;
use middleware::crc::Crc32;

/// Bytes read per step while verifying an image.
const VERIFY_STEP: usize = 256;

/// Why a storage operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreError {
    /// The flash driver reported an error.
    Flash,
    /// The image does not fit a slot.
    TooLarge,
    /// A write did not continue where the previous one ended, or no
    /// transfer is in progress.
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

/// A transfer in progress into the inactive slot.
#[derive(Debug, Clone, Copy)]
struct Transfer {
    slot: Slot,
    writer: WordWriter,
}

/// The payload partition on a flash device.
pub struct PayloadStore<F> {
    flash: F,
    base: u32,
    region: Region,
    limit: u32,
    /// The slot to boot and its header, or why there is none.
    active: Result<(Slot, Header), StoreError>,
    /// Newest generation of any decodable header seen on flash.
    latest: Option<Generation>,
    transfer: Option<Transfer>,
}

impl<F: NorFlash> PayloadStore<F> {
    /// A store for the partition at absolute flash offset `base` (sector
    /// aligned) of `size` bytes, split into two slots. Images are limited to
    /// `limit` bytes as well as the slot capacity. Scans both slots to find
    /// the active one; a slot that cannot be read counts as invalid.
    pub fn new(flash: F, base: u32, size: u32, limit: u32) -> Result<Self, StoreError> {
        if !base.is_multiple_of(SECTOR) {
            return Err(StoreError::Flash);
        }
        let mut store = PayloadStore {
            flash,
            base,
            region: Region::new(size),
            limit,
            active: Err(StoreError::NoImage),
            latest: None,
            transfer: None,
        };
        store.scan();
        Ok(store)
    }

    /// Picks the active slot and the latest generation from flash.
    fn scan(&mut self) {
        let [a, b] = Slot::ALL.map(|slot| self.check_slot(slot));
        let decoded = |r: &Result<Header, Option<Header>>| match *r {
            Ok(h) | Err(Some(h)) => Some(h.generation),
            Err(None) => None,
        };
        self.latest = match (decoded(&a), decoded(&b)) {
            (Some(x), Some(y)) if y.is_newer_than(x) => Some(y),
            (Some(x), _) => Some(x),
            (None, y) => y,
        };
        self.active = match newest(a.ok(), b.ok()) {
            Some(found) => Ok(found),
            None if matches!(a, Err(Some(_))) || matches!(b, Err(Some(_))) => Err(StoreError::Crc),
            None => Err(StoreError::NoImage),
        };
    }

    /// `Ok(header)` for a valid slot; `Err(Some(header))` when the header
    /// decodes but the image does not match it; `Err(None)` when there is
    /// no readable header.
    fn check_slot(&mut self, slot: Slot) -> Result<Header, Option<Header>> {
        let header = self.read_header(slot).ok_or(None)?;
        match self.crc_of(slot, header.len) {
            Ok(crc) if crc == header.crc => Ok(header),
            _ => Err(Some(header)),
        }
    }

    fn read_header(&mut self, slot: Slot) -> Option<Header> {
        let mut buf = [0u8; HEADER_LEN];
        let at = self.base + self.region.header_offset(slot);
        self.flash.read(at, &mut buf).ok()?;
        Header::decode(&buf, self.capacity())
    }

    /// Largest image accepted (per slot).
    pub fn capacity(&self) -> u32 {
        self.region.capacity().min(self.limit)
    }

    /// The slot the store boots from, if any.
    pub fn active_slot(&self) -> Option<Slot> {
        self.active.ok().map(|(slot, _)| slot)
    }

    /// The slot the next transfer writes: the one that is not active.
    fn inactive_slot(&self) -> Slot {
        self.active_slot().map_or(Slot::A, Slot::other)
    }

    /// Starts a transfer of an image of `len` bytes: erases the inactive
    /// slot's header and image sectors. Any earlier transfer is dropped.
    pub fn erase(&mut self, len: u32) -> Result<(), StoreError> {
        self.transfer = None;
        if len > self.capacity() {
            return Err(StoreError::TooLarge);
        }
        let slot = self.inactive_slot();
        let (from, to) = self.region.erase_span(slot, len).ok_or(StoreError::TooLarge)?;
        self.flash.erase(self.base + from, self.base + to).map_err(|_| StoreError::Flash)?;
        self.transfer = Some(Transfer { slot, writer: WordWriter::new() });
        Ok(())
    }

    /// Writes image bytes at `offset` (in order) into the transfer's slot.
    pub fn write(&mut self, offset: u32, data: &[u8]) -> Result<(), StoreError> {
        let t = self.transfer.ok_or(StoreError::OutOfOrder)?;
        let end = offset.checked_add(data.len() as u32).ok_or(StoreError::TooLarge)?;
        if end > self.capacity() {
            return Err(StoreError::TooLarge);
        }
        let (next, plan) = t.writer.push(offset, data)?;
        let at = self.base + self.region.data_offset(t.slot);
        if let Some((off, word)) = plan.head {
            self.flash.write(at + off, &word).map_err(|_| StoreError::Flash)?;
        }
        if let Some((off, bytes)) = plan.body {
            self.flash.write(at + off, bytes).map_err(|_| StoreError::Flash)?;
        }
        self.transfer = Some(Transfer { writer: next, ..t });
        Ok(())
    }

    /// Completes the transfer: flushes the last partial word, checks the
    /// written image against `crc`, writes the slot header with the next
    /// generation, and makes the slot active. `len` must equal the bytes
    /// written and be non-zero. The transfer ends whatever the result; on
    /// failure the previously active slot stays active.
    pub fn activate(&mut self, len: u32, crc: u32) -> Result<(), StoreError> {
        let t = self.transfer.take().ok_or(StoreError::OutOfOrder)?;
        if t.writer.position() != len || len == 0 {
            return Err(StoreError::OutOfOrder);
        }
        if let Some((off, word)) = t.writer.finish() {
            let at = self.base + self.region.data_offset(t.slot);
            self.flash.write(at + off, &word).map_err(|_| StoreError::Flash)?;
        }
        if self.crc_of(t.slot, len)? != crc {
            return Err(StoreError::Crc);
        }
        let generation = self.latest.map_or(Generation::FIRST, Generation::next);
        let header = Header { len, crc, generation };
        let bytes = header.encode();
        let (magic, fields) = bytes.split_at(WORD as usize);
        let at = self.base + self.region.header_offset(t.slot);
        self.flash.write(at + WORD, fields).map_err(|_| StoreError::Flash)?;
        self.flash.write(at, magic).map_err(|_| StoreError::Flash)?;
        self.active = Ok((t.slot, header));
        self.latest = Some(generation);
        Ok(())
    }

    /// The active image's header, if a valid one is installed.
    pub fn header(&self) -> Option<Header> {
        self.active.ok().map(|(_, h)| h)
    }

    /// Reads image bytes `[offset, offset + buf.len())` of `slot`, any
    /// length.
    fn read_image(&mut self, slot: Slot, offset: u32, buf: &mut [u8]) -> Result<(), StoreError> {
        let at = self.base + self.region.data_offset(slot) + offset;
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

    /// CRC-32 of the first `len` image bytes of `slot`.
    fn crc_of(&mut self, slot: Slot, len: u32) -> Result<u32, StoreError> {
        let mut crc = Crc32::new();
        let mut buf = [0u8; VERIFY_STEP];
        let mut at = 0u32;
        // Bounded: callers pass a length within the slot capacity.
        while at < len {
            let n = (len - at).min(VERIFY_STEP as u32);
            let part = buf.get_mut(..n as usize).ok_or(StoreError::Buffer)?;
            self.read_image(slot, at, part)?;
            crc = crc.update(part);
            at += n;
        }
        Ok(crc.finish())
    }

    /// Checks the active image against its header CRC without loading it.
    /// With no active slot, reports why the scan found none (`NoImage`, or
    /// `Crc` if a header was present but its image did not match).
    pub fn verify(&mut self) -> Result<Header, StoreError> {
        let (slot, header) = self.active?;
        if self.crc_of(slot, header.len)? == header.crc { Ok(header) } else { Err(StoreError::Crc) }
    }

    /// Reads the active image described by `header` into `buf` (exactly
    /// `header.len` bytes) and checks its CRC.
    pub fn load(&mut self, header: Header, buf: &mut [u8]) -> Result<(), StoreError> {
        let (slot, _) = self.active.map_err(|_| StoreError::NoImage)?;
        if buf.len() != header.len as usize {
            return Err(StoreError::Buffer);
        }
        self.read_image(slot, 0, buf)?;
        if Crc32::checksum(buf) == header.crc { Ok(()) } else { Err(StoreError::Crc) }
    }
}

#[cfg(test)]
mod tests {
    // Host-only test code: std collections for test images.
    extern crate std;

    use super::*;
    use std::{vec, vec::Vec};
    use embedded_storage::nor_flash::{ErrorType, NorFlashErrorKind, ReadNorFlash};

    const SIZE: usize = 8 * SECTOR as usize;
    const BASE: u32 = 2 * SECTOR;
    /// Slot B's header, absolute.
    const SLOT_B: u32 = BASE + SIZE as u32 / 2;

    /// RAM NOR flash: 4-byte reads/writes, 4 KiB erases, writes only clear
    /// bits. `cut` injects power loss: the erase or write with that index
    /// (counting from 0 after [`RamFlash::cut_at`]) is torn, half of its
    /// bytes applied, and it and every later erase or write fail.
    struct RamFlash {
        mem: [u8; SIZE + BASE as usize],
        erases: usize,
        ops: usize,
        cut: Option<usize>,
    }

    impl RamFlash {
        fn new() -> Self {
            RamFlash { mem: [0x00; SIZE + BASE as usize], erases: 0, ops: 0, cut: None }
        }

        fn cut_at(&mut self, op: usize) {
            self.ops = 0;
            self.cut = Some(op);
        }

        /// Power is back: no more faults.
        fn restore(&mut self) {
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

    fn open(flash: RamFlash) -> PayloadStore<RamFlash> {
        PayloadStore::new(flash, BASE, SIZE as u32, u32::MAX).unwrap()
    }

    /// A "reboot": a new store over the same flash contents.
    fn reboot(store: PayloadStore<RamFlash>) -> PayloadStore<RamFlash> {
        open(store.flash)
    }

    fn try_install(store: &mut PayloadStore<RamFlash>, image: &[u8], chunk: usize) -> Result<(), StoreError> {
        store.erase(image.len() as u32)?;
        for (i, part) in image.chunks(chunk).enumerate() {
            store.write((i * chunk) as u32, part)?;
        }
        store.activate(image.len() as u32, Crc32::checksum(image))
    }

    fn install(store: &mut PayloadStore<RamFlash>, image: &[u8], chunk: usize) {
        try_install(store, image, chunk).unwrap();
    }

    /// The active image's bytes, after verifying it.
    fn contents(store: &mut PayloadStore<RamFlash>) -> Option<Vec<u8>> {
        let header = store.verify().ok()?;
        let mut buf = vec![0u8; header.len as usize];
        store.load(header, &mut buf).unwrap();
        Some(buf)
    }

    fn pattern(len: usize, seed: usize) -> Vec<u8> {
        (0..len).map(|i| ((i + seed) * 31 % 251) as u8).collect()
    }

    #[test]
    fn installed_image_survives_reopen_and_verifies() {
        let image: [u8; 5003] = core::array::from_fn(|i| (i * 31 % 251) as u8);
        let mut store = open(RamFlash::new());
        assert_eq!(store.header(), None, "zeroed flash has no image");
        install(&mut store, &image, 7);
        let mut store = reboot(store);
        let header = store.verify().unwrap();
        assert_eq!(header.len, 5003);
        let mut buf = [0u8; 5003];
        store.load(header, &mut buf).unwrap();
        assert_eq!(buf, image);
        assert!(store.flash.mem[..BASE as usize].iter().all(|&b| b == 0), "outside partition untouched");
    }

    #[test]
    fn first_install_on_empty_flash_uses_slot_a() {
        let mut store = open(RamFlash::new());
        assert_eq!((store.active_slot(), store.verify()), (None, Err(StoreError::NoImage)));
        let image = pattern(300, 1);
        install(&mut store, &image, 64);
        assert_eq!(store.active_slot(), Some(Slot::A));
        assert_eq!(store.header().map(|h| h.generation), Some(Generation::FIRST));
        let mut store = reboot(store);
        assert_eq!(store.active_slot(), Some(Slot::A));
        assert_eq!(contents(&mut store), Some(image));
        assert!(store.flash.mem[SLOT_B as usize..].iter().all(|&b| b == 0), "slot B untouched");
    }

    #[test]
    fn interrupted_transfer_keeps_previous_image() {
        let mut store = open(RamFlash::new());
        install(&mut store, &[1, 2, 3, 4, 5], 2);
        store.erase(100).unwrap();
        store.write(0, &[9; 50]).unwrap();
        assert_eq!(store.header().map(|h| h.len), Some(5));
        assert_eq!(contents(&mut store), Some(vec![1, 2, 3, 4, 5]));
        assert_eq!(store.activate(100, 0), Err(StoreError::OutOfOrder));
        assert_eq!(store.activate(50, 0), Err(StoreError::OutOfOrder), "transfer ended");
        let mut store = reboot(store);
        assert_eq!(store.active_slot(), Some(Slot::A));
        assert_eq!(contents(&mut store), Some(vec![1, 2, 3, 4, 5]));
    }

    #[test]
    fn wrong_crc_commit_keeps_old_image_after_reboot() {
        let mut store = open(RamFlash::new());
        let old = pattern(700, 0);
        install(&mut store, &old, 64);
        let new = pattern(900, 5);
        store.erase(900).unwrap();
        store.write(0, &new).unwrap();
        assert_eq!(store.activate(900, Crc32::checksum(&new) ^ 1), Err(StoreError::Crc));
        assert_eq!(store.active_slot(), Some(Slot::A));
        assert_eq!(contents(&mut store), Some(old.clone()));
        let mut store = reboot(store);
        assert_eq!(store.active_slot(), Some(Slot::A));
        assert_eq!(contents(&mut store), Some(old));
        let raw = &store.flash.mem[SLOT_B as usize..SLOT_B as usize + HEADER_LEN];
        assert!(raw.iter().all(|&b| b == 0xFF), "no header written for the rejected image");
    }

    #[test]
    fn generations_alternate_slots_across_installs() {
        let mut store = open(RamFlash::new());
        let slots = [Slot::A, Slot::B, Slot::A, Slot::B, Slot::A, Slot::B];
        for (n, slot) in slots.into_iter().enumerate() {
            let image = pattern(100 + 997 * n, n);
            install(&mut store, &image, 13);
            store = reboot(store);
            assert_eq!(store.active_slot(), Some(slot), "install {n}");
            let header = store.header().unwrap();
            assert_eq!(header.generation, Generation(n as u32 + 1));
            assert_eq!(contents(&mut store), Some(image));
        }
    }

    #[test]
    fn generation_wraps_past_u32_max() {
        let mut store = open(RamFlash::new());
        install(&mut store, &[1; 40], 40);
        // Rewrite slot A's generation word to the last value.
        let at = BASE as usize + 12;
        store.flash.mem[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        let mut store = reboot(store);
        assert_eq!(store.header().map(|h| h.generation), Some(Generation(u32::MAX)));
        install(&mut store, &[2; 40], 40);
        let mut store = reboot(store);
        assert_eq!(store.active_slot(), Some(Slot::B));
        assert_eq!(store.header().map(|h| h.generation), Some(Generation(0)));
        assert_eq!(contents(&mut store), Some(vec![2; 40]));
    }

    #[test]
    fn corrupt_newest_slot_falls_back_to_older() {
        let mut store = open(RamFlash::new());
        install(&mut store, &[7u8; 64], 64);
        install(&mut store, &[8u8; 64], 64);
        store.flash.mem[(SLOT_B + SECTOR + 10) as usize] = 0;
        let mut store = reboot(store);
        assert_eq!(store.active_slot(), Some(Slot::A));
        assert_eq!(contents(&mut store), Some(vec![7u8; 64]));
        // The next install goes to the corrupt slot and outranks both.
        install(&mut store, &[9u8; 64], 64);
        let mut store = reboot(store);
        assert_eq!(store.active_slot(), Some(Slot::B));
        assert_eq!(store.header().map(|h| h.generation), Some(Generation(3)));
        assert_eq!(contents(&mut store), Some(vec![9u8; 64]));
    }

    #[test]
    fn both_slots_corrupt_boots_empty() {
        let mut store = open(RamFlash::new());
        install(&mut store, &[7u8; 64], 64);
        install(&mut store, &[8u8; 64], 64);
        store.flash.mem[(BASE + SECTOR + 3) as usize] = 0;
        store.flash.mem[(SLOT_B + SECTOR + 3) as usize] = 0;
        let mut store = reboot(store);
        assert_eq!((store.active_slot(), store.header()), (None, None));
        assert_eq!(store.verify(), Err(StoreError::Crc));
        let mut buf = [0u8; 64];
        let h = Header { len: 64, crc: 0, generation: Generation(1) };
        assert_eq!(store.load(h, &mut buf), Err(StoreError::NoImage));
        // Recovery: a new image installs into slot A above both generations.
        install(&mut store, &[5u8; 64], 64);
        let mut store = reboot(store);
        assert_eq!(store.active_slot(), Some(Slot::A));
        assert_eq!(store.header().map(|h| h.generation), Some(Generation(3)));
        assert_eq!(contents(&mut store), Some(vec![5u8; 64]));
    }

    #[test]
    fn corruption_is_detected() {
        let mut store = open(RamFlash::new());
        install(&mut store, &[7u8; 64], 64);
        store.flash.mem[(BASE + SECTOR + 10) as usize] = 0;
        assert_eq!(store.verify(), Err(StoreError::Crc));
        let header = store.header().unwrap();
        let mut buf = [0u8; 64];
        assert_eq!(store.load(header, &mut buf), Err(StoreError::Crc));
        assert_eq!(store.load(header, &mut buf[..63]), Err(StoreError::Buffer));
    }

    /// Installs `prior` images, then installs `new` with power lost at every
    /// erase or write in turn. After each recovery the store holds the last
    /// prior image or `new` (never garbage, never empty when a prior image
    /// existed), and a further install still works.
    fn power_loss_sweep(prior: &[Vec<u8>], new: &[u8], chunk: usize) {
        let old = prior.last();
        for cut in 0..10_000 {
            let mut store = open(RamFlash::new());
            for image in prior {
                install(&mut store, image, chunk);
            }
            store.flash.cut_at(cut);
            let mut store = reboot(store);
            let done = try_install(&mut store, new, chunk).is_ok();
            store.flash.restore();
            let mut store = reboot(store);
            let got = contents(&mut store);
            if done {
                assert_eq!(got.as_deref(), Some(new), "cut {cut}: completed install");
                assert!(cut > 2, "the sweep cut at least the erase and header writes");
                return;
            }
            assert!(
                got.as_deref() == old.map(Vec::as_slice) || got.as_deref() == Some(new),
                "cut {cut}: store holds neither the old nor the new image"
            );
            let after = pattern(222, cut);
            install(&mut store, &after, chunk);
            let mut store = reboot(store);
            assert_eq!(contents(&mut store), Some(after), "cut {cut}: install after recovery");
        }
        panic!("install never completed");
    }

    #[test]
    fn power_loss_at_any_flash_operation_keeps_old_or_new_image() {
        // One prior image in slot A; the new one goes to B.
        power_loss_sweep(&[pattern(300, 1)], &pattern(410, 2), 7);
        // Two prior images: the new one overwrites the older slot A while
        // slot B holds the image to keep. Multi-sector image.
        power_loss_sweep(&[pattern(5003, 3), pattern(4100, 4)], &pattern(6000, 5), 256);
        // First install on empty flash: empty or the new image.
        power_loss_sweep(&[], &pattern(300, 6), 7);
    }

    #[test]
    fn limits_and_ordering_are_enforced() {
        let mut store = PayloadStore::new(RamFlash::new(), BASE, SIZE as u32, 1000).unwrap();
        assert_eq!(store.capacity(), 1000);
        assert_eq!(store.erase(1001), Err(StoreError::TooLarge));
        assert_eq!(store.write(0, &[0]), Err(StoreError::OutOfOrder), "no transfer");
        store.erase(1000).unwrap();
        assert_eq!(store.flash.erases, 1);
        assert_eq!(store.write(4, &[0]), Err(StoreError::OutOfOrder));
        assert_eq!(store.write(999, &[0, 0]), Err(StoreError::TooLarge));
        assert_eq!(store.activate(0, 0), Err(StoreError::OutOfOrder), "empty image");
        assert!(PayloadStore::new(RamFlash::new(), 3, SIZE as u32, 10).is_err());
        let full = open(RamFlash::new());
        assert_eq!(full.capacity(), SIZE as u32 / 2 - SECTOR);
    }
}
