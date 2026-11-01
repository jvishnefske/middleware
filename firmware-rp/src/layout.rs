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
//! RP2350A, Raspberry Pi Pico 2 (4 MiB, [`RP2350`]):
//!
//! | offset     | size     | region                                         |
//! |------------|----------|------------------------------------------------|
//! | `0x000000` | 1.5 MiB  | firmware slot A (today's image; see below)     |
//! | `0x180000` | 1.5 MiB  | firmware slot B, reserved for A/B firmware OTA |
//! | `0x300000` | 256 KiB  | payload store: two 128 KiB slots (124 KiB images) |
//! | `0x340000` | 8 KiB    | trust record                                   |
//! | `0x342000` | 760 KiB  | reserved for firmware OTA metadata             |
//!
//! The RP2350 map leaves the A/B firmware OTA work (branch `fw-ab-ota`)
//! its two equal slots without moving the payload store or the trust
//! record. Today the image is written at offset 0 and the boot ROM finds
//! its `IMAGE_DEF` there. With A/B the boot ROM reads a partition table
//! from the start of flash and maps the chosen partition at `0x1000_0000`
//! (address translation), so the same image runs from either slot; the
//! linked image is capped at slot A less [`PARTITION_TABLE_RESERVE`]
//! (`memory-rp2350.x`) so it still fits slot A once the table takes the
//! first sectors. Nothing in this firmware writes either firmware slot.
//!
//! The RP2040 has no A/B boot support in its ROM, so its map has a single
//! firmware region. Both payload stores have the size of the ESP base
//! firmware's `payload` partition, so `sys/info` reports the same 124 KiB
//! capacity and `mwctl` sees no difference.
//!
//! [`FlashMap::check`] validates a map (alignment, order, no overlap, within
//! the flash); the unit tests check both maps and that the `FLASH` length
//! in each `memory-*.x` equals [`FlashMap::linked_firmware`].

use esp_base::image::SECTOR;

/// Bytes the RP2040's second-stage bootloader takes at the start of flash.
pub const BOOT2_LEN: u32 = 0x100;

/// Bytes kept at the start of RP2350 firmware slot A for the partition
/// table the A/B firmware OTA work will write there.
pub const PARTITION_TABLE_RESERVE: u32 = 2 * SECTOR;

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
    /// RP2350: the image starts at the region start (its `IMAGE_DEF` comes
    /// first), capped [`PARTITION_TABLE_RESERVE`] short of the region end.
    ImageDef,
}

/// One board's flash map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlashMap {
    /// Total flash size.
    pub flash_size: u32,
    /// How the boot ROM finds the image.
    pub boot: Boot,
    /// The firmware region (slot A on the RP2350).
    pub firmware: Region,
    /// Firmware slot B (RP2350, reserved for A/B firmware OTA).
    pub firmware_b: Option<Region>,
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
    /// The firmware region does not start at offset 0.
    FirmwareNotFirst,
    /// The trust region is not the trust store's two sectors.
    TrustSize,
}

impl FlashMap {
    /// Every region, firmware first.
    pub fn regions(&self) -> impl Iterator<Item = Region> {
        [Some(self.firmware), self.firmware_b, Some(self.payload), Some(self.trust)]
            .into_iter()
            .flatten()
    }

    /// Bytes the linker may fill (the `FLASH` length of `memory-*.x`).
    pub const fn linked_firmware(&self) -> u32 {
        match self.boot {
            Boot::Boot2 => self.firmware.size.saturating_sub(BOOT2_LEN),
            Boot::ImageDef => self.firmware.size.saturating_sub(PARTITION_TABLE_RESERVE),
        }
    }

    /// Flash not covered by any region.
    pub fn unused(&self) -> u32 {
        let used = self.regions().fold(0u32, |n, r| n.saturating_add(r.size));
        self.flash_size.saturating_sub(used)
    }

    /// Checks the map: firmware at 0, every region aligned and inside the
    /// flash, no two overlapping, the trust region exactly
    /// [`esp_base::trust_store::PARTITION_LEN`].
    pub fn check(&self) -> Result<(), LayoutError> {
        if self.firmware.offset != 0 {
            return Err(LayoutError::FirmwareNotFirst);
        }
        if self.trust.size != esp_base::trust_store::PARTITION_LEN {
            return Err(LayoutError::TrustSize);
        }
        // At most four regions: every pair is compared once.
        let all: heapless::Vec<Region, 4> = self.regions().collect();
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
    firmware: Region::new(0x00_0000, 0x18_0000),
    firmware_b: None,
    payload: Region::new(0x18_0000, 0x4_0000),
    trust: Region::new(0x1C_0000, 0x2000),
};

/// Raspberry Pi Pico 2: RP2350A with 4 MiB of flash.
pub const RP2350: FlashMap = FlashMap {
    flash_size: 0x40_0000,
    boot: Boot::ImageDef,
    firmware: Region::new(0x00_0000, 0x18_0000),
    firmware_b: Some(Region::new(0x18_0000, 0x18_0000)),
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
    fn v2_rp2350_reserves_two_equal_firmware_slots() {
        let b = RP2350.firmware_b.unwrap();
        assert_eq!(b.size, RP2350.firmware.size);
        assert_eq!(b.offset, RP2350.firmware.end());
        assert!(RP2350.payload.offset >= b.end());
        assert_eq!(RP2040.firmware_b, None);
    }

    #[test]
    fn v2_memory_x_flash_lengths_match_the_maps() {
        assert_eq!(flash_length(include_str!("../memory-rp2040.x")), Some(RP2040.linked_firmware()));
        assert_eq!(flash_length(include_str!("../memory-rp2350.x")), Some(RP2350.linked_firmware()));
        assert_eq!(RP2040.linked_firmware(), 0x18_0000 - 0x100);
        assert_eq!(RP2350.linked_firmware(), 0x18_0000 - 0x2000);
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
        let slot_b = FlashMap { firmware_b: Some(Region::new(0x17_0000, 0x1_0000)), ..RP2350 };
        assert_eq!(slot_b.check(), Err(LayoutError::Overlap));
        assert!(!Region::new(0, 0).is_aligned());
    }
}
