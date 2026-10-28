//! Colour conversion and output pixel formats.
//!
//! [`ycc_to_rgb`] is the JFIF YCbCr → RGB transform in 16-bit fixed point
//! (libjpeg's `jdcolor.c` constants), rounded and clamped to 0..=255: four
//! multiplies per pixel and no tables.
//!
//! A [`PixelFormat`] packs one RGB triple into the output strip. The decoder
//! is generic over it, so a payload only links the format it uses:
//! [`Rgb565Be`] for the GC9A01 (and most SPI panels) and [`Rgb888`] for host
//! tools and tests.

/// Fixed-point fraction bits of the conversion constants.
const SCALE_BITS: u32 = 16;
/// `0.5` in that fixed point.
const ONE_HALF: i32 = 1 << (SCALE_BITS - 1);
/// `round(1.40200 · 2^16)`: Cr → R.
const CR_R: i32 = 91_881;
/// `round(1.77200 · 2^16)`: Cb → B.
const CB_B: i32 = 116_130;
/// `round(0.71414 · 2^16)`: Cr → G.
const CR_G: i32 = 46_802;
/// `round(0.34414 · 2^16)`: Cb → G.
const CB_G: i32 = 22_554;

/// Converts one YCbCr sample (JFIF, full range) to RGB.
#[inline(always)]
pub fn ycc_to_rgb(y: u8, cb: u8, cr: u8) -> [u8; 3] {
    apply(y, chroma(cb, cr))
}

/// The R, G and B offsets a chroma pair adds to luma, so that pixels
/// sharing one chroma sample (4:2:2, 4:2:0) compute them once.
#[inline(always)]
pub(crate) fn chroma(cb: u8, cr: u8) -> [i32; 3] {
    let cb = i32::from(cb) - 128;
    let cr = i32::from(cr) - 128;
    [
        (CR_R * cr + ONE_HALF) >> SCALE_BITS,
        (-CB_G * cb - CR_G * cr + ONE_HALF) >> SCALE_BITS,
        (CB_B * cb + ONE_HALF) >> SCALE_BITS,
    ]
}

/// Adds chroma offsets to a luma sample, clamped to 0..=255.
#[inline(always)]
pub(crate) fn apply(y: u8, d: [i32; 3]) -> [u8; 3] {
    let y = i32::from(y);
    let [r, g, b] = d;
    [clamp(y + r), clamp(y + g), clamp(y + b)]
}

/// Clamps to 0..=255.
#[inline(always)]
fn clamp(v: i32) -> u8 {
    if v < 0 {
        0
    } else if v > 255 {
        255
    } else {
        v as u8
    }
}

/// An output pixel encoding.
pub trait PixelFormat {
    /// Bytes per pixel.
    const BYTES: usize;
    /// Writes `rgb` into `out`, which is exactly [`BYTES`](Self::BYTES)
    /// long (a shorter slice is left partly unwritten, never a panic).
    fn put(out: &mut [u8], rgb: [u8; 3]);
}

/// RGB565, big-endian (high byte first), as SPI panels such as the GC9A01
/// expect with `COLMOD = 0x55`. Each channel is truncated (`r >> 3`,
/// `g >> 2`, `b >> 3`). The panel's BGR or inversion settings are the
/// caller's concern; this is plain RGB order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb565Be;

impl PixelFormat for Rgb565Be {
    const BYTES: usize = 2;
    #[inline(always)]
    fn put(out: &mut [u8], rgb: [u8; 3]) {
        let [r, g, b] = rgb;
        if let [hi, lo] = out {
            *hi = (r & 0xF8) | (g >> 5);
            *lo = ((g << 3) & 0xE0) | (b >> 3);
        }
    }
}

/// 8-bit R, G, B.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb888;

impl PixelFormat for Rgb888 {
    const BYTES: usize = 3;
    #[inline(always)]
    fn put(out: &mut [u8], rgb: [u8; 3]) {
        for (d, s) in out.iter_mut().zip(rgb) {
            *d = s;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grey_stays_grey() {
        for v in [0u8, 1, 77, 128, 254, 255] {
            assert_eq!(ycc_to_rgb(v, 128, 128), [v; 3]);
        }
    }

    #[test]
    fn primaries() {
        // JFIF encodings of pure red, green and blue (rounded).
        assert_eq!(ycc_to_rgb(76, 85, 255), [254, 0, 0]);
        assert_eq!(ycc_to_rgb(150, 44, 21), [0, 255, 1]);
        assert_eq!(ycc_to_rgb(29, 255, 107), [0, 0, 254]);
    }

    #[test]
    fn rgb565_big_endian() {
        let mut px = [0u8; 2];
        Rgb565Be::put(&mut px, [0xFF, 0x00, 0x00]);
        assert_eq!(px, [0xF8, 0x00]);
        Rgb565Be::put(&mut px, [0x00, 0xFF, 0x00]);
        assert_eq!(px, [0x07, 0xE0]);
        Rgb565Be::put(&mut px, [0x00, 0x00, 0xFF]);
        assert_eq!(px, [0x00, 0x1F]);
    }
}
