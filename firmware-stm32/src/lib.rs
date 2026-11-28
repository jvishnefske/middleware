//! STM32 base firmware, pure core (`stm32_base`).
//!
//! The first STM32 board is the BigTreeTech EBB42 CAN v1.2 (STM32G0B1CB,
//! Cortex-M0+, 128 KB flash, 144 KB RAM). The firmware answers the
//! middleware serial protocol on the USB-C port (CDC-ACM): `sys/info`,
//! `sys/board` and `sys/manifest`; there is no payload runtime yet, so it
//! reports `+no-wasm` and payload capacity 0, like the micro:bit firmware.
//!
//! * [`board`] — the board profile: chip name, [`board::board_info`] from
//!   `board-pins`' [`board_pins::boards::EBB42`], the manifest, the node id.
//! * [`requests`] — the `sys/*` handler and [`requests::Node`], which puts
//!   an application's keys beside it.
//! * [`link`] — one step of the link loop: a frame in, its response out.
//! * [`usb`] — USB identity and the zero-length-packet rule.
//!
//! Everything here is pure and tested on the host; `src/main.rs` (feature
//! `shell`) only moves bytes between the USB port and [`link::respond`].
#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

#[cfg(test)]
extern crate std;

pub mod board;
pub mod link;
pub mod requests;
pub mod usb;
