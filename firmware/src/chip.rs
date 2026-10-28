//! Per-chip profiles and GPIO policy.
//!
//! A [`Profile`] collects everything the firmware needs to know about a chip
//! that is not a peripheral driver: the name reported in `sys/info`, whether
//! it has a USB-Serial-JTAG port, whether the image includes the WASM
//! payload runtime (`runtime`), the heap plan, the WASM memory limit in
//! bytes (`max_memory_bytes`), the largest payload image it accepts, the
//! console UART pins, and three GPIO bitmasks:
//!
//! * `gpio` — pins that exist on the chip.
//! * `reserved` — pins a payload may never touch: SPI flash and in-package
//!   or module PSRAM, the USB D-/D+ pair, and UART0 TX/RX (the protocol
//!   port).
//! * `input_only` — pins without an output driver (ESP32 GPIO34–39).
//!
//! and a fourth, `strapping` (E14): pins whose level at reset selects the
//! boot mode or flash voltage (ESP32 GPIO0/2/5/12/15, ESP32-S3 0/3/45/46,
//! ESP32-C3 2/8/9, ESP8266 0/2/15). A payload may use them —
//! [`Profile::check_pin`] does not refuse them — but a host warns about an
//! assignment that drives one, as ESPHome does (`ignore_strapping_warning`).
//!
//! [`Profile::check_pin`] applies the policy and returns the
//! [`AbiError`] a host import reports: `InvalidArgument` for a pin that does
//! not exist, `Unsupported` for a reserved pin or an output on an input-only
//! pin.
//!
//! # PWM and ADC pins (ABI 3, X9, X12)
//!
//! The ABI 3 peripherals have fixed pins, so they are part of the profile:
//!
//! * `pwm_pins` — the GPIO of each LEDC low-speed channel `2·timer +
//!   channel` (ABI timers 0–3, channels 0–1; [`Profile::pwm_pin`]). The
//!   pins avoid strapping, flash/PSRAM, USB, UART0, ADC1 and the default
//!   I2C pins, so a payload can use PWM, ADC and the console at once:
//!
//!   | ABI (t,c) | (0,0) | (0,1) | (1,0) | (1,1) | (2,0) | (2,1) | (3,0) | (3,1) |
//!   |-----------|-------|-------|-------|-------|-------|-------|-------|-------|
//!   | ESP32     | 4     | 13    | 18    | 19    | 23    | 25    | 26    | 27    |
//!   | ESP32-S3  | 11    | 12    | 13    | 14    | 15    | 16    | 17    | 18    |
//!
//!   Empty on chips without the runtime (ESP32-C3, ESP8266).
//! * `pwm_max_bits` — the widest LEDC duty the chip supports that the
//!   firmware uses (16 on ESP32, 14 on ESP32-S3; 0 without PWM), and
//!   `pwm_ref_tick` — whether the 1 MHz REF_TICK is available for
//!   frequencies too low for the APB clock (ESP32 only). See
//!   [`crate::periph::pwm`].
//! * `adc` — pins with an ADC1 channel (ESP32 GPIO32–39, ESP32-S3
//!   GPIO1–10; [`Profile::has_adc`]). ADC2 is never offered: it shares its
//!   controller with Wi-Fi.
//!
//! # Heap plan
//!
//! The heap holds WASM linear memory, the payload image while
//! it is compiled, the wasmi interpreter's module and instance data and,
//! on chips built with ESP-NOW (`espnow` feature: ESP32, ESP32-S3), the
//! Wi-Fi driver's buffers and tasks (`radio_heap`). It is built from two
//! regions: RAM reclaimed from the second-stage bootloader
//! (`heap_reclaimed`, the size `esp-generate` uses per chip) plus a region in
//! ordinary `.bss` (`heap_main`).
//!
//! | chip     | reclaimed | main    | total   | memory  | max image | radio  |
//! |----------|-----------|---------|---------|---------|-----------|--------|
//! | ESP32    | 96.4 KiB  | 80 KiB  | 176 KiB | 64 KiB  | 24 KiB    | 48 KiB |
//! | ESP32-S3 | 72.0 KiB  | 152 KiB | 224 KiB | 64 KiB  | 32 KiB    | 48 KiB |
//! | ESP32-C3 | 64.8 KiB  | 176 KiB | 241 KiB | 128 KiB | 64 KiB    | 0      |
//! | ESP8266  | 0         | 0       | 0       | 0       | 124 KiB   | 0      |
//!
//! ESP-NOW (0.5) cost the ESP32 its second WASM page and part of its image
//! cap: the Wi-Fi driver adds about 30 KiB of static RAM (blob `.data` and
//! `.bss`, the radio task, its queues) and needs heap at run time, and the
//! ESP32's 192 KiB of main DRAM had no slack (`heap_main` 112 → 80 KiB,
//! memory 128 → 64 KiB, image 32 → 24 KiB, which still holds every
//! example payload, the largest 13 KiB). On the ESP32-S3, whose IRAM and
//! DRAM share one SRAM, the driver's RAM-resident code (`.rwtext.wifi`,
//! 31 KiB) also comes out of data RAM, about 58 KiB in all; keeping the
//! stack above the 31 KiB a payload load needs took `heap_main` 176 → 128
//! KiB, memory 128 → 64 KiB and image 80 → 32 KiB. Eager translation
//! (W10) then made a payload's whole bytecode resident from its start, and
//! the ESP32-S3 gave the main stack's surplus back: `heap_main` 128 → 152
//! KiB, leaving about 46 KiB of stack (the conagotchi badge's build checks
//! at least 40 KiB). The radio budget is the
//! driver's footprint with the reduced buffer counts of the radio shell
//! (6 static RX buffers of 1.6 KiB, no aggregation) plus margin; it is an
//! estimate until measured on hardware.
//!
//! The memory limit is a byte cap (`wasm_host::Limits::bytes`): a payload
//! with standard 64 KiB pages gets whole pages up to it, and one built with
//! 1-byte pages (custom-page-sizes, see `payloads/README.md`) may use any
//! size up to it. The caps are page multiples so 64 KiB-page payloads lose
//! nothing. `sys/info` does not carry the limit, so its wire format is
//! unchanged; the boot log reports it in bytes.
//!
//! The ESP8266 runs the protocol only: it has no WASM runtime, so it has no
//! payload heap and no linear memory. Its `max_image` is one slot of the
//! 256 KiB payload flash region at 0x100000 on the 4 MB module: the region
//! holds two 128 KiB slots (see [`crate::image`]), each a 4 KiB header
//! sector plus 124 KiB of image; images are stored, not run. The ESP32-C3 also builds
//! without the runtime (`runtime` is `false`, matching the absent `wasm`
//! cargo feature), though its heap plan is kept for when it gains one.
//!
//! The main regions are sized so the linker leaves a usable main stack
//! (after `.bss`, measured from `_stack_end`..`_stack_start`): ESP32 about
//! 26 KiB, ESP32-S3 about 46 KiB (both with ESP-NOW), ESP32-C3 about
//! 103 KiB. The ESP32-S3 had
//! 240 KiB main and 192 KiB memory until 0.4, which left 17 KiB of stack;
//! loading a payload (wasmi parse plus `init`, about 31 KiB deep) then
//! overflowed into the stack guard on hardware. On the Xtensa
//! chips the ABI 2 peripherals take about 14 KiB of `.bss` (two SPI DMA
//! buffers and the UART transmit buffer of `MAX_DMA` bytes each, the
//! 1 KiB UART receive ring, the event queue) plus esp-hal's RAM-resident
//! SPI/DMA/UART code; the ESP32-C3 builds without them.
//!
//! Each runtime profile leaves at least [`INTERPRETER_RESERVE`] bytes beyond
//! the linear memory, image and radio budget for wasmi, and at least one
//! 64 KiB page of linear memory (checked by a unit test); the heap check
//! applies only to profiles with `runtime` set. This static plan is a
//! floor, not a promise that every image up to `max_image` runs: what a
//! payload needs is `wasm_host::Limits::load_heap_needed` (wasmi's module
//! grows with the image, about 2.75 bytes per image byte plus 16 KiB on
//! these 32-bit chips), and the controller checks it against the heap
//! actually free before every start ([`crate::start`]). With 64 KiB of
//! linear memory that admits images up to about 7 KiB on the ESP32 and
//! 24 KiB on the ESP32-S3 if the radio uses its whole budget.
//!
//! Every profile's `max_image` fits one slot of the 256 KiB (`0x40000`)
//! payload partition, whose per-slot capacity is 124 KiB: the ESP32 family
//! uses the `payload` partition in `partitions.csv`, the ESP8266 the region
//! at 0x100000 (checked by a unit test).

use middleware::loader::{BOARD_PWM_REF_TICK, BOARD_RUNTIME, BOARD_USB_JTAG, BoardInfo};
use middleware::wasm_abi::AbiError;

/// Size of one standard (64 KiB) WASM linear-memory page, used to express
/// the per-chip byte caps.
pub const WASM_PAGE: u32 = 64 * 1024;

/// Heap kept free for the wasmi engine, compiled module and instance on top
/// of linear memory and the image buffer.
pub const INTERPRETER_RESERVE: usize = 40 * 1024;

/// Supported chips.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chip {
    /// ESP32 (Xtensa LX6, dual core).
    Esp32,
    /// ESP32-S3 (Xtensa LX7, dual core, USB-Serial-JTAG).
    Esp32s3,
    /// ESP32-C3 (RISC-V, single core, USB-Serial-JTAG).
    Esp32c3,
    /// ESP8266 (Xtensa LX106, single core, no WASM runtime).
    Esp8266,
}

/// What a payload wants to do with a pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Read the pin level.
    Input,
    /// Drive the pin.
    Output,
}

/// Static description of a chip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    /// The chip.
    pub chip: Chip,
    /// Name reported in `sys/info` (matches the cargo feature and
    /// `espflash --chip`).
    pub name: &'static str,
    /// Whether the chip has a USB-Serial-JTAG port the protocol also serves.
    pub usb_serial_jtag: bool,
    /// Whether the image includes the WASM payload runtime (the `wasm`
    /// cargo feature). Without it payloads can be stored but not run, and
    /// the heap fields and `max_memory_bytes` describe no live allocation.
    pub runtime: bool,
    /// Heap bytes in RAM reclaimed from the bootloader.
    pub heap_reclaimed: usize,
    /// Heap bytes in ordinary `.bss`.
    pub heap_main: usize,
    /// WASM linear-memory limit in bytes, whatever the payload's page size.
    pub max_memory_bytes: u32,
    /// Heap bytes left for the Wi-Fi driver (esp-radio, ESP-NOW), 0 on a
    /// chip built without the `espnow` feature.
    pub radio_heap: usize,
    /// Largest payload image accepted, in bytes (also capped by the flash
    /// partition).
    pub max_image: u32,
    /// UART0 TX pin (protocol port behind the bridge chip).
    pub uart0_tx: u8,
    /// UART0 RX pin.
    pub uart0_rx: u8,
    /// GPIO of each PWM output, indexed by LEDC low-speed channel
    /// `2·timer + channel`; empty without PWM.
    pub pwm_pins: &'static [u8],
    /// Widest PWM duty resolution in bits (0 without PWM).
    pub pwm_max_bits: u8,
    /// Whether PWM timers can fall back to the 1 MHz REF_TICK.
    pub pwm_ref_tick: bool,
    gpio: u64,
    reserved: u64,
    input_only: u64,
    strapping: u64,
    adc: u64,
}

/// Bitmask with bits `from..=to` set.
const fn span(from: u8, to: u8) -> u64 {
    let mut m = 0u64;
    let mut i = from;
    while i <= to {
        m |= 1 << i;
        i += 1;
    }
    m
}

const fn bit(pin: u8) -> u64 {
    1 << pin
}

/// ESP32: GPIO0–19, 21–23, 25–27, 32–39. Flash on 6–11, PSRAM (WROVER) or
/// flash (PICO) on 16/17, UART0 on 1 (TX) / 3 (RX). 34–39 are input only.
pub const ESP32: Profile = Profile {
    chip: Chip::Esp32,
    name: "esp32",
    usb_serial_jtag: false,
    runtime: true,
    heap_reclaimed: 98_768,
    heap_main: 80 * 1024,
    max_memory_bytes: WASM_PAGE,
    radio_heap: 48 * 1024,
    max_image: 24 * 1024,
    uart0_tx: 1,
    uart0_rx: 3,
    gpio: span(0, 19) | span(21, 23) | span(25, 27) | span(32, 39),
    pwm_pins: &[4, 13, 18, 19, 23, 25, 26, 27],
    pwm_max_bits: 16,
    pwm_ref_tick: true,
    reserved: bit(1) | bit(3) | span(6, 11) | bit(16) | bit(17),
    input_only: span(34, 39),
    strapping: bit(0) | bit(2) | bit(5) | bit(12) | bit(15),
    adc: span(32, 39),
};

/// ESP32-S3: GPIO0–21, 26–48. Flash/PSRAM on 26–32, octal flash/PSRAM on
/// 33–37, USB D-/D+ on 19/20, UART0 on 43 (TX) / 44 (RX).
pub const ESP32S3: Profile = Profile {
    chip: Chip::Esp32s3,
    name: "esp32s3",
    usb_serial_jtag: true,
    runtime: true,
    heap_reclaimed: 73_744,
    heap_main: 152 * 1024,
    max_memory_bytes: WASM_PAGE,
    radio_heap: 48 * 1024,
    max_image: 32 * 1024,
    uart0_tx: 43,
    uart0_rx: 44,
    gpio: span(0, 21) | span(26, 48),
    pwm_pins: &[11, 12, 13, 14, 15, 16, 17, 18],
    pwm_max_bits: 14,
    pwm_ref_tick: false,
    reserved: bit(19) | bit(20) | span(26, 37) | bit(43) | bit(44),
    input_only: 0,
    strapping: bit(0) | bit(3) | bit(45) | bit(46),
    adc: span(1, 10),
};

/// ESP32-C3: GPIO0–21. Flash on 12–17, USB D-/D+ on 18/19, UART0 on 21
/// (TX) / 20 (RX).
pub const ESP32C3: Profile = Profile {
    chip: Chip::Esp32c3,
    name: "esp32c3",
    usb_serial_jtag: true,
    runtime: false,
    heap_reclaimed: 66_320,
    heap_main: 176 * 1024,
    max_memory_bytes: 2 * WASM_PAGE,
    radio_heap: 0,
    max_image: 64 * 1024,
    uart0_tx: 21,
    uart0_rx: 20,
    gpio: span(0, 21),
    pwm_pins: &[],
    pwm_max_bits: 0,
    pwm_ref_tick: false,
    reserved: span(12, 17) | bit(18) | bit(19) | bit(20) | bit(21),
    input_only: 0,
    strapping: bit(2) | bit(8) | bit(9),
    adc: 0,
};

/// ESP8266: GPIO0–16. Flash on 6–11, UART0 on 1 (TX) / 3 (RX) behind the
/// CH340 bridge. No WASM runtime, so no payload heap or linear memory; the
/// payload flash region is 256 KiB at 0x100000 on the 4 MB module, two
/// 128 KiB slots of one 4 KiB header sector plus up to 124 KiB of image.
pub const ESP8266: Profile = Profile {
    chip: Chip::Esp8266,
    name: "esp8266",
    usb_serial_jtag: false,
    runtime: false,
    heap_reclaimed: 0,
    heap_main: 0,
    max_memory_bytes: 0,
    radio_heap: 0,
    max_image: 124 * 1024,
    uart0_tx: 1,
    uart0_rx: 3,
    gpio: span(0, 16),
    pwm_pins: &[],
    pwm_max_bits: 0,
    pwm_ref_tick: false,
    reserved: span(6, 11) | bit(1) | bit(3),
    input_only: 0,
    strapping: bit(0) | bit(2) | bit(15),
    adc: 0,
};

impl Chip {
    /// The chip's profile.
    pub const fn profile(self) -> &'static Profile {
        match self {
            Chip::Esp32 => &ESP32,
            Chip::Esp32s3 => &ESP32S3,
            Chip::Esp32c3 => &ESP32C3,
            Chip::Esp8266 => &ESP8266,
        }
    }
}

fn has(mask: u64, pin: u8) -> bool {
    pin < 64 && mask & (1u64 << pin) != 0
}

impl Profile {
    /// Total heap in bytes.
    pub const fn heap_total(&self) -> usize {
        self.heap_reclaimed + self.heap_main
    }

    /// Whether `pin` exists on the chip.
    pub fn exists(&self, pin: u8) -> bool {
        has(self.gpio, pin)
    }

    /// Whether `pin` is reserved for flash, PSRAM, USB or the console UART.
    pub fn is_reserved(&self, pin: u8) -> bool {
        has(self.reserved, pin)
    }

    /// Whether `pin` is a strapping pin (usable, but read at reset).
    pub fn is_strapping(&self, pin: u8) -> bool {
        has(self.strapping, pin)
    }

    /// Whether `pin` has no output driver.
    pub fn is_input_only(&self, pin: u8) -> bool {
        has(self.input_only, pin)
    }

    /// The `sys/board` description of this profile (S8), for a firmware
    /// implementing ABI `abi_version`.
    pub fn board_info(&self, abi_version: u16) -> BoardInfo<'static> {
        let flag = |on: bool, f: u8| if on { f } else { 0 };
        BoardInfo {
            abi_version,
            max_image: self.max_image,
            max_memory: self.max_memory_bytes,
            gpio: self.gpio,
            reserved: self.reserved,
            input_only: self.input_only,
            strapping: self.strapping,
            adc: self.adc,
            pwm_max_bits: self.pwm_max_bits,
            flags: flag(self.runtime, BOARD_RUNTIME)
                | flag(self.usb_serial_jtag, BOARD_USB_JTAG)
                | flag(self.pwm_ref_tick, BOARD_PWM_REF_TICK),
            uart_tx: self.uart0_tx,
            uart_rx: self.uart0_rx,
            pwm_pins: self.pwm_pins,
        }
    }

    /// Whether `pin` exists and has an output driver (ESP32 GPIO34–39 do
    /// not; esp-hal panics when one is routed to an output signal).
    pub fn can_output(&self, pin: u8) -> bool {
        self.exists(pin) && !has(self.input_only, pin)
    }

    /// Whether a payload may use `pin` in `direction`.
    pub fn check_pin(&self, pin: u8, direction: Direction) -> Result<(), AbiError> {
        if !self.exists(pin) {
            Err(AbiError::InvalidArgument)
        } else if self.is_reserved(pin)
            || (direction == Direction::Output && has(self.input_only, pin))
        {
            Err(AbiError::Unsupported)
        } else {
            Ok(())
        }
    }

    /// Whether `pin` has an ADC1 channel a payload may read.
    pub fn has_adc(&self, pin: u8) -> bool {
        has(self.adc, pin)
    }

    /// The GPIO of PWM output `channel` of ABI timer `timer`, if the chip
    /// maps it (timers 0–3, channels 0–1).
    pub fn pwm_pin(&self, timer: u8, channel: u8) -> Option<u8> {
        let ch = crate::periph::pwm::ledc_channel(timer, channel).ok()?;
        self.pwm_pins.get(usize::from(ch)).copied()
    }

    /// Pins a payload may use at all (exist and not reserved), ascending.
    pub fn payload_pins(&self) -> impl Iterator<Item = u8> + '_ {
        (0u8..64).filter(|&p| self.exists(p) && !self.is_reserved(p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [&Profile; 4] = [&ESP32, &ESP32S3, &ESP32C3, &ESP8266];

    #[test]
    fn console_uart_pins_are_reserved() {
        for p in ALL {
            assert!(p.exists(p.uart0_tx) && p.exists(p.uart0_rx), "{}", p.name);
            assert_eq!(p.check_pin(p.uart0_tx, Direction::Output), Err(AbiError::Unsupported));
            assert_eq!(p.check_pin(p.uart0_rx, Direction::Input), Err(AbiError::Unsupported));
        }
    }

    #[test]
    fn flash_psram_and_usb_pins_are_refused() {
        for pin in 6..=11 {
            assert_eq!(ESP32.check_pin(pin, Direction::Input), Err(AbiError::Unsupported));
        }
        assert_eq!(ESP32.check_pin(16, Direction::Output), Err(AbiError::Unsupported));
        assert_eq!(ESP32.check_pin(17, Direction::Output), Err(AbiError::Unsupported));
        for pin in (26..=37).chain([19, 20]) {
            assert_eq!(ESP32S3.check_pin(pin, Direction::Output), Err(AbiError::Unsupported));
        }
        for pin in 12..=19 {
            assert_eq!(ESP32C3.check_pin(pin, Direction::Output), Err(AbiError::Unsupported));
        }
        for pin in 6..=11 {
            assert_eq!(ESP8266.check_pin(pin, Direction::Input), Err(AbiError::Unsupported));
        }
    }

    #[test]
    fn missing_pins_are_invalid() {
        for pin in [20, 24, 28, 31, 40, 63, 64, 255] {
            assert_eq!(ESP32.check_pin(pin, Direction::Input), Err(AbiError::InvalidArgument));
        }
        for pin in [22, 25, 49, 64] {
            assert_eq!(ESP32S3.check_pin(pin, Direction::Input), Err(AbiError::InvalidArgument));
        }
        for pin in [22, 30, 64] {
            assert_eq!(ESP32C3.check_pin(pin, Direction::Input), Err(AbiError::InvalidArgument));
        }
        for pin in [17, 20, 64] {
            assert_eq!(ESP8266.check_pin(pin, Direction::Input), Err(AbiError::InvalidArgument));
        }
    }

    #[test]
    fn esp32_input_only_pins_refuse_output() {
        for pin in 34..=39 {
            assert_eq!(ESP32.check_pin(pin, Direction::Input), Ok(()));
            assert_eq!(ESP32.check_pin(pin, Direction::Output), Err(AbiError::Unsupported));
            assert!(!ESP32.can_output(pin), "esp32 {pin}");
        }
        assert!(ESP32.can_output(33));
        assert!(ESP32S3.can_output(39));
        assert!(!ESP32S3.can_output(22), "missing pin");
    }

    #[test]
    fn general_purpose_pins_are_allowed() {
        for pin in [0, 2, 4, 5, 12, 13, 14, 15, 18, 19, 21, 22, 23, 25, 26, 27, 32, 33] {
            assert_eq!(ESP32.check_pin(pin, Direction::Output), Ok(()), "esp32 {pin}");
        }
        for pin in (0..=18).chain([21, 38, 39, 40, 41, 42, 45, 46, 47, 48]) {
            assert_eq!(ESP32S3.check_pin(pin, Direction::Output), Ok(()), "esp32s3 {pin}");
        }
        for pin in 0..=11 {
            assert_eq!(ESP32C3.check_pin(pin, Direction::Output), Ok(()), "esp32c3 {pin}");
        }
        assert_eq!(ESP32C3.payload_pins().count(), 12);
        for pin in [0, 2, 4, 5, 12, 13, 14, 15, 16] {
            assert_eq!(ESP8266.check_pin(pin, Direction::Output), Ok(()), "esp8266 {pin}");
        }
        assert_eq!(ESP8266.payload_pins().count(), 9);
        assert!(ESP32S3.payload_pins().all(|p| !ESP32S3.is_reserved(p)));
    }

    #[test]
    fn heap_leaves_room_for_payload() {
        for p in ALL.into_iter().filter(|p| p.runtime) {
            let need = p.max_memory_bytes as usize
                + p.max_image as usize
                + INTERPRETER_RESERVE
                + p.radio_heap;
            assert!(p.heap_total() >= need, "{}: {} < {}", p.name, p.heap_total(), need);
            assert!(p.max_memory_bytes >= WASM_PAGE, "{}", p.name);
            // Page multiples, so a 64 KiB-page payload can use all of it.
            assert_eq!(p.max_memory_bytes % WASM_PAGE, 0, "{}", p.name);
        }
    }

    #[test]
    fn heapless_profiles_have_no_linear_memory() {
        // ESP32-C3 has no runtime but keeps its heap plan, so the check is
        // on heapless profiles rather than all non-runtime ones.
        for p in ALL.into_iter().filter(|p| p.heap_total() == 0) {
            assert!(!p.runtime, "{}", p.name);
            assert_eq!(p.max_memory_bytes, 0, "{}", p.name);
        }
        assert_eq!(ESP8266.heap_total(), 0);
    }

    #[test]
    fn max_image_fits_one_payload_slot() {
        // 256 KiB payload partition (ESP32 family) and ESP8266 region.
        let slot = crate::image::Region::new(0x4_0000).capacity();
        assert_eq!(slot, 124 * 1024);
        for p in ALL {
            assert!(p.max_image <= slot, "{}: {} > {}", p.name, p.max_image, slot);
        }
        assert_eq!(ESP8266.max_image, slot, "the ESP8266 accepts a whole slot");
    }

    #[test]
    fn x12_adc1_pins_per_chip() {
        let adc = |p: &Profile| (0u8..64).filter(|&n| p.has_adc(n)).collect::<heapless::Vec<u8, 64>>();
        assert_eq!(adc(&ESP32), [32, 33, 34, 35, 36, 37, 38, 39]);
        assert_eq!(adc(&ESP32S3), [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        assert!(adc(&ESP32C3).is_empty() && adc(&ESP8266).is_empty());
        for p in ALL {
            assert!((0u8..64).filter(|&n| p.has_adc(n)).all(|n| p.check_pin(n, Direction::Input).is_ok()), "{}", p.name);
        }
        assert!(!ESP32.has_adc(64) && !ESP32.has_adc(255));
    }

    #[test]
    fn profiles_match_chips() {
        let chips = [Chip::Esp32, Chip::Esp32s3, Chip::Esp32c3, Chip::Esp8266];
        for (chip, p) in chips.into_iter().zip(ALL) {
            assert_eq!(chip.profile().chip, chip);
            assert_eq!(p.chip, chip);
        }
        let usb: [bool; 4] = ALL.map(|p| p.usb_serial_jtag);
        assert_eq!(usb, [false, true, true, false]);
        let runtime: [bool; 4] = ALL.map(|p| p.runtime);
        assert_eq!(runtime, [true, true, false, false]);
        // PWM only where payloads run.
        let pwm: [(usize, u8, bool); 4] = ALL.map(|p| (p.pwm_pins.len(), p.pwm_max_bits, p.pwm_ref_tick));
        assert_eq!(pwm, [(8, 16, true), (8, 14, false), (0, 0, false), (0, 0, false)]);
    }

    #[test]
    fn e14_strapping_pins_per_chip_are_allowed() {
        let strap = |p: &Profile| (0u8..64).filter(|&n| p.is_strapping(n)).collect::<heapless::Vec<u8, 64>>();
        assert_eq!(strap(&ESP32), [0, 2, 5, 12, 15]);
        assert_eq!(strap(&ESP32S3), [0, 3, 45, 46]);
        assert_eq!(strap(&ESP32C3), [2, 8, 9]);
        assert_eq!(strap(&ESP8266), [0, 2, 15]);
        for p in ALL {
            for n in strap(p) {
                assert_eq!(p.check_pin(n, Direction::Output), Ok(()), "{} {n}", p.name);
            }
        }
        assert!(!ESP32.is_strapping(64) && !ESP32.is_strapping(255));
    }

    #[test]
    fn s8_board_info_matches_profile() {
        use middleware::loader::{BOARD_INFO_MAX, BoardInfo};
        for p in ALL {
            let b = p.board_info(3);
            let mut buf = [0u8; BOARD_INFO_MAX];
            let n = b.encode(&mut buf).unwrap();
            assert_eq!(BoardInfo::decode(&buf[..n]), Some(b), "{}", p.name);
            assert_eq!((b.max_image, b.uart_tx, b.uart_rx), (p.max_image, p.uart0_tx, p.uart0_rx));
            assert_eq!(b.pwm_pins, p.pwm_pins);
            assert!((0u8..64).all(|n| (b.gpio >> n & 1 == 1) == p.exists(n)));
        }
        let flags: [u8; 4] = ALL.map(|p| p.board_info(3).flags);
        assert_eq!(flags, [0b101, 0b011, 0b010, 0]);
        // The protocol crate's golden fixture is the ESP32-S3.
        assert_eq!(ESP32S3.board_info(3).strapping, 0x0000_6000_0000_0009);
        assert_eq!(ESP32S3.board_info(3).reserved, 0x0000_183F_FC18_0000);
    }
}
