//! i.MX RT1050 (RT1052) pads: IOMUXC registers, GPIO lines, ALT functions
//! and ADC1 inputs, as pure `const` data.
//!
//! A [`Pad`] is one of the chip's GPIO pads: an IOMUXC pad
//! ([`IomuxcPad`], a [`PadGroup`] and an index, e.g. `GPIO_AD_B1_07`) or
//! the SNVS `WAKEUP` pad. Everything the firmware shell needs to drive a
//! pad follows from it by fixed rules of the RT1050 reference manual,
//! checked against every `IOMUXC_GPIO_*` macro of NXP's MCUXpresso SDK
//! `devices/MIMXRT1052/drivers/fsl_iomuxc.h` (all 124 IOMUXC GPIO pads
//! agree; the tests spot check the board's pads against those macros):
//!
//! | group  | pads   | IOMUXC index of pad 0 | GPIO line of pad `n`             |
//! |--------|--------|-----------------------|----------------------------------|
//! | EMC    | 00–41  | 0                     | GPIO4_IO`n` (n < 32), GPIO3_IO`n − 14` |
//! | AD_B0  | 00–15  | 42                    | GPIO1_IO`n`                      |
//! | AD_B1  | 00–15  | 58                    | GPIO1_IO`16 + n`                 |
//! | B0     | 00–15  | 74                    | GPIO2_IO`n`                      |
//! | B1     | 00–15  | 90                    | GPIO2_IO`16 + n`                 |
//! | SD_B0  | 00–05  | 106                   | GPIO3_IO`12 + n`                 |
//! | SD_B1  | 00–11  | 112                   | GPIO3_IO`n`                      |
//! | WAKEUP | —      | (IOMUXC_SNVS)         | GPIO5_IO00                       |
//!
//! The IOMUXC index `i` gives the mux register `0x401F_8014 + 4·i` and the
//! pad-control register `0x401F_8204 + 4·i`; `WAKEUP`'s are
//! `0x400A_8000` and `0x400A_8018` in IOMUXC_SNVS. On every pad the GPIO
//! function is ALT5.
//!
//! A peripheral function of a pad ([`PadFunction`]) is its ALT mode, the
//! input daisy chain (select-input register and value) where the signal
//! has more than one possible pad, and whether the input path is forced on
//! (SION, which the SDK sets for the LPI2C lines). Analog inputs are not
//! ALT functions: ADC1 samples its input pad whatever the mux, and the SDK
//! leaves such a pad in its GPIO mode with the input buffer and keeper
//! off; [`Adc1Channel`] is the ADC1 input a pad carries.
//!
//! The EVKB's logical board pins, which the payload ABI's pin numbers name,
//! are in [`evkb`].
//!
//! The constructors that take numbers return `Option` and refuse an
//! out-of-range value, so a [`Pad`], [`GpioLine`] or [`Adc1Channel`] always
//! names something that exists. The board tables are built in `const`
//! items through private helpers that turn a refusal into a compile error.

pub mod evkb;

use core::fmt;

/// A group of IOMUXC pads, named as in the reference manual
/// (`GPIO_<group>_<nn>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PadGroup {
    /// `GPIO_EMC_00`–`41` (the SEMC external memory pads).
    Emc,
    /// `GPIO_AD_B0_00`–`15`.
    AdB0,
    /// `GPIO_AD_B1_00`–`15`.
    AdB1,
    /// `GPIO_B0_00`–`15`.
    B0,
    /// `GPIO_B1_00`–`15`.
    B1,
    /// `GPIO_SD_B0_00`–`05`.
    SdB0,
    /// `GPIO_SD_B1_00`–`11`.
    SdB1,
}

impl PadGroup {
    /// Pads in the group.
    pub const fn pad_count(self) -> u8 {
        match self {
            PadGroup::Emc => 42,
            PadGroup::AdB0 | PadGroup::AdB1 | PadGroup::B0 | PadGroup::B1 => 16,
            PadGroup::SdB0 => 6,
            PadGroup::SdB1 => 12,
        }
    }

    /// IOMUXC index of the group's pad 0 (see the module docs).
    const fn first_index(self) -> u8 {
        match self {
            PadGroup::Emc => 0,
            PadGroup::AdB0 => 42,
            PadGroup::AdB1 => 58,
            PadGroup::B0 => 74,
            PadGroup::B1 => 90,
            PadGroup::SdB0 => 106,
            PadGroup::SdB1 => 112,
        }
    }

    /// The group's part of the pad name, e.g. `AD_B1`.
    pub const fn name(self) -> &'static str {
        match self {
            PadGroup::Emc => "EMC",
            PadGroup::AdB0 => "AD_B0",
            PadGroup::AdB1 => "AD_B1",
            PadGroup::B0 => "B0",
            PadGroup::B1 => "B1",
            PadGroup::SdB0 => "SD_B0",
            PadGroup::SdB1 => "SD_B1",
        }
    }
}

/// One IOMUXC pad: a group and an index inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IomuxcPad {
    group: PadGroup,
    index: u8,
}

impl IomuxcPad {
    /// Pad `index` of `group`, if the group has it.
    pub const fn new(group: PadGroup, index: u8) -> Option<Self> {
        if index < group.pad_count() {
            Some(IomuxcPad { group, index })
        } else {
            None
        }
    }

    /// The pad's group.
    pub const fn group(self) -> PadGroup {
        self.group
    }

    /// The pad's index in its group.
    pub const fn index(self) -> u8 {
        self.index
    }

    /// The pad's IOMUXC index (its mux register's, see the module docs).
    pub const fn iomuxc_index(self) -> u8 {
        self.group.first_index() + self.index
    }
}

/// A GPIO pad of the RT1050.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pad {
    /// An IOMUXC pad.
    Iomuxc(IomuxcPad),
    /// The SNVS `WAKEUP` pad (GPIO5_IO00).
    Wakeup,
}

/// The base of the IOMUXC mux registers (`SW_MUX_CTL_PAD_GPIO_EMC_00`).
const IOMUXC_MUX_BASE: u32 = 0x401F_8014;
/// The base of the IOMUXC pad-control registers (`SW_PAD_CTL_PAD_GPIO_EMC_00`).
const IOMUXC_PAD_BASE: u32 = 0x401F_8204;
/// IOMUXC_SNVS `SW_MUX_CTL_PAD_WAKEUP`.
const SNVS_WAKEUP_MUX: u32 = 0x400A_8000;
/// IOMUXC_SNVS `SW_PAD_CTL_PAD_WAKEUP`.
const SNVS_WAKEUP_PAD: u32 = 0x400A_8018;

impl Pad {
    /// The GPIO line the pad carries in [`Pad::GPIO_ALT`].
    pub const fn gpio(self) -> GpioLine {
        let (bank, bit) = match self {
            Pad::Wakeup => (GpioBank::Gpio5, 0),
            Pad::Iomuxc(IomuxcPad { group, index: n }) => match group {
                PadGroup::Emc if n < 32 => (GpioBank::Gpio4, n),
                PadGroup::Emc => (GpioBank::Gpio3, n - 14),
                PadGroup::AdB0 => (GpioBank::Gpio1, n),
                PadGroup::AdB1 => (GpioBank::Gpio1, 16 + n),
                PadGroup::B0 => (GpioBank::Gpio2, n),
                PadGroup::B1 => (GpioBank::Gpio2, 16 + n),
                PadGroup::SdB0 => (GpioBank::Gpio3, 12 + n),
                PadGroup::SdB1 => (GpioBank::Gpio3, n),
            },
        };
        // Every group's index bound keeps `bit` below 32.
        GpioLine { bank, bit }
    }

    /// The ALT mode of the GPIO function, on every pad.
    pub const GPIO_ALT: Alt = Alt::Alt5;

    /// The address of the pad's mux register (`SW_MUX_CTL_PAD_*`).
    pub const fn mux_register(self) -> u32 {
        match self {
            Pad::Wakeup => SNVS_WAKEUP_MUX,
            Pad::Iomuxc(p) => IOMUXC_MUX_BASE + 4 * p.iomuxc_index() as u32,
        }
    }

    /// The address of the pad's pad-control register (`SW_PAD_CTL_PAD_*`).
    pub const fn config_register(self) -> u32 {
        match self {
            Pad::Wakeup => SNVS_WAKEUP_PAD,
            Pad::Iomuxc(p) => IOMUXC_PAD_BASE + 4 * p.iomuxc_index() as u32,
        }
    }

    /// The pad's IOMUXC index, or `None` for `WAKEUP` (an IOMUXC_SNVS pad).
    pub const fn iomuxc_index(self) -> Option<u8> {
        match self {
            Pad::Wakeup => None,
            Pad::Iomuxc(p) => Some(p.iomuxc_index()),
        }
    }
}

impl fmt::Display for Pad {
    /// The reference manual's pad name, e.g. `GPIO_AD_B1_07` or `WAKEUP`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Pad::Wakeup => f.write_str("WAKEUP"),
            Pad::Iomuxc(p) => write!(f, "GPIO_{}_{:02}", p.group.name(), p.index),
        }
    }
}

/// A GPIO controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpioBank {
    /// GPIO1 (the AD_B0 and AD_B1 pads).
    Gpio1,
    /// GPIO2 (the B0 and B1 pads).
    Gpio2,
    /// GPIO3 (the SD_B0, SD_B1 and EMC_32–41 pads).
    Gpio3,
    /// GPIO4 (the EMC_00–31 pads).
    Gpio4,
    /// GPIO5 (the SNVS pads).
    Gpio5,
}

impl GpioBank {
    /// The bank's number (1–5).
    pub const fn number(self) -> u8 {
        match self {
            GpioBank::Gpio1 => 1,
            GpioBank::Gpio2 => 2,
            GpioBank::Gpio3 => 3,
            GpioBank::Gpio4 => 4,
            GpioBank::Gpio5 => 5,
        }
    }
}

/// One GPIO line: a bank and a bit (0–31) of its registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpioLine {
    bank: GpioBank,
    bit: u8,
}

impl GpioLine {
    /// Bit `bit` of `bank`, if below 32.
    pub const fn new(bank: GpioBank, bit: u8) -> Option<Self> {
        if bit < 32 {
            Some(GpioLine { bank, bit })
        } else {
            None
        }
    }

    /// The bank.
    pub const fn bank(self) -> GpioBank {
        self.bank
    }

    /// The bit in the bank's registers (0–31).
    pub const fn bit(self) -> u8 {
        self.bit
    }

    /// The line's bit in the bank's 32-bit registers.
    pub const fn mask(self) -> u32 {
        1u32 << self.bit
    }
}

impl fmt::Display for GpioLine {
    /// The reference manual's name, e.g. `GPIO1_IO23`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GPIO{}_IO{:02}", self.bank.number(), self.bit)
    }
}

/// An IOMUXC mux mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alt {
    /// ALT0.
    Alt0,
    /// ALT1.
    Alt1,
    /// ALT2.
    Alt2,
    /// ALT3.
    Alt3,
    /// ALT4.
    Alt4,
    /// ALT5 (GPIO on every pad).
    Alt5,
    /// ALT6.
    Alt6,
    /// ALT7.
    Alt7,
}

impl Alt {
    /// The `MUX_MODE` field value (0–7).
    pub const fn mode(self) -> u8 {
        self as u8
    }
}

/// A peripheral signal the board routes to a pad.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// LPI2C1 clock.
    Lpi2c1Scl,
    /// LPI2C1 data.
    Lpi2c1Sda,
    /// LPSPI1 clock.
    Lpspi1Sck,
    /// LPSPI1 data out (controller mode: MOSI).
    Lpspi1Sdo,
    /// LPSPI1 data in (controller mode: MISO).
    Lpspi1Sdi,
    /// LPSPI1 chip select 0.
    Lpspi1Pcs0,
    /// LPUART1 transmit.
    Lpuart1Txd,
    /// LPUART1 receive.
    Lpuart1Rxd,
}

/// An input daisy chain setting: the select-input register and its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Daisy {
    /// The address of the `*_SELECT_INPUT` register.
    pub register: u32,
    /// The value that selects this pad.
    pub value: u8,
}

/// A peripheral function of a pad.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PadFunction {
    /// The signal.
    pub signal: Signal,
    /// The mux mode that selects it.
    pub alt: Alt,
    /// The input daisy chain, for a signal with more than one possible pad.
    pub daisy: Option<Daisy>,
    /// Whether the mux register's SION bit (input path forced on) is set.
    pub sion: bool,
}

/// An ADC1 input channel (0–15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Adc1Channel(u8);

impl Adc1Channel {
    /// Channel `n`, if below 16.
    pub const fn new(n: u8) -> Option<Self> {
        if n < 16 { Some(Adc1Channel(n)) } else { None }
    }

    /// The channel number (`ADC_HCn` `ADCH` field value).
    pub const fn number(self) -> u8 {
        self.0
    }
}

/// Pad `index` of `group`, for `const` tables only: an index past the
/// group stops the build (const evaluation), it never runs.
const fn pad(group: PadGroup, index: u8) -> Pad {
    match IomuxcPad::new(group, index) {
        Some(p) => Pad::Iomuxc(p),
        None => panic!("no such pad"),
    }
}

/// ADC1 channel `n`, for `const` tables only (see [`pad`]).
const fn adc1(n: u8) -> Option<Adc1Channel> {
    match Adc1Channel::new(n) {
        Some(c) => Some(c),
        None => panic!("no such ADC1 channel"),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use core::fmt::Write;

    /// A fixed buffer for formatting names without an allocator.
    #[derive(Default)]
    pub(crate) struct Name([u8; 24], usize);

    impl Name {
        pub(crate) fn of(v: &dyn fmt::Display) -> Self {
            let mut n = Name::default();
            write!(n, "{v}").unwrap();
            n
        }

        pub(crate) fn as_str(&self) -> &str {
            core::str::from_utf8(&self.0[..self.1]).unwrap()
        }
    }

    impl Write for Name {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            let end = self.1 + s.len();
            self.0
                .get_mut(self.1..end)
                .ok_or(fmt::Error)?
                .copy_from_slice(s.as_bytes());
            self.1 = end;
            Ok(())
        }
    }

    fn line(bank: GpioBank, bit: u8) -> GpioLine {
        GpioLine::new(bank, bit).unwrap()
    }

    #[test]
    fn rt10_pad_rules_match_the_sdk_iomuxc_macros() {
        use GpioBank::*;
        use PadGroup::*;
        // (pad, GPIO line, mux register, pad-control register), from the
        // `IOMUXC_<pad>_GPIOx_IOyy` macros of the SDK's MIMXRT1052
        // `fsl_iomuxc.h`.
        let sdk = [
            (pad(Emc, 0), line(Gpio4, 0), 0x401F_8014, 0x401F_8204),
            (pad(Emc, 31), line(Gpio4, 31), 0x401F_8090, 0x401F_8280),
            (pad(Emc, 32), line(Gpio3, 18), 0x401F_8094, 0x401F_8284),
            (pad(Emc, 41), line(Gpio3, 27), 0x401F_80B8, 0x401F_82A8),
            (pad(AdB0, 0), line(Gpio1, 0), 0x401F_80BC, 0x401F_82AC),
            (pad(AdB0, 9), line(Gpio1, 9), 0x401F_80E0, 0x401F_82D0),
            (pad(AdB0, 12), line(Gpio1, 12), 0x401F_80EC, 0x401F_82DC),
            (pad(AdB1, 0), line(Gpio1, 16), 0x401F_80FC, 0x401F_82EC),
            (pad(AdB1, 11), line(Gpio1, 27), 0x401F_8128, 0x401F_8318),
            (pad(B0, 4), line(Gpio2, 4), 0x401F_814C, 0x401F_833C),
            (pad(B1, 15), line(Gpio2, 31), 0x401F_81B8, 0x401F_83A8),
            (pad(SdB0, 0), line(Gpio3, 12), 0x401F_81BC, 0x401F_83AC),
            (pad(SdB0, 3), line(Gpio3, 15), 0x401F_81C8, 0x401F_83B8),
            (pad(SdB1, 0), line(Gpio3, 0), 0x401F_81D4, 0x401F_83C4),
            (pad(SdB1, 11), line(Gpio3, 11), 0x401F_8200, 0x401F_83F0),
            (Pad::Wakeup, line(Gpio5, 0), 0x400A_8000, 0x400A_8018),
        ];
        for (p, gpio, mux, cfg) in sdk {
            let name = Name::of(&p);
            assert_eq!(p.gpio(), gpio, "{}", name.as_str());
            assert_eq!(p.mux_register(), mux, "{}", name.as_str());
            assert_eq!(p.config_register(), cfg, "{}", name.as_str());
        }
        assert_eq!(Pad::GPIO_ALT.mode(), 5);
    }

    #[test]
    fn rt10_constructors_refuse_what_does_not_exist() {
        for g in [
            PadGroup::Emc,
            PadGroup::AdB0,
            PadGroup::AdB1,
            PadGroup::B0,
            PadGroup::B1,
            PadGroup::SdB0,
            PadGroup::SdB1,
        ] {
            assert!(IomuxcPad::new(g, g.pad_count() - 1).is_some());
            assert_eq!(IomuxcPad::new(g, g.pad_count()), None);
            // Every pad's GPIO bit is a real line.
            for i in 0..g.pad_count() {
                let p = Pad::Iomuxc(IomuxcPad::new(g, i).unwrap());
                assert!(p.gpio().bit() < 32);
            }
        }
        assert_eq!(GpioLine::new(GpioBank::Gpio1, 32), None);
        assert_eq!(Adc1Channel::new(16), None);
        assert_eq!(Adc1Channel::new(15).map(Adc1Channel::number), Some(15));
        assert_eq!(line(GpioBank::Gpio3, 13).mask(), 1 << 13);
    }

    #[test]
    fn rt10_names_follow_the_reference_manual() {
        assert_eq!(Name::of(&pad(PadGroup::AdB1, 7)).as_str(), "GPIO_AD_B1_07");
        assert_eq!(Name::of(&pad(PadGroup::SdB0, 0)).as_str(), "GPIO_SD_B0_00");
        assert_eq!(Name::of(&Pad::Wakeup).as_str(), "WAKEUP");
        assert_eq!(Name::of(&line(GpioBank::Gpio1, 9)).as_str(), "GPIO1_IO09");
        assert_eq!(Name::of(&line(GpioBank::Gpio5, 0)).as_str(), "GPIO5_IO00");
    }
}
