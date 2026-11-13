//! The image vector table (IVT) and boot data of the bootloader image.
//!
//! The boot ROM reads the IVT at flash offset 0x1000 (absolute
//! 0x6000_1000) once the FCB has configured FlexSPI. All words are
//! little-endian:
//!
//! | IVT offset | field |
//! |---|---|
//! | 0x00 | header: tag `0xD1`, length 0x0020 (big-endian), version `0x41` |
//! | 0x04 | entry: the image's vector table (the ROM loads SP and PC from it) |
//! | 0x08 | reserved, 0 |
//! | 0x0C | DCD address, or 0 for none |
//! | 0x10 | boot data address |
//! | 0x14 | self: the IVT's own address |
//! | 0x18 | CSF address, 0 (no HAB signature) |
//! | 0x1C | reserved, 0 |
//!
//! The boot data follows at 0x1020: image start (0x6000_0000, the flash
//! base: the image includes the boot header), image length in bytes, plugin
//! flag (0: not a plugin), and a zero word that pads it to 16 bytes. The
//! DCD, when present, follows at 0x1030 ([`DCD_OFFSET`]). This is the
//! layout `imxrt-rt` 0.1.8's `imxrt-boot-header.x` links (except that it
//! writes IVT version 0x40, and 0xDEADBEEF as the padding word).
//!
//! [`BootImage::new`] refuses a length past the 64 MiB flash or shorter
//! than the 8 KiB boot region, and an entry that is not 1024-byte aligned
//! (the alignment `imxrt-rt` gives the vector table, as VTOR requires for
//! the RT1050's 176 vectors) or whose first two words (SP, reset vector)
//! are not inside the image after the boot region.

use crate::hyperflash::{FLASH_BASE, FLASH_SIZE};

/// IVT offset from the flash base.
pub const IVT_OFFSET: u32 = 0x1000;
/// IVT size.
pub const IVT_LEN: usize = 32;
/// Boot data offset from the flash base.
pub const BOOT_DATA_OFFSET: u32 = IVT_OFFSET + IVT_LEN as u32;
/// Boot data size (12 bytes of fields, padded to 16).
pub const BOOT_DATA_LEN: usize = 16;
/// DCD offset from the flash base.
pub const DCD_OFFSET: u32 = BOOT_DATA_OFFSET + BOOT_DATA_LEN as u32;
/// The boot region: FCB, IVT, boot data and DCD (0x0..0x2000).
pub const BOOT_REGION_LEN: u32 = 0x2000;
/// Required vector table alignment.
pub const VECTOR_TABLE_ALIGN: u32 = 1024;

const IVT_TAG: u8 = 0xD1;
/// IVT header version.
pub const IVT_VERSION: u8 = 0x41;

/// Why the image description was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootHeaderError {
    /// The image runs past the end of the flash.
    LengthPastFlash,
    /// The image is shorter than the boot region.
    LengthTooShort,
    /// The vector table is not 1024-byte aligned.
    EntryMisaligned,
    /// The vector table is not inside the image, after the boot region.
    EntryOutsideImage,
}

/// The bootloader image the boot ROM loads: from the flash base, `length`
/// bytes, entered through the vector table at `vector_table`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootImage {
    vector_table: u32,
    length: u32,
}

impl BootImage {
    /// Check and describe the image (`vector_table` is an absolute address
    /// in the FlexSPI map).
    pub const fn new(vector_table: u32, length: u32) -> Result<BootImage, BootHeaderError> {
        if length > FLASH_SIZE {
            return Err(BootHeaderError::LengthPastFlash);
        }
        if length < BOOT_REGION_LEN {
            return Err(BootHeaderError::LengthTooShort);
        }
        if !vector_table.is_multiple_of(VECTOR_TABLE_ALIGN) {
            return Err(BootHeaderError::EntryMisaligned);
        }
        let first = FLASH_BASE + BOOT_REGION_LEN;
        let end = FLASH_BASE + length;
        if vector_table < first || vector_table > end - 8 {
            return Err(BootHeaderError::EntryOutsideImage);
        }
        Ok(BootImage {
            vector_table,
            length,
        })
    }

    /// The vector table address.
    pub const fn vector_table(&self) -> u32 {
        self.vector_table
    }

    /// Image length in bytes.
    pub const fn length(&self) -> u32 {
        self.length
    }

    /// The IVT; `dcd` says whether a DCD follows the boot data.
    pub const fn ivt(&self, dcd: bool) -> [u8; IVT_LEN] {
        let words = [
            u32::from_le_bytes([IVT_TAG, 0x00, IVT_LEN as u8, IVT_VERSION]),
            self.vector_table,
            0,
            if dcd { FLASH_BASE + DCD_OFFSET } else { 0 },
            FLASH_BASE + BOOT_DATA_OFFSET,
            FLASH_BASE + IVT_OFFSET,
            0,
            0,
        ];
        let mut out = [0u8; IVT_LEN];
        let mut i = 0;
        while i < words.len() {
            let b = words[i].to_le_bytes();
            out[4 * i] = b[0];
            out[4 * i + 1] = b[1];
            out[4 * i + 2] = b[2];
            out[4 * i + 3] = b[3];
            i += 1;
        }
        out
    }

    /// The boot data: start, length, plugin flag, padding.
    pub const fn boot_data(&self) -> [u8; BOOT_DATA_LEN] {
        let words = [FLASH_BASE, self.length, 0, 0];
        let mut out = [0u8; BOOT_DATA_LEN];
        let mut i = 0;
        while i < words.len() {
            let b = words[i].to_le_bytes();
            out[4 * i] = b[0];
            out[4 * i + 1] = b[1];
            out[4 * i + 2] = b[2];
            out[4 * i + 3] = b[3];
            i += 1;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
    }

    #[test]
    fn rt2_ivt_fields_at_the_boot_rom_offsets() {
        let Ok(img) = BootImage::new(0x6000_2000, 0x1_0000) else {
            panic!("valid image")
        };
        let ivt = img.ivt(true);
        assert_eq!(&ivt[0..4], &[0xD1, 0x00, 0x20, 0x41]);
        assert_eq!(word(&ivt, 0x04), 0x6000_2000, "entry");
        assert_eq!(word(&ivt, 0x08), 0);
        assert_eq!(word(&ivt, 0x0C), 0x6000_1030, "DCD");
        assert_eq!(word(&ivt, 0x10), 0x6000_1020, "boot data");
        assert_eq!(word(&ivt, 0x14), 0x6000_1000, "self");
        assert_eq!(word(&ivt, 0x18), 0, "CSF");
        assert_eq!(word(&ivt, 0x1C), 0);
        assert_eq!(word(&img.ivt(false), 0x0C), 0, "no DCD");
        let bd = img.boot_data();
        assert_eq!(word(&bd, 0), 0x6000_0000, "start");
        assert_eq!(word(&bd, 4), 0x1_0000, "length");
        assert_eq!(word(&bd, 8), 0, "not a plugin");
    }

    #[test]
    fn rt2_ivt_refuses_bad_length_or_entry() {
        assert_eq!(
            BootImage::new(0x6000_2000, FLASH_SIZE + 1),
            Err(BootHeaderError::LengthPastFlash)
        );
        assert_eq!(
            BootImage::new(0x6000_2000, 0x1FFF),
            Err(BootHeaderError::LengthTooShort)
        );
        assert_eq!(
            BootImage::new(0x6000_2004, 0x1_0000),
            Err(BootHeaderError::EntryMisaligned)
        );
        assert_eq!(
            BootImage::new(0x6000_2200, 0x1_0000),
            Err(BootHeaderError::EntryMisaligned)
        );
        assert_eq!(
            BootImage::new(0x6000_1C00, 0x1_0000),
            Err(BootHeaderError::EntryOutsideImage)
        );
        assert_eq!(
            BootImage::new(0x6001_0000, 0x1_0000),
            Err(BootHeaderError::EntryOutsideImage)
        );
        assert_eq!(
            BootImage::new(0x0000_0000, 0x1_0000),
            Err(BootHeaderError::EntryOutsideImage)
        );
        assert!(BootImage::new(0x6000_FC00, 0x1_0000).is_ok());
        assert!(BootImage::new(0x6000_2000, FLASH_SIZE).is_ok());
    }
}
