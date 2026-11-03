//! UF2 files for the RP2350 (pure): the factory UF2 for BOOTSEL and the
//! update UF2 for `mwctl fw update`.
//!
//! A UF2 is a sequence of 512-byte blocks, each carrying 256 bytes for one
//! flash page: magic `0x0A324655`, `0x9E5D5157`, flags (`0x2000`: family id
//! present), target address, payload size 256, block number, block count,
//! family id, the payload, zeros, magic `0x0AB16F30`. The boot ROM erases
//! each 4 KiB sector the first time a download writes into it.
//!
//! * [`update_pages`]: the update image's pages at `0x1000_0000` (family
//!   `rp2350-arm-s`), what `mwctl` reassembles from the lowest address.
//! * [`factory_pages`]: the factory flash ([`super::factory::factory_flash`])
//!   at its absolute addresses (family `absolute`, so the boot ROM writes
//!   it where it says even over a partition table whose unpartitioned
//!   space accepts that family), plus one erased page at the start of
//!   slot B and of each update-record sector, so a board flashed before
//!   keeps no bootable image in B and no stale update record. The payload
//!   store and the trust record are not touched.

use crate::layout::FlashMap;
use esp_base::image::SECTOR;

/// Bytes per UF2 block.
pub const BLOCK: usize = 512;
/// Payload bytes per block (one flash page).
pub const PAGE: usize = 256;
/// The `absolute` family: written at its addresses.
pub const FAMILY_ABSOLUTE: u32 = 0xe48b_ff57;
/// The `rp2350-arm-s` family.
pub const FAMILY_RP2350_ARM_S: u32 = 0xe48b_ff59;
/// Flash offset 0 in the XIP address space.
pub const FLASH_BASE: u32 = 0x1000_0000;
const MAGIC_START0: u32 = 0x0A32_4655;
const MAGIC_START1: u32 = 0x9E5D_5157;
const MAGIC_END: u32 = 0x0AB1_6F30;
const FLAG_FAMILY_ID: u32 = 0x2000;

/// One page of a UF2: its flash offset and contents.
pub type Page = (u32, [u8; PAGE]);

/// Encodes page `index` of `count`, `page` at flash offset `offset`.
pub fn block(offset: u32, page: &[u8; PAGE], index: u32, count: u32, family: u32) -> [u8; BLOCK] {
    let mut b = [0u8; BLOCK];
    let words = [MAGIC_START0, MAGIC_START1, FLAG_FAMILY_ID, FLASH_BASE + offset, PAGE as u32, index, count, family];
    for (w, o) in words.iter().zip(b.as_chunks_mut::<4>().0) {
        *o = w.to_le_bytes();
    }
    b[32..32 + PAGE].copy_from_slice(page);
    b[BLOCK - 4..].copy_from_slice(&MAGIC_END.to_le_bytes());
    b
}

/// The pages of `image` from flash offset `base`, the last one padded with
/// `0xFF`.
pub fn pages(base: u32, image: &[u8]) -> impl Iterator<Item = Page> + Clone + '_ {
    image.chunks(PAGE).enumerate().map(move |(i, c)| {
        let mut p = [0xFFu8; PAGE];
        p[..c.len()].copy_from_slice(c);
        (base + (i * PAGE) as u32, p)
    })
}

/// The update image's pages (at offset 0: the partition the boot ROM or
/// `mwctl` puts it in).
pub fn update_pages(image: &[u8]) -> impl Iterator<Item = Page> + Clone + '_ {
    pages(0, image)
}

/// The factory flash's pages (see the module docs): `flash` is
/// [`super::factory::factory_flash`]'s output for `map`.
pub fn factory_pages<'a>(flash: &'a [u8], map: &FlashMap) -> impl Iterator<Item = Page> + Clone + 'a {
    let erased = map.ab().map(|ab| [ab.b.offset, ab.record.offset, ab.record.offset + SECTOR]);
    pages(0, flash).chain(erased.into_iter().flatten().map(|at| (at, [0xFFu8; PAGE])))
}

/// Writes `pages` as UF2 blocks of `family` into `out`, returning the
/// bytes written (`BLOCK` per page); `None` if `out` is too short.
pub fn write<I>(pages: I, family: u32, out: &mut [u8]) -> Option<usize>
where
    I: Iterator<Item = Page> + Clone,
{
    let count = u32::try_from(pages.clone().count()).ok()?;
    let mut n = 0usize;
    for (i, (offset, page)) in pages.enumerate() {
        let dst = out.get_mut(n..n + BLOCK)?;
        dst.copy_from_slice(&block(offset, &page, i as u32, count, family));
        n += BLOCK;
    }
    Some(n)
}
