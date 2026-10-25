//! Layout of the payload flash partition.
//!
//! The `payload` data partition (see `partitions.csv`) is split into two
//! equal slots, [`Slot::A`] and [`Slot::B`], each a whole number of 4 KiB
//! sectors (`slot = (size / 2)` rounded down to a sector). Offsets are
//! relative to the partition start:
//!
//! | offset            | content                                              |
//! |-------------------|------------------------------------------------------|
//! | `0`               | slot A [`Header`]: magic, length, CRC-32, generation |
//! | `0x1000`          | slot A image bytes, then erased (`0xFF`)             |
//! | `slot`            | slot B [`Header`]                                    |
//! | `slot + 0x1000`   | slot B image bytes, then erased (`0xFF`)             |
//!
//! For the 256 KiB partition (ESP32 family and ESP8266) `slot` is
//! `0x20000`, so each slot holds an image of up to 124 KiB
//! ([`Region::capacity`] is per slot).
//!
//! A transfer erases the *inactive* slot's header sector and the sectors the
//! image needs ([`Region::erase_span`]), writes the image in order, and only
//! on activate writes that slot's header: first the length, CRC and
//! generation words, then the magic word, so a header whose magic reads
//! back intact was written completely. The active slot is never touched, so
//! an interrupted or rejected transfer leaves the previous image installed.
//!
//! On boot each slot is valid when its header decodes and its image matches
//! the header's CRC-32. [`newest`] picks the valid slot with the newer
//! [`Generation`]; if neither is valid the partition is empty. Each install
//! takes the active slot's generation plus one (wrapping), so the two slots
//! always differ by one and the comparison is serial-number arithmetic
//! ([`Generation::is_newer_than`]); a tie or an ambiguous distance (exactly
//! half the range, which a correct writer never produces) resolves to
//! slot A.
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

/// Encoded header length: magic, length, CRC-32, generation.
pub const HEADER_LEN: usize = 16;

/// Install sequence number of a slot's image. Compared with serial-number
/// arithmetic, so it may wrap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Generation(pub u32);

impl Generation {
    /// Generation of the first image written to an empty partition.
    pub const FIRST: Generation = Generation(1);

    /// The generation after this one (wrapping).
    pub const fn next(self) -> Self {
        Generation(self.0.wrapping_add(1))
    }

    /// Whether `self` was written after `other`: the wrapping distance from
    /// `other` to `self` is positive and less than half the range. Equal
    /// generations and a distance of exactly `2^31` are not newer either
    /// way.
    pub const fn is_newer_than(self, other: Generation) -> bool {
        (self.0.wrapping_sub(other.0) as i32) > 0
    }
}

/// The installed image's description, written after the image on activate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Image length in bytes.
    pub len: u32,
    /// CRC-32 of the image.
    pub crc: u32,
    /// Install sequence number.
    pub generation: Generation,
}

impl Header {
    /// `[magic u32 LE][len u32 LE][crc u32 LE][generation u32 LE]`.
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        let words = [MAGIC, self.len, self.crc, self.generation.0];
        for (dst, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(words) {
            *dst = word.to_le_bytes();
        }
        out
    }

    /// Parses a header; `None` for a bad magic (erased, never written, or
    /// torn), a zero length, or a length beyond `capacity`.
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
        let generation = Generation(word(3)?);
        (len != 0 && len <= capacity).then_some(Header { len, crc, generation })
    }
}

/// One of the partition's two image slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// The slot at the start of the partition.
    A,
    /// The slot in the second half of the partition.
    B,
}

impl Slot {
    /// Both slots, in tie-break order.
    pub const ALL: [Slot; 2] = [Slot::A, Slot::B];

    /// The other slot.
    pub const fn other(self) -> Self {
        match self {
            Slot::A => Slot::B,
            Slot::B => Slot::A,
        }
    }
}

/// Which slot holds the image to boot, given each slot's header if the slot
/// is valid (header decodes and image CRC matches): the newer generation,
/// slot A on a tie or ambiguous distance, `None` if neither is valid.
pub fn newest(a: Option<Header>, b: Option<Header>) -> Option<(Slot, Header)> {
    match (a, b) {
        (None, None) => None,
        (Some(a), None) => Some((Slot::A, a)),
        (None, Some(b)) => Some((Slot::B, b)),
        (Some(a), Some(b)) if b.generation.is_newer_than(a.generation) => Some((Slot::B, b)),
        (Some(a), Some(_)) => Some((Slot::A, a)),
    }
}

/// The payload partition, by size, split into two slots. Offsets are
/// relative to its start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    slot_size: u32,
}

impl Region {
    /// A partition of `size` bytes: two slots of half of it each, rounded
    /// down to whole sectors.
    pub const fn new(size: u32) -> Self {
        let half = size / 2;
        Region { slot_size: half - half % SECTOR }
    }

    /// Size of one slot (header sector plus image area).
    pub const fn slot_size(&self) -> u32 {
        self.slot_size
    }

    /// Offset of `slot`'s header.
    pub const fn header_offset(&self, slot: Slot) -> u32 {
        match slot {
            Slot::A => 0,
            Slot::B => self.slot_size,
        }
    }

    /// Offset of `slot`'s first image byte.
    pub const fn data_offset(&self, slot: Slot) -> u32 {
        self.header_offset(slot) + SECTOR
    }

    /// Largest image one slot holds.
    pub const fn capacity(&self) -> u32 {
        self.slot_size.saturating_sub(SECTOR)
    }

    /// Sector-aligned `[from, to)` to erase before writing an image of `len`
    /// bytes into `slot`: its header sector plus the sectors the image
    /// covers. `None` if it does not fit.
    pub fn erase_span(&self, slot: Slot, len: u32) -> Option<(u32, u32)> {
        if len > self.capacity() {
            return None;
        }
        let from = self.header_offset(slot);
        let sectors = len.div_ceil(SECTOR);
        let to = from.checked_add(SECTOR)?.checked_add(sectors.checked_mul(SECTOR)?)?;
        Some((from, to))
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

    fn header(generation: u32) -> Header {
        Header { len: 1234, crc: 0xDEAD_BEEF, generation: Generation(generation) }
    }

    #[test]
    fn header_round_trips_and_rejects_erased_or_oversized() {
        let h = header(7);
        assert_eq!(Header::decode(&h.encode(), 4096), Some(h));
        assert_eq!(Header::decode(&[0xFF; HEADER_LEN], u32::MAX), None);
        assert_eq!(Header::decode(&h.encode(), 1233), None);
        let zero = Header { len: 0, crc: 0, generation: Generation(0) };
        assert_eq!(Header::decode(&zero.encode(), 10), None);
        assert_eq!(Header::decode(&h.encode()[..15], 4096), None);
        // Fields written but the magic word still erased: a torn header.
        let mut torn = h.encode();
        torn[..4].fill(0xFF);
        assert_eq!(Header::decode(&torn, 4096), None);
    }

    #[test]
    fn region_splits_into_two_slots() {
        let r = Region::new(256 * 1024);
        assert_eq!(r.slot_size(), 128 * 1024);
        assert_eq!(r.capacity(), 124 * 1024);
        assert_eq!((r.header_offset(Slot::A), r.data_offset(Slot::A)), (0, SECTOR));
        assert_eq!((r.header_offset(Slot::B), r.data_offset(Slot::B)), (0x2_0000, 0x2_1000));
        assert_eq!(r.erase_span(Slot::A, 1), Some((0, 2 * SECTOR)));
        assert_eq!(r.erase_span(Slot::A, SECTOR), Some((0, 2 * SECTOR)));
        assert_eq!(r.erase_span(Slot::A, SECTOR + 1), Some((0, 3 * SECTOR)));
        assert_eq!(r.erase_span(Slot::A, r.capacity()), Some((0, 0x2_0000)));
        assert_eq!(r.erase_span(Slot::B, 1), Some((0x2_0000, 0x2_2000)));
        assert_eq!(r.erase_span(Slot::B, r.capacity()), Some((0x2_0000, 0x4_0000)));
        assert_eq!(r.erase_span(Slot::B, r.capacity() + 1), None);
        assert_eq!(Region::new(SECTOR * 4 + 5).capacity(), SECTOR);
        assert_eq!(Region::new(SECTOR * 3).capacity(), 0);
        assert_eq!(Region::new(100).capacity(), 0);
        assert_eq!(Slot::A.other(), Slot::B);
        assert_eq!(Slot::B.other(), Slot::A);
    }

    #[test]
    fn generations_compare_across_wrap() {
        assert!(Generation(2).is_newer_than(Generation(1)));
        assert!(!Generation(1).is_newer_than(Generation(2)));
        assert!(!Generation(5).is_newer_than(Generation(5)));
        let last = Generation(u32::MAX);
        assert_eq!(last.next(), Generation(0));
        assert!(last.next().is_newer_than(last));
        assert!(!last.is_newer_than(last.next()));
        // Exactly half the range apart: newer neither way.
        assert!(!Generation(0x8000_0000).is_newer_than(Generation(0)));
        assert!(!Generation(0).is_newer_than(Generation(0x8000_0000)));
    }

    #[test]
    fn newest_valid_slot_wins_and_ties_go_to_a() {
        assert_eq!(newest(None, None), None);
        assert_eq!(newest(Some(header(3)), None), Some((Slot::A, header(3))));
        assert_eq!(newest(None, Some(header(3))), Some((Slot::B, header(3))));
        assert_eq!(newest(Some(header(3)), Some(header(4))), Some((Slot::B, header(4))));
        assert_eq!(newest(Some(header(5)), Some(header(4))), Some((Slot::A, header(5))));
        assert_eq!(newest(Some(header(0)), Some(header(u32::MAX))), Some((Slot::A, header(0))));
        assert_eq!(newest(Some(header(9)), Some(header(9))), Some((Slot::A, header(9))));
        assert_eq!(
            newest(Some(header(0)), Some(header(0x8000_0000))),
            Some((Slot::A, header(0)))
        );
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
