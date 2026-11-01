//! LED strips for payloads (ABI 5): WS2812 frames on RMT channel 0.
//!
//! [`LedStrip::new`] takes the RMT peripheral at boot, sets its counter to
//! [`RMT_HZ`](esp_base::ledstrip::RMT_HZ) (80 MHz, refused if the chip
//! cannot reach it exactly), and configures channel 0 for transmit without
//! a pin: clock divider 1, no carrier, idle output low, and [`MEM_BLOCKS`]
//! memory blocks. The device connects [`SIGNAL`] (`RMT_SIG_0`) to the data
//! pin through the pin bank (`PinBank::route_out`), like the SPI and UART
//! signals, so the pin stays in the bank and is released with it.
//!
//! The whole frame must sit in channel RAM, so the CPU never refills it
//! while a radio interrupt could delay the refill and break the timing:
//! [`LEDS`] is `esp_base::ledstrip::capacity` of `MEM_BLOCKS` blocks of
//! `esp_hal::rmt::CHANNEL_RAM_SIZE` codes (8 LEDs on the ESP32-S3 with 48
//! words per block, 10 on the ESP32 with 64), and [`LIMITS`] is one strip of
//! that length. A longer chain is refused at `ledstrip_open`.
//!
//! [`LedStrip::send`] is bounded in time:
//!
//! 1. the frame is encoded (`esp_base::ledstrip::encode_into`) straight
//!    into a fixed `[PulseCode; CODES]` buffer;
//! 2. if the previous frame left the wire less than `RESET_US` (300 µs)
//!    ago, it busy-waits out the rest of the latch gap
//!    (`esp_base::ledstrip::latch_wait`, at most `RESET_US`);
//! 3. it transmits on a *reborrow* of the channel and polls until the
//!    transmission ends or `esp_base::ledstrip::deadline_us` (wire time
//!    plus 200 µs) passes. On a timeout the transaction is dropped, which
//!    makes esp-hal stop the channel, and the frame fails with
//!    `Transport`. Because only the reborrow is consumed, the channel
//!    itself is never lost, whatever the outcome.
//!
//! The time between frames is measured with esp-hal's system timer,
//! truncated to a wrapping 32-bit microsecond count.

use esp_base::ledstrip::{Limits, Micros, RMT_HZ, capacity, deadline_us, encode_into, latch_wait};
use esp_hal::Blocking;
use esp_hal::gpio::{Level, OutputSignal};
use esp_hal::peripherals::RMT;
use esp_hal::rmt::{CHANNEL_RAM_SIZE, Channel, PulseCode, Rmt, Tx, TxChannelConfig, TxChannelCreator};
use esp_hal::time::{Instant, Rate};
use middleware::wasm_abi::ledstrip::BYTES_PER_LED;
use wasm_host::AbiError;

/// RMT memory blocks given to channel 0 (channels 1–3 lose theirs, which
/// the firmware does not use).
pub const MEM_BLOCKS: u8 = 4;

/// Pulse codes channel 0's RAM holds.
const CODES: usize = MEM_BLOCKS as usize * CHANNEL_RAM_SIZE;

/// Longest chain one frame can carry.
pub const LEDS: usize = capacity(MEM_BLOCKS as usize, CHANNEL_RAM_SIZE);

/// What the board drives: one strip (RMT channel 0) of [`LEDS`] LEDs.
pub const LIMITS: Limits = match Limits::new(1, LEDS) {
    Some(l) => l,
    None => panic!("RMT channel RAM holds no LED"),
};

/// The GPIO-matrix output signal of RMT channel 0.
pub const SIGNAL: OutputSignal = OutputSignal::RMT_SIG_0;

/// Microseconds since boot, wrapping at 2³².
fn now() -> Micros {
    Micros(Instant::now().duration_since_epoch().as_micros() as u32)
}

/// Busy-waits `us` microseconds (callers pass at most `RESET_US`).
fn wait_us(us: u32) {
    if us == 0 {
        return;
    }
    let start = Instant::now();
    while start.elapsed().as_micros() < u64::from(us) {}
}

/// RMT channel 0 as a WS2812 transmitter.
pub struct LedStrip {
    channel: Channel<'static, Blocking, Tx>,
    codes: [PulseCode; CODES],
    /// When the previous frame left the wire.
    last_end: Option<Micros>,
}

impl LedStrip {
    /// Configures RMT channel 0 for transmit without a pin; `None` if the
    /// RMT cannot run at 80 MHz or the channel's memory is unavailable.
    pub fn new(rmt: RMT<'static>) -> Option<Self> {
        let rmt = Rmt::new(rmt, Rate::from_hz(RMT_HZ)).ok()?;
        if rmt.frequency() != Rate::from_hz(RMT_HZ) {
            return None;
        }
        let config = TxChannelConfig::default()
            .with_clk_divider(1)
            .with_idle_output(true)
            .with_idle_output_level(Level::Low)
            .with_carrier_modulation(false)
            .with_memsize(MEM_BLOCKS);
        let channel = rmt.channel0.configure_tx(&config).ok()?;
        Some(LedStrip { channel, codes: [PulseCode::default(); CODES], last_end: None })
    }

    /// Sends one frame of G, R, B bytes and returns once it is on the wire.
    /// `InvalidArgument` for a frame longer than [`LEDS`] (the device's
    /// strip table refuses those first), `Transport` if the RMT reports an
    /// error or the frame misses its deadline.
    pub fn send(&mut self, grb: &[u8]) -> Result<(), AbiError> {
        let n = encode_into(grb, &mut self.codes, PulseCode).map_err(|_| AbiError::InvalidArgument)?;
        let frame = self.codes.get(..n).ok_or(AbiError::InvalidArgument)?;
        wait_us(latch_wait(self.last_end, now()));
        let deadline = u64::from(deadline_us(grb.len() / BYTES_PER_LED));
        let start = Instant::now();
        // Only the reborrow moves into the transaction, so the channel
        // survives every path below.
        let result = match self.channel.reborrow().transmit(frame) {
            Ok(mut tx) => {
                let mut done = tx.poll();
                while !done && start.elapsed().as_micros() <= deadline {
                    done = tx.poll();
                }
                if done {
                    // Ended: `wait` returns at once.
                    tx.wait().map(drop).map_err(|_| AbiError::Transport)
                } else {
                    // Dropping the transaction stops the channel.
                    drop(tx);
                    Err(AbiError::Transport)
                }
            }
            Err(_) => Err(AbiError::Transport),
        };
        self.last_end = Some(now());
        result
    }
}
