//! Switching a payload pin between GPIO and a bus function.
//!
//! Every payload pin stays wrapped in its embassy-rp `Flex` in the pin
//! bank for the life of the firmware (the GPIO calls drive it through SIO).
//! When a bus takes a pin, [`route`] selects the pin's SPI (F1) or I2C
//! (F3) function in `IO_BANK0` and sets its pad; [`to_gpio`] selects SIO
//! again with the pad as `Flex::new` leaves it, and the caller then makes
//! the `Flex` an input without pull. Which pin may take which function is
//! decided before, in `rp_base::periph` (function tables, claims); these
//! are plain register writes through embassy-rp's PAC (safe accessors, no
//! `unsafe`), on pins below 30 only.

use embassy_rp::pac;
use rp_base::periph::func::TABLE_GPIOS;

/// A bus function of a pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Function {
    /// F1: SPI (SCK, TX, RX).
    Spi,
    /// F3: I2C (SDA, SCL), with the pad's pull-up on.
    I2c,
}

/// `FUNCSEL` of SIO (F5) on both chips.
const FUNC_SIO: u8 = 5;

impl Function {
    const fn funcsel(self) -> u8 {
        match self {
            Function::Spi => 1,
            Function::I2c => 3,
        }
    }
}

/// Connects `pin` to `function`: input enabled, Schmitt trigger, no
/// pull (I2C: pull-up). A pin past GPIO 29 is ignored.
pub fn route(pin: u8, function: Function) {
    if pin >= TABLE_GPIOS {
        return;
    }
    let n = usize::from(pin);
    pac::PADS_BANK0.gpio(n).write(|w| {
        #[cfg(feature = "rp2350")]
        w.set_iso(false);
        w.set_schmitt(true);
        w.set_slewfast(false);
        w.set_ie(true);
        w.set_od(false);
        w.set_pue(function == Function::I2c);
        w.set_pde(false);
    });
    pac::IO_BANK0.gpio(n).ctrl().write(|w| w.set_funcsel(function.funcsel()));
}

/// Gives `pin` back to SIO, the pad as `Flex::new` sets it. A pin past
/// GPIO 29 is ignored.
pub fn to_gpio(pin: u8) {
    if pin >= TABLE_GPIOS {
        return;
    }
    let n = usize::from(pin);
    pac::PADS_BANK0.gpio(n).write(|w| {
        #[cfg(feature = "rp2350")]
        w.set_iso(false);
        w.set_ie(true);
    });
    pac::IO_BANK0.gpio(n).ctrl().write(|w| w.set_funcsel(FUNC_SIO));
}
