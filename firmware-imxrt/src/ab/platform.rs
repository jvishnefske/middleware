//! `middleware::fw_ota` on the RT1050 (RT7): [`RtImage`], the slot image
//! header as an [`ImageFormat`], and [`RtPlatform`], the [`FwPlatform`]
//! over any block-erased NOR flash and the [`BootRecord`] the bootloader
//! reads.
//!
//! The updater is `middleware::fw_ota::service::FwService::<RtImage>`,
//! protocol unchanged. What the RP2350's boot ROM does, the record does
//! here ([`super::record`] has the mapping table):
//!
//! * `boot_info`: the [`Handoff`] the bootloader left (slot, trial).
//! * `persist` / `load_record`: the record's `FwRecord` part
//!   ([`BootRecord::with_fw`]), saved atomically in the two record blocks.
//! * `reboot_flash_update(slot)`: the record must say `slot` is pending
//!   with its reboot requested and no attempt yet (the updater persisted
//!   that just before; written again only if not), then a reset is
//!   requested ([`AbState::take_reset`]). The bootloader boots `slot` on
//!   trial.
//! * `reboot_normal`: a reset; a trial that ends this way rolls back.
//! * `explicit_buy`: the record marks the running (trial) slot confirmed
//!   ([`BootRecord::confirmed_as`]) and the trial watchdog may be released
//!   ([`AbState::trial_released`]).
//!
//! Every save made on a boot that is not a trial also records the running
//! slot as confirmed. Normally it already is; when the bootloader fell
//! back to the other slot because the confirmed one failed its check, this
//! keeps the next update's target (the inactive, failed slot) from being
//! taken for the confirmed slot: a committed update there then still boots
//! on trial, never as confirmed.
//!
//! # Image format
//!
//! The transferred image is the slot's contents: the [`SlotHeader`], then
//! the SDRAM image. At commit [`RtImage::inspect`] decodes the header and
//! hashes header fields and image as the bootloader will; the image is
//! accepted ([`RtImageInfo::acceptable`]) only if the header's version is
//! the declared one, its length is exactly what was received, the image
//! fits the SDRAM region and its SHA-256 matches. `sys/fw/info` reports a
//! slot's header version (`tbyb` is always false: the trial is the
//! record's, not the image's).
//!
//! # Erasing 256 KiB blocks for 4 KiB sectors
//!
//! `fw_ota` erases a 4 KiB sector before the first chunk of each sector,
//! chunks arrive in any order, and sector 0 (the header) is erased at
//! `begin` and written last, at commit, after the record. The HyperFlash
//! erases 256 KiB blocks, so [`AbState`] keeps, per slot, which sectors
//! are known erased (one bit per sector, RAM only):
//!
//! * `erase(slot, sector)` is a no-op when the sector is known erased;
//!   otherwise it erases the sector's whole block and marks its 64 sectors
//!   erased. So within a transfer each block is erased at most once,
//!   before its first program (`begin` erases block 0; the first chunk to
//!   reach another block erases that block), and a later chunk in an
//!   already erased block does not destroy its neighbours.
//! * `write` marks the sectors it touches not erased before programming.
//! * Sector 0 is only stashed until commit, so it is still known erased
//!   when the commit's install erases and writes it: the install's erase
//!   is a no-op and the header is the last program of the image. A
//!   discard (abort, or the boot after an interrupted commit) erases
//!   block 0 when sector 0 holds data, which removes the header.
//!
//! After a reset nothing is known erased, so the first erase of each block
//! is real again. A block erase takes the S26KS512S's sector erase time
//! (typically about a second; the datasheet's maximum is several times
//! that, to be measured on the board, RT13), during which the request
//! blocks: `sys/fw/begin` and the first chunk of every 256 KiB of the image
//! answer that much later than the others (a 1 MiB image: five such
//! waits). The host's request timeout must allow for it.

use super::boot::Handoff;
use super::header::{HEADER_LEN, SlotHeader};
use super::io::{IoError, read_at, write_at};
use super::record::{BootRecord, Stored, load, save};
use crate::layout::{AbMap, BLOCK, SDRAM_IMAGE, SECTOR};
use embedded_storage::nor_flash::NorFlash;
use middleware::fw_ota::format::{ImageFormat, ImageInfo};
use middleware::fw_ota::record::FwRecord;
use middleware::fw_ota::wire::SlotInfo;
use middleware::fw_ota::{BootInfo, FwPlatform, MAX_IMAGE, Slot};
use middleware::loader::trust::Version;

/// 4 KiB sectors of the largest slot.
const SLOT_SECTORS: usize = (MAX_IMAGE / SECTOR) as usize;
/// Sectors per erase block.
const SECTORS_PER_BLOCK: u32 = BLOCK / SECTOR;
/// Bytes hashed per read at commit.
const HASH_STEP: usize = 256;

/// What [`RtImage::inspect`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtImageInfo {
    /// The decoded header.
    pub header: SlotHeader,
    /// The length of the image as received (header included).
    pub image_len: u32,
    /// The header fields and image hash to the header's SHA-256.
    pub hash_ok: bool,
}

impl ImageInfo for RtImageInfo {
    /// The header's version is `version`, its length is exactly what was
    /// received, the image fits the SDRAM region and the hash matches.
    fn acceptable(&self, version: Version) -> bool {
        self.hash_ok
            && self.header.version == version
            && self.header.total_len() == Some(self.image_len)
            && self
                .header
                .fits(self.image_len, SDRAM_IMAGE.size as usize)
                .is_ok()
    }
}

/// The RT1050 slot image format. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RtImage;

fn read_header<E, R>(read: &mut R) -> Result<[u8; HEADER_LEN], E>
where
    R: FnMut(u32, &mut [u8]) -> Result<(), E>,
{
    let mut raw = [0u8; HEADER_LEN];
    read(0, &mut raw)?;
    Ok(raw)
}

impl ImageFormat for RtImage {
    type Info = RtImageInfo;

    /// Decodes the header and, when its length is the received length,
    /// hashes header fields and image (256-byte reads, bounded by
    /// `len`).
    fn inspect<E, R>(read: &mut R, len: u32) -> Result<Option<RtImageInfo>, E>
    where
        R: FnMut(u32, &mut [u8]) -> Result<(), E>,
    {
        if (len as usize) < HEADER_LEN {
            return Ok(None);
        }
        let Ok(header) = SlotHeader::decode(&read_header(read)?) else {
            return Ok(None);
        };
        let mut hash_ok = false;
        if header.total_len() == Some(len) {
            let mut h = header.hasher();
            let mut buf = [0u8; HASH_STEP];
            let mut at = HEADER_LEN as u32;
            // Bounded: len / HASH_STEP + 1 reads.
            while at < len {
                let n = (len - at).min(HASH_STEP as u32) as usize;
                let b = buf.get_mut(..n).unwrap_or_default();
                read(at, b)?;
                h = h.update(b);
                at += n as u32;
            }
            hash_ok = h.finish() == header.sha256;
        }
        Ok(Some(RtImageInfo {
            header,
            image_len: len,
            hash_ok,
        }))
    }

    /// The header's version when it decodes and fits the slot and the
    /// SDRAM region (no hash: the bootloader checked the running slot, and
    /// the other is re-checked before it boots).
    fn slot_info<E, R>(read: &mut R, slot_size: u32) -> Result<SlotInfo, E>
    where
        R: FnMut(u32, &mut [u8]) -> Result<(), E>,
    {
        let raw = read_header(read)?;
        Ok(match SlotHeader::decode(&raw) {
            Ok(h) if h.fits(slot_size, SDRAM_IMAGE.size as usize).is_ok() => SlotInfo {
                version: Some(h.version),
                tbyb: false,
            },
            _ => SlotInfo::default(),
        })
    }
}

/// A reset the firmware requested; the shell performs it once the reply
/// has left (as the RP2350's 100 ms delayed reboot).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetRequest {
    /// Into the bootloader, which boots the pending slot on trial.
    Update(Slot),
    /// Into the bootloader, no trial requested.
    Normal,
}

/// Why a platform operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbError<E> {
    /// The flash driver failed.
    Flash(E),
    /// The flash's read or write granule is unsupported.
    Granule,
    /// The flash's erase size is not [`BLOCK`], or the map's slots are
    /// larger than `fw_ota` accepts.
    Geometry,
    /// An erase or write aimed at the running slot.
    Running,
    /// An access beyond the slot.
    Bounds,
    /// `explicit_buy` outside a trial boot.
    NotTrial,
    /// `reboot_flash_update` without that slot pending in the record.
    NotPending,
    /// A save would lower `min_version`.
    Rollback,
}

impl<E> From<IoError<E>> for AbError<E> {
    fn from(e: IoError<E>) -> Self {
        match e {
            IoError::Flash(e) => AbError::Flash(e),
            IoError::Granule => AbError::Granule,
        }
    }
}

/// The updater's state that outlives one request: this boot's handoff,
/// the record as last read or saved, which sectors are known erased, and
/// the requests for the shell (reset, watchdog release). About 300 bytes;
/// the shell keeps it beside the `FwService` and lends both per request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbState {
    boot: BootInfo,
    stored: Option<Stored>,
    erased: [[u32; SLOT_SECTORS / 32]; 2],
    reset: Option<ResetRequest>,
    released: bool,
}

impl AbState {
    /// The state at boot, from the bootloader's handoff.
    pub const fn new(handoff: Handoff) -> Self {
        AbState {
            boot: BootInfo {
                running: handoff.slot,
                trial: handoff.trial,
            },
            stored: None,
            erased: [[0; SLOT_SECTORS / 32]; 2],
            reset: None,
            released: false,
        }
    }

    /// The reset the firmware asked for, once: `reboot_flash_update` or
    /// `reboot_normal` (or an abort of a trial).
    pub fn take_reset(&mut self) -> Option<ResetRequest> {
        self.reset.take()
    }

    /// The trial was confirmed: the shell may stop the trial watchdog.
    pub const fn trial_released(&self) -> bool {
        self.released
    }

    /// The record as last read or saved.
    pub fn record(&self) -> Option<BootRecord> {
        self.stored.map(|s| s.record)
    }

    fn known_erased(&self, slot: Slot, sector: u32) -> bool {
        let i = sector as usize;
        self.erased[slot as usize]
            .get(i / 32)
            .is_some_and(|w| w & (1 << (i % 32)) != 0)
    }

    fn mark(&mut self, slot: Slot, sectors: core::ops::Range<u32>, erased: bool) {
        // Bounded: at most one block's sectors.
        for s in sectors {
            let i = s as usize;
            if let Some(w) = self.erased[slot as usize].get_mut(i / 32) {
                if erased {
                    *w |= 1 << (i % 32);
                } else {
                    *w &= !(1 << (i % 32));
                }
            }
        }
    }
}

/// The RT1050 platform for one request.
pub struct RtPlatform<'a, F> {
    flash: &'a mut F,
    state: &'a mut AbState,
    ab: AbMap,
}

impl<'a, F: NorFlash> RtPlatform<'a, F> {
    /// The platform over `flash` (offsets from the start of the
    /// HyperFlash, erase size [`BLOCK`]) for the slots and record of `ab`.
    pub fn new(
        flash: &'a mut F,
        state: &'a mut AbState,
        ab: AbMap,
    ) -> Result<Self, AbError<F::Error>> {
        let slots_ok = ab.a.size == ab.b.size && ab.a.size <= MAX_IMAGE;
        if F::ERASE_SIZE != BLOCK as usize || !slots_ok {
            return Err(AbError::Geometry);
        }
        Ok(RtPlatform { flash, state, ab })
    }

    /// The flash offset of `len` bytes at `offset` of `slot`.
    fn at(&self, slot: Slot, offset: u32, len: usize) -> Result<u32, AbError<F::Error>> {
        let r = self.ab.slot(slot);
        let len = u32::try_from(len).map_err(|_| AbError::Bounds)?;
        match offset.checked_add(len) {
            Some(end) if end <= r.size => Ok(r.offset + offset),
            _ => Err(AbError::Bounds),
        }
    }

    fn writable(&self, slot: Slot) -> Result<(), AbError<F::Error>> {
        if slot == self.state.boot.running {
            Err(AbError::Running)
        } else {
            Ok(())
        }
    }

    fn stored(&mut self) -> Result<Stored, AbError<F::Error>> {
        match self.state.stored {
            Some(s) => Ok(s),
            None => {
                let s = load(self.flash, self.ab.record)?;
                self.state.stored = Some(s);
                Ok(s)
            }
        }
    }

    /// Saves `next` unless it is what the record already holds. On a boot
    /// that is not a trial the running slot is recorded as confirmed (see
    /// the module docs).
    fn save(&mut self, next: BootRecord) -> Result<(), AbError<F::Error>> {
        let boot = self.state.boot;
        let next = if boot.trial {
            next
        } else {
            BootRecord {
                confirmed: boot.running,
                ..next
            }
        };
        let current = self.stored()?;
        if current.record.fw.lowers(&next.fw) {
            return Err(AbError::Rollback);
        }
        if current.at.is_some() && current.record == next {
            return Ok(());
        }
        // On failure the flash still holds `current` (or the save is torn
        // and reads back as `current`); read it again next time.
        self.state.stored = None;
        let saved = save(self.flash, self.ab.record, &current, next)?;
        self.state.stored = Some(saved);
        Ok(())
    }
}

impl<F: NorFlash> FwPlatform for RtPlatform<'_, F> {
    type Error = AbError<F::Error>;

    fn boot_info(&mut self) -> Result<BootInfo, Self::Error> {
        Ok(self.state.boot)
    }

    fn slot_size(&self) -> u32 {
        self.ab.slot_size()
    }

    fn read(&mut self, slot: Slot, offset: u32, buf: &mut [u8]) -> Result<(), Self::Error> {
        let at = self.at(slot, offset, buf.len())?;
        Ok(read_at(self.flash, at, buf)?)
    }

    /// Erases the block holding `sector` unless the sector is known
    /// erased (see the module docs).
    fn erase(&mut self, slot: Slot, sector: u32) -> Result<(), Self::Error> {
        self.writable(slot)?;
        let offset = sector.checked_mul(SECTOR).ok_or(AbError::Bounds)?;
        self.at(slot, offset, SECTOR as usize)?;
        if self.state.known_erased(slot, sector) {
            return Ok(());
        }
        let first = sector - sector % SECTORS_PER_BLOCK;
        let block = first..first + SECTORS_PER_BLOCK;
        let at = self.at(slot, first * SECTOR, BLOCK as usize)?;
        self.state.mark(slot, block.clone(), false);
        self.flash.erase(at, at + BLOCK).map_err(AbError::Flash)?;
        self.state.mark(slot, block, true);
        Ok(())
    }

    fn write(&mut self, slot: Slot, offset: u32, data: &[u8]) -> Result<(), Self::Error> {
        self.writable(slot)?;
        let at = self.at(slot, offset, data.len())?;
        if !data.is_empty() {
            let last = offset + data.len() as u32 - 1;
            self.state
                .mark(slot, offset / SECTOR..last / SECTOR + 1, false);
        }
        Ok(write_at(self.flash, at, data)?)
    }

    fn load_record(&mut self) -> Result<FwRecord, Self::Error> {
        self.state.stored = None;
        Ok(self.stored()?.record.fw)
    }

    fn persist(&mut self, record: &FwRecord) -> Result<(), Self::Error> {
        let next = self.stored()?.record.with_fw(*record);
        self.save(next)
    }

    /// The record must request `slot`'s trial ([`BootRecord::trial`], no
    /// attempt yet); a reset is requested.
    fn reboot_flash_update(&mut self, slot: Slot) -> Result<(), Self::Error> {
        let current = self.stored()?.record;
        let mut fw = current.fw;
        match fw.pending.as_mut() {
            Some(p) if p.slot == slot && slot != self.state.boot.running => p.rebooted = true,
            _ => return Err(AbError::NotPending),
        }
        let next = BootRecord {
            attempts: 0,
            ..current.with_fw(fw)
        };
        self.save(next)?;
        self.state.reset = Some(ResetRequest::Update(slot));
        Ok(())
    }

    fn reboot_normal(&mut self) -> Result<(), Self::Error> {
        self.state.reset = Some(ResetRequest::Normal);
        Ok(())
    }

    /// Marks the running trial slot confirmed; the trial watchdog may then
    /// be released.
    fn explicit_buy(&mut self) -> Result<(), Self::Error> {
        if !self.state.boot.trial {
            return Err(AbError::NotTrial);
        }
        let running = self.state.boot.running;
        let next = self.stored()?.record.confirmed_as(running);
        self.save(next)?;
        self.state.released = true;
        Ok(())
    }
}
