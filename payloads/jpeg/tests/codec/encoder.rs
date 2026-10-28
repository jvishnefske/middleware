//! A minimal baseline JPEG encoder for the tests, with a floating point
//! reference decode of its own output.
//!
//! No JPEG crate is available offline, so the tests encode their images
//! here: JFIF YCbCr (or grayscale), 4:4:4 / 4:2:2 / 4:2:0 with box-filtered
//! chroma, the IJG-scaled Annex K quantisation tables, the Annex K Huffman
//! tables, one interleaved scan and optional restart intervals.
//!
//! The reference decode starts from the quantised coefficients the encoder
//! wrote (so it shares no code with the decoder under test): exact float
//! IDCT, rounding, clamping, chroma replication and float YCbCr → RGB. For
//! half scale it box-averages each block's unclamped 8×8 IDCT to 4×4 (what
//! libjpeg's reduced IDCT computes) and replicates chroma at that size.

use std::f64::consts::PI;

/// Chroma subsampling of an encoded image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sampling {
    /// One component.
    Gray,
    /// Y, Cb, Cr at full resolution.
    S444,
    /// Chroma halved horizontally.
    S422,
    /// Chroma halved both ways.
    S420,
}

impl Sampling {
    /// Luma sampling factors (h, v); chroma is always 1×1.
    fn luma(self) -> (usize, usize) {
        match self {
            Sampling::Gray | Sampling::S444 => (1, 1),
            Sampling::S422 => (2, 1),
            Sampling::S420 => (2, 2),
        }
    }
}

/// An encoded image and its reference decodes (RGB888, row-major).
pub struct Encoded {
    /// The JPEG file.
    pub jpeg: Vec<u8>,
    /// Full-scale reference.
    pub full: Vec<u8>,
    /// Half-scale reference, `ceil(w/2) × ceil(h/2)` (read only with the
    /// `half` feature).
    #[cfg_attr(not(feature = "half"), allow(dead_code))]
    pub half: Vec<u8>,
}

/// Zigzag position → natural (row-major) index.
pub const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// Annex K.1 luminance quantisation table (natural order).
const LUMA_Q: [u16; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69, 56,
    14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113,
    92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
];

/// Annex K.1 chrominance quantisation table (natural order).
const CHROMA_Q: [u16; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99, 99, 99,
    47, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
];

/// Annex K.3 Huffman tables: (code-length counts, symbols).
pub struct Spec {
    /// Codes of each length 1..=16.
    pub bits: [u8; 16],
    /// Symbols in code order.
    pub vals: &'static [u8],
}

/// DC luminance.
pub const DC_LUMA: Spec = Spec {
    bits: [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0],
    vals: &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
};
/// DC chrominance.
pub const DC_CHROMA: Spec = Spec {
    bits: [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0],
    vals: &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
};
/// AC luminance.
pub const AC_LUMA: Spec = Spec {
    bits: [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 125],
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
/// AC chrominance.
pub const AC_CHROMA: Spec = Spec {
    bits: [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 119],
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

/// Canonical codes of a table: `(code, length)` per symbol.
fn codes(spec: &Spec) -> [(u16, u8); 256] {
    let mut out = [(0u16, 0u8); 256];
    let mut code = 0u16;
    let mut k = 0;
    for (len, &n) in (1u8..=16).zip(&spec.bits) {
        for _ in 0..n {
            out[usize::from(spec.vals[k])] = (code, len);
            code += 1;
            k += 1;
        }
        code <<= 1;
    }
    out
}

/// An IJG-scaled quantisation table (natural order), 8-bit.
fn scaled(base: &[u16; 64], quality: u32) -> [u8; 64] {
    let q = quality.clamp(1, 100);
    let scale = if q < 50 { 5000 / q } else { 200 - 2 * q };
    base.map(|b| ((u32::from(b) * scale + 50) / 100).clamp(1, 255) as u8)
}

/// MSB-first bit writer with byte stuffing.
struct Bits {
    out: Vec<u8>,
    acc: u32,
    n: u32,
}

impl Bits {
    fn put(&mut self, value: u32, len: u32) {
        for i in (0..len).rev() {
            self.acc = (self.acc << 1) | ((value >> i) & 1);
            self.n += 1;
            if self.n == 8 {
                let b = self.acc as u8;
                self.out.push(b);
                if b == 0xFF {
                    self.out.push(0);
                }
                self.acc = 0;
                self.n = 0;
            }
        }
    }

    /// Pads the last byte with ones.
    fn flush(&mut self) {
        if self.n > 0 {
            self.put(0x7F, 8 - self.n);
        }
    }
}

/// Magnitude category and bits of `v`.
fn category(v: i32) -> (u32, u32) {
    let size = 32 - v.unsigned_abs().leading_zeros();
    let bits = if v < 0 {
        (v - 1) as u32 & ((1 << size) - 1)
    } else {
        v as u32
    };
    (size, bits)
}

/// Float forward DCT of a level-shifted 8×8 block (row-major).
fn fdct(block: &[f64; 64]) -> [f64; 64] {
    let c = |u: usize| if u == 0 { 1.0 / 2f64.sqrt() } else { 1.0 };
    let mut out = [0.0; 64];
    for v in 0..8 {
        for u in 0..8 {
            let mut s = 0.0;
            for y in 0..8 {
                for x in 0..8 {
                    s += block[y * 8 + x]
                        * (((2 * x + 1) * u) as f64 * PI / 16.0).cos()
                        * (((2 * y + 1) * v) as f64 * PI / 16.0).cos();
                }
            }
            out[v * 8 + u] = s * c(u) * c(v) / 4.0;
        }
    }
    out
}

/// Float inverse DCT, unclamped and level-shifted.
fn idct(coef: &[f64; 64]) -> [f64; 64] {
    let c = |u: usize| if u == 0 { 1.0 / 2f64.sqrt() } else { 1.0 };
    let mut out = [0.0; 64];
    for y in 0..8 {
        for x in 0..8 {
            let mut s = 0.0;
            for v in 0..8 {
                for u in 0..8 {
                    s += c(u)
                        * c(v)
                        * coef[v * 8 + u]
                        * (((2 * x + 1) * u) as f64 * PI / 16.0).cos()
                        * (((2 * y + 1) * v) as f64 * PI / 16.0).cos();
                }
            }
            out[y * 8 + x] = s / 4.0 + 128.0;
        }
    }
    out
}

/// A component plane, padded to whole blocks.
struct Plane {
    w: usize,
    h: usize,
    data: Vec<f64>,
}

impl Plane {
    fn at(&self, x: usize, y: usize) -> f64 {
        self.data[y.min(self.h - 1) * self.w + x.min(self.w - 1)]
    }
}

/// Clamped rounding to a byte.
fn byte(v: f64) -> u8 {
    v.round().clamp(0.0, 255.0) as u8
}

/// Float JFIF YCbCr → RGB.
fn to_rgb(y: f64, cb: f64, cr: f64) -> [u8; 3] {
    let (cb, cr) = (cb - 128.0, cr - 128.0);
    [
        byte(y + 1.402 * cr),
        byte(y - 0.344136 * cb - 0.714136 * cr),
        byte(y + 1.772 * cb),
    ]
}

/// Encodes `rgb` (`w × h`, RGB888) and decodes the reference images.
pub fn encode(
    rgb: &[u8],
    w: usize,
    h: usize,
    sampling: Sampling,
    quality: u32,
    restart: u16,
) -> Encoded {
    let (hy, vy) = sampling.luma();
    let gray = sampling == Sampling::Gray;
    let (mw, mh) = (8 * hy, 8 * vy);
    let (mx, my) = (w.div_ceil(mw), h.div_ceil(mh));
    // Source planes (edge-replicated, padded to whole MCUs).
    let px = |x: usize, y: usize| {
        let o = (y.min(h - 1) * w + x.min(w - 1)) * 3;
        (
            f64::from(rgb[o]),
            f64::from(rgb[o + 1]),
            f64::from(rgb[o + 2]),
        )
    };
    let ycc = |x: usize, y: usize| {
        let (r, g, b) = px(x, y);
        [
            0.299 * r + 0.587 * g + 0.114 * b,
            -0.168736 * r - 0.331264 * g + 0.5 * b + 128.0,
            0.5 * r - 0.418688 * g - 0.081312 * b + 128.0,
        ]
    };
    let ncomp = if gray { 1 } else { 3 };
    let mut planes = Vec::new();
    for c in 0..ncomp {
        let (sx, sy) = if c == 0 { (1, 1) } else { (hy, vy) };
        let (pw, ph) = (mx * mw / sx, my * mh / sy);
        let mut data = vec![0.0; pw * ph];
        for y in 0..ph {
            for x in 0..pw {
                let mut s = 0.0;
                for dy in 0..sy {
                    for dx in 0..sx {
                        s += ycc(x * sx + dx, y * sy + dy)[c];
                    }
                }
                data[y * pw + x] = s / (sx * sy) as f64;
            }
        }
        planes.push(Plane { w: pw, h: ph, data });
    }
    let qt = [scaled(&LUMA_Q, quality), scaled(&CHROMA_Q, quality)];
    let tables = [
        (codes(&DC_LUMA), codes(&AC_LUMA)),
        (codes(&DC_CHROMA), codes(&AC_CHROMA)),
    ];

    // Headers.
    let mut out = vec![0xFF, 0xD8];
    let seg = |out: &mut Vec<u8>, marker: u8, body: &[u8]| {
        out.extend_from_slice(&[0xFF, marker]);
        out.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(body);
    };
    seg(&mut out, 0xE0, b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
    seg(&mut out, 0xFE, b"mw-jpeg test encoder");
    for (id, q) in qt.iter().enumerate().take(if gray { 1 } else { 2 }) {
        let mut body = vec![id as u8];
        body.extend(ZIGZAG.iter().map(|&z| q[z]));
        seg(&mut out, 0xDB, &body);
    }
    let mut sof = vec![8];
    sof.extend_from_slice(&(h as u16).to_be_bytes());
    sof.extend_from_slice(&(w as u16).to_be_bytes());
    sof.push(ncomp as u8);
    for c in 0..ncomp {
        let hv = if c == 0 { ((hy << 4) | vy) as u8 } else { 0x11 };
        sof.extend_from_slice(&[c as u8 + 1, hv, u8::from(c > 0)]);
    }
    seg(&mut out, 0xC0, &sof);
    for (class, specs) in [(0u8, [&DC_LUMA, &DC_CHROMA]), (1, [&AC_LUMA, &AC_CHROMA])] {
        for (id, spec) in specs.iter().enumerate().take(if gray { 1 } else { 2 }) {
            let mut body = vec![(class << 4) | id as u8];
            body.extend_from_slice(&spec.bits);
            body.extend_from_slice(spec.vals);
            seg(&mut out, 0xC4, &body);
        }
    }
    if restart > 0 {
        seg(&mut out, 0xDD, &restart.to_be_bytes());
    }
    let mut sos = vec![ncomp as u8];
    for c in 0..ncomp {
        sos.extend_from_slice(&[c as u8 + 1, if c == 0 { 0x00 } else { 0x11 }]);
    }
    sos.extend_from_slice(&[0, 63, 0]);
    seg(&mut out, 0xDA, &sos);

    // Entropy-coded data and reference planes (full and half scale).
    let mut bits = Bits { out, acc: 0, n: 0 };
    let mut recon: Vec<Vec<f64>> = planes.iter().map(|p| vec![0.0; p.w * p.h]).collect();
    let mut recon_half: Vec<Vec<f64>> = planes.iter().map(|p| vec![0.0; p.w * p.h / 4]).collect();
    let mut preds = [0i32; 3];
    let mut rst = 0u8;
    let total = mx * my;
    for m in 0..total {
        if restart > 0 && m > 0 && m % usize::from(restart) == 0 {
            bits.flush();
            bits.out.extend_from_slice(&[0xFF, 0xD0 + rst]);
            rst = (rst + 1) & 7;
            preds = [0; 3];
        }
        let (bx, by) = (m % mx, m / mx);
        for c in 0..ncomp {
            let (nh, nv) = if c == 0 { (hy, vy) } else { (1, 1) };
            let q = &qt[usize::from(c > 0)];
            let (dc_codes, ac_codes) = &tables[usize::from(c > 0)];
            let p = &planes[c];
            for v in 0..nv {
                for u in 0..nh {
                    let (x0, y0) = ((bx * nh + u) * 8, (by * nv + v) * 8);
                    let mut block = [0.0; 64];
                    for y in 0..8 {
                        for x in 0..8 {
                            block[y * 8 + x] = p.at(x0 + x, y0 + y) - 128.0;
                        }
                    }
                    let f = fdct(&block);
                    let quant: Vec<i32> = (0..64)
                        .map(|i| (f[i] / f64::from(q[i])).round() as i32)
                        .collect();
                    // Reference reconstruction.
                    let deq: [f64; 64] =
                        std::array::from_fn(|i| f64::from(quant[i] * i32::from(q[i])));
                    let s = idct(&deq);
                    for y in 0..8 {
                        for x in 0..8 {
                            recon[c][(y0 + y) * p.w + x0 + x] = s[y * 8 + x];
                        }
                    }
                    for y in 0..4 {
                        for x in 0..4 {
                            let avg = (s[2 * y * 8 + 2 * x]
                                + s[2 * y * 8 + 2 * x + 1]
                                + s[(2 * y + 1) * 8 + 2 * x]
                                + s[(2 * y + 1) * 8 + 2 * x + 1])
                                / 4.0;
                            recon_half[c][(y0 / 2 + y) * (p.w / 2) + x0 / 2 + x] = avg;
                        }
                    }
                    // Huffman coding.
                    let diff = quant[0] - preds[c];
                    preds[c] = quant[0];
                    let (size, b) = category(diff);
                    let (code, len) = dc_codes[size as usize];
                    bits.put(u32::from(code), u32::from(len));
                    bits.put(b, size);
                    let mut run = 0;
                    for &z in &ZIGZAG[1..] {
                        let a = quant[z];
                        if a == 0 {
                            run += 1;
                            continue;
                        }
                        while run > 15 {
                            let (code, len) = ac_codes[0xF0];
                            bits.put(u32::from(code), u32::from(len));
                            run -= 16;
                        }
                        let (size, b) = category(a);
                        let (code, len) = ac_codes[(run << 4) | size as usize];
                        bits.put(u32::from(code), u32::from(len));
                        bits.put(b, size);
                        run = 0;
                    }
                    if run > 0 {
                        let (code, len) = ac_codes[0];
                        bits.put(u32::from(code), u32::from(len));
                    }
                }
            }
        }
    }
    bits.flush();
    let mut jpeg = bits.out;
    jpeg.extend_from_slice(&[0xFF, 0xD9]);

    // Colour conversion of the references, chroma replicated.
    let render = |planes: &[Vec<f64>], pw: &dyn Fn(usize) -> usize, ow: usize, oh: usize| {
        let mut img = Vec::with_capacity(ow * oh * 3);
        for y in 0..oh {
            for x in 0..ow {
                let yv = planes[0][y * pw(0) + x].round().clamp(0.0, 255.0);
                if gray {
                    let g = yv as u8;
                    img.extend_from_slice(&[g, g, g]);
                } else {
                    let (cx, cy) = (x / hy, y / vy);
                    let cb = planes[1][cy * pw(1) + cx].round().clamp(0.0, 255.0);
                    let cr = planes[2][cy * pw(2) + cx].round().clamp(0.0, 255.0);
                    img.extend_from_slice(&to_rgb(yv, cb, cr));
                }
            }
        }
        img
    };
    let full = render(&recon, &|c| planes[c].w, w, h);
    let half = render(
        &recon_half,
        &|c| planes[c].w / 2,
        w.div_ceil(2),
        h.div_ceil(2),
    );
    Encoded { jpeg, full, half }
}

/// A deterministic test image: gradients, a disc, bars and seeded noise.
pub fn image(w: usize, h: usize, noise: u32, seed: u32) -> Vec<u8> {
    let mut s = seed | 1;
    let mut rnd = move || {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        s
    };
    let mut img = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (x as f64 / w as f64, y as f64 / h as f64);
            let mut p = [
                128.0 + 100.0 * (fx * 6.0).sin(),
                255.0 * fy,
                128.0 + 90.0 * ((fx + fy) * 5.0).cos(),
            ];
            let (dx, dy) = (fx - 0.35, fy - 0.5);
            if dx * dx + dy * dy < 0.04 {
                p = [230.0, 40.0, 50.0];
            }
            if (x / 3) % 7 == 0 && fy > 0.7 {
                p = [20.0, 60.0, 200.0];
            }
            for v in &mut p {
                if noise > 0 {
                    *v += f64::from(rnd() % (2 * noise + 1)) - f64::from(noise);
                }
                img.push(v.round().clamp(0.0, 255.0) as u8);
            }
        }
    }
    img
}
