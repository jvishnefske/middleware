//! Resolving a payload's SPI pins on a board ([`route`]).
//!
//! Each pin is checked in the direction its signal needs (SCK, MOSI and CS
//! drive the pin; MISO reads it) with [`PinMap::check`]. On the RP chips
//! ([`SpiRouting::RpFunctionTable`]) SCK, MOSI and MISO must also sit on a
//! GPIO carrying that signal in the F1 table ([`crate::rp::spi_function`]),
//! all on the block SCK selects; CS is driven as a plain output, so any
//! output pin may be CS. On the ESP chips ([`SpiRouting::Matrix`]) the GPIO
//! matrix reaches any pin, so the direction check is all.
//!
//! The pins are checked in a fixed order (SCK, MOSI, MISO, CS; for each,
//! the board policy first, then the function table) and the first problem
//! is returned, so the firmware's error code for a given request never
//! depends on anything but the request.

use crate::rp::{SpiBlock, SpiSignal, spi_function};
use crate::{Direction, PinError, PinMap, SpiRouting};

/// The pins of one SPI bus: SCK and any of MOSI, MISO and CS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpiPins {
    /// Clock.
    pub sck: u8,
    /// Controller out, if any.
    pub mosi: Option<u8>,
    /// Controller in, if any.
    pub miso: Option<u8>,
    /// Chip select, if any.
    pub cs: Option<u8>,
}

/// A pin's role on the bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpiRole {
    /// Clock.
    Sck,
    /// Controller out.
    Mosi,
    /// Controller in.
    Miso,
    /// Chip select.
    Cs,
}

impl SpiRole {
    /// The direction the role drives the pin in.
    pub const fn direction(self) -> Direction {
        match self {
            SpiRole::Miso => Direction::Input,
            _ => Direction::Output,
        }
    }

    /// The F1 signal the role needs on the RP chips (`None` for CS, which
    /// is a plain GPIO output).
    pub const fn signal(self) -> Option<SpiSignal> {
        match self {
            SpiRole::Sck => Some(SpiSignal::Sck),
            SpiRole::Mosi => Some(SpiSignal::Tx),
            SpiRole::Miso => Some(SpiSignal::Rx),
            SpiRole::Cs => None,
        }
    }
}

/// What is wrong with one pin of the bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpiProblem {
    /// The board policy refuses the pin in the role's direction.
    Pin(PinError),
    /// The pin does not carry the role's signal (RP function table).
    NoSignal,
    /// The pin carries the signal on the other block than SCK's.
    OtherBlock {
        /// The block SCK selects.
        sck: SpiBlock,
        /// The block this pin's signal belongs to.
        pin: SpiBlock,
    },
}

/// The first problem found with a bus's pins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpiError {
    /// The role of the pin at fault.
    pub role: SpiRole,
    /// The pin.
    pub pin: u8,
    /// What is wrong with it.
    pub problem: SpiProblem,
}

/// Checks `pins` on `map` (see the module docs). On the RP chips returns
/// the block the bus uses; through the ESP matrix, `None`.
pub fn route(map: &PinMap, pins: &SpiPins) -> Result<Option<SpiBlock>, SpiError> {
    let sck_block = check_role(map, SpiRole::Sck, pins.sck, None)?;
    if let Some(p) = pins.mosi {
        check_role(map, SpiRole::Mosi, p, sck_block)?;
    }
    if let Some(p) = pins.miso {
        check_role(map, SpiRole::Miso, p, sck_block)?;
    }
    if let Some(p) = pins.cs {
        check_role(map, SpiRole::Cs, p, sck_block)?;
    }
    Ok(sck_block)
}

/// Checks one pin in `role`: the board policy, then (RP) the function
/// table and the block, which must be `block` when given. Returns the
/// pin's block on the RP chips (for SCK, the bus's block).
pub fn check_role(
    map: &PinMap,
    role: SpiRole,
    pin: u8,
    block: Option<SpiBlock>,
) -> Result<Option<SpiBlock>, SpiError> {
    let fail = |problem| SpiError { role, pin, problem };
    map.check(pin, role.direction())
        .map_err(|e| fail(SpiProblem::Pin(e)))?;
    let (SpiRouting::RpFunctionTable, Some(signal)) = (map.spi_routing(), role.signal()) else {
        return Ok(block);
    };
    match spi_function(pin) {
        Some((b, s)) if s == signal => match block {
            Some(sck) if sck != b => Err(fail(SpiProblem::OtherBlock { sck, pin: b })),
            _ => Ok(Some(b)),
        },
        _ => Err(fail(SpiProblem::NoSignal)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boards::{ESP32, ESP32S3, PICO};

    fn p(sck: u8, mosi: Option<u8>, miso: Option<u8>, cs: Option<u8>) -> SpiPins {
        SpiPins {
            sck,
            mosi,
            miso,
            cs,
        }
    }

    fn err(role: SpiRole, pin: u8, problem: SpiProblem) -> Result<Option<SpiBlock>, SpiError> {
        Err(SpiError { role, pin, problem })
    }

    #[test]
    fn rp_pins_resolve_onto_one_block() {
        use SpiBlock::*;
        // The Pico's default SPI0 pins.
        assert_eq!(
            route(&PICO, &p(18, Some(19), Some(16), Some(17))),
            Ok(Some(Spi0))
        );
        assert_eq!(route(&PICO, &p(10, Some(11), None, None)), Ok(Some(Spi1)));
        assert_eq!(route(&PICO, &p(14, None, Some(12), None)), Ok(Some(Spi1)));
        // Any output pin may be the (GPIO-driven) chip select, LED included.
        assert_eq!(
            route(&PICO, &p(2, Some(3), Some(0), Some(25))),
            Ok(Some(Spi0))
        );
        // A pin without the signal.
        assert_eq!(
            route(&PICO, &p(19, Some(18), None, None)),
            err(SpiRole::Sck, 19, SpiProblem::NoSignal)
        );
        assert_eq!(
            route(&PICO, &p(17, Some(19), None, None)),
            err(SpiRole::Sck, 17, SpiProblem::NoSignal)
        );
        assert_eq!(
            route(&PICO, &p(18, Some(19), Some(17), None)),
            err(SpiRole::Miso, 17, SpiProblem::NoSignal)
        );
        // Signals on two blocks.
        assert_eq!(
            route(&PICO, &p(18, Some(11), None, None)),
            err(
                SpiRole::Mosi,
                11,
                SpiProblem::OtherBlock {
                    sck: Spi0,
                    pin: Spi1
                }
            )
        );
        assert_eq!(
            route(&PICO, &p(10, Some(11), Some(16), None)),
            err(
                SpiRole::Miso,
                16,
                SpiProblem::OtherBlock {
                    sck: Spi1,
                    pin: Spi0
                }
            )
        );
        // The board policy applies first.
        assert_eq!(
            route(&PICO, &p(26, Some(27), None, None)),
            err(SpiRole::Sck, 26, SpiProblem::Pin(PinError::Reserved))
        );
        assert_eq!(
            route(&PICO, &p(18, Some(19), Some(16), Some(24))),
            err(SpiRole::Cs, 24, SpiProblem::Pin(PinError::InputOnly))
        );
        assert_eq!(
            route(&PICO, &p(14, Some(15), Some(24), None)),
            Ok(Some(Spi1)),
            "MISO may be input-only 24"
        );
        assert_eq!(
            route(&PICO, &p(18, Some(19), Some(16), Some(30))),
            err(SpiRole::Cs, 30, SpiProblem::Pin(PinError::NoSuchPin))
        );
    }

    #[test]
    fn esp_pins_need_only_the_direction() {
        assert_eq!(
            route(&ESP32S3, &p(12, Some(11), Some(13), Some(10))),
            Ok(None)
        );
        assert_eq!(
            route(&ESP32, &p(14, Some(13), Some(35), None)),
            Ok(None),
            "MISO on input-only 35"
        );
        assert_eq!(
            route(&ESP32, &p(35, Some(13), None, None)),
            err(SpiRole::Sck, 35, SpiProblem::Pin(PinError::InputOnly))
        );
        assert_eq!(
            route(&ESP32, &p(14, Some(7), None, None)),
            err(SpiRole::Mosi, 7, SpiProblem::Pin(PinError::Reserved))
        );
    }
}
