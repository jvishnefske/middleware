//! Analog inputs for payloads: ADC1 one-shot conversions (X12).
//!
//! | chip     | pins (ADC1 channel)                      |
//! |----------|------------------------------------------|
//! | ESP32    | GPIO36–39 (CH0–3), GPIO32–35 (CH4–7)     |
//! | ESP32-S3 | GPIO1–10 (CH0–9)                         |
//!
//! ADC2 is never used: on both chips it shares its controller with Wi-Fi.
//! Every channel runs at 11 dB attenuation (about 0–2.5 V on ESP32, 0–3.1 V
//! on ESP32-S3) and returns the raw, uncalibrated 12-bit reading, as the
//! ABI's `adc_read` specifies.
//!
//! esp-hal fixes attenuations when the driver is built, and panics on a
//! read of a channel it was not told about, so the board builds [`AdcUnit`]
//! once at boot with every ADC1 pin enabled (`board::adc1!`), before the
//! pin bank takes the pins (building a `Flex` puts a pin back on the
//! digital mux, undoing the analog setup).
//!
//! A read needs esp-hal's `AdcPin`, which owns the GPIO. The pin bank keeps
//! that GPIO as a `Flex`, so [`AdcUnit::read`] conjures a second handle with
//! `GPIOn::steal()` for the duration of the read (the only `unsafe` in the
//! shell). This is sound because the bank slot is in `Mode::Analog` and the
//! claims table reserves the pin for the ADC, so nothing else drives the
//! `Flex` while the stolen handle exists. The handle comes back as an
//! `AnyPin` that the bank keeps to rebuild the slot's `Flex` on release,
//! which clears the analog (RTC) mux.
//!
//! A conversion takes tens of microseconds; one that has not finished after
//! [`TIMEOUT`] is `Transport`. esp-hal then still counts that conversion as
//! running, so later reads of other channels also time out until the same
//! channel is read again.

use esp_hal::Blocking;
use esp_hal::analog::adc::{Adc, AdcChannel, AdcConfig, Attenuation};
use esp_hal::gpio::{AnalogPin, AnyPin, Pin};
use esp_hal::peripherals::ADC1;
use esp_hal::time::{Duration, Instant};
use middleware::wasm_abi::AbiError;

/// Longest wait for one conversion.
pub const TIMEOUT: Duration = Duration::from_millis(1);

/// Every channel's attenuation.
pub const ATTENUATION: Attenuation = Attenuation::_11dB;

/// The ADC1 driver.
pub type Adc1 = Adc<'static, ADC1<'static>, Blocking>;

/// ADC1 with every payload channel enabled.
pub struct AdcUnit {
    adc: Adc1,
}

/// What a read returns: the reading, and the pin's handle whenever the
/// pin was switched to the analog mux (also on a timeout).
pub type Reading = (Result<u16, AbiError>, Option<AnyPin<'static>>);

/// Converts on `pin`.
fn sample<P>(adc: &mut Adc1, pin: P) -> Reading
where
    P: AdcChannel + AnalogPin + Pin + 'static,
{
    let mut config = AdcConfig::<ADC1<'static>>::new();
    let mut pin = config.enable_pin(pin, ATTENUATION);
    let start = Instant::now();
    let raw = loop {
        match adc.read_oneshot(&mut pin) {
            Ok(raw) => break Ok(raw),
            Err(nb::Error::WouldBlock) if start.elapsed() < TIMEOUT => {}
            Err(_) => break Err(AbiError::Transport),
        }
    };
    (raw, Some(pin.pin.degrade()))
}

/// Dispatches `n` to [`sample`] with a stolen `GPIOn` (see the module
/// docs for why this is sound).
macro_rules! read_pin {
    ($adc:expr, $n:expr; $($pin:literal => $gpio:ident),* $(,)?) => {
        match $n {
            // SAFETY: the bank slot of this pin is in `Mode::Analog` and the
            // claims table holds it for the ADC; the stolen handle is the
            // only one in use until it is handed back.
            $( $pin => sample($adc, unsafe { esp_hal::peripherals::$gpio::steal() }), )*
            _ => (Err(AbiError::Unsupported), None),
        }
    };
}

impl AdcUnit {
    /// Wraps the boot-time driver (see `board::adc1!`).
    pub fn new(adc: Adc1) -> Self {
        AdcUnit { adc }
    }

    /// One raw 12-bit conversion on ADC1 pin `n`, with the pin's handle;
    /// `Unsupported` for a pin without an ADC1 channel, `Transport` after
    /// [`TIMEOUT`].
    #[allow(unsafe_code)]
    pub fn read(&mut self, n: u8) -> Reading {
        let adc = &mut self.adc;
        #[cfg(feature = "esp32")]
        let r = read_pin!(adc, n;
            32 => GPIO32, 33 => GPIO33, 34 => GPIO34, 35 => GPIO35,
            36 => GPIO36, 37 => GPIO37, 38 => GPIO38, 39 => GPIO39,
        );
        #[cfg(feature = "esp32s3")]
        let r = read_pin!(adc, n;
            1 => GPIO1, 2 => GPIO2, 3 => GPIO3, 4 => GPIO4, 5 => GPIO5,
            6 => GPIO6, 7 => GPIO7, 8 => GPIO8, 9 => GPIO9, 10 => GPIO10,
        );
        r
    }
}
