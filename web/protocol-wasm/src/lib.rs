//! Browser bindings for the middleware serial protocol, the payload loader
//! and the payload simulator.
//!
//! The web site (`web/`) talks to devices through this crate compiled to
//! WASM with `wasm-pack`, so the browser runs the same Rust frame codec,
//! CRCs, loader rules and payload runtime as the firmware instead of a
//! second implementation.
//!
//! * [`decoder`] — streaming decoder: serial bytes → frames, console text
//!   and rejected segments.
//! * [`encode`] — request, publish and response frame encoders.
//! * [`loader`] — upload plans (begin/chunk/commit bodies with CRC-32),
//!   `sys/info` decoding, payload status names, the signed-payload helpers
//!   (SHA-256, signed message, signed commit body, `sys/trust/info`
//!   names), and a device-side loader with a simulated root of trust for
//!   fakes.
//! * [`simulator`] (`simulator` feature, default) — runs a payload in
//!   `wasm-host` against a virtual device: GPIO with edge interrupts, ABI 2
//!   hardware timers on a JS-supplied clock, an SPI device (loopback or a
//!   pattern), a UART terminal and the bounded interrupt event queue that
//!   feeds `on_irq`.
//! * `bindings` — the `wasm-bindgen` exports (`Decoder`, `encodeRequest`,
//!   `uploadPlan`, `signedMessage`, `decodeTrustInfo`, `Simulator`,
//!   `abi()`, ...). It only converts values.
//!
//! The pure modules use owned `Vec`/`String` results: this crate runs in a
//! browser, not on a microcontroller, and owned values are what cross into
//! JS. They are unit tested natively with `cargo test -p protocol-wasm`.
#![forbid(unsafe_code)]

pub mod bindings;
pub mod decoder;
pub mod encode;
pub mod loader;
#[cfg(feature = "simulator")]
pub mod simulator;
