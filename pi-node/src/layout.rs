//! The flash map: where the payload store and the trust record live in
//! the node's flash ([`crate::flash`]), and what follows from it.
//!
//! | region | offset | size |
//! |--------|--------|------|
//! | payload store (two slots) | 0x00000 | 512 KiB |
//! | trust record (two sectors) | 0x80000 | 8 KiB |
//!
//! Each payload slot is 256 KiB: a header sector and 252 KiB of image
//! (`esp_base::image::Region`), twice the ESP partition's because a Linux
//! host has the memory to run larger payloads. The store and the record
//! are esp-base's, byte for byte, so a state directory holds what a
//! board's flash would.

use esp_base::image::Region;
use esp_base::trust_store::PARTITION_LEN;

use crate::flash::SECTOR;

/// Offset of the payload store.
pub const PAYLOAD_OFFSET: u32 = 0;
/// Size of the payload store.
pub const PAYLOAD_SIZE: u32 = 512 * 1024;
/// Offset of the trust record.
pub const TRUST_OFFSET: u32 = PAYLOAD_OFFSET + PAYLOAD_SIZE;
/// Size of the trust record.
pub const TRUST_SIZE: u32 = PARTITION_LEN;
/// The flash's size in sectors.
pub const SECTORS: u32 = (TRUST_OFFSET + TRUST_SIZE) / SECTOR;
/// The largest image a slot holds (what `sys/info` reports as capacity).
pub const MAX_IMAGE: u32 = Region::new(PAYLOAD_SIZE).capacity();

// The regions do not overlap and the flash ends with the trust record.
const _: () = assert!(PAYLOAD_OFFSET + PAYLOAD_SIZE <= TRUST_OFFSET);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pn5_regions_are_whole_sectors_and_do_not_overlap() {
        for (offset, size) in [(PAYLOAD_OFFSET, PAYLOAD_SIZE), (TRUST_OFFSET, TRUST_SIZE)] {
            assert!(offset.is_multiple_of(SECTOR) && size.is_multiple_of(SECTOR));
        }
        assert_eq!(SECTORS, 130);
        assert_eq!(MAX_IMAGE, 252 * 1024);
    }
}
