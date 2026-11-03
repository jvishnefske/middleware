//! Flash maps: where the firmware, the payload store and the trust record
//! live on each board's QSPI flash.
//!
//! Offsets are from the start of flash (XIP address `0x1000_0000`), the
//! convention of `embassy_rp::flash::Flash` and of the `embedded-storage`
//! offsets the store uses. Every region is a whole number of 4 KiB sectors.
//!
//! RP2040, Raspberry Pi Pico (2 MiB, [`RP2040`]):
//!
//! | offset     | size     | region                                         |
//! |------------|----------|------------------------------------------------|
//! | `0x000000` | 1.5 MiB  | firmware: 256-byte boot2, then the image       |
//! | `0x180000` | 256 KiB  | payload store: two 128 KiB slots (124 KiB images) |
//! | `0x1C0000` | 8 KiB    | trust record: two alternately written sectors |
//! | `0x1C2000` | 248 KiB  | unused                                         |
//!
//! RP2350A, Raspberry Pi Pico 2 (4 MiB, [`RP2350`]), with A/B firmware
//! slots for over-the-air updates ([`crate::ota`]):
//!
//! | offset     | size      | region                                        |
//! |------------|-----------|-----------------------------------------------|
//! | `0x000000` | 8 KiB     | partition table (the boot ROM reads sector 0) |
//! | `0x002000` | 1528 KiB  | firmware slot A, partition 0                  |
//! | `0x180000` | 1528 KiB  | firmware slot B, partition 1 (linked to A)    |
//! | `0x2FE000` | 8 KiB     | firmware-update record: two alternately written sectors |
//! | `0x300000` | 256 KiB   | payload store: two 128 KiB slots (124 KiB images) |
//! | `0x340000` | 8 KiB     | trust record                                  |
//! | `0x342000` | 760 KiB   | unused                                        |
//!
//! The partition table ([`crate::ota::table`]) makes slots A and B an A/B
//! pair; the boot ROM boots the valid image with the higher version and
//! maps the chosen partition at `0x1000_0000` (address translation), so the
//! one linked image (`memory-rp2350.x`, `FLASH` = one slot) runs from
//! either slot. Everything outside the running partition is reached
//! through an untranslated window ([`crate::flash::Window`]), as the
//! payload store, the trust record and the other slot must be. The payload
//! store and the trust record keep the offsets of the first RP2350 map, so
//! a board flashed before A/B keeps its payload and its pinned key.
//!
//! The RP2040 has no A/B boot support in its ROM, so its map has a single
//! firmware region. Both payload stores have the size of the ESP base
//! firmware's `payload` partition, so `sys/info` reports the same 124 KiB
//! capacity and `mwctl` sees no difference.
//!
//! [`FlashMap::check`] validates a map (alignment, order, no overlap, within
//! the flash, equal A/B slots, a partition table and an update record
//! exactly when there are two slots); the unit tests check both maps and
//! that the `FLASH` length in each `memory-*.x` equals
//! [`FlashMap::linked_firmware`].

use esp_base::image::SECTOR;

/// Bytes the RP2040's second-stage bootloader takes at the start of flash.
pub const BOOT2_LEN: u32 = 0x100;

/// Bytes the RP2350 partition table region takes at the start of flash.
pub const PARTITION_TABLE_RESERVE: u32 = 2 * SECTOR;

/// Bytes of the firmware-update record region (two sectors written in
/// turn, `middleware::fw_ota::record`).
pub const FW_RECORD_LEN: u32 = 2 * SECTOR;

/// A sector-aligned span of flash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    /// Offset from the start of flash.
    pub offset: u32,
    /// Length in bytes.
    pub size: u32,
}

impl Region {
    /// The region `[offset, offset + size)`.
    pub const fn new(offset: u32, size: u32) -> Self {
        Region { offset, size }
    }

    /// One past the last byte (saturating).
    pub const fn end(&self) -> u32 {
        self.offset.saturating_add(self.size)
    }

    /// Whether offset and size are whole sectors and the region is not empty.
    pub const fn is_aligned(&self) -> bool {
        self.size > 0 && self.offset.is_multiple_of(SECTOR) && self.size.is_multiple_of(SECTOR)
    }

    /// Whether the two regions share a byte.
    pub const fn overlaps(&self, other: &Region) -> bool {
        self.offset < other.end() && other.offset < self.end()
    }
}

/// What precedes the linked image inside the first firmware region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Boot {
    /// RP2040: the 256-byte second-stage bootloader (`BOOT2` in
    /// `memory-rp2040.x`), then the image.
    Boot2,
    /// RP2350: the image's `IMAGE_DEF` lies in its first 4 KiB, and the
    /// image fills at most one firmware slot.
    ImageDef,
}

/// One board's flash map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlashMap {
    /// Total flash size.
    pub flash_size: u32,
    /// How the boot ROM finds the image.
    pub boot: Boot,
    /// The partition table (RP2350 with A/B slots): the first sectors.
    pub partition_table: Option<Region>,
    /// The firmware region (slot A, partition 0, on the RP2350).
    pub firmware: Region,
    /// Firmware slot B (RP2350, partition 1), the size of slot A.
    pub firmware_b: Option<Region>,
    /// The firmware-update record (RP2350), outside both slots.
    pub fw_record: Option<Region>,
    /// The two-slot payload store ([`esp_base::store::PayloadStore`]).
    pub payload: Region,
    /// The trust record ([`esp_base::trust_store::TrustStore`]).
    pub trust: Region,
}

/// Why a flash map is invalid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutError {
    /// A region is empty or not sector aligned.
    Unaligned,
    /// A region ends beyond the flash.
    OutOfFlash,
    /// Two regions overlap.
    Overlap,
    /// The first region (partition table, else firmware) does not start
    /// at offset 0.
    FirmwareNotFirst,
    /// The trust region is not the trust store's two sectors.
    TrustSize,
    /// Slot B, the partition table and the update record come together,
    /// slot B has slot A's size and the record is [`FW_RECORD_LEN`].
    AbShape,
}

/// The A/B part of a map: what the firmware updater addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AbMap {
    /// Slot A (partition 0).
    pub a: Region,
    /// Slot B (partition 1).
    pub b: Region,
    /// The two record sectors.
    pub record: Region,
}

impl FlashMap {
    /// Every region, in flash order for the shipped maps.
    pub fn regions(&self) -> impl Iterator<Item = Region> {
        [
            self.partition_table,
            Some(self.firmware),
            self.firmware_b,
            self.fw_record,
            Some(self.payload),
            Some(self.trust),
        ]
        .into_iter()
        .flatten()
    }

    /// Bytes the linker may fill (the `FLASH` length of `memory-*.x`): the
    /// firmware region less boot2 on the RP2040, one slot on the RP2350.
    pub const fn linked_firmware(&self) -> u32 {
        match self.boot {
            Boot::Boot2 => self.firmware.size.saturating_sub(BOOT2_LEN),
            Boot::ImageDef => self.firmware.size,
        }
    }

    /// The A/B slots and the update record, when the map has them.
    pub const fn ab(&self) -> Option<AbMap> {
        match (self.firmware_b, self.fw_record) {
            (Some(b), Some(record)) => Some(AbMap { a: self.firmware, b, record }),
            _ => None,
        }
    }

    /// Flash not covered by any region.
    pub fn unused(&self) -> u32 {
        let used = self.regions().fold(0u32, |n, r| n.saturating_add(r.size));
        self.flash_size.saturating_sub(used)
    }

    /// Checks the map: the partition table (or, without one, the firmware)
    /// at 0, every region aligned and inside the flash, no two
    /// overlapping, the trust region exactly
    /// [`esp_base::trust_store::PARTITION_LEN`], and the A/B shape
    /// ([`LayoutError::AbShape`]).
    pub fn check(&self) -> Result<(), LayoutError> {
        let first = self.partition_table.unwrap_or(self.firmware);
        if first.offset != 0 {
            return Err(LayoutError::FirmwareNotFirst);
        }
        if self.trust.size != esp_base::trust_store::PARTITION_LEN {
            return Err(LayoutError::TrustSize);
        }
        let ab = (self.partition_table, self.firmware_b, self.fw_record);
        match ab {
            (None, None, None) => {}
            (Some(_), Some(b), Some(r)) if b.size == self.firmware.size && r.size == FW_RECORD_LEN => {}
            _ => return Err(LayoutError::AbShape),
        }
        // At most six regions: every pair is compared once.
        let all: heapless::Vec<Region, 6> = self.regions().collect();
        for (i, a) in all.iter().enumerate() {
            if !a.is_aligned() {
                return Err(LayoutError::Unaligned);
            }
            if a.end() > self.flash_size {
                return Err(LayoutError::OutOfFlash);
            }
            if all.iter().skip(i + 1).any(|b| a.overlaps(b)) {
                return Err(LayoutError::Overlap);
            }
        }
        Ok(())
    }
}

/// Raspberry Pi Pico: RP2040 with 2 MiB of flash.
pub const RP2040: FlashMap = FlashMap {
    flash_size: 0x20_0000,
    boot: Boot::Boot2,
    partition_table: None,
    firmware: Region::new(0x00_0000, 0x18_0000),
    firmware_b: None,
    fw_record: None,
    payload: Region::new(0x18_0000, 0x4_0000),
    trust: Region::new(0x1C_0000, 0x2000),
};

/// Raspberry Pi Pico 2: RP2350A with 4 MiB of flash.
pub const RP2350: FlashMap = FlashMap {
    flash_size: 0x40_0000,
    boot: Boot::ImageDef,
    partition_table: Some(Region::new(0x00_0000, PARTITION_TABLE_RESERVE)),
    firmware: Region::new(0x00_2000, 0x17_E000),
    firmware_b: Some(Region::new(0x18_0000, 0x17_E000)),
    fw_record: Some(Region::new(0x2F_E000, FW_RECORD_LEN)),
    payload: Region::new(0x30_0000, 0x4_0000),
    trust: Region::new(0x34_0000, 0x2000),
};

#[cfg(test)]
mod tests {
    use super::*;
    use esp_base::image::Region as Slots;

    /// The `LENGTH` of the `FLASH` line of a `memory.x`.
    fn flash_length(memory_x: &str) -> Option<u32> {
        let line = memory_x.lines().find(|l| l.trim_start().starts_with("FLASH"))?;
        let hex = line.split("LENGTH = 0x").nth(1)?;
        let digits: &str = hex.split(|c: char| !c.is_ascii_hexdigit()).next()?;
        u32::from_str_radix(digits, 16).ok()
    }

    #[test]
    fn v2_both_maps_are_valid_and_keep_the_esp_payload_capacity() {
        for map in [RP2040, RP2350] {
            assert_eq!(map.check(), Ok(()), "{map:?}");
            assert_eq!(Slots::new(map.payload.size).capacity(), 124 * 1024);
        }
        assert_eq!(RP2040.unused(), 248 * 1024);
        assert_eq!(RP2350.unused(), 760 * 1024);
    }

    #[test]
    fn y13_rp2350_has_a_table_two_equal_slots_and_the_record_outside_them() {
        let pt = RP2350.partition_table.unwrap();
        assert_eq!(pt, Region::new(0, PARTITION_TABLE_RESERVE));
        let ab = RP2350.ab().unwrap();
        assert_eq!(ab.a.offset, pt.end());
        assert_eq!(ab.b.size, ab.a.size);
        assert_eq!(ab.b.offset, ab.a.end());
        assert_eq!(ab.record.offset, ab.b.end());
        assert_eq!(ab.record.size, 2 * SECTOR);
        assert!(!ab.record.overlaps(&ab.a) && !ab.record.overlaps(&ab.b));
        // The payload store and the trust record did not move.
        assert_eq!((RP2350.payload.offset, RP2350.trust.offset), (0x30_0000, 0x34_0000));
        assert_eq!(ab.record.end(), RP2350.payload.offset);
        assert_eq!(RP2040.ab(), None);
        assert_eq!(RP2040.partition_table, None);
    }

    #[test]
    fn v2_memory_x_flash_lengths_match_the_maps() {
        assert_eq!(flash_length(include_str!("../memory-rp2040.x")), Some(RP2040.linked_firmware()));
        assert_eq!(flash_length(include_str!("../memory-rp2350.x")), Some(RP2350.linked_firmware()));
        assert_eq!(RP2040.linked_firmware(), 0x18_0000 - 0x100);
        assert_eq!(RP2350.linked_firmware(), 0x18_0000 - 0x2000);
        assert_eq!(RP2350.linked_firmware(), RP2350.ab().unwrap().b.size);
    }

    #[test]
    fn v2_check_refuses_broken_maps() {
        let unaligned = FlashMap { payload: Region::new(0x18_0100, 0x4_0000), ..RP2040 };
        assert_eq!(unaligned.check(), Err(LayoutError::Unaligned));
        let overlap = FlashMap { trust: Region::new(0x1B_F000, 0x2000), ..RP2040 };
        assert_eq!(overlap.check(), Err(LayoutError::Overlap));
        let beyond = FlashMap { trust: Region::new(0x1F_F000, 0x2000), ..RP2040 };
        assert_eq!(beyond.check(), Err(LayoutError::OutOfFlash));
        let late = FlashMap { firmware: Region::new(0x1000, 0x17_F000), ..RP2040 };
        assert_eq!(late.check(), Err(LayoutError::FirmwareNotFirst));
        let small = FlashMap { trust: Region::new(0x1C_0000, 0x1000), ..RP2040 };
        assert_eq!(small.check(), Err(LayoutError::TrustSize));
        let slot_b = FlashMap { firmware_b: Some(Region::new(0x17_0000, 0x17_E000)), ..RP2350 };
        assert_eq!(slot_b.check(), Err(LayoutError::Overlap));
        let short_b = FlashMap { firmware_b: Some(Region::new(0x18_0000, 0x1_0000)), ..RP2350 };
        assert_eq!(short_b.check(), Err(LayoutError::AbShape));
        let no_record = FlashMap { fw_record: None, ..RP2350 };
        assert_eq!(no_record.check(), Err(LayoutError::AbShape));
        let no_table = FlashMap { partition_table: None, ..RP2350 };
        assert_eq!(no_table.check(), Err(LayoutError::FirmwareNotFirst));
        let big_record = FlashMap { fw_record: Some(Region::new(0x2F_D000, 0x3000)), ..RP2350 };
        assert_eq!(big_record.check(), Err(LayoutError::AbShape));
        assert!(!Region::new(0, 0).is_aligned());
    }
}
