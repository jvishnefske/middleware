//! PWM for payloads on the ESP32 (ABI 3 `pwm_open` / `pwm_set` /
//! `pwm_stop`): LEDC high-speed timer 0 and its channels 0..3.
//!
//! The board pin map (`esp_base::chip`, ESP32-CAM) wires PWM timer 0's
//! channels to GPIO 0 (the camera's XCLK), 4 (the flash LED), 13 and 14.
//! [`Pwm::new`] takes the LEDC peripheral and turns its clock on through
//! esp-hal's [`Ledc`] (which never turns it off again); [`Pwm`] then keeps
//! only the frequency plan of the open timer
//! (`esp_base::periph::pwm::plan`: the most duty bits the frequency
//! allows, 2 at 20 MHz), so it can rest in the device's static home.
//! esp-hal's LEDC timer and channel handles borrow the register block and
//! each other, so they are rebuilt for each call (they are register
//! handles; building one writes nothing until it is configured) instead of
//! being stored. A channel has no esp-hal pin
//! ([`NoPin`]): the device routes `LEDC_HS_SIG<n>` to the mapped pin
//! through the pin bank, like the SPI and UART signals, so the pin stays in
//! the bank and is released with it.
//!
//! The duty is the ABI's 16-bit fraction rounded to the timer's resolution
//! (`esp_base::periph::pwm::duty_counts`).

use esp_base::periph::pwm::{APB_HZ, Plan, duty_counts, plan};
use esp_hal::gpio::{DriveMode, NoPin, OutputSignal};
use esp_hal::ledc::channel::{self, ChannelHW, ChannelIFace};
use esp_hal::ledc::timer::{self, TimerIFace};
use esp_hal::ledc::{HighSpeed, Ledc};
use esp_hal::peripherals::LEDC;
use esp_hal::time::Rate;
use wasm_host::{AbiError, PwmSpec};

/// The GPIO-matrix signal of PWM timer 0's `channel`.
pub fn signal(channel: u8) -> Result<OutputSignal, AbiError> {
    match channel {
        0 => Ok(OutputSignal::LEDC_HS_SIG0),
        1 => Ok(OutputSignal::LEDC_HS_SIG1),
        2 => Ok(OutputSignal::LEDC_HS_SIG2),
        3 => Ok(OutputSignal::LEDC_HS_SIG3),
        _ => Err(AbiError::Unsupported),
    }
}

/// LEDC duty resolution for `bits` (1..=16).
fn duty(bits: u8) -> Result<timer::config::Duty, AbiError> {
    use timer::config::Duty::*;
    Ok(match bits {
        1 => Duty1Bit,
        2 => Duty2Bit,
        3 => Duty3Bit,
        4 => Duty4Bit,
        5 => Duty5Bit,
        6 => Duty6Bit,
        7 => Duty7Bit,
        8 => Duty8Bit,
        9 => Duty9Bit,
        10 => Duty10Bit,
        11 => Duty11Bit,
        12 => Duty12Bit,
        13 => Duty13Bit,
        14 => Duty14Bit,
        15 => Duty15Bit,
        16 => Duty16Bit,
        _ => return Err(AbiError::Unsupported),
    })
}

fn channel_number(channel: u8) -> Result<channel::Number, AbiError> {
    match channel {
        0 => Ok(channel::Number::Channel0),
        1 => Ok(channel::Number::Channel1),
        2 => Ok(channel::Number::Channel2),
        3 => Ok(channel::Number::Channel3),
        _ => Err(AbiError::Unsupported),
    }
}

/// LEDC for payloads: the open timer's plan.
pub struct Pwm {
    open: Option<Plan>,
}

impl Pwm {
    /// Takes the LEDC peripheral and turns its clock on (nothing
    /// configured).
    pub fn new(ledc: esp_hal::peripherals::LEDC<'static>) -> Self {
        let _clock_on = Ledc::new(ledc);
        Pwm { open: None }
    }

    /// The configured high-speed timer 0 for `plan`.
    fn timer(&self, plan: Plan) -> Result<timer::Timer<'static, HighSpeed>, AbiError> {
        let mut t = timer::Timer::<HighSpeed>::new(LEDC::regs(), timer::Number::Timer0);
        t.configure(timer::config::Config {
            duty: duty(plan.bits)?,
            clock_source: timer::HSClockSource::APBClk,
            frequency: Rate::from_hz(plan.freq_hz),
        })
        .map_err(|_| AbiError::Unsupported)?;
        Ok(t)
    }

    /// Starts (or retunes) timer 0 at `spec.freq_hz` with every channel at
    /// duty 0; `Unsupported` for another timer or an unreachable frequency.
    pub fn open(&mut self, spec: PwmSpec) -> Result<(), AbiError> {
        if spec.timer != 0 {
            return Err(AbiError::Unsupported);
        }
        let p = plan(APB_HZ, spec.freq_hz)?;
        let t = self.timer(p)?;
        for ch in 0..4 {
            set(&t, ch, 0)?;
        }
        self.open = Some(p);
        Ok(())
    }

    /// Sets `channel`'s duty (`Busy` when the timer is not open).
    pub fn set(&mut self, timer: u8, channel: u8, value: u16) -> Result<(), AbiError> {
        let p = match (timer, self.open) {
            (0, Some(p)) => p,
            (0, None) => return Err(AbiError::Busy),
            _ => return Err(AbiError::Unsupported),
        };
        let t = self.timer(p)?;
        set(&t, channel, duty_counts(value, p.bits))
    }

    /// Drives every channel low and forgets the timer.
    pub fn stop(&mut self, timer: u8) -> Result<(), AbiError> {
        if timer != 0 {
            return Err(AbiError::Unsupported);
        }
        if let Some(p) = self.open.take() {
            let t = self.timer(p)?;
            for ch in 0..4 {
                set(&t, ch, 0)?;
            }
        }
        Ok(())
    }

    /// Whether timer 0 is open.
    pub const fn is_open(&self) -> bool {
        self.open.is_some()
    }
}

/// Configures `channel` on `t` and sets its duty to `counts`.
fn set(t: &timer::Timer<'static, HighSpeed>, channel: u8, counts: u32) -> Result<(), AbiError> {
    let mut ch = channel::Channel::<HighSpeed>::new(channel_number(channel)?, NoPin);
    ch.configure(channel::config::Config { timer: t, duty_pct: 0, drive_mode: DriveMode::PushPull })
        .map_err(|_| AbiError::Unsupported)?;
    ch.set_duty_hw(counts);
    Ok(())
}
