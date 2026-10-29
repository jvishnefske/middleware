//! I2C for payloads on the ESP32 (ABI 3 `i2c_open` / `i2c_transfer`):
//! controller I2C0 as bus 0.
//!
//! The board pin map (`esp_base::chip`, ESP32-CAM) wires bus 0 to SDA
//! GPIO26 and SCL GPIO27, the camera sensor's SCCB. The esp-hal driver is
//! built at boot without pins (blocking: transfers are synchronous and at
//! most `MAX_I2C` bytes each way, so a call blocks for a few milliseconds
//! at most); `i2c_open` applies the clock and the device routes the
//! `I2CEXT0_SDA` / `I2CEXT0_SCL` signals to the mapped pins through the pin
//! bank (open drain, pull-up). esp-hal refuses clocks above 400 kHz on
//! this chip with a config error, reported as `Unsupported`.
//!
//! A transfer writes `tx` then, after a repeated start, reads `rx`; either
//! may be empty, and both empty probe the address. Every bus error (no
//! acknowledge, arbitration, timeout) is `Transport`, as ABI 3 asks for a
//! missing acknowledge.

use esp_hal::Blocking;
use esp_hal::gpio::{InputSignal, OutputSignal};
use esp_hal::i2c::master::{Config, I2c};
use esp_hal::time::Rate;
use wasm_host::{AbiError, I2cConfig};

/// SDA's input and output signals.
pub const SDA: (InputSignal, OutputSignal) = (InputSignal::I2CEXT0_SDA, OutputSignal::I2CEXT0_SDA);
/// SCL's input and output signals.
pub const SCL: (InputSignal, OutputSignal) = (InputSignal::I2CEXT0_SCL, OutputSignal::I2CEXT0_SCL);

/// Most bus clock this driver accepts.
const MAX_HZ: u32 = 400_000;

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

    /// Applies `cfg`'s clock; `Unsupported` for a bus other than 0 or a
    /// clock above 400 kHz.
    pub fn open(&mut self, cfg: &I2cConfig) -> Result<(), AbiError> {
        if cfg.bus != 0 || cfg.freq_hz > MAX_HZ {
            return Err(AbiError::Unsupported);
        }
        self.i2c
            .apply_config(&Config::default().with_frequency(Rate::from_hz(cfg.freq_hz)))
            .map_err(|_| AbiError::Unsupported)?;
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
