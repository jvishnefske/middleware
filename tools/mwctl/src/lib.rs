//! `mwctl`: host CLI for the ESP base firmware's serial protocol.
//!
//! Functional core, imperative shell:
//!
//! * [`cli`] — pure argument parsing into a [`cli::Cli`] value, and the help
//!   text.
//! * [`plan`] — the upload request sequence (`sys/loader/begin`, chunks,
//!   `sys/loader/commit`) built from the image with the
//!   [`middleware::loader`] encoders, including the `--corrupt-chunk` test
//!   hook.
//! * [`sign`] — key seeds, public keys and signed commits: SHA-256 of the
//!   image and an Ed25519 signature over the device's 54-byte message.
//! * [`link`] — the [`link::Transport`] byte-stream trait and
//!   [`link::Link`]: frames requests with [`middleware::serial::frame`],
//!   cuts the reply stream with its `Splitter` (resynchronising on garbage
//!   such as a 74880-baud ROM banner that has no delimiter before the next
//!   frame), matches responses by correlation id, and retries timed-out
//!   attempts with a fresh id.
//! * [`bridge`] — the serial framing to the USB↔ESP-NOW bridge (an
//!   ESP8266, `tools/espnow-bridge`): COBS + CRC-16 frames carrying raw
//!   ESP-NOW frames with their MACs, `Hello`, `Stats` and send results.
//! * [`air`] — [`air::AirLink`], an [`link::Endpoint`] that reaches a device
//!   over ESP-NOW through the bridge with `middleware::rpc` requests:
//!   unicast, retransmitted with the same id, small chunks.
//! * [`device`] — the protocol operations (`info`, `upload` signed or not,
//!   `trust info`, `trust claim`, `start`, `stop`, monitor events) over any
//!   transport.
//! * [`report`] — human-readable formatting of device info and
//!   publications.
//! * [`error`] — [`error::Error`], with context-specific explanations of
//!   device status codes.
//!
//! The binary (`src/main.rs`, `src/port.rs`) is the shell: it opens the one
//! serial port it is given with the `serialport` crate, drives the control
//! lines, and prints. The core is unit tested against an in-memory fake
//! device that runs the real [`middleware::loader::Loader`] state machine
//! and the device's trust rules.
#![forbid(unsafe_code)]

pub mod air;
pub mod bridge;
pub mod cli;
pub mod device;
pub mod error;
pub mod link;
pub mod plan;
pub mod report;
pub mod sign;

#[cfg(test)]
mod fake;
