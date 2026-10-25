//! Layout of the payload flash partition.
//!
//! The `payload` data partition (see `partitions.csv`) holds one image:
//!
//! | offset in partition | content                                         |
//! |---------------------|-------------------------------------------------|
//! | `0x0000`            | [`Header`]: magic, length, CRC-32 (12 bytes)    |
//! | `0x1000`            | image bytes, then erased (`0xFF`) to the end    |
//!
//! A transfer erases the header sector and the sectors the image needs
//! ([`Region::erase_span`]), writes the image in order, and only on activate
//! writes the header. An interrupted transfer therefore leaves an erased
//! header, which [`Header::decode`] rejects, and the device boots with no
//! payload. On boot the shell decodes the header and verifies the image
//! CRC-32 before running it.
//!
//! NOR flash is written in 4-byte words, while loader chunks may have any
//! length; [`WordWriter`] buffers the trailing partial word between chunks
//! and pads the final one with `0xFF` (the erased value, so the padding is a
//! no-op on flash and outside the CRC).

/// Flash erase granularity.
pub const SECTOR: u32 = 4096;

/// Flash write granularity.
pub const WORD: u32 = 4;

/// Header magic, ASCII `MWPL` little-endian.
pub const MAGIC: u32 = u32::from_le_bytes(*b"MWPL");

/// Encoded header length.
pub const HEADER_LEN: usize = 12;

/// The installed image's description, written after the image on activate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Image length in bytes.
    pub len: u32,
    /// CRC-32 of the image.
    pub crc: u32,
}

impl Header {
    /// `[magic u32 LE][len u32 LE][crc u32 LE]`.
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        for (dst, word) in out.as_chunks_mut::<4>().0.iter_mut().zip([MAGIC, self.len, self.crc]) {
            *dst = word.to_le_bytes();
        }
        out
    }

    /// Parses a header; `None` for a bad magic (erased or never written), a
    /// zero length, or a length beyond `capacity`.
    pub fn decode(bytes: &[u8], capacity: u32) -> Option<Self> {
        let word = |i: usize| -> Option<u32> {
            let b = bytes.get(i * 4..i * 4 + 4)?;
            Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        };
        if word(0)? != MAGIC {
            return None;
        }
        let len = word(1)?;
        let crc = word(2)?;
        (len != 0 && len <= capacity).then_some(Header { len, crc })
    }
}

/// The payload partition, by size. Offsets are relative to its start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    size: u32,
}

impl Region {
    /// A partition of `size` bytes (rounded down to whole sectors).
    pub const fn new(size: u32) -> Self {
        Region { size: size - size % SECTOR }
    }

    /// Offset of the header.
    pub const fn header_offset(&self) -> u32 {
        0
    }

    /// Offset of the first image byte.
    pub const fn data_offset(&self) -> u32 {
        SECTOR
    }

    /// Largest image the partition holds.
    pub const fn capacity(&self) -> u32 {
        self.size.saturating_sub(SECTOR)
    }

    /// Sector-aligned `[from, to)` to erase before writing an image of `len`
    /// bytes: the header sector plus the sectors the image covers. `None`
    /// if it does not fit.
    pub fn erase_span(&self, len: u32) -> Option<(u32, u32)> {
        if len > self.capacity() {
            return None;
        }
        let sectors = len.div_ceil(SECTOR);
        let to = SECTOR.checked_add(sectors.checked_mul(SECTOR)?)?;
        Some((0, to))
    }
}

/// A word write: `bytes` at image offset `offset` (word aligned).
pub type Word = (u32, [u8; WORD as usize]);

/// The aligned writes for one chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WritePlan<'a> {
    /// A completed word assembled from earlier bytes and the chunk's head.
    pub head: Option<Word>,
    /// A word-aligned run taken directly from the chunk.
    pub body: Option<(u32, &'a [u8])>,
}

/// A chunk did not start where the previous one ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutOfOrder;

/// Turns in-order byte chunks into word-aligned writes. Value typed:
/// [`WordWriter::push`] returns the next state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WordWriter {
    /// Image offset of the next expected byte.
    next: u32,
    tail: [u8; WORD as usize],
    tail_len: usize,
}

impl Default for WordWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl WordWriter {
    /// Expects the first chunk at offset 0.
    pub const fn new() -> Self {
        WordWriter { next: 0, tail: [0xFF; WORD as usize], tail_len: 0 }
    }

    /// Image offset of the next expected byte.
    pub const fn position(&self) -> u32 {
        self.next
    }

    /// Accepts `data` at `offset` and returns the writes it completes.
    pub fn push<'a>(self, offset: u32, data: &'a [u8]) -> Result<(Self, WritePlan<'a>), OutOfOrder> {
        if offset != self.next {
            return Err(OutOfOrder);
        }
        let n = u32::try_from(data.len()).map_err(|_| OutOfOrder)?;
        let end = offset.checked_add(n).ok_or(OutOfOrder)?;
        let mut tail = self.tail;
        let mut tail_len = self.tail_len;
        let word = WORD as usize;
        // Complete a pending partial word first.
        let fill = if tail_len > 0 { (word - tail_len).min(data.len()) } else { 0 };
        let (front, rest) = data.split_at(fill);
        for (dst, src) in tail.iter_mut().skip(tail_len).zip(front) {
            *dst = *src;
        }
        tail_len += fill;
        let mut head = None;
        if tail_len == word {
            // `offset + fill` is word aligned here, so this cannot underflow.
            head = Some((offset + fill as u32 - WORD, tail));
            tail = [0xFF; WORD as usize];
            tail_len = 0;
        }
        if tail_len > 0 {
            // The chunk ended inside the pending word.
            let next = WordWriter { next: end, tail, tail_len };
            return Ok((next, WritePlan { head, body: None }));
        }
        let aligned = rest.len() - rest.len() % word;
        let (bulk, remainder) = rest.split_at(aligned);
        let body = (!bulk.is_empty()).then_some((offset + fill as u32, bulk));
        for (dst, src) in tail.iter_mut().zip(remainder) {
            *dst = *src;
        }
        let next = WordWriter { next: end, tail, tail_len: remainder.len() };
        Ok((next, WritePlan { head, body }))
    }

    /// The final partial word padded with `0xFF`, if any.
    pub fn finish(self) -> Option<Word> {
        (self.tail_len > 0).then(|| (self.next - self.tail_len as u32, self.tail))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trips_and_rejects_erased_or_oversized() {
        let h = Header { len: 1234, crc: 0xDEAD_BEEF };
        assert_eq!(Header::decode(&h.encode(), 4096), Some(h));
        assert_eq!(Header::decode(&[0xFF; HEADER_LEN], u32::MAX), None);
        assert_eq!(Header::decode(&h.encode(), 1233), None);
        assert_eq!(Header::decode(&Header { len: 0, crc: 0 }.encode(), 10), None);
        assert_eq!(Header::decode(&h.encode()[..11], 4096), None);
    }

    #[test]
    fn region_erases_header_and_image_sectors() {
        let r = Region::new(256 * 1024);
        assert_eq!(r.capacity(), 252 * 1024);
        assert_eq!(r.data_offset(), SECTOR);
        assert_eq!(r.erase_span(1), Some((0, 2 * SECTOR)));
        assert_eq!(r.erase_span(SECTOR), Some((0, 2 * SECTOR)));
        assert_eq!(r.erase_span(SECTOR + 1), Some((0, 3 * SECTOR)));
        assert_eq!(r.erase_span(r.capacity()), Some((0, 256 * 1024)));
        assert_eq!(r.erase_span(r.capacity() + 1), None);
        assert_eq!(Region::new(SECTOR * 2 + 5).capacity(), SECTOR);
        assert_eq!(Region::new(100).capacity(), 0);
    }

    /// Replays chunks through the writer onto a simulated flash and checks
    /// every write is aligned and the result equals the image.
    fn replay(image: &[u8], sizes: &[usize]) {
        let mut flash = [0xFFu8; 256];
        let mut write = |off: u32, bytes: &[u8]| {
            assert_eq!(off % WORD, 0);
            assert_eq!(bytes.len() % WORD as usize, 0);
            for (i, b) in bytes.iter().enumerate() {
                let cell = &mut flash[off as usize + i];
                assert_eq!(*cell, 0xFF, "double write at {}", off as usize + i);
                *cell = *b;
            }
        };
        let mut w = WordWriter::new();
        let mut at = 0usize;
        for &s in sizes.iter().cycle() {
            if at >= image.len() {
                break;
            }
            let end = (at + s).min(image.len());
            let (next, plan) = w.push(at as u32, &image[at..end]).unwrap();
            if let Some((off, word)) = plan.head {
                write(off, &word);
            }
            if let Some((off, bytes)) = plan.body {
                write(off, bytes);
            }
            w = next;
            at = end;
        }
        if let Some((off, word)) = w.finish() {
            write(off, &word);
        }
        assert_eq!(&flash[..image.len()], image);
        assert!(flash[image.len()..].iter().all(|&b| b == 0xFF));
    }

    #[test]
    fn word_writer_aligns_any_chunking() {
        let image: [u8; 101] = core::array::from_fn(|i| (i * 7 + 1) as u8);
        for sizes in [&[1usize][..], &[3], &[4], &[5], &[2, 7], &[64], &[101], &[1, 4, 6, 3]] {
            replay(&image, sizes);
        }
        replay(&image[..8], &[8]);
        replay(&image[..2], &[1]);
    }

    #[test]
    fn word_writer_rejects_out_of_order() {
        let (w, _) = WordWriter::new().push(0, &[1, 2, 3]).unwrap();
        assert_eq!(w.position(), 3);
        assert_eq!(w.push(4, &[0]), Err(OutOfOrder));
        assert_eq!(WordWriter::new().finish(), None);
    }
}
