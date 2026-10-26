//! PWM outputs through the `pwm_open`, `pwm_set` and `pwm_stop` imports
//! (ABI 3).
//!
//! A payload has [`MAX_PWM_TIMERS`] PWM timers with [`PWM_CHANNELS`]
//! channels each. The firmware maps every `(timer, channel)` to a fixed pin
//! (board documentation), so a payload never names pins here. All channels
//! of one timer share its frequency.
//!
//! [`Pwm::open`] starts a timer with every channel at duty 0 and validates
//! its arguments with the ABI's own parser ([`PwmSpec::parse`]) before
//! calling the host. [`Pwm::channel`] hands out a [`PwmChannel`] that
//! implements `embedded_hal::pwm::SetDutyCycle` with a full-scale duty of
//! `u16::MAX` (the ABI's own scale, so no rescaling happens). A channel does
//! not borrow its timer: after [`Pwm::stop`], setting a duty fails with
//! whatever the host reports (`Busy` for a timer that is not open).

use crate::gpio::HalError;
use crate::{AbiError, check, sys};
pub use middleware::wasm_abi::periph::{
    MAX_PWM_TIMERS, PWM_CHANNELS, PWM_FREQ_HZ, PwmDuty, PwmSpec,
};

/// Validates `pwm_open` arguments with the ABI parser.
fn spec(timer: u8, freq_hz: u32) -> Result<PwmSpec, AbiError> {
    let freq = i32::try_from(freq_hz).map_err(|_| AbiError::InvalidArgument)?;
    PwmSpec::parse(i32::from(timer), freq)
}

/// An open PWM timer.
#[derive(Debug, PartialEq, Eq)]
pub struct Pwm {
    spec: PwmSpec,
}

impl Pwm {
    /// Starts timer `timer` (`< MAX_PWM_TIMERS`) at `freq_hz` (within
    /// [`PWM_FREQ_HZ`]), every channel at duty 0. Opening an open timer
    /// changes its frequency.
    pub fn open(timer: u8, freq_hz: u32) -> Result<Self, AbiError> {
        let spec = spec(timer, freq_hz)?;
        check(sys::pwm_open(i32::from(spec.timer), spec.freq_hz as i32))?;
        Ok(Pwm { spec })
    }

    /// The timer id.
    pub fn timer(&self) -> u8 {
        self.spec.timer
    }

    /// The frequency in Hz.
    pub fn freq_hz(&self) -> u32 {
        self.spec.freq_hz
    }

    /// Channel `channel` (`< PWM_CHANNELS`) of this timer.
    pub fn channel(&self, channel: u8) -> Result<PwmChannel, AbiError> {
        let d = PwmDuty::parse(i32::from(self.spec.timer), i32::from(channel), 0)?;
        Ok(PwmChannel {
            timer: d.timer,
            channel: d.channel,
        })
    }

    /// Stops the timer; its outputs go low.
    pub fn stop(self) -> Result<(), AbiError> {
        check(sys::pwm_stop(i32::from(self.spec.timer))).map(|_| ())
    }
}

/// One output channel of a [`Pwm`] timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PwmChannel {
    timer: u8,
    channel: u8,
}

impl PwmChannel {
    /// The channel number.
    pub fn channel(&self) -> u8 {
        self.channel
    }

    /// Sets the high time as a fraction of `u16::MAX` (0 = always low,
    /// `u16::MAX` = always high).
    pub fn set_duty(&mut self, duty: u16) -> Result<(), AbiError> {
        check(sys::pwm_set(
            i32::from(self.timer),
            i32::from(self.channel),
            i32::from(duty),
        ))
        .map(|_| ())
    }
}

impl embedded_hal::pwm::Error for HalError {
    fn kind(&self) -> embedded_hal::pwm::ErrorKind {
        embedded_hal::pwm::ErrorKind::Other
    }
}

impl embedded_hal::pwm::ErrorType for PwmChannel {
    type Error = HalError;
}

impl embedded_hal::pwm::SetDutyCycle for PwmChannel {
    fn max_duty_cycle(&self) -> u16 {
        u16::MAX
    }

    fn set_duty_cycle(&mut self, duty: u16) -> Result<(), HalError> {
        self.set_duty(duty).map_err(HalError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embedded_hal::pwm::SetDutyCycle;

    #[test]
    fn open_validates_before_the_host() {
        let bad = Err(AbiError::InvalidArgument);
        assert_eq!(Pwm::open(MAX_PWM_TIMERS as u8, 1000), bad);
        assert_eq!(Pwm::open(0, 0), bad);
        assert_eq!(Pwm::open(0, 1_000_001), bad);
        assert_eq!(Pwm::open(0, u32::MAX), bad);
        assert_eq!(Pwm::open(5, 20_000), Err(AbiError::Unsupported));
        assert_eq!(
            spec(5, 20_000),
            Ok(PwmSpec {
                timer: 5,
                freq_hz: 20_000
            })
        );
    }

    #[test]
    fn channels_are_range_checked_and_reach_the_host() {
        let pwm = Pwm {
            spec: spec(2, 1000).expect("valid"),
        };
        assert_eq!((pwm.timer(), pwm.freq_hz()), (2, 1000));
        assert_eq!(
            pwm.channel(PWM_CHANNELS as u8),
            Err(AbiError::InvalidArgument)
        );
        let mut ch = pwm.channel(3).expect("valid channel");
        assert_eq!(ch.channel(), 3);
        assert_eq!(ch.max_duty_cycle(), u16::MAX);
        assert_eq!(ch.set_duty(100), Err(AbiError::Unsupported));
        assert_eq!(
            ch.set_duty_cycle_percent(50),
            Err(HalError(AbiError::Unsupported))
        );
        assert_eq!(pwm.stop(), Err(AbiError::Unsupported));
    }
}
