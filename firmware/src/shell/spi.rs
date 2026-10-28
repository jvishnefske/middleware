//! SPI bus 0 for payloads: SPI2 as a DMA master (D6).
//!
//! | chip     | peripheral | DMA channel                  | signals                      |
//! |----------|------------|------------------------------|------------------------------|
//! | ESP32    | SPI2 (HSPI)| `DMA_SPI2` (SPI DMA engine)  | HSPICLK, HSPID, HSPIQ, HSPICS0 |
//! | ESP32-S3 | SPI2 (FSPI)| `DMA_CH0` (GDMA)             | FSPICLK, FSPID, FSPIQ, FSPICS0 |
//!
//! (ESP32-C3 builds without the `wasm` runtime, so this module is not
//! compiled there.)
//!
//! The driver is created once at boot without pins
//! (`Spi::new(..).with_dma(..)`) and rests as [`SpiParts`] (blocking mode,
//! which is `Send`, so it can sit in the static hardware home) between
//! payload runs; a payload's device turns it into an async [`SpiBus`] and
//! back ([`SpiBus::close`]). `spi_open` applies clock and mode
//! (`SpiDma::apply_config`); the device routes the pins through the GPIO
//! matrix (see `gpio::PinBank::route_out`).
//!
//! Two static DMA buffers of [`MAX_DMA`] bytes in internal RAM
//! (`InternalMemory`, one descriptor each) are allocated once. A transfer
//! copies the guest's bytes into the transmit buffer, pads with `0x00`, and
//! starts `SpiDma::transfer_buffers`, which moves the driver and buffers
//! into an `SpiDmaTransfer`; `spi_transfer` returns at once. The
//! controller awaits [`SpiBus::completion`] (woken by esp-hal's DMA
//! interrupt), which takes the driver back and pushes `DmaDone(Spi0, len)`.
//! `dma_read` copies out of the receive buffer, consuming it. The pure
//! [`DmaState`] machine decides `Busy` and what is readable.
//!
//! Where the driver and buffers are (idle, or inside the running transfer)
//! is the pure [`DmaHw`] state held in an [`HwSlot`]: every start, reclaim
//! and close is a by-value transition that returns the parts, so a payload
//! stop in any state (idle, payload transfer or route write in flight)
//! cancels what runs and hands SPI2 and both static buffers back to the
//! hardware home for the next payload. (Before this, closing an idle bus
//! dropped them, and every later `spi_open` in that boot was
//! `Unsupported`.)
//!
//! A sink route (ABI 4) sends through [`SpiBus::write_route`] instead: the
//! same buffers and driver, awaited in place by the controller, without
//! touching the payload's [`DmaState`] (no `DmaDone`, nothing to read).

use core::future::pending;

use esp_base::periph::dma::DmaState;
use esp_base::periph::dma_hw::{self, DmaHw, HwSlot};
use esp_base::periph::dma_done_event;
use esp_hal::{Async, Blocking};
use esp_hal::dma::aligned::InternalMemory;
use esp_hal::dma::{DmaDescriptor, DmaRxBuf, DmaTxBuf};
use esp_hal::gpio::{InputSignal, OutputSignal};
use esp_hal::spi::Mode;
use esp_hal::spi::master::{Config, SpiDma, SpiDmaTransfer};
use esp_hal::time::Rate;
use middleware::wasm_abi::AbiError;
use middleware::wasm_abi::events::DmaChannel;
use middleware::wasm_abi::periph::{MAX_DMA, SpiConfig};
use embassy_time::{Duration, with_timeout};
use static_cell::ConstStaticCell;

use super::irq;

/// SPI2 clock output.
#[cfg(feature = "esp32")]
pub const SCK: OutputSignal = OutputSignal::HSPICLK;
/// SPI2 controller-out data.
#[cfg(feature = "esp32")]
pub const MOSI: OutputSignal = OutputSignal::HSPID;
/// SPI2 controller-in data.
#[cfg(feature = "esp32")]
pub const MISO: InputSignal = InputSignal::HSPIQ;
/// SPI2 hardware chip select 0.
#[cfg(feature = "esp32")]
pub const CS: OutputSignal = OutputSignal::HSPICS0;

/// SPI2 clock output.
#[cfg(not(feature = "esp32"))]
pub const SCK: OutputSignal = OutputSignal::FSPICLK;
/// SPI2 controller-out data.
#[cfg(not(feature = "esp32"))]
pub const MOSI: OutputSignal = OutputSignal::FSPID;
/// SPI2 controller-in data.
#[cfg(not(feature = "esp32"))]
pub const MISO: InputSignal = InputSignal::FSPIQ;
/// SPI2 hardware chip select 0.
#[cfg(not(feature = "esp32"))]
pub const CS: OutputSignal = OutputSignal::FSPICS0;

const DESCRIPTORS: usize = MAX_DMA.div_ceil(esp_hal::dma::CHUNK_SIZE);

static RX_BUF: ConstStaticCell<InternalMemory<[u8; MAX_DMA]>> =
    ConstStaticCell::new(InternalMemory::new([0; MAX_DMA]));
static TX_BUF: ConstStaticCell<InternalMemory<[u8; MAX_DMA]>> =
    ConstStaticCell::new(InternalMemory::new([0; MAX_DMA]));
static RX_DESC: ConstStaticCell<InternalMemory<[DmaDescriptor; DESCRIPTORS]>> =
    ConstStaticCell::new(InternalMemory::new([DmaDescriptor::EMPTY; DESCRIPTORS]));
static TX_DESC: ConstStaticCell<InternalMemory<[DmaDescriptor; DESCRIPTORS]>> =
    ConstStaticCell::new(InternalMemory::new([DmaDescriptor::EMPTY; DESCRIPTORS]));

/// Longest one sink-route transfer may take (a chunk at the slowest clock
/// takes a few milliseconds).
const ROUTE_TIMEOUT: Duration = Duration::from_millis(50);

/// esp-hal's running transfer, which owns the driver and both buffers.
struct Transfer(SpiDmaTransfer<'static, Async, (DmaRxBuf, DmaTxBuf)>);

struct Parts {
    spi: SpiDma<'static, Async>,
    rx: DmaRxBuf,
    tx: DmaTxBuf,
}

impl dma_hw::Transfer for Transfer {
    type Parts = Parts;

    fn cancel(&mut self) {
        self.0.cancel();
    }

    fn finish(self) -> Parts {
        let (spi, (rx, tx)) = self.0.wait();
        Parts { spi, rx, tx }
    }
}

impl Parts {
    /// Loads `n` bytes into the transmit buffer with `fill` and starts a
    /// full-duplex transfer of `n` bytes; on failure the parts come back.
    fn start(mut self, n: usize, fill: impl FnOnce(&mut [u8])) -> Result<Transfer, (AbiError, Parts)> {
        let n = n.min(self.tx.capacity()).min(self.rx.capacity());
        if let Some(room) = self.tx.as_mut_slice().get_mut(..n) {
            fill(room);
        }
        self.tx.set_length(n);
        self.rx.set_length(n);
        match self.spi.transfer_buffers(n, self.rx, n, self.tx) {
            Ok(t) => Ok(Transfer(t)),
            Err((_, spi, rx, tx)) => Err((AbiError::Transport, Parts { spi, rx, tx })),
        }
    }
}

/// The idle bus between payload runs: blocking driver and DMA buffers.
pub struct SpiParts {
    spi: SpiDma<'static, Blocking>,
    rx: DmaRxBuf,
    tx: DmaTxBuf,
}

impl SpiParts {
    /// Wraps the boot-time driver and takes the static DMA buffers. Call
    /// once; `None` if the buffers were already taken or are misplaced.
    pub fn new(spi: SpiDma<'static, Blocking>) -> Option<Self> {
        let rx = DmaRxBuf::new(RX_DESC.try_take()?.get_mut().unsize(), RX_BUF.try_take()?.get_mut().unsize()).ok()?;
        let tx = DmaTxBuf::new(TX_DESC.try_take()?.get_mut().unsize(), TX_BUF.try_take()?.get_mut().unsize()).ok()?;
        Some(SpiParts { spi, rx, tx })
    }
}

/// SPI bus 0 and its DMA channel state, for one payload run.
pub struct SpiBus {
    hw: HwSlot<Transfer>,
    dma: DmaState,
    open: bool,
}

impl SpiBus {
    /// The bus for a payload run (closed until `spi_open`).
    pub fn new(parts: SpiParts) -> Self {
        let SpiParts { spi, rx, tx } = parts;
        SpiBus { hw: HwSlot::new(Parts { spi: spi.into_async(), rx, tx }), dma: DmaState::Idle, open: false }
    }

    /// Whether a transfer is running.
    pub fn busy(&self) -> bool {
        self.dma.in_flight()
    }

    /// Whether `spi_open` configured the bus.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Hands the driver and buffers back after a finished (or cancelled)
    /// transfer; nothing happens when the bus is idle.
    fn reclaim(&mut self) {
        self.hw.step(|hw| (hw.reclaim(), ()));
    }

    /// Sends `data` for a sink route and waits for the DMA to finish
    /// (bounded by [`ROUTE_TIMEOUT`]; a transfer that overruns it is
    /// cancelled and reported as `Transport`). The payload's [`DmaState`]
    /// is not touched, so no `DmaDone` event is raised and `dma_read` is
    /// unaffected. `Busy` if the bus is closed or a transfer runs;
    /// `InvalidArgument` if `data` exceeds the DMA buffer.
    ///
    /// Cancellation safe: if this future is dropped mid-transfer, the
    /// transfer stays in the bus and [`SpiBus::close`] (or the next
    /// completion) takes the parts back.
    pub async fn write_route(&mut self, data: &[u8]) -> Result<(), AbiError> {
        if !self.open || self.dma.in_flight() {
            return Err(AbiError::Busy);
        }
        if data.len() > MAX_DMA {
            return Err(AbiError::InvalidArgument);
        }
        let started = self
            .hw
            .step(|hw| hw.start(|p| p.start(data.len(), |room| room.iter_mut().zip(data).for_each(|(d, s)| *d = *s))))
            .unwrap_or(Err(AbiError::Busy));
        started?;
        let mut result = Ok(());
        if let Some(t) = self.hw.get_mut().and_then(DmaHw::transfer_mut)
            && with_timeout(ROUTE_TIMEOUT, t.0.wait_for_done()).await.is_err()
        {
            t.0.cancel();
            result = Err(AbiError::Transport);
        }
        self.reclaim();
        result
    }

    /// Applies clock and mode (pins are routed by the caller).
    pub fn open(&mut self, cfg: &SpiConfig) -> Result<(), AbiError> {
        let parts = self.hw.get_mut().and_then(DmaHw::parts_mut).ok_or(AbiError::Busy)?;
        let mode = match cfg.mode {
            0 => Mode::_0,
            1 => Mode::_1,
            2 => Mode::_2,
            _ => Mode::_3,
        };
        let config = Config::default().with_frequency(Rate::from_khz(cfg.freq_khz)).with_mode(mode);
        parts.spi.apply_config(&config).map_err(|_| AbiError::InvalidArgument)?;
        self.open = true;
        Ok(())
    }

    /// Starts a full-duplex transfer of `len` bytes; `tx` is padded with
    /// `0x00`.
    pub fn transfer(&mut self, tx: &[u8], len: usize) -> Result<(), AbiError> {
        if !self.open {
            return Err(AbiError::Busy);
        }
        let started = self.dma.start(len)?;
        let fill = |room: &mut [u8]| {
            for (i, b) in room.iter_mut().enumerate() {
                *b = tx.get(i).copied().unwrap_or(0);
            }
        };
        self.hw.step(|hw| hw.start(|p| p.start(len, fill))).unwrap_or(Err(AbiError::Busy))?;
        self.dma = started;
        Ok(())
    }

    /// Waits for the running transfer and pushes its `DmaDone` event;
    /// pending forever when nothing runs. Cancellation safe: the transfer
    /// keeps running and the next call waits again.
    pub async fn completion(&mut self) {
        match self.hw.get_mut().and_then(DmaHw::transfer_mut) {
            Some(t) => t.0.wait_for_done().await,
            None => pending::<()>().await,
        }
        self.reclaim();
        let (dma, value) = self.dma.complete(Ok(()));
        self.dma = dma;
        if let Some(value) = value {
            irq::push(dma_done_event(DmaChannel::Spi0, value, irq::now_us() as u32));
        }
    }

    /// Copies the received bytes into `out` (consuming them).
    pub fn read(&mut self, out: &mut [u8]) -> Result<usize, AbiError> {
        let parts = self.hw.get().and_then(DmaHw::parts).ok_or(AbiError::Busy)?;
        let (dma, len) = self.dma.take()?;
        self.dma = dma;
        let src = parts.rx.as_slice();
        let n = len.min(out.len()).min(src.len());
        if let (Some(dst), Some(src)) = (out.get_mut(..n), src.get(..n)) {
            dst.copy_from_slice(src);
        }
        Ok(n)
    }

    /// Payload stop, in any state (idle, payload transfer or route write
    /// in flight): cancels a running transfer, closes the bus and returns
    /// the idle parts ([`DmaHw::close`]). `None` is unreachable: the slot
    /// is only empty inside a step.
    pub fn close(self) -> Option<SpiParts> {
        let parts = self.hw.close();
        MISO.connect_to(&esp_hal::gpio::Level::Low);
        parts.map(|Parts { spi, rx, tx }| SpiParts { spi: spi.into_blocking(), rx, tx })
    }
}
