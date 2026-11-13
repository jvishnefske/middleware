//! Per-chip profiles and the board's pin policy.
//!
//! A [`Profile`] holds what the firmware needs to know about a chip and its
//! board beyond the drivers: the name `sys/info` reports, the flash map
//! ([`crate::layout`]), whether the image runs payloads, the heap plan, the
//! WASM memory and image caps, and the pin map. Both chips use
//! `board_pins::boards::PICO` (the board-pins crate, which the web editor
//! also reads), so the tables below are that map's; [`Profile::check_pin`]
//! and the other pin methods apply it and map its errors to [`AbiError`].
//!
//! # Pins (Raspberry Pi Pico and Pico 2)
//!
//! Both boards bring out GPIO 0–22 and 26–28 on the header (the RP2040 and
//! the RP2350A have GPIO 0–29). The rest are wired on the board:
//!
//! | GPIO  | board use            | payload access                   |
//! |-------|----------------------|----------------------------------|
//! | 23    | regulator power save | none (`Unsupported`)             |
//! | 24    | VBUS sense           | input only                       |
//! | 25    | on-board LED         | input and output                 |
//! | 26–28 | ADC0–ADC2            | `adc_read` only (`Unsupported` as GPIO) |
//! | 29    | VSYS / 3             | none (`Unsupported`)             |
//!
//! The QSPI flash and USB pins are dedicated pads, not GPIOs, so no payload
//! pin can reach them. GPIO 26–28 are given to the ADC at boot (an ADC
//! channel owns its pin), so they are analog only; [`Profile::adc_channel`]
//! maps them to channels 0–2. [`Profile::check_pin`] returns
//! `InvalidArgument` for a pin the chip lacks and `Unsupported` for a
//! reserved pin or an output on an input-only pin, as esp-base does.
//!
//! # Board pin map for I2C (ABI 3)
//!
//! ABI 3 leaves the I2C pins to the board: `i2c_open(bus, ..)` names a bus,
//! and the board maps it to fixed pins, the Pico and Pico 2 pinout's I2C
//! defaults ([`Profile::i2c_pins`]):
//!
//! | I2C bus | controller | SDA    | SCL    |
//! |---------|------------|--------|--------|
//! | 0       | I2C0       | GPIO 4 | GPIO 5 |
//! | 1       | I2C1       | GPIO 6 | GPIO 7 |
//!
//! Both pairs carry their controller's lines in the chips' GPIO function
//! table ([`crate::periph::func`]). They are ordinary payload pins until a
//! payload opens the bus; then the bus holds them ([`crate::periph::claims`]).
//! SPI pins are the payload's choice (`spi_open`), checked against the same
//! function table. PWM needs pins bound at boot; this board map assigns
//! none, so `pwm_*` is `Unsupported` (see the ABI coverage table in
//! `design.md`).
//!
//! # Heap plan
//!
//! Only the payload runtime allocates (`wasm` feature): the heap is a fixed
//! `.bss` arena of [`Profile::heap`] bytes holding WASM linear memory, the
//! image while it is compiled and the wasmi module and instance. Nothing
//! else in the firmware allocates.
//!
//! | chip   | SRAM    | heap    | WASM memory | max image | runtime |
//! |--------|---------|---------|-------------|-----------|---------|
//! | RP2040 | 264 KiB | 0       | 0           | 124 KiB (stored only) | no |
//! | RP2350 | 520 KiB | 384 KiB | 128 KiB     | 64 KiB    | yes     |
//!
//! The RP2040 has no payload runtime: wasmi needs CAS atomics and
//! `alloc::sync::Arc`, which the Cortex-M0+ target lacks, so its image
//! stores payloads (the loader, store and trust work) but does not run
//! them, and it has no heap at all. Its `max_image` is one store slot, as
//! on the ESP8266.
//!
//! On the RP2350 the 136 KiB outside the heap hold the statics (USB, the
//! message bus, the link buffers, the embassy task futures) and the main
//! stack, which a payload load needs about 31 KiB of (measured on the
//! ESP32-S3). [`Profile::plan_fits`] checks that the heap covers linear
//! memory, the largest image and [`INTERPRETER_RESERVE`]; what one payload
//! needs is decided before every start from its image
//! (`wasm_host::Limits::heap_needed`, [`esp_base::start`]); see
//! `crate::budget` (built with the runtime).

use crate::layout::{self, FlashMap};
use board_pins::{AdcError, PinError, PinMap, boards};
use middleware::wasm_abi::AbiError;

pub use board_pins::Direction;

/// Size of one standard WASM page, for the memory caps.
pub const WASM_PAGE: u32 = 64 * 1024;

/// Heap kept for the wasmi engine, module and instance beyond linear
/// memory and the image (esp-base's figure).
pub const INTERPRETER_RESERVE: usize = esp_base::chip::INTERPRETER_RESERVE;

/// Supported chips.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chip {
    /// RP2040 (dual Cortex-M0+), on the Raspberry Pi Pico.
    Rp2040,
    /// RP2350A (dual Cortex-M33), on the Raspberry Pi Pico 2.
    Rp2350,
}

/// Static description of a chip and its board.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    /// The chip.
    pub chip: Chip,
    /// Name reported in `sys/info` (matches the cargo feature).
    pub name: &'static str,
    /// The flash map.
    pub flash: FlashMap,
    /// SRAM size in bytes.
    pub sram: u32,
    /// Whether the image includes the WASM payload runtime.
    pub runtime: bool,
    /// Heap arena in bytes (0 without the runtime).
    pub heap: usize,
    /// WASM linear-memory cap in bytes.
    pub max_memory_bytes: u32,
    /// Largest payload image accepted, in bytes (also capped by a store
    /// slot).
    pub max_image: u32,
    /// GPIOs the chip has (`0..gpio_count`; the pin map's GPIOs).
    pub gpio_count: u8,
    /// The board's pins: reserved and input-only pins, the ADC and I2C
    /// maps, SPI routing (`board_pins::boards::PICO`).
    pins: PinMap,
}

/// Raspberry Pi Pico.
pub const RP2040: Profile = Profile {
    chip: Chip::Rp2040,
    name: "rp2040",
    flash: layout::RP2040,
    sram: 264 * 1024,
    runtime: false,
    heap: 0,
    max_memory_bytes: 0,
    max_image: 124 * 1024,
    gpio_count: 30,
    pins: boards::PICO,
};

/// Raspberry Pi Pico 2.
pub const RP2350: Profile = Profile {
    chip: Chip::Rp2350,
    name: "rp2350",
    flash: layout::RP2350,
    sram: 520 * 1024,
    runtime: true,
    heap: 384 * 1024,
    max_memory_bytes: 2 * WASM_PAGE,
    max_image: 64 * 1024,
    gpio_count: 30,
    pins: boards::PICO,
};

impl Profile {
    /// The board's pin map.
    pub const fn pins(&self) -> &PinMap {
        &self.pins
    }

    /// Whether a payload may use `pin` as `dir`: `InvalidArgument` if the
    /// chip has no such pin, `Unsupported` if the board reserves it or it
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

    /// The ADC channel sampling `pin`: `InvalidArgument` for a pin the chip
    /// lacks, `Unsupported` for one without an analog input.
    pub fn adc_channel(&self, pin: u8) -> Result<u8, AbiError> {
        self.pins.adc_channel(pin).map_err(|e| match e {
            AdcError::NoSuchPin => AbiError::InvalidArgument,
            AdcError::NotAnalog => AbiError::Unsupported,
        })
    }

    /// I2C `bus`'s (SDA, SCL) pins (board pin map); `Unsupported` for a bus
    /// the board does not map.
    pub fn i2c_pins(&self, bus: u8) -> Result<(u8, u8), AbiError> {
        self.pins.i2c_pins(bus).ok_or(AbiError::Unsupported)
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

    /// Bytes of SRAM outside the heap: statics and the main stack.
    pub const fn sram_outside_heap(&self) -> usize {
        (self.sram as usize).saturating_sub(self.heap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use esp_base::image::Region as Slots;

    #[test]
    fn v8_pins_follow_the_pico_board_map() {
        for p in [RP2040, RP2350] {
            assert_eq!(p.check_pin(30, Direction::Input), Err(AbiError::InvalidArgument));
            assert_eq!(p.check_pin(255, Direction::Output), Err(AbiError::InvalidArgument));
            for reserved in [23, 26, 27, 28, 29] {
                assert_eq!(p.check_pin(reserved, Direction::Input), Err(AbiError::Unsupported));
            }
            assert_eq!(p.check_pin(24, Direction::Input), Ok(()));
            assert_eq!(p.check_pin(24, Direction::Output), Err(AbiError::Unsupported));
            for ok in [0, 8, 15, 22, 25] {
                assert_eq!(p.check_pin(ok, Direction::Output), Ok(()));
            }
            assert_eq!(p.payload_pins().count(), 25);
        }
    }

    #[test]
    fn gpio_count_matches_the_pin_map() {
        for p in [RP2040, RP2350] {
            assert_eq!(usize::from(p.gpio_count), p.pins().gpios().count(), "{}", p.name);
            assert!(p.pins().gpios().all(|g| g < p.gpio_count), "{}", p.name);
        }
    }

    #[test]
    fn v8_adc_reads_gpio_26_to_28() {
        assert_eq!(RP2350.adc_channel(26), Ok(0));
        assert_eq!(RP2350.adc_channel(28), Ok(2));
        assert_eq!(RP2350.adc_channel(29), Err(AbiError::Unsupported));
        assert_eq!(RP2350.adc_channel(0), Err(AbiError::Unsupported));
        assert_eq!(RP2350.adc_channel(40), Err(AbiError::InvalidArgument));
    }

    #[test]
    fn v16_i2c_buses_map_to_the_pico_default_pins() {
        use crate::periph::func::{I2cBlock, I2cLine, i2c_function};
        for p in [RP2040, RP2350] {
            assert_eq!(p.i2c_pins(0), Ok((4, 5)));
            assert_eq!(p.i2c_pins(1), Ok((6, 7)));
            assert_eq!(p.i2c_pins(2), Err(AbiError::Unsupported));
            for (bus, block) in [(0, I2cBlock::I2c0), (1, I2cBlock::I2c1)] {
                let (sda, scl) = p.i2c_pins(bus).unwrap();
                assert_eq!(i2c_function(sda), Some((block, I2cLine::Sda)), "bus {bus}");
                assert_eq!(i2c_function(scl), Some((block, I2cLine::Scl)), "bus {bus}");
                assert_eq!(p.check_pin(sda, Direction::Output), Ok(()));
                assert_eq!(p.check_pin(scl, Direction::Output), Ok(()));
            }
        }
    }

    #[test]
    fn v7_heap_plans_fit_and_images_fit_a_slot() {
        for p in [RP2040, RP2350] {
            assert!(p.plan_fits(), "{}", p.name);
            assert!(p.max_image <= Slots::new(p.flash.payload.size).capacity(), "{}", p.name);
        }
        // The RP2040 runs no payloads and so has no heap.
        assert_eq!((RP2040.runtime, RP2040.heap), (false, 0));
        // The RP2350 keeps at least 128 KiB of SRAM for statics and stack.
        assert!(RP2350.sram_outside_heap() >= 128 * 1024);
        let broken = Profile { heap: 100 * 1024, ..RP2350 };
        assert!(!broken.plan_fits());
        assert!(!Profile { heap: 4, ..RP2040 }.plan_fits());
    }
}
