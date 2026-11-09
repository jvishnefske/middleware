//! Pure core of the ESP base firmware.
//!
//! Everything here is deterministic decision logic with no hardware access,
//! so it is unit tested on the host (`cargo test --lib`, no chip feature):
//!
//! * [`chip`] — per-chip profiles: name, heap plan, WASM memory limit,
//!   largest payload image, and the GPIO tables that decide which pins a
//!   payload may touch (flash/PSRAM, USB D+/D-, and console UART pins are
//!   refused).
//! * [`image`] — layout of the payload flash partition: a header sector
//!   (magic, length, CRC-32) followed by the image, erase spans, and a
//!   word aligner that turns in-order loader chunks into 4-byte aligned flash
//!   writes.
//! * [`store`] — [`store::PayloadStore`], the loader's storage effects
//!   (erase, ordered writes, activate) and boot-time verification over any
//!   `embedded-storage` NOR flash; tested against a RAM flash model.
//! * [`dispatch`] — request dispatch: `(State, request) -> (State, reply,
//!   Action)`, built on [`middleware::loader::Loader`], plus the payload
//!   lifecycle transitions and publication routing.
//! * [`periph`] — the ABI 2 peripheral decisions: pin claims, the timer
//!   deadline table, the DMA channel state machine, the UART receive ring,
//!   and the events ISRs and completions push.
//!
//!
//! With the `embassy` feature (no chip HAL) the lib also carries the
//! portable half of the imperative shell:
//!
//! * `embassy` — the message bus, serial link tasks and the controller,
//!   generic over the flash, the port set and (with `wasm`) the board's
//!   payload device and interrupt event source.
//!
//! The ESP imperative shell (`src/main.rs` and `src/shell/`) is only
//! compiled with a chip feature (`esp32`, `esp32s3`, `esp32c3`), which also
//! enables `embassy`. It owns the UARTs, USB-Serial-JTAG, flash and GPIO,
//! instantiates the controller with them, implements the payload device,
//! and performs the reboot the controller returns.
#![no_std]
#![forbid(unsafe_code)]

#[cfg(all(feature = "embassy", feature = "wasm"))]
extern crate alloc;

pub mod chip;
pub mod dispatch;
#[cfg(feature = "embassy")]
pub mod embassy;
pub mod image;
pub mod periph;
pub mod store;
