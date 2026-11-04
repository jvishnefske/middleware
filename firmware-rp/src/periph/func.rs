//! The RP2040 and RP2350 GPIO function tables for SPI and I2C, and the
//! resolution of a payload's SPI pins (V15).
//!
//! Both chips route each peripheral signal to a fixed set of GPIOs (no
//! matrix as on the ESP32): function F1 (SPI) and F3 (I2C) of the datasheets'
//! GPIO function tables (RP2040 datasheet §2.19.2, RP2350 datasheet §9.4).
//! For GPIO 0–29 the two tables agree:
//!
//! | GPIO `n` | SPI block           | SPI signal (`n % 4`)           | I2C block      | I2C line (`n % 2`) |
//! |----------|---------------------|--------------------------------|----------------|--------------------|
//! | 0–7      | SPI0                | 0 RX, 1 CSn, 2 SCK, 3 TX       | `(n / 2) % 2`  | 0 SDA, 1 SCL       |
//! | 8–15     | SPI1                | same                           | same           | same               |
//! | 16–23    | SPI0                | same                           | same           | same               |
//! | 24–29    | SPI1                | same                           | same           | same               |
//!
//! So SPI0's SCK is on GPIO 2, 6, 18 or 22, its TX (MOSI) on 3, 7, 19 or
//! 23, its RX (MISO) on 0, 4, 16 or 20; SPI1's SCK on 10, 14 or 26, TX on
//! 11, 15 or 27, RX on 8, 12, 24 or 28. I2C0 has SDA on 0, 4, 8, 12, 16,
//! 20, 24, 28 and SCL one above each; I2C1 SDA on 2, 6, 10, 14, 18, 22,
//! 26 and SCL one above each.
//!
//! [`SpiRoute::resolve`] maps a validated [`SpiConfig`] onto one block:
//! SCK, MOSI and MISO must each sit on a GPIO with that signal, all on the
//! same block (the block is whichever SCK selects). The chip select is not
//! the block's CSn: the shell drives it as a plain output for the whole
//! transfer (see [`super::spi`]), so any payload output pin may be CS.
//! Errors follow the board policy ([`Profile::check_pin`]): a pin the chip
//! lacks is `InvalidArgument`, a reserved pin (or an output on an
//! input-only pin) `Unsupported`; a pin without the needed signal, or
//! signals on two different blocks, is `Unsupported` (the hardware cannot
//! do it).

use middleware::wasm_abi::AbiError;
use middleware::wasm_abi::periph::SpiConfig;

use crate::chip::{Direction, Profile};

/// One of the two SPI controllers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpiBlock {
    /// SPI0.
    Spi0,
    /// SPI1.
    Spi1,
}

/// An SPI signal a GPIO can carry (function F1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpiSignal {
    /// Controller in (MISO).
    Rx,
    /// The block's hardware chip select (not used: CS is driven as GPIO).
    Csn,
    /// Clock.
    Sck,
    /// Controller out (MOSI).
    Tx,
}

/// One of the two I2C controllers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum I2cBlock {
    /// I2C0.
    I2c0,
    /// I2C1.
    I2c1,
}

/// An I2C line a GPIO can carry (function F3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum I2cLine {
    /// Data.
    Sda,
    /// Clock.
    Scl,
}

/// GPIOs covered by the tables (both chips have GPIO 0–29).
pub const TABLE_GPIOS: u8 = 30;

/// The SPI block and signal of `pin`'s F1 function; `None` past GPIO 29.
pub const fn spi_function(pin: u8) -> Option<(SpiBlock, SpiSignal)> {
    if pin >= TABLE_GPIOS {
        return None;
    }
    let block = if (pin / 8).is_multiple_of(2) { SpiBlock::Spi0 } else { SpiBlock::Spi1 };
    let signal = match pin % 4 {
        0 => SpiSignal::Rx,
        1 => SpiSignal::Csn,
        2 => SpiSignal::Sck,
        _ => SpiSignal::Tx,
    };
    Some((block, signal))
}

/// The I2C block and line of `pin`'s F3 function; `None` past GPIO 29.
pub const fn i2c_function(pin: u8) -> Option<(I2cBlock, I2cLine)> {
    if pin >= TABLE_GPIOS {
        return None;
    }
    let block = if (pin / 2).is_multiple_of(2) { I2cBlock::I2c0 } else { I2cBlock::I2c1 };
    let line = if pin.is_multiple_of(2) { I2cLine::Sda } else { I2cLine::Scl };
    Some((block, line))
}

/// A payload's SPI pins resolved onto one block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpiRoute {
    /// The controller the pins select.
    pub block: SpiBlock,
    /// Clock pin (F1 SCK).
    pub sck: u8,
    /// Controller-out pin (F1 TX), if any.
    pub mosi: Option<u8>,
    /// Controller-in pin (F1 RX), if any.
    pub miso: Option<u8>,
    /// Chip select, driven as a GPIO output, if any.
    pub cs: Option<u8>,
}

impl SpiRoute {
    /// Resolves `cfg`'s pins on `profile` (see the module docs).
    pub fn resolve(profile: &Profile, cfg: &SpiConfig) -> Result<Self, AbiError> {
        profile.check_pin(cfg.sck, Direction::Output)?;
        let (block, _) = signal_on(cfg.sck, SpiSignal::Sck)?;
        let on_block = |pin: Option<u8>, signal: SpiSignal, dir: Direction| -> Result<(), AbiError> {
            let Some(pin) = pin else { return Ok(()) };
            profile.check_pin(pin, dir)?;
            match signal_on(pin, signal)? {
                (b, _) if b == block => Ok(()),
                _ => Err(AbiError::Unsupported),
            }
        };
        on_block(cfg.mosi, SpiSignal::Tx, Direction::Output)?;
        on_block(cfg.miso, SpiSignal::Rx, Direction::Input)?;
        if let Some(cs) = cfg.cs {
            profile.check_pin(cs, Direction::Output)?;
        }
        Ok(SpiRoute { block, sck: cfg.sck, mosi: cfg.mosi, miso: cfg.miso, cs: cfg.cs })
    }

    /// The pins the bus routes to the SPI block (SCK, MOSI, MISO).
    pub fn data_pins(&self) -> impl Iterator<Item = u8> {
        [Some(self.sck), self.mosi, self.miso].into_iter().flatten()
    }

    /// Every pin the bus holds, chip select included.
    pub fn pins(&self) -> impl Iterator<Item = u8> {
        [Some(self.sck), self.mosi, self.miso, self.cs].into_iter().flatten()
    }
}

/// `pin`'s F1 function if it is `signal`, else `Unsupported`.
fn signal_on(pin: u8, signal: SpiSignal) -> Result<(SpiBlock, SpiSignal), AbiError> {
    match spi_function(pin) {
        Some((block, s)) if s == signal => Ok((block, s)),
        _ => Err(AbiError::Unsupported),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chip::{RP2040, RP2350};

    fn cfg(sck: i32, mosi: i32, miso: i32, cs: i32) -> SpiConfig {
        SpiConfig::parse(0, sck, mosi, miso, cs, 1000, 0).unwrap()
    }

    #[test]
    fn v15_function_tables_match_the_datasheets() {
        // Spot checks against the RP2040 / RP2350 GPIO function tables.
        use I2cBlock::*;
        use I2cLine::*;
        use SpiBlock::*;
        use SpiSignal::*;
        let spi = [
            (0, Spi0, Rx), (1, Spi0, Csn), (2, Spi0, Sck), (3, Spi0, Tx),
            (4, Spi0, Rx), (7, Spi0, Tx), (8, Spi1, Rx), (10, Spi1, Sck),
            (11, Spi1, Tx), (15, Spi1, Tx), (16, Spi0, Rx), (17, Spi0, Csn),
            (18, Spi0, Sck), (19, Spi0, Tx), (22, Spi0, Sck), (24, Spi1, Rx),
            (26, Spi1, Sck), (27, Spi1, Tx), (28, Spi1, Rx), (29, Spi1, Csn),
        ];
        for (pin, block, signal) in spi {
            assert_eq!(spi_function(pin), Some((block, signal)), "GPIO {pin}");
        }
        let i2c = [
            (0, I2c0, Sda), (1, I2c0, Scl), (2, I2c1, Sda), (3, I2c1, Scl),
            (4, I2c0, Sda), (5, I2c0, Scl), (6, I2c1, Sda), (7, I2c1, Scl),
            (20, I2c0, Sda), (21, I2c0, Scl), (26, I2c1, Sda), (29, I2c0, Scl),
        ];
        for (pin, block, line) in i2c {
            assert_eq!(i2c_function(pin), Some((block, line)), "GPIO {pin}");
        }
        assert_eq!(spi_function(30), None);
        assert_eq!(i2c_function(255), None);
        // Each SPI block has exactly one signal of each kind per group of four.
        for group in 0..7u8 {
            let signals: heapless::Vec<_, 4> =
                (group * 4..group * 4 + 4).filter_map(spi_function).map(|(_, s)| s).collect();
            assert_eq!(signals.as_slice(), &[Rx, Csn, Sck, Tx], "GPIO {}..", group * 4);
        }
    }

    #[test]
    fn v15_spi_pins_resolve_onto_one_block_on_both_chips() {
        for p in [&RP2040, &RP2350] {
            // The Pico's default SPI0 pins.
            let r = SpiRoute::resolve(p, &cfg(18, 19, 16, 17)).unwrap();
            assert_eq!(r.block, SpiBlock::Spi0);
            assert_eq!(r.pins().collect::<heapless::Vec<u8, 4>>().as_slice(), &[18, 19, 16, 17]);
            assert_eq!(r.data_pins().count(), 3);
            // SPI1, transmit only, no CS.
            let r = SpiRoute::resolve(p, &cfg(10, 11, -1, -1)).unwrap();
            assert_eq!((r.block, r.miso, r.cs), (SpiBlock::Spi1, None, None));
            // Receive only.
            assert_eq!(SpiRoute::resolve(p, &cfg(14, -1, 12, -1)).unwrap().block, SpiBlock::Spi1);
            // Any output pin may be the (GPIO-driven) chip select, LED included.
            assert!(SpiRoute::resolve(p, &cfg(2, 3, 0, 25)).is_ok());
            assert!(SpiRoute::resolve(p, &cfg(2, 3, 0, 9)).is_ok());
            // A pin without the signal.
            assert_eq!(SpiRoute::resolve(p, &cfg(19, 18, -1, -1)), Err(AbiError::Unsupported), "swapped");
            assert_eq!(SpiRoute::resolve(p, &cfg(17, 19, -1, -1)), Err(AbiError::Unsupported), "CSn as SCK");
            assert_eq!(SpiRoute::resolve(p, &cfg(18, 19, 17, -1)), Err(AbiError::Unsupported), "CSn as MISO");
            // Signals on two blocks.
            assert_eq!(SpiRoute::resolve(p, &cfg(18, 11, -1, -1)), Err(AbiError::Unsupported));
            assert_eq!(SpiRoute::resolve(p, &cfg(10, 11, 16, -1)), Err(AbiError::Unsupported));
            // The board policy applies first: reserved, input-only, missing.
            assert_eq!(SpiRoute::resolve(p, &cfg(26, 27, -1, -1)), Err(AbiError::Unsupported), "ADC pins");
            assert_eq!(SpiRoute::resolve(p, &cfg(22, 23, -1, -1)), Err(AbiError::Unsupported), "GPIO 23");
            assert_eq!(SpiRoute::resolve(p, &cfg(18, 19, 16, 24)), Err(AbiError::Unsupported), "CS on input-only 24");
            assert!(SpiRoute::resolve(p, &cfg(14, 15, 24, -1)).is_ok(), "MISO may be input-only 24");
            assert_eq!(SpiRoute::resolve(p, &cfg(18, 19, 16, 30)), Err(AbiError::InvalidArgument));
        }
    }
}
