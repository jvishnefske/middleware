//! A minimal baseline JPEG encoder, integer only, so its output is
//! byte-for-byte reproducible on any host.
//!
//! What it writes, and nothing else:
//!
//! * `SOI`, one `DQT` with two 8-bit tables (Annex K luminance and
//!   chrominance, scaled by the IJG quality formula), `SOF0` (baseline,
//!   8-bit, three components: Y sampled 2×2, Cb and Cr 1×1, i.e. YCbCr
//!   4:2:0), one `DHT` with the four Annex K Huffman tables, `SOS` (all
//!   three components interleaved, `Ss = 0`, `Se = 63`, `Ah = Al = 0`), the
//!   entropy-coded MCUs, `EOI`.
//! * No `APPn` (no JFIF or EXIF header), no restart markers, no comment.
//!   The OV2640 emits the same structure.
//!
//! The pipeline: RGB888 → YCbCr (JFIF integer coefficients, 16-bit fixed
//! point) → chroma 2×2 box average → 16×16 MCUs, edges replicated → level
//! shift → the IJG `jfdctint` integer forward DCT → quantisation with
//! rounding → zig-zag, DC differences and AC run lengths → Huffman codes
//! with `0xFF` byte stuffing, the last byte padded with 1 bits.

/// Why [`encode`] refused an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeError {
    /// Width or height is 0.
    Empty,
    /// The pixel buffer is not `width * height * 3` bytes.
    Size,
}

/// Natural (row-major) index of each zig-zag position.
pub const ZIGZAG: [usize; 64] = zigzag();

const fn zigzag() -> [usize; 64] {
    let mut out = [0usize; 64];
    let mut i = 0;
    let mut s = 0usize;
    while s < 15 {
        let lo = s.saturating_sub(7);
        let mut a = if s < 8 { s } else { 7 };
        loop {
            // Even diagonals run up and to the right (row `a` falling),
            // odd ones down and to the left (column `a` falling).
            let (row, col) = if s.is_multiple_of(2) {
                (a, s - a)
            } else {
                (s - a, a)
            };
            out[i] = row * 8 + col;
            i += 1;
            if a == lo {
                break;
            }
            a -= 1;
        }
        s += 1;
    }
    out
}

/// Annex K.1 luminance quantisation table, natural order (quality 50).
const LUMA_Q: [u16; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, //
    12, 12, 14, 19, 26, 58, 60, 55, //
    14, 13, 16, 24, 40, 57, 69, 56, //
    14, 17, 22, 29, 51, 87, 80, 62, //
    18, 22, 37, 56, 68, 109, 103, 77, //
    24, 35, 55, 64, 81, 104, 113, 92, //
    49, 64, 78, 87, 103, 121, 120, 101, //
    72, 92, 95, 98, 112, 100, 103, 99,
];

/// Annex K.1 chrominance quantisation table, natural order (quality 50).
const CHROMA_Q: [u16; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, //
    18, 21, 26, 66, 99, 99, 99, 99, //
    24, 26, 56, 99, 99, 99, 99, 99, //
    47, 66, 99, 99, 99, 99, 99, 99, //
    99, 99, 99, 99, 99, 99, 99, 99, //
    99, 99, 99, 99, 99, 99, 99, 99, //
    99, 99, 99, 99, 99, 99, 99, 99, //
    99, 99, 99, 99, 99, 99, 99, 99,
];

/// A Huffman table as `DHT` stores it: code counts per length 1..=16 and
/// the symbols in code order.
#[derive(Debug, Clone, Copy)]
pub struct HuffSpec {
    /// Number of codes of each length 1..=16.
    pub bits: [u8; 16],
    /// Symbols, in order of increasing code.
    pub vals: &'static [u8],
}

/// Annex K.3 luminance DC table.
pub const DC_LUMA: HuffSpec = HuffSpec {
    bits: [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0],
    vals: &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
};

/// Annex K.3 chrominance DC table.
pub const DC_CHROMA: HuffSpec = HuffSpec {
    bits: [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0],
    vals: &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
};

/// Annex K.3 luminance AC table.
pub const AC_LUMA: HuffSpec = HuffSpec {
    bits: [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d],
    vals: &[
        0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61,
        0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52,
        0xd1, 0xf0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25,
        0x26, 0x27, 0x28, 0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45,
        0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64,
        0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83,
        0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99,
        0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6,
        0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3,
        0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8,
        0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
    ],
};

/// Annex K.3 chrominance AC table.
pub const AC_CHROMA: HuffSpec = HuffSpec {
    bits: [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77],
    vals: &[
        0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61,
        0x71, 0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33,
        0x52, 0xf0, 0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18,
        0x19, 0x1a, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44,
        0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63,
        0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a,
        0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97,
        0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4,
        0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca,
        0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7,
        0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
    ],
};

/// The quantisation table `base` scaled to `quality` (1..=100) with the IJG
/// formula, each entry clamped to `1..=255`.
pub fn scaled_table(base: &[u16; 64], quality: u8) -> [u16; 64] {
    let q = u32::from(quality.clamp(1, 100));
    let scale = if q < 50 { 5000 / q } else { 200 - 2 * q };
    base.map(|b| ((u32::from(b) * scale + 50) / 100).clamp(1, 255) as u16)
}

/// Canonical codes (Annex C) of a [`HuffSpec`]: `(code, length)` per symbol.
fn codes(spec: &HuffSpec) -> [(u16, u8); 256] {
    let mut out = [(0u16, 0u8); 256];
    let mut code = 0u16;
    let mut vals = spec.vals.iter();
    for (len, &count) in (1u8..).zip(spec.bits.iter()) {
        for _ in 0..count {
            if let Some(&v) = vals.next() {
                out[usize::from(v)] = (code, len);
            }
            code = code.wrapping_add(1);
        }
        code <<= 1;
    }
    out
}

/// The IJG `jfdctint` (slow-but-accurate integer) forward DCT, in place.
/// Input: level-shifted samples; output: coefficients scaled by 8.
pub fn fdct(block: &mut [i32; 64]) {
    const CONST_BITS: u32 = 13;
    const PASS1_BITS: u32 = 2;
    const F_0_298: i32 = 2446;
    const F_0_390: i32 = 3196;
    const F_0_541: i32 = 4433;
    const F_0_765: i32 = 6270;
    const F_0_899: i32 = 7373;
    const F_1_175: i32 = 9633;
    const F_1_501: i32 = 12299;
    const F_1_847: i32 = 15137;
    const F_1_961: i32 = 16069;
    const F_2_053: i32 = 16819;
    const F_2_562: i32 = 20995;
    const F_3_072: i32 = 25172;
    fn descale(x: i32, n: u32) -> i32 {
        (x + (1 << (n - 1))) >> n
    }
    // One 1-D pass over 8 values `stride` apart starting at `at`. The even
    // outputs 0 and 4 are scaled by `shift_dc` (left) or descaled (right),
    // the rest descaled by `odd_shift`.
    fn pass(b: &mut [i32; 64], at: usize, stride: usize, first: bool) {
        let d = |i: usize| b[at + i * stride];
        let (d0, d1, d2, d3, d4, d5, d6, d7) = (d(0), d(1), d(2), d(3), d(4), d(5), d(6), d(7));
        let (t0, t7) = (d0 + d7, d0 - d7);
        let (t1, t6) = (d1 + d6, d1 - d6);
        let (t2, t5) = (d2 + d5, d2 - d5);
        let (t3, t4) = (d3 + d4, d3 - d4);
        let (t10, t13) = (t0 + t3, t0 - t3);
        let (t11, t12) = (t1 + t2, t1 - t2);
        let shift = if first {
            CONST_BITS - PASS1_BITS
        } else {
            CONST_BITS + PASS1_BITS
        };
        let (o0, o4) = if first {
            ((t10 + t11) << PASS1_BITS, (t10 - t11) << PASS1_BITS)
        } else {
            (
                descale(t10 + t11, PASS1_BITS),
                descale(t10 - t11, PASS1_BITS),
            )
        };
        let z1 = (t12 + t13) * F_0_541;
        let o2 = descale(z1 + t13 * F_0_765, shift);
        let o6 = descale(z1 - t12 * F_1_847, shift);
        let z1 = t4 + t7;
        let z2 = t5 + t6;
        let z3 = t4 + t6;
        let z4 = t5 + t7;
        let z5 = (z3 + z4) * F_1_175;
        let (t4, t5, t6, t7) = (t4 * F_0_298, t5 * F_2_053, t6 * F_3_072, t7 * F_1_501);
        let (z1, z2) = (-z1 * F_0_899, -z2 * F_2_562);
        let z3 = -z3 * F_1_961 + z5;
        let z4 = -z4 * F_0_390 + z5;
        let out = [
            o0,
            descale(t7 + z1 + z4, shift),
            o2,
            descale(t6 + z2 + z3, shift),
            o4,
            descale(t5 + z2 + z4, shift),
            o6,
            descale(t4 + z1 + z3, shift),
        ];
        for (i, v) in out.into_iter().enumerate() {
            b[at + i * stride] = v;
        }
    }
    for row in 0..8 {
        pass(block, row * 8, 1, true);
    }
    for col in 0..8 {
        pass(block, col, 8, false);
    }
}

/// Divides DCT output (scaled by 8) by `q * 8`, rounding half away from 0.
fn quantize(coef: i32, q: u16) -> i32 {
    let d = i32::from(q) * 8;
    let half = d / 2;
    if coef < 0 {
        -((-coef + half) / d)
    } else {
        (coef + half) / d
    }
}

/// Appends entropy-coded bits, stuffing a 0 after every `0xFF` byte.
struct BitWriter {
    out: Vec<u8>,
    acc: u8,
    used: u8,
}

impl BitWriter {
    fn put(&mut self, value: u32, len: u8) {
        for i in (0..len).rev() {
            self.acc = (self.acc << 1) | ((value >> i) & 1) as u8;
            self.used += 1;
            if self.used == 8 {
                self.out.push(self.acc);
                if self.acc == 0xFF {
                    self.out.push(0);
                }
                self.acc = 0;
                self.used = 0;
            }
        }
    }

    /// Pads the last byte with 1 bits.
    fn flush(mut self) -> Vec<u8> {
        if self.used > 0 {
            let pad = 8 - self.used;
            self.put((1 << pad) - 1, pad);
        }
        self.out
    }
}

/// Bits needed for the magnitude of `v` (JPEG "category").
fn category(v: i32) -> u8 {
    (32 - v.unsigned_abs().leading_zeros()) as u8
}

/// The `cat` magnitude bits of `v` (one's complement for negatives).
fn magnitude(v: i32, cat: u8) -> u32 {
    let bits = if v < 0 { v - 1 } else { v };
    (bits as u32) & ((1u32 << cat) - 1)
}

/// One component's coding state.
struct Coder {
    q: [u16; 64],
    dc: [(u16, u8); 256],
    ac: [(u16, u8); 256],
    prev_dc: i32,
}

impl Coder {
    /// DCTs, quantises and codes one block of level-shifted samples.
    fn block(&mut self, w: &mut BitWriter, mut samples: [i32; 64]) {
        fdct(&mut samples);
        let mut z = [0i32; 64];
        for (k, &n) in ZIGZAG.iter().enumerate() {
            z[k] = quantize(samples[n], self.q[n]);
        }
        let diff = z[0] - self.prev_dc;
        self.prev_dc = z[0];
        let cat = category(diff);
        let (code, len) = self.dc[usize::from(cat)];
        w.put(u32::from(code), len);
        w.put(magnitude(diff, cat), cat);
        let mut run = 0u8;
        for &v in &z[1..] {
            if v == 0 {
                run += 1;
                continue;
            }
            while run > 15 {
                let (code, len) = self.ac[0xF0];
                w.put(u32::from(code), len);
                run -= 16;
            }
            let cat = category(v);
            let (code, len) = self.ac[usize::from((run << 4) | cat)];
            w.put(u32::from(code), len);
            w.put(magnitude(v, cat), cat);
            run = 0;
        }
        if run > 0 {
            let (code, len) = self.ac[0];
            w.put(u32::from(code), len);
        }
    }
}

/// Clamps `v` to `0..=255`.
fn clamp8(v: i32) -> i32 {
    v.clamp(0, 255)
}

/// JFIF RGB → YCbCr, 16-bit fixed point.
fn ycbcr(r: i32, g: i32, b: i32) -> [i32; 3] {
    let y = (19595 * r + 38470 * g + 7471 * b + 32768) >> 16;
    let cb = (-11059 * r - 21709 * g + 32768 * b + (128 << 16) + 32767) >> 16;
    let cr = (32768 * r - 27439 * g - 5329 * b + (128 << 16) + 32767) >> 16;
    [clamp8(y), clamp8(cb), clamp8(cr)]
}

fn segment(out: &mut Vec<u8>, marker: u8, body: &[u8]) {
    out.extend_from_slice(&[0xFF, marker]);
    out.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(body);
}

/// Encodes `rgb` (`width * height` RGB888 pixels, row-major) as a baseline
/// YCbCr 4:2:0 JPEG at `quality` (1..=100, clamped).
pub fn encode(width: u16, height: u16, rgb: &[u8], quality: u8) -> Result<Vec<u8>, EncodeError> {
    if width == 0 || height == 0 {
        return Err(EncodeError::Empty);
    }
    let (w, h) = (usize::from(width), usize::from(height));
    if rgb.len() != w * h * 3 {
        return Err(EncodeError::Size);
    }
    let planes: Vec<[i32; 3]> = rgb
        .chunks_exact(3)
        .map(|p| ycbcr(i32::from(p[0]), i32::from(p[1]), i32::from(p[2])))
        .collect();
    // Sample of component `c` at (x, y), edges replicated.
    let at = |c: usize, x: usize, y: usize| planes[y.min(h - 1) * w + x.min(w - 1)][c];

    let luma_q = scaled_table(&LUMA_Q, quality);
    let chroma_q = scaled_table(&CHROMA_Q, quality);

    let mut out = vec![0xFF, 0xD8];
    let mut dqt = vec![0x00];
    dqt.extend(ZIGZAG.iter().map(|&n| luma_q[n] as u8));
    dqt.push(0x01);
    dqt.extend(ZIGZAG.iter().map(|&n| chroma_q[n] as u8));
    segment(&mut out, 0xDB, &dqt);
    let [h0, h1] = height.to_be_bytes();
    let [w0, w1] = width.to_be_bytes();
    segment(
        &mut out,
        0xC0,
        &[8, h0, h1, w0, w1, 3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1],
    );
    let mut dht = Vec::new();
    for (class_id, spec) in [
        (0x00, DC_LUMA),
        (0x10, AC_LUMA),
        (0x01, DC_CHROMA),
        (0x11, AC_CHROMA),
    ] {
        dht.push(class_id);
        dht.extend_from_slice(&spec.bits);
        dht.extend_from_slice(spec.vals);
    }
    segment(&mut out, 0xC4, &dht);
    segment(&mut out, 0xDA, &[3, 1, 0x00, 2, 0x11, 3, 0x11, 0, 63, 0]);

    let mut y = Coder {
        q: luma_q,
        dc: codes(&DC_LUMA),
        ac: codes(&AC_LUMA),
        prev_dc: 0,
    };
    let chroma = || Coder {
        q: chroma_q,
        dc: codes(&DC_CHROMA),
        ac: codes(&AC_CHROMA),
        prev_dc: 0,
    };
    let (mut cb, mut cr) = (chroma(), chroma());
    let mut bits = BitWriter {
        out: Vec::new(),
        acc: 0,
        used: 0,
    };
    for my in 0..h.div_ceil(16) {
        for mx in 0..w.div_ceil(16) {
            let (x0, y0) = (mx * 16, my * 16);
            for (bx, by) in [(0, 0), (8, 0), (0, 8), (8, 8)] {
                let block = core::array::from_fn(|i| at(0, x0 + bx + i % 8, y0 + by + i / 8) - 128);
                y.block(&mut bits, block);
            }
            for (c, coder) in [(1, &mut cb), (2, &mut cr)] {
                let block = core::array::from_fn(|i| {
                    let (sx, sy) = (x0 + 2 * (i % 8), y0 + 2 * (i / 8));
                    let sum = at(c, sx, sy)
                        + at(c, sx + 1, sy)
                        + at(c, sx, sy + 1)
                        + at(c, sx + 1, sy + 1);
                    ((sum + 2) >> 2) - 128
                });
                coder.block(&mut bits, block);
            }
        }
    }
    out.extend_from_slice(&bits.flush());
    out.extend_from_slice(&[0xFF, 0xD9]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zigzag_starts_and_ends_right_and_is_a_permutation() {
        assert_eq!(&ZIGZAG[..10], &[0, 1, 8, 16, 9, 2, 3, 10, 17, 24]);
        assert_eq!(&ZIGZAG[58..], &[61, 54, 47, 55, 62, 63]);
        let mut seen = [false; 64];
        for &n in &ZIGZAG {
            assert!(!seen[n]);
            seen[n] = true;
        }
    }

    #[test]
    fn huffman_tables_are_complete_and_cover_every_symbol() {
        for spec in [DC_LUMA, DC_CHROMA] {
            let n: usize = spec.bits.iter().map(|&b| usize::from(b)).sum();
            assert_eq!(n, spec.vals.len());
            assert_eq!(spec.vals, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]);
        }
        // Every AC symbol: EOB, ZRL and (run 0..15, size 1..10).
        let mut want: Vec<u8> = vec![0x00, 0xF0];
        for run in 0..16u8 {
            for size in 1..=10u8 {
                want.push(run << 4 | size);
            }
        }
        want.sort_unstable();
        for spec in [AC_LUMA, AC_CHROMA] {
            let n: usize = spec.bits.iter().map(|&b| usize::from(b)).sum();
            assert_eq!(n, spec.vals.len());
            let mut vals = spec.vals.to_vec();
            vals.sort_unstable();
            assert_eq!(vals, want);
        }
        // Canonical codes never run out of room (Kraft sum ≤ 1, and no
        // code is all ones).
        for spec in [DC_LUMA, DC_CHROMA, AC_LUMA, AC_CHROMA] {
            let c = codes(&spec);
            assert!(spec.vals.iter().all(|&v| {
                let (code, len) = c[usize::from(v)];
                len > 0 && u32::from(code) < (1 << len) - 1
            }));
        }
    }

    /// The integer DCT matches the textbook float DCT (scaled by 8) to
    /// within rounding.
    #[test]
    fn fdct_matches_the_reference_dct() {
        let mut block: [i32; 64] = core::array::from_fn(|i| ((i * 37 + 11) % 256) as i32 - 128);
        let input = block;
        fdct(&mut block);
        for v in 0..8 {
            for u in 0..8 {
                let cu = if u == 0 {
                    std::f64::consts::FRAC_1_SQRT_2
                } else {
                    1.0
                };
                let cv = if v == 0 {
                    std::f64::consts::FRAC_1_SQRT_2
                } else {
                    1.0
                };
                let mut sum = 0.0;
                for y in 0..8 {
                    for x in 0..8 {
                        let f = f64::from(input[y * 8 + x]);
                        let a = ((2 * x + 1) as f64 * u as f64 * std::f64::consts::PI / 16.0).cos();
                        let b = ((2 * y + 1) as f64 * v as f64 * std::f64::consts::PI / 16.0).cos();
                        sum += f * a * b;
                    }
                }
                let want = 0.25 * cu * cv * sum * 8.0;
                let got = f64::from(block[v * 8 + u]);
                assert!((got - want).abs() <= 8.0, "({u},{v}): {got} vs {want}");
            }
        }
    }

    #[test]
    fn quantize_rounds_half_away_from_zero_and_tables_scale() {
        assert_eq!(quantize(8 * 16 * 3 / 2, 16), 2);
        assert_eq!(quantize(-(8 * 16 * 3 / 2), 16), -2);
        assert_eq!(quantize(8 * 16 / 2 - 1, 16), 0);
        assert_eq!(scaled_table(&LUMA_Q, 50), LUMA_Q);
        assert_eq!(scaled_table(&LUMA_Q, 100), [1; 64]);
        assert_eq!(scaled_table(&LUMA_Q, 1)[0], 255);
        assert_eq!(scaled_table(&LUMA_Q, 25)[0], 32);
    }

    #[test]
    fn categories_and_magnitudes() {
        assert_eq!(category(0), 0);
        assert_eq!(category(1), 1);
        assert_eq!(category(-1), 1);
        assert_eq!(category(-3), 2);
        assert_eq!(category(1023), 10);
        assert_eq!(magnitude(-1, 1), 0);
        assert_eq!(magnitude(-3, 2), 0);
        assert_eq!(magnitude(-2, 2), 1);
        assert_eq!(magnitude(3, 2), 3);
    }

    #[test]
    fn bit_writer_stuffs_and_pads() {
        let mut w = BitWriter {
            out: Vec::new(),
            acc: 0,
            used: 0,
        };
        w.put(0xFF, 8);
        w.put(0, 1);
        assert_eq!(w.flush(), vec![0xFF, 0x00, 0x7F]);
    }

    /// Marker structure: SOI, DQT, SOF0 (4:2:0), DHT, SOS, data, EOI; no
    /// APPn or DRI, and no marker inside the entropy-coded data.
    #[test]
    fn writes_a_baseline_420_stream() {
        let (w, h) = (20u16, 12u16);
        let rgb: Vec<u8> = (0..usize::from(w) * usize::from(h) * 3)
            .map(|i| (i * 7) as u8)
            .collect();
        let jpg = encode(w, h, &rgb, 50).expect("encodes");
        assert_eq!(&jpg[..2], &[0xFF, 0xD8]);
        assert_eq!(&jpg[jpg.len() - 2..], &[0xFF, 0xD9]);
        let mut at = 2;
        let mut markers = Vec::new();
        while jpg[at + 1] != 0xDA {
            assert_eq!(jpg[at], 0xFF);
            markers.push(jpg[at + 1]);
            let len = usize::from(u16::from_be_bytes([jpg[at + 2], jpg[at + 3]]));
            if jpg[at + 1] == 0xC0 {
                let sof = &jpg[at + 4..at + 2 + len];
                assert_eq!(
                    sof,
                    &[8, 0, 12, 0, 20, 3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]
                );
            }
            at += 2 + len;
        }
        assert_eq!(markers, vec![0xDB, 0xC0, 0xC4]);
        let sos_len = usize::from(u16::from_be_bytes([jpg[at + 2], jpg[at + 3]]));
        let data = &jpg[at + 2 + sos_len..jpg.len() - 2];
        for pair in data.windows(2) {
            assert!(
                pair[0] != 0xFF || pair[1] == 0x00,
                "unstuffed 0xFF in scan data"
            );
        }
        assert_eq!(encode(0, 1, &[], 50), Err(EncodeError::Empty));
        assert_eq!(encode(1, 1, &[0; 2], 50), Err(EncodeError::Size));
    }
}
