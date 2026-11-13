//! The slot image header (RT5): what the bootloader and the updater check
//! before an image may run.
//!
//! A firmware slot holds a [`HEADER_LEN`]-byte header at offset 0 and the
//! firmware image right after it, the image being the bytes the bootloader
//! copies to SDRAM at the firmware's link address
//! ([`crate::layout::SDRAM_IMAGE`]). The update `mwctl fw update` sends is
//! the slot's contents, header first, so `sys/fw/begin`'s length is
//! `HEADER_LEN + len` and its SHA-256 covers both.
//!
//! | offset | size | field |
//! |--------|------|-------|
//! | 0      | 4    | magic `b"MWRT"` |
//! | 4      | 2    | header version, u16 LE ([`HEADER_VERSION`]) |
//! | 6      | 2    | zero |
//! | 8      | 4    | image version, u32 LE: `major << 16 \| minor` |
//! | 12     | 4    | image length in bytes, u32 LE (excluding the header) |
//! | 16     | 32   | SHA-256 of header bytes 0..16 followed by the image |
//! | 48     | 208  | zero |
//!
//! The hash covers the header's own fields as well as the image, so a
//! damaged version or length is caught like a damaged image. A header
//! whose reserved bytes are not zero is a format this bootloader does not
//! know ([`Refusal::BadHeaderVersion`]). 256 bytes keep the image aligned
//! for the copy and for the HyperFlash's 512-byte write buffer.
//!
//! [`check_copy`] is the integrity check: it decodes the header, checks
//! the image fits the slot and the SDRAM region, copies the image from the
//! slot into the destination and hashes **the copy**, so what was checked
//! is what runs. Each failure has its own [`Refusal`].

use middleware::loader::sha256::ImageHash;
use middleware::loader::trust::Version;

/// Header length; the image starts at this slot offset.
pub const HEADER_LEN: usize = 256;
/// Header magic.
pub const MAGIC: [u8; 4] = *b"MWRT";
/// The header version this crate writes and reads.
pub const HEADER_VERSION: u16 = 1;
/// Header bytes covered by the hash (magic to length).
const HASHED_FIELDS: usize = 16;
/// Bytes copied per step.
const COPY_STEP: usize = 4096;

/// Why a slot does not boot (or an image is not accepted).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The slot could not be read.
    Unreadable,
    /// No header magic (an erased or foreign slot).
    BadMagic,
    /// A header version (or reserved field) this bootloader does not know.
    BadHeaderVersion,
    /// The image length is zero.
    Empty,
    /// Header and image do not fit the slot.
    PastSlot,
    /// The image does not fit the SDRAM image region.
    PastSdram,
    /// The SHA-256 of the copy does not match the header.
    HashMismatch,
    /// Not checked: the slot's trial boot is spent (it reset without
    /// `sys/fw/confirm`), so it is never booted again ([`super::boot`]).
    TrialSpent,
}

impl Refusal {
    /// A short reason, for the bootloader's recovery message on LPUART1.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Refusal::Unreadable => "unreadable",
            Refusal::BadMagic => "no image (bad magic)",
            Refusal::BadHeaderVersion => "unknown header version",
            Refusal::Empty => "empty image",
            Refusal::PastSlot => "image larger than the slot",
            Refusal::PastSdram => "image larger than the SDRAM region",
            Refusal::HashMismatch => "SHA-256 mismatch",
            Refusal::TrialSpent => "trial spent without confirm",
        }
    }
}

/// A decoded slot image header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotHeader {
    /// Image version (`major << 16 | minor`).
    pub version: Version,
    /// Image length, excluding the header.
    pub len: u32,
    /// SHA-256 of header bytes 0..16 and the image.
    pub sha256: [u8; 32],
}

fn le32(b: &[u8], at: usize) -> u32 {
    let mut w = [0u8; 4];
    if let Some(src) = b.get(at..at + 4) {
        w.copy_from_slice(src);
    }
    u32::from_le_bytes(w)
}

impl SlotHeader {
    /// The header bytes the hash covers, for `version` and `len`.
    fn fields(version: Version, len: u32) -> [u8; HASHED_FIELDS] {
        let mut f = [0u8; HASHED_FIELDS];
        f[..4].copy_from_slice(&MAGIC);
        f[4..6].copy_from_slice(&HEADER_VERSION.to_le_bytes());
        f[8..12].copy_from_slice(&version.0.to_le_bytes());
        f[12..16].copy_from_slice(&len.to_le_bytes());
        f
    }

    /// The header for `image` as version `version` (what the build tool
    /// writes in front of the image); `None` if `image` is 4 GiB or more.
    pub fn for_image(version: Version, image: &[u8]) -> Option<SlotHeader> {
        let len = u32::try_from(image.len()).ok()?;
        let sha256 = ImageHash::new()
            .update(&Self::fields(version, len))
            .update(image)
            .finish();
        Some(SlotHeader {
            version,
            len,
            sha256,
        })
    }

    /// The header's bytes.
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[..HASHED_FIELDS].copy_from_slice(&Self::fields(self.version, self.len));
        out[16..48].copy_from_slice(&self.sha256);
        out
    }

    /// Decodes a header: the magic, the header version and the zero
    /// fields. The length and the hash are checked by [`SlotHeader::fits`]
    /// and [`check_copy`].
    pub fn decode(b: &[u8; HEADER_LEN]) -> Result<SlotHeader, Refusal> {
        if b[..4] != MAGIC {
            return Err(Refusal::BadMagic);
        }
        let reserved_zero = b[6..8].iter().chain(&b[48..]).all(|&x| x == 0);
        if u16::from_le_bytes([b[4], b[5]]) != HEADER_VERSION || !reserved_zero {
            return Err(Refusal::BadHeaderVersion);
        }
        let mut sha256 = [0u8; 32];
        sha256.copy_from_slice(&b[16..48]);
        Ok(SlotHeader {
            version: Version(le32(b, 8)),
            len: le32(b, 12),
            sha256,
        })
    }

    /// Header and image length: what the slot holds.
    pub const fn total_len(&self) -> Option<u32> {
        self.len.checked_add(HEADER_LEN as u32)
    }

    /// Whether the image fits a slot of `slot_size` bytes after its header
    /// and an SDRAM region of `ram` bytes.
    pub fn fits(&self, slot_size: u32, ram: usize) -> Result<(), Refusal> {
        if self.len == 0 {
            return Err(Refusal::Empty);
        }
        match self.total_len() {
            Some(total) if total <= slot_size => {}
            _ => return Err(Refusal::PastSlot),
        }
        if self.len as usize > ram {
            return Err(Refusal::PastSdram);
        }
        Ok(())
    }

    /// The hash over the header's fields, to be continued with the image.
    pub fn hasher(&self) -> ImageHash {
        ImageHash::new().update(&Self::fields(self.version, self.len))
    }
}

/// Where [`check_copy`] puts the image: the SDRAM image region. A plain
/// byte slice in host tests; on the board a region the firmware cannot
/// borrow as `&mut [u8]` without `unsafe` (memory at a fixed address, held
/// as atomics), so the copy goes through this trait.
pub trait ImageDest {
    /// Bytes available.
    fn capacity(&self) -> usize;
    /// Writes `data` at byte offset `at`; `at + data.len()` is at most
    /// [`ImageDest::capacity`] (the caller checks).
    fn write(&mut self, at: usize, data: &[u8]);
    /// Reads `buf.len()` bytes at byte offset `at` back from the region;
    /// same bound as [`ImageDest::write`].
    fn read(&self, at: usize, buf: &mut [u8]);
}

impl ImageDest for [u8] {
    fn capacity(&self) -> usize {
        self.len()
    }

    fn write(&mut self, at: usize, data: &[u8]) {
        if let Some(d) = self.get_mut(at..at + data.len()) {
            d.copy_from_slice(data);
        }
    }

    fn read(&self, at: usize, buf: &mut [u8]) {
        if let Some(s) = self.get(at..at + buf.len()) {
            buf.copy_from_slice(s);
        }
    }
}

/// The integrity check of one slot. `read(offset, buf)` reads the slot;
/// `dest` is the SDRAM image region. Decodes the header, checks the image
/// fits the slot of `slot_size` bytes and `dest`, copies it into `dest`,
/// then reads the copy back and compares its SHA-256 with the header's:
/// what is hashed is what will run. Bounded: `len / 4096 + 1` slot reads
/// and `len / 4096 + 1` read-backs.
pub fn check_copy<E, R, D>(
    read: &mut R,
    slot_size: u32,
    dest: &mut D,
) -> Result<SlotHeader, Refusal>
where
    R: FnMut(u32, &mut [u8]) -> Result<(), E>,
    D: ImageDest + ?Sized,
{
    let mut raw = [0u8; HEADER_LEN];
    read(0, &mut raw).map_err(|_| Refusal::Unreadable)?;
    let header = SlotHeader::decode(&raw)?;
    header.fits(slot_size, dest.capacity())?;
    let len = header.len as usize;
    if len > dest.capacity() {
        return Err(Refusal::PastSdram);
    }
    let mut step = [0u8; COPY_STEP];
    let mut at = 0;
    while at < len {
        let n = COPY_STEP.min(len - at);
        let buf = &mut step[..n];
        read((HEADER_LEN + at) as u32, buf).map_err(|_| Refusal::Unreadable)?;
        dest.write(at, buf);
        at += n;
    }
    let mut hash = header.hasher();
    let mut at = 0;
    while at < len {
        let n = COPY_STEP.min(len - at);
        let buf = &mut step[..n];
        dest.read(at, buf);
        hash = hash.update(buf);
        at += n;
    }
    if hash.finish() != header.sha256 {
        return Err(Refusal::HashMismatch);
    }
    Ok(header)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec;
    use std::vec::Vec;

    const SLOT: u32 = 64 * 1024;
    const V: Version = Version(0x0001_0002);

    /// A slot's contents: header, `len` image bytes, erased after.
    fn slot(len: usize) -> Vec<u8> {
        let image: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();
        let mut s = SlotHeader::for_image(V, &image).unwrap().encode().to_vec();
        s.extend_from_slice(&image);
        s.resize(SLOT as usize, 0xFF);
        s
    }

    fn check(s: &[u8], dest: &mut [u8]) -> Result<SlotHeader, Refusal> {
        let mut read = |at: u32, buf: &mut [u8]| -> Result<(), ()> {
            let src = s.get(at as usize..at as usize + buf.len()).ok_or(())?;
            buf.copy_from_slice(src);
            Ok(())
        };
        check_copy(&mut read, SLOT, dest)
    }

    #[test]
    fn rt5_a_well_formed_header_gives_version_length_and_hash_and_the_copy_is_the_image() {
        let len = 10_000;
        let s = slot(len);
        let mut dest = vec![0u8; 32 * 1024];
        let h = check(&s, &mut dest).unwrap();
        assert_eq!((h.version, h.len), (V, len as u32));
        assert_eq!(&dest[..len], &s[HEADER_LEN..HEADER_LEN + len]);
        assert_eq!(h.total_len(), Some((HEADER_LEN + len) as u32));
        // The encoding round trips and the layout is the documented one.
        let raw: [u8; HEADER_LEN] = s[..HEADER_LEN].try_into().unwrap();
        assert_eq!(SlotHeader::decode(&raw), Ok(h));
        assert_eq!(&raw[..4], b"MWRT");
        assert_eq!(&raw[4..8], &[1, 0, 0, 0]);
        assert_eq!(&raw[8..12], &0x0001_0002u32.to_le_bytes());
        assert_eq!(&raw[12..16], &(len as u32).to_le_bytes());
        assert_eq!(&raw[16..48], &h.sha256);
        let mut whole = raw[..16].to_vec();
        whole.extend_from_slice(&s[HEADER_LEN..HEADER_LEN + len]);
        assert_eq!(
            middleware::loader::sha256::ImageHash::digest(&whole),
            h.sha256
        );
    }

    #[test]
    fn rt5_each_defect_is_refused_with_its_own_reason() {
        let len = 5000;
        let good = slot(len);
        let mut dest = vec![0u8; 32 * 1024];
        let damaged = |at: usize, value: u8| {
            let mut s = good.clone();
            s[at] = value;
            s
        };
        assert_eq!(
            check(&vec![0xFF; SLOT as usize], &mut dest),
            Err(Refusal::BadMagic)
        );
        assert_eq!(check(&damaged(0, b'X'), &mut dest), Err(Refusal::BadMagic));
        assert_eq!(
            check(&damaged(4, 2), &mut dest),
            Err(Refusal::BadHeaderVersion)
        );
        assert_eq!(
            check(&damaged(6, 1), &mut dest),
            Err(Refusal::BadHeaderVersion)
        );
        assert_eq!(
            check(&damaged(200, 1), &mut dest),
            Err(Refusal::BadHeaderVersion)
        );
        // The image byte, the version and the length are all hashed.
        assert_eq!(
            check(&damaged(HEADER_LEN + 17, 0), &mut dest),
            Err(Refusal::HashMismatch)
        );
        assert_eq!(check(&damaged(8, 3), &mut dest), Err(Refusal::HashMismatch));
        assert_eq!(
            check(&damaged(12, 0x87), &mut dest),
            Err(Refusal::HashMismatch)
        );
        assert_eq!(
            check(&damaged(20, 0), &mut dest),
            Err(Refusal::HashMismatch)
        );
        // Lengths: zero, past the slot, past the SDRAM region.
        let with_len = |n: u32| {
            let mut s = good.clone();
            s[12..16].copy_from_slice(&n.to_le_bytes());
            s
        };
        assert_eq!(check(&with_len(0), &mut dest), Err(Refusal::Empty));
        assert_eq!(
            check(&with_len(SLOT - HEADER_LEN as u32 + 1), &mut dest),
            Err(Refusal::PastSlot)
        );
        assert_eq!(
            check(&with_len(u32::MAX), &mut dest),
            Err(Refusal::PastSlot)
        );
        assert_eq!(check(&good, &mut dest[..len - 1]), Err(Refusal::PastSdram));
        assert_eq!(
            check(&good, &mut dest[..len]).map(|h| h.len),
            Ok(len as u32)
        );
        // A slot that cannot be read.
        let mut failing = |_: u32, _: &mut [u8]| -> Result<(), ()> { Err(()) };
        assert_eq!(
            check_copy(&mut failing, SLOT, dest.as_mut_slice()),
            Err(Refusal::Unreadable)
        );
        // Every reason has its own text.
        let all = [
            Refusal::Unreadable,
            Refusal::BadMagic,
            Refusal::BadHeaderVersion,
            Refusal::Empty,
            Refusal::PastSlot,
            Refusal::PastSdram,
            Refusal::HashMismatch,
        ];
        for (i, a) in all.iter().enumerate() {
            assert!(all[i + 1..].iter().all(|b| a.as_str() != b.as_str()));
        }
    }

    #[test]
    fn rt5_an_image_filling_the_slot_exactly_is_accepted() {
        let len = SLOT as usize - HEADER_LEN;
        let s = slot(len);
        let mut dest = vec![0u8; len];
        assert_eq!(check(&s, &mut dest).map(|h| h.len), Ok(len as u32));
    }
}
