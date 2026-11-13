//! The hardware seam: what the RP2350 integration implements.
//!
//! [`FwPlatform`] is **blocking**. On the RP2350 every flash program or
//! erase runs with XIP disabled (embassy-rp's `Flash::blocking_erase` /
//! `blocking_write`, or the bootrom's `flash_op`), so there is nothing to
//! await, and the bootrom calls (`get_sys_info`, `reboot`, `explicit_buy`)
//! are synchronous; the serial dispatcher handles one request at a time, so
//! a blocking handler only delays that request's reply. A 4 KiB sector
//! erase takes tens of milliseconds (hundreds at worst), so `sys/fw/begin`
//! and the first chunk of each sector answer later than the others; the
//! host allows for it.
//!
//! Slots are addressed by byte offset from the start of their partition;
//! the integration maps them to flash through the partition table. The
//! exact contract each method must meet, and the bootrom calls behind it,
//! are in the method docs and in design.md "A/B firmware OTA (RP2350)".

use super::Slot;
use super::record::FwRecord;

/// What the bootrom says about this boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootInfo {
    /// The slot (partition of the A/B pair) the bootrom booted.
    pub running: Slot,
    /// A buy is pending: this is a try-before-you-buy boot of a TBYB image
    /// (`BOOT_TBYB_AND_UPDATE_FLAG_BUY_PENDING` in the boot info's
    /// `tbyb_and_update_info`).
    pub trial: bool,
}

/// The RP2350 operations the firmware updater needs.
///
/// Every method returns `Err` instead of panicking; the updater answers a
/// failed request with `Internal` and leaves its state recoverable.
pub trait FwPlatform {
    /// Platform failure.
    type Error: core::fmt::Debug;

    /// This boot: the booted partition of the A/B pair ([`Slot::A`] or
    /// [`Slot::B`]) and whether a buy is pending ([`BootInfo::trial`]), the
    /// facts `rom_get_sys_info(SYS_INFO_BOOT_INFO)` reports (partition
    /// number, bit 0 of `tbyb_and_update_info`). The RP base firmware
    /// derives them without that `unsafe` call: the partition the ROM maps
    /// at `0x1000_0000` (QMI `ATRANS0`) and the running image's own TBYB
    /// flag (`rp_base::ota::rom`).
    fn boot_info(&mut self) -> Result<BootInfo, Self::Error>;

    /// The size of each slot's partition in bytes (both are equal). Images
    /// up to this size (and [`super::MAX_IMAGE`]) are accepted.
    fn slot_size(&self) -> u32;

    /// Reads `buf.len()` bytes at `offset` of `slot`. Called for any
    /// word-aligned offset below the slot size, including the running slot.
    fn read(&mut self, slot: Slot, offset: u32, buf: &mut [u8]) -> Result<(), Self::Error>;

    /// Erases sector `sector` ([`super::SECTOR`] bytes at
    /// `sector * SECTOR`) of `slot` to `0xFF`. Never called for the running
    /// slot.
    fn erase(&mut self, slot: Slot, sector: u32) -> Result<(), Self::Error>;

    /// Programs `data` at `offset` of `slot`; the bytes were erased since
    /// they were last programmed. `offset` is a multiple of
    /// [`super::CHUNK_LEN`] and `data` at most [`super::SECTOR`] bytes (the
    /// integration pads to the flash's 256-byte pages as needed). Never
    /// called for the running slot.
    fn write(&mut self, slot: Slot, offset: u32, data: &[u8]) -> Result<(), Self::Error>;

    /// The persisted record, [`FwRecord::EMPTY`] when none was ever saved
    /// (see [`super::record`] for a format).
    fn load_record(&mut self) -> Result<FwRecord, Self::Error>;

    /// Saves `record` atomically: after a power cut `load_record` returns
    /// either the previous record or this one.
    fn persist(&mut self, record: &FwRecord) -> Result<(), Self::Error>;

    /// Schedules a flash-update reboot into `slot`:
    /// `rom_reboot(REBOOT_TYPE_FLASH_UPDATE, delay_ms, p0 = flash start of
    /// slot's partition, 0)` with a delay long enough for the reply to
    /// leave (100 ms), then returns `Ok`. The bootrom then boots `slot`
    /// even though it is TBYB, on trial.
    fn reboot_flash_update(&mut self, slot: Slot) -> Result<(), Self::Error>;

    /// Schedules a normal reboot (`REBOOT_TYPE_NORMAL`, same delay); the
    /// bootrom skips an unbought TBYB image and boots the other slot.
    fn reboot_normal(&mut self) -> Result<(), Self::Error>;

    /// Buys the running trial image: clears its TBYB flag by rewriting the
    /// image's first sector (what `rom_explicit_buy` does with a 4 KiB work
    /// buffer; the RP base firmware rewrites the sector itself through its
    /// flash driver, `rp_base::ota::platform`), then stops the bootrom's
    /// trial watchdog from resetting the chip. Only called on a trial boot.
    fn explicit_buy(&mut self) -> Result<(), Self::Error>;
}
