//! The source side: splitting a frame into chunks.
//!
//! A [`Chunker`] describes one frame (sequence number, length) and the
//! largest chunk data size. Chunk `i` covers bytes `i * max_data ..
//! min((i + 1) * max_data, total)`; every chunk but the last is exactly
//! `max_data` bytes. [`Chunker::chunk`] computes any chunk directly, and
//! [`Chunker::chunks`] iterates them in order (at most
//! [`Chunker::count`] steps, so the loop is bounded by the frame length).
//!
//! Sequence numbers are the source's to choose; the convention is one per
//! frame, starting anywhere and advancing with [`next_seq`], so that a sink
//! can tell a new frame from a repeated chunk.

use core::ops::Range;

use super::chunk::{ChunkError, ChunkHeader};
use super::{MAX_CHUNK_DATA, MAX_FRAME_BYTES};

/// The sequence number of the frame after `seq` (wrapping).
pub const fn next_seq(seq: u16) -> u16 {
    seq.wrapping_add(1)
}

/// One planned chunk: its header and the frame bytes it carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkPlan {
    /// The header to send.
    pub header: ChunkHeader,
    /// Byte range of the frame carried as data (`1..=max_data` bytes).
    pub range: Range<usize>,
}

/// Splits one frame into chunks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunker {
    seq: u16,
    total: u32,
    max_data: u32,
}

impl Chunker {
    /// A frame of `total` bytes (`1..=MAX_FRAME_BYTES`) with sequence number
    /// `seq`, cut into chunks of at most `max_data` data bytes
    /// (`1..=MAX_CHUNK_DATA`). Anything else is [`ChunkError::Bounds`].
    pub const fn new(seq: u16, total: u32, max_data: usize) -> Result<Self, ChunkError> {
        if total == 0 || total > MAX_FRAME_BYTES || max_data == 0 || max_data > MAX_CHUNK_DATA {
            return Err(ChunkError::Bounds);
        }
        Ok(Chunker {
            seq,
            total,
            max_data: max_data as u32,
        })
    }

    /// The frame's sequence number.
    pub const fn seq(&self) -> u16 {
        self.seq
    }

    /// The frame length in bytes.
    pub const fn total(&self) -> u32 {
        self.total
    }

    /// Number of chunks in the frame (at least 1).
    pub const fn count(&self) -> u32 {
        self.total.div_ceil(self.max_data)
    }

    /// Chunk `index`, or `None` past the last one.
    pub fn chunk(&self, index: u32) -> Option<ChunkPlan> {
        let offset = index.checked_mul(self.max_data)?;
        if offset >= self.total {
            return None;
        }
        let end = offset.saturating_add(self.max_data).min(self.total);
        Some(ChunkPlan {
            header: ChunkHeader {
                seq: self.seq,
                offset,
                total: self.total,
            },
            range: usize::try_from(offset).ok()?..usize::try_from(end).ok()?,
        })
    }

    /// The chunks in order.
    pub const fn chunks(&self) -> Chunks {
        Chunks {
            chunker: *self,
            next: 0,
        }
    }
}

/// Iterator over a frame's chunks, from [`Chunker::chunks`].
#[derive(Debug, Clone)]
pub struct Chunks {
    chunker: Chunker,
    next: u32,
}

impl Iterator for Chunks {
    type Item = ChunkPlan;

    fn next(&mut self) -> Option<ChunkPlan> {
        let plan = self.chunker.chunk(self.next)?;
        self.next = self.next.saturating_add(1);
        Some(plan)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::stream::{ESPNOW_CHUNK_DATA, HEADER_LEN, Sink, chunk};
    use std::vec::Vec;

    #[test]
    fn splits_into_full_chunks_and_a_short_tail() {
        let c = Chunker::new(7, 10, 4).expect("valid");
        assert_eq!(c.count(), 3);
        let plans: Vec<ChunkPlan> = c.chunks().collect();
        assert_eq!(plans.len(), 3);
        assert_eq!(plans[0].range, 0..4);
        assert_eq!(plans[1].range, 4..8);
        assert_eq!(plans[2].range, 8..10);
        assert!(
            plans
                .iter()
                .all(|p| p.header.seq == 7 && p.header.total == 10)
        );
        assert_eq!(plans[1].header.offset, 4);
        assert_eq!(c.chunk(3), None);
        assert_eq!(c.chunk(u32::MAX), None, "no overflow");
    }

    #[test]
    fn exact_multiple_and_single_chunk() {
        let c = Chunker::new(0, 8, 4).expect("valid");
        assert_eq!(c.count(), 2);
        assert_eq!(c.chunk(1).map(|p| p.range), Some(4..8));
        let one = Chunker::new(0, 3, 166).expect("valid");
        assert_eq!(one.count(), 1);
        let p = one.chunk(0).expect("one chunk");
        assert_eq!(
            p.header.flags(p.range.len()),
            chunk::FLAG_FIRST | chunk::FLAG_LAST
        );
    }

    #[test]
    fn rejects_bad_sizes() {
        assert_eq!(Chunker::new(0, 0, 4), Err(ChunkError::Bounds));
        assert_eq!(
            Chunker::new(0, MAX_FRAME_BYTES + 1, 4),
            Err(ChunkError::Bounds)
        );
        assert_eq!(Chunker::new(0, 4, 0), Err(ChunkError::Bounds));
        assert_eq!(
            Chunker::new(0, 4, MAX_CHUNK_DATA + 1),
            Err(ChunkError::Bounds)
        );
        assert!(Chunker::new(0, MAX_FRAME_BYTES, 1).is_ok());
    }

    #[test]
    fn seq_wraps() {
        assert_eq!(next_seq(u16::MAX), 0);
        assert_eq!(next_seq(4), 5);
    }

    /// Every planned chunk encodes, decodes and is accepted in order by a
    /// sink, which completes the frame exactly once.
    #[test]
    fn chunker_output_feeds_a_sink_end_to_end() {
        let total = 120 * 120 * 2;
        let frame: [u8; 120 * 120 * 2] = core::array::from_fn(|i| i as u8);
        let mut sink = Sink::new();
        let mut done = 0;
        let mut written = 0usize;
        let c = Chunker::new(u16::MAX, total, ESPNOW_CHUNK_DATA).expect("valid");
        for plan in c.chunks() {
            let mut msg = [0u8; HEADER_LEN + ESPNOW_CHUNK_DATA];
            let data = &frame[plan.range.clone()];
            let n = plan.header.write(data, &mut msg).expect("fits");
            assert!(n <= crate::stream::ESPNOW_MAX_PAYLOAD);
            let (step, out) = sink.receive(total, &msg[..n]);
            assert_eq!(out, data);
            assert_eq!(usize::try_from(plan.header.offset), Ok(written));
            written += out.len();
            done += u32::from(step.completed);
            sink = step.sink;
        }
        assert_eq!((written, done), (frame.len(), 1));
    }
}
