//! Digital GPIO through the `gpio_*` imports.
//!
//! [`Output::new`] and [`Input::new`] configure the pin on the host and fail
//! with [`AbiError::Unsupported`] for pins the firmware reserves (flash, USB,
//! console UART) or [`AbiError::InvalidArgument`] for pins the chip lacks.
//! Both types also implement the `embedded-hal` 1.0 digital traits, with
//! [`HalError`] wrapping the [`AbiError`], so drivers written against
//! `embedded-hal` run inside a payload.
//!
//! [`Output`] remembers the last level it wrote, so [`Output::toggle`] and
//! `StatefulOutputPin` need no read-back from the host.
//!
//! # Edge interrupts (ABI 2)
//!
//! [`Input::enable_irq`] asks the firmware to raise an
//! [`Event::Gpio`] on the selected [`Edge`]s; the
//! event reaches [`Payload::on_irq`](crate::Payload::on_irq) from the main
//! loop, never from the ISR, carrying the level after the edge and the ISR's
//! timestamp. GPIO events do not coalesce: a bouncing contact can fill the
//! queue, so debounce with the timestamps and check
//! [`irq::overflow`](crate::irq::overflow). [`Input::disable_irq`] also
//! discards the pin's queued events.

use crate::irq::Event;
use crate::{AbiError, Pull, check, sys};
pub use middleware::wasm_abi::periph::Edge;

/// Pin number as the ABI expects it.
fn pin_i32(pin: u8) -> i32 {
    i32::from(pin)
}

/// A pin configured as a push-pull output, initially low.
#[derive(Debug, PartialEq, Eq)]
pub struct Output {
    pin: u8,
    high: bool,
}

impl Output {
    /// Configures `pin` as an output and drives it low.
    pub fn new(pin: u8) -> Result<Self, AbiError> {
        check(sys::gpio_output(pin_i32(pin)))?;
        let mut out = Output { pin, high: true };
        out.set(false)?;
        Ok(out)
    }

    /// The pin number.
    pub fn pin(&self) -> u8 {
        self.pin
    }

    /// Drives the pin high (`true`) or low (`false`).
    pub fn set(&mut self, high: bool) -> Result<(), AbiError> {
        check(sys::gpio_write(pin_i32(self.pin), i32::from(high)))?;
        self.high = high;
        Ok(())
    }

    /// Inverts the level and returns the new one.
    pub fn toggle(&mut self) -> Result<bool, AbiError> {
        let next = !self.high;
        self.set(next)?;
        Ok(next)
    }

    /// The last level successfully written.
    pub fn is_set_high(&self) -> bool {
        self.high
    }
}

/// A pin configured as an input.
#[derive(Debug, PartialEq, Eq)]
pub struct Input {
    pin: u8,
}

impl Input {
    /// Configures `pin` as an input with the given pull.
    pub fn new(pin: u8, pull: Pull) -> Result<Self, AbiError> {
        check(sys::gpio_input(pin_i32(pin), pull as i32))?;
        Ok(Input { pin })
    }

    /// The pin number.
    pub fn pin(&self) -> u8 {
        self.pin
    }

    /// Reads the level: `true` for high.
    pub fn is_high(&self) -> Result<bool, AbiError> {
        check(sys::gpio_read(pin_i32(self.pin))).map(|v| v != 0)
    }

    /// Raises an [`Event::Gpio`] on each matching edge (`Edge::Off`
    /// disables, like [`Input::disable_irq`]).
    pub fn enable_irq(&mut self, edge: Edge) -> Result<(), AbiError> {
        check(sys::gpio_irq(pin_i32(self.pin), edge as i32)).map(|_| ())
    }

    /// Stops edge events and purges the pin's queued ones.
    pub fn disable_irq(&mut self) -> Result<(), AbiError> {
        self.enable_irq(Edge::Off)
    }

    /// The level after the edge if `event` is a GPIO event of this pin.
    pub fn edge_level(&self, event: &Event) -> Option<bool> {
        match *event {
            Event::Gpio { pin, high, .. } if pin == self.pin => Some(high),
            _ => None,
        }
    }
}

/// [`AbiError`] as an `embedded-hal` error: the SDK's error type for the
/// digital traits here, `SetDutyCycle` ([`crate::pwm`]) and `I2c`
/// ([`crate::i2c`], which reports [`AbiError::Transport`] as a missing
/// acknowledge).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HalError(pub AbiError);

impl embedded_hal::digital::Error for HalError {
    fn kind(&self) -> embedded_hal::digital::ErrorKind {
        embedded_hal::digital::ErrorKind::Other
    }
}

impl embedded_hal::digital::ErrorType for Output {
    type Error = HalError;
}

impl embedded_hal::digital::OutputPin for Output {
    fn set_low(&mut self) -> Result<(), HalError> {
        self.set(false).map_err(HalError)
    }
    fn set_high(&mut self) -> Result<(), HalError> {
        self.set(true).map_err(HalError)
    }
}

impl embedded_hal::digital::StatefulOutputPin for Output {
    fn is_set_high(&mut self) -> Result<bool, HalError> {
        Ok(self.high)
    }
    fn is_set_low(&mut self) -> Result<bool, HalError> {
        Ok(!self.high)
    }
}

impl embedded_hal::digital::ErrorType for Input {
    type Error = HalError;
}

impl embedded_hal::digital::InputPin for Input {
    fn is_high(&mut self) -> Result<bool, HalError> {
        Input::is_high(self).map_err(HalError)
    }
    fn is_low(&mut self) -> Result<bool, HalError> {
        Input::is_high(self).map(|h| !h).map_err(HalError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_stubs_refuse_pins() {
        assert_eq!(Output::new(8), Err(AbiError::Unsupported));
        assert_eq!(Input::new(9, Pull::Up), Err(AbiError::Unsupported));
    }

    #[test]
    fn irq_helpers() {
        let mut input = Input { pin: 4 };
        assert_eq!(input.enable_irq(Edge::Falling), Err(AbiError::Unsupported));
        assert_eq!(input.disable_irq(), Err(AbiError::Unsupported));
        let mine = Event::Gpio {
            pin: 4,
            high: false,
            time_us: 1,
        };
        let other = Event::Gpio {
            pin: 5,
            high: true,
            time_us: 1,
        };
        assert_eq!(input.edge_level(&mine), Some(false));
        assert_eq!(input.edge_level(&other), None);
        for e in [Edge::Off, Edge::Rising, Edge::Falling, Edge::Any] {
            assert_eq!(Edge::from_i32(e as i32), Ok(e));
        }
    }

    #[test]
    fn pull_codes_match_the_abi() {
        assert_eq!(Pull::from_i32(Pull::None as i32), Some(Pull::None));
        assert_eq!(Pull::from_i32(Pull::Up as i32), Some(Pull::Up));
        assert_eq!(Pull::from_i32(Pull::Down as i32), Some(Pull::Down));
    }
}
