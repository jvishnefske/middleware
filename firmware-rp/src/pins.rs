//! The payload's GPIO modes: which pins it configured, and how.
//!
//! [`Pins`] is a value: each operation checks the pin against the board's
//! policy ([`Profile::check_pin`]) and returns the next table, so the
//! device shell applies a change to the hardware only once it is allowed.
//! The rules are esp-base's pin bank's:
//!
//! * `gpio_output` makes a pin an output driven low, `gpio_input` an input
//!   with the requested pull; either may be called again to switch.
//! * `gpio_write` needs an output (`InvalidArgument` otherwise).
//! * `gpio_read` of an input reads the pad; of an output, the level it
//!   drives; of a pin never configured, `InvalidArgument`.
//!
//! When the payload stops, the shell returns every pin in
//! [`Pins::configured`] to an input without pull, the state it had at
//! boot.

use middleware::wasm_abi::AbiError;

use crate::chip::{Direction, Profile};

/// Number of GPIOs on both chips.
pub const GPIOS: usize = 30;

/// What a pin is configured as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Not configured by the payload.
    Idle,
    /// An input.
    Input,
    /// A push-pull output.
    Output,
}

/// Where a `gpio_read` takes its level from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadFrom {
    /// The pad (an input).
    Pad,
    /// The output latch (an output).
    Latch,
}

/// The modes of all pins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pins {
    modes: [Mode; GPIOS],
}

impl Default for Pins {
    fn default() -> Self {
        Self::new()
    }
}

impl Pins {
    /// No pin configured.
    pub const fn new() -> Self {
        Pins { modes: [Mode::Idle; GPIOS] }
    }

    /// The mode of `pin` (`Idle` for a pin the chip lacks).
    pub fn mode(&self, pin: u8) -> Mode {
        self.modes.get(usize::from(pin)).copied().unwrap_or(Mode::Idle)
    }

    fn with(self, profile: &Profile, pin: u8, mode: Mode) -> Result<Self, AbiError> {
        let dir = if mode == Mode::Output { Direction::Output } else { Direction::Input };
        profile.check_pin(pin, dir)?;
        let mut modes = self.modes;
        let slot = modes.get_mut(usize::from(pin)).ok_or(AbiError::InvalidArgument)?;
        *slot = mode;
        Ok(Pins { modes })
    }

    /// `pin` becomes an output.
    pub fn output(self, profile: &Profile, pin: u8) -> Result<Self, AbiError> {
        self.with(profile, pin, Mode::Output)
    }

    /// `pin` becomes an input.
    pub fn input(self, profile: &Profile, pin: u8) -> Result<Self, AbiError> {
        self.with(profile, pin, Mode::Input)
    }

    /// Whether `pin` may be written.
    pub fn check_write(&self, profile: &Profile, pin: u8) -> Result<(), AbiError> {
        profile.check_pin(pin, Direction::Output)?;
        match self.mode(pin) {
            Mode::Output => Ok(()),
            Mode::Idle | Mode::Input => Err(AbiError::InvalidArgument),
        }
    }

    /// Where a read of `pin` comes from.
    pub fn read_from(&self, profile: &Profile, pin: u8) -> Result<ReadFrom, AbiError> {
        profile.check_pin(pin, Direction::Input)?;
        match self.mode(pin) {
            Mode::Input => Ok(ReadFrom::Pad),
            Mode::Output => Ok(ReadFrom::Latch),
            Mode::Idle => Err(AbiError::InvalidArgument),
        }
    }

    /// Pins the payload configured, ascending.
    pub fn configured(&self) -> impl Iterator<Item = u8> + '_ {
        (0u8..).zip(self.modes.iter()).filter(|(_, m)| **m != Mode::Idle).map(|(p, _)| p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chip::RP2350;

    #[test]
    fn v8_gpio_modes_follow_the_esp_base_rules() {
        let p = &RP2350;
        let pins = Pins::new();
        assert_eq!(pins.read_from(p, 8), Err(AbiError::InvalidArgument), "never configured");
        assert_eq!(pins.check_write(p, 8), Err(AbiError::InvalidArgument));
        let pins = pins.output(p, 8).unwrap();
        assert_eq!(pins.check_write(p, 8), Ok(()));
        assert_eq!(pins.read_from(p, 8), Ok(ReadFrom::Latch));
        let pins = pins.input(p, 8).unwrap();
        assert_eq!(pins.read_from(p, 8), Ok(ReadFrom::Pad));
        assert_eq!(pins.check_write(p, 8), Err(AbiError::InvalidArgument));
        // The board policy applies first.
        assert_eq!(pins.output(p, 23), Err(AbiError::Unsupported));
        assert_eq!(pins.output(p, 24), Err(AbiError::Unsupported));
        let pins = pins.input(p, 24).unwrap();
        assert_eq!(pins.output(p, 30), Err(AbiError::InvalidArgument));
        assert_eq!(pins.read_from(p, 26), Err(AbiError::Unsupported), "ADC pin");
        let pins = pins.output(p, 25).unwrap();
        assert_eq!(pins.configured().collect::<heapless::Vec<u8, 4>>().as_slice(), &[8, 24, 25]);
        assert_eq!(pins.mode(99), Mode::Idle);
    }
}
