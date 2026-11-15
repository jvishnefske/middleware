//! LPUART1 (the OpenSDA VCOM) as the protocol link's byte stream, without
//! interrupts.
//!
//! An interrupt handler would need an `unsafe fn` in this crate (the HAL's
//! `Handler::on_interrupt`), which `#![forbid(unsafe_code)]` refuses, so:
//!
//! * **receive**: eDMA channel 0 ([`super::edma`]), requested by LPUART1's
//!   receiver through DMAMUX (source [`DMAMUX_LPUART1_RX`]), copies every
//!   received byte into [`RX_RING`], a ring of [`RX_RING_LEN`] atomics in
//!   DTCM (the TCM is not cached, so no cache maintenance; a write by the
//!   DMA engine is like a write by another thread). The ring never stops
//!   ([`super::edma::Ring`]), so no byte is lost while the
//!   firmware is busy (a 256 KiB erase blocks for a second; the ring holds
//!   about 350 ms of input at 115200 baud, and the host sends nothing while
//!   its request is in flight). [`UartRx`] reads the DMA's write position
//!   (`DADDR`) and polls every [`RX_POLL`] while it is empty.
//! * **transmit**: polled; while the 4-word FIFO is full the writer waits
//!   [`TX_POLL`] (a byte takes 87 µs), so other tasks run. `flush` waits
//!   (bounded) for the last stop bit (STAT.TC).
//!
//! The UART clock is the 24 MHz oscillator (`Console::new`'s setup, which
//! this reuses: 115385 baud, 0.16 % off).

use core::sync::atomic::{AtomicU8, Ordering::Relaxed};

use embassy_nxp::pac;
use embassy_time::{Duration, Instant, Timer};
use embedded_io_async::{ErrorKind, ErrorType, Read, Write};
use imxrt_base::shell::console::Console;

use super::edma;

/// Bytes of the receive ring (a power of two, below eDMA's 15-bit loop
/// count).
pub const RX_RING_LEN: usize = 4096;

/// The receive ring the eDMA writes, in DTCM (`.bss`).
static RX_RING: [AtomicU8; RX_RING_LEN] = [const { AtomicU8::new(0) }; RX_RING_LEN];

/// The eDMA channel of the receiver.
pub const RX_CHANNEL: edma::Channel = edma::Channel::Ch0;

/// DMAMUX request source of LPUART1's receiver (RT1050 RM, DMAMUX table:
/// LPUART1 TX 2, RX 3).
pub const DMAMUX_LPUART1_RX: u8 = 3;

/// How often an empty receiver is polled.
pub const RX_POLL: Duration = Duration::from_millis(1);

/// How long the writer waits for room in the transmit FIFO.
pub const TX_POLL: Duration = Duration::from_micros(50);

/// Longest wait for the transmitter to drain (`flush`): a 4-byte FIFO at
/// 115200 baud empties in under 0.5 ms.
const FLUSH_TIMEOUT: Duration = Duration::from_millis(20);

/// The transmit FIFO's watermark: TDRE while at most this many words wait,
/// so a set TDRE means room for at least one byte (FIFO depth 4).
const TX_WATER: u8 = 3;

/// LPUART1 never fails a read or write (no flow control, no framing errors
/// reported up); the error type exists for `embedded-io`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Never;

impl core::fmt::Display for Never {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("LPUART1")
    }
}

impl core::error::Error for Never {}

impl embedded_io_async::Error for Never {
    fn kind(&self) -> ErrorKind {
        ErrorKind::Other
    }
}

/// The receiving half: the reader of [`RX_RING`].
pub struct UartRx {
    /// Ring index of the next byte to read.
    tail: usize,
    ring: edma::Ring,
}

/// The transmitting half.
pub struct UartTx {
    _uart: Console,
}

/// Brings up LPUART1 at 115200 8N1 with both FIFOs and the receive DMA,
/// and returns its two halves.
pub fn split(uart: embassy_nxp::Peri<'static, embassy_nxp::peripherals::LPUART1>) -> (UartRx, UartTx) {
    let console = Console::new(uart);
    let u = pac::LPUART1;
    // FIFOs on (flushed), receive watermark 0 (a request per byte).
    u.ctrl().modify(|w| {
        w.set_te(false);
        w.set_re(false);
    });
    u.fifo().modify(|w| {
        w.set_rxfe(true);
        w.set_txfe(true);
        w.set_rxflush(true);
        w.set_txflush(pac::lpuart::vals::Txflush::TxfifoRst);
    });
    u.water().write(|w| {
        w.set_rxwater(0);
        w.set_txwater(TX_WATER);
    });

    let ring = edma::Ring {
        channel: RX_CHANNEL,
        source: DMAMUX_LPUART1_RX,
        src: u.data().as_ptr() as u32,
        base: RX_RING.as_ptr() as u32,
        len: RX_RING_LEN as u16,
        width: edma::Width::U8,
    };
    edma::start_ring(&ring);

    u.baud().modify(|w| w.set_rdmae(true));
    u.ctrl().modify(|w| {
        w.set_te(true);
        w.set_re(true);
    });
    (UartRx { tail: 0, ring }, UartTx { _uart: console })
}

impl UartRx {
    /// The ring index the DMA writes next.
    fn head(&self) -> usize {
        edma::write_index(&self.ring)
    }

    /// Clears a receiver overrun (which stops reception until cleared);
    /// cannot happen while the DMA runs, kept as a guard.
    fn clear_overrun(&self) {
        if pac::LPUART1.stat().read().or() {
            pac::LPUART1.stat().write(|w| w.set_or(true));
        }
    }

    /// Copies what has arrived into `buf`; 0 when nothing has.
    fn take(&mut self, buf: &mut [u8]) -> usize {
        let head = self.head();
        let mut n = 0;
        while self.tail != head && n < buf.len() {
            if let (Some(b), Some(slot)) = (RX_RING.get(self.tail), buf.get_mut(n)) {
                *slot = b.load(Relaxed);
            }
            self.tail = (self.tail + 1) % RX_RING_LEN;
            n += 1;
        }
        n
    }
}

impl ErrorType for UartRx {
    type Error = Never;
}

impl Read for UartRx {
    /// Waits for at least one byte. Cancel-safe: the ring keeps whatever
    /// arrives while no read is waiting.
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Never> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let n = self.take(buf);
            if n > 0 {
                return Ok(n);
            }
            self.clear_overrun();
            Timer::after(RX_POLL).await;
        }
    }
}

impl ErrorType for UartTx {
    type Error = Never;
}

impl Write for UartTx {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Never> {
        let mut n = 0;
        for &b in buf {
            while pac::LPUART1.stat().read().tdre() == pac::lpuart::vals::Tdre::Txdata {
                if n > 0 {
                    return Ok(n);
                }
                Timer::after(TX_POLL).await;
            }
            pac::LPUART1.data().write_value(pac::lpuart::regs::Data(u32::from(b)));
            n += 1;
        }
        Ok(n)
    }

    async fn flush(&mut self) -> Result<(), Never> {
        let deadline = Instant::now() + FLUSH_TIMEOUT;
        while pac::LPUART1.stat().read().tc() == pac::lpuart::vals::Tc::Active && Instant::now() < deadline {
            Timer::after(TX_POLL).await;
        }
        Ok(())
    }
}
