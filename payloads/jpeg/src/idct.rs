//! Integer inverse DCTs: 8×8, and the reduced 4×4 for half-scale output.
//!
//! The algorithm is libjpeg's `jidctint.c` ("ISLOW", Loeffler–Ligtenberg–
//! Moschytz with 12 multiplies per 1-D pass) in 32-bit fixed point:
//! `CONST_BITS = 13` for the constants and `PASS1_BITS = 2` of extra
//! precision between the column and row passes. For valid 8-bit data its
//! output equals libjpeg's default IDCT; against an exact (floating point)
//! IDCT the error is at most one level for nearly every sample.
//!
//! The input is the coefficient block *transposed* (`coef_t[u][v]`, column
//! `u` of the block as row `u`), which the entropy decoder produces for free
//! by de-zigzagging into transposed positions, so the column pass reads
//! rows. Columns whose AC terms are all zero take libjpeg's shortcut and
//! the upper four columns are skipped when the entropy decoder saw no
//! coefficient there; the rows are filled without a transform when no
//! coefficient has a horizontal frequency; a block with only a DC term
//! skips the IDCT altogether ([`dc_only`]).
//!
//! [`idct_4x4`] is libjpeg's `jpeg_idct_4x4` (`jidctred.c`): 4×4 output
//! samples straight from the coefficients, equal to a 2×2 box average of
//! the 8×8 result for frequencies below 4.
//!
//! wasmi charges fuel per loop iteration for the code inside the loop, so
//! the 1-D transforms that a shortcut skips live in out-of-line functions
//! ([`column()`], [`rows`]) rather than inline in the loop body.
//!
//! Arithmetic uses `wrapping_*` operations: valid coefficients never
//! overflow, and the garbage coefficients of a corrupt stream produce
//! garbage pixels instead of a panic. The final value is clamped to
//! 0..=255.

/// Fixed-point precision of the multiplier constants.
const CONST_BITS: u32 = 13;
/// Extra precision kept between the passes.
const PASS1_BITS: u32 = 2;

/// `FIX(x) = round(x · 2^13)` for the constants of `jidctint.c`.
const FIX_0_298631336: i32 = 2446;
/// See [`FIX_0_298631336`].
const FIX_0_390180644: i32 = 3196;
/// See [`FIX_0_298631336`].
const FIX_0_541196100: i32 = 4433;
/// See [`FIX_0_298631336`].
const FIX_0_765366865: i32 = 6270;
/// See [`FIX_0_298631336`].
const FIX_0_899976223: i32 = 7373;
/// See [`FIX_0_298631336`].
const FIX_1_175875602: i32 = 9633;
/// See [`FIX_0_298631336`].
const FIX_1_501321110: i32 = 12299;
/// See [`FIX_0_298631336`].
const FIX_1_847759065: i32 = 15137;
/// See [`FIX_0_298631336`].
const FIX_1_961570560: i32 = 16069;
/// See [`FIX_0_298631336`].
const FIX_2_053119869: i32 = 16819;
/// See [`FIX_0_298631336`].
const FIX_2_562915447: i32 = 20995;
/// See [`FIX_0_298631336`].
const FIX_3_072711336: i32 = 25172;

/// An 8×8 block as rows.
pub(crate) type Block = [[i32; 8]; 8];

/// Wrapping `a + b`.
#[inline(always)]
fn add(a: i32, b: i32) -> i32 {
    a.wrapping_add(b)
}

/// Wrapping `a - b`.
#[inline(always)]
fn sub(a: i32, b: i32) -> i32 {
    a.wrapping_sub(b)
}

/// Wrapping `a · b`.
#[inline(always)]
fn mul(a: i32, b: i32) -> i32 {
    a.wrapping_mul(b)
}

/// One 1-D IDCT of `v` (natural order) with each output rounded and
/// shifted right by `shift` (libjpeg's `DESCALE`).
#[inline(always)]
fn idct_1d(v: [i32; 8], shift: u32) -> [i32; 8] {
    let [v0, v1, v2, v3, v4, v5, v6, v7] = v;
    // Even part.
    let z1 = mul(add(v2, v6), FIX_0_541196100);
    let tmp2 = sub(z1, mul(v6, FIX_1_847759065));
    let tmp3 = add(z1, mul(v2, FIX_0_765366865));
    let tmp0 = add(v0, v4).wrapping_shl(CONST_BITS);
    let tmp1 = sub(v0, v4).wrapping_shl(CONST_BITS);
    // The rounding constant is folded into the even part once.
    let round = 1i32 << (shift - 1);
    let tmp0 = add(tmp0, round);
    let tmp1 = add(tmp1, round);
    let tmp10 = add(tmp0, tmp3);
    let tmp13 = sub(tmp0, tmp3);
    let tmp11 = add(tmp1, tmp2);
    let tmp12 = sub(tmp1, tmp2);
    // Odd part.
    let z1 = add(v7, v1);
    let z2 = add(v5, v3);
    let z3 = add(v7, v3);
    let z4 = add(v5, v1);
    let z5 = mul(add(z3, z4), FIX_1_175875602);
    let t0 = mul(v7, FIX_0_298631336);
    let t1 = mul(v5, FIX_2_053119869);
    let t2 = mul(v3, FIX_3_072711336);
    let t3 = mul(v1, FIX_1_501321110);
    let z1 = mul(z1, -FIX_0_899976223);
    let z2 = mul(z2, -FIX_2_562915447);
    let z3 = sub(z5, mul(z3, FIX_1_961570560));
    let z4 = sub(z5, mul(z4, FIX_0_390180644));
    let t0 = add(t0, add(z1, z3));
    let t1 = add(t1, add(z2, z4));
    let t2 = add(t2, add(z2, z3));
    let t3 = add(t3, add(z1, z4));
    [
        add(tmp10, t3) >> shift,
        add(tmp11, t2) >> shift,
        add(tmp12, t1) >> shift,
        add(tmp13, t0) >> shift,
        sub(tmp13, t0) >> shift,
        sub(tmp12, t1) >> shift,
        sub(tmp11, t2) >> shift,
        sub(tmp10, t3) >> shift,
    ]
}

/// Whether the AC terms of a row are all zero.
#[inline(always)]
fn ac_zero(v: &[i32; 8]) -> bool {
    let [_, a, b, c, d, e, f, g] = *v;
    (a | b | c | d | e | f | g) == 0
}

/// Level-shifts and clamps a sample to 0..=255.
#[inline(always)]
fn to_sample(x: i32) -> u8 {
    let x = x.wrapping_add(128);
    if x < 0 {
        0
    } else if x > 255 {
        255
    } else {
        x as u8
    }
}

/// The sample value of a block whose only non-zero coefficient is the
/// (dequantised) DC term: what [`idct_8x8`] would produce everywhere.
#[inline(always)]
pub(crate) fn dc_only(dc: i32) -> u8 {
    let pass1 = dc.wrapping_shl(PASS1_BITS);
    to_sample(add(pass1, 1 << (PASS1_BITS + 2)) >> (PASS1_BITS + 3))
}

/// Writes `v` into column `c` (masked to 0..8) of `ws`.
#[inline(always)]
fn set_col(ws: &mut Block, c: usize, v: [i32; 8]) {
    let c = c & 7;
    let [r0, r1, r2, r3, r4, r5, r6, r7] = ws;
    let [a, b, d, e, f, g, h, i] = v;
    macro_rules! put {
        ($row:ident, $x:ident) => {
            if let Some(dst) = $row.get_mut(c) {
                *dst = $x;
            }
        };
    }
    put!(r0, a);
    put!(r1, b);
    put!(r2, d);
    put!(r3, e);
    put!(r4, f);
    put!(r5, g);
    put!(r6, h);
    put!(r7, i);
}

/// Column pass for one column `c` with AC terms: `col` is the column
/// (a row of the transposed input); the result goes to column `c` of `ws`,
/// scaled by 2^PASS1_BITS. Out of line (see the module docs).
#[inline(never)]
fn column(col: &[i32; 8], ws: &mut Block, c: usize) {
    set_col(ws, c, idct_1d(*col, CONST_BITS - PASS1_BITS));
}

/// Row pass over all eight rows of `ws`, level-shifted and clamped into
/// `out`. Out of line, with the transform in the loop body: rows with AC
/// terms are the rule once any coefficient has a horizontal frequency
/// (otherwise [`idct_8x8`] fills constant rows itself).
#[inline(never)]
fn rows(ws: &Block, out: &mut [[u8; 8]; 8]) {
    let shift = CONST_BITS + PASS1_BITS + 3;
    for (ws_row, dst) in ws.iter().zip(out.iter_mut()) {
        let [a, b, c, d, e, f, g, h] = idct_1d(*ws_row, shift);
        *dst = [
            to_sample(a),
            to_sample(b),
            to_sample(c),
            to_sample(d),
            to_sample(e),
            to_sample(f),
            to_sample(g),
            to_sample(h),
        ];
    }
}

/// Which parts of a block hold non-zero coefficients, as found by the
/// entropy decoder: lets [`idct_8x8`] skip or shorten passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Extent {
    /// Some coefficient has horizontal frequency `u >= 4`.
    pub(crate) hi_u: bool,
    /// Some coefficient has a horizontal frequency `u >= 1` (otherwise
    /// every row of the result is constant).
    pub(crate) any_u: bool,
}

/// Inverse-transforms a transposed block of dequantised coefficients
/// (`coef_t[u][v]`, see the module docs) into samples, row-major
/// (`out[y][x]`). `extent` must cover every non-zero coefficient.
///
/// The column pass writes its results transposed, so the row pass reads
/// rows. The column pass loops over 8 columns (4 when `!extent.hi_u`) with
/// a cheap all-AC-zero shortcut inline and the 1-D transform out of line
/// ([`column()`]): wasmi charges a loop iteration for the code inside it, so
/// the shortcut must not pay for the transform it skips. The row pass
/// ([`rows`]) transforms every row unless no coefficient has a horizontal
/// frequency, in which case all rows are constant.
pub(crate) fn idct_8x8(coef_t: &Block, extent: Extent, out: &mut [[u8; 8]; 8]) {
    let mut ws: Block = [[0; 8]; 8];
    let cols = if extent.hi_u { 8 } else { 4 };
    for c in 0..cols {
        let Some(col) = coef_t.get(c) else { break };
        if ac_zero(col) {
            if col[0] != 0 {
                set_col(&mut ws, c, [col[0].wrapping_shl(PASS1_BITS); 8]);
            }
        } else {
            column(col, &mut ws, c);
        }
    }
    if !extent.any_u {
        for (ws_row, dst) in ws.iter().zip(out.iter_mut()) {
            let dc = add(ws_row[0], 1 << (PASS1_BITS + 2)) >> (PASS1_BITS + 3);
            *dst = [to_sample(dc); 8];
        }
    } else {
        rows(&ws, out);
    }
}

/// `FIX(x)` constants of libjpeg's reduced 4×4 IDCT (`jidctred.c`).
const FIX_0_211164243: i32 = 1730;
/// See [`FIX_0_211164243`].
const FIX_0_509795579: i32 = 4176;
/// See [`FIX_0_211164243`].
const FIX_0_601344887: i32 = 4926;
/// See [`FIX_0_211164243`].
const FIX_1_061594337: i32 = 8697;
/// See [`FIX_0_211164243`].
const FIX_1_451774981: i32 = 11893;
/// See [`FIX_0_211164243`].
const FIX_2_172734803: i32 = 17799;

/// One 4-point output 1-D IDCT from 8 inputs (`v[4]` unused), libjpeg's
/// `jpeg_idct_4x4` pass, rounded and shifted right by `shift`.
#[inline(always)]
fn idct_1d_4(v: [i32; 8], shift: u32) -> [i32; 4] {
    let [v0, v1, v2, v3, _, v5, v6, v7] = v;
    // Even part.
    let tmp0 = add(v0.wrapping_shl(CONST_BITS + 1), 1i32 << (shift - 1));
    let tmp2 = sub(mul(v2, FIX_1_847759065), mul(v6, FIX_0_765366865));
    let tmp10 = add(tmp0, tmp2);
    let tmp12 = sub(tmp0, tmp2);
    // Odd part.
    let t0 = add(
        add(mul(v7, -FIX_0_211164243), mul(v5, FIX_1_451774981)),
        add(mul(v3, -FIX_2_172734803), mul(v1, FIX_1_061594337)),
    );
    let t2 = add(
        add(mul(v7, -FIX_0_509795579), mul(v5, -FIX_0_601344887)),
        add(mul(v3, FIX_0_899976223), mul(v1, FIX_2_562915447)),
    );
    [
        add(tmp10, t2) >> shift,
        add(tmp12, t0) >> shift,
        sub(tmp12, t0) >> shift,
        sub(tmp10, t2) >> shift,
    ]
}

/// Whether the AC terms a 4-point transform reads (`v[1..8]` but `v[4]`)
/// are all zero.
#[inline(always)]
fn ac_zero_4(v: &[i32; 8]) -> bool {
    let [_, a, b, c, _, e, f, g] = *v;
    (a | b | c | e | f | g) == 0
}

/// Reduced IDCT for half-scale output (libjpeg's `jpeg_idct_4x4`): the
/// 4×4 samples of a block downscaled by 2, computed in the DCT domain from
/// the coefficients (those with frequency 4 do not contribute), into
/// `out[0..4][0..4]`. Roughly a quarter of the work of [`idct_8x8`].
/// `coef_t` is transposed as for [`idct_8x8`].
#[inline(never)]
pub(crate) fn idct_4x4(coef_t: &Block, out: &mut [[u8; 8]; 8]) {
    // Pass 1: columns (rows of `coef_t`), skipping column 4, into
    // `ws[y][u]` for the 4 output rows.
    let mut ws: Block = [[0; 8]; 8];
    let shift1 = CONST_BITS - PASS1_BITS + 1;
    for c in 0..8 {
        if c == 4 {
            continue;
        }
        let Some(col) = coef_t.get(c) else { break };
        let v = if ac_zero_4(col) {
            [col[0].wrapping_shl(PASS1_BITS); 4]
        } else {
            idct_1d_4(*col, shift1)
        };
        let [a, b, d, e] = v;
        let [r0, r1, r2, r3, ..] = &mut ws;
        for (row, x) in [(r0, a), (r1, b), (r2, d), (r3, e)] {
            if let Some(dst) = row.get_mut(c) {
                *dst = x;
            }
        }
    }
    // Pass 2: the 4 rows.
    let shift2 = CONST_BITS + PASS1_BITS + 3 + 1;
    for (ws_row, dst) in ws.iter().zip(out.iter_mut()).take(4) {
        let [a, b, c, d] = if ac_zero_4(ws_row) {
            let dc = add(ws_row[0], 1 << (PASS1_BITS + 2)) >> (PASS1_BITS + 3);
            [dc; 4]
        } else {
            idct_1d_4(*ws_row, shift2)
        };
        let [o0, o1, o2, o3, ..] = dst;
        *o0 = to_sample(a);
        *o1 = to_sample(b);
        *o2 = to_sample(c);
        *o3 = to_sample(d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Transposes a block (constant indices only).
    fn transpose(m: Block) -> Block {
        let [a, b, c, d, e, f, g, h] = m;
        macro_rules! col {
            ($i:tt) => {
                [a[$i], b[$i], c[$i], d[$i], e[$i], f[$i], g[$i], h[$i]]
            };
        }
        [
            col!(0),
            col!(1),
            col!(2),
            col!(3),
            col!(4),
            col!(5),
            col!(6),
            col!(7),
        ]
    }

    /// `idct_8x8` returning its output.
    fn idct(coef_t: &Block) -> [[u8; 8]; 8] {
        let mut out = [[0u8; 8]; 8];
        let full = Extent {
            hi_u: true,
            any_u: true,
        };
        idct_8x8(coef_t, full, &mut out);
        let mut lo = [[0u8; 8]; 8];
        let nz = |f: &dyn Fn(usize, usize) -> bool| {
            (0..8).any(|u| (0..8).any(|v| f(u, v) && coef_t[u][v] != 0))
        };
        let extent = Extent {
            hi_u: nz(&|u, _| u >= 4),
            any_u: nz(&|u, _| u >= 1),
        };
        idct_8x8(coef_t, extent, &mut lo);
        assert_eq!(out, lo, "skipping zero columns and rows must be exact");
        out
    }

    /// Float reference IDCT of a natural-order block (rows).
    fn reference(coef: &Block) -> [[f64; 8]; 8] {
        let c = |u: usize| if u == 0 { 1.0 / 2f64.sqrt() } else { 1.0 };
        let mut out = [[0.0; 8]; 8];
        for (y, row) in out.iter_mut().enumerate() {
            for (x, o) in row.iter_mut().enumerate() {
                let mut s = 0.0;
                for (v, crow) in coef.iter().enumerate() {
                    for (u, &f) in crow.iter().enumerate() {
                        s += c(u)
                            * c(v)
                            * f64::from(f)
                            * (((2 * x + 1) as f64 * u as f64 * core::f64::consts::PI) / 16.0)
                                .cos()
                            * (((2 * y + 1) as f64 * v as f64 * core::f64::consts::PI) / 16.0)
                                .cos();
                    }
                }
                *o = (s / 4.0 + 128.0).clamp(0.0, 255.0);
            }
        }
        out
    }

    #[test]
    fn dc_only_block_is_flat() {
        let mut coef = [[0i32; 8]; 8];
        coef[0][0] = 80; // DC 80 → +10 levels
        assert_eq!(idct(&coef), [[138u8; 8]; 8]);
        assert_eq!(dc_only(80), 138);
        assert_eq!(dc_only(-4000), 0);
        assert_eq!(dc_only(4000), 255);
    }

    #[test]
    fn clamps_extremes() {
        let mut coef = [[0i32; 8]; 8];
        coef[0][0] = 4000;
        assert_eq!(idct(&coef), [[255u8; 8]; 8]);
        coef[0][0] = -4000;
        assert_eq!(idct(&coef), [[0u8; 8]; 8]);
    }

    #[test]
    fn reduced_idct_is_the_2x2_average_of_the_full_idct() {
        // libjpeg's 4×4 transform equals the 8×8 IDCT followed by a 2×2
        // box average for frequencies below 4 (higher ones are folded in
        // approximately): checked against the unclamped float IDCT within
        // one level.
        let mut seed = 0x9E37_79B9u32;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        let c = |u: usize| if u == 0 { 1.0 / 2f64.sqrt() } else { 1.0 };
        let pi = core::f64::consts::PI;
        let f = |coef: &Block, x: usize, y: usize| {
            let mut s = 0.0;
            for (v, row) in coef.iter().enumerate() {
                for (u, &k) in row.iter().enumerate() {
                    s += c(u)
                        * c(v)
                        * f64::from(k)
                        * (((2 * x + 1) * u) as f64 * pi / 16.0).cos()
                        * (((2 * y + 1) * v) as f64 * pi / 16.0).cos();
                }
            }
            s / 4.0 + 128.0
        };
        for _ in 0..500 {
            let mut coef = [[0i32; 8]; 8];
            for (v, row) in coef.iter_mut().enumerate().take(4) {
                for (u, k) in row.iter_mut().enumerate().take(4) {
                    let range = 400 / (1 + u + v) as u32;
                    *k = (next() % (2 * range + 1)) as i32 - range as i32;
                }
            }
            let mut half = [[0u8; 8]; 8];
            idct_4x4(&transpose(coef), &mut half);
            for (y, half_row) in half.iter().enumerate().take(4) {
                for (x, &got) in half_row.iter().enumerate().take(4) {
                    let avg = (f(&coef, 2 * x, 2 * y)
                        + f(&coef, 2 * x + 1, 2 * y)
                        + f(&coef, 2 * x, 2 * y + 1)
                        + f(&coef, 2 * x + 1, 2 * y + 1))
                        / 4.0;
                    let want = avg.clamp(0.0, 255.0);
                    let got = f64::from(got);
                    assert!((got - want).abs() <= 1.0, "{got} vs {want}");
                }
            }
        }
    }

    #[test]
    fn huge_coefficients_do_not_panic() {
        let mut out = [[0u8; 8]; 8];
        idct_4x4(&[[i32::MAX; 8]; 8], &mut out);
        idct_4x4(&[[i32::MIN; 8]; 8], &mut out);
        let _ = idct(&[[i32::MAX; 8]; 8]);
        let _ = idct(&[[i32::MIN; 8]; 8]);
    }

    #[test]
    fn matches_float_idct_within_one_level() {
        // Deterministic pseudo-random blocks with typical magnitudes.
        let mut seed = 0x1234_5678u32;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for _ in 0..500 {
            let mut coef = [[0i32; 8]; 8];
            // Half the blocks only have low frequencies, as camera blocks
            // mostly do, to exercise the skipped columns and rows.
            let band = if next() % 2 == 0 { 4 } else { 8 };
            for (v, row) in coef.iter_mut().enumerate() {
                for (u, f) in row.iter_mut().enumerate() {
                    let range = 600 / (1 + u + v) as u32;
                    if u < band && v < band.max(next() as usize % 9) {
                        *f = (next() % (2 * range + 1)) as i32 - range as i32;
                    }
                }
            }
            let got = idct(&transpose(coef));
            let want = reference(&coef);
            for (g, w) in got.iter().flatten().zip(want.iter().flatten()) {
                assert!((f64::from(*g) - w).abs() <= 1.0, "{g} vs {w}");
            }
        }
    }
}
