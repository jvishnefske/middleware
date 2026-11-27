//! Floating point (feature `float`).
//!
//! `f32` arithmetic is done in `f64` and rounded once: for `+ - * /` and
//! square root that gives the correctly rounded `f32` result (53 bits is
//! more than twice 24 plus two), and the rounding functions are exact. One
//! set of routines serves both widths, which is what keeps this small on
//! parts with soft floats. Sign operations work on the bits, as the
//! specification requires for NaNs.

use crate::Error;

fn get(v: u64, single: bool) -> f64 {
    if single {
        f32::from_bits(v as u32) as f64
    } else {
        f64::from_bits(v)
    }
}

fn put(x: f64, single: bool) -> u64 {
    if single {
        (x as f32).to_bits() as u64
    } else {
        x.to_bits()
    }
}

fn sign(single: bool) -> u64 {
    if single { 1 << 31 } else { 1 << 63 }
}

/// Comparisons (`0x5B..=0x66`) and two-operand arithmetic.
pub(crate) fn binary(op: u8, a: u64, b: u64) -> u64 {
    let single = op < 0x61 || (0x92..=0x98).contains(&op);
    let (x, y) = (get(a, single), get(b, single));
    if op <= 0x66 {
        return (match (op - 0x5B) % 6 {
            0 => x == y,
            1 => x != y,
            2 => x < y,
            3 => x > y,
            4 => x <= y,
            _ => x >= y,
        }) as u64;
    }
    // add sub mul div min max copysign
    let k = op - if single { 0x92 } else { 0xA0 };
    let z = match k {
        0 => x + y,
        1 => x - y,
        2 => x * y,
        3 => x / y,
        6 => {
            let s = sign(single);
            return (a & (s - 1)) | (b & s);
        }
        _ if x.is_nan() || y.is_nan() => x + y,
        // Equal operands differ at most in the sign of zero.
        _ if x == y => f64::from_bits(if k == 4 {
            x.to_bits() | y.to_bits()
        } else {
            x.to_bits() & y.to_bits()
        }),
        _ if (x < y) == (k == 4) => x,
        _ => y,
    };
    put(z, single)
}

/// One-operand arithmetic and conversions.
pub(crate) fn unary(op: u8, a: u64) -> Result<u64, Error> {
    Ok(match op {
        0x8B..=0x91 | 0x99..=0x9F => {
            let single = op < 0x99;
            let s = sign(single);
            let x = get(a, single);
            // abs neg ceil floor trunc nearest sqrt
            let z = match op - if single { 0x8B } else { 0x99 } {
                0 => return Ok(a & (s - 1)),
                1 => return Ok((a ^ s) & (s | (s - 1))),
                2 => libm::ceil(x),
                3 => libm::floor(x),
                4 => libm::trunc(x),
                5 => libm::rint(x),
                _ => libm::sqrt(x),
            };
            // A NaN comes back quiet, as from arithmetic.
            put(if z.is_nan() { z + z } else { z }, single)
        }
        0xA8..=0xAB => trunc(op - 0xA8, a, false)?,
        0xAE..=0xB1 => trunc(op - 0xAE + 4, a, false)?,
        0xB2 => put(a as u32 as i32 as f64, true),
        0xB3 => put(a as u32 as f64, true),
        // 64-bit integers do not fit an f64, so these round directly.
        0xB4 => (a as i64 as f32).to_bits() as u64,
        0xB5 => (a as f32).to_bits() as u64,
        0xB6 => put(f64::from_bits(a), true),
        0xB7 => (a as u32 as i32 as f64).to_bits(),
        0xB8 => (a as u32 as f64).to_bits(),
        0xB9 => (a as i64 as f64).to_bits(),
        0xBA => (a as f64).to_bits(),
        _ => get(a, true).to_bits(),
    })
}

/// Float to integer. `k` bit 0: unsigned; bit 1: from `f64`; bit 2: to
/// `i64`. `sat` saturates instead of trapping.
pub(crate) fn trunc(k: u8, a: u64, sat: bool) -> Result<u64, Error> {
    let x = get(a, k & 2 == 0);
    let (lo, hi) = match k & 5 {
        0 => (-2147483649.0, 2147483648.0),
        4 => (-9223372036854777856.0, 9223372036854775808.0),
        1 => (-1.0, 4294967296.0),
        _ => (-1.0, 18446744073709551616.0),
    };
    if !sat {
        if x.is_nan() {
            return Err(Error::InvalidConversion);
        }
        if !(x > lo && x < hi) {
            return Err(Error::IntegerOverflow);
        }
    }
    // `as` saturates and maps NaN to zero.
    Ok(match k & 5 {
        0 => x as i32 as u32 as u64,
        4 => x as i64 as u64,
        1 => x as u32 as u64,
        _ => x as u64,
    })
}
