//! Camera capture decisions (R15): the ESP32 I2S camera-mode sample
//! layout and the JPEG frame assembler, pure and host tested.
//!
//! The shell (`src/shell/camera.rs`, ESP32 only) runs I2S0 in camera
//! (slave, parallel) mode: on each PCLK rising edge while HREF is high it
//! samples the 8 data lines and its DMA writes the sample into a ring of
//! descriptors. The ESP32 does not pack 8-bit samples: depending on the
//! receive FIFO mode each byte occupies a 32-bit word ([`SampleMode::Word`],
//! FIFO mode 3, esp32-camera's `SM_0A00_0B00`) or half of one
//! ([`SampleMode::HalfWord`], FIFO mode 1, `SM_0A0B_0C0D`); the sample is
//! the low byte of a 16-bit half, and of the two halves of a little-endian
//! word the upper one comes first. [`extract`] turns whole DMA words into
//! the camera's byte stream, so this copy is the one the hardware forces
//! (there is no way to point a chunk at the DMA buffer).
//!
//! The OV2640 in JPEG mode clocks out one baseline JPEG per frame while
//! HREF is high. [`Assembler`] (state in, state out) finds a frame in that
//! stream by its markers, across any split into DMA buffers: it skips
//! everything up to start-of-image (`FF D8`), then copies into the frame
//! buffer up to and including end-of-image (`FF D9`). JPEG stuffs every
//! `FF` in entropy-coded data as `FF 00` and allows `FF` fill bytes before
//! a marker, so `FF D9` cannot occur inside a frame. A frame that does not
//! fit the buffer is reported as [`Status::Overflow`] (the shell drops it
//! and counts `RouteDropped`).
//!
//! A whole-frame buffer is unavoidable: every chunk header carries the
//! frame's total length (`middleware::stream::ChunkHeader`), known only at
//! end-of-image, so no chunk can leave before the frame is complete.

/// How the I2S receive FIFO lays 8-bit camera samples into DMA memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleMode {
    /// One sample per 32-bit word, in byte 2 (FIFO mode 3,
    /// `00 s1 00 00`).
    Word,
    /// Two samples per 32-bit word, byte 2 then byte 0 (FIFO mode 1,
    /// `00 s1 00 s2`).
    HalfWord,
}

impl SampleMode {
    /// DMA bytes per camera byte.
    pub const fn expansion(self) -> usize {
        match self {
            SampleMode::Word => 4,
            SampleMode::HalfWord => 2,
        }
    }
}

/// Copies the camera bytes of whole 32-bit words of `raw` into `out`, as
/// many words as both hold; returns `(raw bytes consumed, bytes written)`.
/// A trailing partial word is left for the next call.
pub fn extract(mode: SampleMode, raw: &[u8], out: &mut [u8]) -> (usize, usize) {
    let per_word = 4 / mode.expansion();
    let words = (raw.len() / 4).min(out.len() / per_word);
    let mut written = 0;
    for (word, dst) in raw.chunks_exact(4).take(words).zip(out.chunks_exact_mut(per_word)) {
        match (mode, word, dst) {
            (SampleMode::Word, [_, _, s, _], [d]) => *d = *s,
            (SampleMode::HalfWord, [s2, _, s1, _], [d1, d2]) => {
                *d1 = *s1;
                *d2 = *s2;
            }
            _ => break,
        }
        written += per_word;
    }
    (written / per_word * 4, written)
}

/// Where the assembler is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Looking for start-of-image.
    Seeking,
    /// Copying a frame (`len` bytes so far).
    Frame,
    /// A complete frame of `len` bytes is in the buffer.
    Done,
}

/// The result of one [`Assembler::feed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// The input is used up; the frame (if one started) continues.
    More,
    /// A complete frame of this many bytes (`FF D8` … `FF D9`) is in the
    /// buffer; input after its end was not consumed.
    Done(u32),
    /// The frame in progress did not fit the buffer; the assembler is
    /// seeking the next start-of-image.
    Overflow,
}

/// Finds one JPEG frame in a byte stream (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Assembler {
    phase: Phase,
    len: u32,
    /// The previous input byte was `FF`.
    after_ff: bool,
    /// Bytes skipped while seeking (saturating).
    skipped: u32,
}

impl Default for Assembler {
    fn default() -> Self {
        Self::new()
    }
}

impl Assembler {
    /// Seeking start-of-image, nothing skipped.
    pub const fn new() -> Self {
        Assembler { phase: Phase::Seeking, len: 0, after_ff: false, skipped: 0 }
    }

    /// Bytes skipped before start-of-image so far.
    pub const fn skipped(&self) -> u32 {
        self.skipped
    }

    /// Bytes of the frame in progress (or the complete one).
    pub const fn len(&self) -> u32 {
        self.len
    }

    /// Whether no frame byte has been kept yet.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Feeds `input`, keeping frame bytes in `frame`; returns the next
    /// state, the input bytes consumed and the [`Status`]. After
    /// [`Status::Done`] further calls consume nothing and keep reporting
    /// the frame.
    pub fn feed(self, input: &[u8], frame: &mut [u8]) -> (Self, usize, Status) {
        let mut st = self;
        if st.phase == Phase::Done {
            return (st, 0, Status::Done(st.len));
        }
        for (i, &b) in input.iter().enumerate() {
            match st.phase {
                Phase::Seeking => {
                    if st.after_ff && b == 0xD8 {
                        match frame.get_mut(..2) {
                            Some(head) => head.copy_from_slice(&[0xFF, 0xD8]),
                            None => return (Assembler::new(), i + 1, Status::Overflow),
                        }
                        // The FF was counted as skipped; it is the frame's.
                        st = Assembler { phase: Phase::Frame, len: 2, after_ff: false, skipped: st.skipped.saturating_sub(1) };
                    } else {
                        st.after_ff = b == 0xFF;
                        st.skipped = st.skipped.saturating_add(1);
                    }
                }
                Phase::Frame => {
                    let Some(slot) = usize::try_from(st.len).ok().and_then(|at| frame.get_mut(at)) else {
                        let next = Assembler { skipped: st.skipped, ..Assembler::new() };
                        return (next, i + 1, Status::Overflow);
                    };
                    *slot = b;
                    st.len += 1;
                    if st.after_ff && b == 0xD9 {
                        st.phase = Phase::Done;
                        st.after_ff = false;
                        return (st, i + 1, Status::Done(st.len));
                    }
                    st.after_ff = b == 0xFF;
                }
                Phase::Done => return (st, i, Status::Done(st.len)),
            }
        }
        (st, input.len(), Status::More)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::vec::Vec;

    /// A small JPEG-shaped frame with stuffed `FF 00`, fill `FF FF` and a
    /// restart marker inside.
    const FRAME: &[u8] = &[
        0xFF, 0xD8, 0xFF, 0xDB, 0x00, 0x04, 0x11, 0xFF, 0x00, 0x22, 0xFF, 0xD0, 0x33, 0xFF, 0xFF,
        0xD9,
    ];

    /// The camera stream: noise, then the frame, then the next frame's
    /// start.
    fn stream() -> Vec<u8> {
        let mut s = std::vec![0x00, 0xFF, 0x12, 0xFF, 0xFF, 0x00];
        s.extend_from_slice(FRAME);
        s.extend_from_slice(&[0xFF, 0xD8, 0x01]);
        s
    }

    /// Encodes camera bytes as the DMA writes them.
    fn dma(mode: SampleMode, bytes: &[u8]) -> Vec<u8> {
        match mode {
            SampleMode::Word => bytes.iter().flat_map(|&b| [0xEE, 0xEE, b, 0xEE]).collect(),
            SampleMode::HalfWord => bytes
                .chunks(2)
                .flat_map(|p| [p.get(1).copied().unwrap_or(0), 0xEE, p[0], 0xEE])
                .collect(),
        }
    }

    #[test]
    fn r15_extract_takes_the_sample_bytes_of_whole_words() {
        for mode in [SampleMode::Word, SampleMode::HalfWord] {
            let bytes: Vec<u8> = (0..40u8).collect();
            let raw = dma(mode, &bytes);
            assert_eq!(raw.len(), bytes.len() * mode.expansion());
            let mut out = [0u8; 64];
            assert_eq!(extract(mode, &raw, &mut out), (raw.len(), bytes.len()), "{mode:?}");
            assert_eq!(&out[..bytes.len()], &bytes[..]);
            // A partial trailing word waits; a short output stops early.
            assert_eq!(extract(mode, &raw[..7], &mut out), (4, 4 / mode.expansion()));
            let mut small = [0u8; 3];
            let (used, n) = extract(mode, &raw, &mut small);
            assert_eq!(n, 3 / (4 / mode.expansion()) * (4 / mode.expansion()));
            assert_eq!(used, n * mode.expansion());
            assert_eq!(&small[..n], &bytes[..n]);
            assert_eq!(extract(mode, &[], &mut out), (0, 0));
        }
    }

    #[test]
    fn r15_assembler_finds_the_frame_in_one_feed() {
        let s = stream();
        let mut buf = [0u8; 64];
        let (st, used, status) = Assembler::new().feed(&s, &mut buf);
        assert_eq!(status, Status::Done(FRAME.len() as u32));
        assert_eq!(&buf[..FRAME.len()], FRAME);
        assert_eq!(used, 6 + FRAME.len(), "stops after EOI");
        assert_eq!(st.skipped(), 6);
        // Done is sticky and consumes nothing more.
        assert_eq!(st.feed(&s[used..], &mut buf), (st, 0, Status::Done(FRAME.len() as u32)));
    }

    #[test]
    fn r15_assembler_is_split_proof() {
        // Every way of cutting the stream into two or three pieces gives
        // the same frame (markers split across DMA buffers included).
        let s = stream();
        for a in 0..=s.len() {
            for b in a..=s.len() {
                let mut buf = [0u8; 64];
                let mut st = Assembler::new();
                let mut done = None;
                for piece in [&s[..a], &s[a..b], &s[b..]] {
                    let (next, used, status) = st.feed(piece, &mut buf);
                    st = next;
                    if let Status::Done(n) = status {
                        done = Some(n);
                        assert!(used <= piece.len());
                        break;
                    }
                    assert_eq!(status, Status::More);
                    assert_eq!(used, piece.len());
                }
                assert_eq!(done, Some(FRAME.len() as u32), "split {a}/{b}");
                assert_eq!(&buf[..FRAME.len()], FRAME, "split {a}/{b}");
            }
        }
    }

    #[test]
    fn r15_assembler_reports_overflow_and_seeks_again() {
        let s = stream();
        let mut small = [0u8; 8];
        let (st, used, status) = Assembler::new().feed(&s, &mut small);
        assert_eq!(status, Status::Overflow);
        assert_eq!(used, 6 + 8 + 1);
        assert!(st.is_empty());
        // The rest of the stream holds the next start; with room it would
        // assemble, here it keeps seeking or overflows again, never Done
        // with a truncated frame.
        let mut st = st;
        let mut rest = &s[used..];
        while !rest.is_empty() {
            let (next, n, status) = st.feed(rest, &mut small);
            assert_ne!(status, Status::Done(8));
            st = next;
            rest = &rest[n.max(1).min(rest.len())..];
        }
        // A buffer too small for even the SOI.
        let mut tiny = [0u8; 1];
        assert_eq!(Assembler::new().feed(&s, &mut tiny).2, Status::Overflow);
    }

    #[test]
    fn r15_assembler_through_the_dma_layout() {
        for mode in [SampleMode::Word, SampleMode::HalfWord] {
            let raw = dma(mode, &stream());
            let mut st = Assembler::new();
            let mut frame = [0u8; 64];
            let mut bytes = [0u8; 5];
            let mut done = None;
            // DMA buffers of 12 bytes, extracted 5 at a time.
            'outer: for buf in raw.chunks(12) {
                let mut at = 0;
                while at < buf.len() {
                    let (used, n) = extract(mode, &buf[at..], &mut bytes);
                    if used == 0 {
                        break;
                    }
                    at += used;
                    let (next, _, status) = st.feed(&bytes[..n], &mut frame);
                    st = next;
                    if let Status::Done(len) = status {
                        done = Some(len);
                        break 'outer;
                    }
                }
            }
            assert_eq!(done, Some(FRAME.len() as u32), "{mode:?}");
            assert_eq!(&frame[..FRAME.len()], FRAME);
        }
    }
}
