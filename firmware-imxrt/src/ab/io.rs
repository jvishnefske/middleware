//! Byte-granular reads and writes over a NOR flash with a coarser read or
//! write granule (the HyperFlash's is one 16-bit word).
//!
//! [`read_at`] bounces a read the flash's `READ_SIZE` does not allow
//! through a small buffer; [`write_at`] pads a tail that is not a whole
//! `WRITE_SIZE` with `0xFF` (which programs nothing). Both refuse a
//! granule above [`MAX_GRANULE`] and a write that does not start on one.
//! Every loop is bounded by the length of the request.

use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};

/// Largest read or write granule supported.
pub const MAX_GRANULE: usize = 16;
/// Bytes read per bounced step.
const BOUNCE: usize = 256;

/// Why a byte-granular access failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoError<E> {
    /// The flash driver failed.
    Flash(E),
    /// The flash's granule is above [`MAX_GRANULE`], or a write does not
    /// start on one.
    Granule,
}

/// Reads `buf` at flash offset `at`.
pub fn read_at<F: ReadNorFlash>(
    flash: &mut F,
    at: u32,
    buf: &mut [u8],
) -> Result<(), IoError<F::Error>> {
    let g = F::READ_SIZE.max(1);
    if g > MAX_GRANULE {
        return Err(IoError::Granule);
    }
    if (at as usize).is_multiple_of(g) && buf.len().is_multiple_of(g) {
        return flash.read(at, buf).map_err(IoError::Flash);
    }
    let mut tmp = [0u8; BOUNCE + 2 * MAX_GRANULE];
    let mut done = 0usize;
    // Bounded: buf.len() / BOUNCE + 1 steps.
    while done < buf.len() {
        let n = (buf.len() - done).min(BOUNCE);
        let start = at as usize + done;
        let lo = start - start % g;
        let hi = (start + n).div_ceil(g) * g;
        let span = tmp.get_mut(..hi - lo).ok_or(IoError::Granule)?;
        let lo32 = u32::try_from(lo).map_err(|_| IoError::Granule)?;
        flash.read(lo32, span).map_err(IoError::Flash)?;
        let src = span
            .get(start - lo..start - lo + n)
            .ok_or(IoError::Granule)?;
        buf.get_mut(done..done + n)
            .ok_or(IoError::Granule)?
            .copy_from_slice(src);
        done += n;
    }
    Ok(())
}

/// Programs `data` at flash offset `at` (a multiple of the write granule),
/// padding a short tail with `0xFF`.
pub fn write_at<F: NorFlash>(flash: &mut F, at: u32, data: &[u8]) -> Result<(), IoError<F::Error>> {
    let g = F::WRITE_SIZE.max(1);
    if g > MAX_GRANULE || !(at as usize).is_multiple_of(g) {
        return Err(IoError::Granule);
    }
    let whole = data.len() - data.len() % g;
    let (head, tail) = data.split_at(whole);
    if !head.is_empty() {
        flash.write(at, head).map_err(IoError::Flash)?;
    }
    if !tail.is_empty() {
        let mut pad = [0xFFu8; MAX_GRANULE];
        pad.get_mut(..tail.len())
            .ok_or(IoError::Granule)?
            .copy_from_slice(tail);
        let off = u32::try_from(whole).map_err(|_| IoError::Granule)?;
        let p = pad.get(..g).ok_or(IoError::Granule)?;
        flash.write(at + off, p).map_err(IoError::Flash)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::ab::test_flash::BlockFlash;

    #[test]
    fn rt7_odd_reads_bounce_and_odd_tails_are_padded() {
        let mut f = BlockFlash::erased(crate::layout::BLOCK as usize);
        write_at(&mut f, 0, &[1, 2, 3]).unwrap();
        assert_eq!(f.bytes(0, 4), [1, 2, 3, 0xFF]);
        assert_eq!(write_at(&mut f, 1, &[1]), Err(IoError::Granule));
        f.program(
            4,
            &(0..=255u8).cycle().take(700).collect::<std::vec::Vec<u8>>(),
        );
        let mut buf = [0u8; 601];
        read_at(&mut f, 5, &mut buf).unwrap();
        assert_eq!(buf.as_slice(), f.bytes(5, 601).as_slice());
        let mut one = [0u8; 1];
        read_at(&mut f, 2, &mut one).unwrap();
        assert_eq!(one, [3]);
    }
}
