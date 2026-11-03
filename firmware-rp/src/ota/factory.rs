//! Image checks and the factory flash image (pure; used by the device's
//! buy and by the host tool `rp-image`).
//!
//! * [`check_update`] refuses an image that `mwctl fw update` and the
//!   device would refuse (the checks of `mwctl::fw::load_image` and of
//!   `FwUpdate`'s commit), plus one that does not fit a slot.
//! * [`clear_tbyb`] clears the TBYB bit of an image's IMAGE_TYPE item in
//!   a copy of its first sector: what the boot ROM's explicit buy does, and
//!   how the factory image is made from the update image (the two differ
//!   in that one bit).
//! * [`factory_flash`] lays out the flash a new board is programmed with
//!   over BOOTSEL: the partition table ([`super::table`]) at offset 0 and
//!   the update image, bought, at slot A, gaps `0xFF`. Slot B stays
//!   erased, so the boot ROM boots A normally and the first update goes
//!   into B.

use middleware::fw_ota::picobin::{ITEM_IMAGE_TYPE, ImageDef, SENTINEL_FROM, find_image_def_in};

use super::image::TBYB;
use super::table::partition_table_bytes;
use crate::layout::FlashMap;

/// Why an image cannot be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactoryError {
    /// No valid block loop with an IMAGE_DEF in the first 4 KiB.
    NoImageDef,
    /// Not an RP2350 executable.
    NotRp2350Exe,
    /// No VERSION item.
    NoVersion,
    /// A rollback version (OTP) is present.
    Rollback,
    /// The IMAGE_DEF is not flagged try-before-you-buy.
    NotTbyb,
    /// No block of the loop in the first sector's last page.
    NoSentinel,
    /// Larger than one slot.
    TooLarge {
        /// Image length.
        len: usize,
        /// Slot size.
        slot: u32,
    },
    /// The IMAGE_TYPE item is not where the IMAGE_DEF says, or not in the
    /// buffer.
    TypeItem,
    /// The map has no A/B slots, or the output buffer is too small.
    Layout,
}

/// Checks an update image as the device and `mwctl` do; its IMAGE_DEF.
pub fn check_update(image: &[u8], slot_size: u32) -> Result<ImageDef, FactoryError> {
    if image.len() > slot_size as usize {
        return Err(FactoryError::TooLarge { len: image.len(), slot: slot_size });
    }
    let def = find_image_def_in(image).ok_or(FactoryError::NoImageDef)?;
    if !def.is_exe() || !def.is_rp2350() {
        return Err(FactoryError::NotRp2350Exe);
    }
    let v = def.version.ok_or(FactoryError::NoVersion)?;
    if v.rollback.is_some() {
        return Err(FactoryError::Rollback);
    }
    if !def.is_tbyb() {
        return Err(FactoryError::NotTbyb);
    }
    if def.window_tail < SENTINEL_FROM {
        return Err(FactoryError::NoSentinel);
    }
    Ok(def)
}

/// Clears the TBYB bit of `def`'s IMAGE_TYPE item in `first`, a copy of
/// the image's first bytes (at least up to the item). Idempotent.
pub fn clear_tbyb(first: &mut [u8], def: &ImageDef) -> Result<(), FactoryError> {
    let at = def.type_offset as usize;
    let item = first.get_mut(at..at + 4).ok_or(FactoryError::TypeItem)?;
    let mut w = u32::from_le_bytes([item[0], item[1], item[2], item[3]]);
    if (w & 0xff) as u8 != ITEM_IMAGE_TYPE || (w >> 16) as u16 != def.flags {
        return Err(FactoryError::TypeItem);
    }
    w &= !(u32::from(TBYB) << 16);
    item.copy_from_slice(&w.to_le_bytes());
    Ok(())
}

/// Bytes [`factory_flash`] writes for an `image_len`-byte update image.
pub fn factory_len(map: &FlashMap, image_len: usize) -> Option<usize> {
    let ab = map.ab()?;
    (ab.a.offset as usize).checked_add(image_len)
}

/// Writes the factory flash contents for `update` into `out` (from flash
/// offset 0): the partition table, `0xFF` up to slot A, the image with
/// TBYB cleared. Returns the length written ([`factory_len`]).
pub fn factory_flash(update: &[u8], map: &FlashMap, out: &mut [u8]) -> Result<usize, FactoryError> {
    let ab = map.ab().ok_or(FactoryError::Layout)?;
    let pt = map.partition_table.ok_or(FactoryError::Layout)?;
    let def = check_update(update, ab.a.size)?;
    let len = factory_len(map, update.len()).ok_or(FactoryError::Layout)?;
    let out = out.get_mut(..len).ok_or(FactoryError::Layout)?;
    out.fill(0xFF);
    let table = partition_table_bytes(&ab);
    if table.len() > pt.size as usize {
        return Err(FactoryError::Layout);
    }
    out.get_mut(..table.len()).ok_or(FactoryError::Layout)?.copy_from_slice(&table);
    let slot = out.get_mut(ab.a.offset as usize..).ok_or(FactoryError::Layout)?;
    slot.copy_from_slice(update);
    clear_tbyb(slot, &def)?;
    Ok(len)
}
