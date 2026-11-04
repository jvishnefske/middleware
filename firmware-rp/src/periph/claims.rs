//! Pin claims: which pins the SPI bus and the two I2C buses hold (V18).
//!
//! A [`Claims`] value is one pin bitmask per [`Owner`]. Every transition is
//! a function from a claims value to a new one, which the device shell
//! applies only when it succeeds (and only then touches the hardware):
//!
//! * [`Claims::claim_spi`] takes the pins of a resolved
//!   [`SpiRoute`] (SCK, MOSI, MISO and the GPIO-driven chip select),
//!   replacing the bus's previous pins (re-opening reconfigures).
//! * [`Claims::claim_i2c`] takes a bus's board-mapped SDA and SCL
//!   ([`Profile::i2c_pins`]); re-opening an open bus keeps them.
//! * [`Claims::release`] drops one owner's pins (the shell returns them to
//!   inputs without pull).
//! * [`Claims::check_gpio`] guards the payload's `gpio_*` calls: a pin a bus
//!   holds is `Busy` until the payload stops.
//!
//! A pin is exclusive both ways. A bus cannot take a pin another bus holds
//! or one the payload configured with `gpio_output` / `gpio_input`
//! ([`Pins::mode`] not `Idle`): both are [`AbiError::Busy`]. The board
//! policy applies first ([`Profile::check_pin`]): reserved pins, the ADC
//! pins (GPIO 26–28, analog only) among them, are `Unsupported`, so a bus
//! never takes an analog pin and `adc_read` never meets a bus pin.
//!
//! esp-base's claims (`esp_base::periph::claims`) refuse a pin another
//! peripheral holds with `InvalidArgument` and let a peripheral take a pin
//! the payload drives as GPIO; on the RP base both are `Busy`, the ABI's
//! code for a resource in use.
//!
//! Everything is released when the payload stops, traps or is replaced:
//! the device starts each run from [`Claims::new`].

use middleware::wasm_abi::AbiError;

use super::func::SpiRoute;
use crate::chip::{Direction, Profile};
use crate::pins::{Mode, Pins};

/// Who holds a pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    /// SPI bus 0 (on SPI0 or SPI1, whichever its pins select).
    Spi,
    /// I2C bus `0` or `1`.
    I2c(u8),
}

/// Pin ownership for one payload run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Claims {
    spi: u32,
    i2c: [u32; 2],
}

const fn bit(pin: u8) -> u32 {
    if pin < 32 { 1u32 << pin } else { 0 }
}

/// The pins of `mask`, ascending.
pub fn pins(mask: u32) -> impl Iterator<Item = u8> {
    (0u8..32).filter(move |&p| mask & bit(p) != 0)
}

impl Claims {
    /// Nothing claimed.
    pub const fn new() -> Self {
        Claims { spi: 0, i2c: [0; 2] }
    }

    /// Who holds `pin`, if anyone.
    pub const fn owner(&self, pin: u8) -> Option<Owner> {
        let b = bit(pin);
        if b == 0 {
            None
        } else if self.spi & b != 0 {
            Some(Owner::Spi)
        } else if self.i2c[0] & b != 0 {
            Some(Owner::I2c(0))
        } else if self.i2c[1] & b != 0 {
            Some(Owner::I2c(1))
        } else {
            None
        }
    }

    /// The pins `owner` holds.
    pub fn pins_of(&self, owner: Owner) -> u32 {
        match owner {
            Owner::Spi => self.spi,
            Owner::I2c(bus) => self.i2c.get(usize::from(bus)).copied().unwrap_or(0),
        }
    }

    /// Every pin any bus holds.
    pub const fn all(&self) -> u32 {
        self.spi | self.i2c[0] | self.i2c[1]
    }

    /// The claims without `owner`'s pins.
    pub fn release(self, owner: Owner) -> Self {
        match owner {
            Owner::Spi => Claims { spi: 0, ..self },
            Owner::I2c(bus) => {
                let mut i2c = self.i2c;
                if let Some(m) = i2c.get_mut(usize::from(bus)) {
                    *m = 0;
                }
                Claims { i2c, ..self }
            }
        }
    }

    /// Whether a `gpio_*` call may touch `pin`: `Busy` while a bus holds it.
    pub const fn check_gpio(&self, pin: u8) -> Result<(), AbiError> {
        match self.owner(pin) {
            Some(_) => Err(AbiError::Busy),
            None => Ok(()),
        }
    }

    /// Checks `wanted` (pin, direction) pairs for `owner`: the board policy,
    /// then no other owner and no payload GPIO; returns their mask.
    fn check(
        &self,
        profile: &Profile,
        gpio: &Pins,
        owner: Owner,
        wanted: &[(Option<u8>, Direction)],
    ) -> Result<u32, AbiError> {
        let mut mask = 0u32;
        for &(pin, dir) in wanted {
            let Some(pin) = pin else { continue };
            profile.check_pin(pin, dir)?;
            match self.owner(pin) {
                Some(o) if o != owner => return Err(AbiError::Busy),
                Some(_) => {}
                None if gpio.mode(pin) != Mode::Idle => return Err(AbiError::Busy),
                None => {}
            }
            mask |= bit(pin);
        }
        Ok(mask)
    }

    /// Claims an SPI route's pins, replacing the bus's previous ones.
    pub fn claim_spi(self, profile: &Profile, gpio: &Pins, route: &SpiRoute) -> Result<Self, AbiError> {
        let spi = self.check(
            profile,
            gpio,
            Owner::Spi,
            &[
                (Some(route.sck), Direction::Output),
                (route.mosi, Direction::Output),
                (route.miso, Direction::Input),
                (route.cs, Direction::Output),
            ],
        )?;
        Ok(Claims { spi, ..self })
    }

    /// Claims I2C `bus`'s board-mapped SDA and SCL (both driven open drain
    /// and read back); returns the new claims and `(sda, scl)`.
    pub fn claim_i2c(self, profile: &Profile, gpio: &Pins, bus: u8) -> Result<(Self, (u8, u8)), AbiError> {
        let (sda, scl) = profile.i2c_pins(bus)?;
        if sda == scl {
            return Err(AbiError::InvalidArgument);
        }
        let mask = self.check(
            profile,
            gpio,
            Owner::I2c(bus),
            &[(Some(sda), Direction::Output), (Some(scl), Direction::Output)],
        )?;
        let mut i2c = self.i2c;
        *i2c.get_mut(usize::from(bus)).ok_or(AbiError::Unsupported)? = mask;
        Ok((Claims { i2c, ..self }, (sda, scl)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chip::{RP2040, RP2350};
    use middleware::wasm_abi::periph::SpiConfig;

    fn route(p: &Profile, sck: i32, mosi: i32, miso: i32, cs: i32) -> SpiRoute {
        SpiRoute::resolve(p, &SpiConfig::parse(0, sck, mosi, miso, cs, 1000, 0).unwrap()).unwrap()
    }

    fn list(mask: u32) -> heapless::Vec<u8, 8> {
        pins(mask).collect()
    }

    #[test]
    fn v18_buses_hold_their_pins_and_gpio_is_busy_on_them() {
        for p in [&RP2040, &RP2350] {
            let gpio = Pins::new();
            let c = Claims::new().claim_spi(p, &gpio, &route(p, 18, 19, 16, 17)).unwrap();
            assert_eq!(list(c.pins_of(Owner::Spi)).as_slice(), &[16, 17, 18, 19]);
            let (c, pins_0) = c.claim_i2c(p, &gpio, 0).unwrap();
            assert_eq!(pins_0, (4, 5));
            let (c, _) = c.claim_i2c(p, &gpio, 1).unwrap();
            assert_eq!(c.owner(4), Some(Owner::I2c(0)));
            assert_eq!(c.owner(7), Some(Owner::I2c(1)));
            assert_eq!(c.owner(17), Some(Owner::Spi), "the chip select");
            assert_eq!(list(c.all()).len(), 8);
            for pin in [4, 5, 6, 7, 16, 17, 18, 19] {
                assert_eq!(c.check_gpio(pin), Err(AbiError::Busy), "GPIO {pin}");
            }
            assert_eq!(c.check_gpio(8), Ok(()));
            assert_eq!(c.owner(200), None);
            // Re-opening an I2C bus keeps its pins.
            let (again, _) = c.claim_i2c(p, &gpio, 0).unwrap();
            assert_eq!(again, c);
        }
    }

    #[test]
    fn v18_pins_are_exclusive_between_buses_and_gpio() {
        let p = &RP2350;
        let gpio = Pins::new();
        let (c, _) = Claims::new().claim_i2c(p, &gpio, 0).unwrap();
        // SPI0 RX on GPIO 4 is I2C bus 0's SDA.
        assert_eq!(c.claim_spi(p, &gpio, &route(p, 6, 7, 4, -1)), Err(AbiError::Busy));
        // CS on I2C bus 0's SCL.
        assert_eq!(c.claim_spi(p, &gpio, &route(p, 18, 19, 16, 5)), Err(AbiError::Busy));
        let c = c.claim_spi(p, &gpio, &route(p, 2, 3, 0, 1)).unwrap();
        // I2C bus 1 is free, but not if SPI held its pins.
        let c2 = Claims::new().claim_spi(p, &gpio, &route(p, 6, 7, -1, -1)).unwrap();
        assert_eq!(c2.claim_i2c(p, &gpio, 1), Err(AbiError::Busy));
        // A pin the payload configured as GPIO cannot become a bus pin.
        let gpio = Pins::new().output(p, 19).unwrap().input(p, 6).unwrap();
        assert_eq!(Claims::new().claim_spi(p, &gpio, &route(p, 18, 19, 16, -1)), Err(AbiError::Busy));
        assert_eq!(Claims::new().claim_i2c(p, &gpio, 1), Err(AbiError::Busy));
        assert!(Claims::new().claim_i2c(p, &gpio, 0).is_ok());
        // A failed claim leaves the previous value untouched (values).
        assert_eq!(c.owner(4), Some(Owner::I2c(0)));
        assert_eq!(c.owner(0), Some(Owner::Spi));
        // Bus 2 does not exist on this board map.
        assert_eq!(Claims::new().claim_i2c(p, &Pins::new(), 2), Err(AbiError::Unsupported));
    }

    #[test]
    fn v18_reopening_spi_replaces_its_pins_and_release_frees_one_owner() {
        let p = &RP2040;
        let gpio = Pins::new();
        let c = Claims::new().claim_spi(p, &gpio, &route(p, 18, 19, 16, 17)).unwrap();
        let (c, _) = c.claim_i2c(p, &gpio, 1).unwrap();
        let c = c.claim_spi(p, &gpio, &route(p, 10, 11, -1, 9)).unwrap();
        assert_eq!(list(c.pins_of(Owner::Spi)).as_slice(), &[9, 10, 11]);
        assert_eq!(c.owner(18), None);
        assert_eq!(c.check_gpio(16), Ok(()));
        // The new pins may overlap the old ones (the bus's own).
        let c = c.claim_spi(p, &gpio, &route(p, 10, 11, 12, -1)).unwrap();
        assert_eq!(list(c.pins_of(Owner::Spi)).as_slice(), &[10, 11, 12]);
        let freed = c.release(Owner::Spi);
        assert_eq!(freed.pins_of(Owner::Spi), 0);
        assert_eq!(freed.owner(6), Some(Owner::I2c(1)), "I2C keeps its pins");
        let none = freed.release(Owner::I2c(1));
        assert_eq!(none, Claims::new());
        assert_eq!(none.release(Owner::I2c(7)), Claims::new(), "no such bus");
    }

    #[test]
    fn v18_analog_and_reserved_pins_never_become_bus_pins() {
        for p in [&RP2040, &RP2350] {
            let gpio = Pins::new();
            // GPIO 26-28 are SPI1 / I2C1 capable but analog only on the board.
            for (sck, mosi, miso) in [(26, 27, 28), (14, 27, -1), (14, 15, 28)] {
                let cfg = SpiConfig::parse(0, sck, mosi, miso, -1, 1000, 0).unwrap();
                assert_eq!(SpiRoute::resolve(p, &cfg), Err(AbiError::Unsupported), "{sck} {mosi} {miso}");
            }
            // The chip select must be able to drive.
            let cfg = SpiConfig::parse(0, 18, 19, -1, 23, 1000, 0).unwrap();
            assert_eq!(SpiRoute::resolve(p, &cfg), Err(AbiError::Unsupported));
            let c = Claims::new().claim_i2c(p, &gpio, 0).unwrap().0;
            for adc in [26, 27, 28] {
                assert_eq!(c.owner(adc), None);
                assert!(p.adc_channel(adc).is_ok());
            }
        }
    }
}
