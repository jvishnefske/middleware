//! What the transport needs from the platform besides `embedded-hal`'s
//! master [`I2c`](embedded_hal_async::i2c::I2c): the target (slave) side of
//! the I2C peripheral, [`I2cTarget`], and a [`Clock`].
//!
//! `embedded-hal` 1.0 has no I2C target trait, so this module defines the
//! smallest one the transport can work with. A HAL adapter implements it
//! over its driver (embassy-rp's `i2c_slave::I2cSlave`, esp-hal's I2C
//! target mode, a PAC-level driver); none is provided here.
//!
//! # The `I2cTarget` contract
//!
//! An adapter must:
//!
//! 1. **Answer two addresses**: the node's own 7-bit address and the bus's
//!    broadcast address ([`Config::broadcast`](super::Config)). For the
//!    default broadcast address, the general call `0x00`, that means
//!    enabling general-call response in the peripheral (RP2040:
//!    `IC_ACK_GENERAL_CALL`; most controllers have such a bit and leave it
//!    off by default). For any other broadcast address the peripheral must
//!    match a second address, which not every controller can.
//! 2. **Acknowledge every byte** of a write addressed to it, including the
//!    PEC byte: this layer checks the PEC and the framing itself. It must
//!    not interpret the SMBus command byte.
//! 3. **Report each completed write transaction** (ended by STOP or a
//!    repeated START) once, in the order they completed, as a
//!    [`Transaction`]: which of its addresses matched and every byte the
//!    master wrote after the address byte (`[command, count, data.., PEC]`
//!    for this protocol), copied into the caller's buffer. A transaction
//!    longer than the buffer is reported with its full `len` and only the
//!    bytes that fit; the transport drops it. Reads addressed to the node
//!    are not this protocol's and may be answered with `0xFF` or NACKed.
//! 4. **Service the hardware independently of this layer's calls**, from
//!    an interrupt or its own task, buffering completed transactions in a
//!    bounded queue in arrival order. Two nodes that each wait to write to
//!    the other would otherwise both stretch the clock until the SMBus
//!    clock-low timeout (25..=35 ms) resets their interfaces. When its
//!    queue is full an adapter may NACK (the sender retries, then reports
//!    `NoAcknowledge`) or drop the newest transaction, never an older one.
//! 5. **Be cancel safe**: dropping a [`I2cTarget::listen`] future before it
//!    completes loses no transaction; the next call returns it.
//!
//! A typical embassy adapter is a task that loops on the driver's
//! `listen`/`respond_to_write` and pushes `(address, bytes)` into an
//! `embassy_sync::channel::Channel`; `listen` is the channel's `receive`
//! and `try_listen` its `try_receive`.
//!
//! # The clock
//!
//! [`Clock`] gives the transport the time (reassembly timeouts, request
//! deadlines) and its waits (backoff after a lost arbitration, request
//! timeouts). Both methods take `&self`, so the halves of a split node can
//! share one clock by reference. Over embassy-time: `now_ms` is
//! `Instant::now().as_millis()` and `delay_us` is
//! `Timer::after_micros(us.into()).await`.

/// One completed write transaction received by an [`I2cTarget`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transaction {
    /// The 7-bit address the master wrote to: the node's own address or the
    /// broadcast address.
    pub address: u8,
    /// Bytes the master wrote after the address byte. When larger than the
    /// buffer passed in, only the bytes that fit were copied.
    pub len: usize,
}

/// The target (slave) side of an I2C peripheral (see the module docs for
/// the full contract).
pub trait I2cTarget {
    /// Adapter failure.
    type Error: core::fmt::Debug;

    /// Waits for the next completed write transaction addressed to the
    /// node, copying its bytes into `buf`. Cancel safe.
    async fn listen(&mut self, buf: &mut [u8]) -> Result<Transaction, Self::Error>;

    /// Returns the next completed write transaction if one is already
    /// buffered, without waiting.
    fn try_listen(&mut self, buf: &mut [u8]) -> Result<Option<Transaction>, Self::Error>;
}

/// The node's time source and waits (see the module docs).
pub trait Clock {
    /// Milliseconds since an arbitrary fixed origin, never decreasing.
    fn now_ms(&self) -> u64;

    /// Waits `us` microseconds. Cancel safe.
    async fn delay_us(&self, us: u32);
}

impl<C: Clock> Clock for &C {
    fn now_ms(&self) -> u64 {
        (**self).now_ms()
    }

    async fn delay_us(&self, us: u32) {
        (**self).delay_us(us).await
    }
}
