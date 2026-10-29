//! PWM decisions: the ABI 3 timer/channel mapping onto LEDC, the duty
//! resolution a frequency allows, duty scaling, and the open/set/stop
//! lifecycle (X9, X10, X11).
//!
//! # Mapping (X9)
//!
//! ABI timers 0–3 are LEDC low-speed timers 0–3; channels 0–1 of timer `t`
//! are LEDC low-speed channel `2·t + c` ([`ledc_channel`]), on the fixed
//! pin [`Profile::pwm_pin`]. Timers 4–5 and channels 2–3 (valid ABI ids)
//! are [`AbiError::Unsupported`]: eight LEDC channels cover four timers
//! with two outputs each.
//!
//! # Resolution and duty (X10)
//!
//! A low-speed timer divides its clock by an 8.8 fixed-point divider
//! (`div ∈ 256..=0x3FFFF`, i.e. 1.0 to 1023.99) and counts `2^bits` per
//! period, so `div = (clk << 8) / freq / 2^bits`. [`resolution`] picks the
//! largest `bits` (capped by [`Profile::pwm_max_bits`]: 16 on ESP32, 14 on
//! ESP32-S3) whose divider is at least 256 on the 80 MHz APB clock. When
//! that divider overflows (low frequencies at full width), ESP32 falls back
//! to the 1 MHz REF_TICK ([`Profile::pwm_ref_tick`]) exactly as esp-hal's
//! `Timer::configure` does; ESP32-S3 has no REF_TICK and refuses 1–4 Hz
//! with `Unsupported`.
//!
//! The ABI duty is a fraction of 65535; [`duty_raw`] rounds it to
//! `0..=2^bits` (`2^bits` is always high on LEDC).
//!
//! # Lifecycle (X11)
//!
//! [`PwmTable`] is a value; each transition returns the next table and what
//! the shell must do, and the shell commits the table only after the
//! hardware accepted it:
//!
//! * [`PwmTable::open`] starts (or retunes) a timer: both channels are
//!   bound to it, a fresh timer at duty 0, a retuned one keeping its ABI
//!   duties rescaled to the new resolution.
//! * [`PwmTable::set`] sets one channel; on a closed timer it is
//!   [`AbiError::Busy`]. The first set of a channel claims and routes its
//!   pin ([`SetStep::route`]); the pin stays routed until stop.
//! * [`PwmTable::stop`] closes a timer and returns its routed pins, which
//!   the shell drives low as GPIO and frees. Stopping a closed timer is a
//!   no-op.

use middleware::wasm_abi::AbiError;
use middleware::wasm_abi::periph::PwmSpec;

use crate::chip::Profile;

/// ABI timers backed by LEDC (`0..LEDC_TIMERS`).
pub const LEDC_TIMERS: usize = 4;

/// Outputs per ABI timer backed by LEDC (`0..LEDC_OUTPUTS`).
pub const LEDC_OUTPUTS: usize = 2;

/// LEDC low-speed channels in use (`LEDC_TIMERS · LEDC_OUTPUTS`).
pub const LEDC_CHANNELS: usize = LEDC_TIMERS * LEDC_OUTPUTS;

/// The APB clock feeding the LEDC timers, in Hz.
pub const APB_HZ: u32 = 80_000_000;

/// The ESP32 REF_TICK clock, in Hz.
pub const REF_TICK_HZ: u32 = 1_000_000;

/// Smallest 8.8 fixed-point divider (1.0).
pub const DIV_MIN: u64 = 256;

/// Largest 8.8 fixed-point divider.
pub const DIV_MAX: u64 = 0x3FFFF;

/// The LEDC low-speed channel of ABI `(timer, channel)`: `2·timer +
/// channel`; `Unsupported` for timers 4–5 and channels 2–3.
pub const fn ledc_channel(timer: u8, channel: u8) -> Result<u8, AbiError> {
    if (timer as usize) < LEDC_TIMERS && (channel as usize) < LEDC_OUTPUTS {
        Ok(timer * LEDC_OUTPUTS as u8 + channel)
    } else {
        Err(AbiError::Unsupported)
    }
}

/// The ABI timer as an LEDC timer index; `Unsupported` for timers 4–5.
const fn ledc_timer(timer: u8) -> Result<usize, AbiError> {
    if (timer as usize) < LEDC_TIMERS { Ok(timer as usize) } else { Err(AbiError::Unsupported) }
}

/// A timer clock and duty width that reach a frequency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
    /// Duty resolution in bits (`1..=pwm_max_bits`).
    pub bits: u8,
    /// Whether the timer runs from the 1 MHz REF_TICK instead of APB.
    pub ref_tick: bool,
}

/// The 8.8 divider for `freq_hz` from `clk_hz` at `bits`, as esp-hal
/// computes it.
const fn divider(clk_hz: u32, freq_hz: u32, bits: u8) -> u64 {
    ((clk_hz as u64) << 8) / freq_hz as u64 / (1u64 << bits)
}

/// The largest `bits <= max` with `freq_hz · 2^bits <= clk_hz` (so the
/// divider is at least 1.0), or 0 when none fits.
const fn widest(clk_hz: u32, freq_hz: u32, max: u8) -> u8 {
    let mut bits = max;
    while bits > 0 && (freq_hz as u64) << bits > clk_hz as u64 {
        bits -= 1;
    }
    bits
}

/// The resolution for `freq_hz` on `profile`; `Unsupported` when no clock
/// reaches it (ESP32-S3 at 1–4 Hz) or the chip has no PWM.
pub fn resolution(profile: &Profile, freq_hz: u32) -> Result<Resolution, AbiError> {
    let fits = |clk, ref_tick| {
        let bits = widest(clk, freq_hz, profile.pwm_max_bits);
        (bits > 0 && divider(clk, freq_hz, bits) <= DIV_MAX).then_some(Resolution { bits, ref_tick })
    };
    if freq_hz == 0 {
        return Err(AbiError::InvalidArgument);
    }
    fits(APB_HZ, false)
        .or_else(|| if profile.pwm_ref_tick { fits(REF_TICK_HZ, true) } else { None })
        .ok_or(AbiError::Unsupported)
}

/// The LEDC duty register value for ABI duty `duty` (a fraction of 65535)
/// at `bits`: `(duty · 2^bits + 32767) / 65535`, so 0 is always low and
/// 65535 always high (`2^bits`).
pub const fn duty_raw(duty: u16, bits: u8) -> u32 {
    ((duty as u64 * (1u64 << bits) + 32_767) / 65_535) as u32
}

/// One open timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Open {
    freq_hz: u32,
    res: Resolution,
    duty: [u16; LEDC_OUTPUTS],
    /// Outputs whose pin is claimed and routed.
    routed: [bool; LEDC_OUTPUTS],
}

/// What `pwm_open` asks of the hardware.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenStep {
    /// LEDC timer to configure.
    pub timer: u8,
    /// Its frequency.
    pub freq_hz: u32,
    /// Its clock and duty width.
    pub res: Resolution,
    /// LEDC channels to bind, with their raw duty.
    pub channels: [(u8, u32); LEDC_OUTPUTS],
}

/// What `pwm_set` asks of the hardware.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetStep {
    /// LEDC channel.
    pub channel: u8,
    /// Raw duty.
    pub raw: u32,
    /// The pin to claim and route first (the channel's first set).
    pub route: Option<u8>,
}

/// What `pwm_stop` asks of the hardware.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopStep {
    /// LEDC channels to zero.
    pub channels: [u8; LEDC_OUTPUTS],
    /// Routed pins to drive low and free.
    pub pins: [Option<u8>; LEDC_OUTPUTS],
}

/// The payload's PWM timers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PwmTable {
    timers: [Option<Open>; LEDC_TIMERS],
}

impl PwmTable {
    /// Every timer closed.
    pub const fn new() -> Self {
        PwmTable { timers: [None; LEDC_TIMERS] }
    }

    /// Opens or retunes `spec.timer`; `Unsupported` for a timer the board
    /// maps no pin to (the ESP32-CAM wires timers 0–1 only).
    pub fn open(self, profile: &Profile, spec: PwmSpec) -> Result<(Self, OpenStep), AbiError> {
        let t = ledc_timer(spec.timer)?;
        if (0..LEDC_OUTPUTS as u8).all(|c| profile.pwm_pin(spec.timer, c).is_none()) {
            return Err(AbiError::Unsupported);
        }
        let res = resolution(profile, spec.freq_hz)?;
        let (duty, routed) = self.timers[t].map_or(([0; LEDC_OUTPUTS], [false; LEDC_OUTPUTS]), |o| (o.duty, o.routed));
        let channel = |c: usize| (spec.timer * LEDC_OUTPUTS as u8 + c as u8, duty_raw(duty[c], res.bits));
        let step = OpenStep { timer: spec.timer, freq_hz: spec.freq_hz, res, channels: [channel(0), channel(1)] };
        let mut next = self;
        next.timers[t] = Some(Open { freq_hz: spec.freq_hz, res, duty, routed });
        Ok((next, step))
    }

    /// Sets the duty of `channel` on `timer`; `Busy` while the timer is
    /// closed.
    pub fn set(self, profile: &Profile, timer: u8, channel: u8, duty: u16) -> Result<(Self, SetStep), AbiError> {
        let ch = ledc_channel(timer, channel)?;
        let pin = profile.pwm_pin(timer, channel).ok_or(AbiError::Unsupported)?;
        let t = usize::from(timer);
        let c = usize::from(channel);
        let mut open = self.timers[t].ok_or(AbiError::Busy)?;
        let route = (!open.routed[c]).then_some(pin);
        open.duty[c] = duty;
        open.routed[c] = true;
        let mut next = self;
        next.timers[t] = Some(open);
        Ok((next, SetStep { channel: ch, raw: duty_raw(duty, open.res.bits), route }))
    }

    /// Closes `timer`; `None` when it was not open.
    pub fn stop(self, profile: &Profile, timer: u8) -> Result<(Self, Option<StopStep>), AbiError> {
        let t = ledc_timer(timer)?;
        let Some(open) = self.timers[t] else { return Ok((self, None)) };
        let out = |c: u8| (open.routed[usize::from(c)]).then(|| profile.pwm_pin(timer, c)).flatten();
        let base = timer * LEDC_OUTPUTS as u8;
        let mut next = self;
        next.timers[t] = None;
        Ok((next, Some(StopStep { channels: [base, base + 1], pins: [out(0), out(1)] })))
    }

    /// The frequency of an open timer.
    pub fn freq_hz(&self, timer: u8) -> Option<u32> {
        self.timers.get(usize::from(timer))?.map(|o| o.freq_hz)
    }

    /// The resolution of an open timer.
    pub fn resolution(&self, timer: u8) -> Option<Resolution> {
        self.timers.get(usize::from(timer))?.map(|o| o.res)
    }

    /// The ABI duty last set on an output of an open timer.
    pub fn duty(&self, timer: u8, channel: u8) -> Option<u16> {
        self.timers.get(usize::from(timer))?.and_then(|o| o.duty.get(usize::from(channel)).copied())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chip::{ESP32, ESP32C3, ESP32S3, ESP8266};

    fn spec(timer: i32, freq: i32) -> PwmSpec {
        PwmSpec::parse(timer, freq).unwrap()
    }

    #[test]
    fn x9_abi_timers_and_channels_map_to_ledc_low_speed_channels() {
        assert_eq!(ledc_channel(0, 0), Ok(0));
        assert_eq!(ledc_channel(0, 1), Ok(1));
        assert_eq!(ledc_channel(3, 1), Ok(7));
        for (t, c) in [(4, 0), (5, 1), (0, 2), (3, 3)] {
            assert_eq!(ledc_channel(t, c), Err(AbiError::Unsupported), "({t},{c})");
        }
        let pins = |p: &Profile| {
            (0..4u8).flat_map(|t| (0..2u8).map(move |c| (t, c))).filter_map(|(t, c)| p.pwm_pin(t, c)).collect::<heapless::Vec<u8, 8>>()
        };
        assert_eq!(pins(&ESP32), [0, 4, 13, 14]);
        assert_eq!(pins(&ESP32S3), [11, 12, 13, 14, 15, 16, 17, 18]);
        assert_eq!(ESP32.pwm_pin(4, 0), None);
        assert_eq!(ESP32.pwm_pin(2, 0), None, "the ESP32-CAM maps timers 0-1");
        assert_eq!(ESP32S3.pwm_pin(0, 2), None);
        assert_eq!(ESP32C3.pwm_pin(0, 0), None);
        assert_eq!(ESP8266.pwm_pin(0, 0), None);
    }

    #[test]
    fn x9_pwm_pins_avoid_strapping_flash_usb_console_adc1_and_i2c() {
        use crate::chip::Direction;
        // The default I2C pins (ESP32-CAM SCCB 26/27, ESP32-S3 8/9);
        // strapping pins come from the profile. The ESP32-CAM's XCLK
        // (GPIO 0) is the one strapping pin on purpose: the sensor's clock
        // is wired there (R15).
        let i2c: [(&Profile, &[u8], &[u8], usize); 2] =
            [(&ESP32, &[26, 27], &[0], 4), (&ESP32S3, &[8, 9], &[], LEDC_CHANNELS)];
        for (p, avoid, strapping_ok, len) in i2c {
            for &pin in p.pwm_pins {
                assert!(!p.is_strapping(pin) || strapping_ok.contains(&pin), "{} {pin} is a strapping pin", p.name);
                assert_eq!(p.check_pin(pin, Direction::Output), Ok(()), "{} {pin}", p.name);
                assert!(!p.has_adc(pin), "{} {pin} is ADC1", p.name);
                assert!(!avoid.contains(&pin), "{} {pin}", p.name);
                assert_eq!(p.pwm_pins.iter().filter(|&&q| q == pin).count(), 1, "{} {pin} twice", p.name);
            }
            assert_eq!(p.pwm_pins.len(), len, "{}", p.name);
        }
    }

    #[test]
    fn x10_resolution_is_the_widest_the_apb_divider_allows() {
        let r = |p, f| resolution(p, f).map(|r| (r.bits, r.ref_tick));
        assert_eq!(r(&ESP32, 1000), Ok((16, false)), "capped at 16 on ESP32");
        assert_eq!(r(&ESP32S3, 1000), Ok((14, false)), "capped at 14 on ESP32-S3");
        assert_eq!(r(&ESP32, 20_000), Ok((11, false)), "80e6 / 20e3 = 4000 >= 2^11");
        assert_eq!(r(&ESP32S3, 1_000_000), Ok((6, false)));
        assert_eq!(r(&ESP32, 1_000_000), Ok((6, false)));
        // The divider stays inside 256..=0x3FFFF across the ABI range.
        for p in [&ESP32, &ESP32S3] {
            for f in [5, 7, 50, 999, 1000, 4883, 25_000, 333_333, 1_000_000].into_iter().chain(p.pwm_ref_tick.then_some(1)) {
                let res = resolution(p, f).unwrap();
                let clk = if res.ref_tick { REF_TICK_HZ } else { APB_HZ };
                let div = divider(clk, f, res.bits);
                assert!((DIV_MIN..=DIV_MAX).contains(&div), "{} {f} Hz: div {div}", p.name);
                assert!(res.bits == p.pwm_max_bits || divider(clk, f, res.bits + 1) < DIV_MIN, "{} {f} Hz", p.name);
            }
        }
    }

    #[test]
    fn x10_low_frequencies_use_ref_tick_on_esp32_and_are_refused_on_esp32s3() {
        assert_eq!(resolution(&ESP32, 2), Ok(Resolution { bits: 16, ref_tick: false }), "312500/2 fits");
        assert_eq!(resolution(&ESP32, 1), Ok(Resolution { bits: 16, ref_tick: true }));
        assert_eq!(divider(REF_TICK_HZ, 1, 16), 3906);
        for f in 1..=4 {
            assert_eq!(resolution(&ESP32S3, f), Err(AbiError::Unsupported), "{f} Hz");
        }
        assert_eq!(resolution(&ESP32S3, 5), Ok(Resolution { bits: 14, ref_tick: false }));
        assert_eq!(resolution(&ESP32C3, 1000), Err(AbiError::Unsupported), "no PWM");
        assert_eq!(resolution(&ESP32, 0), Err(AbiError::InvalidArgument));
    }

    #[test]
    fn x10_duty_scales_with_rounding_to_the_full_range() {
        for bits in [1, 6, 8, 14, 16] {
            assert_eq!(duty_raw(0, bits), 0);
            assert_eq!(duty_raw(u16::MAX, bits), 1 << bits, "always high");
        }
        assert_eq!(duty_raw(32_768, 8), 128);
        assert_eq!(duty_raw(32_767, 1), 1, "rounds half up");
        assert_eq!(duty_raw(16_383, 1), 0);
        assert_eq!(duty_raw(1, 16), 1);
        assert_eq!(duty_raw(256, 8), 1);
        assert_eq!(duty_raw(127, 8), 0);
    }

    #[test]
    fn x11_open_binds_both_channels_at_zero_and_reopen_rescales_kept_duties() {
        let (t, step) = PwmTable::new().open(&ESP32S3, spec(1, 1000)).unwrap();
        assert_eq!(step, OpenStep { timer: 1, freq_hz: 1000, res: Resolution { bits: 14, ref_tick: false }, channels: [(2, 0), (3, 0)] });
        let (t, set) = t.set(&ESP32S3, 1, 1, 32_768).unwrap();
        assert_eq!(set, SetStep { channel: 3, raw: 8192, route: Some(14) });
        let (t, set) = t.set(&ESP32S3, 1, 1, u16::MAX).unwrap();
        assert_eq!(set.route, None, "routed once");
        assert_eq!(set.raw, 1 << 14);
        // 1 MHz: 6 bits, the kept duty rescaled.
        let (t, step) = t.open(&ESP32S3, spec(1, 1_000_000)).unwrap();
        assert_eq!(step.channels, [(2, 0), (3, 64)]);
        assert_eq!((t.freq_hz(1), t.duty(1, 1), t.resolution(1).map(|r| r.bits)), (Some(1_000_000), Some(u16::MAX), Some(6)));
        let (_, set) = t.set(&ESP32S3, 1, 1, 0).unwrap();
        assert_eq!(set.route, None, "reopening keeps the routed pin");
    }

    #[test]
    fn x11_set_needs_an_open_timer_and_unmapped_ids_are_unsupported() {
        let t = PwmTable::new();
        assert_eq!(t.set(&ESP32, 0, 0, 1), Err(AbiError::Busy));
        assert_eq!(t.set(&ESP32, 0, 2, 1), Err(AbiError::Unsupported));
        assert_eq!(t.set(&ESP32, 4, 0, 1), Err(AbiError::Unsupported));
        assert_eq!(t.open(&ESP32, spec(4, 1000)).map(|_| ()), Err(AbiError::Unsupported));
        assert_eq!(t.open(&ESP32, spec(5, 1000)).map(|_| ()), Err(AbiError::Unsupported));
        for timer in [2, 3] {
            assert_eq!(t.open(&ESP32, spec(timer, 1000)).map(|_| ()), Err(AbiError::Unsupported), "ESP32-CAM timer {timer}");
        }
        assert_eq!(t.stop(&ESP32, 5).map(|_| ()), Err(AbiError::Unsupported));
        assert_eq!(t.open(&ESP32S3, spec(0, 3)).map(|_| ()), Err(AbiError::Unsupported), "no clock reaches 3 Hz");
        assert_eq!(t.open(&ESP32C3, spec(0, 1000)).map(|_| ()), Err(AbiError::Unsupported));
        let (open, _) = t.open(&ESP32, spec(0, 1000)).unwrap();
        assert_eq!(open.set(&ESP32, 0, 3, 1), Err(AbiError::Unsupported));
        assert_eq!(open.set(&ESP32, 1, 0, 1), Err(AbiError::Busy), "other timer still closed");
    }

    #[test]
    fn x11_stop_returns_routed_pins_and_closes_the_timer() {
        let (t, _) = PwmTable::new().open(&ESP32S3, spec(3, 50)).unwrap();
        let (t, _) = t.set(&ESP32S3, 3, 1, 100).unwrap();
        let (t, stop) = t.stop(&ESP32S3, 3).unwrap();
        assert_eq!(stop, Some(StopStep { channels: [6, 7], pins: [None, Some(18)] }));
        assert_eq!(t, PwmTable::new());
        assert_eq!(t.set(&ESP32S3, 3, 1, 1), Err(AbiError::Busy));
        assert_eq!(t.stop(&ESP32S3, 3), Ok((t, None)), "stopping a closed timer is a no-op");
        // Reopening after stop starts from duty 0 with nothing routed.
        let (t, step) = t.open(&ESP32S3, spec(3, 50)).unwrap();
        assert_eq!(step.channels, [(6, 0), (7, 0)]);
        assert_eq!(t.set(&ESP32S3, 3, 1, 1).unwrap().1.route, Some(18));
        // The ESP32-CAM's timer 1 drives GPIO 13 and 14.
        let (t, _) = PwmTable::new().open(&ESP32, spec(1, 50)).unwrap();
        let (t, _) = t.set(&ESP32, 1, 1, 100).unwrap();
        let (_, stop) = t.stop(&ESP32, 1).unwrap();
        assert_eq!(stop, Some(StopStep { channels: [2, 3], pins: [None, Some(14)] }));
    }
}
