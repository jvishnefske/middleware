//! Where a source route's frame bytes come from: the generated RGB565
//! [`pattern`] or a fixed set of stored frames (the fake JPEG camera).
//!
//! * [`FrameSet`] — `n ≥ 1` stored frames, borrowed (on a microcontroller:
//!   `&'static` slices in flash). Frame `i` of the stream is
//!   `frames[i % n]`, so a set loops forever. Each frame is `1..=`
//!   [`MAX_FRAME_BYTES`] bytes, checked once by [`FrameSet::new`], and the
//!   frames may differ in length (JPEG).
//! * [`FrameSource`] — either the pattern or a set, with one interface: the
//!   length of frame `i` ([`FrameSource::frame_len`]), its [`Chunker`]
//!   ([`FrameSource::chunker`], whose `total` is that length) and any byte
//!   range of it written into a caller's buffer ([`FrameSource::fill`]). A
//!   host fills each chunk's data straight into its outgoing message: the
//!   pattern is generated in place, a stored frame is copied from where it
//!   lies, and there is never a frame buffer.
//! * [`FrameSource::test_pattern`] — what `SourceKind::TestPattern` serves
//!   for a [`SourceConfig`]: the pattern for RGB565; for JPEG the baked
//!   fixture set of that size from `fixtures` (feature `cam-fixtures`),
//!   or `Unsupported` without the feature.
//!
//! Nothing allocates; every loop is bounded by the number of frames or the
//! output length.

#[cfg(feature = "cam-fixtures")]
pub mod fixtures;

use core::fmt;

use super::chunk::ChunkError;
use super::source::Chunker;
use super::{MAX_FRAME_BYTES, pattern};
use crate::wasm_abi::AbiError;
use crate::wasm_abi::route::{PixelFormat, SourceConfig};

/// A non-empty set of stored frames, each `1..=MAX_FRAME_BYTES` bytes,
/// served in a loop.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct FrameSet<'a> {
    frames: &'a [&'a [u8]],
}

impl fmt::Debug for FrameSet<'_> {
    /// Frame count and largest frame, not the bytes.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameSet")
            .field("frames", &self.frames.len())
            .field("largest", &self.largest())
            .finish()
    }
}

impl<'a> FrameSet<'a> {
    /// The set of `frames`: [`ChunkError::Empty`] when there are none,
    /// [`ChunkError::Bounds`] when a frame is empty or longer than
    /// [`MAX_FRAME_BYTES`].
    pub const fn new(frames: &'a [&'a [u8]]) -> Result<Self, ChunkError> {
        if frames.is_empty() {
            return Err(ChunkError::Empty);
        }
        let mut rest = frames;
        while let [first, tail @ ..] = rest {
            if first.is_empty() || first.len() > MAX_FRAME_BYTES as usize {
                return Err(ChunkError::Bounds);
            }
            rest = tail;
        }
        Ok(FrameSet { frames })
    }

    /// Number of distinct frames (at least 1).
    pub const fn count(&self) -> usize {
        self.frames.len()
    }

    /// Frame `n` of the stream: stored frame `n mod count`.
    pub fn frame(&self, n: u32) -> &'a [u8] {
        let len = self.frames.len().max(1);
        let index = usize::try_from(n).map_or(0, |n| n % len);
        self.frames.get(index).copied().unwrap_or(&[])
    }

    /// Length of frame `n` in bytes, `1..=MAX_FRAME_BYTES`.
    pub fn frame_len(&self, n: u32) -> u32 {
        // Checked by `new`: at most MAX_FRAME_BYTES, so it fits u32.
        u32::try_from(self.frame(n).len()).unwrap_or(0)
    }

    /// Length of the longest frame in bytes.
    pub fn largest(&self) -> u32 {
        let longest = self.frames.iter().map(|f| f.len()).max().unwrap_or(0);
        u32::try_from(longest).unwrap_or(0)
    }
}

/// The bytes of a source route's frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameSource<'a> {
    /// The RGB565 test [`pattern`] at this size, generated on demand.
    Pattern {
        /// Width in pixels.
        width: u16,
        /// Height in pixels.
        height: u16,
    },
    /// Stored frames, looped.
    Frames(FrameSet<'a>),
}

impl FrameSource<'static> {
    /// What `SourceKind::TestPattern` serves for `cfg`: the pattern for
    /// RGB565; for JPEG the fixture set of `cfg.width`×`cfg.height`
    /// (`fixtures::jpeg`), `InvalidArgument` for a size without one, and
    /// `Unsupported` when built without the `cam-fixtures` feature.
    pub fn test_pattern(cfg: &SourceConfig) -> Result<Self, AbiError> {
        match cfg.format {
            PixelFormat::Rgb565Be => Ok(FrameSource::Pattern {
                width: cfg.width,
                height: cfg.height,
            }),
            PixelFormat::Jpeg => Self::jpeg_fixture(cfg.width, cfg.height),
        }
    }

    #[cfg(feature = "cam-fixtures")]
    fn jpeg_fixture(width: u16, height: u16) -> Result<Self, AbiError> {
        fixtures::jpeg(width, height)
            .map(FrameSource::Frames)
            .ok_or(AbiError::InvalidArgument)
    }

    #[cfg(not(feature = "cam-fixtures"))]
    fn jpeg_fixture(_width: u16, _height: u16) -> Result<Self, AbiError> {
        Err(AbiError::Unsupported)
    }
}

impl<'a> FrameSource<'a> {
    /// Length of frame `n` in bytes (0 only for a degenerate pattern size).
    pub fn frame_len(&self, n: u32) -> u32 {
        match self {
            FrameSource::Pattern { width, height } => pattern::frame_len(*width, *height),
            FrameSource::Frames(set) => set.frame_len(n),
        }
    }

    /// The chunker of frame `n` with sequence number `seq` and chunks of at
    /// most `max_data` bytes (see [`Chunker::new`]).
    pub fn chunker(&self, n: u32, seq: u16, max_data: usize) -> Result<Chunker, ChunkError> {
        Chunker::new(seq, self.frame_len(n), max_data)
    }

    /// Writes bytes `offset..` of frame `n` into `out`, stopping at the end
    /// of `out` or of the frame; returns the count written.
    pub fn fill(&self, n: u32, offset: usize, out: &mut [u8]) -> usize {
        match self {
            FrameSource::Pattern { width, height } => {
                pattern::fill(n, *width, *height, offset, out)
            }
            FrameSource::Frames(set) => {
                let src = set.frame(n).get(offset..).unwrap_or(&[]);
                let k = src.len().min(out.len());
                match (out.get_mut(..k), src.get(..k)) {
                    (Some(dst), Some(src)) => {
                        dst.copy_from_slice(src);
                        k
                    }
                    _ => 0,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::stream::{ESPNOW_CHUNK_DATA, HEADER_LEN, chunk};
    use std::vec::Vec;

    const A: &[u8] = &[1, 2, 3, 4, 5];
    const B: &[u8] = &[9; 400];
    const SET: &[&[u8]] = &[A, B];

    #[test]
    fn sets_are_checked_once() {
        assert_eq!(FrameSet::new(&[]), Err(ChunkError::Empty));
        assert_eq!(FrameSet::new(&[A, &[]]), Err(ChunkError::Bounds));
        let big = [0u8; MAX_FRAME_BYTES as usize + 1];
        assert_eq!(FrameSet::new(&[&big]), Err(ChunkError::Bounds));
        assert!(FrameSet::new(&[&big[1..]]).is_ok());
        let s = FrameSet::new(SET).expect("valid");
        assert_eq!((s.count(), s.largest()), (2, 400));
        assert_eq!(
            std::format!("{s:?}"),
            "FrameSet { frames: 2, largest: 400 }"
        );
    }

    #[test]
    fn frames_loop_and_vary_in_length() {
        let s = FrameSet::new(SET).expect("valid");
        assert_eq!(s.frame(0), A);
        assert_eq!(s.frame(1), B);
        assert_eq!(s.frame(2), A);
        assert_eq!(s.frame(u32::MAX), B);
        let src = FrameSource::Frames(s);
        assert_eq!(
            (src.frame_len(0), src.frame_len(1), src.frame_len(4)),
            (5, 400, 5)
        );
        let c = src.chunker(1, 7, ESPNOW_CHUNK_DATA).expect("valid");
        assert_eq!((c.seq(), c.total(), c.count()), (7, 400, 3));
        assert_eq!(
            src.chunker(0, 0, ESPNOW_CHUNK_DATA).map(|c| c.count()),
            Ok(1)
        );
    }

    #[test]
    fn fill_copies_any_range_and_stops_at_the_end() {
        let src = FrameSource::Frames(FrameSet::new(SET).expect("valid"));
        let mut out = [0xAAu8; 4];
        assert_eq!(src.fill(0, 3, &mut out), 2);
        assert_eq!(out, [4, 5, 0xAA, 0xAA]);
        assert_eq!(src.fill(0, 5, &mut out), 0);
        assert_eq!(src.fill(0, usize::MAX, &mut out), 0);
        assert_eq!(src.fill(1, 0, &mut out), 4);
        assert_eq!(out, [9; 4]);
    }

    #[test]
    fn pattern_source_matches_the_pattern() {
        let src = FrameSource::Pattern {
            width: 4,
            height: 3,
        };
        assert_eq!(src.frame_len(9), 24);
        let mut a = [0u8; 24];
        let mut b = [0u8; 24];
        assert_eq!(src.fill(9, 0, &mut a), 24);
        pattern::fill(9, 4, 3, 0, &mut b);
        assert_eq!(a, b);
    }

    /// Every chunk of every frame, filled through the source, reassembles
    /// the stored frame; each frame's chunks carry its own `total`.
    #[test]
    fn chunks_rebuild_each_stored_frame() {
        let src = FrameSource::Frames(FrameSet::new(SET).expect("valid"));
        for n in 0..4u32 {
            let c = src.chunker(n, n as u16, ESPNOW_CHUNK_DATA).expect("valid");
            let mut rebuilt = Vec::new();
            for plan in c.chunks() {
                let mut data = [0u8; ESPNOW_CHUNK_DATA];
                let len = plan.range.len();
                assert_eq!(src.fill(n, plan.range.start, &mut data[..len]), len);
                let mut msg = [0u8; HEADER_LEN + ESPNOW_CHUNK_DATA];
                let k = plan.header.write(&data[..len], &mut msg).expect("fits");
                let ch = chunk::decode(&msg[..k]).expect("chunk");
                assert_eq!(ch.header.total, src.frame_len(n));
                rebuilt.extend_from_slice(ch.data);
            }
            assert_eq!(rebuilt, SET[n as usize % 2]);
        }
    }

    #[test]
    fn test_pattern_picks_the_source_by_format() {
        let rgb = SourceConfig::new(120, 120, 100).expect("valid");
        assert_eq!(
            FrameSource::test_pattern(&rgb),
            Ok(FrameSource::Pattern {
                width: 120,
                height: 120
            })
        );
        let odd = SourceConfig::jpeg(100, 100, 100).expect("valid");
        #[cfg(not(feature = "cam-fixtures"))]
        assert_eq!(FrameSource::test_pattern(&odd), Err(AbiError::Unsupported));
        #[cfg(feature = "cam-fixtures")]
        {
            assert_eq!(
                FrameSource::test_pattern(&odd),
                Err(AbiError::InvalidArgument)
            );
            for size in [240, 120] {
                let cfg = SourceConfig::jpeg(size, size, 200).expect("valid");
                let want = fixtures::jpeg(size, size).map(FrameSource::Frames);
                assert_eq!(FrameSource::test_pattern(&cfg).ok(), want);
            }
            let wide = SourceConfig::jpeg(240, 120, 200).expect("valid");
            assert_eq!(
                FrameSource::test_pattern(&wide),
                Err(AbiError::InvalidArgument)
            );
        }
    }
}
