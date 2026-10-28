//! The chunk wire format: one message on a stream key.
//!
//! | bytes | field                                                        |
//! |-------|--------------------------------------------------------------|
//! | 1     | version [`VERSION`] (1)                                      |
//! | 1     | flags: [`FLAG_FIRST`] (bit 0), [`FLAG_LAST`] (bit 1), rest 0 |
//! | 2     | frame sequence number, `u16` little-endian (wrapping)        |
//! | 4     | byte offset of the data within the frame, `u32` LE           |
//! | 4     | frame length in bytes, `u32` LE (`1..=MAX_FRAME_BYTES`)      |
//! | rest  | data, at least 1 byte                                        |
//!
//! The header is [`HEADER_LEN`] (12) bytes. The flags are redundant with
//! the offsets (`FIRST` iff `offset == 0`, `LAST` iff `offset + len ==
//! total`) and are checked against them, so a receiver can rely on either.
//! [`decode`] accepts a chunk only if its data lies entirely inside the
//! frame, so a consumer that writes the data at `offset` never writes past
//! `total`.

use super::MAX_FRAME_BYTES;

/// Chunk format version.
pub const VERSION: u8 = 1;

/// Size of the fixed header.
pub const HEADER_LEN: usize = 12;

/// Flag: the chunk starts its frame (`offset == 0`).
pub const FLAG_FIRST: u8 = 0x01;

/// Flag: the chunk ends its frame (`offset + len == total`).
pub const FLAG_LAST: u8 = 0x02;

/// Why a chunk was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkError {
    /// Shorter than [`HEADER_LEN`].
    Truncated,
    /// Unknown format version.
    Version,
    /// Reserved flag bits set, or `FIRST` / `LAST` disagree with the offsets.
    Flags,
    /// No data bytes.
    Empty,
    /// Frame length 0 or above [`MAX_FRAME_BYTES`], or data reaching past
    /// the frame end.
    Bounds,
    /// The output buffer cannot hold header and data (encoding only).
    BufferTooSmall,
}

/// The header of one chunk; the data length is the rest of the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkHeader {
    /// Frame sequence number; wraps from `u16::MAX` to 0.
    pub seq: u16,
    /// Byte offset of this chunk's data within the frame.
    pub offset: u32,
    /// Frame length in bytes.
    pub total: u32,
}

impl ChunkHeader {
    /// Whether this chunk starts its frame.
    pub const fn is_first(&self) -> bool {
        self.offset == 0
    }

    /// Offset just past `len` data bytes, or `None` when the data would not
    /// lie inside the frame.
    pub const fn end(&self, len: usize) -> Option<u32> {
        if len > u32::MAX as usize {
            return None;
        }
        match self.offset.checked_add(len as u32) {
            Some(end) if end <= self.total => Some(end),
            _ => None,
        }
    }

    /// Checks the header against a data length of `len` bytes: data present,
    /// frame length `1..=MAX_FRAME_BYTES`, data inside the frame.
    pub const fn check(&self, len: usize) -> Result<(), ChunkError> {
        if len == 0 {
            return Err(ChunkError::Empty);
        }
        if self.total == 0 || self.total > MAX_FRAME_BYTES {
            return Err(ChunkError::Bounds);
        }
        match self.end(len) {
            Some(_) => Ok(()),
            None => Err(ChunkError::Bounds),
        }
    }

    /// The flags byte for a chunk of `len` data bytes.
    pub const fn flags(&self, len: usize) -> u8 {
        let first = if self.is_first() { FLAG_FIRST } else { 0 };
        let last = match self.end(len) {
            Some(end) if end == self.total => FLAG_LAST,
            _ => 0,
        };
        first | last
    }

    /// The encoded header for a chunk of `len` data bytes, after
    /// [`ChunkHeader::check`]. A host that fills the data in place writes
    /// these bytes in front of it.
    pub const fn encode(&self, len: usize) -> Result<[u8; HEADER_LEN], ChunkError> {
        if let Err(e) = self.check(len) {
            return Err(e);
        }
        let s = self.seq.to_le_bytes();
        let o = self.offset.to_le_bytes();
        let t = self.total.to_le_bytes();
        Ok([
            VERSION,
            self.flags(len),
            s[0],
            s[1],
            o[0],
            o[1],
            o[2],
            o[3],
            t[0],
            t[1],
            t[2],
            t[3],
        ])
    }

    /// Writes header and `data` to the front of `out`; returns the message
    /// length.
    pub fn write(&self, data: &[u8], out: &mut [u8]) -> Result<usize, ChunkError> {
        let header = self.encode(data.len())?;
        let n = HEADER_LEN + data.len();
        let slot = out.get_mut(..n).ok_or(ChunkError::BufferTooSmall)?;
        let (head, body) = slot.split_at_mut(HEADER_LEN);
        head.copy_from_slice(&header);
        body.copy_from_slice(data);
        Ok(n)
    }
}

/// A decoded chunk borrowing its data from the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunk<'a> {
    /// The header.
    pub header: ChunkHeader,
    /// The data, `1..` bytes, lying inside the frame.
    pub data: &'a [u8],
}

/// Reads a little-endian `u32` at `at` (the caller guarantees the bounds).
fn le32(b: &[u8; HEADER_LEN], at: usize) -> u32 {
    let mut v = [0u8; 4];
    if let Some(src) = b.get(at..at + 4) {
        v.copy_from_slice(src);
    }
    u32::from_le_bytes(v)
}

/// Parses one message as a chunk, checking version, flags and bounds.
pub fn decode(bytes: &[u8]) -> Result<Chunk<'_>, ChunkError> {
    let (head, data) = bytes
        .split_first_chunk::<HEADER_LEN>()
        .ok_or(ChunkError::Truncated)?;
    if head[0] != VERSION {
        return Err(ChunkError::Version);
    }
    let header = ChunkHeader {
        seq: u16::from_le_bytes([head[2], head[3]]),
        offset: le32(head, 4),
        total: le32(head, 8),
    };
    header.check(data.len())?;
    if head[1] != header.flags(data.len()) {
        return Err(ChunkError::Flags);
    }
    Ok(Chunk { header, data })
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: ChunkHeader = ChunkHeader {
        seq: 0x0102,
        offset: 0,
        total: 6,
    };

    #[test]
    fn layout_and_round_trip() {
        let mut out = [0u8; 32];
        let n = H.write(&[9, 8, 7], &mut out).expect("fits");
        assert_eq!(n, HEADER_LEN + 3);
        assert_eq!(
            &out[..n],
            &[1, FLAG_FIRST, 0x02, 0x01, 0, 0, 0, 0, 6, 0, 0, 0, 9, 8, 7]
        );
        let c = decode(&out[..n]).expect("valid");
        assert_eq!(c.header, H);
        assert_eq!(c.data, &[9, 8, 7]);

        let last = ChunkHeader { offset: 3, ..H };
        let n = last.write(&[1, 2, 3], &mut out).expect("fits");
        assert_eq!(out[1], FLAG_LAST);
        assert_eq!(decode(&out[..n]).map(|c| c.header), Ok(last));

        let whole = ChunkHeader { total: 3, ..H };
        assert_eq!(whole.flags(3), FLAG_FIRST | FLAG_LAST);
        let n = whole.write(&[1, 2, 3], &mut out).expect("fits");
        assert_eq!(decode(&out[..n]).map(|c| c.data.len()), Ok(3));
    }

    #[test]
    fn rejects_truncated_version_and_flags() {
        let mut out = [0u8; 32];
        let n = H.write(&[1], &mut out).expect("fits");
        assert_eq!(decode(&out[..HEADER_LEN - 1]), Err(ChunkError::Truncated));
        assert_eq!(decode(&[]), Err(ChunkError::Truncated));
        let mut bad = out;
        bad[0] = 2;
        assert_eq!(decode(&bad[..n]), Err(ChunkError::Version));
        let mut bad = out;
        bad[1] = FLAG_FIRST | FLAG_LAST; // not last: 1 of 6 bytes
        assert_eq!(decode(&bad[..n]), Err(ChunkError::Flags));
        let mut bad = out;
        bad[1] = 0; // offset 0 must say FIRST
        assert_eq!(decode(&bad[..n]), Err(ChunkError::Flags));
        let mut bad = out;
        bad[1] = FLAG_FIRST | 0x80;
        assert_eq!(decode(&bad[..n]), Err(ChunkError::Flags));
    }

    #[test]
    fn rejects_zero_length_and_out_of_frame_data() {
        let mut out = [0u8; 32];
        assert_eq!(H.write(&[], &mut out), Err(ChunkError::Empty));
        let n = H.write(&[1], &mut out).expect("fits");
        assert_eq!(decode(&out[..HEADER_LEN]), Err(ChunkError::Empty));
        let _ = n;
        // Data past the frame end.
        let late = ChunkHeader { offset: 5, ..H };
        assert_eq!(late.write(&[1, 2], &mut out), Err(ChunkError::Bounds));
        assert_eq!(late.check(1), Ok(()));
        // Frame length 0 or above the ceiling.
        assert_eq!(
            ChunkHeader { total: 0, ..H }.check(1),
            Err(ChunkError::Bounds)
        );
        assert_eq!(
            ChunkHeader {
                total: MAX_FRAME_BYTES + 1,
                ..H
            }
            .check(1),
            Err(ChunkError::Bounds)
        );
        assert_eq!(
            ChunkHeader {
                total: MAX_FRAME_BYTES,
                ..H
            }
            .check(1),
            Ok(())
        );
        // Offset overflow.
        let wrap = ChunkHeader {
            offset: u32::MAX,
            total: MAX_FRAME_BYTES,
            seq: 0,
        };
        assert_eq!(wrap.end(2), None);
        assert_eq!(wrap.check(2), Err(ChunkError::Bounds));
        // A hand-made header claiming data past the end is refused on decode.
        let mut raw = [1, 0, 0, 0, 5, 0, 0, 0, 6, 0, 0, 0, 1, 2];
        assert_eq!(decode(&raw), Err(ChunkError::Bounds));
        raw[4] = 4;
        raw[1] = FLAG_LAST;
        assert!(decode(&raw).is_ok());
    }

    #[test]
    fn write_needs_room() {
        let mut out = [0u8; HEADER_LEN + 1];
        assert_eq!(H.write(&[1, 2], &mut out), Err(ChunkError::BufferTooSmall));
        assert_eq!(H.write(&[1], &mut out), Ok(HEADER_LEN + 1));
    }

    #[test]
    fn seq_is_little_endian_and_full_range() {
        for seq in [0u16, 1, 0xff, 0x100, u16::MAX] {
            let h = ChunkHeader { seq, ..H };
            let mut out = [0u8; 16];
            let n = h.write(&[0], &mut out).expect("fits");
            assert_eq!(decode(&out[..n]).map(|c| c.header.seq), Ok(seq));
        }
    }
}
