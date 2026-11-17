//! Microphone sample processing (SC5): fixed-point levels and the G.711
//! µ-law codec.
//!
//! Levels ([`levels`]) are integer arithmetic, no floats: the peak is the
//! largest magnitude and the RMS the integer square root of the mean
//! square, both `u16` in sample units, so full scale (`i16::MIN`) is
//! 32768 = 0 dBFS. A block of up to 2³² samples cannot overflow the `u64`
//! sum of squares (each square is at most 2³⁰). The M7's FPU would do as
//! well; integers keep the result exact and identical on the host.
//!
//! µ-law ([`mulaw_encode`], [`mulaw_decode`]) is ITU-T G.711's, in the
//! formulation of the reference `g711.c`: the 16-bit sample is reduced to
//! G.711's 14-bit range (an arithmetic shift by two), its magnitude clipped
//! to 8159 and biased by 33, and the code is sign, segment (0..=7) and four
//! mantissa bits, all inverted. 0 encodes to 0xFF, the positive extreme to
//! 0x80 and the negative to 0x00; decoding gives the segment midpoints
//! scaled back to 16 bits (±32124 at the extremes).

/// Peak and RMS of a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Levels {
    /// Root mean square, in sample units (0..=32768), rounded down.
    pub rms: u16,
    /// Largest magnitude, in sample units (0..=32768).
    pub peak: u16,
}

/// The levels of a block; zero for an empty block.
pub fn levels(block: &[i16]) -> Levels {
    let mut sum: u64 = 0;
    let mut peak: u16 = 0;
    for &s in block {
        let m = s.unsigned_abs();
        peak = peak.max(m);
        sum = sum.saturating_add(u64::from(m) * u64::from(m));
    }
    let n = block.len() as u64;
    let mean = sum.checked_div(n).unwrap_or(0);
    // mean ≤ 2³⁰, so the root is ≤ 32768 and fits u16.
    let rms = u16::try_from(isqrt(mean)).unwrap_or(u16::MAX);
    Levels { rms, peak }
}

/// The integer square root (rounded down).
pub const fn isqrt(n: u64) -> u64 {
    // Bit-by-bit: no division, no overflow.
    let mut rem = n;
    let mut root: u64 = 0;
    let mut bit: u64 = 1 << 62;
    while bit > n {
        bit >>= 2;
    }
    while bit != 0 {
        if rem >= root + bit {
            rem -= root + bit;
            root = (root >> 1) + bit;
        } else {
            root >>= 1;
        }
        bit >>= 2;
    }
    root
}

/// µ-law bias (33 in the 14-bit domain).
const BIAS: i16 = 0x84 >> 2;
/// The largest 14-bit magnitude before the bias.
const CLIP: i16 = 8159;

/// Encode one 16-bit sample to G.711 µ-law.
pub const fn mulaw_encode(sample: i16) -> u8 {
    let mut v = sample >> 2; // −8192..=8191
    let mask: u8 = if v < 0 {
        v = -v; // ≤ 8192, no overflow
        0x7F
    } else {
        0xFF
    };
    if v > CLIP {
        v = CLIP;
    }
    let v = (v + BIAS) as u16; // 33..=8192
    // Segment: how many times v reaches 0x40, 0x80, …, 0x2000.
    let mut seg: u16 = 0;
    while seg < 8 && v >= (0x40 << seg) {
        seg += 1;
    }
    if seg >= 8 {
        return 0x7F ^ mask;
    }
    let code = ((seg << 4) | ((v >> (seg + 1)) & 0x0F)) as u8;
    code ^ mask
}

/// Decode one G.711 µ-law code to a 16-bit sample.
pub const fn mulaw_decode(code: u8) -> i16 {
    let u = !code;
    let t = ((((u & 0x0F) as i16) << 3) + 0x84) << ((u & 0x70) >> 4);
    if u & 0x80 != 0 { 0x84 - t } else { t - 0x84 }
}

/// Encode a block into `out`; returns the number of codes written (the
/// shorter of the two lengths).
pub fn mulaw_encode_block(block: &[i16], out: &mut [u8]) -> usize {
    let mut n = 0;
    for (o, &s) in out.iter_mut().zip(block) {
        *o = mulaw_encode(s);
        n += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    #[test]
    fn sc5_mulaw_spot_values_from_the_g711_table() {
        assert_eq!(mulaw_encode(0), 0xFF);
        assert_eq!(mulaw_encode(1), 0xFF);
        assert_eq!(mulaw_encode(3), 0xFF);
        assert_eq!(mulaw_encode(i16::MAX), 0x80);
        assert_eq!(mulaw_encode(i16::MIN), 0x00);
        assert_eq!(mulaw_encode(-4), 0x7E);
        assert_eq!(mulaw_decode(0xFF), 0);
        assert_eq!(mulaw_decode(0x7F), 0);
        assert_eq!(mulaw_decode(0x80), 32124);
        assert_eq!(mulaw_decode(0x00), -32124);
        assert_eq!(mulaw_decode(0xFE), 8);
        assert_eq!(mulaw_decode(0xEF), 132);
        assert_eq!(mulaw_decode(0x8F), 16764);
    }

    #[test]
    fn sc5_mulaw_segment_boundaries() {
        // G.711's µ-law decision values at the segment edges (14-bit
        // 31, 95, 223, 479, 991, 2015, 4063, 8159), × 4 for 16-bit: the
        // first value of each segment and the last of the one below.
        let edges: [(i16, u8); 7] = [
            (124, 0xEF),
            (380, 0xDF),
            (892, 0xCF),
            (1916, 0xBF),
            (3964, 0xAF),
            (8060, 0x9F),
            (16252, 0x8F),
        ];
        for (x, code) in edges {
            assert_eq!(mulaw_encode(x), code, "{x}");
            assert_eq!(mulaw_encode(x - 1), code + 1, "{}", x - 1);
            // The negative side is the same code without the sign bit.
            assert_eq!(mulaw_encode(-x), code & 0x7F, "{}", -x);
        }
        // Clipping: past 14-bit 8159 everything is the extreme code.
        assert_eq!(mulaw_encode(32636), 0x80);
        assert_eq!(mulaw_encode(32635), 0x80);
        assert_eq!(mulaw_encode(32000), 0x80);
        assert_eq!(mulaw_encode(31000), 0x81);
    }

    #[test]
    fn sc5_mulaw_round_trips_every_code_and_is_monotonic() {
        for c in 0..=u8::MAX {
            let expect = if c == 0x7F { 0xFF } else { c }; // −0 is +0
            assert_eq!(mulaw_encode(mulaw_decode(c)), expect, "{c:#04x}");
        }
        // Encoding is monotonic: larger input, never a smaller level.
        let level = |c: u8| mulaw_decode(c);
        let mut prev = level(mulaw_encode(i16::MIN));
        for x in (i16::MIN..=i16::MAX).step_by(7) {
            let l = level(mulaw_encode(x));
            assert!(l >= prev, "{x}");
            // Quantization error stays within half a step of the segment
            // (step 2^(seg+3) in 16-bit units, at most 1024), plus clipping.
            if x.unsigned_abs() < 32000 {
                assert!((i32::from(l) - i32::from(x)).abs() <= 1024, "{x}");
            }
            prev = l;
        }
        let mut out = [0u8; 4];
        assert_eq!(mulaw_encode_block(&[0, i16::MAX, i16::MIN], &mut out), 3);
        assert_eq!(out[..3], [0xFF, 0x80, 0x00]);
        assert_eq!(mulaw_encode_block(&[0; 9], &mut out), 4);
        assert_eq!(mulaw_encode_block(&[], &mut out), 0);
    }

    #[test]
    fn sc5_levels_are_fixed_point_rms_and_peak() {
        assert_eq!(levels(&[]), Levels { rms: 0, peak: 0 });
        assert_eq!(levels(&[0; 64]), Levels { rms: 0, peak: 0 });
        assert_eq!(
            levels(&[1000, -1000, 1000, -1000]),
            Levels {
                rms: 1000,
                peak: 1000
            }
        );
        assert_eq!(
            levels(&[i16::MIN; 16]),
            Levels {
                rms: 32768,
                peak: 32768
            }
        );
        assert_eq!(levels(&[i16::MAX, i16::MIN]).peak, 32768);
        assert_eq!(levels(&[3, 4]).rms, 3); // √12.5 = 3.53, rounded down
        // A full-scale square wave: RMS = peak; a sine: RMS ≈ peak / √2.
        let sine: Vec<i16> = (0..1600)
            .map(|i| {
                let p = 2.0 * core::f64::consts::PI * f64::from(i) / 16.0;
                (p.sin() * 16384.0) as i16
            })
            .collect();
        let l = levels(&sine);
        assert_eq!(l.peak, 16384);
        assert!((11580..=11590).contains(&l.rms), "{}", l.rms); // 11585
    }

    #[test]
    fn sc5_isqrt_is_exact_floor() {
        for n in [
            0u64,
            1,
            2,
            3,
            4,
            15,
            16,
            17,
            1 << 30,
            (1 << 30) - 1,
            u64::MAX,
        ] {
            let r = isqrt(n);
            assert!(r * r <= n, "{n}");
            assert!((r + 1).checked_mul(r + 1).is_none_or(|sq| sq > n), "{n}");
        }
        assert_eq!(isqrt(1 << 30), 32768);
        assert_eq!(isqrt(u64::MAX), u64::from(u32::MAX));
    }
}
