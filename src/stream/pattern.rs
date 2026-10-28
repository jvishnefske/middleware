//! A deterministic RGB565 test pattern: the fake camera.
//!
//! Frame `n` of a `width`×`height` pattern is, per pixel `(x, y)`:
//!
//! * a counter band, the top `height / 8` rows: 16 equal cells across the
//!   width showing bits 15..0 of `n` (white = 1, black = 0);
//! * otherwise, a white vertical bar [`BAR_WIDTH`] pixels wide whose left
//!   edge is at `x = (2n) mod width`, moving right two pixels per frame and
//!   wrapping;
//! * otherwise, a gradient: red rises with `x`, green with `y`, and blue is
//!   `n mod 32`, so the whole background shifts each frame.
//!
//! Pixels are RGB565, sent big-endian (high byte first) as SPI displays
//! expect. [`fill`] writes any byte range of a frame, so a host can produce
//! each chunk on demand without a frame buffer. Every function is pure,
//! integer only, and loops at most once per output byte.

/// Width of the moving bar, in pixels.
pub const BAR_WIDTH: u32 = 8;

/// White in RGB565.
pub const WHITE: u16 = 0xFFFF;

/// Black in RGB565.
pub const BLACK: u16 = 0x0000;

/// Bytes per RGB565 pixel.
pub const BYTES_PER_PIXEL: u32 = 2;

/// Packs 5-bit red, 6-bit green and 5-bit blue (excess bits ignored).
pub const fn rgb565(r: u32, g: u32, b: u32) -> u16 {
    (((r & 0x1F) << 11) | ((g & 0x3F) << 5) | (b & 0x1F)) as u16
}

/// Frame length in bytes of a `width`×`height` RGB565 frame.
pub const fn frame_len(width: u16, height: u16) -> u32 {
    width as u32 * height as u32 * BYTES_PER_PIXEL
}

/// `v * max / (span - 1)`, or 0 for a one-pixel span.
const fn ramp(v: u32, span: u32, max: u32) -> u32 {
    if span <= 1 { 0 } else { v * max / (span - 1) }
}

/// The colour of pixel `(x, y)` in frame `frame`. Pixels outside the frame
/// are black.
pub const fn pixel(frame: u32, width: u16, height: u16, x: u16, y: u16) -> u16 {
    let (w, h, x, y) = (width as u32, height as u32, x as u32, y as u32);
    if x >= w || y >= h {
        return BLACK;
    }
    if y < h / 8 {
        let bit = 15 - (x * 16 / w);
        return if (frame >> bit) & 1 == 1 {
            WHITE
        } else {
            BLACK
        };
    }
    let bar = frame.wrapping_mul(2) % w;
    if (x + w - bar) % w < BAR_WIDTH {
        return WHITE;
    }
    rgb565(ramp(x, w, 31), ramp(y, h, 63), frame % 32)
}

/// Writes bytes `offset..` of frame `frame` into `out`, stopping at the end
/// of `out` or of the frame; returns the count written (0 for an empty
/// frame or an offset at or past its end).
pub fn fill(frame: u32, width: u16, height: u16, offset: usize, out: &mut [u8]) -> usize {
    let w = usize::from(width);
    let len = usize::try_from(frame_len(width, height)).unwrap_or(0);
    let n = len.saturating_sub(offset).min(out.len());
    for (i, byte) in out.iter_mut().take(n).enumerate() {
        let at = offset + i;
        let p = at / 2;
        // p < width * height, so both fit u16.
        let (x, y) = ((p % w) as u16, (p / w) as u16);
        let [hi, lo] = pixel(frame, width, height, x, y).to_be_bytes();
        *byte = if at.is_multiple_of(2) { hi } else { lo };
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_rgb565() {
        assert_eq!(rgb565(31, 63, 31), WHITE);
        assert_eq!(rgb565(31, 0, 0), 0xF800);
        assert_eq!(rgb565(0, 63, 0), 0x07E0);
        assert_eq!(rgb565(0, 0, 31), 0x001F);
        assert_eq!(frame_len(120, 120), 28_800);
    }

    #[test]
    fn counter_band_shows_the_frame_number() {
        // 160 wide: cells of 10 pixels; frame 0x8001 sets the first and
        // last cells.
        let f = 0x8001;
        assert_eq!(pixel(f, 160, 120, 0, 0), WHITE);
        assert_eq!(pixel(f, 160, 120, 9, 14), WHITE);
        assert_eq!(pixel(f, 160, 120, 10, 0), BLACK);
        assert_eq!(pixel(f, 160, 120, 159, 0), WHITE);
        assert_eq!(pixel(f, 160, 120, 149, 0), BLACK);
    }

    #[test]
    fn bar_moves_and_wraps() {
        let (w, h, y) = (120, 120, 60);
        assert_eq!(pixel(0, w, h, 0, y), WHITE);
        assert_eq!(pixel(0, w, h, 7, y), WHITE);
        assert_ne!(pixel(0, w, h, 8, y), WHITE);
        assert_eq!(pixel(10, w, h, 20, y), WHITE);
        assert_ne!(pixel(10, w, h, 19, y), WHITE);
        // Frame 58: bar at 116, wrapping over 116..120 and 0..4.
        assert_eq!(pixel(58, w, h, 119, y), WHITE);
        assert_eq!(pixel(58, w, h, 3, y), WHITE);
        assert_ne!(pixel(58, w, h, 4, y), WHITE);
    }

    #[test]
    fn frames_differ_and_are_deterministic() {
        let mut a = [0u8; 28_800];
        let mut b = [0u8; 28_800];
        assert_eq!(fill(1, 120, 120, 0, &mut a), 28_800);
        assert_eq!(fill(2, 120, 120, 0, &mut b), 28_800);
        assert_ne!(a, b);
        let mut c = [0u8; 28_800];
        fill(1, 120, 120, 0, &mut c);
        assert_eq!(a, c);
    }

    #[test]
    fn fill_by_ranges_matches_the_whole_frame() {
        let mut whole = [0u8; 28_800];
        fill(7, 120, 120, 0, &mut whole);
        let mut pieced = [0u8; 28_800];
        let mut at = 0;
        // Odd-sized pieces split pixels across chunk boundaries.
        while at < pieced.len() {
            let end = (at + 167).min(pieced.len());
            let n = fill(7, 120, 120, at, &mut pieced[at..end]);
            assert_eq!(n, end - at);
            at = end;
        }
        assert_eq!(whole, pieced);
        // Big-endian: high byte first.
        let p = pixel(7, 120, 120, 0, 60).to_be_bytes();
        assert_eq!(&whole[120 * 60 * 2..120 * 60 * 2 + 2], &p);
    }

    #[test]
    fn fill_stops_at_the_frame_end_and_handles_degenerate_sizes() {
        let mut out = [0xAAu8; 8];
        assert_eq!(fill(0, 2, 2, 6, &mut out), 2);
        assert_eq!(out[2], 0xAA, "untouched past the frame");
        assert_eq!(fill(0, 2, 2, 8, &mut out), 0);
        assert_eq!(fill(0, 2, 2, usize::MAX, &mut out), 0);
        assert_eq!(fill(0, 0, 2, 0, &mut out), 0);
        assert_eq!(fill(0, 1, 1, 0, &mut out), 2);
        assert_eq!(pixel(0, 1, 1, 1, 0), BLACK, "outside");
        assert_eq!(pixel(u32::MAX, 1, 1, 0, 0), pixel(u32::MAX, 1, 1, 0, 0));
    }
}
