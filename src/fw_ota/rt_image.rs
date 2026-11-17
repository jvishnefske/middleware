//! The i.MX RT1050 slot image header (`MWRT`), decoded and checked
//! without the board crate (RT15): what `mwctl fw update` checks before
//! it sends an RT image, and, with `trust`, an [`ImageFormat`] for it.
//!
//! An RT slot image is the slot's contents: a [`HEADER_LEN`]-byte header,
//! then the image the bootloader copies to SDRAM. The firmware's own copy
//! of this layout is `firmware-imxrt/src/ab/header.rs` (written by its
//! `imxrt-image` tool, checked by its bootloader and `RtImage`); this
//! module reads the same bytes. The update sends the whole slot contents,
//! header first: `sys/fw/begin`'s length is `HEADER_LEN + len` and its
//! SHA-256 covers both.
//!
//! | offset | size | field |
//! |--------|------|-------|
//! | 0      | 4    | magic `b"MWRT"` ([`MAGIC`]) |
//! | 4      | 2    | header version, u16 LE ([`HEADER_VERSION`]) |
//! | 6      | 2    | zero |
//! | 8      | 4    | image version, u32 LE: `major << 16 \| minor` |
//! | 12     | 4    | image length in bytes, u32 LE (excluding the header) |
//! | 16     | 32   | SHA-256 of header bytes 0..16 followed by the image |
//! | 48     | 208  | zero |
//!
//! The checks, each with its own [`RtImageError`]:
//!
//! * [`RtHeader::decode`] — the magic, the header version and the zero
//!   reserved bytes (a non-zero reserved byte is a format this version
//!   does not know);
//! * [`RtHeader::check_len`] — a non-empty image whose header and image
//!   are exactly the bytes given and fit `max`;
//! * [`RtHeader::check_digest`] — the SHA-256 over
//!   [`RtHeader::hashed_fields`] and the image, computed by the caller
//!   (mwctl builds this crate without `trust`, so it hashes with its own
//!   `sha2`); [`check`] (feature `trust`) does all three.
//!
//! Everything here is `no_std`, allocation free and panic free: slices
//! are taken with `get`, lengths with checked arithmetic.

use super::format::ImageInfo;
#[cfg(feature = "trust")]
use super::{format::ImageFormat, wire::SlotInfo};
use crate::loader::trust::Version;

/// Header length; the image starts at this slot offset.
pub const HEADER_LEN: usize = 256;
/// Header magic.
pub const MAGIC: [u8; 4] = *b"MWRT";
/// The header version this module reads.
pub const HEADER_VERSION: u16 = 1;
/// Header bytes covered by the hash (magic to length).
pub const HASHED_FIELDS: usize = 16;
/// Offset of the header's SHA-256.
const SHA_AT: usize = 16;
/// End of the header's used fields; zero from here to [`HEADER_LEN`].
const USED: usize = 48;

/// Why bytes are not an acceptable RT slot image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtImageError {
    /// Fewer than [`HEADER_LEN`] bytes.
    Short,
    /// No `MWRT` magic.
    BadMagic,
    /// A header version or a non-zero reserved byte this module does not
    /// know.
    BadHeaderVersion,
    /// The header's image length is zero.
    Empty,
    /// The header's length plus [`HEADER_LEN`] is not the number of bytes
    /// given (a truncated or padded file).
    LengthMismatch {
        /// Header and image, as the header says.
        header: u32,
        /// Bytes given.
        actual: usize,
    },
    /// Header and image exceed the limit (the slot, or what the protocol
    /// carries).
    TooLarge {
        /// Header and image, as the header says.
        total: u64,
        /// The limit.
        max: u32,
    },
    /// The SHA-256 over the header fields and the image does not match
    /// the header.
    HashMismatch,
}

impl RtImageError {
    /// A short reason.
    pub const fn as_str(&self) -> &'static str {
        match self {
            RtImageError::Short => "shorter than the 256-byte slot header",
            RtImageError::BadMagic => "no MWRT magic",
            RtImageError::BadHeaderVersion => "unknown header version or non-zero reserved bytes",
            RtImageError::Empty => "empty image",
            RtImageError::LengthMismatch { .. } => "header length differs from the file length",
            RtImageError::TooLarge { .. } => "image too large",
            RtImageError::HashMismatch => "SHA-256 mismatch",
        }
    }
}

/// Whether `bytes` start with the RT slot header magic (they may still
/// fail [`RtHeader::decode`]).
pub fn is_rt_image(bytes: &[u8]) -> bool {
    bytes.get(..MAGIC.len()) == Some(&MAGIC[..])
}

fn le32(b: &[u8], at: usize) -> u32 {
    let mut w = [0u8; 4];
    if let Some(src) = b.get(at..at + 4) {
        w.copy_from_slice(src);
    }
    u32::from_le_bytes(w)
}

/// A decoded RT slot image header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtHeader {
    /// Image version (`major << 16 | minor`).
    pub version: Version,
    /// Image length, excluding the header.
    pub len: u32,
    /// SHA-256 of header bytes 0..16 and the image.
    pub sha256: [u8; 32],
}

impl RtHeader {
    /// Decodes the first [`HEADER_LEN`] bytes of `bytes`: the magic, the
    /// header version and the zero fields. The length and the hash are
    /// checked by [`RtHeader::check_len`] and [`RtHeader::check_digest`].
    pub fn decode(bytes: &[u8]) -> Result<RtHeader, RtImageError> {
        let b = bytes.get(..HEADER_LEN).ok_or(RtImageError::Short)?;
        if !is_rt_image(b) {
            return Err(RtImageError::BadMagic);
        }
        let version = u16::from_le_bytes([b[4], b[5]]);
        let mut reserved = b[6..8].iter().chain(&b[USED..]);
        if version != HEADER_VERSION || reserved.any(|&x| x != 0) {
            return Err(RtImageError::BadHeaderVersion);
        }
        let mut sha256 = [0u8; 32];
        sha256.copy_from_slice(&b[SHA_AT..USED]);
        Ok(RtHeader {
            version: Version(le32(b, 8)),
            len: le32(b, 12),
            sha256,
        })
    }

    /// The header bytes the hash covers (magic to length).
    pub fn hashed_fields(&self) -> [u8; HASHED_FIELDS] {
        let mut f = [0u8; HASHED_FIELDS];
        f[..4].copy_from_slice(&MAGIC);
        f[4..6].copy_from_slice(&HEADER_VERSION.to_le_bytes());
        f[8..12].copy_from_slice(&self.version.0.to_le_bytes());
        f[12..16].copy_from_slice(&self.len.to_le_bytes());
        f
    }

    /// The header's bytes.
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[..HASHED_FIELDS].copy_from_slice(&self.hashed_fields());
        out[SHA_AT..USED].copy_from_slice(&self.sha256);
        out
    }

    /// Header and image length: what the slot holds; `None` past 4 GiB.
    pub const fn total_len(&self) -> Option<u32> {
        self.len.checked_add(HEADER_LEN as u32)
    }

    /// The image is not empty, header and image are exactly `actual`
    /// bytes, and they fit `max` bytes.
    pub fn check_len(&self, actual: usize, max: u32) -> Result<(), RtImageError> {
        if self.len == 0 {
            return Err(RtImageError::Empty);
        }
        let total = u64::from(self.len) + HEADER_LEN as u64;
        if total > u64::from(max) {
            return Err(RtImageError::TooLarge { total, max });
        }
        if total != actual as u64 {
            return Err(RtImageError::LengthMismatch {
                header: total as u32,
                actual,
            });
        }
        Ok(())
    }

    /// `digest` (SHA-256 of [`RtHeader::hashed_fields`] followed by the
    /// image) is the header's.
    pub fn check_digest(&self, digest: &[u8; 32]) -> Result<(), RtImageError> {
        if *digest == self.sha256 {
            Ok(())
        } else {
            Err(RtImageError::HashMismatch)
        }
    }

    /// The header for `image` as version `version`; `None` if `image` is
    /// 4 GiB or more.
    #[cfg(feature = "trust")]
    pub fn for_image(version: Version, image: &[u8]) -> Option<RtHeader> {
        let len = u32::try_from(image.len()).ok()?;
        let mut h = RtHeader {
            version,
            len,
            sha256: [0; 32],
        };
        h.sha256 = h.hasher().update(image).finish();
        Some(h)
    }

    /// The hash over the header's fields, to be continued with the image.
    #[cfg(feature = "trust")]
    pub fn hasher(&self) -> crate::loader::sha256::ImageHash {
        crate::loader::sha256::ImageHash::new().update(&self.hashed_fields())
    }
}

/// Decodes and checks the RT slot image `slot` (header and image, as
/// sent): header, length (exactly `slot`, at most `max`) and hash.
#[cfg(feature = "trust")]
pub fn check(slot: &[u8], max: u32) -> Result<RtHeader, RtImageError> {
    let header = RtHeader::decode(slot)?;
    header.check_len(slot.len(), max)?;
    let image = slot.get(HEADER_LEN..).unwrap_or_default();
    header.check_digest(&header.hasher().update(image).finish())?;
    Ok(header)
}

/// What `RtSlotImage::inspect` (feature `trust`) found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtSlotInfo {
    /// The decoded header.
    pub header: RtHeader,
    /// The length received (header included).
    pub image_len: u32,
    /// The header fields and image hash to the header's SHA-256.
    pub hash_ok: bool,
    /// The largest image (header excluded) the format accepts.
    pub ram: u32,
}

impl ImageInfo for RtSlotInfo {
    /// The header's version is `version`, the image is not empty and no
    /// larger than `ram`, its length is exactly what was received, and
    /// the hash matches.
    fn acceptable(&self, version: Version) -> bool {
        self.hash_ok
            && self.header.version == version
            && self.header.len != 0
            && self.header.len <= self.ram
            && self.header.total_len() == Some(self.image_len)
    }
}

/// The RT slot image as an [`ImageFormat`] (feature `trust`: the hash),
/// the image (header excluded) at most `RAM` bytes. A host model of what
/// the board's own format (`firmware-imxrt`'s `RtImage`, whose `RAM` is
/// its SDRAM image region) accepts; mwctl's fake device runs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RtSlotImage<const RAM: u32 = { super::MAX_IMAGE }>;

/// Bytes hashed per read at commit.
#[cfg(feature = "trust")]
const HASH_STEP: usize = 256;

#[cfg(feature = "trust")]
impl<const RAM: u32> ImageFormat for RtSlotImage<RAM> {
    type Info = RtSlotInfo;

    /// Decodes the header and, when its length is the received length,
    /// hashes header fields and image (256-byte reads, bounded by `len`).
    fn inspect<E, R>(read: &mut R, len: u32) -> Result<Option<RtSlotInfo>, E>
    where
        R: FnMut(u32, &mut [u8]) -> Result<(), E>,
    {
        if (len as usize) < HEADER_LEN {
            return Ok(None);
        }
        let mut raw = [0u8; HEADER_LEN];
        read(0, &mut raw)?;
        let Ok(header) = RtHeader::decode(&raw) else {
            return Ok(None);
        };
        let mut hash_ok = false;
        if header.total_len() == Some(len) {
            let mut h = header.hasher();
            let mut buf = [0u8; HASH_STEP];
            let mut at = HEADER_LEN as u32;
            // Bounded: len / HASH_STEP + 1 reads.
            while at < len {
                let n = (len - at).min(HASH_STEP as u32) as usize;
                let b = buf.get_mut(..n).unwrap_or_default();
                read(at, b)?;
                h = h.update(b);
                at += n as u32;
            }
            hash_ok = h.finish() == header.sha256;
        }
        Ok(Some(RtSlotInfo {
            header,
            image_len: len,
            hash_ok,
            ram: RAM,
        }))
    }

    /// The header's version when it decodes and fits the slot and `RAM`
    /// (no hash); `tbyb` is always false (the RT trial is the boot
    /// record's, not the image's).
    fn slot_info<E, R>(read: &mut R, slot_size: u32) -> Result<SlotInfo, E>
    where
        R: FnMut(u32, &mut [u8]) -> Result<(), E>,
    {
        let mut raw = [0u8; HEADER_LEN];
        read(0, &mut raw)?;
        Ok(match RtHeader::decode(&raw) {
            Ok(h)
                if h.len != 0 && h.len <= RAM && h.total_len().is_some_and(|t| t <= slot_size) =>
            {
                SlotInfo {
                    version: Some(h.version),
                    tbyb: false,
                }
            }
            _ => SlotInfo::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    #[cfg(feature = "trust")]
    use std::vec::Vec;

    const V: Version = Version(0x0001_0002);

    /// A header written field by field from the documented layout (not by
    /// [`RtHeader::encode`]), with `sha256` filled in.
    fn layout(version: u32, len: u32, sha256: [u8; 32]) -> [u8; HEADER_LEN] {
        let mut h = [0u8; HEADER_LEN];
        h[..4].copy_from_slice(b"MWRT");
        h[4..6].copy_from_slice(&1u16.to_le_bytes());
        h[8..12].copy_from_slice(&version.to_le_bytes());
        h[12..16].copy_from_slice(&len.to_le_bytes());
        h[16..48].copy_from_slice(&sha256);
        h
    }

    #[test]
    fn rt15_the_documented_layout_decodes() {
        let raw = layout(V.0, 1000, [7; 32]);
        let h = RtHeader::decode(&raw).unwrap();
        assert_eq!((h.version, h.len, h.sha256), (V, 1000, [7; 32]));
        assert_eq!(h.encode(), raw);
        assert_eq!(&h.hashed_fields()[..], &raw[..16]);
        assert_eq!(h.total_len(), Some(1256));
        assert!(is_rt_image(&raw) && !is_rt_image(b"MWR") && !is_rt_image(b"UF2\n"));
        // Bytes after the header are not the header's business.
        let mut longer = [0xAA; HEADER_LEN + 10];
        longer[..HEADER_LEN].copy_from_slice(&raw);
        assert_eq!(RtHeader::decode(&longer), Ok(h));
    }

    #[test]
    fn rt15_each_header_defect_has_its_own_reason() {
        let good = layout(V.0, 1000, [7; 32]);
        let damaged = |at: usize, value: u8| {
            let mut b = good;
            b[at] = value;
            RtHeader::decode(&b)
        };
        assert_eq!(RtHeader::decode(&good[..255]), Err(RtImageError::Short));
        assert_eq!(RtHeader::decode(&[]), Err(RtImageError::Short));
        assert_eq!(damaged(0, b'X'), Err(RtImageError::BadMagic));
        assert_eq!(damaged(4, 2), Err(RtImageError::BadHeaderVersion));
        assert_eq!(damaged(5, 1), Err(RtImageError::BadHeaderVersion));
        assert_eq!(damaged(6, 1), Err(RtImageError::BadHeaderVersion));
        assert_eq!(damaged(7, 1), Err(RtImageError::BadHeaderVersion));
        assert_eq!(damaged(48, 1), Err(RtImageError::BadHeaderVersion));
        assert_eq!(damaged(255, 1), Err(RtImageError::BadHeaderVersion));
        let h = RtHeader::decode(&good).unwrap();
        // Lengths: exact, truncated, padded, too large, empty.
        assert_eq!(h.check_len(1256, 4096), Ok(()));
        assert_eq!(
            h.check_len(1255, 4096),
            Err(RtImageError::LengthMismatch {
                header: 1256,
                actual: 1255
            })
        );
        assert!(matches!(
            h.check_len(1257, 4096),
            Err(RtImageError::LengthMismatch { .. })
        ));
        assert_eq!(
            h.check_len(1256, 1255),
            Err(RtImageError::TooLarge {
                total: 1256,
                max: 1255
            })
        );
        let huge = RtHeader { len: u32::MAX, ..h };
        assert_eq!(huge.total_len(), None);
        assert!(matches!(
            huge.check_len(usize::MAX, u32::MAX),
            Err(RtImageError::TooLarge { .. })
        ));
        let empty = RtHeader { len: 0, ..h };
        assert_eq!(empty.check_len(256, 4096), Err(RtImageError::Empty));
        // The digest.
        assert_eq!(h.check_digest(&[7; 32]), Ok(()));
        assert_eq!(h.check_digest(&[8; 32]), Err(RtImageError::HashMismatch));
        let all = [
            RtImageError::Short,
            RtImageError::BadMagic,
            RtImageError::BadHeaderVersion,
            RtImageError::Empty,
            RtImageError::LengthMismatch {
                header: 0,
                actual: 0,
            },
            RtImageError::TooLarge { total: 0, max: 0 },
            RtImageError::HashMismatch,
        ];
        for (i, a) in all.iter().enumerate() {
            assert!(all[i + 1..].iter().all(|b| a.as_str() != b.as_str()));
        }
    }

    /// A slot image: header then `len` bytes.
    #[cfg(feature = "trust")]
    fn slot(len: usize) -> Vec<u8> {
        let image: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();
        let mut s = RtHeader::for_image(V, &image).unwrap().encode().to_vec();
        s.extend_from_slice(&image);
        s
    }

    #[cfg(feature = "trust")]
    #[test]
    fn rt15_the_hash_covers_header_fields_and_image() {
        use crate::loader::sha256::ImageHash;
        let s = slot(5000);
        let h = check(&s, 1 << 20).unwrap();
        assert_eq!((h.version, h.len), (V, 5000));
        let mut whole = s[..16].to_vec();
        whole.extend_from_slice(&s[HEADER_LEN..]);
        assert_eq!(ImageHash::digest(&whole), h.sha256);
        assert_eq!(s[..HEADER_LEN], layout(V.0, 5000, h.sha256));
        // The image, the version, the length and the hash are all checked.
        let damaged = |at: usize, value: u8| {
            let mut b = s.clone();
            b[at] = value;
            check(&b, 1 << 20)
        };
        assert_eq!(damaged(HEADER_LEN + 17, 0), Err(RtImageError::HashMismatch));
        assert_eq!(damaged(8, 3), Err(RtImageError::HashMismatch));
        assert_eq!(damaged(20, 0), Err(RtImageError::HashMismatch));
        assert!(matches!(
            damaged(13, 0x80),
            Err(RtImageError::LengthMismatch { .. })
        ));
        assert_eq!(damaged(0, 0), Err(RtImageError::BadMagic));
        assert!(matches!(
            check(&s, 4096),
            Err(RtImageError::TooLarge { .. })
        ));
        assert!(matches!(
            check(&s[..s.len() - 1], 1 << 20),
            Err(RtImageError::LengthMismatch { .. })
        ));
    }

    #[cfg(feature = "trust")]
    #[test]
    fn rt15_the_format_accepts_the_declared_version_of_an_intact_image() {
        let s = slot(3000);
        let len = s.len() as u32;
        let inspect = |b: &[u8], len: u32| {
            let mut read = |at: u32, buf: &mut [u8]| -> Result<(), ()> {
                let src = b.get(at as usize..at as usize + buf.len()).ok_or(())?;
                buf.copy_from_slice(src);
                Ok(())
            };
            RtSlotImage::<4096>::inspect(&mut read, len).unwrap()
        };
        let info = inspect(&s, len).unwrap();
        assert!(info.hash_ok && info.acceptable(V));
        assert!(!info.acceptable(Version(V.0 + 1)));
        // A received length other than the header's: not hashed, refused.
        let mut padded = s.clone();
        padded.extend_from_slice(&[0; 512]);
        let p = inspect(&padded, padded.len() as u32).unwrap();
        assert!(!p.hash_ok && !p.acceptable(V));
        let mut bad = s.clone();
        bad[HEADER_LEN] ^= 1;
        assert!(!inspect(&bad, len).unwrap().acceptable(V));
        // Larger than RAM: refused even when intact.
        let big = slot(5000);
        assert!(!inspect(&big, big.len() as u32).unwrap().acceptable(V));
        // Not an RT image, or shorter than a header.
        assert_eq!(inspect(&[0xFF; 512], 512), None);
        assert_eq!(inspect(&s, 100), None);
        // The slot summary: the version of a header that fits.
        let summary = |b: &[u8], size: u32| {
            let mut padded = b.to_vec();
            padded.resize(HEADER_LEN.max(b.len()), 0xFF);
            let mut read = |at: u32, buf: &mut [u8]| -> Result<(), ()> {
                let src = padded.get(at as usize..at as usize + buf.len()).ok_or(())?;
                buf.copy_from_slice(src);
                Ok(())
            };
            RtSlotImage::<4096>::slot_info(&mut read, size).unwrap()
        };
        assert_eq!(
            summary(&s, 8192),
            SlotInfo {
                version: Some(V),
                tbyb: false
            }
        );
        assert_eq!(summary(&s, 1024), SlotInfo::default());
        assert_eq!(summary(&big, 1 << 20), SlotInfo::default());
        assert_eq!(summary(&[0xFF; 256], 8192), SlotInfo::default());
    }
}
