//! PWM timer planning for the ESP32's LEDC (ABI 3 `pwm_open` / `pwm_set`).
//!
//! An LEDC timer divides its source clock by a fractional divider (8
//! fractional bits, at least 1.0) and counts `2^bits` steps per period, so
//! a higher frequency leaves fewer duty bits: at the 80 MHz APB clock a
//! 20 MHz camera clock (R15) has 2 bits, 40 MHz has 1. [`plan`] picks the
//! most duty bits (at most [`MAX_DUTY_BITS`]) a frequency allows;
//! [`duty_counts`] maps the ABI's 16-bit duty (65535 = always on) onto
//! them, rounding to the nearest step. Both are pure so the shell only
//! applies the result (esp-hal's `Timer::configure` checks the divider
//! again and falls back to the 1 MHz REF_TICK for slow frequencies).

use middleware::wasm_abi::AbiError;

/// The LEDC timers' source clock on the ESP32 (APB).
pub const APB_HZ: u32 = 80_000_000;

/// Most duty bits a plan uses: the ABI's duty is 16-bit, finer steps would
/// add nothing.
pub const MAX_DUTY_BITS: u8 = 16;

/// How an LEDC timer runs one frequency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    /// Requested frequency in Hz.
    pub freq_hz: u32,
    /// Duty resolution in bits, `1..=MAX_DUTY_BITS`.
    pub bits: u8,
}

/// The plan for `freq_hz` from a `src_hz` clock: the most duty bits with a
/// divider of at least 1 (`src_hz / freq_hz >= 2^bits`). `Unsupported`
/// when not even one bit fits (frequency above half the clock) or the
/// frequency is 0.
pub fn plan(src_hz: u32, freq_hz: u32) -> Result<Plan, AbiError> {
    if freq_hz == 0 {
        return Err(AbiError::Unsupported);
    }
    let ratio = src_hz / freq_hz;
    // floor(log2(ratio)), bounded by MAX_DUTY_BITS.
    let bits = (u32::BITS - ratio.leading_zeros()).saturating_sub(1).min(u32::from(MAX_DUTY_BITS));
    if bits == 0 {
        return Err(AbiError::Unsupported);
    }
    Ok(Plan { freq_hz, bits: bits as u8 })
}

/// The LEDC duty count for the ABI `duty` (a fraction of 65535) at `bits`
/// of resolution: `duty * 2^bits / 65535`, rounded to nearest; 65535 is
/// `2^bits` (always on) and 0 is 0 (always off).
pub fn duty_counts(duty: u16, bits: u8) -> u32 {
    let steps = 1u64 << bits.min(MAX_DUTY_BITS);
    let num = u64::from(duty) * steps + u64::from(u16::MAX / 2);
    // At most 65535 * 65536 + 32767 / 65535 ≤ 65536: fits u32.
    u32::try_from(num / u64::from(u16::MAX)).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r15_camera_clocks_get_the_bits_the_divider_allows() {
        assert_eq!(plan(APB_HZ, 20_000_000), Ok(Plan { freq_hz: 20_000_000, bits: 2 }));
        assert_eq!(plan(APB_HZ, 10_000_000), Ok(Plan { freq_hz: 10_000_000, bits: 3 }));
        assert_eq!(plan(APB_HZ, 40_000_000), Ok(Plan { freq_hz: 40_000_000, bits: 1 }));
        assert_eq!(plan(APB_HZ, 24_000_000).map(|p| p.bits), Ok(1), "80/24 = 3.3");
        assert_eq!(plan(APB_HZ, 40_000_001), Err(AbiError::Unsupported));
        assert_eq!(plan(APB_HZ, 0), Err(AbiError::Unsupported));
        // Slow PWM is capped at the ABI's 16 bits.
        assert_eq!(plan(APB_HZ, 1_000).map(|p| p.bits), Ok(16));
        assert_eq!(plan(APB_HZ, 1).map(|p| p.bits), Ok(16));
        assert_eq!(plan(APB_HZ, 1_220).map(|p| p.bits), Ok(16));
        assert_eq!(plan(APB_HZ, 2_500_000).map(|p| p.bits), Ok(5));
    }

    #[test]
    fn duty_rounds_to_the_nearest_step_with_exact_ends() {
        assert_eq!(duty_counts(0, 2), 0);
        assert_eq!(duty_counts(u16::MAX, 2), 4, "always on");
        assert_eq!(duty_counts(32_768, 2), 2, "50 % XCLK");
        assert_eq!(duty_counts(32_767, 1), 1);
        assert_eq!(duty_counts(16_000, 1), 0);
        assert_eq!(duty_counts(u16::MAX, 16), 65_536);
        assert_eq!(duty_counts(1, 16), 1);
        assert_eq!(duty_counts(u16::MAX, 40), 65_536, "bits capped");
        // Monotonic in duty at every resolution.
        for bits in 1..=MAX_DUTY_BITS {
            let mut last = 0;
            for d in (0..=u16::MAX).step_by(97).chain([u16::MAX]) {
                let c = duty_counts(d, bits);
                assert!(c >= last && c <= 1 << bits, "bits {bits} duty {d}");
                last = c;
            }
        }
    }
}
