//! SPI loopback: checks the SPI DMA path end to end with a jumper wire.
//!
//! **Wiring:** connect [`MOSI_PIN`] to [`MISO_PIN`] (GPIO 11 → GPIO 13 on an
//! ESP32-S3-DevKitC-1). Nothing else is needed; [`SCK_PIN`] and [`CS_PIN`]
//! may be left open or probed with a logic analyser. The pins are the
//! ESP32-S3 SPI2 (FSPI) defaults and are free on the dev boards. The
//! simulator's virtual SPI device loops MOSI back to MISO the same way.
//!
//! * `init` opens SPI bus 0 at [`SPI_KHZ`] in mode 0 and starts hardware
//!   timer [`TIMER_ID`] with a [`PERIOD_US`] (100 ms) period.
//! * Each timer event starts a [`LEN`]-byte DMA transfer of a counter
//!   [`pattern`] and returns immediately.
//! * The `DmaDone` event reads the received bytes and compares them with
//!   what was sent ([`verdict`]); the ok or error count is incremented and
//!   published on [`OK_KEY`] / [`ERR_KEY`] as ASCII decimal.
//! * A timer event that finds the previous transfer still in flight is
//!   skipped. After [`STALL_PERIODS`] such periods the completion is assumed
//!   lost (event queue overflow) and the bus is recovered.
#![cfg_attr(not(test), no_std)]
#![deny(unsafe_code)]

use mw_payload::irq::Event;
use mw_payload::spi::{Mode, Spi, SpiConfig};
use mw_payload::timer::Timer;
use mw_payload::{AbiError, LogLine, Payload};

/// SPI clock (ESP32-S3 FSPI default).
pub const SCK_PIN: u8 = 12;
/// Controller out; jumper it to [`MISO_PIN`].
pub const MOSI_PIN: u8 = 11;
/// Controller in.
pub const MISO_PIN: u8 = 13;
/// Hardware chip select.
pub const CS_PIN: u8 = 10;
/// SPI clock in kHz.
pub const SPI_KHZ: u32 = 1_000;
/// Hardware timer driving the transfers.
pub const TIMER_ID: u8 = 0;
/// Transfer period: 100 ms.
pub const PERIOD_US: u32 = 100_000;
/// Bytes per transfer.
pub const LEN: usize = 16;
/// Busy periods after which a completion is assumed lost.
pub const STALL_PERIODS: u32 = 10;
/// Key the count of matching transfers is published on.
pub const OK_KEY: &str = "app/spi/ok";
/// Key the count of failed or mismatching transfers is published on.
pub const ERR_KEY: &str = "app/spi/err";

/// The bytes sent in transfer `seq`: `seq, seq+1, ...` (wrapping).
pub fn pattern(seq: u8) -> [u8; LEN] {
    core::array::from_fn(|i| seq.wrapping_add(i as u8))
}

/// Whether a transfer succeeded: it completed with `LEN` bytes, `copied`
/// of them were read back, and they equal `sent`.
pub fn verdict(
    sent: &[u8; LEN],
    done: Result<usize, AbiError>,
    copied: Result<usize, AbiError>,
    rx: &[u8; LEN],
) -> bool {
    done == Ok(LEN) && copied == Ok(LEN) && rx == sent
}

/// Application state.
pub struct Loopback {
    spi: Option<Spi>,
    _timer: Option<Timer>,
    seq: u8,
    sent: [u8; LEN],
    busy_periods: u32,
    ok: u32,
    err: u32,
}

/// Logs `"spi_loopback: <what> error <code>"`.
fn log_err(what: &str, err: AbiError) {
    let mut line = LogLine::new();
    line.push_str("spi_loopback: ")
        .push_str(what)
        .push_str(" error ")
        .push_u32(err.code().unsigned_abs());
    line.log();
}

/// Publishes `n` as ASCII decimal on `key`.
fn publish_count(key: &str, n: u32) {
    let mut text = LogLine::new();
    text.push_u32(n);
    if let Err(e) = mw_payload::publish(key, text.as_str().as_bytes()) {
        log_err("publish", e);
    }
}

fn open() -> Result<Spi, AbiError> {
    let config = SpiConfig::builder(SCK_PIN)
        .mosi(MOSI_PIN)
        .miso(MISO_PIN)
        .cs(CS_PIN)
        .freq_khz(SPI_KHZ)
        .mode(Mode::Mode0)
        .build()?;
    Spi::open(config)
}

impl Loopback {
    fn record(&mut self, good: bool) {
        if good {
            self.ok = self.ok.wrapping_add(1);
            publish_count(OK_KEY, self.ok);
        } else {
            self.err = self.err.wrapping_add(1);
            publish_count(ERR_KEY, self.err);
        }
    }

    fn on_timer(&mut self) {
        let Some(spi) = self.spi.as_mut() else {
            return;
        };
        let mut failed = false;
        if spi.is_busy() {
            self.busy_periods += 1;
            if self.busy_periods < STALL_PERIODS {
                return;
            }
            mw_payload::log("spi_loopback: completion lost, recovering");
            spi.recover();
            failed = true;
        }
        self.busy_periods = 0;
        let tx = pattern(self.seq);
        match spi.transfer(&tx, LEN) {
            Ok(()) => {
                self.sent = tx;
                self.seq = self.seq.wrapping_add(1);
            }
            // The host's DMA engine is still running; retry next period.
            Err(AbiError::Busy) => {}
            Err(e) => {
                log_err("spi_transfer", e);
                failed = true;
            }
        }
        if failed {
            self.record(false);
        }
    }

    fn on_done(&mut self, done: Result<usize, AbiError>) {
        let Some(spi) = self.spi.as_mut() else {
            return;
        };
        let mut rx = [0u8; LEN];
        let copied = match done {
            Ok(_) => spi.read_into(&mut rx),
            Err(e) => Err(e),
        };
        let good = verdict(&self.sent, done, copied, &rx);
        self.record(good);
    }
}

impl Payload for Loopback {
    fn init() -> Self {
        let spi = open().map_err(|e| log_err("spi_open", e)).ok();
        let timer = Timer::start_periodic(TIMER_ID, PERIOD_US)
            .map_err(|e| log_err("timer_start", e))
            .ok();
        mw_payload::log("spi_loopback: started (jumper MOSI to MISO)");
        Loopback {
            spi,
            _timer: timer,
            seq: 0,
            sent: [0; LEN],
            busy_periods: 0,
            ok: 0,
            err: 0,
        }
    }

    fn on_irq(&mut self, event: Event) {
        match event {
            Event::Timer { id: TIMER_ID, .. } => self.on_timer(),
            Event::DmaDone { .. } => {
                if let Some(done) = self.spi.as_mut().and_then(|s| s.on_event(&event)) {
                    self.on_done(done);
                }
            }
            _ => {}
        }
    }
}

// The exports need a single-threaded wasm32 module; host builds only run
// the unit tests below.
#[cfg(target_arch = "wasm32")]
mw_payload::payload!(Loopback);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pattern_counts_and_wraps() {
        assert_eq!(pattern(0)[..4], [0, 1, 2, 3]);
        assert_eq!(pattern(250)[5..7], [255, 0]);
    }

    #[test]
    fn verdict_requires_a_full_matching_echo() {
        let tx = pattern(7);
        assert!(verdict(&tx, Ok(LEN), Ok(LEN), &tx));
        assert!(!verdict(&tx, Ok(LEN), Ok(LEN), &pattern(8)), "mismatch");
        assert!(!verdict(&tx, Ok(LEN - 1), Ok(LEN), &tx), "short transfer");
        assert!(!verdict(&tx, Ok(LEN), Ok(3), &tx), "short read");
        assert!(!verdict(&tx, Err(AbiError::Transport), Ok(LEN), &tx));
        assert!(!verdict(&tx, Ok(LEN), Err(AbiError::Busy), &tx));
    }
}
