//! Transport-agnostic messaging middleware.
//!
//! One set of traits — [`Publisher`], [`Subscriber`], [`Requester`],
//! [`Gather`] and [`Responder`], plus [`discovery::Announce`] and
//! [`discovery::Browse`] — is implemented by several transports, so
//! application code written against the traits runs unchanged on a
//! microcontroller talking BLE or on a cloud service talking Zenoh or gRPC
//! inside an Istio mesh.
//!
//! # Layers
//!
//! * [`key`] — validated, transport-neutral addresses (`sensor/temperature`).
//! * [`transport`] — byte-level traits (the 0.5 contract: request metadata
//!   and options, gather, declared delivery, gaps, split). Every call takes
//!   caller-owned buffers, so implementations need no heap.
//! * [`discovery`] — service records, the `Announce` / `Browse` traits, the
//!   pure [`discovery::Directory`] cache and pub/sub beacons (always built,
//!   pure).
//! * [`dns`] — the DNS message codec, decode and encode (always built,
//!   pure); unicast DNS-SD in `dns::unicast` needs `dns-sd`.
//! * [`codec`] — [`Codec`] turns typed messages into bytes; [`typed::Client`]
//!   combines a transport with a codec, [`typed::TypedHandler`] serves typed
//!   requests and [`typed::TypedBrowser`] decodes typed service descriptions.
//! * [`serial`] — COBS-framed wire format for byte streams (always built, pure);
//!   its async `SerialLink` transport needs the `serial` feature.
//! * [`loader`] — system keys and the pure state machine for installing WASM
//!   payloads on a device over any transport; [`loader::trust`] holds the
//!   signed-payload wire types and rules (its crypto needs `trust`).
//! * [`wasm_abi`] — the import/export contract between payloads and firmware.
//! * [`stream`] — chunked frame streams over keys (always built, pure): the
//!   chunk wire format, the source chunker, the sink state machine, a test
//!   pattern and looped stored frames (the fake JPEG camera), behind the
//!   ABI 4 DMA routes.
//! * [`crc`] — CRC-16 for frames, CRC-32 for payload images.
//! * [`mdns`] — mDNS constants and the DNS-SD message format (re-exported
//!   from [`dns`]); its tokio socket (a [`discovery::Browse`]) and
//!   announcer (a [`discovery::Announce`]) need `mdns`.
//! * Backends, each behind a cargo feature:
//!
//! | feature    | module      | target            | needs                     |
//! |------------|-------------|-------------------|---------------------------|
//! | `ble`      | `ble`       | `no_std`, no heap | a radio binding (traits)  |
//! | `serial`   | `serial::link` | `no_std`, no heap | `embedded-io-async`    |
//! | `espnow`   | `espnow`    | `no_std`, no heap | a radio binding (trait)   |
//! | `trust`    | `loader::sha256`, `loader::trust` checks | `no_std`, no heap | sha2, ed25519-dalek (verify only) |
//! | `cam-fixtures` | `stream::frames::fixtures` | `no_std`, no heap | 53 KB of baked JPEG frames (`fixtures/cam`) |
//! | `zenoh`    | `zenoh`     | `std`             | tokio, zenoh              |
//! | `grpc`     | `grpc`      | `std`             | tokio, tonic              |
//! | `istio`    | `grpc::istio` | `std`           | `grpc`                    |
//! | `mdns`     | `mdns::socket`, `mdns::announce`, `mdns::origin` | `std` | tokio, socket2 |
//! | `dns-sd`   | `dns::unicast` | `std`          | tokio                     |
//! | `memory`   | `transport::memory` | `std`     | tokio (timers); for tests and examples |
//! | `postcard` | `codec::postcard` | `no_std`    | serde                     |
//! | `prost`    | `codec::prost`    | `no_std` + `alloc` | prost              |
//!
//! With no features the crate is `#![no_std]`, allocation free and builds for
//! `thumbv6m-none-eabi`. Enabling `zenoh`, `grpc`, `mdns`, `dns-sd` or
//! `memory` turns on `std`; the
//! embedded features never do, so firmware never links cloud dependencies and
//! cloud builds never compile the BLE stack unless asked.
#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]
#![allow(async_fn_in_trait)]

pub mod codec;
pub mod dedup;
pub mod discovery;
pub mod crc;
pub mod dns;
pub mod key;
pub mod loader;
pub mod mdns;
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
    Delivery, Flow, Gather, Handler, HandlerError, Limits, Overflow, Publisher, Reply, Request,
    RequestMeta, RequestOptions, Requester, Responder, Sample, SampleMeta, SenderId, Split,
    Subscribe, Subscriber, Target,
};
