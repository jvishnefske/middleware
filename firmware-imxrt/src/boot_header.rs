//! The RT1050 boot header, built as bytes: FlexSPI configuration block
//! (FCB), image vector table (IVT), boot data and device configuration
//! data (DCD).
//!
//! The boot ROM, booting from FlexSPI NOR, reads the FCB at flash offset 0
//! to configure FlexSPI, then the IVT at 0x1000, which points to the boot
//! data (where the image is and how long), the DCD (register writes it
//! executes first: here the SDRAM setup) and the entry (the image's
//! vector table). [`boot_region`] lays all of it out as the 8 KiB `.boot`
//! region (0x0..0x2000) that precedes the bootloader's code, so a
//! `build.rs` can turn it into an object file placed at the flash start
//! and no `#[link_section]` static is needed. Bytes not covered by a
//! structure are zero.
//!
//! * [`fcb`] — the FCB from typed fields; [`fcb::Fcb::evkb_factory`] is
//!   byte-identical to the board's factory block (RT1).
//! * [`ivt`] — IVT and boot data; the image description refuses a length
//!   past the flash and a misaligned or out-of-image entry (RT2).
//! * [`dcd`] — the boot ROM's DCD encoding (header, write data, check
//!   data, NOP; big-endian).
//! * [`evkb_sdram`] — the SDK's SDRAM setup for the EVKB as DCD commands.

pub mod dcd;
pub mod evkb_sdram;
pub mod fcb;
pub mod ivt;

use dcd::{Dcd, DcdError};
use fcb::{FCB_LEN, Fcb};
use ivt::{BOOT_DATA_LEN, BOOT_DATA_OFFSET, BOOT_REGION_LEN, BootHeaderError, BootImage};
use ivt::{DCD_OFFSET, IVT_LEN, IVT_OFFSET};

/// Size of the `.boot` region.
pub const BOOT_REGION_SIZE: usize = BOOT_REGION_LEN as usize;

/// Why the boot region could not be built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootRegionError {
    /// The image description was refused.
    Image(BootHeaderError),
    /// The DCD was refused.
    Dcd(DcdError),
}

/// Copy `src` into `out` at `at`; the callers' constant offsets and the
/// DCD's 1768-byte bound keep every copy inside the 8 KiB region.
fn place(out: &mut [u8; BOOT_REGION_SIZE], at: u32, src: &[u8]) {
    let at = at as usize;
    if let Some(dst) = out.get_mut(at..at + src.len()) {
        dst.copy_from_slice(src);
    }
}

/// The `.boot` region: `fcb` at 0, the IVT at 0x1000, boot data at 0x1020
/// and `dcd` (if any) at 0x1030.
pub fn boot_region(fcb: &Fcb, image: &BootImage, dcd: Option<&Dcd>) -> [u8; BOOT_REGION_SIZE] {
    let mut out = [0u8; BOOT_REGION_SIZE];
    let fcb_bytes: [u8; FCB_LEN] = fcb.to_bytes();
    place(&mut out, 0, &fcb_bytes);
    let ivt: [u8; IVT_LEN] = image.ivt(dcd.is_some());
    place(&mut out, IVT_OFFSET, &ivt);
    let boot_data: [u8; BOOT_DATA_LEN] = image.boot_data();
    place(&mut out, BOOT_DATA_OFFSET, &boot_data);
    if let Some(dcd) = dcd {
        place(&mut out, DCD_OFFSET, dcd.as_bytes());
    }
    out
}

/// The EVKB bootloader's `.boot` region: the factory FCB, the IVT and boot
/// data for an image of `length` bytes from the flash base entered at
/// `vector_table`, and the SDK SDRAM DCD.
pub fn evkb_boot_region(
    vector_table: u32,
    length: u32,
) -> Result<[u8; BOOT_REGION_SIZE], BootRegionError> {
    let image = BootImage::new(vector_table, length).map_err(BootRegionError::Image)?;
    let dcd = evkb_sdram::dcd().map_err(BootRegionError::Dcd)?;
    Ok(boot_region(&Fcb::evkb_factory(), &image, Some(&dcd)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rt2_boot_region_places_fcb_ivt_boot_data_and_dcd() {
        let Ok(region) = evkb_boot_region(0x6000_2000, 0x2_0000) else {
            panic!("valid image")
        };
        let factory = include_bytes!("../fixtures/rt1050-evkb-factory-fcb.bin");
        let sdk_dcd = include_bytes!("../fixtures/evkbimxrt1050-dcd.bin");
        assert_eq!(&region[..512], &factory[..]);
        assert!(region[512..0x1000].iter().all(|&b| b == 0));
        assert_eq!(&region[0x1000..0x1004], &[0xD1, 0x00, 0x20, 0x41]);
        assert_eq!(&region[0x1004..0x1008], &0x6000_2000u32.to_le_bytes());
        assert_eq!(&region[0x100C..0x1010], &0x6000_1030u32.to_le_bytes());
        assert_eq!(&region[0x1020..0x1024], &0x6000_0000u32.to_le_bytes());
        assert_eq!(&region[0x1024..0x1028], &0x2_0000u32.to_le_bytes());
        let dcd_end = 0x1030 + sdk_dcd.len();
        assert_eq!(&region[0x1030..dcd_end], &sdk_dcd[..]);
        assert!(region[dcd_end..].iter().all(|&b| b == 0));
    }

    #[test]
    fn rt2_boot_region_refuses_bad_images() {
        assert_eq!(
            evkb_boot_region(0x6000_2000, 0x0400_0001).err(),
            Some(BootRegionError::Image(BootHeaderError::LengthPastFlash))
        );
        assert_eq!(
            evkb_boot_region(0x6000_2100, 0x2_0000).err(),
            Some(BootRegionError::Image(BootHeaderError::EntryMisaligned))
        );
    }

    #[test]
    fn rt2_dcd_fits_before_the_bootloader() {
        assert!(DCD_OFFSET as usize + dcd::DCD_MAX_LEN <= BOOT_REGION_SIZE);
    }
}
