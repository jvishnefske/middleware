//! I2C for payloads on the ESP32 and ESP32-S3 (ABI 3 `i2c_open` /
//! `i2c_transfer`): controller I2C0 as bus 0.
//!
//! The board pin map (`esp_base::chip`) wires bus 0 to SDA GPIO26 / SCL
//! GPIO27 on the ESP32 (the ESP32-CAM's camera sensor SCCB) and to SDA
//! GPIO5 / SCL GPIO6 on the ESP32-S3. Both chips have the same
//! `I2CEXT0_SDA` / `I2CEXT0_SCL` matrix signals, so this module is the same
//! on both. The esp-hal driver is built at boot without pins, in blocking
//! mode; `i2c_open` applies the clock and the per-byte timeout, and the
//! device routes the signals to the mapped pins through the pin bank (open
//! drain, pull-up). Buses other than 0 and clocks above 400 kHz are
//! `Unsupported` (`esp_base::periph::i2c::open_plan`; esp-hal itself
//! refuses more than 400 kHz on the ESP32, and the ESP32-S3 keeps the same
//! cap).
//!
//! A transfer writes `tx` then, after a repeated start, reads `rx`; either
//! may be empty, and both empty probe the address. Each side is at most
//! `MAX_I2C` (64) bytes. Every bus error (no acknowledge, arbitration,
//! timeout) is `Transport`, as ABI 3 asks for a missing acknowledge.
//!
//! # What bounds a transfer
//!
//! The transfer runs synchronously on the controller's task, so a stuck bus
//! must not hang it. esp-hal's defaults do not bound that on the ESP32-S3
//! (hardware bus timeout disabled, state-machine timeouts at 2^23 bus
//! cycles), so `open` sets esp-hal's software timeout to
//! `SoftwareTimeout::PerByte(esp_base::periph::i2c::byte_timeout_us(freq))`
//! on both chips: two byte times plus 100 µs per byte, applied per FIFO
//! chunk. A transfer then returns within `(tx + rx + 6)` byte budgets (about
//! 37 ms at 100 kHz, 19 ms at 400 kHz, 255 ms at 10 kHz for 64 bytes each
//! way), plus at most 50 ms of esp-hal's bus clear (nine SCL pulses and a
//! stop) after a timeout. On the ESP32 the hardware SCL timeout (esp-hal's
//! default maximum, about 13 ms of an unchanging SCL) also stays armed.

use esp_hal::Blocking;
use esp_hal::gpio::{InputSignal, OutputSignal};
use esp_base::periph::i2c::open_plan;
use esp_hal::i2c::master::{Config, I2c, SoftwareTimeout};
use esp_hal::time::{Duration, Rate};
use wasm_host::{AbiError, I2cConfig};

/// SDA's input and output signals.
pub const SDA: (InputSignal, OutputSignal) = (InputSignal::I2CEXT0_SDA, OutputSignal::I2CEXT0_SDA);
/// SCL's input and output signals.
pub const SCL: (InputSignal, OutputSignal) = (InputSignal::I2CEXT0_SCL, OutputSignal::I2CEXT0_SCL);

/// I2C bus 0: the driver and whether a payload opened it.
pub struct Bus0 {
    i2c: I2c<'static, Blocking>,
    open: bool,
}

impl Bus0 {
    /// Takes the driver built at boot.
    pub fn new(i2c: I2c<'static, Blocking>) -> Self {
        Bus0 { i2c, open: false }
    }

    /// Applies `cfg`'s clock and the per-byte timeout (see the module
    /// docs); `Unsupported` for a bus other than 0, a clock above 400 kHz,
    /// or a configuration esp-hal refuses.
    pub fn open(&mut self, cfg: &I2cConfig) -> Result<(), AbiError> {
        let byte_us = open_plan(cfg.bus, cfg.freq_hz)?;
        let config = Config::default()
            .with_frequency(Rate::from_hz(cfg.freq_hz))
            .with_software_timeout(SoftwareTimeout::PerByte(Duration::from_micros(u64::from(byte_us))));
        self.i2c.apply_config(&config).map_err(|_| AbiError::Unsupported)?;
        self.open = true;
        Ok(())
    }

    /// One transfer (see the module docs); `Busy` before `open`.
    pub fn transfer(&mut self, addr: u8, tx: &[u8], rx: &mut [u8]) -> Result<(), AbiError> {
        if !self.open {
            return Err(AbiError::Busy);
        }
        let result = match (tx.is_empty(), rx.is_empty()) {
            (_, true) => self.i2c.write(addr, tx),
            (true, false) => self.i2c.read(addr, rx),
            (false, false) => self.i2c.write_read(addr, tx, rx),
        };
        result.map_err(|_| AbiError::Transport)
    }

    /// Forgets the payload's configuration (payload stop).
    pub fn close(mut self) -> Self {
        self.open = false;
        self
    }
}
