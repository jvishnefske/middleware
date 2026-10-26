//! Blink: toggles one GPIO every 500 ms and publishes the level.
//!
//! * `init` configures [`LED_PIN`] as an output and sets a [`PERIOD_MS`] tick.
//! * Each `tick` inverts the pin and publishes the new level on
//!   [`STATE_KEY`] as one ASCII byte, `b"1"` (high) or `b"0"` (low).
//!
//! If the firmware refuses the pin (reserved or missing on that chip) the
//! payload logs the error and keeps publishing the would-be level, so it
//! still runs in the simulator or on a board without that LED.
#![no_std]
#![forbid(unsafe_code)]

use mw_payload::{LogLine, Payload, gpio::Output};

/// GPIO 8: the on-board LED of common ESP32-C3 boards (e.g. "SuperMini",
/// active low) and a free general-purpose pin on ESP32-S3 DevKitC. Change it
/// for other boards.
pub const LED_PIN: u8 = 8;

/// Toggle period.
pub const PERIOD_MS: u32 = 500;

/// Key the level is published on.
pub const STATE_KEY: &str = "app/blink/state";

/// Application state.
pub struct Blink {
    led: Option<Output>,
    high: bool,
}

/// Logs `"<what>: <error>"`.
fn log_err(what: &str, err: mw_payload::AbiError) {
    let mut line = LogLine::new();
    line.push_str(what)
        .push_str(": error ")
        .push_u32(err.code().unsigned_abs());
    line.log();
}

impl Payload for Blink {
    fn init() -> Self {
        let led = Output::new(LED_PIN)
            .map_err(|e| log_err("gpio_output", e))
            .ok();
        if let Err(e) = mw_payload::set_tick(PERIOD_MS) {
            log_err("set_tick", e);
        }
        mw_payload::log("blink: started");
        Blink { led, high: false }
    }

    fn tick(&mut self, _now_ms: u32) {
        self.high = !self.high;
        if let Some(led) = self.led.as_mut()
            && let Err(e) = led.set(self.high)
        {
            log_err("gpio_write", e);
        }
        let level: &[u8] = if self.high { b"1" } else { b"0" };
        if let Err(e) = mw_payload::publish(STATE_KEY, level) {
            log_err("publish", e);
        }
    }
}

mw_payload::payload!(Blink);
