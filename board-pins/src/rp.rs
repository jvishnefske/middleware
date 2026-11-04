//! The RP2040 and RP2350 GPIO function tables for SPI and I2C.
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
    let block = if (pin / 8).is_multiple_of(2) {
        SpiBlock::Spi0
    } else {
        SpiBlock::Spi1
    };
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
    let block = if (pin / 2).is_multiple_of(2) {
        I2cBlock::I2c0
    } else {
        I2cBlock::I2c1
    };
    let line = if pin.is_multiple_of(2) {
        I2cLine::Sda
    } else {
        I2cLine::Scl
    };
    Some((block, line))
}

/// The GPIOs whose F1 function is `signal` on `block`, ascending.
pub fn spi_pins(block: SpiBlock, signal: SpiSignal) -> impl Iterator<Item = u8> {
    (0..TABLE_GPIOS).filter(move |&p| spi_function(p) == Some((block, signal)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[rustfmt::skip]
    fn function_tables_match_the_datasheets() {
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
        // Each group of four GPIOs carries RX, CSn, SCK, TX in that order.
        for group in 0..7u8 {
            for (i, s) in [Rx, Csn, Sck, Tx].into_iter().enumerate() {
                let pin = group * 4 + i as u8;
                if pin < TABLE_GPIOS {
                    assert_eq!(spi_function(pin).map(|f| f.1), Some(s), "GPIO {pin}");
                }
            }
        }
    }

    #[test]
    fn spi_pin_lists() {
        let mut sck0 = [0u8; 4];
        let mut n = 0;
        for p in spi_pins(SpiBlock::Spi0, SpiSignal::Sck) {
            sck0[n] = p;
            n += 1;
        }
        assert_eq!((n, sck0), (4, [2, 6, 18, 22]));
        assert_eq!(spi_pins(SpiBlock::Spi1, SpiSignal::Sck).count(), 3);
        assert_eq!(spi_pins(SpiBlock::Spi1, SpiSignal::Rx).count(), 4);
    }
}
