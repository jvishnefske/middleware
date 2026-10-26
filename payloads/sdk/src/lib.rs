//! Safe `no_std` SDK for writing middleware WASM payloads in Rust.
//!
//! A payload is a `cdylib` built for `wasm32-unknown-unknown` that the base
//! firmware (or `payload-sim`, or the browser simulator) runs in a sandbox.
//! The contract between the two sides lives in [`middleware::wasm_abi`]; this
//! crate wraps it so payload code never touches raw pointers or `i32` error
//! codes.
//!
//! # Writing a payload
//!
//! Implement [`Payload`] and hand the type to [`payload!`], which generates
//! every export the ABI defines (`init`, `tick`, `mw_alloc`, `on_message`,
//! `on_irq`; `memory` is exported by the linker):
//!
//! ```ignore
//! use mw_payload::{Payload, Slot};
//!
//! struct App;
//!
//! impl Payload for App {
//!     fn init() -> Self {
//!         mw_payload::log("hello");
//!         let _ = mw_payload::set_tick(1000);
//!         App
//!     }
//!     fn tick(&mut self, now_ms: u32) {
//!         let _ = mw_payload::publish("app/uptime", &now_ms.to_le_bytes());
//!     }
//! }
//!
//! mw_payload::payload!(App);
//! ```
//!
//! # API
//!
//! * [`log`] / [`LogLine`] — log lines (truncated to [`MAX_LOG`] bytes).
//! * [`publish`], [`subscribe`] returning a [`Slot`] — pub/sub.
//! * [`now_ms`], [`set_tick`] — time and the periodic [`Payload::tick`].
//! * [`gpio::Output`], [`gpio::Input`] with [`Pull`] — digital I/O, also
//!   usable through the `embedded-hal` 1.0 digital traits.
//!
//! Added with ABI 2 (interrupts and DMA; requires firmware implementing
//! [`ABI_VERSION`] 2):
//!
//! * [`irq::Event`] — GPIO edges, timer alarms, DMA completion and UART
//!   receive, delivered to [`Payload::on_irq`] from the main loop (never
//!   from an ISR) through a bounded, coalescing queue; [`irq::now_us`] and
//!   [`irq::overflow`]. The [`irq`] module documents the interrupt model.
//! * [`gpio::Input::enable_irq`] / [`gpio::Input::disable_irq`] with
//!   [`gpio::Edge`].
//! * [`timer::Timer`] — four hardware timers, periodic or one-shot.
//! * [`spi::Spi`] with [`spi::SpiConfig::builder`] — SPI master transfers on
//!   DMA: start, wait for `DmaDone`, read. There is deliberately no blocking
//!   `embedded-hal` `SpiBus` (see [`spi`]).
//! * [`uart::Uart`] — the secondary UART: DMA transmit, buffered receive.
//! * [`dma::Transfer`] — the pure state machine behind `Busy`.
//!
//! DMA never targets guest memory: the host copies transmit data into
//! firmware-owned buffers before `transfer` / `write` return, and copies
//! received data back when the payload reads it.
//!
//! Added with ABI 3 (requires firmware implementing [`ABI_VERSION`] 3):
//!
//! * [`pwm::Pwm`] / [`pwm::PwmChannel`] — PWM timers whose channels
//!   implement the `embedded-hal` 1.0 `SetDutyCycle` trait.
//! * [`adc::AnalogPin`] — 12-bit analog readings, with an integer
//!   millivolt conversion.
//! * [`i2c::I2c`] — a blocking I2C controller implementing the
//!   `embedded-hal` 1.0 `I2c` trait (write, read, write-read, and
//!   transactions of the shape "writes, then reads").
//! * [`AbiError`] — re-exported from `middleware::wasm_abi`, so the codes are
//!   defined exactly once.
//!
//! Received messages are copied by the host into one fixed receive buffer of
//! [`MAX_MESSAGE`] bytes that `mw_alloc` hands out; there is no heap.
//!
//! # The `unsafe` exception (review this)
//!
//! The project forbids `unsafe`. Talking to a WASM host cannot be done
//! without it, so this crate is `#![deny(unsafe_code)]` and the exception is
//! confined to the private module `sys` (`src/sys.rs`), the only module
//! with `#[allow(unsafe_code)]`. It contains exactly four things, each with
//! a `SAFETY:` comment:
//!
//! 1. The `unsafe extern "C"` block declaring the `"mw"` host imports. Every
//!    import that only *reads* guest memory is declared `safe fn`, because
//!    the host validates every pointer/length pair against linear memory and
//!    never writes into guest memory during those calls (`spi_transfer` and
//!    `uart_write` copy into firmware-owned DMA buffers).
//! 2. Three `unsafe { }` call sites, for the ABI 2 imports `dma_read` and
//!    `uart_read` and the ABI 3 import `i2c_transfer`, which *write* into
//!    guest memory and are therefore declared without `safe`. Each sits in a
//!    private wrapper that takes the destination as `&mut [u8]` and passes
//!    exactly that slice's pointer and length, so the host can only write
//!    into memory the caller holds exclusively for the duration of the call.
//! 3. `unsafe impl Sync for GuestCell<T>`, compiled only for `wasm32`
//!    without the `atomics` target feature, where there is exactly one
//!    thread. This lets payload state live in a `static` `RefCell`.
//! 4. The [`payload!`] macro, defined in `sys`, whose expansion carries
//!    `#[unsafe(no_mangle)]` on the five generated exports. `no_mangle` is
//!    an unsafe attribute only because of symbol clashes; the names are the
//!    fixed ABI names. The attribute is written in this crate and the
//!    expansion has no `#[allow(unsafe_code)]`, so a payload crate may be
//!    `#![forbid(unsafe_code)]` (requirement P5; see [`payload!`]).
//!
//! On non-wasm targets the imports are replaced by inert stubs (`log` does
//! nothing, `now_ms` / `now_us` return 0, the rest return
//! [`AbiError::Unsupported`]) so the pure logic can be unit tested on the
//! host.
//!
//! # Features
//!
//! * `panic-handler` (default) — a `#[panic_handler]` that logs
//!   `panic at <file>:<line>` through the `log` import and traps with
//!   `unreachable`. Only compiled for `wasm32`. Disable it to provide your own.
#![cfg_attr(not(test), no_std)]
#![deny(unsafe_code)]
#![deny(missing_docs)]

pub mod adc;
pub mod dma;
pub mod gpio;
pub mod i2c;
pub mod irq;
mod logline;
#[cfg(all(feature = "panic-handler", target_arch = "wasm32", not(test)))]
mod panic;
pub mod pwm;
pub mod rt;
pub mod spi;
#[allow(unsafe_code)]
mod sys;
pub mod timer;
pub mod uart;

pub use logline::LogLine;
pub use middleware::wasm_abi::{
    ABI_VERSION, AbiError, MAX_LOG, MAX_MESSAGE, MAX_SUBSCRIPTIONS, MIN_TICK_MS, Pull,
};
pub use sys::GuestCell;

/// Maps an import result to `Ok(value)` for non-negative values and to the
/// matching [`AbiError`] otherwise.
///
/// A negative code this SDK does not know (a newer host) maps to
/// [`AbiError::Unsupported`].
pub const fn check(code: i32) -> Result<i32, AbiError> {
    if code >= 0 {
        Ok(code)
    } else {
        match AbiError::from_code(code) {
            Some(e) => Err(e),
            None => Err(AbiError::Unsupported),
        }
    }
}

/// Converts a byte length to the ABI's `i32`.
fn len_i32(len: usize) -> Result<i32, AbiError> {
    i32::try_from(len).map_err(|_| AbiError::OutOfBounds)
}

/// Converts a guest address to the ABI's `i32` pointer.
///
/// Exact on wasm32, where addresses are 32 bits. On the host (tests only) the
/// value is truncated, which is harmless because the import stubs ignore it.
fn ptr_i32(bytes: &[u8]) -> i32 {
    bytes.as_ptr() as usize as u32 as i32
}

/// Writes one log line. Lines longer than [`MAX_LOG`] bytes are truncated by
/// the host.
pub fn log(line: &str) {
    if let Ok(len) = len_i32(line.len()) {
        sys::log(ptr_i32(line.as_bytes()), len);
    }
}

/// Publishes `data` on `key`.
///
/// Fails with [`AbiError::InvalidArgument`] without calling the host when
/// `data` exceeds [`MAX_MESSAGE`]; the host rejects reserved (`sys/...`) or
/// malformed keys with [`AbiError::InvalidKey`].
pub fn publish(key: &str, data: &[u8]) -> Result<(), AbiError> {
    if data.len() > MAX_MESSAGE {
        return Err(AbiError::InvalidArgument);
    }
    let code = sys::publish(
        ptr_i32(key.as_bytes()),
        len_i32(key.len())?,
        ptr_i32(data),
        len_i32(data.len())?,
    );
    check(code).map(|_| ())
}

/// A subscription handle, passed back to [`Payload::on_message`] so a payload
/// with several subscriptions can tell them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Slot(u8);

impl Slot {
    /// Parses a host-provided slot; `None` unless `0 <= raw < MAX_SUBSCRIPTIONS`.
    pub fn from_raw(raw: i32) -> Option<Self> {
        let idx = usize::try_from(raw).ok()?;
        if idx < MAX_SUBSCRIPTIONS {
            u8::try_from(idx).ok().map(Slot)
        } else {
            None
        }
    }

    /// The slot index, `0..MAX_SUBSCRIPTIONS`.
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// Subscribes to `key` (which may contain wildcards understood by the host)
/// and returns the slot that [`Payload::on_message`] will report.
///
/// Fails with [`AbiError::NoResources`] once [`MAX_SUBSCRIPTIONS`] are active.
pub fn subscribe(key: &str) -> Result<Slot, AbiError> {
    let code = sys::subscribe(ptr_i32(key.as_bytes()), len_i32(key.len())?);
    let raw = check(code)?;
    Slot::from_raw(raw).ok_or(AbiError::Unsupported)
}

/// Milliseconds since the payload started; wraps at `u32::MAX`.
pub fn now_ms() -> u32 {
    sys::now_ms() as u32
}

/// Sets the period of [`Payload::tick`] in milliseconds; `0` disables it.
///
/// Periods below [`MIN_TICK_MS`] (other than 0) are rejected by the host
/// with [`AbiError::InvalidArgument`]; periods above `i32::MAX` are rejected
/// here with the same error.
pub fn set_tick(period_ms: u32) -> Result<(), AbiError> {
    let period = i32::try_from(period_ms).map_err(|_| AbiError::InvalidArgument)?;
    check(sys::set_tick(period)).map(|_| ())
}

/// A payload application. [`payload!`] wires an implementation to the ABI
/// exports.
///
/// The host calls `init` once, then `tick` every [`set_tick`] period,
/// `on_message` for every sample on a [`subscribe`]d key and `on_irq` for
/// every queued interrupt or DMA event. Calls never overlap or nest. Every
/// call is fuel-metered (see [`middleware::wasm_abi::FUEL_PER_CALL`]), so
/// each must return promptly.
pub trait Payload: Sized {
    /// Builds the application state; called once after instantiation. This is
    /// the place to subscribe, configure GPIO and call [`set_tick`].
    fn init() -> Self;

    /// Called every tick period with the host's [`now_ms`].
    fn tick(&mut self, _now_ms: u32) {}

    /// Called with each received message.
    fn on_message(&mut self, _slot: Slot, _data: &[u8]) {}

    /// Called from the main loop with each interrupt or DMA event (ABI 2);
    /// see [`irq`] for the model. Events the SDK cannot decode are dropped
    /// before reaching this method.
    fn on_irq(&mut self, _event: irq::Event) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_maps_every_abi_code() {
        assert_eq!(check(0), Ok(0));
        assert_eq!(check(7), Ok(7));
        for e in [
            AbiError::InvalidArgument,
            AbiError::OutOfBounds,
            AbiError::NoResources,
            AbiError::Unsupported,
            AbiError::Transport,
            AbiError::InvalidKey,
            AbiError::Busy,
        ] {
            assert_eq!(check(e.code()), Err(e));
        }
        assert_eq!(check(-99), Err(AbiError::Unsupported));
        assert_eq!(check(i32::MIN), Err(AbiError::Unsupported));
    }

    #[test]
    fn slot_range_follows_max_subscriptions() {
        assert_eq!(Slot::from_raw(0).map(Slot::index), Some(0));
        let last = MAX_SUBSCRIPTIONS as i32 - 1;
        assert_eq!(Slot::from_raw(last).map(Slot::index), Some(last as usize));
        assert_eq!(Slot::from_raw(MAX_SUBSCRIPTIONS as i32), None);
        assert_eq!(Slot::from_raw(-1), None);
    }

    #[test]
    fn publish_rejects_oversized_data_before_the_host() {
        let big = [0u8; MAX_MESSAGE + 1];
        assert_eq!(publish("app/x", &big), Err(AbiError::InvalidArgument));
    }

    #[test]
    fn host_stubs_report_unsupported() {
        // Off wasm32 the imports are inert stubs.
        assert_eq!(publish("app/x", b"1"), Err(AbiError::Unsupported));
        assert_eq!(subscribe("app/x"), Err(AbiError::Unsupported));
        assert_eq!(set_tick(100), Err(AbiError::Unsupported));
        assert_eq!(set_tick(u32::MAX), Err(AbiError::InvalidArgument));
        assert_eq!(now_ms(), 0);
        log("no-op on the host");
    }
}
