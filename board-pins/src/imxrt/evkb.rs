//! The MIMXRT1050-EVKB's logical board pins and the pads they map to.
//!
//! The payload ABI names pins with numbers below 64, but the RT1052 has
//! five GPIO banks of 32 lines. So on this board a payload pin is a
//! *logical board pin*: the Arduino header's D0–D15 and A0–A5, the USER
//! button, and the LPUART1 console pins (listed so that the board can say
//! why they are reserved). [`pin`] maps each to its [`BoardPin`]: the pad,
//! its GPIO line, the peripheral functions the firmware routes to it and
//! its ADC1 input. [`crate::boards::RT1050_EVKB`] is the policy over these
//! numbers (reserved, input only, ADC, I2C, SPI).
//!
//! | pin | name | header  | pad             | GPIO       | functions (ALT, daisy)                          | board wiring and payload access |
//! |-----|------|---------|-----------------|------------|--------------------------------------------------|---------------------------------|
//! | 0   | D0   | J22\[1\]  | `GPIO_AD_B1_07` | GPIO1_IO23 | —                                                | Arduino UART_RX (LPUART3 RXD, not used), CSI_HSYNC; GPIO |
//! | 1   | D1   | J22\[2\]  | `GPIO_AD_B1_06` | GPIO1_IO22 | —                                                | Arduino UART_TX (LPUART3 TXD, not used), CSI_VSYNC; GPIO |
//! | 2   | D2   | J22\[3\]  | `GPIO_AD_B0_11` | GPIO1_IO11 | —                                                | FXOS8700CQ INT2 (U32 pin 9), JTAG_nTRST; input only |
//! | 3   | D3   | J22\[4\]  | `GPIO_AD_B1_08` | GPIO1_IO24 | —                                                | AUD_INT (audio jack detect), CSI_D9; input only |
//! | 4   | D4   | J22\[5\]  | `GPIO_AD_B0_09` | GPIO1_IO09 | —                                                | Ethernet PHY reset (KSZ8081RNB), also the USER LED D18 (on when low), JTAG_TDI; reserved |
//! | 5   | D5   | J22\[6\]  | `GPIO_AD_B0_10` | GPIO1_IO10 | —                                                | Ethernet PHY interrupt, also FXOS8700CQ INT1 (U32 pin 11), JTAG_TDO; reserved |
//! | 6   | D6   | J22\[7\]  | `GPIO_AD_B1_02` | GPIO1_IO18 | —                                                | SPDIF_OUT (not fitted); GPIO |
//! | 7   | D7   | J22\[8\]  | `GPIO_AD_B1_03` | GPIO1_IO19 | —                                                | SPDIF_IN (not fitted); GPIO |
//! | 8   | D8   | J24\[1\]  | `GPIO_AD_B0_03` | GPIO1_IO03 | —                                                | USB_OTG1_OC; GPIO |
//! | 9   | D9   | J24\[2\]  | `GPIO_AD_B0_02` | GPIO1_IO02 | —                                                | USB_OTG1_PWR; GPIO |
//! | 10  | D10  | J24\[3\]  | `GPIO_SD_B0_01` | GPIO3_IO13 | LPSPI1_PCS0 (ALT4, 0x401F_84EC = 0)              | SPI CS, microSD SD1_CLK; GPIO (CS is driven as a GPIO) |
//! | 11  | D11  | J24\[4\]  | `GPIO_SD_B0_02` | GPIO3_IO14 | LPSPI1_SDO (ALT4, 0x401F_84F8 = 1)               | SPI MOSI, microSD SD1_D0; GPIO, SPI bus 0 MOSI |
//! | 12  | D12  | J24\[5\]  | `GPIO_SD_B0_03` | GPIO3_IO15 | LPSPI1_SDI (ALT4, 0x401F_84F4 = 1)               | SPI MISO, microSD SD1_D1; GPIO, SPI bus 0 MISO |
//! | 13  | D13  | J24\[6\]  | `GPIO_SD_B0_00` | GPIO3_IO12 | LPSPI1_SCK (ALT4, 0x401F_84F0 = 1)               | SPI SCK, microSD SD1_CMD; GPIO, SPI bus 0 SCK |
//! | 14  | D14  | J24\[9\]  | `GPIO_AD_B0_01` | GPIO1_IO01 | —                                                | USB_OTG1_ID; GPIO (not the Arduino I2C, see below) |
//! | 15  | D15  | J24\[10\] | `GPIO_AD_B0_00` | GPIO1_IO00 | —                                                | USB_HOST_OC; GPIO (not the Arduino I2C) |
//! | 16  | A0   | J23\[1\]  | `GPIO_AD_B1_10` | GPIO1_IO26 | ADC1_IN15                                        | SAI1_RX_SYNC, CSI_D7; GPIO, ADC channel 0 |
//! | 17  | A1   | J23\[2\]  | `GPIO_AD_B1_11` | GPIO1_IO27 | ADC1_IN0                                         | SAI1_RX_BCLK, CSI_D6; GPIO, ADC channel 1 |
//! | 18  | A2   | J23\[3\]  | `GPIO_AD_B1_04` | GPIO1_IO20 | ADC1_IN9                                         | CSI_PIXCLK; GPIO, ADC channel 2 |
//! | 19  | A3   | J23\[4\]  | `GPIO_AD_B1_05` | GPIO1_IO21 | ADC1_IN10                                        | CSI_MCLK; GPIO, ADC channel 3 |
//! | 20  | A4   | J23\[5\]  | `GPIO_AD_B1_01` | GPIO1_IO17 | LPI2C1_SDA (ALT3, SION, 0x401F_84D0 = 1), ADC1_IN6 | I2C1 SDA: FXOS8700CQ, WM8960, camera; I2C bus 0 SDA, ADC channel 4 |
//! | 21  | A5   | J23\[6\]  | `GPIO_AD_B1_00` | GPIO1_IO16 | LPI2C1_SCL (ALT3, SION, 0x401F_84CC = 1), ADC1_IN5 | I2C1 SCL: as A4; I2C bus 0 SCL, ADC channel 5 |
//! | 22  | SW8  | —       | `WAKEUP`        | GPIO5_IO00 | —                                                | USER button SW8; input only |
//! | 23  | TXD  | —       | `GPIO_AD_B0_12` | GPIO1_IO12 | LPUART1_TXD (ALT2)                               | LPUART1, the OpenSDA VCOM (console and protocol link); reserved |
//! | 24  | RXD  | —       | `GPIO_AD_B0_13` | GPIO1_IO13 | LPUART1_RXD (ALT2)                               | as TXD; reserved |
//!
//! Pin numbers are chosen so that D`n` is pin `n`, A`n` is pin `16 + n`
//! and ADC channel `n` is A`n`.
//!
//! # Sources
//!
//! NXP MCUXpresso SDK (`nxp-mcuxpresso/mcux-sdk`, `mcux-sdk-examples`):
//!
//! * header positions and board wiring: the pin labels of
//!   `boards/evkbimxrt1050/project_template/pin_mux.c` (e.g.
//!   `GPIO_AD_B1_07` "CSI_HSYNC/J35\[16\]/J22\[1\]/UART_RX") for every
//!   Arduino pin, matched against the Arduino table (Table 6) of the
//!   MIMXRT1050-EVKB Hardware User's Guide (J22 D0–D7, J23 A0–A5,
//!   J24 D8–D15);
//! * USER LED on GPIO1_IO09 and USER button SW8 on GPIO5_IO00:
//!   `boards/evkbimxrt1050/board.h` (`BOARD_USER_LED_GPIO_PIN`,
//!   `BOARD_USER_BUTTON_*`), muxed in `demo_apps/led_blinky/pin_mux.c`
//!   (`IOMUXC_GPIO_AD_B0_09_GPIO1_IO09`) and
//!   `driver_examples/gpio/input_interrupt/pin_mux.c`
//!   (`IOMUXC_SNVS_WAKEUP_GPIO5_IO00`);
//! * LPI2C1 on `GPIO_AD_B1_00`/`01` (ALT3, SION) and the FXOS8700CQ on
//!   that bus: `driver_examples/lpi2c/interrupt_b2b_transfer/master/pin_mux.c`
//!   and `demo_apps/bubble_peripheral/pin_mux.c`
//!   (`IOMUXC_GPIO_AD_B1_00_LPI2C1_SCL`, `..._01_LPI2C1_SDA`, SION 1);
//!   `board.h` `BOARD_ACCEL_I2C_BASEADDR LPI2C1`;
//! * LPSPI1 on `GPIO_SD_B0_00`–`03` (ALT4):
//!   `driver_examples/lpspi/polling_b2b_transfer/master/pin_mux.c`;
//! * ADC1_IN0 on A1 (`GPIO_AD_B1_11`): `driver_examples/adc/polling/pin_mux.c`;
//!   ADC1_IN15 on A0 (`GPIO_AD_B1_10`):
//!   `driver_examples/adc_etc/adc_etc_software_trigger_conv/pin_mux.c`;
//! * LPUART1 on `GPIO_AD_B0_12`/`13` (ALT2): every example's `pin_mux.c`;
//! * ALT modes, daisy registers and values: the `IOMUXC_*` macros of
//!   `devices/MIMXRT1052/drivers/fsl_iomuxc.h`;
//! * the reserved pads ([`RESERVED_PADS`]): the board `pin_mux.c` labels
//!   (`SEMC_*`, `FlexSPI_*`, `BT_CFG[*]`, `BOOT_MODE[*]`, `SWD_*`, `ENET_MD*`);
//!   the SAI1 pads: `driver_examples/sai/interrupt_record_playback/pin_mux.c`;
//!   the Ethernet pads: `driver_examples/enet/txrx_transfer/pin_mux.c`.
//!
//! # Not confirmed from the SDK board files
//!
//! * ADC1 inputs of A2–A5 (`GPIO_AD_B1_04` IN9, `_05` IN10, `_01` IN6,
//!   `_00` IN5): no evkbimxrt1050 SDK file muxes them to ADC1. They follow
//!   the RT1050 pad table's rule (`GPIO_AD_B1_n` is ADC1_IN`5 + n` for
//!   n ≤ 10, `GPIO_AD_B1_11` is IN0, which the SDK confirms at both ends:
//!   IN0 and IN15), and IN5, IN6 and IN9 appear on these pads in MCUXpresso
//!   Config Tools output of other RT10xx projects; IN10 on `GPIO_AD_B1_05`
//!   is the rule only.
//! * D2 as input only: the label puts the FXOS8700CQ's INT2 (a push-pull
//!   output by default) on this pad; driving it would fight the sensor.
//! * D3 as input only: the label "AUD_INT" names an interrupt from the
//!   audio circuit (the headphone jack detect, Hardware User's Guide 2.10),
//!   whose driver was not checked against the schematic; held as input
//!   only to be safe.
//! * The FXOS8700CQ's I2C address 0x1F: the bubble demo probes
//!   0x1C–0x1F; 0x1F is the board's strapping (WHO_AM_I register 0x0D
//!   reads 0xC7, `components/fxos8700cq/fsl_fxos.h`). Checked on
//!   hardware only.
//!
//! # Shared lines
//!
//! D10–D13 are also the microSD slot's (J20) SD1 lines: with a card
//! fitted, SPI on them talks to the card too. A4/A5 carry
//! I2C bus 0 (LPI2C1), on which the board itself has the FXOS8700CQ
//! accelerometer (0x1F) and the WM8960 audio codec (0x1A), and the bus
//! pull-ups, so an ADC read there sees the bus; a payload must not use
//! those two addresses for devices of its own.
//! D14/D15 sit where Arduino shields expect I2C, but on this board they
//! are LPI2C1's target-mode pins, not its controller pins (Hardware User's
//! Guide, note under Table 6: "use or wire to J23 PIN5&PIN6 instead"), so
//! I2C bus 0 is on A4/A5.
//!
//! # Ethernet
//!
//! The firmware brings up Ethernet (the KSZ8081RNB PHY, RMII), so D4
//! (`GPIO_AD_B0_09`, the PHY reset, which also drives the USER LED D18:
//! driving it low resets the PHY) and D5 (`GPIO_AD_B0_10`, the PHY
//! interrupt) are reserved, and the USER LED is not a payload pin. Source:
//! `driver_examples/enet/txrx_transfer/pin_mux.c` (GPIO1_IO09, GPIO1_IO10,
//! the RMII pads `GPIO_B1_04`–`11` and MDC / MDIO on `GPIO_EMC_40`/`41`).
//! USB OTG1 uses dedicated pins, no GPIO pad.
//!
//! # Reserved pads
//!
//! No logical pin maps to a pad of [`RESERVED_PADS`]: the SDRAM (SEMC),
//! the HyperFlash (FlexSPI) and its reset, the boot mode and boot
//! configuration pads, the SWD debug pads, the Ethernet RMII and
//! management pads, and the SAI1 pads to the WM8960 audio codec (`GPIO_AD_B1_09`
//! SAI1_MCLK, `_12` RX_DATA00, `_13` TX_DATA00, `_14` TX_BCLK, `_15`
//! TX_SYNC; the firmware itself will expose the microphone over them),
//! none of which is an Arduino header pin. A0 and A1 are labelled
//! SAI1_RX_SYNC / SAI1_RX_BCLK as well, but the SDK's record and playback
//! example clocks the receiver from the TX clocks and never muxes them, so
//! they stay payload pins. The LPUART1 pads have logical numbers (23, 24) only so that the
//! board can say why a payload may not use them; they are reserved in
//! [`crate::boards::RT1050_EVKB`].

use super::{Adc1Channel, Alt, Daisy, Pad, PadFunction, PadGroup, Signal, adc1, pad};

/// One logical board pin (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoardPin {
    /// The board's name for it, e.g. `D13`, `A4`, `SW8`.
    pub label: &'static str,
    /// The pad.
    pub pad: Pad,
    /// The peripheral functions the firmware routes to the pad (beside
    /// GPIO, which is [`Pad::GPIO_ALT`] on every pad).
    pub functions: &'static [PadFunction],
    /// The ADC1 input the pad carries, when the board offers it.
    pub adc1: Option<Adc1Channel>,
}

impl BoardPin {
    /// The pad function carrying `signal`, if the board routes it here.
    pub fn function(&self, signal: Signal) -> Option<PadFunction> {
        self.functions.iter().find(|f| f.signal == signal).copied()
    }
}

/// Logical pins (`0..PIN_COUNT`).
pub const PIN_COUNT: u8 = 25;

const fn gpio(label: &'static str, pad: Pad) -> BoardPin {
    BoardPin {
        label,
        pad,
        functions: &[],
        adc1: None,
    }
}

const fn func(signal: Signal, alt: Alt, daisy: Option<(u32, u8)>, sion: bool) -> PadFunction {
    let daisy = match daisy {
        Some((register, value)) => Some(Daisy { register, value }),
        None => None,
    };
    PadFunction {
        signal,
        alt,
        daisy,
        sion,
    }
}

use PadGroup::{AdB0, AdB1, SdB0};

/// The board pins, indexed by logical pin number (see the module docs).
pub const PINS: [BoardPin; PIN_COUNT as usize] = [
    gpio("D0", pad(AdB1, 7)),
    gpio("D1", pad(AdB1, 6)),
    gpio("D2", pad(AdB0, 11)),
    gpio("D3", pad(AdB1, 8)),
    gpio("D4", pad(AdB0, 9)),
    gpio("D5", pad(AdB0, 10)),
    gpio("D6", pad(AdB1, 2)),
    gpio("D7", pad(AdB1, 3)),
    gpio("D8", pad(AdB0, 3)),
    gpio("D9", pad(AdB0, 2)),
    BoardPin {
        label: "D10",
        pad: pad(SdB0, 1),
        functions: &[func(
            Signal::Lpspi1Pcs0,
            Alt::Alt4,
            Some((0x401F_84EC, 0)),
            false,
        )],
        adc1: None,
    },
    BoardPin {
        label: "D11",
        pad: pad(SdB0, 2),
        functions: &[func(
            Signal::Lpspi1Sdo,
            Alt::Alt4,
            Some((0x401F_84F8, 1)),
            false,
        )],
        adc1: None,
    },
    BoardPin {
        label: "D12",
        pad: pad(SdB0, 3),
        functions: &[func(
            Signal::Lpspi1Sdi,
            Alt::Alt4,
            Some((0x401F_84F4, 1)),
            false,
        )],
        adc1: None,
    },
    BoardPin {
        label: "D13",
        pad: pad(SdB0, 0),
        functions: &[func(
            Signal::Lpspi1Sck,
            Alt::Alt4,
            Some((0x401F_84F0, 1)),
            false,
        )],
        adc1: None,
    },
    gpio("D14", pad(AdB0, 1)),
    gpio("D15", pad(AdB0, 0)),
    BoardPin {
        label: "A0",
        pad: pad(AdB1, 10),
        functions: &[],
        adc1: adc1(15),
    },
    BoardPin {
        label: "A1",
        pad: pad(AdB1, 11),
        functions: &[],
        adc1: adc1(0),
    },
    BoardPin {
        label: "A2",
        pad: pad(AdB1, 4),
        functions: &[],
        adc1: adc1(9),
    },
    BoardPin {
        label: "A3",
        pad: pad(AdB1, 5),
        functions: &[],
        adc1: adc1(10),
    },
    BoardPin {
        label: "A4",
        pad: pad(AdB1, 1),
        functions: &[func(
            Signal::Lpi2c1Sda,
            Alt::Alt3,
            Some((0x401F_84D0, 1)),
            true,
        )],
        adc1: adc1(6),
    },
    BoardPin {
        label: "A5",
        pad: pad(AdB1, 0),
        functions: &[func(
            Signal::Lpi2c1Scl,
            Alt::Alt3,
            Some((0x401F_84CC, 1)),
            true,
        )],
        adc1: adc1(5),
    },
    gpio("SW8", Pad::Wakeup),
    BoardPin {
        label: "TXD",
        pad: pad(AdB0, 12),
        functions: &[func(Signal::Lpuart1Txd, Alt::Alt2, None, false)],
        adc1: None,
    },
    BoardPin {
        label: "RXD",
        pad: pad(AdB0, 13),
        functions: &[func(Signal::Lpuart1Rxd, Alt::Alt2, None, false)],
        adc1: None,
    },
];

/// The board pin with logical number `pin`.
pub fn pin(pin: u8) -> Option<&'static BoardPin> {
    PINS.get(usize::from(pin))
}

/// The logical pin of the USER LED (D18): D4, reserved, since the pad is
/// also the Ethernet PHY's reset (see the module docs).
pub const USER_LED: u8 = 4;

/// The logical pin of the USER button (SW8, low while pressed).
pub const USER_BUTTON: u8 = 22;

/// The WM8960 audio codec's 7-bit I2C address on I2C bus 0.
pub const WM8960_ADDRESS: u8 = 0x1A;

/// The FXOS8700CQ's 7-bit I2C address on I2C bus 0.
pub const FXOS8700_ADDRESS: u8 = 0x1F;

/// The FXOS8700CQ's WHO_AM_I register.
pub const FXOS8700_WHO_AM_I_REG: u8 = 0x0D;

/// What the FXOS8700CQ's WHO_AM_I register reads.
pub const FXOS8700_WHO_AM_I: u8 = 0xC7;

/// Pads `first..=last` of one group, held by the board.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PadRange {
    /// The group.
    pub group: PadGroup,
    /// The first pad.
    pub first: u8,
    /// The last pad.
    pub last: u8,
    /// What the board uses them for.
    pub why: &'static str,
}

impl PadRange {
    /// Whether `pad` is in the range.
    pub const fn contains(&self, pad: Pad) -> bool {
        match pad {
            Pad::Iomuxc(p) => {
                p.group() as u8 == self.group as u8
                    && p.index() >= self.first
                    && p.index() <= self.last
            }
            Pad::Wakeup => false,
        }
    }
}

/// The pads no logical pin maps to, and why (see the module docs).
pub const RESERVED_PADS: &[PadRange] = &[
    PadRange {
        group: PadGroup::Emc,
        first: 0,
        last: 39,
        why: "the SDRAM (SEMC)",
    },
    PadRange {
        group: PadGroup::Emc,
        first: 40,
        last: 41,
        why: "the Ethernet PHY's MDC / MDIO",
    },
    PadRange {
        group: PadGroup::B1,
        first: 4,
        last: 11,
        why: "the Ethernet RMII (RX_DATA0/1, RX_EN, TX_DATA0/1, TX_EN, REF_CLK, RX_ER)",
    },
    PadRange {
        group: PadGroup::SdB1,
        first: 0,
        last: 11,
        why: "the HyperFlash (FlexSPI)",
    },
    PadRange {
        group: PadGroup::AdB0,
        first: 4,
        last: 5,
        why: "BOOT_MODE[1:0] (AD_B0_05 is also the HyperFlash reset)",
    },
    PadRange {
        group: PadGroup::AdB0,
        first: 6,
        last: 7,
        why: "SWD (the OpenSDA debugger)",
    },
    PadRange {
        group: PadGroup::AdB0,
        first: 12,
        last: 13,
        why: "LPUART1, the console and protocol link",
    },
    PadRange {
        group: PadGroup::AdB1,
        first: 9,
        last: 9,
        why: "SAI1 to the WM8960 audio codec (SAI1_MCLK)",
    },
    PadRange {
        group: PadGroup::AdB1,
        first: 12,
        last: 15,
        why: "SAI1 to the WM8960 audio codec (RX_DATA00, TX_DATA00, TX_BCLK, TX_SYNC)",
    },
    PadRange {
        group: PadGroup::B0,
        first: 4,
        last: 15,
        why: "the boot configuration BT_CFG[11:0]",
    },
];

/// Why the board holds `pad`, if it does.
pub fn reserved_pad(pad: Pad) -> Option<&'static str> {
    RESERVED_PADS
        .iter()
        .find(|r| r.contains(pad))
        .map(|r| r.why)
}

#[cfg(test)]
mod tests {
    use super::super::tests::Name;
    use super::super::{GpioBank, GpioLine};
    use super::*;

    /// (logical pin, label, pad name, GPIO line name) as the module table
    /// and its SDK sources give them.
    const TABLE: [(u8, &str, &str, &str); 25] = [
        (0, "D0", "GPIO_AD_B1_07", "GPIO1_IO23"),
        (1, "D1", "GPIO_AD_B1_06", "GPIO1_IO22"),
        (2, "D2", "GPIO_AD_B0_11", "GPIO1_IO11"),
        (3, "D3", "GPIO_AD_B1_08", "GPIO1_IO24"),
        (4, "D4", "GPIO_AD_B0_09", "GPIO1_IO09"),
        (5, "D5", "GPIO_AD_B0_10", "GPIO1_IO10"),
        (6, "D6", "GPIO_AD_B1_02", "GPIO1_IO18"),
        (7, "D7", "GPIO_AD_B1_03", "GPIO1_IO19"),
        (8, "D8", "GPIO_AD_B0_03", "GPIO1_IO03"),
        (9, "D9", "GPIO_AD_B0_02", "GPIO1_IO02"),
        (10, "D10", "GPIO_SD_B0_01", "GPIO3_IO13"),
        (11, "D11", "GPIO_SD_B0_02", "GPIO3_IO14"),
        (12, "D12", "GPIO_SD_B0_03", "GPIO3_IO15"),
        (13, "D13", "GPIO_SD_B0_00", "GPIO3_IO12"),
        (14, "D14", "GPIO_AD_B0_01", "GPIO1_IO01"),
        (15, "D15", "GPIO_AD_B0_00", "GPIO1_IO00"),
        (16, "A0", "GPIO_AD_B1_10", "GPIO1_IO26"),
        (17, "A1", "GPIO_AD_B1_11", "GPIO1_IO27"),
        (18, "A2", "GPIO_AD_B1_04", "GPIO1_IO20"),
        (19, "A3", "GPIO_AD_B1_05", "GPIO1_IO21"),
        (20, "A4", "GPIO_AD_B1_01", "GPIO1_IO17"),
        (21, "A5", "GPIO_AD_B1_00", "GPIO1_IO16"),
        (22, "SW8", "WAKEUP", "GPIO5_IO00"),
        (23, "TXD", "GPIO_AD_B0_12", "GPIO1_IO12"),
        (24, "RXD", "GPIO_AD_B0_13", "GPIO1_IO13"),
    ];

    #[test]
    fn rt10_arduino_pins_map_to_the_sdk_pads() {
        for (n, label, pad_name, line) in TABLE {
            let p = pin(n).unwrap();
            assert_eq!(p.label, label, "pin {n}");
            assert_eq!(Name::of(&p.pad).as_str(), pad_name, "pin {n}");
            assert_eq!(Name::of(&p.pad.gpio()).as_str(), line, "pin {n}");
        }
        assert_eq!(pin(PIN_COUNT), None);
        assert_eq!(pin(255), None);
        // board.h: USER LED GPIO1 pin 9, USER button GPIO5 pin 0.
        assert_eq!(
            pin(USER_LED).unwrap().pad.gpio(),
            GpioLine::new(GpioBank::Gpio1, 9).unwrap()
        );
        assert_eq!(
            pin(USER_BUTTON).unwrap().pad.gpio(),
            GpioLine::new(GpioBank::Gpio5, 0).unwrap()
        );
        // No two logical pins share a pad.
        for (i, a) in PINS.iter().enumerate() {
            assert!(PINS[i + 1..].iter().all(|b| b.pad != a.pad), "{}", a.label);
        }
    }

    #[test]
    fn rt10_peripheral_functions_match_the_sdk_iomuxc_macros() {
        // (pin, signal, ALT, daisy register, daisy value, SION): the SDK's
        // `IOMUXC_<pad>_<signal>` macros and the examples' SION argument.
        let sdk = [
            (10, Signal::Lpspi1Pcs0, 4, Some((0x401F_84EC, 0)), false),
            (11, Signal::Lpspi1Sdo, 4, Some((0x401F_84F8, 1)), false),
            (12, Signal::Lpspi1Sdi, 4, Some((0x401F_84F4, 1)), false),
            (13, Signal::Lpspi1Sck, 4, Some((0x401F_84F0, 1)), false),
            (20, Signal::Lpi2c1Sda, 3, Some((0x401F_84D0, 1)), true),
            (21, Signal::Lpi2c1Scl, 3, Some((0x401F_84CC, 1)), true),
            (23, Signal::Lpuart1Txd, 2, None, false),
            (24, Signal::Lpuart1Rxd, 2, None, false),
        ];
        for (n, signal, alt, daisy, sion) in sdk {
            let f = pin(n).unwrap().function(signal).unwrap();
            assert_eq!(f.alt.mode(), alt, "pin {n}");
            assert_eq!(f.daisy.map(|d| (d.register, d.value)), daisy, "pin {n}");
            assert_eq!(f.sion, sion, "pin {n}");
        }
        assert_eq!(pin(13).unwrap().function(Signal::Lpi2c1Scl), None);
        assert_eq!(pin(0).unwrap().functions, &[]);
    }

    #[test]
    fn rt10_analog_inputs_are_adc1() {
        let channels = [(16, 15), (17, 0), (18, 9), (19, 10), (20, 6), (21, 5)];
        for (n, ch) in channels {
            assert_eq!(
                pin(n).unwrap().adc1.map(Adc1Channel::number),
                Some(ch),
                "pin {n}"
            );
        }
        assert!(PINS.iter().filter(|p| p.adc1.is_some()).count() == channels.len());
    }

    #[test]
    fn rt10_no_logical_pin_reaches_a_board_pad() {
        for p in &PINS[..23] {
            assert_eq!(reserved_pad(p.pad), None, "{}", p.label);
        }
        // The console pins are numbered only to be reported as reserved.
        assert!(reserved_pad(pin(23).unwrap().pad).is_some());
        assert!(reserved_pad(pin(24).unwrap().pad).is_some());
        assert_eq!(
            reserved_pad(super::super::pad(PadGroup::SdB1, 7)),
            Some("the HyperFlash (FlexSPI)")
        );
        assert_eq!(
            reserved_pad(super::super::pad(PadGroup::Emc, 26)),
            Some("the SDRAM (SEMC)")
        );
        assert_eq!(reserved_pad(Pad::Wakeup), None);
        assert!(
            reserved_pad(super::super::pad(PadGroup::B1, 10))
                .unwrap()
                .contains("RMII")
        );
        assert_eq!(reserved_pad(super::super::pad(PadGroup::B1, 12)), None);
        // SAI1 to the WM8960 (the SDK's record and playback example).
        for i in [9, 12, 13, 14, 15] {
            let why = reserved_pad(super::super::pad(PadGroup::AdB1, i)).unwrap();
            assert!(why.starts_with("SAI1 to the WM8960"), "GPIO_AD_B1_{i}");
        }
    }
}
