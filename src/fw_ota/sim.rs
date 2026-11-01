//! An in-memory RP2350 bootrom and flash model (feature `fw-sim`, host
//! only), so the whole update lifecycle runs on the host.
//!
//! [`Rp2350Sim`] holds two slots of NOR flash (erase to `0xFF`, program by
//! AND-ing bits), the persisted [`FwRecord`] (saved atomically, as the
//! [`FwPlatform::persist`] contract requires) and the bootrom's view of the
//! current boot. It implements [`FwPlatform`]. Its bootrom selection
//! models what the design relies on (assumptions about the silicon,
//! listed in design.md):
//!
//! * a **normal** boot (power on, normal reboot, the trial watchdog firing)
//!   considers the slots whose IMAGE_DEF is a valid RP2350 executable
//!   **not** flagged TBYB and boots the higher version (A on a tie);
//! * a **flash-update** boot into a slot with a valid image boots it, on
//!   trial (buy pending) when it is flagged TBYB; with no valid image there
//!   it falls back to a normal boot;
//! * **explicit buy** on a trial boot erases and rewrites the image's first
//!   sector with the TBYB bit clear, as the bootrom does.
//!
//! Power loss: [`Rp2350Sim::cut_power_at`] names a mutating operation
//! (erase, program, persist, reboot request, and the buy's own erase and
//! program) by its 0-based ordinal in [`Rp2350Sim::ops`]; that operation
//! is torn and every later call fails until [`Rp2350Sim::power_on`]. A torn
//! erase erases only the second half of the sector (the half without the
//! metadata block: the worse case), a torn program writes the first half
//! of its bytes, a torn persist or reboot request does nothing. A
//! scheduled reboot fires only through [`Rp2350Sim::reset`].
//!
//! [`image`] builds test firmware images with an IMAGE_DEF.

use super::picobin::{
    EXE_TBYB, ImageDef, RP2350_ARM_S_EXE, SENTINEL_FROM, find_image_def_in, image_def_block_bytes,
    sentinel_block_bytes,
};
use super::platform::{BootInfo, FwPlatform};
use super::record::FwRecord;
use super::{SECTOR, Slot};
use std::vec::Vec;

/// Why a simulated operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimError {
    /// The power was cut (by this operation or before it).
    PowerLoss,
    /// An offset or length outside the slot.
    OutOfRange,
    /// `explicit_buy` outside a trial boot.
    NotTrial,
    /// No slot holds a bootable image.
    NotBootable,
}

/// A reboot the firmware requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebootKind {
    /// `REBOOT_TYPE_FLASH_UPDATE` into the slot.
    FlashUpdate(Slot),
    /// `REBOOT_TYPE_NORMAL`.
    Normal,
}

/// The simulated device. See the module docs.
#[derive(Debug, Clone)]
pub struct Rp2350Sim {
    slots: [Vec<u8>; 2],
    record: FwRecord,
    boot: Option<BootInfo>,
    scheduled: Option<RebootKind>,
    cut_at: Option<u64>,
    ops: u64,
    powered: bool,
    persists: u64,
}

/// Offset of the IMAGE_DEF block in [`image`]s (after a vector table).
pub const IMAGE_DEF_OFFSET: usize = 0x40;

/// Offset of the sentinel block in [`image`]s (the first sector's last
/// page, [`super::picobin::SENTINEL_FROM`]).
pub const SENTINEL_OFFSET: usize = SENTINEL_FROM as usize + 0x40;

/// A test image of `len` bytes (at least 4 KiB) with version
/// `version` (`major << 16 | minor`), flagged TBYB when `tbyb`, its block
/// loop an IMAGE_DEF at [`IMAGE_DEF_OFFSET`] and a sentinel at
/// [`SENTINEL_OFFSET`]; the other bytes are a pseudo-random function of
/// `seed`.
pub fn image(len: usize, version: u32, tbyb: bool, seed: u8) -> Vec<u8> {
    let mut v: Vec<u8> = (0..len)
        .map(|i| {
            (i as u32)
                .wrapping_mul(2_654_435_761)
                .wrapping_add(u32::from(seed)) as u8
        })
        .collect();
    let flags = RP2350_ARM_S_EXE | if tbyb { EXE_TBYB } else { 0 };
    let link = (SENTINEL_OFFSET - IMAGE_DEF_OFFSET) as i32;
    let block = image_def_block_bytes(flags, (version >> 16) as u16, version as u16, link);
    if let Some(dst) = v.get_mut(IMAGE_DEF_OFFSET..IMAGE_DEF_OFFSET + block.len()) {
        dst.copy_from_slice(&block);
    }
    let sentinel = sentinel_block_bytes(-link);
    if let Some(dst) = v.get_mut(SENTINEL_OFFSET..SENTINEL_OFFSET + sentinel.len()) {
        dst.copy_from_slice(&sentinel);
    }
    v
}

fn bootable(def: Option<ImageDef>) -> Option<ImageDef> {
    def.filter(|d| d.is_exe() && d.is_rp2350())
}

impl Rp2350Sim {
    /// Erased flash, slots of `slot_size` bytes, no record, powered off.
    pub fn new(slot_size: u32) -> Self {
        let erased = || std::vec![0xFFu8; slot_size as usize];
        Rp2350Sim {
            slots: [erased(), erased()],
            record: FwRecord::EMPTY,
            boot: None,
            scheduled: None,
            cut_at: None,
            ops: 0,
            powered: false,
            persists: 0,
        }
    }

    /// Factory programming (picotool or a BOOTSEL drag): erases `slot` and
    /// writes `image`. Not an operation of the firmware; never torn.
    pub fn program(&mut self, slot: Slot, image: &[u8]) {
        let s = &mut self.slots[slot as usize];
        s.fill(0xFF);
        let n = image.len().min(s.len());
        s[..n].copy_from_slice(&image[..n]);
    }

    /// The IMAGE_DEF of a slot, if it holds a bootable executable.
    pub fn image_def(&self, slot: Slot) -> Option<ImageDef> {
        bootable(find_image_def_in(&self.slots[slot as usize]))
    }

    fn select(&self, kind: RebootKind) -> Option<BootInfo> {
        if let RebootKind::FlashUpdate(s) = kind
            && let Some(d) = self.image_def(s)
        {
            return Some(BootInfo {
                running: s,
                trial: d.is_tbyb(),
            });
        }
        let candidate = |s: Slot| {
            self.image_def(s)
                .filter(|d| !d.is_tbyb())
                .map(|d| d.version_u32())
        };
        match (candidate(Slot::A), candidate(Slot::B)) {
            (Some(a), Some(b)) if b > a => Some(BootInfo {
                running: Slot::B,
                trial: false,
            }),
            (Some(_), _) => Some(BootInfo {
                running: Slot::A,
                trial: false,
            }),
            (None, Some(_)) => Some(BootInfo {
                running: Slot::B,
                trial: false,
            }),
            (None, None) => None,
        }
    }

    /// Power on (after a cut, or the first time): a normal boot. Returns
    /// what booted, `None` when nothing is bootable (a bricked device).
    pub fn power_on(&mut self) -> Option<BootInfo> {
        self.powered = true;
        self.scheduled = None;
        self.boot = self.select(RebootKind::Normal);
        self.boot
    }

    /// The chip resets while powered: the scheduled reboot fires, or (none
    /// scheduled) the trial watchdog bites or the reset button is pressed,
    /// which is a normal boot.
    pub fn reset(&mut self) -> Option<BootInfo> {
        let kind = self.scheduled.take().unwrap_or(RebootKind::Normal);
        self.powered = true;
        self.boot = self.select(kind);
        self.boot
    }

    /// Tears mutating operation number `op` (see [`Rp2350Sim::ops`]).
    pub fn cut_power_at(&mut self, op: u64) {
        self.cut_at = Some(op);
    }

    /// Mutating operations performed so far.
    pub fn ops(&self) -> u64 {
        self.ops
    }

    /// Records persisted so far.
    pub fn persists(&self) -> u64 {
        self.persists
    }

    /// Whether the power is on.
    pub fn powered(&self) -> bool {
        self.powered
    }

    /// The reboot the firmware requested and that has not fired yet.
    pub fn scheduled(&self) -> Option<RebootKind> {
        self.scheduled
    }

    /// The current boot, `None` before power on or when nothing booted.
    pub fn booted(&self) -> Option<BootInfo> {
        self.boot
    }

    /// A slot's bytes.
    pub fn slot(&self, slot: Slot) -> &[u8] {
        &self.slots[slot as usize]
    }

    /// The persisted record.
    pub fn record(&self) -> FwRecord {
        self.record
    }

    /// Counts a mutating operation: `Ok(true)` when it is the one to tear
    /// (the power is then off).
    fn tick(&mut self) -> Result<bool, SimError> {
        if !self.powered {
            return Err(SimError::PowerLoss);
        }
        let torn = self.cut_at == Some(self.ops);
        self.ops += 1;
        if torn {
            self.powered = false;
            self.boot = None;
        }
        Ok(torn)
    }

    fn range(&self, offset: u32, len: usize) -> Result<core::ops::Range<usize>, SimError> {
        let start = offset as usize;
        let end = start.checked_add(len).ok_or(SimError::OutOfRange)?;
        if end > self.slots[0].len() {
            Err(SimError::OutOfRange)
        } else {
            Ok(start..end)
        }
    }

    fn erase_raw(&mut self, slot: Slot, sector: u32) -> Result<(), SimError> {
        let r = self.range(
            sector.checked_mul(SECTOR).ok_or(SimError::OutOfRange)?,
            SECTOR as usize,
        )?;
        let torn = self.tick()?;
        let s = &mut self.slots[slot as usize][r];
        if torn {
            s[SECTOR as usize / 2..].fill(0xFF);
            return Err(SimError::PowerLoss);
        }
        s.fill(0xFF);
        Ok(())
    }

    fn write_raw(&mut self, slot: Slot, offset: u32, data: &[u8]) -> Result<(), SimError> {
        let r = self.range(offset, data.len())?;
        let torn = self.tick()?;
        let n = if torn { data.len() / 2 } else { data.len() };
        for (dst, src) in self.slots[slot as usize][r].iter_mut().zip(&data[..n]) {
            *dst &= *src;
        }
        if torn {
            Err(SimError::PowerLoss)
        } else {
            Ok(())
        }
    }
}

impl FwPlatform for Rp2350Sim {
    type Error = SimError;

    fn boot_info(&mut self) -> Result<BootInfo, SimError> {
        if !self.powered {
            return Err(SimError::PowerLoss);
        }
        self.boot.ok_or(SimError::NotBootable)
    }

    fn slot_size(&self) -> u32 {
        self.slots[0].len() as u32
    }

    fn read(&mut self, slot: Slot, offset: u32, buf: &mut [u8]) -> Result<(), SimError> {
        if !self.powered {
            return Err(SimError::PowerLoss);
        }
        let r = self.range(offset, buf.len())?;
        buf.copy_from_slice(&self.slots[slot as usize][r]);
        Ok(())
    }

    fn erase(&mut self, slot: Slot, sector: u32) -> Result<(), SimError> {
        self.erase_raw(slot, sector)
    }

    fn write(&mut self, slot: Slot, offset: u32, data: &[u8]) -> Result<(), SimError> {
        self.write_raw(slot, offset, data)
    }

    fn load_record(&mut self) -> Result<FwRecord, SimError> {
        if !self.powered {
            return Err(SimError::PowerLoss);
        }
        Ok(self.record)
    }

    fn persist(&mut self, record: &FwRecord) -> Result<(), SimError> {
        if self.tick()? {
            return Err(SimError::PowerLoss);
        }
        self.record = *record;
        self.persists += 1;
        Ok(())
    }

    fn reboot_flash_update(&mut self, slot: Slot) -> Result<(), SimError> {
        if self.tick()? {
            return Err(SimError::PowerLoss);
        }
        self.scheduled = Some(RebootKind::FlashUpdate(slot));
        Ok(())
    }

    fn reboot_normal(&mut self) -> Result<(), SimError> {
        if self.tick()? {
            return Err(SimError::PowerLoss);
        }
        self.scheduled = Some(RebootKind::Normal);
        Ok(())
    }

    fn explicit_buy(&mut self) -> Result<(), SimError> {
        let boot = self.boot.filter(|b| b.trial).ok_or(SimError::NotTrial)?;
        let def = self.image_def(boot.running).ok_or(SimError::NotTrial)?;
        let slot = boot.running;
        let mut sector: Vec<u8> = self.slots[slot as usize][..SECTOR as usize].to_vec();
        // The flags are the upper half-word of the IMAGE_TYPE item header:
        // TBYB is bit 15 of the flags, bit 7 of the header's last byte.
        let flag_byte = def.type_offset as usize + 3;
        if let Some(b) = sector.get_mut(flag_byte) {
            *b &= !((EXE_TBYB >> 8) as u8);
        }
        self.erase_raw(slot, 0)?;
        self.write_raw(slot, 0, &sector)?;
        self.boot = Some(BootInfo {
            running: slot,
            trial: false,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests;
