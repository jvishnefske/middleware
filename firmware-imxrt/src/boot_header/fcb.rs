//! The FlexSPI configuration block (FCB): how the boot ROM talks to the
//! boot flash.
//!
//! 512 bytes at flash offset 0, little-endian, laid out as the boot ROM's
//! `flexspi_nor_config_t` (i.MX RT1050 reference manual, "FlexSPI
//! Configuration Block"; the same layout as the `imxrt-boot-gen` crate and
//! NXP's `evkbimxrt1050_flexspi_nor_config.c`). The fields set here:
//!
//! | offset | field |
//! |---|---|
//! | 0x000 | tag `FCFB` (0x4246_4346) |
//! | 0x004 | version, `'V'` major minor bugfix |
//! | 0x00C | readSampleClkSrc |
//! | 0x00D | csHoldTime |
//! | 0x00E | csSetupTime |
//! | 0x00F | columnAddressWidth |
//! | 0x040 | controllerMiscOption |
//! | 0x044 | deviceType |
//! | 0x045 | sflashPadType |
//! | 0x046 | serialClkFreq |
//! | 0x047 | lutCustomSeqEnable |
//! | 0x050 | sflashA1Size, A2, B1, B2 |
//! | 0x080 | lookupTable (64 words) |
//! | 0x1C0 | pageSize |
//! | 0x1C4 | sectorSize |
//! | 0x1D0 | blockSize |
//!
//! Every other byte is zero. [`Fcb::evkb_factory`] is the board's factory
//! block: HyperFlash, DQS read sampling, DDR, word addressable, 64 MiB,
//! the HyperFlash read sequence in LUT slot 0, and nothing else (no page,
//! sector or block sizes, `deviceType` 0), byte-identical to the 512 bytes
//! read from the board (`fixtures/rt1050-evkb-factory-fcb.bin`).

use crate::hyperflash::lut::Lut;
use crate::hyperflash::s26ks512s::FCB_LUT;

/// FCB size in bytes.
pub const FCB_LEN: usize = 512;
/// The FCB tag, "FCFB" read as a little-endian u32.
pub const FCB_TAG: u32 = 0x4246_4346;

/// FCB format version, encoded `'V' << 24 | major << 16 | minor << 8 |
/// bugfix`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Version {
    /// Major.
    pub major: u8,
    /// Minor.
    pub minor: u8,
    /// Bugfix.
    pub bugfix: u8,
}

impl Version {
    /// V1.0.0, the factory block's.
    pub const V1_0_0: Version = Version {
        major: 1,
        minor: 0,
        bugfix: 0,
    };

    const fn word(self) -> u32 {
        (b'V' as u32) << 24
            | (self.major as u32) << 16
            | (self.minor as u32) << 8
            | self.bugfix as u32
    }
}

/// Where FlexSPI samples read data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadSampleClock {
    /// Internal loopback.
    InternalLoopback = 0,
    /// Loopback from the DQS pad.
    LoopbackFromDqsPad = 1,
    /// Loopback from the SCK pad.
    LoopbackFromSckPad = 2,
    /// The flash's DQS (HyperFlash RWDS).
    FlashProvidedDqs = 3,
}

/// Column address width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnAddressWidth {
    /// Serial NOR other than HyperFlash.
    Other = 0,
    /// HyperFlash: 3 column bits.
    HyperFlash = 3,
}

/// Device type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceType {
    /// Not set (the EVKB's factory block carries 0).
    Unset = 0,
    /// Serial NOR.
    SerialNor = 1,
    /// Serial NAND.
    SerialNand = 2,
}

/// Data pads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PadType {
    /// 1 pad.
    Single = 1,
    /// 2 pads.
    Dual = 2,
    /// 4 pads.
    Quad = 4,
    /// 8 pads (HyperFlash).
    Octal = 8,
}

/// Serial clock frequency (RT1050 boot ROM encoding).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SerialClock {
    /// 30 MHz.
    Mhz30 = 1,
    /// 50 MHz.
    Mhz50 = 2,
    /// 60 MHz.
    Mhz60 = 3,
    /// 75 MHz.
    Mhz75 = 4,
    /// 80 MHz.
    Mhz80 = 5,
    /// 100 MHz.
    Mhz100 = 6,
    /// 133 MHz.
    Mhz133 = 7,
    /// 166 MHz.
    Mhz166 = 8,
}

/// controllerMiscOption bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct MiscOptions(u32);

impl MiscOptions {
    /// Differential clock.
    pub const DIFF_CLK: MiscOptions = MiscOptions(1 << 0);
    /// CK2 (second clock).
    pub const CK2: MiscOptions = MiscOptions(1 << 1);
    /// Parallel mode.
    pub const PARALLEL: MiscOptions = MiscOptions(1 << 2);
    /// Word addressable (HyperFlash).
    pub const WORD_ADDRESSABLE: MiscOptions = MiscOptions(1 << 3);
    /// Safe configuration frequency.
    pub const SAFE_CONFIG_FREQ: MiscOptions = MiscOptions(1 << 4);
    /// Pad setting override.
    pub const PAD_SETTING_OVERRIDE: MiscOptions = MiscOptions(1 << 5);
    /// DDR mode.
    pub const DDR_MODE: MiscOptions = MiscOptions(1 << 6);

    /// No options.
    pub const NONE: MiscOptions = MiscOptions(0);

    /// Both sets of options.
    pub const fn with(self, other: MiscOptions) -> MiscOptions {
        MiscOptions(self.0 | other.0)
    }

    /// The register bits.
    pub const fn bits(self) -> u32 {
        self.0
    }
}

/// Serial flash sizes in bytes per FlexSPI chip select.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct FlashSizes {
    /// Port A, CS 1.
    pub a1: u32,
    /// Port A, CS 2.
    pub a2: u32,
    /// Port B, CS 1.
    pub b1: u32,
    /// Port B, CS 2.
    pub b2: u32,
}

/// The FCB's typed fields. Built with [`Fcb::new`] and the `with_*`
/// methods (each returns a new value), encoded with [`Fcb::to_bytes`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fcb {
    version: Version,
    read_sample_clock: ReadSampleClock,
    cs_hold_time: u8,
    cs_setup_time: u8,
    column_address_width: ColumnAddressWidth,
    misc: MiscOptions,
    device_type: DeviceType,
    pad_type: PadType,
    serial_clock: SerialClock,
    lut_custom_seq_enable: bool,
    sizes: FlashSizes,
    lut: Lut,
    page_size: u32,
    sector_size: u32,
    block_size: u32,
}

impl Fcb {
    /// An FCB with `lut`, version 1.0.0, internal loopback sampling, CS
    /// hold and setup 3 cycles, single pad at 30 MHz, everything else zero.
    pub const fn new(lut: Lut) -> Fcb {
        Fcb {
            version: Version::V1_0_0,
            read_sample_clock: ReadSampleClock::InternalLoopback,
            cs_hold_time: 3,
            cs_setup_time: 3,
            column_address_width: ColumnAddressWidth::Other,
            misc: MiscOptions::NONE,
            device_type: DeviceType::Unset,
            pad_type: PadType::Single,
            serial_clock: SerialClock::Mhz30,
            lut_custom_seq_enable: false,
            sizes: FlashSizes {
                a1: 0,
                a2: 0,
                b1: 0,
                b2: 0,
            },
            lut,
            page_size: 0,
            sector_size: 0,
            block_size: 0,
        }
    }

    /// The EVKB's factory FCB (HyperFlash S26KS512S, 64 MiB on A1).
    pub const fn evkb_factory() -> Fcb {
        Fcb::new(FCB_LUT)
            .with_read_sample_clock(ReadSampleClock::FlashProvidedDqs)
            .with_cs_times(3, 3)
            .with_column_address_width(ColumnAddressWidth::HyperFlash)
            .with_misc(
                MiscOptions::DDR_MODE
                    .with(MiscOptions::WORD_ADDRESSABLE)
                    .with(MiscOptions::SAFE_CONFIG_FREQ)
                    .with(MiscOptions::DIFF_CLK),
            )
            .with_device_type(DeviceType::Unset)
            .with_pad_type(PadType::Octal)
            .with_serial_clock(SerialClock::Mhz133)
            .with_flash_sizes(FlashSizes {
                a1: crate::hyperflash::FLASH_SIZE,
                a2: 0,
                b1: 0,
                b2: 0,
            })
    }

    /// Format version.
    pub const fn with_version(self, version: Version) -> Fcb {
        Fcb { version, ..self }
    }
    /// Read sample clock source.
    pub const fn with_read_sample_clock(self, read_sample_clock: ReadSampleClock) -> Fcb {
        Fcb {
            read_sample_clock,
            ..self
        }
    }
    /// CS hold and setup times, in serial clock cycles.
    pub const fn with_cs_times(self, hold: u8, setup: u8) -> Fcb {
        Fcb {
            cs_hold_time: hold,
            cs_setup_time: setup,
            ..self
        }
    }
    /// Column address width.
    pub const fn with_column_address_width(self, column_address_width: ColumnAddressWidth) -> Fcb {
        Fcb {
            column_address_width,
            ..self
        }
    }
    /// Controller options.
    pub const fn with_misc(self, misc: MiscOptions) -> Fcb {
        Fcb { misc, ..self }
    }
    /// Device type.
    pub const fn with_device_type(self, device_type: DeviceType) -> Fcb {
        Fcb {
            device_type,
            ..self
        }
    }
    /// Data pads.
    pub const fn with_pad_type(self, pad_type: PadType) -> Fcb {
        Fcb { pad_type, ..self }
    }
    /// Serial clock.
    pub const fn with_serial_clock(self, serial_clock: SerialClock) -> Fcb {
        Fcb {
            serial_clock,
            ..self
        }
    }
    /// Use the custom LUT sequence table (left all zero here).
    pub const fn with_lut_custom_seq(self, enable: bool) -> Fcb {
        Fcb {
            lut_custom_seq_enable: enable,
            ..self
        }
    }
    /// Flash sizes per chip select.
    pub const fn with_flash_sizes(self, sizes: FlashSizes) -> Fcb {
        Fcb { sizes, ..self }
    }
    /// Serial NOR page, sector and block sizes.
    pub const fn with_nor_geometry(self, page_size: u32, sector_size: u32, block_size: u32) -> Fcb {
        Fcb {
            page_size,
            sector_size,
            block_size,
            ..self
        }
    }

    /// The 512 bytes.
    pub const fn to_bytes(&self) -> [u8; FCB_LEN] {
        let mut out = [0u8; FCB_LEN];
        put_u32(&mut out, 0x000, FCB_TAG);
        put_u32(&mut out, 0x004, self.version.word());
        out[0x00C] = self.read_sample_clock as u8;
        out[0x00D] = self.cs_hold_time;
        out[0x00E] = self.cs_setup_time;
        out[0x00F] = self.column_address_width as u8;
        put_u32(&mut out, 0x040, self.misc.bits());
        out[0x044] = self.device_type as u8;
        out[0x045] = self.pad_type as u8;
        out[0x046] = self.serial_clock as u8;
        out[0x047] = self.lut_custom_seq_enable as u8;
        put_u32(&mut out, 0x050, self.sizes.a1);
        put_u32(&mut out, 0x054, self.sizes.a2);
        put_u32(&mut out, 0x058, self.sizes.b1);
        put_u32(&mut out, 0x05C, self.sizes.b2);
        let lut = self.lut.to_le_bytes();
        let mut i = 0;
        while i < lut.len() {
            out[0x080 + i] = lut[i];
            i += 1;
        }
        put_u32(&mut out, 0x1C0, self.page_size);
        put_u32(&mut out, 0x1C4, self.sector_size);
        put_u32(&mut out, 0x1D0, self.block_size);
        out
    }
}

/// Little-endian u32 at a constant in-bounds offset (all callers pass
/// offsets below `FCB_LEN - 4`).
const fn put_u32(out: &mut [u8; FCB_LEN], at: usize, value: u32) {
    let b = value.to_le_bytes();
    out[at] = b[0];
    out[at + 1] = b[1];
    out[at + 2] = b[2];
    out[at + 3] = b[3];
}

#[cfg(test)]
mod tests {
    use super::*;

    const FACTORY: &[u8; FCB_LEN] = include_bytes!("../../fixtures/rt1050-evkb-factory-fcb.bin");

    #[test]
    fn rt1_fcb_matches_the_factory_block() {
        assert_eq!(&Fcb::evkb_factory().to_bytes(), FACTORY);
    }

    #[test]
    fn rt1_fcb_fields_at_the_boot_rom_offsets() {
        let b = Fcb::evkb_factory().to_bytes();
        assert_eq!(&b[0..4], b"FCFB");
        assert_eq!(u32::from_le_bytes([b[4], b[5], b[6], b[7]]), 0x5601_0000);
        assert_eq!(b[0x0C], 3, "DQS read sampling");
        assert_eq!(b[0x0F], 3, "HyperFlash column address width");
        assert_eq!(
            b[0x40], 0x59,
            "DDR, word addressable, safe freq, diff clock"
        );
        assert_eq!(b[0x45], 8, "octal pads");
        assert_eq!(
            u32::from_le_bytes([b[0x50], b[0x51], b[0x52], b[0x53]]),
            64 << 20
        );
        // LUT slot 0: the HyperFlash DDR read.
        assert_eq!(
            &b[0x80..0x8C],
            &[
                0xA0, 0x87, 0x18, 0x8B, 0x10, 0x8F, 0x06, 0xB3, 0x04, 0xA7, 0, 0
            ]
        );
    }

    #[test]
    fn rt1_fcb_fields_are_independent() {
        let base = Fcb::evkb_factory();
        let b = base
            .with_nor_geometry(512, 256 * 1024, 256 * 1024)
            .to_bytes();
        assert_eq!(&b[0x1C0..0x1C4], &512u32.to_le_bytes());
        assert_eq!(&b[0x1D0..0x1D4], &(256u32 * 1024).to_le_bytes());
        assert_eq!(&b[..0x1C0], &FACTORY[..0x1C0]);
        let v = base
            .with_version(Version {
                major: 1,
                minor: 4,
                bugfix: 0,
            })
            .to_bytes();
        assert_eq!(&v[4..8], &0x5601_0400u32.to_le_bytes());
        let d = base
            .with_device_type(DeviceType::SerialNor)
            .with_lut_custom_seq(true)
            .to_bytes();
        assert_eq!((d[0x44], d[0x47]), (1, 1));
    }
}
