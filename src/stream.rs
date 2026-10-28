//! Chunked frame streams over pub/sub keys: the pure core behind the ABI 4
//! DMA routes ([`crate::wasm_abi::route`]).
//!
//! A *stream* carries frames (e.g. camera images) that are far larger than
//! one message. The source cuts each frame into chunks, each published as
//! one message on the stream's key; the sink receives them in arrival order
//! and hands each one, unchanged and uncopied, to a DMA engine (an SPI
//! display). Nothing here allocates, blocks or does I/O.
//!
//! * [`chunk`] — the wire format: a fixed [`HEADER_LEN`]-byte
//!   [`ChunkHeader`] (version, first/last flags, frame sequence number, byte
//!   offset, frame length) followed by the data.
//! * [`source`] — the [`Chunker`] that splits a frame of `total` bytes into
//!   chunk headers. Chunk `i` is a pure function of `(seq, total,
//!   max_data, i)`, so a source can generate each chunk on demand without
//!   holding the frame.
//! * [`sink`] — the [`Sink`] state machine that decides, per received chunk,
//!   whether to send the frame preamble and forward it, forward it, or drop
//!   it. A dropped chunk never reaches the display, and a forwarded chunk is
//!   always the exact continuation of what was forwarded before, so loss,
//!   reordering and duplicates can blank part of a frame but never put
//!   bytes in the wrong place.
//! * [`pattern`] — a deterministic RGB565 test pattern (the fake camera),
//!   generated per byte range so a host can fill one chunk at a time.
//! * [`RouteStats`] — the per-route counters behind the `route_stat` import.
//!
//! # Budget per ESP-NOW message
//!
//! The ESP-NOW transport carries at most [`ESPNOW_MAX_PAYLOAD`] (179) bytes
//! per message whatever the key, so a chunk holds at most
//! `179 - HEADER_LEN = 167` data bytes. [`ESPNOW_CHUNK_DATA`] rounds that
//! down to 166, a whole number of RGB565 pixels, and is the default chunk
//! size of a source route. A 120×120 RGB565 frame (28 800 bytes) is then 174
//! chunks.

pub mod chunk;
pub mod pattern;
pub mod sink;
pub mod source;

pub use chunk::{Chunk, ChunkError, ChunkHeader, FLAG_FIRST, FLAG_LAST, HEADER_LEN, VERSION};
pub use sink::{Action, DropReason, Sink, Step};
pub use source::{ChunkPlan, Chunker, Chunks};

/// Largest frame a stream carries, in bytes: 320×240 RGB565 (QVGA).
pub const MAX_FRAME_BYTES: u32 = 320 * 240 * 2;

/// Largest data part of one chunk: a whole chunk must fit one payload
/// message ([`crate::wasm_abi::MAX_MESSAGE`]).
pub const MAX_CHUNK_DATA: usize = crate::wasm_abi::MAX_MESSAGE - HEADER_LEN;

/// Largest payload of one ESP-NOW message (`espnow::MAX_PAYLOAD`): a
/// 250-byte frame minus the 7-byte header and a maximal 64-byte key. Stated
/// here so the stream core does not need the `espnow` feature; a
/// compile-time assertion keeps the two equal when it is enabled.
pub const ESPNOW_MAX_PAYLOAD: usize = 250 - 7 - 64;

#[cfg(feature = "espnow")]
const _: () = assert!(ESPNOW_MAX_PAYLOAD == crate::espnow::MAX_PAYLOAD);
const _: () = assert!(HEADER_LEN + ESPNOW_CHUNK_DATA <= ESPNOW_MAX_PAYLOAD);

/// Default chunk data size: the largest even size that fits one ESP-NOW
/// message with the chunk header (166 bytes, 83 RGB565 pixels).
pub const ESPNOW_CHUNK_DATA: usize = (ESPNOW_MAX_PAYLOAD - HEADER_LEN) & !1;

/// Per-route counters, reported by the `route_stat` import. All counts
/// saturate at `u32::MAX`.
///
/// For a sink route, [`RouteStats::record`] folds in each [`Step`]. A
/// source route counts with [`RouteStats::sent`] and
/// [`RouteStats::skipped`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RouteStats {
    /// Frames completed: fully forwarded to the sink's bus, or fully
    /// published by a source.
    pub frames: u32,
    /// Frames lost: abandoned by a sink after a gap, or skipped by a source
    /// (capture overrun or failed publish).
    pub frames_dropped: u32,
    /// Chunks forwarded (sink) or published (source).
    pub chunks: u32,
    /// Chunks dropped (sink) or not published (source).
    pub chunks_dropped: u32,
}

impl RouteStats {
    /// The counters after a sink [`Step`].
    pub const fn record(self, step: &Step) -> Self {
        let forwarded = matches!(step.action, Action::StartFrame | Action::Forward);
        RouteStats {
            frames: self.frames.saturating_add(step.completed as u32),
            frames_dropped: self.frames_dropped.saturating_add(step.aborted as u32),
            chunks: self.chunks.saturating_add(forwarded as u32),
            chunks_dropped: self.chunks_dropped.saturating_add(!forwarded as u32),
        }
    }

    /// The counters after a source published a whole frame of `chunks`
    /// chunks.
    pub const fn sent(self, chunks: u32) -> Self {
        RouteStats {
            frames: self.frames.saturating_add(1),
            chunks: self.chunks.saturating_add(chunks),
            ..self
        }
    }

    /// The counters after a source gave up a frame, having published `sent`
    /// of its `chunks` chunks.
    pub const fn skipped(self, sent: u32, chunks: u32) -> Self {
        RouteStats {
            frames_dropped: self.frames_dropped.saturating_add(1),
            chunks: self.chunks.saturating_add(sent),
            chunks_dropped: self
                .chunks_dropped
                .saturating_add(chunks.saturating_sub(sent)),
            ..self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn espnow_budget() {
        assert_eq!(ESPNOW_MAX_PAYLOAD, 179);
        assert_eq!(ESPNOW_CHUNK_DATA, 166);
        assert_eq!(ESPNOW_CHUNK_DATA % 2, 0, "whole RGB565 pixels");
        assert_eq!(MAX_CHUNK_DATA, 500);
        let frame: usize = 120 * 120 * 2;
        assert_eq!(frame.div_ceil(ESPNOW_CHUNK_DATA), 174);
    }

    #[test]
    fn stats_count_steps_and_saturate() {
        let h = ChunkHeader {
            seq: 1,
            offset: 0,
            total: 4,
        };
        let s = Sink::new();
        let a = s.step(4, &h, 2);
        let stats = RouteStats::default().record(&a);
        assert_eq!(
            stats,
            RouteStats {
                chunks: 1,
                ..RouteStats::default()
            }
        );
        let b = a.sink.step(4, &ChunkHeader { offset: 2, ..h }, 2);
        let stats = stats.record(&b);
        assert_eq!((stats.frames, stats.chunks), (1, 2));
        let dup = b.sink.step(4, &h, 2);
        let stats = stats.record(&dup);
        assert_eq!(stats.chunks_dropped, 1);

        let full = RouteStats {
            frames: u32::MAX,
            frames_dropped: u32::MAX,
            chunks: u32::MAX,
            chunks_dropped: u32::MAX,
        };
        assert_eq!(full.record(&b), full);
        assert_eq!(full.sent(3), full);
        assert_eq!(full.skipped(1, 3), full);
    }

    #[test]
    fn source_stats() {
        let s = RouteStats::default().sent(174).skipped(10, 174);
        assert_eq!(
            s,
            RouteStats {
                frames: 1,
                frames_dropped: 1,
                chunks: 184,
                chunks_dropped: 164,
            }
        );
        assert_eq!(RouteStats::default().skipped(5, 3).chunks_dropped, 0);
    }
}
