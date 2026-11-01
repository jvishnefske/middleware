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
//! * [`layout`] — the flash maps: firmware (two slots reserved on the
//!   RP2350 for A/B firmware OTA), payload store and trust record, checked
//!   against `memory-rp2040.x` and `memory-rp2350.x`.
//! * [`chip`] — per-chip profiles: name, flash map, heap plan, WASM memory
//!   and image caps, the Pico board's pin policy and ADC pins.
//! * [`node`] — the node id and USB serial number from the chip unique id.
//! * [`pins`] — the payload's GPIO modes under the board policy (the
//!   esp-base pin bank's rules), as a value the device shell applies.
//! * [`fw_update`] — the `sys/fw/*` seam for A/B firmware updates: the
//!   reserved keys and the backend trait, with the `Unavailable` stub.
//! * [`flash`] — a NOR flash adapter that erases one sector per driver call,
//!   so interrupts (USB) are serviced between sectors.
//! * [`service`] — the request service: `sys/fw/*` to the update backend,
//!   everything else through `esp_base::dispatch`, with the storage actions
//!   performed on the flash and notes for `sys/log`.
//! * [`usb`] — the CDC-ACM zero-length-packet rule and the USB identity.
//! * `budget` (with the `wasm` runtime, and in tests) — the limits
//!   payloads run under and the largest image a heap admits.
//!
//! The imperative shell (`src/main.rs` and `src/shell/`) is only compiled
//! with a chip feature (`rp2040`, `rp2350`). It owns USB, flash, GPIO and
//! the ADC, moves messages between the CDC link and the [`service`], and
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
pub mod pins;
#[cfg(test)]
mod ram_flash;
pub mod service;
pub mod usb;
