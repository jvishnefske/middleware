//! The I2C bus plan behind ABI 3 `i2c_open` / `i2c_transfer` (E24): which
//! clocks bus 0 accepts and how long one byte may take before a transfer
//! gives up.
//!
//! The shell's `Bus0` (`src/shell/i2c.rs`) drives the esp-hal I2C0 master
//! in blocking mode on the main task, so a stuck bus (a target holding SCL
//! or SDA low, a missing pull-up) must not hang the firmware. esp-hal's
//! own defaults do not bound that on every chip: on the ESP32 the hardware
//! SCL timeout is at its maximum (about 13 ms of an unchanging SCL at the
//! 80 MHz APB clock), but on the ESP32-S3 the hardware bus timeout is
//! disabled and the state-machine timeouts default to 2^23 bus cycles
//! (about 84 s at 100 kHz). The shell therefore sets esp-hal's per-byte
//! software timeout to [`byte_timeout_us`] on every `i2c_open`.
//!
//! A byte on the wire is nine SCL periods (eight data bits and the
//! acknowledge). The budget allows twice that, so a target may stretch the
//! clock by up to one byte time, plus [`BYTE_SLACK_US`] for the polling loop
//! and interrupts on the core. esp-hal applies the budget per FIFO chunk
//! (at most 31 bytes, plus the address byte), so one transfer of `tx`
//! written and `rx` read bytes blocks for at most
//! `(tx + rx + 6) * byte_timeout_us(freq)` (three chunks each way for
//! [`MAX_I2C`](middleware::wasm_abi::periph::MAX_I2C) bytes) and, after a timeout, esp-hal's bus clear (at most
//! 50 ms). With `MAX_I2C` = 64 both ways that is about 37 ms at 100 kHz,
//! 19 ms at 400 kHz and 255 ms at the ABI's slowest 10 kHz, before the
//! clear.

use middleware::wasm_abi::AbiError;

/// Fastest bus clock bus 0 accepts, in Hz (fast mode). The ABI allows up
/// to 1 MHz (Fast-mode Plus); esp-hal refuses more than 400 kHz on the
/// ESP32, and the firmware keeps the same cap on the ESP32-S3 so a payload
/// behaves alike on both.
pub const MAX_HZ: u32 = 400_000;

/// Fixed part of the per-byte budget, in microseconds.
pub const BYTE_SLACK_US: u32 = 100;

/// SCL periods per byte on the wire: eight data bits and the acknowledge.
const CLOCKS_PER_BYTE: u32 = 9;

/// How long one byte of a transfer at `freq_hz` may take, in microseconds:
/// two byte times plus [`BYTE_SLACK_US`] (see the module docs). A clock of
/// 0 is treated as 1 Hz, so the result is always finite.
pub const fn byte_timeout_us(freq_hz: u32) -> u32 {
    let hz = if freq_hz == 0 { 1 } else { freq_hz };
    let two_bytes = 2 * CLOCKS_PER_BYTE as u64 * 1_000_000 / hz as u64;
    let us = two_bytes + BYTE_SLACK_US as u64;
    if us > u32::MAX as u64 {
        u32::MAX
    } else {
        us as u32
    }
}

/// Checks an `i2c_open` against what bus 0 supports and returns the
/// per-byte timeout to configure: `Unsupported` for a bus other than 0 or
/// a clock above [`MAX_HZ`]. (The ABI layer has already refused buses and
/// clocks outside the ABI's ranges as `InvalidArgument`.)
pub const fn open_plan(bus: u8, freq_hz: u32) -> Result<u32, AbiError> {
    if bus != 0 || freq_hz > MAX_HZ {
        Err(AbiError::Unsupported)
    } else {
        Ok(byte_timeout_us(freq_hz))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::wasm_abi::periph::{I2C_FREQ_HZ, MAX_I2C};

    #[test]
    fn e24_byte_timeout_is_two_byte_times_plus_slack() {
        assert_eq!(byte_timeout_us(100_000), 180 + BYTE_SLACK_US);
        assert_eq!(byte_timeout_us(400_000), 45 + BYTE_SLACK_US);
        assert_eq!(byte_timeout_us(10_000), 1_800 + BYTE_SLACK_US);
        // Never zero, never overflowing.
        assert_eq!(byte_timeout_us(0), 18_000_000 + BYTE_SLACK_US);
        assert!(byte_timeout_us(u32::MAX) >= BYTE_SLACK_US);
    }

    #[test]
    fn e24_a_full_transfer_is_bounded() {
        // (tx + rx + 6 chunk address bytes) budgets, the worst case the
        // module docs state, stays under 300 ms at the ABI's slowest clock
        // and under 40 ms at standard mode.
        let bytes = (2 * MAX_I2C + 6) as u64;
        let slowest = *I2C_FREQ_HZ.start() as u32;
        assert!(bytes * u64::from(byte_timeout_us(slowest)) < 300_000);
        assert!(bytes * u64::from(byte_timeout_us(100_000)) < 40_000);
        assert!(bytes * u64::from(byte_timeout_us(MAX_HZ)) < 20_000);
    }

    #[test]
    fn e24_open_plan_accepts_bus_0_up_to_fast_mode() {
        assert_eq!(open_plan(0, 100_000), Ok(byte_timeout_us(100_000)));
        assert_eq!(open_plan(0, MAX_HZ), Ok(byte_timeout_us(MAX_HZ)));
        assert_eq!(
            open_plan(0, MAX_HZ + 1),
            Err(AbiError::Unsupported),
            "Fast-mode Plus"
        );
        assert_eq!(open_plan(0, 1_000_000), Err(AbiError::Unsupported));
        assert_eq!(open_plan(1, 100_000), Err(AbiError::Unsupported), "bus 1");
    }
}
