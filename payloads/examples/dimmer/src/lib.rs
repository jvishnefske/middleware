//! Dimmer: an ABI 3 example driving a PWM output from an analog input and
//! reading an I2C temperature sensor.
//!
//! * `init` opens PWM timer [`PWM_TIMER`] at [`PWM_HZ`], I2C bus
//!   [`I2C_BUS`] at [`I2C_HZ`], and ticks every [`TICK_MS`] ms.
//! * Each tick reads ADC pin [`POT_PIN`] (a potentiometer), sets PWM
//!   channel [`PWM_CHANNEL`] to the matching duty ([`duty_from_adc`],
//!   through the `embedded-hal` `SetDutyCycle` trait), and publishes the
//!   raw reading on [`ADC_KEY`] as two little-endian bytes.
//! * It then reads the two-byte temperature register of an LM75-style
//!   sensor at [`SENSOR_ADDR`] (through the `embedded-hal` `I2c` trait,
//!   [`read_register`]) and publishes it on [`TEMP_KEY`]. A missing sensor
//!   is logged once.
//!
//! The board maps the PWM channel, the I2C bus and the ADC pin to physical
//! pins. In `payload-sim`, set the potentiometer with `!adc 0 <0..4095>`;
//! the simulator's I2C device sits at 0x50, so the sensor read reports a
//! missing acknowledge.
#![cfg_attr(not(test), no_std)]
#![deny(unsafe_code)]

use core::fmt::Write as _;

use embedded_hal::pwm::SetDutyCycle;
use mw_payload::adc::{ADC_MAX, AnalogPin};
use mw_payload::i2c::I2c;
use mw_payload::pwm::{Pwm, PwmChannel};
use mw_payload::{AbiError, LogLine, Payload};

/// PWM timer driving the output.
pub const PWM_TIMER: u8 = 0;
/// PWM channel of the output.
pub const PWM_CHANNEL: u8 = 0;
/// PWM frequency.
pub const PWM_HZ: u32 = 1_000;
/// Analog pin of the potentiometer.
pub const POT_PIN: u8 = 0;
/// I2C bus of the sensor.
pub const I2C_BUS: u8 = 0;
/// I2C clock.
pub const I2C_HZ: u32 = 100_000;
/// 7-bit address of the LM75-style temperature sensor.
pub const SENSOR_ADDR: u8 = 0x48;
/// Temperature register of the sensor.
pub const TEMP_REG: u8 = 0;
/// Tick period.
pub const TICK_MS: u32 = 100;
/// Key the raw ADC reading is published on.
pub const ADC_KEY: &str = "app/dimmer/adc";
/// Key the raw temperature register is published on.
pub const TEMP_KEY: &str = "app/dimmer/temp";

/// Scales a 12-bit reading to a full-range 16-bit duty: 0 stays 0 and
/// [`ADC_MAX`] becomes `u16::MAX` (the top bits are replicated into the
/// bottom four). Readings above [`ADC_MAX`] count as full scale.
pub const fn duty_from_adc(raw: u16) -> u16 {
    let raw = if raw > ADC_MAX { ADC_MAX } else { raw };
    (raw << 4) | (raw >> 8)
}

/// Reads a two-byte register with one write-read through any
/// `embedded-hal` I2C bus.
pub fn read_register<B: embedded_hal::i2c::I2c>(
    bus: &mut B,
    addr: u8,
    reg: u8,
) -> Result<[u8; 2], B::Error> {
    let mut out = [0u8; 2];
    bus.write_read(addr, &[reg], &mut out)?;
    Ok(out)
}

/// The application state.
pub struct Dimmer {
    output: Option<PwmChannel>,
    pot: AnalogPin,
    bus: Option<I2c>,
    sensor_missing: bool,
}

/// Logs `what` failed with `e`.
fn log_err(what: &str, e: AbiError) {
    let mut line = LogLine::new();
    let _ = write!(line, "{what} failed: {e:?}");
    line.log();
}

fn open_output() -> Result<PwmChannel, AbiError> {
    Pwm::open(PWM_TIMER, PWM_HZ)?.channel(PWM_CHANNEL)
}

impl Payload for Dimmer {
    fn init() -> Self {
        let output = open_output().map_err(|e| log_err("pwm", e)).ok();
        let bus = I2c::open(I2C_BUS, I2C_HZ)
            .map_err(|e| log_err("i2c", e))
            .ok();
        if let Err(e) = mw_payload::set_tick(TICK_MS) {
            log_err("set_tick", e);
        }
        Dimmer {
            output,
            pot: AnalogPin::new(POT_PIN),
            bus,
            sensor_missing: false,
        }
    }

    fn tick(&mut self, _now_ms: u32) {
        if let Ok(raw) = self.pot.read() {
            if let Some(out) = self.output.as_mut() {
                let _ = out.set_duty_cycle(duty_from_adc(raw));
            }
            let _ = mw_payload::publish(ADC_KEY, &raw.to_le_bytes());
        }
        let Some(bus) = self.bus.as_mut() else {
            return;
        };
        match read_register(bus, SENSOR_ADDR, TEMP_REG) {
            Ok(temp) => {
                self.sensor_missing = false;
                let _ = mw_payload::publish(TEMP_KEY, &temp);
            }
            Err(e) if !self.sensor_missing => {
                self.sensor_missing = true;
                log_err("sensor", e.0);
            }
            Err(_) => {}
        }
    }
}

// The exports need a single-threaded wasm32 module; host builds only run
// the unit tests below.
#[cfg(target_arch = "wasm32")]
mw_payload::payload!(Dimmer);

#[cfg(test)]
mod tests {
    use super::*;
    use embedded_hal::i2c::{ErrorKind, ErrorType, Operation};

    #[test]
    fn duty_spans_the_full_range() {
        assert_eq!(duty_from_adc(0), 0);
        assert_eq!(duty_from_adc(ADC_MAX), u16::MAX);
        assert_eq!(duty_from_adc(2048), 0x8008);
        assert_eq!(duty_from_adc(u16::MAX), u16::MAX);
    }

    /// A bus whose target at 0x48 answers any register with `[0x19, 0x80]`.
    struct Lm75;

    impl ErrorType for Lm75 {
        type Error = ErrorKind;
    }

    impl embedded_hal::i2c::I2c for Lm75 {
        fn transaction(&mut self, addr: u8, ops: &mut [Operation<'_>]) -> Result<(), ErrorKind> {
            if addr != SENSOR_ADDR {
                return Err(ErrorKind::Other);
            }
            for op in ops {
                if let Operation::Read(b) = op {
                    b.copy_from_slice(&[0x19, 0x80][..b.len()]);
                }
            }
            Ok(())
        }
    }

    #[test]
    fn register_read_uses_one_write_read() {
        assert_eq!(read_register(&mut Lm75, SENSOR_ADDR, 0), Ok([0x19, 0x80]));
        assert_eq!(read_register(&mut Lm75, 0x49, 0), Err(ErrorKind::Other));
    }
}
