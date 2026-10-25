//! Secondary UART 1 for payloads (D7).
//!
//! # What "DMA" means here, per chip
//!
//! | chip     | transmit                         | receive                        |
//! |----------|----------------------------------|--------------------------------|
//! | ESP32    | async FIFO driver (interrupt-driven) | async FIFO driver into the ring |
//! | ESP32-S3 | async FIFO driver (interrupt-driven) | async FIFO driver into the ring |
//! | ESP32-C3 | not built (no `wasm` runtime)    | not built                      |
//!
//! esp-hal 1.2 has no UART DMA for the ESP32 (no UHCI driver). It has a
//! UHCI driver for the ESP32-S3 (and C3), but `Uhci::new` takes the whole
//! UART and receives in DMA chunks that end on a length or idle condition,
//! with the receive path re-armed per transfer; that does not fit a
//! continuously filled ring without gaps, and mixing UHCI transmit with FIFO
//! receive on one UART is not something esp-hal documents. So both chips use
//! esp-hal's interrupt-driven async FIFO driver: "DMA" for UART 1 means a
//! DMA-equivalent asynchronous transfer, not a DMA engine. The payload's
//! `uart_write` copies into a firmware buffer and returns at once; the
//! controller task (woken by the UART FIFO-empty / TX-done interrupts) feeds
//! the 128-byte hardware FIFO from that buffer and raises
//! `DmaDone(Uart1Tx, len)` after the last bit is on the wire. The CPU copies
//! the bytes into the FIFO in thread context, never in an ISR and never
//! while the payload runs.
//!
//! Received bytes are moved from the FIFO (woken by FIFO-full / RX-timeout
//! interrupts) into the fixed [`UART_RX_BUFFER`] ring
//! ([`RxRing`]: excess dropped and counted), and each batch pushes a
//! coalescing `UartRx(1, buffered)` event. `uart_read` drains the ring.
//!
//! The driver is created at boot without pins and rests as [`Uart1Parts`]
//! (blocking mode, `Send`) in the hardware home between payload runs; `uart_open` sets the baud rate and the device
//! routes U1TXD / U1RXD through the GPIO matrix. Pin policy (never the
//! UART0 console, flash or USB pins) is the pure claim table.

use core::future::pending;

use embassy_futures::select::{Either, select};
use esp_base::periph::dma::DmaState;
use esp_base::periph::ring::RxRing;
use esp_base::periph::{dma_done_event, uart_rx_event};
use esp_hal::{Async, Blocking};
use esp_hal::gpio::{InputSignal, Level, OutputSignal};
use esp_hal::uart::{Config, Uart};
use middleware::wasm_abi::AbiError;
use middleware::wasm_abi::events::DmaChannel;
use middleware::wasm_abi::periph::{MAX_DMA, UART_PORT, UART_RX_BUFFER, UartConfig};
use static_cell::ConstStaticCell;

use super::irq;

/// UART1 transmit signal.
pub const TXD: OutputSignal = OutputSignal::U1TXD;
/// UART1 receive signal.
pub const RXD: InputSignal = InputSignal::U1RXD;

/// Bytes moved from the FIFO per receive step.
const RX_CHUNK: usize = 64;

static TX_BUF: ConstStaticCell<[u8; MAX_DMA]> = ConstStaticCell::new([0; MAX_DMA]);
static RX_RING: ConstStaticCell<RxRing<UART_RX_BUFFER>> = ConstStaticCell::new(RxRing::new());

/// The idle port between payload runs: blocking driver and buffers.
pub struct Uart1Parts {
    uart: Uart<'static, Blocking>,
    tx_buf: &'static mut [u8; MAX_DMA],
    rx: &'static mut RxRing<UART_RX_BUFFER>,
}

impl Uart1Parts {
    /// Wraps the boot-time driver and takes the static buffers. Call once.
    pub fn new(uart: Uart<'static, Blocking>) -> Option<Self> {
        Some(Uart1Parts { uart, tx_buf: TX_BUF.try_take()?, rx: RX_RING.try_take()? })
    }
}

/// UART 1, its transmit buffer and receive ring, for one payload run.
pub struct Uart1Port {
    uart: Uart<'static, Async>,
    tx_buf: &'static mut [u8; MAX_DMA],
    tx_len: usize,
    tx_sent: usize,
    dma: DmaState,
    rx: &'static mut RxRing<UART_RX_BUFFER>,
    open: Option<UartConfig>,
}

impl Uart1Port {
    /// The port for a payload run (closed until `uart_open`).
    pub fn new(parts: Uart1Parts) -> Self {
        let Uart1Parts { uart, tx_buf, rx } = parts;
        rx.clear();
        Uart1Port { uart: uart.into_async(), tx_buf, tx_len: 0, tx_sent: 0, dma: DmaState::Idle, rx, open: None }
    }

    /// Whether a transmit is running.
    pub fn busy(&self) -> bool {
        self.dma.in_flight()
    }

    /// Sets the baud rate and empties the receive ring (pins are routed by
    /// the caller). Without an RX pin the receiver idles high.
    pub fn open(&mut self, cfg: &UartConfig) -> Result<(), AbiError> {
        if self.busy() {
            return Err(AbiError::Busy);
        }
        self.uart
            .apply_config(&Config::default().with_baudrate(cfg.baud))
            .map_err(|_| AbiError::InvalidArgument)?;
        if cfg.rx.is_none() {
            RXD.connect_to(&Level::High);
        }
        self.rx.clear();
        self.open = Some(*cfg);
        Ok(())
    }

    fn port(&self, port: u8) -> Result<UartConfig, AbiError> {
        self.open.filter(|c| c.port == port && port as i32 == UART_PORT).ok_or(AbiError::Busy)
    }

    /// Starts transmitting `data`.
    pub fn write(&mut self, port: u8, data: &[u8]) -> Result<(), AbiError> {
        let cfg = self.port(port)?;
        if cfg.tx.is_none() {
            return Err(AbiError::Unsupported);
        }
        let n = data.len().min(MAX_DMA);
        self.dma = self.dma.start(n)?;
        if let (Some(dst), Some(src)) = (self.tx_buf.get_mut(..n), data.get(..n)) {
            dst.copy_from_slice(src);
        }
        self.tx_len = n;
        self.tx_sent = 0;
        Ok(())
    }

    /// Moves buffered received bytes into `out`.
    pub fn read(&mut self, port: u8, out: &mut [u8]) -> Result<usize, AbiError> {
        self.port(port)?;
        Ok(self.rx.pop_into(out))
    }

    /// One step of transmit or receive work, whichever is ready first;
    /// pending forever when the port is idle. Cancellation safe (esp-hal's
    /// `write_async`, `flush_async` and `read_async` are).
    pub async fn io(&mut self) {
        let receiving = self.open.is_some_and(|c| c.rx.is_some());
        let sending = self.dma.in_flight();
        let Uart1Port { uart, tx_buf, tx_len, tx_sent, dma, rx, .. } = self;
        let (uart_rx, uart_tx) = uart.split_mut();
        let tx_step = async {
            if !sending {
                return pending::<Result<bool, ()>>().await;
            }
            match tx_buf.get(*tx_sent..*tx_len) {
                Some(rest) if !rest.is_empty() => {
                    let n = uart_tx.write_async(rest).await.map_err(|_| ())?;
                    *tx_sent += n;
                    Ok(false)
                }
                _ => uart_tx.flush_async().await.map(|()| true).map_err(|_| ()),
            }
        };
        let rx_step = async {
            if !receiving {
                return pending::<Result<([u8; RX_CHUNK], usize), ()>>().await;
            }
            let mut chunk = [0u8; RX_CHUNK];
            let n = uart_rx.read_async(&mut chunk).await.map_err(|_| ())?;
            Ok((chunk, n))
        };
        let now = || irq::now_us() as u32;
        match select(tx_step, rx_step).await {
            Either::First(Ok(false)) => {}
            Either::First(done) => {
                let result = done.map(|_| ()).map_err(|()| AbiError::Transport);
                let (next, value) = dma.complete(result);
                *dma = next;
                if let Some(value) = value {
                    irq::push(dma_done_event(DmaChannel::Uart1Tx, value, now()));
                }
            }
            Either::Second(Ok((chunk, n))) => {
                rx.push(chunk.get(..n).unwrap_or(&[]));
                if n > 0 {
                    irq::push(uart_rx_event(UART_PORT as u8, rx.len(), now()));
                }
            }
            // Framing/parity/overflow errors are reported and cleared by the
            // driver; the bytes are lost.
            Either::Second(Err(())) => {}
        }
    }

    /// Payload stop: abandons a running transmit, closes the port and
    /// returns the idle parts.
    pub fn close(self) -> Uart1Parts {
        RXD.connect_to(&Level::High);
        self.rx.clear();
        Uart1Parts { uart: self.uart.into_blocking(), tx_buf: self.tx_buf, rx: self.rx }
    }
}
