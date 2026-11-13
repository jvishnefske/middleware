//! The boot ROM seam: what the updater asks of the RP2350 boot ROM and
//! the chip, as a trait the shell implements ([`Rom`]) and the host tests
//! model.
//!
//! The shell's implementation (`src/shell/rom.rs`) makes no `unsafe`
//! call, and no ROM call other than `reboot`:
//!
//! * [`Rom::xip_map`] reads which flash the boot ROM mapped at
//!   `0x1000_0000` from the QMI's `ATRANS0` register (embassy-rp's PAC, a
//!   safe register read). The ROM maps the partition it booted there, so
//!   its base is the running slot's offset ([`running_slot`]);
//! * [`Rom::reboot`] is the boot ROM's `reboot` (embassy-rp's
//!   `rom_data::reboot`, a safe function), asynchronous so the reply
//!   leaves first;
//! * [`Rom::stop_watchdog`] is embassy-rp's `Watchdog::stop`;
//! * [`Rom::uninterrupted`] is `cortex_m::interrupt::free`.
//!
//! Two ROM calls of the design are replaced, because embassy-rp only
//! exposes them as `unsafe fn` (raw buffer pointers) and the one crate with
//! a safe `get_sys_info` wrapper, rp235x-hal, cannot be linked beside
//! embassy-rp (both define the interrupt vector table, `__INTERRUPTS`):
//!
//! * `get_sys_info(SYS_INFO_BOOT_INFO)`: the booted partition comes from
//!   `ATRANS0` (above), and "buy pending" is the running image's own TBYB
//!   flag in flash. The ROM launches a TBYB image only on a flash-update
//!   boot of its partition (anything else is an image condition failure),
//!   and the buy clears the flag, so a running image still flagged TBYB is
//!   exactly a trial ([`crate::ota::platform`]);
//! * `explicit_buy`: its only effect that applies here, clearing the TBYB
//!   bit by rewriting the image's first sector, is done with embassy-rp's
//!   safe blocking flash. Its other effects do not apply: the downgrade
//!   erase of the other slot (versions only grow) and the OTP rollback
//!   version (images carry none; the chip is not in secure boot).

use crate::layout::AbMap;
use middleware::fw_ota::Slot;

/// The XIP address of flash offset 0.
pub const XIP_BASE: u32 = 0x1000_0000;
/// Delay of every reboot: long enough for the reply to leave over USB.
pub const REBOOT_DELAY_MS: u32 = 100;
/// `REBOOT2_FLAG_REBOOT_TYPE_NORMAL`.
pub const REBOOT_TYPE_NORMAL: u32 = 0x0;
/// `REBOOT2_FLAG_REBOOT_TYPE_FLASH_UPDATE`.
pub const REBOOT_TYPE_FLASH_UPDATE: u32 = 0x4;

/// What the QMI maps at `0x1000_0000` (address translation window 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XipMap {
    /// The physical flash offset mapped at `0x1000_0000` (`ATRANS0.BASE`
    /// sectors).
    pub base: u32,
    /// The mapped size in bytes (`ATRANS0.SIZE` sectors).
    pub size: u32,
}

/// The slot the boot ROM booted: the slot whose partition starts at the
/// translated base. `None` for any other base, such as 0 on a board
/// without a partition table (no translation).
pub const fn running_slot(ab: &AbMap, xip: XipMap) -> Option<Slot> {
    if xip.base == ab.a.offset {
        Some(Slot::A)
    } else if xip.base == ab.b.offset {
        Some(Slot::B)
    } else {
        None
    }
}

/// A reboot the updater requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reboot {
    /// A normal boot: the ROM boots the bought image with the higher
    /// version and skips an unbought TBYB image.
    Normal,
    /// A flash-update boot of the region starting at XIP address `start`
    /// (a partition's first byte at `0x1000_0000 + offset`, as picotool
    /// passes it after a load): the ROM prefers that partition and boots a
    /// TBYB image there on trial.
    FlashUpdate {
        /// XIP address of the partition start.
        start: u32,
    },
}

impl Reboot {
    /// The ROM `reboot` arguments `(flags, p0, p1)`.
    pub const fn args(self) -> (u32, u32, u32) {
        match self {
            Reboot::Normal => (REBOOT_TYPE_NORMAL, 0, 0),
            Reboot::FlashUpdate { start } => (REBOOT_TYPE_FLASH_UPDATE, start, 0),
        }
    }
}

/// The boot ROM and chip services the updater needs.
pub trait Rom {
    /// A ROM call failed (the ROM's negative status).
    type Error: core::fmt::Debug;

    /// What the boot ROM mapped at `0x1000_0000` for this boot.
    fn xip_map(&mut self) -> XipMap;

    /// Schedules `reboot` in `delay_ms` and returns.
    fn reboot(&mut self, reboot: Reboot, delay_ms: u32) -> Result<(), Self::Error>;

    /// Stops the watchdog (the ROM's trial watchdog, once bought).
    fn stop_watchdog(&mut self);

    /// Runs `f` with interrupts masked on this core, so no vector is
    /// fetched while the running image's first sector is rewritten.
    fn uninterrupted<T>(&mut self, f: impl FnOnce() -> T) -> T;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::RP2350;

    #[test]
    fn y12_the_translated_base_names_the_running_slot() {
        let ab = RP2350.ab().unwrap();
        let at = |base| XipMap { base, size: ab.a.size };
        assert_eq!(running_slot(&ab, at(0x2000)), Some(Slot::A));
        assert_eq!(running_slot(&ab, at(0x18_0000)), Some(Slot::B));
        for base in [0, 0x1000, 0x17_F000, 0x2F_E000, 0x30_0000] {
            assert_eq!(running_slot(&ab, at(base)), None, "{base:#x}");
        }
        assert_eq!(Reboot::Normal.args(), (0, 0, 0));
        assert_eq!(Reboot::FlashUpdate { start: 0x1018_0000 }.args(), (4, 0x1018_0000, 0));
    }
}
