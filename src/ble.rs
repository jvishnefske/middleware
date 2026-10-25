//! Bluetooth Low Energy transport (`ble` feature, `no_std`, no heap).
//!
//! * [`uuid`] — derives GATT UUIDs and 16-bit advertising ids from keys.
//! * [`frame`] — pure fragmentation of messages into ATT-sized fragments
//!   (`[id][index][total length u16 LE][data]`) and value-typed reassembly
//!   into a fixed buffer; request bodies carry the key, response bodies a
//!   status byte.
//! * [`gatt`] — the role-specific radio traits [`GattServer`] and
//!   [`GattClient`], and the transports layered on them: [`BleServer`]
//!   implements [`Publisher`](crate::Publisher) (notifications on per-key
//!   characteristics) and [`Responder`](crate::Responder) (requests written
//!   to the shared RPC characteristic); [`BleClient`] implements
//!   [`Subscriber`](crate::Subscriber) and [`Requester`](crate::Requester).
//! * [`advert`] — connectionless messaging: an advertising data builder and
//!   zero-copy parser, the [`Advertiser`] and [`Scanner`] radio traits,
//!   [`AdvertisingPublisher`] / [`AdvertisingSubscriber`] carrying
//!   `[short id][sequence][payload]` in manufacturer data, the
//!   [`ShortIdTable`] that resolves short ids back to key text, and
//!   [`discover`](advert::discover) for finding middleware nodes.
//!
//! The radio itself is reached only through these small traits, so the same
//! logic runs over trouble-host or nrf-softdevice on a microcontroller, or a
//! host stack such as BlueZ, and is tested on the host with in-memory fakes.
//! Duplicate suppression and sequence numbers come from [`crate::dedup`].

pub mod advert;
pub mod frame;
pub mod gatt;
pub mod uuid;

pub use advert::{
    AdvertisementBuilder, Advertiser, AdvertisingPublisher, AdvertisingSubscriber, Scanner,
    ShortIdError, ShortIdTable,
};
pub use gatt::{BleClient, BleError, BleServer, GattClient, GattServer, Inbound};
