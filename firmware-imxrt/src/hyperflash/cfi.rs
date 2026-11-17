//! The CFI (Common Flash Interface) query, parsed.
//!
//! After [`cfi_enter`](super::plan::cfi_enter), the flash answers CFI
//! queries: word address `0x10 + i` holds query byte `0x10 + i` in its low
//! byte. [`cfi_read`](super::plan::cfi_read) fetches words 0x10..0x40,
//! high byte first on the bus; [`parse_cfi`] takes those received bytes.
//!
//! Fields read (CFI 2.0 / JESD68 offsets):
//!
//! | offset | field |
//! |---|---|
//! | 0x10..0x12 | "QRY" |
//! | 0x27 | device size, 2^n bytes |
//! | 0x2A..0x2B | max bytes in a multi-byte (buffer) write, 2^n, little-endian |
//! | 0x2C | number of erase block regions |
//! | 0x2D + 4r | region r: blocks - 1 (u16 LE), block size / 256 (u16 LE) |
//!
//! [`CfiInfo::check_s26ks512s`] confirms on hardware what the flash map
//! assumes: 64 MiB, a 512-byte write buffer and one region of 256 uniform
//! 256 KiB sectors (a part with 4 KiB parameter sectors would show more
//! regions and is refused).

use heapless::Vec;

use super::plan::CFI_QUERY_WORDS;
use super::{FLASH_SIZE, SECTOR_COUNT, SECTOR_SIZE, WRITE_BUFFER_SIZE};

/// The most erase block regions kept.
pub const MAX_REGIONS: usize = 4;

/// One erase block region: `blocks` blocks of `block_size` bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EraseRegion {
    /// Number of blocks.
    pub blocks: u32,
    /// Block size in bytes.
    pub block_size: u32,
}

/// The facts the flash reports about itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CfiInfo {
    device_size: u32,
    write_buffer: u32,
    regions: Vec<EraseRegion, MAX_REGIONS>,
}

/// Why a CFI query was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CfiError {
    /// Fewer bytes than the fields need.
    Truncated,
    /// No "QRY" signature (not in CFI mode, or a bad read).
    NoSignature,
    /// A size exponent too large for a 32-bit address space.
    SizeOutOfRange,
    /// Zero, or more than [`MAX_REGIONS`], erase regions.
    BadRegionCount,
    /// The regions do not add up to the device size.
    RegionsDisagree,
}

/// Where the parsed info differs from the S26KS512S the firmware assumes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CfiMismatch {
    /// Not 64 MiB.
    DeviceSize(u32),
    /// Not a 512-byte write buffer.
    WriteBuffer(u32),
    /// Not exactly one region of 256 x 256 KiB.
    NotUniform,
}

/// Query byte at CFI offset `offset` from the received bytes (word
/// `offset`, low byte = second byte on the bus).
fn query_byte(raw: &[u8], offset: usize) -> Result<u8, CfiError> {
    let word = offset.checked_sub(0x10).ok_or(CfiError::Truncated)?;
    raw.get(2 * word + 1).copied().ok_or(CfiError::Truncated)
}

fn query_u16(raw: &[u8], offset: usize) -> Result<u16, CfiError> {
    Ok(u16::from_le_bytes([
        query_byte(raw, offset)?,
        query_byte(raw, offset + 1)?,
    ]))
}

fn pow2(exp: u8) -> Result<u32, CfiError> {
    1u32.checked_shl(exp as u32).ok_or(CfiError::SizeOutOfRange)
}

/// Parse the bytes [`cfi_read`](super::plan::cfi_read) received (words
/// from 0x10, two bytes each, high byte first).
pub fn parse_cfi(raw: &[u8]) -> Result<CfiInfo, CfiError> {
    let raw = raw.get(..(CFI_QUERY_WORDS as usize * 2)).unwrap_or(raw);
    let sig = [
        query_byte(raw, 0x10)?,
        query_byte(raw, 0x11)?,
        query_byte(raw, 0x12)?,
    ];
    if &sig != b"QRY" {
        return Err(CfiError::NoSignature);
    }
    let device_size = pow2(query_byte(raw, 0x27)?)?;
    let wb_exp = query_u16(raw, 0x2A)?;
    let write_buffer = pow2(u8::try_from(wb_exp).map_err(|_| CfiError::SizeOutOfRange)?)?;
    let count = query_byte(raw, 0x2C)? as usize;
    if count == 0 || count > MAX_REGIONS {
        return Err(CfiError::BadRegionCount);
    }
    let mut regions = Vec::new();
    let mut total: u64 = 0;
    for r in 0..count {
        let base = 0x2D + 4 * r;
        let blocks = query_u16(raw, base)? as u32 + 1;
        let block_size = match query_u16(raw, base + 2)? {
            // CFI: 0 means 128 bytes.
            0 => 128,
            z => z as u32 * 256,
        };
        total += blocks as u64 * block_size as u64;
        regions
            .push(EraseRegion { blocks, block_size })
            .map_err(|_| CfiError::BadRegionCount)?;
    }
    if total != device_size as u64 {
        return Err(CfiError::RegionsDisagree);
    }
    Ok(CfiInfo {
        device_size,
        write_buffer,
        regions,
    })
}

impl CfiInfo {
    /// Device size in bytes.
    pub fn device_size(&self) -> u32 {
        self.device_size
    }

    /// Write buffer (multi-byte program) size in bytes.
    pub fn write_buffer_size(&self) -> u32 {
        self.write_buffer
    }

    /// The erase block regions, lowest address first.
    pub fn erase_regions(&self) -> &[EraseRegion] {
        &self.regions
    }

    /// The sector size when every sector is the same size.
    pub fn uniform_sector_size(&self) -> Option<u32> {
        match self.regions.as_slice() {
            [only] => Some(only.block_size),
            _ => None,
        }
    }

    /// Confirm 64 MiB, a 512-byte write buffer and 256 uniform 256 KiB
    /// sectors.
    pub fn check_s26ks512s(&self) -> Result<(), CfiMismatch> {
        if self.device_size != FLASH_SIZE {
            return Err(CfiMismatch::DeviceSize(self.device_size));
        }
        if self.write_buffer != WRITE_BUFFER_SIZE {
            return Err(CfiMismatch::WriteBuffer(self.write_buffer));
        }
        match self.regions.as_slice() {
            [EraseRegion { blocks, block_size }]
                if *blocks == SECTOR_COUNT && *block_size == SECTOR_SIZE =>
            {
                Ok(())
            }
            _ => Err(CfiMismatch::NotUniform),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Received bytes for query bytes 0x10.. (`q[i]` = byte 0x10 + i).
    fn raw(q: &[(usize, u8)]) -> [u8; 96] {
        let mut out = [0u8; 96];
        for &(off, b) in q {
            out[2 * (off - 0x10) + 1] = b;
        }
        out
    }

    fn s26ks512s() -> [u8; 96] {
        raw(&[
            (0x10, b'Q'),
            (0x11, b'R'),
            (0x12, b'Y'),
            (0x27, 0x1A), // 64 MiB
            (0x2A, 0x09), // 512-byte buffer
            (0x2C, 0x01),
            (0x2D, 0xFF), // 256 blocks
            (0x2E, 0x00),
            (0x2F, 0x00), // 0x0400 * 256 = 256 KiB
            (0x30, 0x04),
        ])
    }

    #[test]
    fn rt4_cfi_gives_size_regions_and_write_buffer() {
        let Ok(info) = parse_cfi(&s26ks512s()) else {
            panic!("valid CFI")
        };
        assert_eq!(info.device_size(), 64 * 1024 * 1024);
        assert_eq!(info.write_buffer_size(), 512);
        assert_eq!(
            info.erase_regions(),
            &[EraseRegion {
                blocks: 256,
                block_size: 256 * 1024
            }]
        );
        assert_eq!(info.uniform_sector_size(), Some(256 * 1024));
        assert_eq!(info.check_s26ks512s(), Ok(()));
    }

    #[test]
    fn rt4_cfi_refuses_parameter_sectors_and_bad_queries() {
        // Eight 4 KiB parameter sectors at the bottom + 255 x 256 KiB.
        let mut r = s26ks512s();
        fn put(r: &mut [u8; 96], off: usize, b: u8) {
            r[2 * (off - 0x10) + 1] = b;
        }
        put(&mut r, 0x2C, 2);
        put(&mut r, 0x2D, 0x07);
        put(&mut r, 0x2E, 0x00);
        put(&mut r, 0x2F, 0x10);
        put(&mut r, 0x30, 0x00);
        put(&mut r, 0x31, 0xFE);
        put(&mut r, 0x32, 0x00);
        put(&mut r, 0x33, 0x00);
        put(&mut r, 0x34, 0x04);
        // 8 * 4 KiB + 255 * 256 KiB != 64 MiB: refused as inconsistent.
        assert_eq!(parse_cfi(&r), Err(CfiError::RegionsDisagree));
        // A consistent two-region part: 64 x 4 KiB + 255 x 256 KiB.
        put(&mut r, 0x2D, 0x3F);
        let Ok(info) = parse_cfi(&r) else {
            panic!("consistent two-region CFI")
        };
        assert_eq!(info.uniform_sector_size(), None);
        assert_eq!(info.check_s26ks512s(), Err(CfiMismatch::NotUniform));

        let mut no_sig = s26ks512s();
        no_sig[1] = b'X';
        assert_eq!(parse_cfi(&no_sig), Err(CfiError::NoSignature));
        assert_eq!(parse_cfi(&s26ks512s()[..20]), Err(CfiError::Truncated));
        let mut big = s26ks512s();
        big[2 * (0x27 - 0x10) + 1] = 40;
        assert_eq!(parse_cfi(&big), Err(CfiError::SizeOutOfRange));
        let mut none = s26ks512s();
        none[2 * (0x2C - 0x10) + 1] = 0;
        assert_eq!(parse_cfi(&none), Err(CfiError::BadRegionCount));
        let mut small_buf = s26ks512s();
        small_buf[2 * (0x2A - 0x10) + 1] = 8;
        let Ok(info) = parse_cfi(&small_buf) else {
            panic!("valid CFI")
        };
        assert_eq!(info.check_s26ks512s(), Err(CfiMismatch::WriteBuffer(256)));
    }
}
