//! ESP8266 base firmware: the hardware-independent part.
//!
//! The ESP8266 is the fourth base-firmware target. It speaks the same
//! serial protocol as the ESP32 firmware in `firmware/` (COBS frames with a
//! CRC-16 on UART0 at 115200 baud) but has no WASM runtime, like the
//! ESP32-C3 build without the `wasm` feature: `sys/info`, the loader and
//! flash storage work, the firmware version is `<ver>+no-wasm`, and
//! `sys/payload/start` is answered `NotFound`.
//!
//! Decisions are reused from `esp_base` (the pure core of `firmware/`):
//! request dispatch, the payload store and the chip profile
//! `esp_base::chip::ESP8266`. This crate adds what differs on the ESP8266:
//!
//! * [`flash`] — an `embedded-storage` NOR flash driver over a raw-flash
//!   seam, confined to the payload region `0x100000..0x140000`.
//! * [`link`] — a blocking frame link over an embedded-hal 0.2 serial port.
//! * [`controller`] — the synchronous controller: dispatch, storage
//!   effects, replies and `sys/log` / `sys/payload/status` publications.
//!
//! All three are generic over their hardware and unit tested on the host
//! (`./test.sh`). The imperative shell, `src/main.rs`, binds them to the
//! ESP8266 HAL and only builds for `xtensa-esp8266-none-elf`.
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod controller;
pub mod flash;
pub mod link;
