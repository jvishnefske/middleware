//! Analog input through the `adc_read` import (ABI 3).
//!
//! Readings are raw 12-bit values, `0..=ADC_MAX`, whatever the board's
//! converter resolution (the firmware scales to this range). A pin without
//! an analog input fails with [`AbiError::Unsupported`]. There is no
//! `embedded-hal` 1.0 ADC trait, so [`AnalogPin`] is the SDK's own type;
//! [`to_millivolts`] converts a reading with integer math only.

use crate::{AbiError, check, sys};
pub use middleware::wasm_abi::periph::ADC_MAX;

/// Reads the analog input on board pin `pin`.
pub fn read(pin: u8) -> Result<u16, AbiError> {
    let raw = check(sys::adc_read(i32::from(pin)))?;
    // The host clamps to ADC_MAX; a larger value is a host bug, clamp again.
    Ok(u16::try_from(raw).unwrap_or(ADC_MAX).min(ADC_MAX))
}

/// Converts a raw reading to millivolts for a reference of `vref_mv`
/// (e.g. 3300), rounding down. Readings above [`ADC_MAX`] count as full
/// scale.
pub const fn to_millivolts(raw: u16, vref_mv: u16) -> u32 {
    let raw = if raw > ADC_MAX { ADC_MAX } else { raw };
    raw as u32 * vref_mv as u32 / ADC_MAX as u32
}

/// An analog input pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnalogPin {
    pin: u8,
}

impl AnalogPin {
    /// Names board pin `pin` as an analog input. Nothing is configured until
    /// the first [`AnalogPin::read`]; an unsuitable pin fails there.
    pub const fn new(pin: u8) -> Self {
        AnalogPin { pin }
    }

    /// The pin number.
    pub const fn pin(&self) -> u8 {
        self.pin
    }

    /// One raw reading, `0..=ADC_MAX`.
    pub fn read(&mut self) -> Result<u16, AbiError> {
        read(self.pin)
    }

    /// One reading in millivolts for a reference of `vref_mv`.
    pub fn read_millivolts(&mut self, vref_mv: u16) -> Result<u32, AbiError> {
        self.read().map(|raw| to_millivolts(raw, vref_mv))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn millivolts_use_integer_full_scale() {
        assert_eq!(to_millivolts(0, 3300), 0);
        assert_eq!(to_millivolts(ADC_MAX, 3300), 3300);
        assert_eq!(to_millivolts(2048, 3300), 1650);
        assert_eq!(to_millivolts(u16::MAX, 3300), 3300, "clamped");
        assert_eq!(to_millivolts(ADC_MAX, u16::MAX), 65_535, "no overflow");
    }

    #[test]
    fn reads_reach_the_host() {
        let mut a = AnalogPin::new(3);
        assert_eq!(a.pin(), 3);
        assert_eq!(a.read(), Err(AbiError::Unsupported));
        assert_eq!(a.read_millivolts(3300), Err(AbiError::Unsupported));
        assert_eq!(read(0), Err(AbiError::Unsupported));
    }
}
