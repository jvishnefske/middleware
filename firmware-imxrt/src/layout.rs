//! The HyperFlash map of the MIMXRT1050-EVKB: where the bootloader, the
//! update record, the two firmware slots, the trust record and the payload
//! store live, in whole 256 KiB erase blocks.
//!
//! Offsets are from the start of the HyperFlash (FlexSPI AHB address
//! `0x6000_0000`), the `embedded-storage` offsets of the HyperFlash driver.
//! The S26KS512S erases in uniform 256 KiB sectors, called *blocks* here so
//! they are not confused with the 4 KiB sectors of `middleware::fw_ota` and
//! `esp-base`; every region is a whole number of them ([`FlashMap::check`]),
//! so no erase of one region can touch another.
//!
//! | offset      | size    | blocks | region |
//! |-------------|---------|--------|--------|
//! | `0x000000`  | 256 KiB | 1      | boot: FCB, IVT, boot data, DCD, the bootloader (never written by OTA) |
//! | `0x040000`  | 512 KiB | 2      | update record: two blocks written alternately (`crate::ab::record`) |
//! | `0x0C0000`  | 512 KiB | 2      | trust record (`esp_base::trust_store`, through [`SectorPerBlock`]) |
//! | `0x140000`  | 512 KiB | 2      | payload store (`esp_base::store`, through [`BlockErase`]; 124 KiB images) |
//! | `0x1C0000`  | 2304 KiB| 9      | unused |
//! | `0x400000`  | 4 MiB   | 16     | firmware slot A: slot image header, then the SDRAM image |
//! | `0x800000`  | 4 MiB   | 16     | firmware slot B |
//! | `0xC00000`  | 52 MiB  | 208    | unused |
//!
//! The slots are 4 MiB (the firmware is about 630 KB, so about six times
//! its size) and start on 4 MiB boundaries, so a slot's AHB address is
//! easy to read (`0x6040_0000`, `0x6080_0000`); 4 MiB is also
//! `middleware::fw_ota::MAX_IMAGE`, the largest image the updater accepts.
//!
//! The ESP-derived stores erase 4 KiB sectors; the [`erase`] adapters map
//! them onto whole blocks:
//!
//! * the trust store writes one record per sector at sector offsets 0 and
//!   4 KiB; [`SectorPerBlock`] gives each 4 KiB logical sector its own
//!   block, so erasing one record's sector erases only that block;
//! * the payload store erases a slot's header sector and image sectors as
//!   one span starting at the slot's start; with a two-block region each
//!   payload slot is exactly one block, and [`BlockErase`] erases the
//!   blocks a span starting on a block boundary covers. The slot's own
//!   capacity (252 KiB) is capped at [`PAYLOAD_CAPACITY`], the 124 KiB
//!   the ESP and RP base firmwares report, by the store's `limit`.
//!
//! The firmware runs from SDRAM: the bootloader copies a slot's image to
//! [`SDRAM_IMAGE`] (the firmware's link address) before checking it.

pub mod erase;

pub use erase::{BlockErase, SectorPerBlock};

/// The HyperFlash's uniform erase block (S26KS512S sector), 256 KiB.
pub const BLOCK: u32 = 0x4_0000;

/// The HyperFlash's size, 64 MiB.
pub const FLASH_SIZE: u32 = 0x400_0000;

/// The HyperFlash's AHB (memory-mapped) base address.
pub const FLEXSPI_AHB_BASE: u32 = 0x6000_0000;

/// The 4 KiB sector of `middleware::fw_ota` and `esp-base`.
pub const SECTOR: u32 = 4096;

/// Payload image capacity reported by `sys/info`: the ESP partition's
/// 124 KiB, passed as the payload store's `limit`.
pub const PAYLOAD_CAPACITY: u32 = 124 * 1024;

/// The boot header's end, where the bootloader's image starts (FCB at 0,
/// IVT at `0x1000`, the bootloader from `0x2000`).
pub const BOOTLOADER_OFFSET: u32 = 0x2000;

/// A span of SDRAM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RamRegion {
    /// Start address.
    pub base: u32,
    /// Length in bytes.
    pub size: u32,
}

/// Where the bootloader copies a slot's image, and the firmware is linked
/// to run: the first 4 MiB of the EVKB's 32 MiB SDRAM. The firmware's own
/// RAM (heap, statics not in DTCM) follows it, in the linker layout.
pub const SDRAM_IMAGE: RamRegion = RamRegion {
    base: 0x8000_0000,
    size: 0x40_0000,
};

/// A block-aligned span of flash.
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

    /// The region of `count` blocks from block `first`.
    pub const fn blocks(first: u32, count: u32) -> Self {
        Region {
            offset: first * BLOCK,
            size: count * BLOCK,
        }
    }

    /// One past the last byte (saturating).
    pub const fn end(&self) -> u32 {
        self.offset.saturating_add(self.size)
    }

    /// Whole blocks.
    pub const fn block_count(&self) -> u32 {
        self.size / BLOCK
    }

    /// Whether offset and size are whole blocks and the region is not
    /// empty.
    pub const fn is_aligned(&self) -> bool {
        self.size > 0 && self.offset.is_multiple_of(BLOCK) && self.size.is_multiple_of(BLOCK)
    }

    /// Whether the two regions share a byte.
    pub const fn overlaps(&self, other: &Region) -> bool {
        self.offset < other.end() && other.offset < self.end()
    }
}

/// The A/B part of the map: what the bootloader and the updater address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AbMap {
    /// Slot A.
    pub a: Region,
    /// Slot B.
    pub b: Region,
    /// The update record's two blocks.
    pub record: Region,
}

impl AbMap {
    /// The region of `slot`.
    pub const fn slot(&self, slot: middleware::fw_ota::Slot) -> Region {
        match slot {
            middleware::fw_ota::Slot::A => self.a,
            middleware::fw_ota::Slot::B => self.b,
        }
    }

    /// The size of each slot (both are equal in a checked map).
    pub const fn slot_size(&self) -> u32 {
        self.a.size
    }
}

/// The board's flash map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlashMap {
    /// Total flash size.
    pub flash_size: u32,
    /// Block 0: boot header and bootloader.
    pub boot: Region,
    /// The update record (two blocks).
    pub record: Region,
    /// Firmware slot A.
    pub slot_a: Region,
    /// Firmware slot B (the size of slot A).
    pub slot_b: Region,
    /// The trust record (two blocks, one per trust-store sector).
    pub trust: Region,
    /// The payload store (two blocks, one per payload slot).
    pub payload: Region,
}

/// Why a flash map is invalid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutError {
    /// A region is empty or not whole blocks.
    Unaligned,
    /// A region ends beyond the flash.
    OutOfFlash,
    /// Two regions overlap.
    Overlap,
    /// The boot region is not exactly block 0.
    BootNotBlockZero,
    /// The update record or the trust record is not two blocks.
    RecordSize,
    /// The slots differ in size, or are smaller than the image region they
    /// must fill or larger than the updater accepts.
    SlotShape,
    /// The payload store is not two blocks (one per payload slot).
    PayloadShape,
}

impl FlashMap {
    /// Every region, in flash order for the shipped map.
    pub const fn regions(&self) -> [Region; 6] {
        [
            self.boot,
            self.record,
            self.trust,
            self.payload,
            self.slot_a,
            self.slot_b,
        ]
    }

    /// The A/B part.
    pub const fn ab(&self) -> AbMap {
        AbMap {
            a: self.slot_a,
            b: self.slot_b,
            record: self.record,
        }
    }

    /// Flash not covered by any region.
    pub fn unused(&self) -> u32 {
        let used = self
            .regions()
            .iter()
            .fold(0u32, |n, r| n.saturating_add(r.size));
        self.flash_size.saturating_sub(used)
    }

    /// Checks the map: every region whole blocks, inside the flash and
    /// free of overlaps; block 0 for the boot header and bootloader; the
    /// update and trust records two blocks each; the payload store two
    /// blocks; two equal slots no larger than
    /// `middleware::fw_ota::MAX_IMAGE`.
    pub fn check(&self) -> Result<(), LayoutError> {
        let all = self.regions();
        // Six regions: every pair is compared once.
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
        if self.boot != Region::blocks(0, 1) {
            return Err(LayoutError::BootNotBlockZero);
        }
        if self.record.block_count() != 2 || self.trust.block_count() != 2 {
            return Err(LayoutError::RecordSize);
        }
        if self.slot_a.size != self.slot_b.size || self.slot_a.size > middleware::fw_ota::MAX_IMAGE
        {
            return Err(LayoutError::SlotShape);
        }
        if self.payload.block_count() != 2 {
            return Err(LayoutError::PayloadShape);
        }
        Ok(())
    }
}

/// The MIMXRT1050-EVKB's 64 MiB HyperFlash (S26KS512S). See the module
/// docs for the table.
pub const RT1050_EVKB: FlashMap = FlashMap {
    flash_size: FLASH_SIZE,
    boot: Region::blocks(0, 1),
    record: Region::blocks(1, 2),
    trust: Region::blocks(3, 2),
    payload: Region::blocks(5, 2),
    slot_a: Region::blocks(16, 16),
    slot_b: Region::blocks(32, 16),
};

#[cfg(test)]
mod tests {
    use super::*;
    use esp_base::image::Region as Slots;

    #[test]
    fn rt3_the_evkb_map_is_whole_blocks_inside_the_flash_without_overlap() {
        let m = RT1050_EVKB;
        assert_eq!(m.check(), Ok(()));
        assert_eq!(m.flash_size / BLOCK, 256, "256 blocks of 256 KiB");
        for r in m.regions() {
            assert!(r.is_aligned() && r.end() <= m.flash_size, "{r:?}");
        }
        assert_eq!(m.boot, Region::new(0, BLOCK));
        const { assert!(BOOTLOADER_OFFSET < BLOCK) };
        assert_eq!(m.record, Region::new(0x4_0000, 2 * BLOCK));
        assert_eq!(m.trust, Region::new(0xC_0000, 2 * BLOCK));
        assert_eq!(m.payload, Region::new(0x14_0000, 2 * BLOCK));
        assert_eq!(m.slot_a, Region::new(0x40_0000, 0x40_0000));
        assert_eq!(m.slot_b, Region::new(0x80_0000, 0x40_0000));
        assert_eq!(m.ab().slot_size(), 4 * 1024 * 1024);
        assert_eq!(m.ab().slot_size(), middleware::fw_ota::MAX_IMAGE);
        assert_eq!(m.unused(), (256 - 39) * BLOCK);
    }

    #[test]
    fn rt3_the_payload_store_keeps_the_esp_capacity_one_block_per_slot() {
        let slots = Slots::new(RT1050_EVKB.payload.size);
        // Each payload slot is exactly one erase block, so its erase never
        // touches the other slot.
        assert_eq!(slots.slot_size(), BLOCK);
        assert!(slots.capacity() >= PAYLOAD_CAPACITY);
        assert_eq!(PAYLOAD_CAPACITY, 124 * 1024);
        // The trust store's two sectors map onto the trust region's blocks.
        assert_eq!(
            esp_base::trust_store::PARTITION_LEN,
            RT1050_EVKB.trust.block_count() * SECTOR
        );
    }

    #[test]
    fn rt3_the_slots_hold_a_firmware_with_room_to_grow_and_fit_the_sdram_region() {
        let slot = RT1050_EVKB.ab().slot_size();
        // The firmware is about 630 KB today.
        assert!(slot >= 6 * 630 * 1000);
        assert!(
            slot <= SDRAM_IMAGE.size,
            "every slot image fits the SDRAM copy"
        );
        assert_eq!(SDRAM_IMAGE.base, 0x8000_0000);
    }

    #[test]
    fn rt3_check_refuses_broken_maps() {
        let m = RT1050_EVKB;
        let unaligned = FlashMap {
            trust: Region::new(0xC_1000, 2 * BLOCK),
            ..m
        };
        assert_eq!(unaligned.check(), Err(LayoutError::Unaligned));
        let empty = FlashMap {
            payload: Region::new(0x14_0000, 0),
            ..m
        };
        assert_eq!(empty.check(), Err(LayoutError::Unaligned));
        let beyond = FlashMap {
            slot_b: Region::blocks(250, 16),
            ..m
        };
        assert_eq!(beyond.check(), Err(LayoutError::OutOfFlash));
        let overlap = FlashMap {
            slot_b: Region::blocks(20, 16),
            ..m
        };
        assert_eq!(overlap.check(), Err(LayoutError::Overlap));
        let into_boot = FlashMap {
            record: Region::blocks(0, 2),
            ..m
        };
        assert_eq!(into_boot.check(), Err(LayoutError::Overlap));
        let late_boot = FlashMap {
            boot: Region::blocks(7, 1),
            ..m
        };
        assert_eq!(late_boot.check(), Err(LayoutError::BootNotBlockZero));
        let big_boot = FlashMap {
            boot: Region::blocks(0, 1),
            record: Region::blocks(1, 1),
            ..m
        };
        assert_eq!(big_boot.check(), Err(LayoutError::RecordSize));
        let trust = FlashMap {
            trust: Region::blocks(3, 1),
            ..m
        };
        assert_eq!(trust.check(), Err(LayoutError::RecordSize));
        let unequal = FlashMap {
            slot_b: Region::blocks(32, 8),
            ..m
        };
        assert_eq!(unequal.check(), Err(LayoutError::SlotShape));
        let huge = FlashMap {
            slot_a: Region::blocks(16, 32),
            slot_b: Region::blocks(48, 32),
            ..m
        };
        assert_eq!(huge.check(), Err(LayoutError::SlotShape));
        let payload = FlashMap {
            payload: Region::blocks(5, 1),
            ..m
        };
        assert_eq!(payload.check(), Err(LayoutError::PayloadShape));
    }

    /// `ORIGIN` and `LENGTH` of the MEMORY region `name` in a linker script.
    fn region(script: &str, name: &str) -> (u32, u32) {
        let line = script
            .lines()
            .find(|l| l.trim_start().starts_with(name) && l.contains("ORIGIN"))
            .unwrap_or_else(|| panic!("no MEMORY region {name}"));
        let field = |key: &str| {
            let v = line.split(key).nth(1).unwrap().trim_start_matches([' ', '=']);
            let v = v.split([',', ' ']).next().unwrap();
            if let Some(hex) = v.strip_prefix("0x") {
                u32::from_str_radix(hex, 16).unwrap()
            } else if let Some(m) = v.strip_suffix('M') {
                m.parse::<u32>().unwrap() * 1024 * 1024
            } else if let Some(k) = v.strip_suffix('K') {
                k.parse::<u32>().unwrap() * 1024
            } else {
                v.parse().unwrap()
            }
        };
        (field("ORIGIN"), field("LENGTH"))
    }

    #[test]
    fn rt3_the_link_layouts_match_the_map() {
        let m = RT1050_EVKB;
        // The firmware runs from the SDRAM image region, one slot's size.
        let app = include_str!("../memory-app.x");
        assert_eq!(region(app, "FLASH"), (SDRAM_IMAGE.base, SDRAM_IMAGE.size));
        assert_eq!(SDRAM_IMAGE.size, m.slot_a.size, "the image region holds one slot");
        assert_eq!(m.slot_a.size, m.slot_b.size);
        // The bootloader's APP_IMAGE is the same region.
        let boot = include_str!("../boot-image.x");
        assert_eq!(region(boot, "SDRAM_IMAGE"), (SDRAM_IMAGE.base, SDRAM_IMAGE.size));
        // The bootloader is linked into the boot block (imxrt-rt's FLASH
        // region, so a bootloader that outgrows it fails to link: RT9).
        let build = include_str!("../build.rs");
        extern crate std;
        let boot_size = std::format!("from_flexspi(imxrt_rt::Family::Imxrt1050, {} * 1024)", m.boot.size / 1024);
        assert!(build.contains(&boot_size), "build.rs must link the bootloader into {} bytes", m.boot.size);
        assert_eq!((m.boot.offset, m.boot.size), (0, BLOCK));
    }
}
