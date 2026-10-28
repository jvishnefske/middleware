//! The moving test scene the fake camera's JPEG frames show.
//!
//! Frame `n` of `frames` at `width`×`height` (RGB888), drawn back to front:
//!
//! * a gradient: red rises with `x`, green with `y`, blue steps with `n`
//!   (`64 + 128 n / frames`), so the whole background shifts each frame;
//! * a white vertical bar `width / 10` pixels wide whose left edge is at
//!   `n * width / frames`, so it crosses the image once per loop and the
//!   loop (frame `frames - 1` → 0) is seamless;
//! * the frame number `n mod 10` as a black seven-segment digit, `height /
//!   2` tall, centred;
//! * a counter band, the top `height / 8` rows: 8 equal cells showing bits
//!   7..0 of `n` (white = 1, black = 0), like `middleware::stream::pattern`
//!   minus the high byte.
//!
//! Integer only and pure: the same arguments always give the same pixels.

/// Segments a–g (bit 0 = a … bit 6 = g) lit for each decimal digit.
const DIGITS: [u8; 10] = [0x3F, 0x06, 0x5B, 0x4F, 0x66, 0x6D, 0x7D, 0x07, 0x7F, 0x6F];

/// Whether `(x, y)`, relative to the digit's top-left corner, lies on a lit
/// segment of `digit` drawn `w` wide, `h` tall with strokes `t` thick.
fn on_digit(digit: u32, x: u32, y: u32, w: u32, h: u32, t: u32) -> bool {
    if x >= w || y >= h {
        return false;
    }
    let mid = h / 2;
    let lit = |seg: u8| DIGITS[(digit % 10) as usize] & (1 << seg) != 0;
    let top = y < t;
    let middle = y + t / 2 >= mid && y < mid + t - t / 2;
    let bottom = y + t >= h;
    let left = x < t;
    let right = x + t >= w;
    let upper = y < mid;
    (top && lit(0))
        || (right && upper && lit(1))
        || (right && !upper && lit(2))
        || (bottom && lit(3))
        || (left && !upper && lit(4))
        || (left && upper && lit(5))
        || (middle && lit(6))
}

/// The colour of pixel `(x, y)` of frame `n` of a `frames`-frame loop.
pub fn pixel(n: u32, frames: u32, width: u32, height: u32, x: u32, y: u32) -> [u8; 3] {
    const WHITE: [u8; 3] = [255, 255, 255];
    const BLACK: [u8; 3] = [0, 0, 0];
    let frames = frames.max(1);
    if y < height / 8 {
        let bit = 7 - (x * 8 / width);
        return if (n >> bit) & 1 == 1 { WHITE } else { BLACK };
    }
    let (dh, dw) = (height / 2, height / 4);
    let (dx, dy) = ((width - dw) / 2, (height - dh) / 2 + height / 16);
    if x >= dx && y >= dy && on_digit(n, x - dx, y - dy, dw, dh, (height / 16).max(1)) {
        return BLACK;
    }
    let bar = n % frames * width / frames;
    if (x + width - bar) % width < (width / 10).max(1) {
        return WHITE;
    }
    let ramp = |v: u32, span: u32| {
        if span <= 1 {
            0
        } else {
            (v * 255 / (span - 1)) as u8
        }
    };
    [
        ramp(x, width),
        ramp(y, height),
        (64 + 128 * (n % frames) / frames) as u8,
    ]
}

/// Frame `n` of a `frames`-frame loop as `width * height` RGB888 pixels,
/// row-major.
pub fn render(n: u32, frames: u32, width: u16, height: u16) -> Vec<u8> {
    let (w, h) = (u32::from(width), u32::from(height));
    (0..h)
        .flat_map(|y| (0..w).flat_map(move |x| pixel(n, frames, w, h, x, y)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn band_shows_the_frame_number() {
        // 240 wide: 30-pixel cells; frame 5 = 0b101 lights the last and
        // third-from-last cells.
        assert_eq!(pixel(5, 8, 240, 240, 239, 0), [255; 3]);
        assert_eq!(pixel(5, 8, 240, 240, 209, 0), [0; 3]);
        assert_eq!(pixel(5, 8, 240, 240, 179, 29), [255; 3]);
        assert_eq!(pixel(5, 8, 240, 240, 0, 0), [0; 3]);
    }

    #[test]
    fn bar_moves_an_eighth_per_frame_and_loops() {
        // Row 230 is below the digit.
        assert_eq!(pixel(0, 8, 240, 240, 0, 230), [255; 3]);
        assert_eq!(pixel(0, 8, 240, 240, 23, 230), [255; 3]);
        assert_ne!(pixel(0, 8, 240, 240, 24, 230), [255; 3]);
        assert_eq!(pixel(1, 8, 240, 240, 30, 230), [255; 3]);
        assert_ne!(pixel(1, 8, 240, 240, 29, 230), [255; 3]);
        assert_eq!(pixel(8, 8, 240, 240, 0, 230), pixel(0, 8, 240, 240, 0, 230));
    }

    #[test]
    fn digits_are_drawn_and_frames_differ() {
        // The centre of an "8" is the lit middle bar; of a "0" it is not.
        let (cx, cy) = (120, 120 + 15);
        assert_eq!(pixel(8, 10, 240, 240, cx, cy), [0; 3]);
        assert_ne!(pixel(0, 10, 240, 240, cx, cy), [0; 3]);
        assert_ne!(render(0, 8, 120, 120), render(1, 8, 120, 120));
        assert_eq!(render(3, 8, 120, 120), render(3, 8, 120, 120));
        assert_eq!(render(0, 8, 120, 120).len(), 120 * 120 * 3);
    }
}
