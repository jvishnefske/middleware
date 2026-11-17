//! SMBus multi-master transport (`smbus` feature, `no_std`, no heap).
//!
//! Runs the middleware over an I2C bus shared by several microcontrollers,
//! using SMBus conventions: every node is a **master** when it sends and a
//! **target** (slave) at its own 7-bit address when it receives. Messages
//! larger than one SMBus block stream across as many blocks as they need.
//!
//! * Unicast — requests, replies, errors and [`Target::Node`] — is an SMBus
//!   Block Write to the destination's address.
//! * Publish/subscribe is a Block Write to a broadcast address every
//!   node's target answers: the I2C general call address `0x00` by default
//!   ([`Config::with_broadcast`] picks another).
//! * Every block carries the SMBus Packet Error Code (CRC-8, polynomial
//!   `0x07`, [`crate::crc::crc8`]) over the whole transaction; a block with
//!   a bad PEC is dropped and counted, never delivered.
//!
//! The pure core:
//!
//! * [`address`] — validated addresses (the I2C and SMBus reserved ranges
//!   are refused) and their [`SenderId`](crate::transport::SenderId)s.
//! * [`wire`] — the block layout (fragment header, PEC) and message bodies.
//! * [`reassembly`] — messages from several masters reassembled at once in
//!   fixed slots, with a timeout on the caller's clock.
//! * [`retry`] — the bounded retry policy for arbitration loss and NACKs,
//!   with deterministic address-derived backoff.
//!
//! The shell:
//!
//! * [`target`] — the [`I2cTarget`] trait a HAL adapter implements for the
//!   target side (embedded-hal 1.0 has none) and the [`Clock`].
//! * [`node`] — [`SmbusNode`], implementing [`Publisher`](crate::Publisher),
//!   [`Subscriber`](crate::Subscriber), [`Subscribe`](crate::Subscribe),
//!   [`Requester`](crate::Requester), [`Responder`](crate::Responder) and
//!   [`Split`](crate::Split) over an `embedded-hal-async` master
//!   [`I2c`](embedded_hal_async::i2c::I2c), an [`I2cTarget`] and a
//!   [`Clock`].
//!
//! # SMBus and I2C caveats
//!
//! * The general call address only reaches targets that enable
//!   general-call response, and the master's HAL must accept address `0x00`
//!   for a write (some refuse reserved addresses); otherwise configure a
//!   broadcast address the adapters also match.
//! * SMBus 2.0 limits a block to 32 data bytes; the block size `B` is a
//!   const generic so SMBus 3.x buses can use up to 255. All nodes on a bus
//!   use the same `B` and the same message bound `M`.
//! * The SMBus timeouts (clock low 25..=35 ms), bus free time and
//!   arbitration are the peripheral's; this layer retries what they report
//!   ([`retry`]).
//! * Sender addresses are claimed in each block, not authenticated: any
//!   master can write any address byte.
//!
//! [`Target::Node`]: crate::transport::Target::Node

pub mod address;
pub mod node;
pub mod reassembly;
pub mod retry;
pub mod target;
pub mod wire;

pub use address::{Address, AddressError};
pub use node::{
    Config, ConfigError, DELIVERY, Filtered, RxStats, SmbusError, SmbusNode, SmbusRx, SmbusTx,
    Stats, TxStats,
};
pub use retry::RetryPolicy;
pub use target::{Clock, I2cTarget, Transaction};
