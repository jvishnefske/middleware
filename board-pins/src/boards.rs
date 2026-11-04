//! The boards' pin maps.
//!
//! | map        | board                     | reserved (why)                                        | input only | ADC (channel order)    | I2C bus 0 / 1 (SDA, SCL) | PWM (t,c) = 2·t+c           | SPI     |
//! |------------|---------------------------|-------------------------------------------------------|------------|------------------------|--------------------------|-----------------------------|---------|
//! | [`PICO`]   | Raspberry Pi Pico, Pico 2 | 23 power save, 26–28 ADC, 29 VSYS / 3                  | 24 (VBUS)  | 26 27 28               | 4, 5 / 6, 7              | —                           | F1 table |
//! | [`ESP32`]  | AI-Thinker ESP32-CAM      | 1, 3 UART0 console, 6–11 flash, 16, 17 PSRAM           | 34–39      | ADC1: 36–39, 32–35     | 26, 27 (SCCB) / —        | 0, 4, 13, 14 (timers 0–1)   | matrix  |
//! | [`ESP32S3`]| ESP32-S3 boards           | 19, 20 USB, 26–37 flash / PSRAM, 43, 44 UART0 console  | —          | ADC1: 1–10             | 5, 6 / —                 | 11–18 (timers 0–3)          | matrix  |
//! | [`ESP32C3`]| ESP32-C3 boards           | 12–17 flash, 18, 19 USB, 20, 21 UART0 console          | —          | —                      | — / —                    | —                           | matrix  |
//! | [`ESP8266`]| ESP8266 modules           | 1, 3 UART0 console, 6–11 flash                         | —          | —                      | — / —                    | —                           | matrix  |
//!
//! The RP2040 and the RP2350A share the Pico pinout, so both chips use
//! [`PICO`]. The ESP32 build is the ESP32-CAM's: its I2C bus 0 is the
//! camera's SCCB and its PWM outputs include the camera clock (GPIO 0) and
//! the flash LED (GPIO 4). The ESP32-S3's bus 0 pins are free headers on
//! the boards that build runs on (not strapping, flash, PSRAM, USB or
//! console pins). On the ESP chips the ADC inputs are ADC1's (ADC2 shares
//! its controller with Wi-Fi and is never offered) and the PWM table is the
//! ESP base firmware's (`esp_base::chip`, X9).
//! The ESP32-C3 and ESP8266 builds run no payloads, so they wire no
//! peripherals; their maps still say which pins exist and are reserved.

use crate::{PinMap, Reason, SpiRouting, bit, span};

const PICO_REASONS: &[Reason] = &[
    Reason {
        pins: bit(23),
        why: "the regulator's power-save control",
    },
    Reason {
        pins: bit(24),
        why: "VBUS sense (input only)",
    },
    Reason {
        pins: span(26, 28),
        why: "the ADC (analog input only)",
    },
    Reason {
        pins: bit(29),
        why: "VSYS / 3 sense",
    },
];

/// Raspberry Pi Pico (RP2040) and Pico 2 (RP2350A): GPIO 0–29. GPIO 25 is
/// the on-board LED, an ordinary payload pin.
pub const PICO: PinMap = PinMap {
    gpio: span(0, 29),
    reserved: bit(23) | span(26, 28) | bit(29),
    input_only: bit(24),
    adc: &[26, 27, 28],
    i2c: [Some((4, 5)), Some((6, 7))],
    pwm: &[],
    spi: SpiRouting::RpFunctionTable,
    reasons: PICO_REASONS,
};

/// AI-Thinker ESP32-CAM: GPIO 0–19, 21–23, 25–27, 32–39.
pub const ESP32: PinMap = PinMap {
    gpio: span(0, 19) | span(21, 23) | span(25, 27) | span(32, 39),
    reserved: bit(1) | bit(3) | span(6, 11) | bit(16) | bit(17),
    input_only: span(34, 39),
    adc: &[36, 37, 38, 39, 32, 33, 34, 35],
    i2c: [Some((26, 27)), None],
    pwm: &[0, 4, 13, 14],
    spi: SpiRouting::Matrix,
    reasons: &[
        Reason {
            pins: bit(1) | bit(3),
            why: "the UART0 console (the protocol port)",
        },
        Reason {
            pins: span(6, 11),
            why: "the SPI flash",
        },
        Reason {
            pins: bit(16) | bit(17),
            why: "the PSRAM",
        },
        Reason {
            pins: span(34, 39),
            why: "input only (no output driver)",
        },
    ],
};

/// ESP32-S3: GPIO 0–21, 26–48.
pub const ESP32S3: PinMap = PinMap {
    gpio: span(0, 21) | span(26, 48),
    reserved: bit(19) | bit(20) | span(26, 37) | bit(43) | bit(44),
    input_only: 0,
    adc: &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
    i2c: [Some((5, 6)), None],
    pwm: &[11, 12, 13, 14, 15, 16, 17, 18],
    spi: SpiRouting::Matrix,
    reasons: &[
        Reason {
            pins: bit(19) | bit(20),
            why: "USB D- / D+",
        },
        Reason {
            pins: span(26, 37),
            why: "the flash and PSRAM",
        },
        Reason {
            pins: bit(43) | bit(44),
            why: "the UART0 console",
        },
    ],
};

/// ESP32-C3: GPIO 0–21.
pub const ESP32C3: PinMap = PinMap {
    gpio: span(0, 21),
    reserved: span(12, 17) | bit(18) | bit(19) | bit(20) | bit(21),
    input_only: 0,
    adc: &[],
    i2c: [None, None],
    pwm: &[],
    spi: SpiRouting::Matrix,
    reasons: &[
        Reason {
            pins: span(12, 17),
            why: "the SPI flash",
        },
        Reason {
            pins: bit(18) | bit(19),
            why: "USB D- / D+",
        },
        Reason {
            pins: bit(20) | bit(21),
            why: "the UART0 console",
        },
    ],
};

/// ESP8266: GPIO 0–16.
pub const ESP8266: PinMap = PinMap {
    gpio: span(0, 16),
    reserved: span(6, 11) | bit(1) | bit(3),
    input_only: 0,
    adc: &[],
    i2c: [None, None],
    pwm: &[],
    spi: SpiRouting::Matrix,
    reasons: &[
        Reason {
            pins: bit(1) | bit(3),
            why: "the UART0 console (the protocol port)",
        },
        Reason {
            pins: span(6, 11),
            why: "the SPI flash",
        },
    ],
};

/// The map for a chip name as `sys/info` reports it (`esp32`, `esp32s3`,
/// `esp32c3`, `esp8266`, `rp2040`, `rp2350`).
pub fn by_chip(chip: &str) -> Option<&'static PinMap> {
    match chip {
        "esp32" => Some(&ESP32),
        "esp32s3" => Some(&ESP32S3),
        "esp32c3" => Some(&ESP32C3),
        "esp8266" => Some(&ESP8266),
        "rp2040" | "rp2350" => Some(&PICO),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chip_names_select_maps() {
        assert_eq!(by_chip("rp2040"), Some(&PICO));
        assert_eq!(by_chip("rp2350"), Some(&PICO));
        assert_eq!(by_chip("esp32"), Some(&ESP32));
        assert_eq!(by_chip("esp32s3"), Some(&ESP32S3));
        assert_eq!(by_chip("esp32c3"), Some(&ESP32C3));
        assert_eq!(by_chip("esp8266"), Some(&ESP8266));
        assert_eq!(by_chip("badge"), None);
    }

    #[test]
    fn payload_pin_counts() {
        assert_eq!(PICO.payload_pins().count(), 25);
        assert_eq!(PICO.gpios().count(), 30);
        assert_eq!(ESP32.gpios().count(), 34);
        assert_eq!(ESP32.payload_pins().count(), 34 - 10);
        assert_eq!(ESP32S3.payload_pins().count(), 22 + 23 - 16);
    }
}
