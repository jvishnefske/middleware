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
//! * Backends, each behind a cargo feature:
//!
//! | feature    | module      | target            | needs                     |
//! |------------|-------------|-------------------|---------------------------|
//! | `ble`      | `ble`       | `no_std`, no heap | a radio binding (traits)  |
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
pub mod key;
pub mod transport;
pub mod typed;

#[cfg(feature = "ble")]
pub mod ble;
#[cfg(feature = "grpc")]
pub mod grpc;
#[cfg(feature = "zenoh")]
pub mod zenoh;

pub use codec::Codec;
pub use key::{Key, KeyError};
pub use transport::{Handler, HandlerError, Publisher, Requester, Responder, Sample, Subscriber};
