//! Transport-agnostic messaging middleware.
//!
//! One set of traits — [`Publisher`], [`Subscriber`], [`Requester`] and
//! [`Responder`] — is implemented by several transports, so application code
//! written against the traits runs unchanged on a microcontroller talking BLE
//! or on a cloud service talking Zenoh or gRPC inside an Istio mesh.
//!
//! # Layers
//!
//! * [`key`] — validated, transport-neutral addresses (`sensor/temperature`).
//! * [`transport`] — byte-level traits. Every call takes caller-owned buffers,
//!   so implementations need no heap.
//! * [`codec`] — [`Codec`] turns typed messages into bytes; [`typed::Client`]
//!   combines a transport with a codec.
//! * [`serial`] — COBS-framed wire format for byte streams (always built, pure);
//!   its async `SerialLink` transport needs the `serial` feature.
//! * [`loader`] — system keys and the pure state machine for installing WASM
//!   payloads on a device over any transport; [`loader::trust`] holds the
//!   signed-payload wire types and rules (its crypto needs `trust`).
//! * [`wasm_abi`] — the import/export contract between payloads and firmware.
//! * [`stream`] — chunked frame streams over keys (always built, pure): the
//!   chunk wire format, the source chunker, the sink state machine and a
//!   test pattern, behind the ABI 4 DMA routes.
//! * [`crc`] — CRC-16 for frames, CRC-32 for payload images.
//! * Backends, each behind a cargo feature:
//!
//! | feature    | module      | target            | needs                     |
//! |------------|-------------|-------------------|---------------------------|
//! | `ble`      | `ble`       | `no_std`, no heap | a radio binding (traits)  |
//! | `serial`   | `serial::link` | `no_std`, no heap | `embedded-io-async`    |
//! | `espnow`   | `espnow`    | `no_std`, no heap | a radio binding (trait)   |
//! | `trust`    | `loader::sha256`, `loader::trust` checks | `no_std`, no heap | sha2, ed25519-dalek (verify only) |
//! | `zenoh`    | `zenoh`     | `std`             | tokio, zenoh              |
//! | `grpc`     | `grpc`      | `std`             | tokio, tonic              |
//! | `istio`    | `grpc::istio` | `std`           | `grpc`                    |
//! | `postcard` | `codec::postcard` | `no_std`    | serde                     |
//! | `prost`    | `codec::prost`    | `no_std` + `alloc` | prost              |
//!
//! With no features the crate is `#![no_std]`, allocation free and builds for
//! `thumbv6m-none-eabi`. Enabling `zenoh` or `grpc` turns on `std`; the
//! embedded features never do, so firmware never links cloud dependencies and
//! cloud builds never compile the BLE stack unless asked.
#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]
#![allow(async_fn_in_trait)]

pub mod codec;
pub mod dedup;
pub mod crc;
pub mod key;
pub mod loader;
pub mod serial;
pub mod stream;
pub mod transport;
pub mod typed;
pub mod wasm_abi;

#[cfg(feature = "ble")]
pub mod ble;
#[cfg(feature = "espnow")]
pub mod espnow;
#[cfg(feature = "grpc")]
pub mod grpc;
#[cfg(feature = "zenoh")]
pub mod zenoh;

pub use codec::Codec;
pub use key::{AsKey, Filter, Key, KeyBuf, KeyError, KeyFilter, KeyFilterBuf};
pub use transport::{
    Handler, HandlerError, Limits, Publisher, Requester, Responder, Sample, SampleMeta, SenderId,
    Subscribe, Subscriber,
};
