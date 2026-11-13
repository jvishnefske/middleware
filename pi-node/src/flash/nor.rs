//! The NOR flash rules the model follows, as pure functions: 4-byte
//! aligned reads and writes inside the device, writes that only clear
//! bits, 4 KiB sector erases to `0xFF`. They are the rules of the ESP and
//! RP flash drivers that esp-base's store and trust record were written
//! (and power-loss tested) against, so those keep their guarantees here.

use core::ops::Range;
use embedded_storage::nor_flash::NorFlashErrorKind;

use super::SECTOR;

/// Read and write granularity in bytes.
pub const WORD: u32 = 4;

/// The byte range `[offset, offset + len)` of a read or write, checked for
/// word alignment and for lying inside `capacity` bytes.
pub fn span(offset: u32, len: usize, capacity: usize) -> Result<Range<usize>, NorFlashErrorKind> {
    let w = WORD as usize;
    if !offset.is_multiple_of(WORD) || !len.is_multiple_of(w) {
        return Err(NorFlashErrorKind::NotAligned);
    }
    let start = offset as usize;
    let end = start.checked_add(len).ok_or(NorFlashErrorKind::OutOfBounds)?;
    if end > capacity {
        return Err(NorFlashErrorKind::OutOfBounds);
    }
    Ok(start..end)
}

/// The sectors `[from, to)` of an erase, checked for sector alignment and
/// for lying inside `capacity` bytes.
pub fn erase_sectors(from: u32, to: u32, capacity: usize) -> Result<Range<u32>, NorFlashErrorKind> {
    if !from.is_multiple_of(SECTOR) || !to.is_multiple_of(SECTOR) || from > to {
        return Err(NorFlashErrorKind::NotAligned);
    }
    if to as usize > capacity {
        return Err(NorFlashErrorKind::OutOfBounds);
    }
    Ok(from / SECTOR..to / SECTOR)
}

/// The sectors a non-empty byte range touches, in order.
pub fn sectors_of(bytes: &Range<usize>) -> Range<u32> {
    let s = SECTOR as usize;
    if bytes.is_empty() {
        return 0..0;
    }
    let first = bytes.start / s;
    let last = (bytes.end - 1) / s;
    (first as u32)..(last as u32 + 1)
}

/// The part of `bytes` inside sector `index`.
pub fn within(bytes: &Range<usize>, index: u32) -> Range<usize> {
    let s = SECTOR as usize;
    let lo = (index as usize) * s;
    bytes.start.max(lo)..bytes.end.min(lo + s)
}

/// Programs `src` onto `dst`: each bit can only go from 1 to 0.
pub fn program(dst: &mut [u8], src: &[u8]) {
    for (d, s) in dst.iter_mut().zip(src) {
        *d &= *s;
    }
}

/// Whether `bytes` read as erased flash.
pub fn is_erased(bytes: &[u8]) -> bool {
    bytes.iter().all(|b| *b == 0xFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pn3_spans_are_aligned_and_bounded() {
        assert_eq!(span(8, 16, 64), Ok(8..24));
        assert_eq!(span(0, 0, 0), Ok(0..0));
        assert_eq!(span(2, 4, 64), Err(NorFlashErrorKind::NotAligned));
        assert_eq!(span(4, 3, 64), Err(NorFlashErrorKind::NotAligned));
        assert_eq!(span(60, 8, 64), Err(NorFlashErrorKind::OutOfBounds));
        assert_eq!(span(u32::MAX - 3, 4, usize::MAX), Ok((u32::MAX - 3) as usize..u32::MAX as usize + 1));
        assert_eq!(erase_sectors(0, 8192, 8192), Ok(0..2));
        assert_eq!(erase_sectors(4096, 4096, 8192), Ok(1..1));
        assert_eq!(erase_sectors(1, 4096, 8192), Err(NorFlashErrorKind::NotAligned));
        assert_eq!(erase_sectors(8192, 4096, 8192), Err(NorFlashErrorKind::NotAligned));
        assert_eq!(erase_sectors(0, 12288, 8192), Err(NorFlashErrorKind::OutOfBounds));
    }

    #[test]
    fn pn3_writes_only_clear_bits_and_split_by_sector() {
        let mut d = [0xF0u8, 0xFF, 0x0F, 0xAA];
        program(&mut d, &[0xFF, 0x3C, 0xFF, 0x55]);
        assert_eq!(d, [0xF0, 0x3C, 0x0F, 0x00]);
        assert!(is_erased(&[0xFF; 8]));
        assert!(!is_erased(&[0xFF, 0xFE]));
        let r = 4092..4104;
        assert_eq!(sectors_of(&r), 0..2);
        assert_eq!(within(&r, 0), 4092..4096);
        assert_eq!(within(&r, 1), 4096..4104);
        assert_eq!(sectors_of(&(0..4096)), 0..1);
        assert_eq!(sectors_of(&(8..8)), 0..0);
    }
}
