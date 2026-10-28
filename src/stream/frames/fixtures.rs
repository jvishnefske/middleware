//! The fake JPEG camera's baked frames (feature `cam-fixtures`): eight
//! baseline JPEG frames (YCbCr 4:2:0, no restart markers, no APPn) of a
//! moving test scene at 240×240 ([`JPEG_240`], 4.1–4.8 KB each) and
//! 120×120 ([`JPEG_120`], 2.1–2.4 KB each), embedded with `include_bytes!`
//! from `fixtures/cam/jpeg<size>/frame_<nn>.jpg` (about 53 KB of flash in
//! all). `tools/jpeg-fixtures` generates them reproducibly; see its docs
//! for the scene.
//!
//! The sets are checked by [`FrameSet::new`] at compile time: a missing or
//! oversized file fails the build, never a running device.

use super::FrameSet;

/// Includes the eight frames `frame_00.jpg` … `frame_07.jpg` of one size.
macro_rules! frames {
    ($size:literal) => {
        [
            include_bytes!(concat!(
                "../../../fixtures/cam/jpeg",
                $size,
                "/frame_00.jpg"
            )),
            include_bytes!(concat!(
                "../../../fixtures/cam/jpeg",
                $size,
                "/frame_01.jpg"
            )),
            include_bytes!(concat!(
                "../../../fixtures/cam/jpeg",
                $size,
                "/frame_02.jpg"
            )),
            include_bytes!(concat!(
                "../../../fixtures/cam/jpeg",
                $size,
                "/frame_03.jpg"
            )),
            include_bytes!(concat!(
                "../../../fixtures/cam/jpeg",
                $size,
                "/frame_04.jpg"
            )),
            include_bytes!(concat!(
                "../../../fixtures/cam/jpeg",
                $size,
                "/frame_05.jpg"
            )),
            include_bytes!(concat!(
                "../../../fixtures/cam/jpeg",
                $size,
                "/frame_06.jpg"
            )),
            include_bytes!(concat!(
                "../../../fixtures/cam/jpeg",
                $size,
                "/frame_07.jpg"
            )),
        ]
    };
}

/// Frames per set.
pub const FRAMES: usize = 8;

const FRAMES_240: [&[u8]; FRAMES] = frames!("240");
const FRAMES_120: [&[u8]; FRAMES] = frames!("120");

/// A compile-time checked set (a bad fixture is a build error).
const fn checked(frames: &'static [&'static [u8]]) -> FrameSet<'static> {
    match FrameSet::new(frames) {
        Ok(set) => set,
        Err(_) => panic!("invalid camera fixture"),
    }
}

/// The 240×240 frames.
pub const JPEG_240: FrameSet<'static> = checked(&FRAMES_240);

/// The 120×120 frames.
pub const JPEG_120: FrameSet<'static> = checked(&FRAMES_120);

/// The fixture set of a `width`×`height` JPEG source, if there is one.
pub const fn jpeg(width: u16, height: u16) -> Option<FrameSet<'static>> {
    match (width, height) {
        (240, 240) => Some(JPEG_240),
        (120, 120) => Some(JPEG_120),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::ESPNOW_CHUNK_DATA;

    /// Each frame is one baseline JPEG: SOI first, EOI last, an SOF0 with
    /// the right size and 4:2:0 sampling, and no DRI (restart markers).
    #[test]
    fn frames_are_baseline_420_jpegs_of_their_size() {
        for (set, size) in [(JPEG_240, 240u16), (JPEG_120, 120)] {
            assert_eq!(set.count(), FRAMES);
            for n in 0..FRAMES as u32 {
                let f = set.frame(n);
                assert_eq!(f.get(..2), Some(&[0xFF, 0xD8][..]), "SOI");
                assert_eq!(f.get(f.len() - 2..), Some(&[0xFF, 0xD9][..]), "EOI");
                let mut at = 2;
                let mut sof = None;
                while let (Some(&0xFF), Some(&marker)) = (f.get(at), f.get(at + 1)) {
                    if marker == 0xDA {
                        break;
                    }
                    assert_ne!(marker, 0xDD, "no restart interval");
                    let len = usize::from(u16::from_be_bytes([f[at + 2], f[at + 3]]));
                    if marker == 0xC0 {
                        sof = f.get(at + 4..at + 2 + len);
                    }
                    at += 2 + len;
                }
                let [h0, h1] = size.to_be_bytes();
                assert_eq!(
                    sof,
                    Some(&[8, h0, h1, h0, h1, 3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1][..])
                );
            }
        }
    }

    #[test]
    fn sizes_and_chunk_counts() {
        assert!((4_000..5_000).contains(&JPEG_240.largest()));
        assert!((2_000..2_500).contains(&JPEG_120.largest()));
        for n in 0..FRAMES as u32 {
            let chunks = JPEG_240.frame_len(n).div_ceil(ESPNOW_CHUNK_DATA as u32);
            assert!((25..=29).contains(&chunks), "frame {n}: {chunks} chunks");
        }
        assert_eq!(jpeg(240, 240), Some(JPEG_240));
        assert_eq!(jpeg(120, 120), Some(JPEG_120));
        assert_eq!(jpeg(240, 120), None);
        assert_ne!(JPEG_240.frame(0), JPEG_240.frame(1));
    }
}
