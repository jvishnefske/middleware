//! The RP2350 partition table: slots A and B as an A/B pair.
//!
//! The table is one metadata block at flash offset 0 (the boot ROM looks
//! for it in the first 4 KiB of flash; RP2350 datasheet §5.9.4 "Partition
//! tables", encodings from the pico-sdk's `boot/picobin.h` and picotool's
//! `bintool/metadata.h`), built here from the flash map
//! ([`partition_table`]) rather than from a picotool JSON file, so the
//! table, the linker script and the updater share one map:
//!
//! ```text
//! START
//! PARTITION_TABLE item: 0x0a | size << 8 | 2 << 24   (two partitions, not singleton)
//!   unpartitioned space: permissions all | accepts the "absolute" UF2 family
//!   partition 0 "A": sectors 0x002..=0x17f, permissions all, accepts rp2350-arm-s
//!   partition 1 "B": sectors 0x180..=0x2fd, permissions all, accepts rp2350-arm-s,
//!                    link type "A partition", link value 0 (B belongs to A)
//! LAST(size), link 0 (a loop of its own), END
//! ```
//!
//! Each partition is two words (permissions and location: first and last
//! sector; permissions and flags) and a name word (`len`, then the name's
//! bytes). Permissions (secure, non-secure and bootloader read and write)
//! are granted everywhere: they only govern the boot ROM's own flash APIs
//! and picotool, and the firmware writes flash through the ROM's raw erase
//! and program functions. The unpartitioned space (the update record, the
//! payload store, the trust record) accepts absolute UF2 downloads, so the
//! factory UF2 can be loaded again over a partitioned board.

use crate::layout::{AbMap, Region};
use esp_base::image::SECTOR;

/// `PICOBIN_BLOCK_ITEM_PARTITION_TABLE`.
pub const ITEM_PARTITION_TABLE: u32 = 0x0a;
/// All six permission bits (S, NS, NSBOOT read and write).
pub const PERMISSIONS_ALL: u32 = 0x3f << 26;
/// `PICOBIN_PARTITION_FLAGS_LINK_TYPE_A_PARTITION`, in place.
pub const LINK_A_PARTITION: u32 = 1 << 1;
/// `PICOBIN_PARTITION_FLAGS_HAS_NAME_BITS`.
pub const HAS_NAME: u32 = 0x1000;
/// `PICOBIN_PARTITION_FLAGS_ACCEPTS_DEFAULT_FAMILY_ABSOLUTE_BITS`.
pub const ACCEPTS_ABSOLUTE: u32 = 0x8000;
/// `PICOBIN_PARTITION_FLAGS_ACCEPTS_DEFAULT_FAMILY_RP2350_ARM_S_BITS`.
pub const ACCEPTS_RP2350_ARM_S: u32 = 0x2_0000;
/// Words of [`partition_table`].
pub const TABLE_WORDS: usize = 12;

const BLOCK_START: u32 = 0xffff_ded3;
const BLOCK_END: u32 = 0xab12_3579;

/// The permissions-and-location word of a partition covering `r`.
const fn location(r: Region) -> u32 {
    let first = r.offset / SECTOR;
    let last = (r.offset + r.size) / SECTOR - 1;
    PERMISSIONS_ALL | (first & 0x1fff) | (last & 0x1fff) << 13
}

/// A one-letter name word: length 1, then the letter.
const fn name(letter: u8) -> u32 {
    1 | (letter as u32) << 8
}

/// The partition table block for `ab` (see the module docs). It never
/// changes with an update: only the factory flashing writes it.
pub const fn partition_table(ab: &AbMap) -> [u32; TABLE_WORDS] {
    let item_words = 2 + 3 + 3;
    [
        BLOCK_START,
        ITEM_PARTITION_TABLE | (item_words as u32) << 8 | 2 << 24,
        PERMISSIONS_ALL | ACCEPTS_ABSOLUTE,
        location(ab.a),
        PERMISSIONS_ALL | ACCEPTS_RP2350_ARM_S | HAS_NAME,
        name(b'A'),
        location(ab.b),
        PERMISSIONS_ALL | ACCEPTS_RP2350_ARM_S | HAS_NAME | LINK_A_PARTITION,
        name(b'B'),
        0xff | (item_words as u32) << 8,
        0,
        BLOCK_END,
    ]
}

/// [`partition_table`] as little-endian bytes.
pub fn partition_table_bytes(ab: &AbMap) -> [u8; TABLE_WORDS * 4] {
    let mut out = [0u8; TABLE_WORDS * 4];
    for (w, o) in partition_table(ab).iter().zip(out.as_chunks_mut::<4>().0) {
        *o = w.to_le_bytes();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::RP2350;

    /// One decoded partition: first and last sector, flags, name length.
    #[derive(Debug, PartialEq, Eq)]
    struct Part {
        first: u32,
        last: u32,
        flags: u32,
        name: (u8, u8),
    }

    /// Decodes the table as picotool's `partition_table_item::parse` does,
    /// checking the block framing and the permission duplication.
    fn decode(w: &[u32]) -> (u32, [Part; 2]) {
        assert_eq!((w[0], w[w.len() - 1]), (BLOCK_START, BLOCK_END));
        let header = w[1];
        assert_eq!(header & 0x7f, ITEM_PARTITION_TABLE, "1-byte-size item");
        let size = (header >> 8) & 0xff;
        assert_eq!((header >> 24) & 0x0f, 2, "partition count");
        assert_eq!(header >> 31, 0, "not singleton");
        let last = w[1 + size as usize];
        assert_eq!(last, 0xff | size << 8, "LAST covers the item");
        assert_eq!(w[2 + size as usize], 0, "a loop of its own");
        let part = |i: usize| {
            let (loc, flags, nm) = (w[i], w[i + 1], w[i + 2]);
            assert_eq!(loc & PERMISSIONS_ALL, flags & PERMISSIONS_ALL, "permissions duplicated");
            Part {
                first: loc & 0x1fff,
                last: (loc >> 13) & 0x1fff,
                flags: flags & !PERMISSIONS_ALL,
                name: ((nm & 0xff) as u8, (nm >> 8) as u8),
            }
        };
        (w[2], [part(3), part(6)])
    }

    #[test]
    fn y13_partition_table_makes_slots_a_and_b_an_ab_pair() {
        let ab = RP2350.ab().unwrap();
        let words = partition_table(&ab);
        let (unpartitioned, [a, b]) = decode(&words);
        assert_eq!(unpartitioned, PERMISSIONS_ALL | ACCEPTS_ABSOLUTE);
        assert_eq!(a, Part { first: 0x002, last: 0x17f, flags: ACCEPTS_RP2350_ARM_S | HAS_NAME, name: (1, b'A') });
        assert_eq!(
            b,
            Part { first: 0x180, last: 0x2fd, flags: ACCEPTS_RP2350_ARM_S | HAS_NAME | LINK_A_PARTITION, name: (1, b'B') }
        );
        // B links to partition 0 (link value bits 3..6 are zero).
        assert_eq!((b.flags >> 3) & 0xf, 0);
        // The partitions are the map's slots, sector for sector, and the
        // table fits the region reserved for it.
        assert_eq!(a.first * SECTOR, ab.a.offset);
        assert_eq!((a.last + 1) * SECTOR, ab.a.end());
        assert_eq!((b.first * SECTOR, (b.last + 1) * SECTOR), (ab.b.offset, ab.b.end()));
        assert!(TABLE_WORDS * 4 <= RP2350.partition_table.unwrap().size as usize);
        // The update record lies in unpartitioned space.
        assert!(ab.record.offset >= ab.b.end());
        let bytes = partition_table_bytes(&ab);
        assert_eq!(&bytes[..4], &BLOCK_START.to_le_bytes());
        assert_eq!(&bytes[44..], &BLOCK_END.to_le_bytes());
    }
}
