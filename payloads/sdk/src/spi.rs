//! SPI master on DMA through the `spi_open`, `spi_transfer` and `dma_read`
//! imports.
//!
//! A transfer is asynchronous and event driven:
//!
//! 1. [`Spi::transfer`] hands the transmit bytes to the host, which copies
//!    them into a firmware-owned DMA buffer and returns at once. The bus
//!    clocks `max(tx.len(), rx_len)` bytes (transmit padded with `0x00`),
//!    at most [`MAX_DMA`].
//! 2. Completion arrives later as an
//!    [`Event::DmaDone`] in
//!    [`Payload::on_irq`](crate::Payload::on_irq); pass it to
//!    [`Spi::on_event`].
//! 3. [`Spi::read_into`] then copies the received bytes out of the DMA
//!    buffer. Guest memory is never a DMA target.
//!
//! Starting a transfer while one is in flight fails with
//! [`AbiError::Busy`] (tracked by [`Transfer`]; see its docs for recovering
//! from a lost completion event).
//!
//! [`SpiConfig::builder`] collects the pins, clock and [`Mode`], and
//! [`SpiConfigBuilder::build`] validates them with the ABI's own parser
//! ([`AbiSpiConfig::parse`]), so a [`SpiConfig`] is valid by construction
//! (apart from chip-specific pin checks, which only the firmware can make).
//!
//! # Why there is no `embedded_hal::spi::SpiBus`
//!
//! `SpiBus::transfer` and friends are blocking: they return only once the
//! bytes have been clocked. A payload cannot block, because every call into
//! it runs on a fixed fuel budget and the firmware delivers the completion
//! only after the current call returns. A blocking adapter would have to
//! busy-wait for an event that cannot arrive until it gives up, so this
//! crate exposes the asynchronous start / `DmaDone` / read sequence instead
//! of faking the trait. (`embedded-hal-async` would need an executor inside
//! the payload, which the ABI does not provide.)

use crate::dma::Transfer;
use crate::irq::{DmaChannel, Event};
use crate::{AbiError, check, sys};
pub use middleware::wasm_abi::periph::{
    MAX_DMA, MAX_SPI_KHZ, SPI_BUSES, SpiConfig as AbiSpiConfig, spi_transfer_len,
};

/// SPI mode: clock polarity (CPOL) and phase (CPHA).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// CPOL 0, CPHA 0: idle low, sample on the rising edge.
    #[default]
    Mode0 = 0,
    /// CPOL 0, CPHA 1: idle low, sample on the falling edge.
    Mode1 = 1,
    /// CPOL 1, CPHA 0: idle high, sample on the falling edge.
    Mode2 = 2,
    /// CPOL 1, CPHA 1: idle high, sample on the rising edge.
    Mode3 = 3,
}

/// Clock used when the builder is not given one.
pub const DEFAULT_KHZ: u32 = 1_000;

/// A validated SPI configuration; build one with [`SpiConfig::builder`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpiConfig(AbiSpiConfig);

impl SpiConfig {
    /// Starts a configuration for bus 0 with clock pin `sck`, no data or
    /// chip-select pins, [`DEFAULT_KHZ`] and [`Mode::Mode0`].
    pub const fn builder(sck: u8) -> SpiConfigBuilder {
        SpiConfigBuilder {
            bus: 0,
            sck,
            mosi: None,
            miso: None,
            cs: None,
            freq_khz: DEFAULT_KHZ,
            mode: Mode::Mode0,
        }
    }

    /// The validated ABI values.
    pub const fn abi(&self) -> &AbiSpiConfig {
        &self.0
    }
}

/// Builder for [`SpiConfig`]. Each method consumes and returns the builder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpiConfigBuilder {
    bus: u8,
    sck: u8,
    mosi: Option<u8>,
    miso: Option<u8>,
    cs: Option<u8>,
    freq_khz: u32,
    mode: Mode,
}

/// An optional pin as the ABI encodes it (`-1` = unused).
fn pin_arg(pin: Option<u8>) -> i32 {
    pin.map_or(-1, i32::from)
}

impl SpiConfigBuilder {
    /// Selects the bus (`< SPI_BUSES`; only bus 0 exists today).
    pub const fn bus(self, bus: u8) -> Self {
        Self { bus, ..self }
    }
    /// Controller-out pin.
    pub const fn mosi(self, pin: u8) -> Self {
        Self {
            mosi: Some(pin),
            ..self
        }
    }
    /// Controller-in pin.
    pub const fn miso(self, pin: u8) -> Self {
        Self {
            miso: Some(pin),
            ..self
        }
    }
    /// Hardware chip-select pin (asserted for each transfer).
    pub const fn cs(self, pin: u8) -> Self {
        Self {
            cs: Some(pin),
            ..self
        }
    }
    /// Clock in kHz, `1..=MAX_SPI_KHZ`.
    pub const fn freq_khz(self, khz: u32) -> Self {
        Self {
            freq_khz: khz,
            ..self
        }
    }
    /// SPI mode.
    pub const fn mode(self, mode: Mode) -> Self {
        Self { mode, ..self }
    }

    /// Validates with [`AbiSpiConfig::parse`]: at least one of MOSI / MISO,
    /// distinct pins, clock in range. Fails with
    /// [`AbiError::InvalidArgument`].
    pub fn build(self) -> Result<SpiConfig, AbiError> {
        let freq = i32::try_from(self.freq_khz).map_err(|_| AbiError::InvalidArgument)?;
        AbiSpiConfig::parse(
            i32::from(self.bus),
            i32::from(self.sck),
            pin_arg(self.mosi),
            pin_arg(self.miso),
            pin_arg(self.cs),
            freq,
            self.mode as i32,
        )
        .map(SpiConfig)
    }
}

/// The DMA channel that reports completions for `bus`.
const fn channel(bus: u8) -> Option<DmaChannel> {
    match bus {
        0 => Some(DmaChannel::Spi0),
        _ => None,
    }
}

/// An open SPI bus.
#[derive(Debug, PartialEq, Eq)]
pub struct Spi {
    config: SpiConfig,
    channel: DmaChannel,
    state: Transfer,
}

impl Spi {
    /// Opens the bus with `config`. The firmware refuses reserved or missing
    /// pins with [`AbiError::Unsupported`] / [`AbiError::InvalidArgument`].
    pub fn open(config: SpiConfig) -> Result<Self, AbiError> {
        let c = config.0;
        let channel = channel(c.bus).ok_or(AbiError::InvalidArgument)?;
        check(sys::spi_open(
            i32::from(c.bus),
            i32::from(c.sck),
            pin_arg(c.mosi),
            pin_arg(c.miso),
            pin_arg(c.cs),
            c.freq_khz as i32,
            i32::from(c.mode),
        ))?;
        Ok(Spi {
            config,
            channel,
            state: Transfer::Idle,
        })
    }

    /// The configuration the bus was opened with.
    pub fn config(&self) -> &SpiConfig {
        &self.config
    }

    /// The payload's view of the current transfer.
    pub fn state(&self) -> Transfer {
        self.state
    }

    /// Whether a transfer is in flight.
    pub fn is_busy(&self) -> bool {
        self.state.is_busy()
    }

    /// Starts a full-duplex transfer of `max(tx.len(), rx_len)` bytes.
    ///
    /// `tx` is copied by the host before this returns, so the caller may
    /// reuse it at once. Fails with [`AbiError::Busy`] while a transfer is
    /// in flight and [`AbiError::InvalidArgument`] for a length of 0 or more
    /// than [`MAX_DMA`].
    pub fn transfer(&mut self, tx: &[u8], rx_len: usize) -> Result<(), AbiError> {
        let tx_len = i32::try_from(tx.len()).map_err(|_| AbiError::InvalidArgument)?;
        let rx = i32::try_from(rx_len).map_err(|_| AbiError::InvalidArgument)?;
        spi_transfer_len(tx_len, rx)?;
        let next = self.state.start()?;
        check(sys::spi_transfer(
            i32::from(self.config.0.bus),
            crate::ptr_i32(tx),
            tx_len,
            rx,
        ))?;
        self.state = next;
        Ok(())
    }

    /// Feeds an event to the bus. Returns the transfer result if `event` is
    /// this bus's `DmaDone`, `None` for any other event.
    pub fn on_event(&mut self, event: &Event) -> Option<Result<usize, AbiError>> {
        match *event {
            Event::DmaDone {
                channel, result, ..
            } if channel == self.channel => {
                self.state = self.state.finish(result);
                Some(result)
            }
            _ => None,
        }
    }

    /// Copies the received bytes of the finished transfer into `buf` and
    /// returns how many were copied (at most `buf.len()`).
    ///
    /// Fails with [`AbiError::Busy`] while the transfer is in flight, with
    /// the transfer's own error if it failed, and with
    /// [`AbiError::InvalidArgument`] if nothing was transferred yet.
    pub fn read_into(&mut self, buf: &mut [u8]) -> Result<usize, AbiError> {
        match self.state {
            Transfer::Idle => Err(AbiError::InvalidArgument),
            Transfer::InFlight => Err(AbiError::Busy),
            Transfer::Done(Err(e)) => Err(e),
            Transfer::Done(Ok(n)) => {
                let len = n.min(buf.len());
                let dst = buf.get_mut(..len).ok_or(AbiError::OutOfBounds)?;
                if dst.is_empty() {
                    return Ok(0);
                }
                check(sys::dma_read(self.channel as i32, dst)).map(|c| c as usize)
            }
        }
    }

    /// Forgets an in-flight transfer whose `DmaDone` was lost (see
    /// [`Transfer`]).
    pub fn recover(&mut self) {
        self.state = self.state.recover();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> SpiConfig {
        SpiConfig::builder(12)
            .mosi(11)
            .miso(13)
            .cs(10)
            .freq_khz(2_000)
            .mode(Mode::Mode3)
            .build()
            .expect("valid")
    }

    fn open_bus() -> Spi {
        Spi {
            config: cfg(),
            channel: DmaChannel::Spi0,
            state: Transfer::Idle,
        }
    }

    #[test]
    fn builder_produces_the_abi_values() {
        let c = cfg();
        assert_eq!(
            *c.abi(),
            AbiSpiConfig::parse(0, 12, 11, 13, 10, 2_000, 3).expect("valid")
        );
        let defaults = SpiConfig::builder(6).mosi(7).build().expect("valid");
        assert_eq!(
            (
                defaults.abi().freq_khz,
                defaults.abi().mode,
                defaults.abi().miso
            ),
            (DEFAULT_KHZ, 0, None)
        );
    }

    #[test]
    fn builder_mirrors_the_abi_rules() {
        let bad = Err(AbiError::InvalidArgument);
        assert_eq!(SpiConfig::builder(6).build(), bad, "no data pin");
        assert_eq!(SpiConfig::builder(6).mosi(6).build(), bad, "duplicate");
        assert_eq!(
            SpiConfig::builder(6).mosi(7).cs(7).build(),
            bad,
            "duplicate cs"
        );
        assert_eq!(SpiConfig::builder(6).mosi(7).freq_khz(0).build(), bad);
        assert_eq!(
            SpiConfig::builder(6)
                .mosi(7)
                .freq_khz(MAX_SPI_KHZ as u32 + 1)
                .build(),
            bad
        );
        assert_eq!(
            SpiConfig::builder(6).mosi(7).freq_khz(u32::MAX).build(),
            bad
        );
        assert_eq!(SpiConfig::builder(6).mosi(7).bus(1).build(), bad, "bus");
    }

    #[test]
    fn open_reaches_the_host_stub() {
        assert_eq!(Spi::open(cfg()), Err(AbiError::Unsupported));
    }

    #[test]
    fn transfer_checks_lengths_and_busy_before_the_host() {
        let mut spi = open_bus();
        assert_eq!(spi.transfer(&[], 0), Err(AbiError::InvalidArgument));
        assert_eq!(
            spi.transfer(&[0; MAX_DMA + 1], 0),
            Err(AbiError::InvalidArgument)
        );
        // A valid request reaches the (unsupported) host and leaves the state.
        assert_eq!(spi.transfer(&[1, 2], 4), Err(AbiError::Unsupported));
        assert_eq!(spi.state(), Transfer::Idle);
        spi.state = Transfer::InFlight;
        assert_eq!(spi.transfer(&[1], 1), Err(AbiError::Busy));
    }

    #[test]
    fn dma_done_completes_and_read_follows_the_state() {
        let mut spi = open_bus();
        let mut buf = [0u8; 4];
        assert_eq!(spi.read_into(&mut buf), Err(AbiError::InvalidArgument));
        spi.state = Transfer::InFlight;
        assert_eq!(spi.read_into(&mut buf), Err(AbiError::Busy));
        let other = Event::DmaDone {
            channel: DmaChannel::Uart1Tx,
            result: Ok(3),
            time_us: 0,
        };
        assert_eq!(spi.on_event(&other), None);
        assert!(spi.is_busy());
        let done = Event::DmaDone {
            channel: DmaChannel::Spi0,
            result: Err(AbiError::Transport),
            time_us: 0,
        };
        assert_eq!(spi.on_event(&done), Some(Err(AbiError::Transport)));
        assert_eq!(spi.read_into(&mut buf), Err(AbiError::Transport));
        spi.state = Transfer::Done(Ok(8));
        // Reaches the host stub with the buffer clamped to its length.
        assert_eq!(spi.read_into(&mut buf), Err(AbiError::Unsupported));
        assert_eq!(spi.read_into(&mut []), Ok(0));
    }

    #[test]
    fn recover_clears_a_lost_transfer() {
        let mut spi = open_bus();
        spi.state = Transfer::InFlight;
        spi.recover();
        assert!(!spi.is_busy());
    }
}
