//! Bluetooth Low Energy transport (`ble` feature, `no_std`, no heap).
//!
//! * [`uuid`] — derives GATT UUIDs and 16-bit advertising ids from keys.
//! * [`frame`] — pure fragmentation of messages into ATT-sized fragments
//!   (`[id][index][total length u16 LE][data]`) and value-typed reassembly
//!   into a fixed buffer; request bodies carry the key, response bodies a
//!   status byte.
//! * [`gatt`] — the [`GattLink`] radio trait and [`BleTransport`], which
//!   implements [`Publisher`](crate::Publisher) and
//!   [`Subscriber`](crate::Subscriber) as notifications on per-key
//!   characteristics, and [`Requester`](crate::Requester) and
//!   [`Responder`](crate::Responder) as correlated request/response frames on
//!   the shared RPC characteristic.
//! * [`advert`] — connectionless messaging: an advertising data builder and
//!   zero-copy parser, the [`Advertiser`] and [`Scanner`] radio traits,
//!   [`AdvertisingPublisher`] / [`AdvertisingSubscriber`] carrying
//!   `[short id][sequence][payload]` in manufacturer data, and
//!   [`discover`](advert::discover) for finding middleware nodes.
//!
//! The radio itself is reached only through these small traits, so the same
//! logic runs over trouble-host or nrf-softdevice on a microcontroller, or a
//! host stack such as BlueZ, and is tested on the host with in-memory fakes.

pub mod advert;
pub mod frame;
pub mod gatt;
pub mod uuid;

pub use advert::{
    AdvertisementBuilder, Advertiser, AdvertisingPublisher, AdvertisingSubscriber, Scanner,
};
pub use gatt::{BleError, BleTransport, GattLink};
