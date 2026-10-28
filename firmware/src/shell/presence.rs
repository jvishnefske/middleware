//! Physical presence: the BOOT button task and the shared re-key window.
//!
//! [`watch`] samples the BOOT button every
//! [`POLL_MS`](esp_base::presence::POLL_MS) and runs the pure
//! [`Hold`] detector. When the button has been held for
//! [`PRESENCE_HOLD_MS`](esp_base::presence::PRESENCE_HOLD_MS) (pressed
//! after boot), it opens the re-key [`Window`] for
//! [`REKEY_WINDOW_MS`](middleware::loader::trust::REKEY_WINDOW_MS) and logs
//! it on `sys/log`; when the window times out it closes it and logs that.
//! The controller reads the window per request ([`window_open`]) and closes
//! it after a successful claim ([`close`]).
//!
//! The button is GPIO0 on ESP32 and ESP32-S3 and GPIO9 on ESP32-C3. With
//! `wasm` the pin stays in the payload pin bank (payloads use it as a
//! button) and is sampled through [`PINS`](super::gpio::PINS), which
//! refuses to report it while a payload drives it or pulls it down. On
//! ESP32-C3 (no runtime) the shell owns it as a pulled-up input.

use core::cell::Cell;

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::{Duration, Instant, Ticker};
use esp_base::presence::{Hold, POLL_MS};
use middleware::loader::trust::Window;

use super::bus::Bus;

/// The re-key window, shared by [`watch`] and the controller.
static WINDOW: Mutex<CriticalSectionRawMutex, Cell<Window>> = Mutex::new(Cell::new(Window::Closed));

/// Milliseconds since boot.
pub fn now_ms() -> u64 {
    Instant::now().as_millis()
}

/// Whether the re-key window is open now.
pub fn window_open() -> bool {
    let now = now_ms();
    WINDOW.lock(|w| w.get().is_open(now))
}

/// Closes the re-key window (after a successful claim).
pub fn close() {
    WINDOW.lock(|w| w.set(Window::Closed));
}

/// The BOOT button.
pub struct BootButton {
    #[cfg(feature = "wasm")]
    pin: u8,
    #[cfg(not(feature = "wasm"))]
    pin: esp_hal::gpio::Input<'static>,
}

impl BootButton {
    /// GPIO `pin`, read through the payload pin bank.
    #[cfg(feature = "wasm")]
    pub const fn in_bank(pin: u8) -> Self {
        BootButton { pin }
    }

    /// An input owned by the shell.
    #[cfg(not(feature = "wasm"))]
    pub fn owned(pin: esp_hal::gpio::Input<'static>) -> Self {
        BootButton { pin }
    }

    /// `Some(pressed)`, or `None` when the pin cannot be read honestly.
    fn sample(&mut self) -> Option<bool> {
        #[cfg(feature = "wasm")]
        return super::gpio::PINS.boot_button(self.pin);
        #[cfg(not(feature = "wasm"))]
        Some(self.pin.is_low())
    }
}

/// Watches the BOOT button forever (see the module docs).
#[embassy_executor::task]
pub async fn watch(mut boot: BootButton, bus: &'static Bus) {
    let mut hold = Hold::default();
    let mut ticker = Ticker::every(Duration::from_millis(POLL_MS));
    loop {
        ticker.next().await;
        let now = now_ms();
        let (next, fired) = hold.step(boot.sample(), now);
        hold = next;
        if fired {
            WINDOW.lock(|w| w.set(Window::opened_at(now)));
            bus.log(format_args!(
                "trust: BOOT held 3 s, re-key window open for 60 s (send sys/trust/claim)"
            ));
        }
        let expired = WINDOW.lock(|w| {
            let was = w.get();
            let is = was.expire(now);
            w.set(is);
            was != is
        });
        if expired {
            bus.log(format_args!("trust: re-key window closed (timed out)"));
        }
    }
}
