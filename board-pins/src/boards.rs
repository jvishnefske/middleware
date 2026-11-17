//! The boards' pin maps.
//!
//! | map        | board                     | reserved (why)                                        | input only | ADC (channel order)    | I2C bus 0 / 1 (SDA, SCL) | PWM (t,c) = 2·t+c           | SPI     |
//! |------------|---------------------------|-------------------------------------------------------|------------|------------------------|--------------------------|-----------------------------|---------|
//! | [`PICO`]   | Raspberry Pi Pico, Pico 2 | 23 power save, 26–28 ADC, 29 VSYS / 3                  | 24 (VBUS)  | 26 27 28               | 4, 5 / 6, 7              | —                           | F1 table |
//! | [`ESP32`]  | AI-Thinker ESP32-CAM      | 1, 3 UART0 console, 6–11 flash, 16, 17 PSRAM           | 34–39      | ADC1: 36–39, 32–35     | 26, 27 (SCCB) / —        | 0, 4, 13, 14 (timers 0–1)   | matrix  |
//! | [`ESP32S3`]| ESP32-S3 boards           | 19, 20 USB, 26–37 flash / PSRAM, 43, 44 UART0 console  | —          | ADC1: 1–10             | 5, 6 / —                 | 11–18 (timers 0–3)          | matrix  |
//! | [`ESP32C3`]| ESP32-C3 boards           | 12–17 flash, 18, 19 USB, 20, 21 UART0 console          | —          | —                      | — / —                    | —                           | matrix  |
//! | [`ESP8266`]| ESP8266 modules           | 1, 3 UART0 console, 6–11 flash                         | —          | —                      | — / —                    | —                           | matrix  |
//! | [`COLORLIGHT_5A75B`] | Colorlight 5A-75B v7.0 (ECP5) | —                                    | 1 (button) | —                      | — / —                    | —                           | none    |
//! | [`RT1050_EVKB`] | NXP MIMXRT1050-EVKB  | 23, 24 LPUART1 console                                 | 2, 3, 5, 22 | ADC1: 16–21 (A0–A5)   | 20, 21 (LPI2C1) / —      | —                           | fixed LPSPI1 (13, 11, 12) |
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
//!
//! On the RT1050-EVKB the pin numbers are logical board pins, not GPIO
//! lines: D`n` is pin `n`, A`n` is pin `16 + n`, the USER button SW8 is 22
//! and the LPUART1 console pins are 23 and 24 ([`crate::imxrt::evkb`] maps
//! each to its pad, with the table and its sources).

use crate::{FixedSpi, PinMap, Reason, SpiRouting, bit, span};

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

/// Colorlight 5A-75B v7.0 (Lattice ECP5 LFE5U-25F, gateware, no CPU):
/// the payload-visible pins are virtual GPIO numbers the gateware maps to
/// FPGA balls. GPIO 0 is the user LED (ball P11, active low; the gateware
/// inverts it, so `gpio_write(0, 1)` lights it), GPIO 1 the user button
/// (ball M13, input only, active low with the board's pull-up; reads 1
/// when pressed). Everything else on the board (the two RGMII PHYs, the
/// SDRAM, the SPI flash, the HUB75 connectors) belongs to the gateware.
/// The chip profile is in `gateware-5a75b/` and `wasm-hdl-ref`: 16 KiB of
/// linear memory, 16 KiB images.
pub const COLORLIGHT_5A75B: PinMap = PinMap {
    gpio: span(0, 1),
    reserved: 0,
    input_only: bit(1),
    adc: &[],
    i2c: [None, None],
    pwm: &[],
    spi: SpiRouting::None,
    reasons: &[Reason {
        pins: bit(1),
        why: "the user button (input only)",
    }],
};

/// NXP MIMXRT1050-EVKB (i.MX RT1052): logical board pins 0–24
/// ([`crate::imxrt::evkb`] has the pin → pad table and its SDK sources).
///
/// | pins  | what                    | payload access |
/// |-------|-------------------------|----------------|
/// | 0–15  | Arduino D0–D15          | GPIO; D2, D3 input only (driven by the FXOS8700CQ INT2 and the audio jack detect); D4, D5 reserved (the Ethernet PHY's reset, which also drives the USER LED D18, and its interrupt) |
/// | 16–21 | Arduino A0–A5           | GPIO and ADC1 (ADC channel `n` is A`n`) |
/// | 20, 21| A4, A5                  | I2C bus 0 SDA, SCL (LPI2C1; the board's FXOS8700CQ at 0x1F and WM8960 codec at 0x1A share it) |
/// | 13, 11, 12 | D13, D11, D12      | SPI bus 0 SCK, MOSI, MISO (LPSPI1, fixed); CS any output pin |
/// | 22    | USER button SW8         | input only |
/// | 23, 24| LPUART1 TXD, RXD        | reserved (the console and protocol link) |
///
/// The firmware brings up Ethernet (KSZ8081RNB PHY), so its reset and
/// interrupt lines D4 and D5 are reserved; the USER LED, wired to the PHY
/// reset, is therefore not a payload pin.
///
/// The SDRAM, HyperFlash, boot, SWD and Ethernet (RMII, MDIO) pads and the
/// SAI1 pads to the WM8960 codec (none on the Arduino header) have no
/// logical number at all ([`crate::imxrt::evkb::RESERVED_PADS`]). No PWM
/// outputs are wired.
pub const RT1050_EVKB: PinMap = PinMap {
    gpio: span(0, 24),
    reserved: bit(4) | bit(5) | bit(23) | bit(24),
    input_only: bit(2) | bit(3) | bit(22),
    adc: &[16, 17, 18, 19, 20, 21],
    i2c: [Some((20, 21)), None],
    pwm: &[],
    spi: SpiRouting::Fixed(FixedSpi {
        sck: 13,
        mosi: 11,
        miso: 12,
    }),
    reasons: &[
        Reason {
            pins: bit(23) | bit(24),
            why: "LPUART1, the console and protocol link (OpenSDA VCOM)",
        },
        Reason {
            pins: bit(4),
            why: "the Ethernet PHY reset (also the USER LED D18; driving it low resets the PHY)",
        },
        Reason {
            pins: bit(5),
            why: "the Ethernet PHY interrupt",
        },
        Reason {
            pins: bit(2),
            why: "driven by the FXOS8700CQ's INT2 output (input only)",
        },
        Reason {
            pins: bit(3),
            why: "the audio jack detect, AUD_INT (input only)",
        },
        Reason {
            pins: bit(22),
            why: "the USER button SW8 (input only)",
        },
    ],
};

/// The map for a chip name as `sys/info` reports it (`esp32`, `esp32s3`,
/// `esp32c3`, `esp8266`, `rp2040`, `rp2350`, `ecp5`, `rt1050`).
pub fn by_chip(chip: &str) -> Option<&'static PinMap> {
    match chip {
        "esp32" => Some(&ESP32),
        "esp32s3" => Some(&ESP32S3),
        "esp32c3" => Some(&ESP32C3),
        "esp8266" => Some(&ESP8266),
        "rp2040" | "rp2350" => Some(&PICO),
        "ecp5" => Some(&COLORLIGHT_5A75B),
        "rt1050" => Some(&RT1050_EVKB),
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
        assert_eq!(by_chip("ecp5"), Some(&COLORLIGHT_5A75B));
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

    #[test]
    fn cl2_the_5a75b_offers_its_led_and_button_only() {
        use crate::Direction;
        let m = &COLORLIGHT_5A75B;
        assert_eq!(m.gpios().count(), 2);
        assert!(m.check(0, Direction::Output).is_ok());
        assert!(m.check(1, Direction::Input).is_ok());
        assert!(m.check(1, Direction::Output).is_err());
        assert!(m.check(2, Direction::Input).is_err());
        let pins = crate::spi::SpiPins {
            sck: 0,
            mosi: None,
            miso: None,
            cs: None,
        };
        assert!(crate::spi::route(m, &pins).is_err());
    }

    #[test]
    fn rt10_rt1050_evkb_map() {
        use crate::{AdcError, Direction, PinError};
        assert_eq!(by_chip("rt1050"), Some(&RT1050_EVKB));
        let m = &RT1050_EVKB;
        assert_eq!(
            m.gpios().count(),
            usize::from(crate::imxrt::evkb::PIN_COUNT)
        );
        assert_eq!(m.payload_pins().count(), 21);
        // Every logical pin has a pad; none past them exists.
        assert!(m.gpios().all(|p| crate::imxrt::evkb::pin(p).is_some()));
        assert_eq!(m.check(25, Direction::Input), Err(PinError::NoSuchPin));
        // Reserved and input-only pins, each with its reason.
        for p in [4, 5, 23, 24] {
            assert_eq!(m.check(p, Direction::Input), Err(PinError::Reserved));
        }
        assert_eq!(
            m.reason(4),
            Some("the Ethernet PHY reset (also the USER LED D18; driving it low resets the PHY)")
        );
        assert_eq!(m.reason(5), Some("the Ethernet PHY interrupt"));
        for p in [2, 3, 22] {
            assert_eq!(m.check(p, Direction::Input), Ok(()));
            assert_eq!(m.check(p, Direction::Output), Err(PinError::InputOnly));
        }
        for p in m.gpios() {
            let held = m.is_reserved(p) || m.is_input_only(p);
            assert_eq!(m.reason(p).is_some(), held, "pin {p}");
        }
        // The SPI and I2C pins are outputs.
        for p in [0, 1, 6, 10, 11, 12, 13, 14, 15, 16, 21] {
            assert_eq!(m.check(p, Direction::Output), Ok(()), "pin {p}");
        }
        // ADC channel n is An, on ADC1.
        for n in 0..6u8 {
            let p = 16 + n;
            assert_eq!(m.adc_channel(p), Ok(n));
            let pad = crate::imxrt::evkb::pin(p).unwrap();
            assert!(pad.adc1.is_some(), "A{n}");
        }
        assert_eq!(m.adc_channel(13), Err(AdcError::NotAnalog));
        assert_eq!(m.adc_channel(30), Err(AdcError::NoSuchPin));
        // I2C bus 0 is LPI2C1 on A4 (SDA) / A5 (SCL).
        use crate::imxrt::Signal;
        assert_eq!(m.i2c_pins(0), Some((20, 21)));
        assert_eq!(m.i2c_pins(1), None);
        let (sda, scl) = m.i2c_pins(0).unwrap();
        let pin = |p| crate::imxrt::evkb::pin(p).unwrap();
        assert!(pin(sda).function(Signal::Lpi2c1Sda).is_some());
        assert!(pin(scl).function(Signal::Lpi2c1Scl).is_some());
        assert_eq!(m.check(sda, Direction::Output), Ok(()));
        assert_eq!(m.check(scl, Direction::Output), Ok(()));
        // SPI bus 0 is LPSPI1 on D13 / D11 / D12.
        let SpiRouting::Fixed(spi) = m.spi_routing() else {
            panic!("fixed LPSPI1 routing");
        };
        assert!(pin(spi.sck).function(Signal::Lpspi1Sck).is_some());
        assert!(pin(spi.mosi).function(Signal::Lpspi1Sdo).is_some());
        assert!(pin(spi.miso).function(Signal::Lpspi1Sdi).is_some());
        assert_eq!(m.pwm_pins().count(), 0);
    }
}
