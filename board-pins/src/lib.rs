//! Board pin rules, shared by the firmwares and the web editor.
//!
//! A [`PinMap`] is the pin half of a board profile: which GPIOs the chip
//! has, which the board reserves (flash, PSRAM, USB, the console UART, the
//! Pico's power and sense pins), which have no output driver, which GPIOs
//! the ADC channels own, the board-wired I2C buses and PWM outputs, and
//! how SPI pins are routed. The PWM outputs are the ESP base firmware's
//! ABI 3 table (main 0.5): ABI timer `t` (0–3) channel `c` (0–1) is entry
//! `2·t + c` of [`PinMap::pwm_table`], an LEDC low-speed channel; a pair
//! past the table is not wired. The ESP base firmware (`firmware/src/chip.rs`)
//! and the RP base firmware (`firmware-rp/src/chip.rs`) build their
//! `Profile`s on these maps, and `web/protocol-wasm` exposes them to the
//! browser, so the web pin table checks pins with the rules the device
//! applies instead of a second copy.
//!
//! * [`boards`] — the maps: [`boards::PICO`] (Raspberry Pi Pico and Pico 2),
//!   [`boards::ESP32`] (AI-Thinker ESP32-CAM), [`boards::ESP32S3`],
//!   [`boards::ESP32C3`], [`boards::ESP8266`], and [`boards::by_chip`].
//! * [`rp`] — the RP2040 / RP2350 GPIO function tables for SPI (F1) and
//!   I2C (F3).
//! * [`spi`] — resolving a payload's SPI pins on a map (one SPI block on
//!   the RP chips; any capable pin through the ESP GPIO matrix).
//!
//! [`PinMap::check`] is the board policy behind every `gpio_*` call:
//! [`PinError::NoSuchPin`] for a pin the chip lacks (the ABI's
//! `InvalidArgument`), [`PinError::Reserved`] for a reserved pin and
//! [`PinError::InputOnly`] for an output on an input-only pin (both
//! `Unsupported`). Each firmware maps these to its `AbiError`.
//!
//! Everything is `const` data and pure functions: no heap, no I/O, and
//! every loop is bounded by the 64 possible pins.
#![no_std]
#![forbid(unsafe_code)]

pub mod boards;
pub mod rp;
pub mod spi;

/// What a payload wants to do with a pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Read the pin level.
    Input,
    /// Drive the pin.
    Output,
}

/// Why a pin cannot be used as asked ([`PinMap::check`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinError {
    /// The chip has no such GPIO.
    NoSuchPin,
    /// The board uses the pin itself ([`PinMap::reason`] says for what).
    Reserved,
    /// The pin has no output driver and an output was asked for.
    InputOnly,
}

/// Why a pin cannot be read by the ADC ([`PinMap::adc_channel`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdcError {
    /// The chip has no such GPIO.
    NoSuchPin,
    /// No ADC channel samples the pin.
    NotAnalog,
}

/// How payload SPI pins reach the SPI controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpiRouting {
    /// Through the ESP GPIO matrix: any pin that passes [`PinMap::check`]
    /// in the signal's direction.
    Matrix,
    /// Through the RP2040 / RP2350 function table ([`rp::spi_function`]):
    /// SCK, MOSI and MISO on GPIOs with that signal, all on one block; CS
    /// is driven as a plain output, so any output pin.
    RpFunctionTable,
}

/// A plain-language reason the board holds some pins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reason {
    /// The pins (bit `n` for GPIO `n`).
    pub pins: u64,
    /// What they are used for, e.g. "the SPI flash".
    pub why: &'static str,
}

/// The pin half of a board profile (see the crate docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinMap {
    gpio: u64,
    reserved: u64,
    input_only: u64,
    adc: &'static [u8],
    i2c: [Option<(u8, u8)>; 2],
    pwm: &'static [u8],
    spi: SpiRouting,
    reasons: &'static [Reason],
}

/// ABI PWM timers a board can wire (`0..PWM_TIMERS`).
pub const PWM_TIMERS: u8 = 4;

/// Outputs per ABI PWM timer a board can wire (`0..PWM_OUTPUTS`).
pub const PWM_OUTPUTS: u8 = 2;

/// Bit `pin` set, or 0 for a pin past 63.
pub const fn bit(pin: u8) -> u64 {
    if pin < 64 { 1u64 << pin } else { 0 }
}

/// Bits `from..=to` set.
pub const fn span(from: u8, to: u8) -> u64 {
    let mut m = 0u64;
    let mut i = from;
    while i <= to && i < 64 {
        m |= 1u64 << i;
        i += 1;
    }
    m
}

const fn has(mask: u64, pin: u8) -> bool {
    mask & bit(pin) != 0
}

/// The pins of `mask`, ascending.
pub fn pins_of(mask: u64) -> impl Iterator<Item = u8> {
    (0u8..64).filter(move |&p| has(mask, p))
}

impl PinMap {
    /// The pins that exist (bit `n` for GPIO `n`).
    pub const fn gpio_mask(&self) -> u64 {
        self.gpio
    }

    /// The pins the board keeps from payloads.
    pub const fn reserved_mask(&self) -> u64 {
        self.reserved
    }

    /// The pins without an output driver.
    pub const fn input_only_mask(&self) -> u64 {
        self.input_only
    }

    /// The pins an ADC channel samples.
    pub const fn adc_mask(&self) -> u64 {
        let mut m = 0u64;
        let mut i = 0;
        while i < self.adc.len() {
            m |= bit(self.adc[i]);
            i += 1;
        }
        m
    }

    /// Whether the chip has GPIO `pin`.
    pub const fn exists(&self, pin: u8) -> bool {
        has(self.gpio, pin)
    }

    /// Whether the board keeps `pin` from payloads.
    pub const fn is_reserved(&self, pin: u8) -> bool {
        has(self.reserved, pin)
    }

    /// Whether `pin` can only be read.
    pub const fn is_input_only(&self, pin: u8) -> bool {
        has(self.input_only, pin)
    }

    /// Whether `pin` exists and has an output driver.
    pub const fn can_output(&self, pin: u8) -> bool {
        self.exists(pin) && !self.is_input_only(pin)
    }

    /// Whether a payload may use `pin` as `dir` (see the crate docs).
    pub const fn check(&self, pin: u8, dir: Direction) -> Result<(), PinError> {
        if !self.exists(pin) {
            Err(PinError::NoSuchPin)
        } else if self.is_reserved(pin) {
            Err(PinError::Reserved)
        } else if matches!(dir, Direction::Output) && self.is_input_only(pin) {
            Err(PinError::InputOnly)
        } else {
            Ok(())
        }
    }

    /// Every GPIO the chip has, ascending.
    pub fn gpios(&self) -> impl Iterator<Item = u8> {
        pins_of(self.gpio)
    }

    /// Pins a payload may use at all (they exist and are not reserved),
    /// ascending.
    pub fn payload_pins(&self) -> impl Iterator<Item = u8> {
        pins_of(self.gpio & !self.reserved)
    }

    /// The ADC channel sampling `pin`.
    pub fn adc_channel(&self, pin: u8) -> Result<u8, AdcError> {
        if !self.exists(pin) {
            return Err(AdcError::NoSuchPin);
        }
        let channel = self
            .adc
            .iter()
            .position(|&p| p == pin)
            .ok_or(AdcError::NotAnalog)?;
        u8::try_from(channel).map_err(|_| AdcError::NotAnalog)
    }

    /// The GPIOs of ADC channels 0, 1, 2, ... (empty when the board gives
    /// payloads no analog input).
    pub const fn adc_pins(&self) -> &'static [u8] {
        self.adc
    }

    /// I2C `bus`'s board-wired (SDA, SCL), if the board maps the bus.
    pub fn i2c_pins(&self, bus: u8) -> Option<(u8, u8)> {
        self.i2c.get(usize::from(bus)).copied().flatten()
    }

    /// The pin PWM `timer`'s `channel` drives, if the board wires it
    /// (entry `2·timer + channel` of [`PinMap::pwm_table`]).
    pub fn pwm_pin(&self, timer: u8, channel: u8) -> Option<u8> {
        if timer >= PWM_TIMERS || channel >= PWM_OUTPUTS {
            return None;
        }
        self.pwm.get(usize::from(timer * PWM_OUTPUTS + channel)).copied()
    }

    /// The PWM output pins, indexed by `2·timer + channel` (empty when the
    /// board wires none).
    pub const fn pwm_table(&self) -> &'static [u8] {
        self.pwm
    }

    /// The ((timer, channel), pin) PWM outputs, in table order.
    pub fn pwm_pins(&self) -> impl Iterator<Item = ((u8, u8), u8)> + '_ {
        self.pwm.iter().zip(0u8..).map(|(&p, i)| ((i / PWM_OUTPUTS, i % PWM_OUTPUTS), p))
    }

    /// How SPI pins are routed.
    pub const fn spi_routing(&self) -> SpiRouting {
        self.spi
    }

    /// What the board uses `pin` for, when it holds it (reserved or input
    /// only).
    pub fn reason(&self, pin: u8) -> Option<&'static str> {
        self.reasons
            .iter()
            .find(|r| has(r.pins, pin))
            .map(|r| r.why)
    }
}

#[cfg(test)]
mod tests {
    use super::boards::*;
    use super::*;

    #[test]
    fn masks_and_spans() {
        assert_eq!(span(0, 3), 0b1111);
        assert_eq!(span(62, 70), bit(62) | bit(63));
        assert_eq!(bit(64), 0);
        assert_eq!(
            pins_of(bit(3) | bit(40)).collect::<heapless_free::Vec4>(),
            heapless_free::Vec4::of(&[3, 40])
        );
    }

    #[test]
    fn check_follows_exists_reserved_input_only() {
        assert_eq!(ESP32.check(40, Direction::Input), Err(PinError::NoSuchPin));
        assert_eq!(ESP32.check(6, Direction::Input), Err(PinError::Reserved));
        assert_eq!(ESP32.check(34, Direction::Output), Err(PinError::InputOnly));
        assert_eq!(ESP32.check(34, Direction::Input), Ok(()));
        assert!(ESP32.can_output(4) && !ESP32.can_output(35) && !ESP32.can_output(20));
        assert_eq!(PICO.check(23, Direction::Input), Err(PinError::Reserved));
        assert_eq!(PICO.check(24, Direction::Output), Err(PinError::InputOnly));
        assert_eq!(PICO.check(25, Direction::Output), Ok(()));
        assert_eq!(PICO.check(30, Direction::Input), Err(PinError::NoSuchPin));
        assert_eq!(PICO.check(255, Direction::Output), Err(PinError::NoSuchPin));
    }

    #[test]
    fn every_held_pin_has_a_reason() {
        for map in [&PICO, &ESP32, &ESP32S3, &ESP32C3, &ESP8266] {
            for p in map.gpios() {
                let held = map.is_reserved(p) || map.is_input_only(p);
                assert_eq!(map.reason(p).is_some(), held, "GPIO {p}");
            }
        }
        assert_eq!(ESP32.reason(7), Some("the SPI flash"));
        assert_eq!(PICO.reason(26), Some("the ADC (analog input only)"));
    }

    #[test]
    fn adc_pwm_and_i2c_maps() {
        assert_eq!(PICO.adc_channel(26), Ok(0));
        assert_eq!(PICO.adc_channel(28), Ok(2));
        assert_eq!(PICO.adc_channel(29), Err(AdcError::NotAnalog));
        assert_eq!(PICO.adc_channel(40), Err(AdcError::NoSuchPin));
        assert_eq!(ESP32.adc_channel(36), Ok(0), "ADC1_CH0");
        assert_eq!(ESP32.adc_channel(32), Ok(4), "ADC1_CH4");
        assert_eq!(ESP32.adc_channel(25), Err(AdcError::NotAnalog), "ADC2 is never offered");
        assert_eq!(ESP32S3.adc_channel(1), Ok(0));
        assert_eq!(ESP32S3.adc_mask(), span(1, 10));
        assert_eq!(ESP32.adc_mask(), span(32, 39));
        assert_eq!(PICO.i2c_pins(0), Some((4, 5)));
        assert_eq!(PICO.i2c_pins(1), Some((6, 7)));
        assert_eq!(PICO.i2c_pins(2), None);
        assert_eq!(ESP32.i2c_pins(0), Some((26, 27)));
        assert_eq!(ESP32S3.i2c_pins(0), Some((5, 6)));
        assert_eq!(ESP32S3.i2c_pins(1), None);
        assert_eq!(ESP32C3.i2c_pins(0), None);
        assert_eq!(ESP32.pwm_pins().count(), 4);
        assert_eq!(ESP32.pwm_pin(0, 1), Some(4));
        assert_eq!(ESP32.pwm_pin(1, 0), Some(13));
        assert_eq!(ESP32.pwm_pin(2, 0), None, "the ESP32-CAM wires timers 0-1");
        assert_eq!(ESP32.pwm_pin(0, 2), None, "two outputs per timer");
        assert_eq!(ESP32.pwm_pins().nth(3), Some(((1, 1), 14)));
        assert_eq!(ESP32S3.pwm_pin(3, 1), Some(18));
        assert_eq!(PICO.pwm_pins().count(), 0);
    }

    #[test]
    fn board_wired_pins_are_payload_pins() {
        for map in [&PICO, &ESP32, &ESP32S3, &ESP32C3, &ESP8266] {
            for bus in 0..2 {
                if let Some((sda, scl)) = map.i2c_pins(bus) {
                    assert_eq!(map.check(sda, Direction::Output), Ok(()));
                    assert_eq!(map.check(scl, Direction::Output), Ok(()));
                }
            }
            for (_, p) in map.pwm_pins() {
                assert_eq!(map.check(p, Direction::Output), Ok(()));
            }
        }
    }

    /// A four-element collector, so the tests need no allocator.
    mod heapless_free {
        #[derive(Debug, PartialEq, Eq, Default)]
        pub struct Vec4([u8; 4], usize);
        impl Vec4 {
            pub fn of(s: &[u8]) -> Self {
                s.iter().copied().collect()
            }
        }
        impl FromIterator<u8> for Vec4 {
            fn from_iter<I: IntoIterator<Item = u8>>(it: I) -> Self {
                let mut v = Vec4::default();
                for (i, x) in it.into_iter().take(4).enumerate() {
                    v.0[i] = x;
                    v.1 = i + 1;
                }
                v
            }
        }
    }
}
