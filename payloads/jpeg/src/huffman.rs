//! Huffman tables (DHT) and symbol decoding.
//!
//! A [`HuffTable`] is built from the 16 code-length counts and the symbol
//! list of a DHT entry, assigning canonical codes (JPEG Annex C). Decoding
//! uses an 8-bit lookahead table for codes of up to 8 bits (nearly every
//! code in camera images) and the canonical `maxcode`/`valoff` arrays
//! (Annex F.2.2.3, with an exclusive `limit` instead of `maxcode`) for
//! longer ones, so one symbol costs a table load or at most eight compares.
//! Memory: 256 × 2 + 2 × 17 × 4 + 256 bytes (~900 B) per table.
//!
//! The empty table is all zero bytes, so a decoder in a zero-initialised
//! static costs no data segment in the `.wasm`.

use crate::bits::BitReader;
use crate::error::{Error, Malformed};

/// One decoding table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HuffTable {
    /// Indexed by the next 8 bits: the length of the code they start with
    /// if it has at most 8 bits, `0` if it is longer (or no code). Lengths
    /// and symbols are separate byte arrays so that building fills byte
    /// runs (a bulk `memory.fill`, nearly free in wasmi) rather than 16-bit
    /// entries one by one.
    look_len: [u8; 256],
    /// Indexed like `look_len`: the symbol of that code.
    look_sym: [u8; 256],
    /// One past the largest code of each length 1..=16 (index = length),
    /// `0` if there is none.
    limit: [i32; 17],
    /// For each length, `symbol index − code` of its codes.
    valoff: [i32; 17],
    /// Symbols in code order.
    values: [u8; 256],
    /// A DHT segment defined this table.
    defined: bool,
}

impl HuffTable {
    /// An undefined table (every lookup fails).
    pub(crate) const EMPTY: HuffTable = HuffTable {
        look_len: [0; 256],
        look_sym: [0; 256],
        limit: [0; 17],
        valoff: [0; 17],
        values: [0; 256],
        defined: false,
    };

    /// Whether a DHT segment defined this table.
    pub(crate) const fn is_defined(&self) -> bool {
        self.defined
    }

    /// Builds a table from `counts[i]` = number of codes of length `i + 1`
    /// and the symbols in code order.
    ///
    /// Errors with [`Malformed::HuffmanTable`] if `symbols` is not exactly
    /// the sum of `counts`, longer than 256, or the counts over-fill the
    /// code space (libjpeg's check: the all-ones code of each length stays
    /// unused).
    #[inline(never)]
    pub(crate) fn build(counts: &[u8; 16], symbols: &[u8]) -> Result<HuffTable, Error> {
        let total: usize = counts.iter().map(|&c| usize::from(c)).sum();
        if total != symbols.len() || total > 256 {
            return Err(Malformed::HuffmanTable.into());
        }
        let mut t = HuffTable::EMPTY;
        t.defined = true;
        if let Some(dst) = t.values.get_mut(..symbols.len()) {
            dst.copy_from_slice(symbols);
        }
        let mut code: i32 = 0;
        let mut k: i32 = 0;
        for (len, &n) in (1u32..=16).zip(counts.iter()) {
            let n = i32::from(n);
            let slot = len as usize;
            if n > 0 {
                if let Some(v) = t.valoff.get_mut(slot) {
                    *v = k - code;
                }
                if let Some(m) = t.limit.get_mut(slot) {
                    *m = code + n;
                }
                if len <= 8 {
                    fill_lookup(&mut t, code, k, n, len);
                }
            }
            code += n;
            k += n;
            if code >= (1 << len) {
                return Err(Malformed::HuffmanTable.into());
            }
            code <<= 1;
        }
        Ok(t)
    }

    /// Decodes one symbol: one table load for codes of up to 8 bits.
    ///
    /// Error: [`Malformed::HuffmanCode`] for a bit pattern that is no code.
    /// Running past the end of the data is detected by the caller with
    /// [`BitReader::overrun`].
    #[inline(always)]
    pub(crate) fn decode(&self, br: &mut BitReader, data: &[u8]) -> Result<u8, Malformed> {
        br.ensure(data, 16);
        let i = br.peek8();
        let len = self.look_len.get(i).copied().unwrap_or(0);
        if len != 0 {
            br.skip(u32::from(len));
            return Ok(self.look_sym.get(i).copied().unwrap_or(0));
        }
        let entry = self.decode_long(br.peek16());
        if entry <= 0xFF {
            return Err(Malformed::HuffmanCode);
        }
        br.skip(u32::from(entry >> 8));
        Ok(entry as u8)
    }

    /// The slow path of [`decode`](Self::decode) for codes of 9 to 16 bits
    /// (about 3 of the ~70 symbols of a camera MCU): searches the next 16
    /// bits by length and returns `(length << 8) | symbol`, or 0 if they
    /// start with no code. Out of line and pure, so the fast path stays
    /// small and the bit reader is not borrowed.
    #[cold]
    #[inline(never)]
    fn decode_long(&self, code16: u32) -> u16 {
        for len in 9u16..17 {
            let code = (code16 >> (16 - len)) as i32;
            let slot = usize::from(len);
            let limit = self.limit.get(slot).copied().unwrap_or(0);
            if code < limit {
                let off = self.valoff.get(slot).copied().unwrap_or(0);
                return usize::try_from(code + off)
                    .ok()
                    .and_then(|i| self.values.get(i))
                    .map_or(0, |&sym| (len << 8) | u16::from(sym));
            }
        }
        0
    }
}

/// Fills the lookahead entries of the `n` codes of length `len` (≤ 8)
/// starting at `code`, whose symbols start at `values[k]`.
///
/// The runs are written with `copy_from_slice` from arrays built by a
/// repeat expression: both compile to bulk memory instructions, whereas
/// `fill` on a byte slice stays a loop that wasmi charges per byte.
fn fill_lookup(t: &mut HuffTable, code: i32, k: i32, n: i32, len: u32) {
    let shift = 8 - len;
    let count = 1usize << shift;
    let lens = [len as u8; 128];
    for i in 0..n {
        let sym = usize::try_from(k + i)
            .ok()
            .and_then(|j| t.values.get(j))
            .copied()
            .unwrap_or(0);
        let syms = [sym; 128];
        let first = ((code + i) << shift) as usize;
        let range = first..first.saturating_add(count);
        if let (Some(l), Some(s), Some(lv), Some(sv)) = (
            t.look_len.get_mut(range.clone()),
            t.look_sym.get_mut(range),
            lens.get(..count),
            syms.get(..count),
        ) && l.len() == lv.len()
            && s.len() == sv.len()
        {
            l.copy_from_slice(lv);
            s.copy_from_slice(sv);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Table with codes 0 → 'a' (1 bit), 10 → 'b' (2 bits), and nine-bit
    /// code 110000000 → 'c'.
    fn table() -> HuffTable {
        let mut counts = [0u8; 16];
        counts[0] = 1;
        counts[1] = 1;
        counts[8] = 1;
        HuffTable::build(&counts, b"abc").unwrap()
    }

    #[test]
    fn decodes_short_and_long_codes() {
        let t = table();
        // 0 | 10 | 110000000 | 0 → a b c a
        let data = [0b0101_1000, 0b0000_0000];
        let mut br = BitReader::new(0);
        assert_eq!(t.decode(&mut br, &data), Ok(b'a'));
        assert_eq!(t.decode(&mut br, &data), Ok(b'b'));
        assert_eq!(t.decode(&mut br, &data), Ok(b'c'));
        assert_eq!(t.decode(&mut br, &data), Ok(b'a'));
    }

    #[test]
    fn unknown_code_is_an_error() {
        let t = table();
        let data = [0xFF, 0x00, 0xFF, 0x00];
        let mut br = BitReader::new(0);
        assert_eq!(t.decode(&mut br, &data), Err(Malformed::HuffmanCode));
    }

    #[test]
    fn rejects_overfull_and_mismatched_tables() {
        let mut counts = [0u8; 16];
        counts[0] = 2; // codes 0 and 1: the all-ones code is used
        assert!(HuffTable::build(&counts, b"ab").is_err());
        counts[0] = 1;
        assert!(HuffTable::build(&counts, b"ab").is_err());
    }
}
