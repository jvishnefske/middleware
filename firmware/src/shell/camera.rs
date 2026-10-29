//! The ESP32-CAM's OV2640 behind `SourceKind::Camera` (R15): I2S0 in
//! camera mode with its DMA, feeding the radio task's source route.
//!
//! The payload sets the sensor up (power, XCLK on PWM, registers over
//! I2C); this module only receives what the sensor clocks out.
//!
//! # Hardware
//!
//! [`install`] (boot) builds esp-hal's I2S0 receiver with DMA channel
//! `DMA_I2S0` and no pins: 16-bit mono receive, which makes esp-hal set the
//! receive FIFO to mode 1 (two samples per 32-bit word,
//! `esp_base::camera::SampleMode::HalfWord`) and the channel mode to 1, and a
//! 312.5 kHz "sample rate" whose 256× master clock puts the I2S module
//! clock at 80 MHz (divider 2), fast enough to sample PCLK. (FIFO mode 3,
//! one sample per word as esp32-camera uses for JPEG, lost every other
//! byte with esp-hal's 32-bit width on this board; mode 1 halves the DMA
//! traffic too.) The sensor's PCLK must stay within what the receiver can
//! sample: the payload runs the OV2640 without its clock doubler and with
//! a fixed DVP divider of 8. It then turns
//! on camera mode with single-bit register fields only (safe PAC writers,
//! reached through `I2S0::regs()`): slave receive, LCD and camera enable,
//! no MSB shift or short sync, `rx_dsync_sw`. Multi-bit fields (FIFO mode,
//! channel mode, bit width, clock dividers, the DMA link) are written by
//! esp-hal from the config, so no `unsafe` is needed here.
//!
//! [`route`] (route start) wires the pins through the GPIO matrix: each of
//! D0..D7 feeds `I2S0I_DATA_<n>` and `I2S0I_DATA_<n+8>`, HREF feeds
//! `I2S0I_H_ENABLE`, PCLK feeds `I2S0I_WS`; `I2S0I_H_SYNC` and
//! `I2S0I_V_SYNC` are held high, so the receiver samples whenever HREF is
//! high (the OV2640 in JPEG mode raises HREF only for JPEG bytes). VSYNC
//! is claimed and left an input: frame boundaries come from the JPEG
//! markers, which also skip the partial frame a capture starts in.
//!
//! # Buffers and copies
//!
//! The first camera route allocates, once for the life of the firmware,
//! a DMA ring ([`RING_BYTES`] in [`DESCRIPTORS`] descriptors, an esp-hal
//! `DmaRxStreamBuf`) and the frame buffer ([`FRAME_BYTES`]) from the heap
//! (`try_reserve`, so a short heap is `NoResources`, not a panic); boards
//! that never start a camera route never pay for them. A capture
//! ([`capture`], radio task) starts the DMA, and while it runs takes each
//! filled stretch of the ring in place (`peek`), extracts the sample bytes
//! straight into a 256-byte scratch block (`esp_base::camera::extract`),
//! runs them through the `Assembler` into the frame buffer, and hands the
//! stretch back to the DMA (`consume`). Each camera byte is thus copied
//! twice before the radio: DMA word → scratch → frame buffer (the
//! hardware's word-per-sample layout forbids pointing at the ring), then
//! once more into each ESP-NOW frame by `write_frame`. A whole-frame buffer
//! is needed because every chunk header carries the frame's length. A
//! frame larger than the buffer, a ring the DMA filled before we drained
//! it, or no complete frame within [`CAPTURE_TIMEOUT_MS`] drops the frame
//! (`RouteDropped`).
//!
//! The parts rest in [`HOME`] (a critical-section mutex, never held
//! across an await): the radio task takes them for a capture and puts
//! them back; it reads the frame buffer under the lock to fill each chunk.

use alloc::vec::Vec;
use core::cell::RefCell;

use embassy_futures::yield_now;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::Instant;
use esp_base::camera::{Assembler, SampleMode, Status, extract};
use esp_base::route::source::CaptureFault;
use esp_hal::Blocking;
use esp_hal::dma::aligned::DmaAlignedMut;
use esp_hal::dma::{DmaDescriptor, DmaRxStreamBuf};
use esp_hal::gpio::{InputSignal, Level};
use esp_hal::i2s::master::{Channels, DataFormat, I2s, I2sRx, TdmConfig};
use esp_hal::peripherals::{DMA_I2S0, I2S0};
use esp_hal::time::Rate;
use wasm_host::{AbiError, CameraPins, Pull};

use super::gpio::PinBank;

/// How the DMA lays the samples out (FIFO mode 1, set by the 16-bit mono
/// config in [`install`]).
pub const MODE: SampleMode = SampleMode::HalfWord;

/// DMA descriptors in the ring (esp-hal's stream buffer needs at least 4).
pub const DESCRIPTORS: usize = 8;

/// DMA ring bytes: [`DESCRIPTORS`] stretches of 2 KiB, 8 KiB of camera
/// bytes in all at two DMA bytes per camera byte.
pub const RING_BYTES: usize = DESCRIPTORS * 2048;

/// Largest frame: the badge viewer reassembles into 8 KiB slots, so a
/// larger frame would be dropped there anyway.
pub const FRAME_BYTES: usize = 8 * 1024;

/// Longest capture: room for the partial frame a capture starts in and a
/// whole frame after it at the OV2640's slowest JPEG rates.
pub const CAPTURE_TIMEOUT_MS: u64 = 600;

/// Camera bytes extracted per step.
const SCRATCH: usize = 256;

/// Input signals of the data lines: D<n> feeds both.
const DATA: [(InputSignal, InputSignal); 8] = [
    (InputSignal::I2S0I_DATA_0, InputSignal::I2S0I_DATA_8),
    (InputSignal::I2S0I_DATA_1, InputSignal::I2S0I_DATA_9),
    (InputSignal::I2S0I_DATA_2, InputSignal::I2S0I_DATA_10),
    (InputSignal::I2S0I_DATA_3, InputSignal::I2S0I_DATA_11),
    (InputSignal::I2S0I_DATA_4, InputSignal::I2S0I_DATA_12),
    (InputSignal::I2S0I_DATA_5, InputSignal::I2S0I_DATA_13),
    (InputSignal::I2S0I_DATA_6, InputSignal::I2S0I_DATA_14),
    (InputSignal::I2S0I_DATA_7, InputSignal::I2S0I_DATA_15),
];

/// The camera's hardware and (after the first route) its buffers.
struct Parts {
    rx: I2sRx<'static, Blocking>,
    ring: Option<DmaRxStreamBuf>,
    frame: Option<&'static mut [u8]>,
    /// Captures so far (the first logs its raw words).
    captures: u32,
}

/// Where the camera rests (see the module docs).
static HOME: Mutex<CriticalSectionRawMutex, RefCell<Option<Parts>>> = Mutex::new(RefCell::new(None));

fn with_home<R>(f: impl FnOnce(&mut Option<Parts>) -> R) -> Option<R> {
    HOME.lock(|c| c.try_borrow_mut().ok().map(|mut h| f(&mut h)))
}

/// Builds the I2S0 camera receiver at boot (see the module docs); `false`
/// when esp-hal refused the config.
pub fn install(i2s0: I2S0<'static>, dma: DMA_I2S0<'static>) -> bool {
    let config = TdmConfig::new_tdm_philips()
        .with_sample_rate(Rate::from_hz(312_500))
        .with_data_format(DataFormat::Data16Channel16)
        .with_channels(Channels::MONO);
    let Ok(i2s) = I2s::new(i2s0, dma, config) else { return false };
    let rx = i2s.i2s_rx.build();
    let regs = I2S0::regs();
    regs.conf().modify(|_, w| {
        w.rx_slave_mod().set_bit();
        w.rx_right_first().clear_bit();
        w.rx_msb_right().clear_bit();
        w.rx_msb_shift().clear_bit();
        w.rx_mono().clear_bit();
        w.rx_short_sync().clear_bit()
    });
    regs.conf2().modify(|_, w| {
        w.lcd_en().set_bit();
        w.camera_en().set_bit()
    });
    regs.fifo_conf().modify(|_, w| {
        w.dscr_en().set_bit();
        w.rx_fifo_mod_force_en().set_bit()
    });
    regs.timing().modify(|_, w| w.rx_dsync_sw().set_bit());
    with_home(|h| *h = Some(Parts { rx, ring: None, frame: None, captures: 0 })).is_some()
}

/// `n` zeroed heap bytes, 4-byte aligned, kept forever; `NoResources`
/// when the heap cannot hold them.
fn leak_bytes(n: usize) -> Result<&'static mut [u8], AbiError> {
    let mut v: Vec<u8> = Vec::new();
    v.try_reserve_exact(n + 4).map_err(|_| AbiError::NoResources)?;
    v.resize(n + 4, 0);
    let all = v.leak();
    let skew = all.as_ptr() as usize % 4;
    let start = (4 - skew) % 4;
    all.get_mut(start..start + n).ok_or(AbiError::NoResources)
}

/// The DMA ring.
fn ring() -> Result<DmaRxStreamBuf, AbiError> {
    let mut d: Vec<DmaDescriptor> = Vec::new();
    d.try_reserve_exact(DESCRIPTORS).map_err(|_| AbiError::NoResources)?;
    d.resize(DESCRIPTORS, DmaDescriptor::EMPTY);
    let descriptors = DmaAlignedMut::new(d.leak()).map_err(|_| AbiError::NoResources)?;
    let buffer = DmaAlignedMut::new(leak_bytes(RING_BYTES)?).map_err(|_| AbiError::NoResources)?;
    DmaRxStreamBuf::new(descriptors, buffer).map_err(|_| AbiError::NoResources)
}

/// Makes sure the camera exists and its buffers are allocated (once);
/// returns the free heap afterwards. `Unsupported` without the camera
/// receiver, `NoResources` when the heap is short, `Busy` while a capture
/// holds the parts.
pub fn prepare() -> Result<usize, AbiError> {
    with_home(|h| {
        let parts = h.as_mut().ok_or(AbiError::Busy)?;
        if parts.frame.is_none() {
            parts.frame = Some(leak_bytes(FRAME_BYTES)?);
        }
        if parts.ring.is_none() {
            parts.ring = Some(ring()?);
        }
        Ok(esp_alloc::HEAP.free())
    })
    .unwrap_or(Err(AbiError::Busy))
}

/// Whether the receiver was installed at boot.
pub fn present() -> bool {
    with_home(|h| h.is_some()).unwrap_or(true)
}

/// Routes the camera's pins (see the module docs).
pub fn route(bank: &mut PinBank, pins: &CameraPins) -> Result<(), AbiError> {
    for (&pin, (lo, hi)) in pins.data.iter().zip(DATA) {
        bank.route_in(pin, lo)?;
        bank.route_in(pin, hi)?;
    }
    bank.route_in(pins.href, InputSignal::I2S0I_H_ENABLE)?;
    bank.route_in(pins.pclk, InputSignal::I2S0I_WS)?;
    bank.input(pins.vsync, Pull::None)?;
    InputSignal::I2S0I_H_SYNC.connect_to(&Level::High);
    InputSignal::I2S0I_V_SYNC.connect_to(&Level::High);
    Ok(())
}

/// Runs `f` on the captured frame's first `len` bytes (radio task, to
/// fill one chunk); `None` while the parts are away or `len` is too long.
pub fn with_frame<R>(len: u32, f: impl FnOnce(&[u8]) -> R) -> Option<R> {
    with_home(|h| {
        let frame = h.as_ref()?.frame.as_deref()?;
        let n = usize::try_from(len).ok()?;
        frame.get(..n).map(f)
    })
    .flatten()
}

/// What a capture saw, for the log.
#[derive(Debug, Clone, Copy, Default)]
pub struct Seen {
    /// DMA bytes taken from the ring.
    pub raw: u32,
    /// Camera bytes skipped before the frame's start-of-image.
    pub skipped: u32,
    /// Milliseconds the capture took.
    pub ms: u32,
    /// The first 8 DMA bytes of the capture (the sample layout, for the
    /// log).
    pub head: [u8; 8],
}

/// Captures one JPEG frame into the frame buffer (see the module docs);
/// returns its length.
pub async fn capture() -> (Result<u32, CaptureFault>, Seen) {
    let mut seen = Seen::default();
    let Some(Some(mut parts)) = with_home(Option::take) else {
        return (Err(CaptureFault::Hardware), seen);
    };
    let (Some(ring), Some(frame)) = (parts.ring.take(), parts.frame.take()) else {
        with_home(|h| *h = Some(parts));
        return (Err(CaptureFault::Hardware), seen);
    };
    parts.captures = parts.captures.saturating_add(1);
    let Parts { rx, captures, .. } = parts;
    let (result, rx, ring) = match rx.read(ring) {
        Ok(mut transfer) => {
            let result = drain(&mut transfer, frame, &mut seen).await;
            let (rx, ring) = transfer.stop();
            (result, rx, ring)
        }
        Err((_, rx, ring)) => (Err(CaptureFault::Hardware), rx, ring),
    };
    with_home(|h| *h = Some(Parts { rx, ring: Some(ring), frame: Some(frame), captures }));
    (result, seen)
}

/// The capture loop: bounded by [`CAPTURE_TIMEOUT_MS`] of wall time, and
/// each step consumes ring bytes or yields.
async fn drain(
    transfer: &mut esp_hal::i2s::master::I2sRxDmaTransfer<'static, Blocking, DmaRxStreamBuf>,
    frame: &mut [u8],
    seen: &mut Seen,
) -> Result<u32, CaptureFault> {
    let start = Instant::now();
    let mut asm = Assembler::new();
    let mut scratch = [0u8; SCRATCH];
    let mut head_at = 0;
    loop {
        let elapsed = start.elapsed().as_millis();
        seen.ms = u32::try_from(elapsed).unwrap_or(u32::MAX);
        if elapsed > CAPTURE_TIMEOUT_MS {
            seen.skipped = asm.skipped();
            return Err(CaptureFault::Timeout);
        }
        let (used, n) = {
            let avail = transfer.peek();
            if avail.is_empty() {
                (0, 0)
            } else {
                for (dst, src) in seen.head.iter_mut().skip(head_at).zip(avail) {
                    *dst = *src;
                    head_at += 1;
                }
                match extract(MODE, avail, &mut scratch) {
                    // A stray partial word: drop it rather than spin.
                    (0, _) => (avail.len(), 0),
                    step => step,
                }
            }
        };
        if used == 0 {
            if transfer.is_done() {
                // The DMA ran out of descriptors before we drained them.
                seen.skipped = asm.skipped();
                return Err(CaptureFault::Timeout);
            }
            yield_now().await;
            continue;
        }
        transfer.consume(used);
        seen.raw = seen.raw.saturating_add(u32::try_from(used).unwrap_or(u32::MAX));
        let bytes = scratch.get(..n).unwrap_or(&[]);
        let (next, _, status) = asm.feed(bytes, frame);
        asm = next;
        match status {
            Status::More => {}
            Status::Done(len) => {
                seen.skipped = asm.skipped();
                return Ok(len);
            }
            Status::Overflow => return Err(CaptureFault::Overflow),
        }
    }
}
