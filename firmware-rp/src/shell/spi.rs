//! SPI bus 0 for payloads (ABI 2 `spi_open` / `spi_transfer` /
//! `dma_read`): SPI0 or SPI1, whichever controller the payload's pins
//! select (`rp_base::periph::func::SpiRoute`).
//!
//! Like I2C ([`super::i2c`]) the PL022 controllers are driven through
//! embassy-rp's PAC: embassy-rp's `Spi` takes its pins by type, which the
//! pin bank cannot give it. There is no DMA: a transfer is copied into a
//! firmware buffer of `MAX_DMA` bytes (zero padded to its length) and
//! pumped through the 8-byte FIFOs by [`SpiBus::step`], which the device's
//! `io` future calls between yields to the executor, so `spi_transfer`
//! returns at once and the controller loop raises `DmaDone` when the last
//! byte is in (`rp_base::periph::spi` has the pump, the divider and the
//! time bound; `esp_base::periph::dma::DmaState` decides `Busy` and what
//! `dma_read` returns). A transfer past its deadline resets the controller
//! and completes with `Transport`. Without a MISO pin the received bytes
//! read as zeros.
//!
//! The chip select is not the controller's CSn: the device drives the
//! payload's CS pin (a `Flex` output) low for the whole transfer and high
//! when it ends ([`SpiBus::cs`]).

use embassy_rp::Peri;
use embassy_rp::pac;
use embassy_rp::peripherals::{SPI0, SPI1};
use embassy_time::{Duration, Instant};
use esp_base::periph::dma::DmaState;
use middleware::wasm_abi::AbiError;
use middleware::wasm_abi::periph::{MAX_DMA, SpiConfig};
use rp_base::periph::func::{SpiBlock, SpiRoute};
use rp_base::periph::spi::{Clock, FIFO_DEPTH, Pump, mode_bits, timeout_us};

/// Longest wait for a controller to leave reset.
const RESET_WAIT: Duration = Duration::from_millis(1);

/// The firmware buffer one transfer goes through.
pub type Buffer = [u8; MAX_DMA];

/// The open bus: its pins and clock.
#[derive(Debug, Clone, Copy)]
struct Open {
    route: SpiRoute,
    clock: Clock,
    /// `(SPO, SPH)`.
    mode: (bool, bool),
    hz: u32,
}

/// The running transfer.
#[derive(Debug, Clone, Copy)]
struct Xfer {
    pump: Pump,
    deadline: Instant,
}

/// SPI bus 0: both controllers, the buffer and the transfer state.
pub struct SpiBus {
    _spi0: Peri<'static, SPI0>,
    _spi1: Peri<'static, SPI1>,
    buf: &'static mut Buffer,
    open: Option<Open>,
    state: DmaState,
    xfer: Option<Xfer>,
}

const fn regs(block: SpiBlock) -> pac::spi::Spi {
    match block {
        SpiBlock::Spi0 => pac::SPI0,
        SpiBlock::Spi1 => pac::SPI1,
    }
}

fn hold_reset(block: SpiBlock, held: bool) {
    pac::RESETS.reset().modify(|w| match block {
        SpiBlock::Spi0 => w.set_spi0(held),
        SpiBlock::Spi1 => w.set_spi1(held),
    });
}

/// Resets `block` and waits (bounded) for it to come out of reset.
fn reset(block: SpiBlock) -> Result<(), AbiError> {
    hold_reset(block, true);
    hold_reset(block, false);
    let deadline = Instant::now() + RESET_WAIT;
    loop {
        let done = pac::RESETS.reset_done().read();
        if match block {
            SpiBlock::Spi0 => done.spi0(),
            SpiBlock::Spi1 => done.spi1(),
        } {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(AbiError::Unsupported);
        }
    }
}

/// Resets and configures the open bus's controller: 8-bit Motorola frames,
/// the mode and divider, enabled as controller.
fn configure(open: &Open) -> Result<(), AbiError> {
    reset(open.route.block)?;
    let r = regs(open.route.block);
    r.cpsr().write(|w| w.set_cpsdvsr(open.clock.cpsdvsr));
    r.cr0().write(|w| {
        w.set_dss(0b0111);
        w.set_frf(0);
        w.set_spo(open.mode.0);
        w.set_sph(open.mode.1);
        w.set_scr(open.clock.scr);
    });
    r.cr1().write(|w| w.set_sse(true));
    Ok(())
}

impl SpiBus {
    /// Takes both controllers (held in reset until a payload opens the
    /// bus) and the transfer buffer.
    pub fn new(spi0: Peri<'static, SPI0>, spi1: Peri<'static, SPI1>, buf: &'static mut Buffer) -> Self {
        hold_reset(SpiBlock::Spi0, true);
        hold_reset(SpiBlock::Spi1, true);
        SpiBus { _spi0: spi0, _spi1: spi1, buf, open: None, state: DmaState::Idle, xfer: None }
    }

    /// Whether a transfer is running.
    pub fn busy(&self) -> bool {
        self.state.in_flight()
    }

    /// The open bus's chip select pin.
    pub fn cs(&self) -> Option<u8> {
        self.open.and_then(|o| o.route.cs)
    }

    /// Opens (or reconfigures) the bus on `route`'s controller at
    /// `cfg`'s clock and mode: `Busy` while a transfer runs, `Unsupported`
    /// for a clock below the slowest divider. The device routes the pins.
    pub fn open(&mut self, route: SpiRoute, cfg: &SpiConfig) -> Result<(), AbiError> {
        if self.busy() {
            return Err(AbiError::Busy);
        }
        let clk = embassy_rp::clocks::clk_peri_freq();
        let clock = Clock::new(clk, cfg.freq_khz.saturating_mul(1000))?;
        let mode = mode_bits(cfg.mode)?;
        let open = Open { route, clock, mode, hz: clock.hz(clk) };
        if let Some(old) = self.open
            && old.route.block != route.block
        {
            hold_reset(old.route.block, true);
        }
        configure(&open)?;
        self.open = Some(open);
        self.state = DmaState::Idle;
        Ok(())
    }

    /// Starts a `len`-byte transfer of `tx` (zero padded): `Busy` if the
    /// bus is not open or a transfer runs, `InvalidArgument` for a length
    /// the buffer cannot hold.
    pub fn transfer(&mut self, tx: &[u8], len: usize) -> Result<(), AbiError> {
        let open = self.open.ok_or(AbiError::Busy)?;
        let buf = self.buf.get_mut(..len).ok_or(AbiError::InvalidArgument)?;
        if tx.len() > len {
            return Err(AbiError::InvalidArgument);
        }
        let state = self.state.start(len)?;
        let (head, tail) = buf.split_at_mut(tx.len());
        head.copy_from_slice(tx);
        tail.fill(0);
        let budget = Duration::from_micros(timeout_us(open.hz, len));
        self.xfer = Some(Xfer { pump: Pump::new(len), deadline: Instant::now() + budget });
        self.state = state;
        Ok(())
    }

    /// One bounded pump step (at most `FIFO_DEPTH` bytes each way). Returns
    /// the `DmaDone` value (length, or the negative error code) when the
    /// transfer ended, `None` while it runs or when none does.
    pub fn step(&mut self) -> Option<i32> {
        let (open, xfer) = (self.open?, self.xfer?);
        let r = regs(open.route.block);
        let mut pump = xfer.pump;
        for _ in 0..FIFO_DEPTH {
            match pump.next_send() {
                Some(i) if r.sr().read().tnf() => {
                    let byte = self.buf.get(i).copied().unwrap_or(0);
                    r.dr().write(|w| w.set_data(u16::from(byte)));
                    pump = pump.sent_one();
                }
                _ => break,
            }
        }
        for _ in 0..FIFO_DEPTH {
            match pump.next_receive() {
                Some(i) if r.sr().read().rne() => {
                    let byte = r.dr().read().data() as u8;
                    if let Some(slot) = self.buf.get_mut(i) {
                        *slot = if open.route.miso.is_some() { byte } else { 0 };
                    }
                    pump = pump.received_one();
                }
                _ => break,
            }
        }
        let result = if pump.done() {
            Ok(())
        } else if Instant::now() >= xfer.deadline {
            // Drop what is left in the FIFOs with the controller's state.
            let _ = configure(&open);
            Err(AbiError::Transport)
        } else {
            self.xfer = Some(Xfer { pump, ..xfer });
            return None;
        };
        self.xfer = None;
        let (state, value) = self.state.complete(result);
        self.state = state;
        value
    }

    /// `dma_read`: copies the received bytes into `out` once; `Busy` while
    /// a transfer runs, 0 when nothing is unread.
    pub fn read(&mut self, out: &mut [u8]) -> Result<usize, AbiError> {
        let (state, len) = self.state.take()?;
        self.state = state;
        let n = len.min(out.len());
        let src = self.buf.get(..n).ok_or(AbiError::InvalidArgument)?;
        out.get_mut(..n).ok_or(AbiError::InvalidArgument)?.copy_from_slice(src);
        Ok(n)
    }

    /// Closes the bus (payload stop): any transfer abandoned, both
    /// controllers held in reset.
    pub fn close(&mut self) {
        hold_reset(SpiBlock::Spi0, true);
        hold_reset(SpiBlock::Spi1, true);
        self.open = None;
        self.xfer = None;
        self.state = self.state.abort();
    }
}
