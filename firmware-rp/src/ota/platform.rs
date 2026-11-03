//! [`RpPlatform`]: `middleware::fw_ota::FwPlatform` on the RP2350's QSPI
//! flash and boot ROM.
//!
//! It is generic over the flash (any `NorFlash` whose offsets are physical
//! flash offsets for reads, erases and writes alike: on the board
//! `Sectored<Window<embassy_rp::flash::Flash>>`, in tests a RAM model) and
//! the [`Rom`], and is built for one request at a time from borrows held
//! by [`crate::ota::AbUpdate`]. Everything is blocking; every loop is
//! bounded by a sector or a fixed step.
//!
//! * Slots are the [`AbMap`]'s regions; slot offsets are bounded by the
//!   slot size, and `erase` and `write` refuse the running slot
//!   ([`PlatformError::Running`]) before touching flash.
//! * `erase` is one 4 KiB sector per call, so USB is serviced between
//!   sectors; `write` pads a tail that is not a multiple of the flash's
//!   write size with `0xFF`; `read` bounces reads the flash's read size
//!   does not allow through a small buffer.
//! * The record (`middleware::fw_ota::record`) lives in the map's two
//!   record sectors: a save erases the sector *not* holding the newest
//!   valid record and writes the new one there with the next sequence
//!   number, fields first and magic last, so a power cut leaves the old
//!   record or the new one.
//! * `boot_info`: the running slot from the boot ROM's address
//!   translation ([`running_slot`]), the trial flag from that image's own
//!   IMAGE_DEF TBYB flag read from flash (see [`crate::ota::rom`] for why
//!   that is the ROM's "buy pending").
//! * `explicit_buy`: reads the running image's first sector into a 4 KiB
//!   buffer on the stack, clears the IMAGE_TYPE TBYB bit
//!   ([`super::factory::clear_tbyb`]), and erases and programs the sector
//!   again with interrupts masked ([`Rom::uninterrupted`]; the first sector
//!   holds the vector table and no code, `super::image`), checks it read
//!   back, then stops the watchdog. A power cut part way leaves a torn
//!   first sector whose block loop does not close (the sentinel), so the
//!   other slot boots. Buying an image already bought only stops the
//!   watchdog.
//! * `reboot_flash_update` / `reboot_normal`: the ROM's asynchronous
//!   reboot, [`REBOOT_DELAY_MS`] later, `p0` the partition's XIP address.

use embedded_storage::nor_flash::{ErrorType, NorFlash};
use esp_base::image::SECTOR;
use middleware::fw_ota::picobin::{ImageDef, find_image_def};
use middleware::fw_ota::record::{FwRecord, RECORD_LEN, newest};
use middleware::fw_ota::{BootInfo, FwPlatform, Slot};

use super::factory::clear_tbyb;
use super::rom::{REBOOT_DELAY_MS, Reboot, Rom, XIP_BASE, running_slot};
use crate::layout::{AbMap, Region};

/// Bytes read per bounced step.
const BOUNCE: usize = 256;
/// Largest flash read or write granule supported (the RP2350 driver's is 1,
/// the test model's 4).
const MAX_GRANULE: usize = 8;

/// Why a platform operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformError<FE, RE> {
    /// The flash driver failed.
    Flash(FE),
    /// A ROM call failed.
    Rom(RE),
    /// An erase or write aimed at the running slot.
    Running,
    /// An access beyond the slot, or misaligned for the flash.
    Bounds,
    /// The boot ROM did not boot a slot of the A/B pair (the translated
    /// base, e.g. 0 without a partition table).
    NotPartitioned {
        /// The physical offset mapped at `0x1000_0000`.
        base: u32,
    },
    /// The running slot's first sector holds no RP2350 IMAGE_DEF, or the
    /// buy did not read back bought.
    NoImage,
    /// `explicit_buy` before `boot_info`.
    NoBoot,
}

/// A platform result for flash `F` and ROM `R`.
pub type PlatformResult<T, F, R> = Result<T, PlatformError<<F as ErrorType>::Error, <R as Rom>::Error>>;

/// The record (sequence number and record) read from each record sector.
type Records = [Option<(u32, FwRecord)>; 2];

/// The RP2350 platform for one request.
pub struct RpPlatform<'a, F, R> {
    flash: &'a mut F,
    rom: &'a mut R,
    ab: AbMap,
    running: Option<Slot>,
}

impl<'a, F: NorFlash, R: Rom> RpPlatform<'a, F, R> {
    /// The platform over `flash` and `rom` for the slots of `ab`;
    /// `running` once known (from a previous [`FwPlatform::boot_info`]).
    pub fn new(flash: &'a mut F, rom: &'a mut R, ab: AbMap, running: Option<Slot>) -> Self {
        RpPlatform { flash, rom, ab, running }
    }

    /// The running slot, once known.
    pub fn running(&self) -> Option<Slot> {
        self.running
    }

    fn region(&self, slot: Slot) -> Region {
        match slot {
            Slot::A => self.ab.a,
            Slot::B => self.ab.b,
        }
    }

    /// The physical offset of `len` bytes at `offset` of `slot`.
    fn at(&self, slot: Slot, offset: u32, len: usize) -> PlatformResult<u32, F, R> {
        let r = self.region(slot);
        let len = u32::try_from(len).map_err(|_| PlatformError::Bounds)?;
        match offset.checked_add(len) {
            Some(end) if end <= r.size => Ok(r.offset + offset),
            _ => Err(PlatformError::Bounds),
        }
    }

    fn writable(&self, slot: Slot) -> PlatformResult<(), F, R> {
        if self.running == Some(slot) { Err(PlatformError::Running) } else { Ok(()) }
    }

    /// Reads `buf` at physical `at`, bouncing what the flash's read size
    /// does not allow.
    fn read_phys(&mut self, at: u32, buf: &mut [u8]) -> PlatformResult<(), F, R> {
        let g = F::READ_SIZE.max(1);
        if g > MAX_GRANULE {
            return Err(PlatformError::Bounds);
        }
        if (at as usize).is_multiple_of(g) && buf.len().is_multiple_of(g) {
            return self.flash.read(at, buf).map_err(PlatformError::Flash);
        }
        let mut tmp = [0u8; BOUNCE + 2 * MAX_GRANULE];
        let mut done = 0usize;
        // Bounded: buf.len() / BOUNCE + 1 steps.
        while done < buf.len() {
            let n = (buf.len() - done).min(BOUNCE);
            let start = at as usize + done;
            let lo = start - start % g;
            let hi = (start + n).div_ceil(g) * g;
            let span = tmp.get_mut(..hi - lo).ok_or(PlatformError::Bounds)?;
            let lo32 = u32::try_from(lo).map_err(|_| PlatformError::Bounds)?;
            self.flash.read(lo32, span).map_err(PlatformError::Flash)?;
            let src = span.get(start - lo..start - lo + n).ok_or(PlatformError::Bounds)?;
            buf.get_mut(done..done + n).ok_or(PlatformError::Bounds)?.copy_from_slice(src);
            done += n;
        }
        Ok(())
    }

    /// Programs `data` at physical `at`, padding a short tail with `0xFF`.
    fn write_phys(&mut self, at: u32, data: &[u8]) -> PlatformResult<(), F, R> {
        let g = F::WRITE_SIZE.max(1);
        if g > MAX_GRANULE || !(at as usize).is_multiple_of(g) {
            return Err(PlatformError::Bounds);
        }
        let whole = data.len() - data.len() % g;
        let (head, tail) = data.split_at(whole);
        if !head.is_empty() {
            self.flash.write(at, head).map_err(PlatformError::Flash)?;
        }
        if !tail.is_empty() {
            let mut pad = [0xFFu8; MAX_GRANULE];
            pad.get_mut(..tail.len()).ok_or(PlatformError::Bounds)?.copy_from_slice(tail);
            let off = u32::try_from(whole).map_err(|_| PlatformError::Bounds)?;
            let p = pad.get(..g).ok_or(PlatformError::Bounds)?;
            self.flash.write(at + off, p).map_err(PlatformError::Flash)?;
        }
        Ok(())
    }

    /// The valid record in each record sector, as `(sequence, record)`.
    fn records(&mut self) -> PlatformResult<Records, F, R> {
        let mut out = [None, None];
        for (i, o) in out.iter_mut().enumerate() {
            let mut b = [0u8; RECORD_LEN];
            self.read_phys(self.ab.record.offset + i as u32 * SECTOR, &mut b)?;
            *o = FwRecord::decode(&b);
        }
        Ok(out)
    }

    /// The IMAGE_DEF of the image in `slot` (its block loop read from flash).
    fn image_def(&mut self, slot: Slot) -> PlatformResult<Option<ImageDef>, F, R> {
        let size = self.region(slot).size;
        find_image_def(
            |off| {
                let mut w = [0u8; 4];
                let at = self.at(slot, off, 4)?;
                self.read_phys(at, &mut w)?;
                Ok(u32::from_le_bytes(w))
            },
            size,
        )
    }

    /// Whether physical `at` holds exactly `want`, compared [`BOUNCE`]
    /// bytes at a time.
    fn holds(&mut self, at: u32, want: &[u8]) -> PlatformResult<bool, F, R> {
        let mut got = [0u8; BOUNCE];
        for (i, w) in want.chunks(BOUNCE).enumerate() {
            let g = got.get_mut(..w.len()).ok_or(PlatformError::Bounds)?;
            self.read_phys(at + (i * BOUNCE) as u32, g)?;
            if g != w {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn reboot(&mut self, r: Reboot) -> PlatformResult<(), F, R> {
        self.rom.reboot(r, REBOOT_DELAY_MS).map_err(PlatformError::Rom)
    }
}

impl<F: NorFlash, R: Rom> FwPlatform for RpPlatform<'_, F, R> {
    type Error = PlatformError<F::Error, R::Error>;

    fn boot_info(&mut self) -> Result<BootInfo, Self::Error> {
        let xip = self.rom.xip_map();
        let running = running_slot(&self.ab, xip).ok_or(PlatformError::NotPartitioned { base: xip.base })?;
        let def = self.image_def(running)?.ok_or(PlatformError::NoImage)?;
        self.running = Some(running);
        Ok(BootInfo { running, trial: def.is_tbyb() })
    }

    fn slot_size(&self) -> u32 {
        self.ab.a.size
    }

    fn read(&mut self, slot: Slot, offset: u32, buf: &mut [u8]) -> Result<(), Self::Error> {
        let at = self.at(slot, offset, buf.len())?;
        self.read_phys(at, buf)
    }

    fn erase(&mut self, slot: Slot, sector: u32) -> Result<(), Self::Error> {
        self.writable(slot)?;
        let from = self.at(slot, sector.checked_mul(SECTOR).ok_or(PlatformError::Bounds)?, SECTOR as usize)?;
        self.flash.erase(from, from + SECTOR).map_err(PlatformError::Flash)
    }

    fn write(&mut self, slot: Slot, offset: u32, data: &[u8]) -> Result<(), Self::Error> {
        self.writable(slot)?;
        let at = self.at(slot, offset, data.len())?;
        self.write_phys(at, data)
    }

    fn load_record(&mut self) -> Result<FwRecord, Self::Error> {
        let [a, b] = self.records()?;
        Ok(newest(a, b).map_or(FwRecord::EMPTY, |(_, r)| r))
    }

    fn persist(&mut self, record: &FwRecord) -> Result<(), Self::Error> {
        let [a, b] = self.records()?;
        // The sector not holding the newest record (sector 0 on a new board).
        let (target, seq) = match (a, b) {
            (Some(x), Some(y)) if y.0 > x.0 => (0, y.0),
            (Some(x), _) => (1, x.0),
            (None, Some(y)) => (0, y.0),
            (None, None) => (0, 0),
        };
        let bytes = record.encode(seq.wrapping_add(1));
        let at = self.ab.record.offset + target * SECTOR;
        self.flash.erase(at, at + SECTOR).map_err(PlatformError::Flash)?;
        let (magic, fields) = bytes.split_at(4);
        self.write_phys(at + 4, fields)?;
        self.write_phys(at, magic)
    }

    fn reboot_flash_update(&mut self, slot: Slot) -> Result<(), Self::Error> {
        let start = XIP_BASE + self.region(slot).offset;
        self.reboot(Reboot::FlashUpdate { start })
    }

    fn reboot_normal(&mut self) -> Result<(), Self::Error> {
        self.reboot(Reboot::Normal)
    }

    fn explicit_buy(&mut self) -> Result<(), Self::Error> {
        let running = self.running.ok_or(PlatformError::NoBoot)?;
        let def = self.image_def(running)?.ok_or(PlatformError::NoImage)?;
        if def.is_tbyb() {
            let at = self.at(running, 0, SECTOR as usize)?;
            let mut sector = [0u8; SECTOR as usize];
            self.read_phys(at, &mut sector)?;
            clear_tbyb(&mut sector, &def).map_err(|_| PlatformError::NoImage)?;
            let RpPlatform { flash, rom, .. } = self;
            rom.uninterrupted(|| {
                flash.erase(at, at + SECTOR)?;
                flash.write(at, &sector)
            })
            .map_err(PlatformError::Flash)?;
            let bought = self.image_def(running)?.is_some_and(|d| !d.is_tbyb());
            if !bought || !self.holds(at, &sector)? {
                return Err(PlatformError::NoImage);
            }
        }
        self.rom.stop_watchdog();
        Ok(())
    }
}
