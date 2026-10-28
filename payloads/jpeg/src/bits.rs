//! Bit reader over the entropy-coded segment.
//!
//! [`BitReader`] holds only a byte position into the caller's JPEG slice and
//! a 32-bit accumulator; the slice itself is passed to every call, so the
//! decoder never borrows or copies the input between calls.
//!
//! * A stuffed `FF 00` pair yields the data byte `FF`.
//! * Any other `FF xx` is a marker. The reader stops in front of it and
//!   feeds zero bits from then on (as libjpeg does), so a decoder that peeks
//!   past the last code of the scan, or a corrupt stream that hits a marker
//!   early, still makes progress in bounded time. [`BitReader::restart`]
//!   consumes the marker.
//! * The end of the slice also feeds zero bits, but *consuming* one of them
//!   is [`Error::Truncated`]: peeking past the end is normal near the end of
//!   the scan; needing those bits means the data was cut short. Synthetic
//!   bits are counted in `pad` (they are always the last `pad` buffered
//!   bits), so [`BitReader::overrun`] can check this once per block instead
//!   of on every read.
//!
//! The accumulator is left-aligned: the next bit is bit 31.
//!
//! # Cost model
//!
//! wasmi charges fuel when it enters a function, loop or `if` for the
//! *static* size of the code up to the next nested loop or `if`, whichever
//! branches run (LLVM's `block`/`br_if` regions do not start a new charge).
//! So the refill loop in [`ensure`](BitReader::ensure) is only entered when
//! fewer bits than needed are buffered, its rare cases (stuffing, markers,
//! the end of the data) live in an out-of-line function, and the
//! per-symbol fast path is a compare, a shift and a subtract.

use crate::error::{Error, Malformed};

/// Read position in the entropy-coded data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BitReader {
    /// Index of the next byte to load.
    pos: usize,
    /// Buffered bits, left-aligned.
    acc: u32,
    /// Number of valid bits in `acc` (0..=32).
    nbits: u32,
    /// How many of the last `nbits` bits are synthetic zeros fed after the
    /// end of the data or after a marker; `nbits < pad` means some were
    /// consumed.
    pad: u32,
    /// The reader has stopped in front of a marker.
    marker: bool,
}

impl BitReader {
    /// A reader positioned at byte `pos` (the first entropy-coded byte).
    pub(crate) const fn new(pos: usize) -> Self {
        BitReader {
            pos,
            acc: 0,
            nbits: 0,
            pad: 0,
            marker: false,
        }
    }

    /// Makes sure at least `n` (≤ 25) bits are buffered, loading bytes until
    /// more than 24 are (at most four).
    ///
    /// The loop sits behind the `nbits < n` test so that wasmi only charges
    /// its body when a refill happens; plain data bytes are handled inline
    /// and everything else (stuffing, markers, the end of the data) by the
    /// out-of-line pure function [`special_byte`], so the reader is never
    /// borrowed by a call and stays in wasm locals.
    #[inline(always)]
    pub(crate) fn ensure(&mut self, data: &[u8], n: u32) {
        if self.nbits < n {
            for _ in 0..4 {
                let byte = match data.get(self.pos) {
                    Some(&b) if b != 0xFF && !self.marker => {
                        self.pos = self.pos.wrapping_add(1);
                        b
                    }
                    _ => {
                        let s = special_byte(data, self.pos, self.marker);
                        self.pos = self.pos.wrapping_add(s.advance);
                        self.pad = self.pad.saturating_add(if s.synthetic { 8 } else { 0 });
                        self.marker |= s.marker;
                        s.byte
                    }
                };
                self.acc |= u32::from(byte) << (24 - self.nbits.min(24));
                self.nbits += 8;
                if self.nbits > 24 {
                    break;
                }
            }
        }
    }

    /// The next 8 bits (buffered or not), for the Huffman lookahead table.
    #[inline(always)]
    pub(crate) fn peek8(&self) -> usize {
        (self.acc >> 24) as usize
    }

    /// The next 16 bits, for the long-code search.
    #[inline(always)]
    pub(crate) fn peek16(&self) -> u32 {
        self.acc >> 16
    }

    /// Drops `n` (0..=31) buffered bits.
    #[inline(always)]
    pub(crate) fn skip(&mut self, n: u32) {
        self.acc = self.acc.checked_shl(n).unwrap_or(0);
        self.nbits = self.nbits.saturating_sub(n);
    }

    /// Reads `n` (1..=16) bits as an unsigned number.
    #[inline(always)]
    pub(crate) fn bits(&mut self, data: &[u8], n: u32) -> u32 {
        self.ensure(data, n);
        let v = self.acc.checked_shr(32 - n).unwrap_or(0);
        self.skip(n);
        v
    }

    /// Whether bits fed after the end of the data (not after a marker) have
    /// been consumed: the data was truncated.
    #[inline(always)]
    pub(crate) fn overrun(&self) -> bool {
        self.nbits < self.pad && !self.marker
    }

    /// Ends a restart interval: drops the buffered bits, skips any bytes up
    /// to the next marker, and consumes it if it is `RSTn` with
    /// `n == expected` (0..=7).
    ///
    /// Errors: [`Malformed::Restart`] for another marker,
    /// [`Error::Truncated`] if the data ends first. The scan is bounded by
    /// the slice length.
    #[inline(never)]
    pub(crate) fn restart(&mut self, data: &[u8], expected: u8) -> Result<(), Error> {
        *self = BitReader::new(self.pos);
        let rest = data.get(self.pos..).ok_or(Error::Truncated)?;
        // Find `FF xx` with `xx` neither 00 (stuffing) nor FF (fill).
        let mut found = None;
        let mut prev_ff = false;
        for (i, &b) in rest.iter().enumerate() {
            if prev_ff && b != 0x00 && b != 0xFF {
                found = Some((i, b));
                break;
            }
            prev_ff = b == 0xFF;
        }
        let (i, code) = found.ok_or(Error::Truncated)?;
        if code != 0xD0 | (expected & 7) {
            return Err(Malformed::Restart.into());
        }
        self.pos = self.pos.saturating_add(i).saturating_add(1);
        Ok(())
    }
}

/// What [`special_byte`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Special {
    /// The byte to shift in.
    byte: u8,
    /// Bytes consumed from the data (0, 1 or 2).
    advance: usize,
    /// `byte` is a synthetic zero.
    synthetic: bool,
    /// The reader has reached a marker.
    marker: bool,
}

/// The byte at `pos` when it is not a plain data byte: `FF 00` is a data
/// `FF` (two bytes consumed); `FF xx` is a marker, after which (as after
/// the end of the data) zeros are synthesised.
#[inline(never)]
fn special_byte(data: &[u8], pos: usize, marker: bool) -> Special {
    let zero = Special {
        byte: 0,
        advance: 0,
        synthetic: true,
        marker,
    };
    if marker {
        return zero;
    }
    match (data.get(pos), data.get(pos.wrapping_add(1))) {
        (Some(&0xFF), Some(&0x00)) => Special {
            byte: 0xFF,
            advance: 2,
            synthetic: false,
            marker: false,
        },
        (Some(&0xFF), Some(_)) => Special {
            marker: true,
            ..zero
        },
        (Some(&b), _) if b != 0xFF => Special {
            byte: b,
            advance: 1,
            synthetic: false,
            marker: false,
        },
        _ => zero,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_bits_msb_first() {
        let data = [0b1010_0000, 0x0F];
        let mut br = BitReader::new(0);
        assert_eq!(br.bits(&data, 3), 0b101);
        assert_eq!(br.bits(&data, 9), 0);
        assert_eq!(br.bits(&data, 4), 0xF);
        assert!(!br.overrun());
    }

    #[test]
    fn unstuffs_ff00() {
        let data = [0xFF, 0x00, 0x80];
        let mut br = BitReader::new(0);
        assert_eq!(br.bits(&data, 8), 0xFF);
        assert_eq!(br.bits(&data, 1), 1);
    }

    #[test]
    fn end_of_data_is_an_overrun_only_when_consumed() {
        let data = [0xAB];
        let mut br = BitReader::new(0);
        br.ensure(&data, 16);
        assert_eq!(br.peek16(), 0xAB00);
        br.skip(8);
        assert!(!br.overrun());
        br.skip(1);
        assert!(br.overrun());
    }

    #[test]
    fn marker_feeds_zeros_without_overrun() {
        let data = [0x80, 0xFF, 0xD9];
        let mut br = BitReader::new(0);
        assert_eq!(br.bits(&data, 8), 0x80);
        assert_eq!(br.bits(&data, 16), 0);
        assert!(!br.overrun());
    }

    #[test]
    fn restart_checks_number() {
        let data = [0xFF, 0xD3, 0x55];
        let mut br = BitReader::new(0);
        assert_eq!(br.restart(&data, 3), Ok(()));
        assert_eq!(br.bits(&data, 8), 0x55);
        let mut br = BitReader::new(0);
        assert_eq!(br.restart(&data, 4), Err(Malformed::Restart.into()));
        let mut br = BitReader::new(2);
        assert_eq!(br.restart(&data, 4), Err(Error::Truncated));
    }
}
