//! Pure core of the RP base firmware (Raspberry Pi Pico, RP2040, and
//! Pico 2, RP2350).
//!
//! Everything here is deterministic decision logic and storage effects over
//! a generic NOR flash, with no hardware access, so it is unit tested on the
//! host (`cargo test --lib`, no chip feature). The request decisions are the
//! ESP base firmware's own: [`service`] builds on `esp_base::dispatch`
//! (requests, loader, trust), `esp_base::store` and `esp_base::image`
//! (the two-slot payload store), `esp_base::trust_store` (the pinned key)
//! and `esp_base::start` (the heap check), which the `esp-base` crate
//! exports without any ESP dependency (as it does for the ESP8266
//! firmware). This crate adds what differs on the RP chips:
//!
//! * [`layout`] — the flash maps: firmware (on the RP2350 a partition
//!   table, A/B slots and the update record), payload store and trust
//!   record, checked against `memory-rp2040.x` and `memory-rp2350.x`.
//! * [`chip`] — per-chip profiles: name, flash map, heap plan, WASM memory
//!   and image caps, the Pico board's pin policy and ADC pins.
//! * [`node`] — the node id and USB serial number from the chip unique id
//!   (`sys/info`'s node and the payload's `node_id`).
//! * [`pins`] — the payload's GPIO modes under the board policy (the
//!   esp-base pin bank's rules), as a value the device shell applies.
//! * [`periph`] — the payload SPI and I2C buses: the chips' GPIO function
//!   tables, pin claims, I2C timing and transfer plans, the SPI divider and
//!   FIFO pump, and the time bound of every transfer.
//! * [`fw_update`] — the `sys/fw/*` seam for firmware updates: the
//!   reserved keys, the backend trait, the context a backend gets and the
//!   self-test that gates confirming a trial.
//! * [`ota`] — A/B firmware OTA on the RP2350: the image metadata the
//!   build places in the first sector, the partition table, the factory
//!   image, the boot ROM seam and `FwPlatform` on the flash, and the
//!   backend that runs `middleware::fw_ota`.
//! * [`flash`] — NOR flash adapters: one sector per erase call, so
//!   interrupts (USB) are serviced between sectors, and reads through the
//!   RP2350's untranslated flash window.
//! * [`service`] — the request service: `sys/fw/*` to the update backend,
//!   everything else through `esp_base::dispatch`, with the storage actions
//!   performed on the flash and notes for `sys/log`.
//! * `requests` (with the `wasm` runtime, and in tests) — one request
//!   while a payload may run: `esp_base::serve` routing (`sys` keys and
//!   unserved keys to the service, served keys to `Payload::request` with a
//!   512-byte reply, `Unavailable` for a stopped or trapped payload's keys,
//!   a trap answered `Internal` and the payload stopped).
//! * [`usb`] — the CDC-ACM zero-length-packet rule and the USB identity.
//! * `budget` (with the `wasm` runtime, and in tests) — the limits
//!   payloads run under and the largest image a heap admits.
//!
//! The imperative shell (`src/main.rs` and `src/shell/`) is only compiled
//! with a chip feature (`rp2040`, `rp2350`). It owns USB, flash, GPIO,
//! the ADC and the SPI and I2C controllers, moves messages between the CDC link and the [`service`], and
//! runs the payload.
#![no_std]
#![forbid(unsafe_code)]

#[cfg(any(test, feature = "wasm"))]
pub mod budget;
pub mod chip;
pub mod flash;
pub mod fw_update;
pub mod layout;
pub mod node;
pub mod ota;
pub mod periph;
pub mod pins;
#[cfg(test)]
mod ram_flash;
#[cfg(any(test, feature = "wasm"))]
pub mod requests;
pub mod service;
pub mod usb;
