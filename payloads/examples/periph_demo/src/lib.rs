//! Peripheral demo: one SPI loopback transfer and one I2C register read,
//! repeated every second, with every result on `sys/log`.
//!
//! **Wiring** (defaults for the Raspberry Pi Pico 2 running `rp-base`):
//!
//! * SPI bus 0 on the RP2350's SPI0 pins: [`SCK_PIN`] GP18,
//!   [`MOSI_PIN`] GP19, [`MISO_PIN`] GP16, [`CS_PIN`] GP17. Jumper GP19 to
//!   GP16 for the loopback; SCK and CS may stay open or go to a logic
//!   analyser. On `esp-base` change the four pin constants, e.g. to the
//!   ESP32-S3's SPI2 defaults 12 / 11 / 13 / 10 that `spi_loopback` uses
//!   (jumper 11 to 13).
//! * I2C bus [`I2C_BUS`] at [`I2C_HZ`]. The board maps the bus to its pins
//!   (on `rp-base` bus 0 is SDA GP4, SCL GP5); the payload never names
//!   them.
//! * [`I2C_ADDR`] 0x50 / [`I2C_REG`] 0x00: the simulator's register device,
//!   and also where a 24Cxx EEPROM answers, for which reading register 0 is
//!   harmless. **Set both to a device actually wired on your bus.** The
//!   payload only ever addresses that one configured device: it never
//!   scans or probes other addresses, and it never writes a data register
//!   (the only byte written is the register pointer of the write-read).
//!
//! Behaviour:
//!
//! * `init` opens SPI bus 0 ([`SpiConfig`] builder, [`SPI_KHZ`], mode 0)
//!   and I2C bus [`I2C_BUS`], starts hardware timer [`TIMER_ID`] with a
//!   [`PERIOD_US`] (1 s) period, and runs one round at once.
//! * A round starts one [`LEN`]-byte SPI DMA transfer of [`PATTERN`] and
//!   does one I2C write-read: the register pointer [`I2C_REG`] is written
//!   to [`I2C_ADDR`], then one byte is read back after a repeated start.
//!   The read is logged ([`i2c_line`]: `periph_demo: i2c 0x50 reg 0x00 =
//!   0x5a` or `... error 5`) and, on success only, the byte is published
//!   raw on [`I2C_KEY`]; a failed read publishes nothing.
//! * The SPI `DmaDone` event reads the received bytes and compares them
//!   with [`PATTERN`] ([`verdict`]). The outcome is logged ([`spi_line`]:
//!   `periph_demo: spi loopback ok (8 bytes)`, or a short, mismatch or
//!   error line) and `ok` or `err` is published on [`SPI_KEY`].
//! * A round that finds the previous SPI transfer still in flight skips
//!   the SPI half. After [`STALL_PERIODS`] such rounds the completion is
//!   assumed lost (event queue overflow): the bus is recovered, `err` is
//!   published, and the next round transfers again.
//!
//! In `payload-sim`, preset the register with `!i2c 0 0x00 0x5a` on stdin;
//! the virtual SPI device loops MOSI back to MISO. The simulator reads
//! stdin after `init`, so the preset value shows from the first 1 s repeat
//! (the read in `init` sees the register's initial 0).
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use mw_payload::i2c::I2c;
use mw_payload::irq::Event;
use mw_payload::spi::{Mode, Spi, SpiConfig};
use mw_payload::timer::Timer;
use mw_payload::{AbiError, LogLine, Payload};

/// SPI clock (RP2350 SPI0, GP18).
pub const SCK_PIN: u8 = 18;
/// Controller out (GP19); jumper it to [`MISO_PIN`].
pub const MOSI_PIN: u8 = 19;
/// Controller in (GP16).
pub const MISO_PIN: u8 = 16;
/// Hardware chip select (GP17).
pub const CS_PIN: u8 = 17;
/// SPI clock in kHz.
pub const SPI_KHZ: u32 = 1_000;
/// I2C bus of the configured device (board-mapped pins).
pub const I2C_BUS: u8 = 0;
/// I2C clock: 100 kHz standard mode.
pub const I2C_HZ: u32 = 100_000;
/// 7-bit address of the one device read; set it to a device on your bus.
pub const I2C_ADDR: u8 = 0x50;
/// Register read from [`I2C_ADDR`]; only this pointer byte is written.
pub const I2C_REG: u8 = 0x00;
/// Hardware timer repeating the rounds.
pub const TIMER_ID: u8 = 0;
/// Round period: 1 s.
pub const PERIOD_US: u32 = 1_000_000;
/// Bytes per SPI transfer.
pub const LEN: usize = 8;
/// The bytes each SPI transfer sends: both nibble orders, the extremes and
/// single bits, so a stuck or swapped line shows up as a mismatch.
pub const PATTERN: [u8; LEN] = [0xA5, 0x5A, 0x00, 0xFF, 0x01, 0x80, 0x3C, 0xC3];
/// Rounds with the SPI transfer still in flight after which its
/// completion is assumed lost.
pub const STALL_PERIODS: u32 = 3;
/// Key `ok` / `err` is published on after each SPI transfer.
pub const SPI_KEY: &str = "app/periph_demo/spi";
/// Key the register byte is published on after each successful read.
pub const I2C_KEY: &str = "app/periph_demo/i2c";

/// Prefix of every log line.
const TAG: &str = "periph_demo: ";

/// How an SPI loopback transfer ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpiOutcome {
    /// All [`LEN`] bytes came back as sent.
    Ok,
    /// The transfer or the read-back failed.
    Failed(AbiError),
    /// Fewer than [`LEN`] bytes were transferred or read back.
    Short(usize),
    /// Byte `at` differs: `sent` went out, `got` came back.
    Mismatch {
        /// Index of the first differing byte.
        at: usize,
        /// Byte sent there.
        sent: u8,
        /// Byte received there.
        got: u8,
    },
}

/// Judges a finished transfer: its completion `done`, the read-back result
/// `copied`, and the received bytes `rx`, against `sent`. A failure is
/// reported before a short count, and a short count before a mismatch.
pub fn verdict(
    sent: &[u8; LEN],
    done: Result<usize, AbiError>,
    copied: Result<usize, AbiError>,
    rx: &[u8; LEN],
) -> SpiOutcome {
    let n = match (done, copied) {
        (Err(e), _) | (_, Err(e)) => return SpiOutcome::Failed(e),
        (Ok(d), Ok(c)) => d.min(c),
    };
    if n != LEN {
        return SpiOutcome::Short(n);
    }
    sent.iter()
        .zip(rx.iter())
        .enumerate()
        .find(|(_, (s, g))| s != g)
        .map_or(SpiOutcome::Ok, |(at, (&sent, &got))| SpiOutcome::Mismatch {
            at,
            sent,
            got,
        })
}

/// The ASCII hex digit of the low nibble of `n`.
fn hex_digit(n: u8) -> u8 {
    match n & 0x0F {
        d @ 0..=9 => b'0' + d,
        d => b'a' + (d - 10),
    }
}

/// Appends `b` as `0x` and two lowercase hex digits.
fn push_hex(line: &mut LogLine, b: u8) -> &mut LogLine {
    let digits = [b'0', b'x', hex_digit(b >> 4), hex_digit(b)];
    // ASCII only, so always valid UTF-8.
    line.push_bytes_lossy(&digits)
}

/// The log line for an SPI outcome, e.g. `periph_demo: spi loopback ok (8
/// bytes)`; error codes are the ABI's, without the sign.
pub fn spi_line(outcome: SpiOutcome) -> LogLine {
    let mut line = LogLine::new();
    line.push_str(TAG).push_str("spi loopback ");
    match outcome {
        SpiOutcome::Ok => {
            line.push_str("ok (").push_u32(LEN as u32).push_str(" bytes)");
        }
        SpiOutcome::Failed(e) => {
            line.push_str("error ").push_u32(e.code().unsigned_abs());
        }
        SpiOutcome::Short(n) => {
            line.push_str("short (")
                .push_u32(u32::try_from(n).unwrap_or(u32::MAX))
                .push_str(" of ")
                .push_u32(LEN as u32)
                .push_str(" bytes)");
        }
        SpiOutcome::Mismatch { at, sent, got } => {
            line.push_str("mismatch at ")
                .push_u32(u32::try_from(at).unwrap_or(u32::MAX))
                .push_str(": sent ");
            push_hex(&mut line, sent).push_str(" got ");
            push_hex(&mut line, got);
        }
    }
    line
}

/// The log line for a register read of `reg` at `addr`, e.g.
/// `periph_demo: i2c 0x50 reg 0x00 = 0x5a` or `... error 5`.
pub fn i2c_line(addr: u8, reg: u8, read: Result<u8, AbiError>) -> LogLine {
    let mut line = LogLine::new();
    line.push_str(TAG).push_str("i2c ");
    push_hex(&mut line, addr).push_str(" reg ");
    push_hex(&mut line, reg);
    match read {
        Ok(b) => {
            line.push_str(" = ");
            push_hex(&mut line, b);
        }
        Err(e) => {
            line.push_str(" error ").push_u32(e.code().unsigned_abs());
        }
    }
    line
}

/// What [`SPI_KEY`] carries for an outcome.
pub fn spi_status(outcome: SpiOutcome) -> &'static [u8] {
    match outcome {
        SpiOutcome::Ok => b"ok",
        _ => b"err",
    }
}

/// Logs `"periph_demo: <what> error <code>"`.
fn log_err(what: &str, err: AbiError) {
    let mut line = LogLine::new();
    line.push_str(TAG)
        .push_str(what)
        .push_str(" error ")
        .push_u32(err.code().unsigned_abs());
    line.log();
}

/// Publishes `data` on `key`, logging a failure.
fn publish(key: &str, data: &[u8]) {
    if let Err(e) = mw_payload::publish(key, data) {
        log_err("publish", e);
    }
}

fn open_spi() -> Result<Spi, AbiError> {
    let config = SpiConfig::builder(SCK_PIN)
        .mosi(MOSI_PIN)
        .miso(MISO_PIN)
        .cs(CS_PIN)
        .freq_khz(SPI_KHZ)
        .mode(Mode::Mode0)
        .build()?;
    Spi::open(config)
}

/// Reads register [`I2C_REG`] of [`I2C_ADDR`]: one write-read of the
/// register pointer and one data byte.
fn read_register(bus: &mut I2c) -> Result<u8, AbiError> {
    let mut rx = [0u8; 1];
    bus.transfer(I2C_ADDR, &[I2C_REG], &mut rx)?;
    Ok(rx[0])
}

/// Application state.
pub struct Demo {
    spi: Option<Spi>,
    i2c: Option<I2c>,
    _timer: Option<Timer>,
    busy_periods: u32,
}

impl Demo {
    /// Finishes an SPI transfer: logs and publishes the [`verdict`].
    fn finish_spi(&mut self, outcome: SpiOutcome) {
        spi_line(outcome).log();
        publish(SPI_KEY, spi_status(outcome));
    }

    /// Starts the SPI transfer, unless the previous one is in flight.
    fn start_spi(&mut self) {
        let Some(spi) = self.spi.as_mut() else {
            return;
        };
        if spi.is_busy() {
            self.busy_periods += 1;
            if self.busy_periods < STALL_PERIODS {
                return;
            }
            mw_payload::log("periph_demo: spi completion lost, recovering");
            spi.recover();
            self.busy_periods = 0;
            publish(SPI_KEY, b"err");
            return;
        }
        self.busy_periods = 0;
        if let Err(e) = spi.transfer(&PATTERN, LEN) {
            log_err("spi_transfer", e);
            publish(SPI_KEY, b"err");
        }
    }

    /// Reads the configured register; logs it and publishes it on success.
    fn read_i2c(&mut self) {
        let Some(bus) = self.i2c.as_mut() else {
            return;
        };
        let read = read_register(bus);
        i2c_line(I2C_ADDR, I2C_REG, read).log();
        if let Ok(b) = read {
            publish(I2C_KEY, &[b]);
        }
    }

    fn round(&mut self) {
        self.start_spi();
        self.read_i2c();
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
        self.finish_spi(verdict(&PATTERN, done, copied, &rx));
    }
}

impl Payload for Demo {
    fn init() -> Self {
        let spi = open_spi().map_err(|e| log_err("spi_open", e)).ok();
        let i2c = I2c::open(I2C_BUS, I2C_HZ)
            .map_err(|e| log_err("i2c_open", e))
            .ok();
        let timer = Timer::start_periodic(TIMER_ID, PERIOD_US)
            .map_err(|e| log_err("timer_start", e))
            .ok();
        mw_payload::log("periph_demo: started (jumper MOSI to MISO)");
        let mut demo = Demo {
            spi,
            i2c,
            _timer: timer,
            busy_periods: 0,
        };
        demo.round();
        demo
    }

    fn on_irq(&mut self, event: Event) {
        match event {
            Event::Timer { id: TIMER_ID, .. } => self.round(),
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
mw_payload::payload!(Demo);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_accepts_only_a_full_matching_echo() {
        let tx = PATTERN;
        assert_eq!(verdict(&tx, Ok(LEN), Ok(LEN), &tx), SpiOutcome::Ok);
        let mut rx = tx;
        rx[3] = 0x7F;
        assert_eq!(
            verdict(&tx, Ok(LEN), Ok(LEN), &rx),
            SpiOutcome::Mismatch {
                at: 3,
                sent: 0xFF,
                got: 0x7F
            }
        );
    }

    #[test]
    fn verdict_reports_failures_then_short_counts() {
        let tx = PATTERN;
        let zero = [0u8; LEN];
        assert_eq!(
            verdict(&tx, Err(AbiError::Transport), Ok(LEN), &tx),
            SpiOutcome::Failed(AbiError::Transport)
        );
        assert_eq!(
            verdict(&tx, Ok(LEN), Err(AbiError::Busy), &tx),
            SpiOutcome::Failed(AbiError::Busy)
        );
        assert_eq!(
            verdict(&tx, Ok(LEN - 1), Ok(LEN), &tx),
            SpiOutcome::Short(LEN - 1),
            "short transfer"
        );
        assert_eq!(
            verdict(&tx, Ok(LEN), Ok(3), &zero),
            SpiOutcome::Short(3),
            "short read beats the mismatch"
        );
    }

    #[test]
    fn spi_lines() {
        assert_eq!(
            spi_line(SpiOutcome::Ok).as_str(),
            "periph_demo: spi loopback ok (8 bytes)"
        );
        assert_eq!(
            spi_line(SpiOutcome::Failed(AbiError::Busy)).as_str(),
            "periph_demo: spi loopback error 7"
        );
        assert_eq!(
            spi_line(SpiOutcome::Short(3)).as_str(),
            "periph_demo: spi loopback short (3 of 8 bytes)"
        );
        assert_eq!(
            spi_line(SpiOutcome::Mismatch {
                at: 2,
                sent: 0x00,
                got: 0xFF
            })
            .as_str(),
            "periph_demo: spi loopback mismatch at 2: sent 0x00 got 0xff"
        );
        assert_eq!(spi_status(SpiOutcome::Ok), b"ok");
        assert_eq!(spi_status(SpiOutcome::Short(0)), b"err");
    }

    #[test]
    fn i2c_lines() {
        assert_eq!(
            i2c_line(0x50, 0x00, Ok(0x5A)).as_str(),
            "periph_demo: i2c 0x50 reg 0x00 = 0x5a"
        );
        assert_eq!(
            i2c_line(0x50, 0xAB, Err(AbiError::Transport)).as_str(),
            "periph_demo: i2c 0x50 reg 0xab error 5"
        );
    }

    #[test]
    fn hex_digits() {
        let digits: [u8; 16] = core::array::from_fn(|i| hex_digit(i as u8));
        assert_eq!(&digits, b"0123456789abcdef");
        assert_eq!(hex_digit(0xF3), b'3', "low nibble only");
    }

    #[test]
    fn pattern_is_eight_distinct_bytes() {
        for (i, a) in PATTERN.iter().enumerate() {
            assert!(!PATTERN[i + 1..].contains(a), "{a:#04x} repeats");
        }
    }
}
