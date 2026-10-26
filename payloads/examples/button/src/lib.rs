//! Button: a GPIO edge interrupt toggles an LED and counts presses.
//!
//! * `init` configures [`BUTTON_PIN`] as an input with pull-up, enables a
//!   falling-edge interrupt on it (pressed = pulled to ground) and configures
//!   [`LED_PIN`] as an output.
//! * Each [`Event::Gpio`] for the button arrives in `on_irq` from the
//!   firmware main loop, timestamped in the ISR. Presses closer than
//!   [`DEBOUNCE_US`] to the last accepted one are contact bounce and are
//!   ignored; an accepted press toggles the LED and publishes the running
//!   count on [`COUNT_KEY`] as ASCII decimal.
//! * If the firmware reports dropped events ([`irq::overflow`]), the count
//!   is logged so lost presses are visible.
//!
//! Pins default to an ESP32-S3-DevKitC-1: GPIO 0 is the on-board BOOT button
//! (active low, with its own pull-up; a strapping pin, so do not hold it
//! during reset), and GPIO 8 is a free header pin for an external LED with
//! a series resistor. Missing pins are logged and the payload keeps running.
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use mw_payload::gpio::{Edge, Input, Output};
use mw_payload::irq::{self, Event};
use mw_payload::{AbiError, LogLine, Payload, Pull};

/// Button input: the BOOT button of ESP32-S3 dev boards.
pub const BUTTON_PIN: u8 = 0;

/// LED output: a free pin on ESP32-S3 dev boards (on-board LED on C3 boards).
pub const LED_PIN: u8 = 8;

/// Edges within this time of the last accepted press are bounce.
pub const DEBOUNCE_US: u32 = 20_000;

/// Key the press count is published on.
pub const COUNT_KEY: &str = "app/button/count";

/// Time of the last accepted press; a pure debouncer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Debounce {
    last_us: Option<u32>,
}

impl Debounce {
    /// Classifies a press at `time_us` (wrapping microseconds). Returns the
    /// next state and whether the press counts.
    pub const fn press(self, time_us: u32) -> (Self, bool) {
        match self.last_us {
            Some(last) if time_us.wrapping_sub(last) < DEBOUNCE_US => (self, false),
            _ => (
                Debounce {
                    last_us: Some(time_us),
                },
                true,
            ),
        }
    }
}

/// Application state.
pub struct Button {
    button: Option<Input>,
    led: Option<Output>,
    debounce: Debounce,
    count: u32,
}

/// Logs `"button: <what> error <code>"`.
fn log_err(what: &str, err: AbiError) {
    let mut line = LogLine::new();
    line.push_str("button: ")
        .push_str(what)
        .push_str(" error ")
        .push_u32(err.code().unsigned_abs());
    line.log();
}

/// Configures the button with a falling-edge interrupt.
fn button() -> Result<Input, AbiError> {
    let mut input = Input::new(BUTTON_PIN, Pull::Up)?;
    input.enable_irq(Edge::Falling)?;
    Ok(input)
}

impl Button {
    fn pressed(&mut self) {
        self.count = self.count.wrapping_add(1);
        if let Some(led) = self.led.as_mut()
            && let Err(e) = led.toggle()
        {
            log_err("gpio_write", e);
        }
        let mut text = LogLine::new();
        text.push_u32(self.count);
        if let Err(e) = mw_payload::publish(COUNT_KEY, text.as_str().as_bytes()) {
            log_err("publish", e);
        }
        if let Ok(dropped) = irq::overflow()
            && dropped > 0
        {
            let mut line = LogLine::new();
            line.push_str("button: ")
                .push_u32(dropped)
                .push_str(" events dropped");
            line.log();
        }
    }
}

impl Payload for Button {
    fn init() -> Self {
        let button = button().map_err(|e| log_err("gpio_irq", e)).ok();
        let led = Output::new(LED_PIN)
            .map_err(|e| log_err("gpio_output", e))
            .ok();
        mw_payload::log("button: started");
        Button {
            button,
            led,
            debounce: Debounce::default(),
            count: 0,
        }
    }

    fn on_irq(&mut self, event: Event) {
        let Some(false) = self.button.as_ref().and_then(|b| b.edge_level(&event)) else {
            return;
        };
        let (next, counts) = self.debounce.press(event.time_us());
        self.debounce = next;
        if counts {
            self.pressed();
        }
    }
}

// The exports need a single-threaded wasm32 module; host builds only run
// the unit tests below.
#[cfg(target_arch = "wasm32")]
mw_payload::payload!(Button);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_press_counts_and_bounce_is_ignored() {
        let d = Debounce::default();
        let (d, a) = d.press(1_000);
        let (d, b) = d.press(1_000 + DEBOUNCE_US - 1);
        let (_, c) = d.press(1_000 + DEBOUNCE_US);
        assert_eq!((a, b, c), (true, false, true));
    }

    #[test]
    fn debounce_survives_timer_wrap() {
        let (d, _) = Debounce::default().press(u32::MAX - 5);
        assert!(!d.press(10).1, "16 us after the wrap is bounce");
        assert!(d.press(DEBOUNCE_US).1);
    }
}
