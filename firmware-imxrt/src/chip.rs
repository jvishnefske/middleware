//! The board profile of the MIMXRT1050-EVKB and its pin policy.
//!
//! A [`Profile`] holds what the firmware needs to know about the chip and
//! its board beyond the drivers: the name `sys/info` reports, whether the
//! image runs payloads, the heap plan, the WASM memory and image caps, and
//! the pin map. The pin map is `board_pins::boards::RT1050_EVKB` (the
//! board-pins crate, which the web editor also reads); [`Profile::check_pin`]
//! and the other pin methods apply it and map its errors to [`AbiError`]
//! exactly as the RP and ESP base firmwares do.
//!
//! The profile has no flash map: the HyperFlash map is [`crate::layout`]'s,
//! and the firmware shell joins the two (the payload store's slot capacity,
//! `layout::PAYLOAD_CAPACITY`, caps [`Profile::max_image`] as a store slot
//! does on the other boards).
//!
//! # Pins
//!
//! Payload pin numbers are the EVKB's logical board pins, not GPIO lines
//! (the RT1052 has five banks of 32): D`n` of the Arduino header is pin
//! `n`, A`n` is pin `16 + n`, the USER button SW8 is pin 22, and pins 23
//! and 24 are the LPUART1 console. `board_pins::imxrt::evkb` maps each to
//! its pad, GPIO line and ALT functions, with the table and its NXP SDK
//! sources; [`Profile::board_pin`] gives the shell that entry.
//!
//! | pins       | board use                                  | payload access                    |
//! |------------|--------------------------------------------|-----------------------------------|
//! | 0–15       | Arduino D0–D15                             | input and output                  |
//! | 2, 3       | FXOS8700CQ INT2, audio jack detect         | input only                        |
//! | 4, 5       | Ethernet PHY reset (also the USER LED D18), PHY interrupt | none (`Unsupported`) |
//! | 16–21      | Arduino A0–A5                              | input, output and `adc_read`      |
//! | 22         | USER button SW8                            | input only                        |
//! | 23, 24     | LPUART1 (the protocol link)                | none (`Unsupported`)              |
//!
//! The firmware brings up Ethernet, so D4 and D5, the KSZ8081RNB PHY's
//! reset and interrupt lines, are the firmware's; the USER LED shares the
//! PHY reset and so is not a payload pin.
//! | 25 and up  | —                                          | none (`InvalidArgument`)          |
//!
//! [`Profile::check_pin`] returns `InvalidArgument` for a pin the board
//! lacks and `Unsupported` for a reserved pin or an output on an
//! input-only pin. [`Profile::adc_channel`] maps A0–A5 to ADC channels 0–5
//! (the ABI's channel; [`Profile::adc1_input`] gives the ADC1 input the
//! shell samples). The SDRAM, HyperFlash, boot, SWD, Ethernet (RMII and
//! MDIO) and SAI1 (audio codec) pads have no logical number, so no payload pin
//! reaches them.
//!
//! # Board pin map for I2C and SPI
//!
//! | bus       | controller | pins                                             |
//! |-----------|------------|--------------------------------------------------|
//! | I2C bus 0 | LPI2C1     | SDA A4 (pin 20), SCL A5 (pin 21)                 |
//! | SPI bus 0 | LPSPI1     | SCK D13, MOSI D11, MISO D12; CS any output pin   |
//!
//! I2C bus 0 also carries the board's FXOS8700CQ accelerometer (0x1F) and
//! WM8960 audio codec (0x1A). The SPI data pins are fixed (no function
//! table to choose from); [`Profile::spi_route`] refuses any other pin for
//! SCK, MOSI or MISO with `Unsupported`, and the chip select is driven as a
//! plain GPIO output for the whole transfer, as on the RP base firmware.
//! No PWM outputs are wired, so `pwm_*` is `Unsupported`.
//!
//! # Heap plan
//!
//! Only the payload runtime allocates: the heap is a fixed arena of
//! [`Profile::heap`] bytes holding WASM linear memory, the image while it
//! is compiled and the wasmi module and instance.
//!
//! | memory  | size    | holds                                                             |
//! |---------|---------|-------------------------------------------------------------------|
//! | FlexRAM | 512 KiB | DTCM: the main stack and the hot statics; ITCM: the bootloader    |
//! | SDRAM   | 32 MiB  | the firmware's run copy, its other statics, and the 384 KiB heap |
//!
//! | heap    | WASM memory | max image | runtime |
//! |---------|-------------|-----------|---------|
//! | 384 KiB | 128 KiB     | 64 KiB    | yes     |
//!
//! The firmware with the runtime (about 630 KB) is larger than the
//! FlexRAM, so it runs from SDRAM (design.md, "i.MX RT base firmware") and
//! the heap is in SDRAM too; the FlexRAM keeps only the stack and the
//! statics that need single-cycle access, so the heap plan takes nothing
//! from it. The caps are the RP2350's on purpose: the SDRAM could hold far
//! more, but a payload the web editor or `mwctl` accepts for one runtime
//! board should load on the others, and the 64 KiB image cap keeps an
//! image well inside a 124 KiB payload store slot. The 384 KiB heap is
//! what those caps need ([`Profile::plan_fits`]: linear memory, the
//! largest image and [`INTERPRETER_RESERVE`]) with the RP2350's margin,
//! about 1 % of the SDRAM; [`Profile::sdram_outside_heap`] is left for the
//! firmware copy and its statics, at least [`FIRMWARE_SDRAM`].

use board_pins::imxrt::Adc1Channel;
use board_pins::imxrt::evkb::{self, BoardPin};
use board_pins::spi::{SpiPins, SpiProblem, route};
use board_pins::{AdcError, PinError, PinMap, boards};
use middleware::wasm_abi::AbiError;
use middleware::wasm_abi::periph::SpiConfig;

pub use board_pins::Direction;

/// Size of one standard WASM page, for the memory caps.
pub const WASM_PAGE: u32 = 64 * 1024;

/// Heap kept for the wasmi engine, module and instance beyond linear
/// memory and the image (esp-base's figure).
pub const INTERPRETER_RESERVE: usize = esp_base::chip::INTERPRETER_RESERVE;

/// SDRAM kept outside the heap for the firmware's run copy (about 630 KB
/// with the runtime) and the statics that live in SDRAM: a bound with room
/// to grow, which the firmware slot size (`layout`) must not exceed.
pub const FIRMWARE_SDRAM: usize = 2 * 1024 * 1024;

/// Supported chips.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chip {
    /// i.MX RT1052 (Cortex-M7), on the MIMXRT1050-EVKB.
    Rt1050,
}

/// Static description of the chip and its board.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    /// The chip.
    pub chip: Chip,
    /// Name reported in `sys/info`.
    pub name: &'static str,
    /// FlexRAM (ITCM, DTCM and OCRAM together) in bytes.
    pub flexram: u32,
    /// SDRAM in bytes (the firmware's run copy and the heap).
    pub sdram: u32,
    /// Whether the image includes the WASM payload runtime.
    pub runtime: bool,
    /// Heap arena in bytes, in SDRAM (0 without the runtime).
    pub heap: usize,
    /// WASM linear-memory cap in bytes.
    pub max_memory_bytes: u32,
    /// Largest payload image accepted, in bytes (also capped by a store
    /// slot).
    pub max_image: u32,
    /// Logical board pins (`0..pin_count`; the pin map's pins).
    pub pin_count: u8,
    /// The board's pins (`board_pins::boards::RT1050_EVKB`).
    pins: PinMap,
}

/// NXP MIMXRT1050-EVKB.
pub const RT1050: Profile = Profile {
    chip: Chip::Rt1050,
    name: "rt1050",
    flexram: 512 * 1024,
    sdram: 32 * 1024 * 1024,
    runtime: true,
    heap: 384 * 1024,
    max_memory_bytes: 2 * WASM_PAGE,
    max_image: 64 * 1024,
    pin_count: evkb::PIN_COUNT,
    pins: boards::RT1050_EVKB,
};

/// A payload's SPI pins checked on the board: LPSPI1's fixed SCK, MOSI and
/// MISO, and a GPIO chip select.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpiRoute {
    /// Clock pin (D13, LPSPI1_SCK).
    pub sck: u8,
    /// Controller-out pin (D11, LPSPI1_SDO), if any.
    pub mosi: Option<u8>,
    /// Controller-in pin (D12, LPSPI1_SDI), if any.
    pub miso: Option<u8>,
    /// Chip select, driven as a GPIO output, if any.
    pub cs: Option<u8>,
}

impl SpiRoute {
    /// The pins routed to LPSPI1 (SCK, MOSI, MISO).
    pub fn data_pins(&self) -> impl Iterator<Item = u8> {
        [Some(self.sck), self.mosi, self.miso].into_iter().flatten()
    }

    /// Every pin the bus holds, chip select included.
    pub fn pins(&self) -> impl Iterator<Item = u8> {
        [Some(self.sck), self.mosi, self.miso, self.cs]
            .into_iter()
            .flatten()
    }
}

impl Profile {
    /// The board's pin map.
    pub const fn pins(&self) -> &PinMap {
        &self.pins
    }

    /// Whether a payload may use `pin` as `dir`: `InvalidArgument` if the
    /// board has no such pin, `Unsupported` if the board reserves it or it
    /// is input only and `dir` is `Output`.
    pub const fn check_pin(&self, pin: u8, dir: Direction) -> Result<(), AbiError> {
        match self.pins.check(pin, dir) {
            Ok(()) => Ok(()),
            Err(PinError::NoSuchPin) => Err(AbiError::InvalidArgument),
            Err(PinError::Reserved | PinError::InputOnly) => Err(AbiError::Unsupported),
        }
    }

    /// Pins a payload may use as GPIO (inputs at least), ascending.
    pub fn payload_pins(&self) -> impl Iterator<Item = u8> + '_ {
        self.pins.payload_pins()
    }

    /// The pad, GPIO line and functions of logical `pin`, for the shell:
    /// `InvalidArgument` for a pin the board lacks.
    pub fn board_pin(&self, pin: u8) -> Result<&'static BoardPin, AbiError> {
        if !self.pins.exists(pin) {
            return Err(AbiError::InvalidArgument);
        }
        evkb::pin(pin).ok_or(AbiError::InvalidArgument)
    }

    /// The ADC channel sampling `pin` (channel `n` is A`n`):
    /// `InvalidArgument` for a pin the board lacks, `Unsupported` for one
    /// without an analog input.
    pub fn adc_channel(&self, pin: u8) -> Result<u8, AbiError> {
        self.pins.adc_channel(pin).map_err(|e| match e {
            AdcError::NoSuchPin => AbiError::InvalidArgument,
            AdcError::NotAnalog => AbiError::Unsupported,
        })
    }

    /// The ADC1 input the shell samples for `pin`, with the errors of
    /// [`Profile::adc_channel`].
    pub fn adc1_input(&self, pin: u8) -> Result<Adc1Channel, AbiError> {
        self.adc_channel(pin)?;
        self.board_pin(pin)?.adc1.ok_or(AbiError::Unsupported)
    }

    /// I2C `bus`'s (SDA, SCL) pins (board pin map); `Unsupported` for a bus
    /// the board does not map.
    pub fn i2c_pins(&self, bus: u8) -> Result<(u8, u8), AbiError> {
        self.pins.i2c_pins(bus).ok_or(AbiError::Unsupported)
    }

    /// Checks `cfg`'s pins on the board (see the module docs): in the order
    /// SCK, MOSI, MISO, CS, each against the board policy first, then
    /// LPSPI1's fixed pins. A pin the board lacks is `InvalidArgument`; a
    /// reserved pin, an output on an input-only pin, or a data pin other
    /// than LPSPI1's is `Unsupported`.
    pub fn spi_route(&self, cfg: &SpiConfig) -> Result<SpiRoute, AbiError> {
        let pins = SpiPins {
            sck: cfg.sck,
            mosi: cfg.mosi,
            miso: cfg.miso,
            cs: cfg.cs,
        };
        route(&self.pins, &pins).map_err(|e| match e.problem {
            SpiProblem::Pin(PinError::NoSuchPin) => AbiError::InvalidArgument,
            _ => AbiError::Unsupported,
        })?;
        Ok(SpiRoute {
            sck: cfg.sck,
            mosi: cfg.mosi,
            miso: cfg.miso,
            cs: cfg.cs,
        })
    }

    /// Whether the heap plan holds linear memory, the largest image and the
    /// interpreter reserve (vacuously true without the runtime, which must
    /// then have no heap).
    pub const fn plan_fits(&self) -> bool {
        if !self.runtime {
            return self.heap == 0;
        }
        let needed = self.max_memory_bytes as usize + self.max_image as usize + INTERPRETER_RESERVE;
        needed <= self.heap && self.max_memory_bytes >= WASM_PAGE
    }

    /// Bytes of SDRAM outside the heap: the firmware's run copy and its
    /// SDRAM statics.
    pub const fn sdram_outside_heap(&self) -> usize {
        (self.sdram as usize).saturating_sub(self.heap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(sck: i32, mosi: i32, miso: i32, cs: i32) -> SpiConfig {
        SpiConfig::parse(0, sck, mosi, miso, cs, 1000, 0).unwrap()
    }

    #[test]
    fn rt11_profile_is_the_rt1050_with_the_runtime() {
        assert_eq!(RT1050.chip, Chip::Rt1050);
        assert_eq!((RT1050.name, RT1050.runtime), ("rt1050", true));
        // The name `sys/info` reports selects the same map in board-pins
        // (and so in the web editor).
        assert_eq!(boards::by_chip(RT1050.name), Some(RT1050.pins()));
        assert_eq!(usize::from(RT1050.pin_count), RT1050.pins().gpios().count());
        assert!(RT1050.pins().gpios().all(|g| g < RT1050.pin_count));
    }

    #[test]
    fn rt11_heap_plan_fits_the_sdram_and_matches_the_rp2350_caps() {
        assert!(RT1050.plan_fits());
        assert_eq!(RT1050.max_memory_bytes, 2 * WASM_PAGE);
        assert_eq!(RT1050.max_image, 64 * 1024);
        // An image fits one payload store slot (the ESP partition's
        // 124 KiB, which the RT1050 store keeps).
        let slot = esp_base::image::Region::new(0x4_0000).capacity();
        assert!(RT1050.max_image <= slot);
        // The heap is in SDRAM, beside the firmware copy.
        assert!(RT1050.sdram_outside_heap() >= FIRMWARE_SDRAM);
        // A heap that cannot hold the caps is refused.
        assert!(
            !Profile {
                heap: 100 * 1024,
                ..RT1050
            }
            .plan_fits()
        );
        assert!(
            !Profile {
                runtime: false,
                ..RT1050
            }
            .plan_fits()
        );
        assert!(
            Profile {
                runtime: false,
                heap: 0,
                ..RT1050
            }
            .plan_fits()
        );
    }

    #[test]
    fn rt11_pin_policy_maps_to_abi_errors() {
        let p = RT1050;
        assert_eq!(
            p.check_pin(25, Direction::Input),
            Err(AbiError::InvalidArgument)
        );
        assert_eq!(
            p.check_pin(255, Direction::Output),
            Err(AbiError::InvalidArgument)
        );
        for reserved in [4, 5, 23, 24] {
            assert_eq!(
                p.check_pin(reserved, Direction::Input),
                Err(AbiError::Unsupported)
            );
        }
        for input_only in [2, 3, 22] {
            assert_eq!(p.check_pin(input_only, Direction::Input), Ok(()));
            assert_eq!(
                p.check_pin(input_only, Direction::Output),
                Err(AbiError::Unsupported)
            );
        }
        for ok in [0, 1, 7, 13, 15, 16, 20, 21] {
            assert_eq!(p.check_pin(ok, Direction::Output), Ok(()));
        }
        assert_eq!(p.payload_pins().count(), 21);
        // The shell reaches each pin's pad through the profile.
        assert_eq!(p.board_pin(evkb::USER_LED).map(|b| b.label), Ok("D4"));
        assert_eq!(p.board_pin(evkb::USER_BUTTON).map(|b| b.label), Ok("SW8"));
        assert_eq!(p.board_pin(25), Err(AbiError::InvalidArgument));
    }

    #[test]
    fn rt11_adc_reads_a0_to_a5_on_adc1() {
        let p = RT1050;
        for n in 0..6u8 {
            assert_eq!(p.adc_channel(16 + n), Ok(n), "A{n}");
        }
        assert_eq!(p.adc1_input(16).map(Adc1Channel::number), Ok(15));
        assert_eq!(p.adc1_input(17).map(Adc1Channel::number), Ok(0));
        assert_eq!(p.adc1_input(21).map(Adc1Channel::number), Ok(5));
        assert_eq!(p.adc_channel(0), Err(AbiError::Unsupported));
        assert_eq!(p.adc1_input(13), Err(AbiError::Unsupported));
        assert_eq!(p.adc_channel(40), Err(AbiError::InvalidArgument));
        assert_eq!(p.adc1_input(40), Err(AbiError::InvalidArgument));
    }

    #[test]
    fn rt11_i2c_bus_0_is_lpi2c1_on_a4_a5() {
        use board_pins::imxrt::Signal;
        assert_eq!(RT1050.i2c_pins(0), Ok((20, 21)));
        assert_eq!(RT1050.i2c_pins(1), Err(AbiError::Unsupported));
        let (sda, scl) = RT1050.i2c_pins(0).unwrap();
        let f = |pin, s| RT1050.board_pin(pin).ok().and_then(|b| b.function(s));
        assert!(f(sda, Signal::Lpi2c1Sda).is_some());
        assert!(f(scl, Signal::Lpi2c1Scl).is_some());
        assert_eq!(RT1050.check_pin(sda, Direction::Output), Ok(()));
        assert_eq!(RT1050.check_pin(scl, Direction::Output), Ok(()));
    }

    #[test]
    fn rt11_spi_pins_resolve_onto_lpspi1() {
        let p = RT1050;
        let r = p.spi_route(&cfg(13, 11, 12, 10)).unwrap();
        assert_eq!(
            r.pins().collect::<heapless::Vec<u8, 4>>().as_slice(),
            &[13, 11, 12, 10]
        );
        assert_eq!(r.data_pins().count(), 3);
        // Transmit only, CS on D7.
        let r = p.spi_route(&cfg(13, 11, -1, 7)).unwrap();
        assert_eq!((r.miso, r.cs), (None, Some(7)));
        // The Ethernet PHY reset (D4) is no chip select.
        assert_eq!(p.spi_route(&cfg(13, 11, -1, 4)), Err(AbiError::Unsupported));
        // A data role off its fixed pin.
        assert_eq!(
            p.spi_route(&cfg(12, 11, -1, -1)),
            Err(AbiError::Unsupported)
        );
        assert_eq!(
            p.spi_route(&cfg(13, 10, -1, -1)),
            Err(AbiError::Unsupported)
        );
        // The board policy: a reserved or input-only CS, a missing pin.
        assert_eq!(
            p.spi_route(&cfg(13, 11, 12, 23)),
            Err(AbiError::Unsupported)
        );
        assert_eq!(
            p.spi_route(&cfg(13, 11, 12, 22)),
            Err(AbiError::Unsupported)
        );
        assert_eq!(
            p.spi_route(&cfg(13, 11, 12, 40)),
            Err(AbiError::InvalidArgument)
        );
    }
}
