//! The portable embassy shell: serial link tasks, the message bus and the
//! controller, shared by every embassy-based board (the ESP32 family here,
//! the Raspberry Pi boards in their own crate).
//!
//! Compiled with the `embassy` cargo feature, which pulls in only
//! `embassy-time`, `embassy-sync`, `embassy-futures` and
//! `embedded-io-async` (no chip HAL), so it builds for any target with an
//! embassy time driver and a `critical-section` implementation, for
//! example `thumbv6m-none-eabi` (RP2040). With `wasm` as well, the
//! controller also runs payloads; that needs `alloc` and pointer-width
//! atomics (`thumbv8m.main-none-eabihf` for the RP2350, not `thumbv6m`).
//!
//! * [`bus`] — the [`bus::Bus`] channels between link tasks and the
//!   controller, generic over the board's port set ([`bus::Port`], or
//!   [`bus::Single`] for one port).
//! * [`link`] — [`link::serve`] runs the protocol on one
//!   `embedded-io-async` stream; [`link::prime`] writes the leading
//!   delimiter.
//! * [`controller`] — [`controller::Controller`] dispatches requests,
//!   performs storage actions on a [`crate::store::PayloadStore`] over any
//!   NOR flash and (with `wasm`) runs the payload on the board's
//!   [`controller::PayloadDevice`] and [`controller::EventSource`]. Its
//!   `run` returns the [`crate::dispatch::RebootMode`] once a `sys/reboot`
//!   reply is flushed; the board performs the reboot.
//!
//! What a board supplies: a `'static` [`bus::Bus`], one task per port
//! calling [`link::prime`] then [`link::serve`], the flash and its payload
//! region for [`crate::store::PayloadStore::new`], a
//! [`crate::dispatch::Context`] (including which reboot modes it
//! implements), and the reboot itself.

pub mod bus;
pub mod controller;
pub mod link;

/// Milliseconds since boot on the embassy clock, wrapping (the payload
/// tick clock).
pub fn now_ms() -> u32 {
    embassy_time::Instant::now().as_millis() as u32
}
