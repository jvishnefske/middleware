//! Serial transport (`serial` feature, `no_std`, no heap).
//!
//! Runs the middleware over a byte stream: a UART behind a USB-serial bridge
//! (CP2102, CH340, FTDI), a native USB-Serial-JTAG / CDC-ACM port, or the
//! browser's Web Serial API (through the `protocol-wasm` bindings).
//!
//! * [`cobs`] — byte stuffing so `0x00` can delimit frames.
//! * [`frame`] — the frame layout (request, response, publish) with a
//!   CRC-16, and a [`Splitter`](frame::Splitter) that cuts a stream into
//!   frames while skipping non-frame text such as ROM boot banners.
//! * `link` (feature `serial`) — `SerialLink`, implementing the core
//!   traits over `embedded_io_async::{Read, Write}`.

pub mod cobs;
pub mod frame;
#[cfg(feature = "serial")]
pub mod link;
