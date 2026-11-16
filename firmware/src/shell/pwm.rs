//! PWM outputs for payloads: LEDC low-speed timers 0–3 and channels 0–7
//! (X9–X11).
//!
//! The pure [`esp_base::periph::pwm`] table decides the mapping, the duty
//! resolution, the raw duty values and which pins to route; this module
//! only writes them to the LEDC.
//!
//! [`LedcHw`] keeps the `LEDC` peripheral singleton for the life of the
//! firmware (it rests in the static hardware home between payload runs,
//! like the SPI and UART drivers) and builds short-lived esp-hal handles
//! (`Ledc`, `Timer`, `Channel`) for each call. esp-hal's `Ledc::new`
//! enables and resets the peripheral clock only on first use, so this is
//! cheap and keeps no borrow across calls. A fresh esp-hal `Timer` handle
//! does not know it was configured, so binding channels happens in the same
//! call as configuring their timer ([`LedcHw::open`]).
//!
//! Channels are created with `NoPin`: esp-hal connects nothing, and the
//! device routes the channel's fixed pin through the GPIO matrix itself
//! (`PinBank::route_out(pin, `[`signal`]`(ch))`), so the pin never leaves
//! the pin bank and plain GPIO takes it back on stop.
//!
//! esp-hal picks the clock itself: it computes the divider on APB and, on
//! ESP32 only, switches to REF_TICK when that divider overflows, the same
//! rule [`esp_base::periph::pwm::resolution`] applies, so the pure table's
//! `ref_tick` flag is informational here.

use esp_base::periph::pwm::{LEDC_CHANNELS, OpenStep};
use esp_hal::gpio::{DriveMode, NoPin, OutputSignal};
use esp_hal::ledc::channel::{self, ChannelHW, ChannelIFace};
use esp_hal::ledc::timer::{self, TimerIFace};
use esp_hal::ledc::{LSGlobalClkSource, Ledc, LowSpeed};
use esp_hal::peripherals::LEDC;
use esp_hal::time::Rate;
use middleware::wasm_abi::AbiError;

/// The LEDC low-speed output signal of channel `ch` (`0..8`).
pub fn signal(ch: u8) -> OutputSignal {
    match ch {
        0 => OutputSignal::LEDC_LS_SIG0,
        1 => OutputSignal::LEDC_LS_SIG1,
        2 => OutputSignal::LEDC_LS_SIG2,
        3 => OutputSignal::LEDC_LS_SIG3,
        4 => OutputSignal::LEDC_LS_SIG4,
        5 => OutputSignal::LEDC_LS_SIG5,
        6 => OutputSignal::LEDC_LS_SIG6,
        _ => OutputSignal::LEDC_LS_SIG7,
    }
}

fn timer_number(t: u8) -> timer::Number {
    match t {
        0 => timer::Number::Timer0,
        1 => timer::Number::Timer1,
        2 => timer::Number::Timer2,
        _ => timer::Number::Timer3,
    }
}

fn channel_number(ch: u8) -> channel::Number {
    match ch {
        0 => channel::Number::Channel0,
        1 => channel::Number::Channel1,
        2 => channel::Number::Channel2,
        3 => channel::Number::Channel3,
        4 => channel::Number::Channel4,
        5 => channel::Number::Channel5,
        6 => channel::Number::Channel6,
        _ => channel::Number::Channel7,
    }
}

/// The LED controller, idle or driving a payload's PWM outputs.
pub struct LedcHw {
    ledc: LEDC<'static>,
}

impl LedcHw {
    /// Takes the peripheral at boot and selects the APB clock for the
    /// low-speed timers (the ESP32-S3 low-speed timers have no clock until
    /// it is selected).
    pub fn new(mut ledc: LEDC<'static>) -> Self {
        Ledc::new(ledc.reborrow()).set_global_slow_clock(LSGlobalClkSource::APBClk);
        LedcHw { ledc }
    }

    /// Configures a timer and binds its channels with their raw duties.
    pub fn open(&mut self, step: &OpenStep) -> Result<(), AbiError> {
        let ledc = Ledc::new(self.ledc.reborrow());
        let mut t = ledc.timer::<LowSpeed>(timer_number(step.timer));
        let duty = timer::config::Duty::try_from(u32::from(step.res.bits)).map_err(|_| AbiError::Unsupported)?;
        t.configure(timer::config::Config {
            duty,
            clock_source: timer::LSClockSource::APBClk,
            frequency: Rate::from_hz(step.freq_hz),
        })
        .map_err(|_| AbiError::Unsupported)?;
        for &(ch, raw) in &step.channels {
            let mut c = ledc.channel::<LowSpeed>(channel_number(ch), NoPin);
            c.configure(channel::config::Config { timer: &t, duty_pct: 0, drive_mode: DriveMode::PushPull })
                .map_err(|_| AbiError::Unsupported)?;
            c.set_duty_hw(raw);
        }
        Ok(())
    }

    /// Writes a channel's raw duty (the channel stays bound to its timer).
    pub fn set(&mut self, ch: u8, raw: u32) {
        let ledc = Ledc::new(self.ledc.reborrow());
        ledc.channel::<LowSpeed>(channel_number(ch), NoPin).set_duty_hw(raw);
    }

    /// Drives every payload channel to duty 0 (payload stop).
    pub fn zero_all(&mut self) {
        for ch in 0..LEDC_CHANNELS as u8 {
            self.set(ch, 0);
        }
    }
}
