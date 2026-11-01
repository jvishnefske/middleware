//! The imperative shell around [`FwUpdate`] (feature `trust`).
//!
//! [`FwService`] owns the update state, the in-RAM copy of the persisted
//! [`FwRecord`], this boot's [`BootInfo`] and the sector-0 stash, and runs
//! each `sys/fw/*` request: parse ([`Command::parse`]), decide
//! ([`FwUpdate::step`], pure), perform the [`Effect`] on the
//! [`FwPlatform`], and feed the result back ([`FwUpdate::verified`],
//! [`FwUpdate::installed`], [`FwUpdate::bought`]). It needs the `trust`
//! feature for SHA-256 ([`ImageHash`], over [`FwPlatform::read`]) and
//! Ed25519.
//!
//! Every platform failure is answered `Internal`; the state is then the
//! one before the request, except where the effect already took hold
//! (documented per effect in [`FwService::handle_command`]). Every effect
//! that erases or writes refuses the running slot (`Internal`) before
//! touching flash, as defence in depth: the pure core never targets it.
//!
//! The service holds a 4 KiB stash, so it is about 5.2 KiB; the
//! integration keeps it in a `static StaticCell` or the dispatcher's
//! frame and takes `&mut` to it, as the shell is the one place state is
//! mutated in place.

use super::machine::{BootAction, BootFacts, Effect, Env, Facts, FwUpdate, acceptable};
use super::picobin::{ImageDef, find_image_def};
use super::platform::{BootInfo, FwPlatform};
use super::record::{FwRecord, Pending};
use super::wire::{Command, FwInfo, SlotInfo};
use super::{SECTOR, Slot};
use crate::key::Key;
use crate::loader::sha256::ImageHash;
use crate::loader::trust::{TrustState, Version};
use crate::transport::{HandlerError, SenderId};

/// Bytes read per step while hashing or comparing flash.
const READ_STEP: usize = 256;

/// What the integration supplies with every request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Context {
    /// The device node id (the one `sys/info` reports), bound into the
    /// commit signature.
    pub node: SenderId,
    /// The root of trust from the trust store.
    pub trust: TrustState,
    /// The running firmware's self-test passed: a trial may be confirmed.
    pub self_test_ok: bool,
}

/// The firmware updater of one device. See the module docs.
pub struct FwService {
    state: FwUpdate,
    record: FwRecord,
    boot: BootInfo,
    running_version: Version,
    stash: [u8; SECTOR as usize],
}

impl core::fmt::Debug for FwService {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FwService")
            .field("state", &self.state)
            .field("record", &self.record)
            .field("boot", &self.boot)
            .finish_non_exhaustive()
    }
}

fn internal<E>(_: E) -> HandlerError {
    HandlerError::Internal
}

fn read_word<P: FwPlatform>(p: &mut P, slot: Slot, at: u32) -> Result<u32, P::Error> {
    let mut w = [0u8; 4];
    p.read(slot, at, &mut w)?;
    Ok(u32::from_le_bytes(w))
}

/// The IMAGE_DEF of the `len`-byte image in `slot`, reading the first
/// sector from `stash` when given.
fn image_def<P: FwPlatform>(
    p: &mut P,
    slot: Slot,
    len: u32,
    stash: Option<&[u8; SECTOR as usize]>,
) -> Result<Option<ImageDef>, P::Error> {
    let read = |at: u32| -> Result<u32, P::Error> {
        match stash {
            Some(s) if at + 4 <= SECTOR => {
                let i = at as usize;
                Ok(s.get(i..i + 4)
                    .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])))
            }
            _ => read_word(p, slot, at),
        }
    };
    find_image_def(read, len)
}

/// SHA-256 of the `len`-byte image in `slot`, the first sector from
/// `stash` when given. Bounded: `len / READ_STEP + 1` reads.
fn hash<P: FwPlatform>(
    p: &mut P,
    slot: Slot,
    len: u32,
    stash: Option<&[u8; SECTOR as usize]>,
) -> Result<[u8; 32], P::Error> {
    let mut h = ImageHash::new();
    let mut at = 0u32;
    if let Some(s) = stash {
        let n = len.min(SECTOR);
        h = h.update(s.get(..n as usize).unwrap_or_default());
        at = n;
    }
    let mut buf = [0u8; READ_STEP];
    while at < len {
        let n = (len - at).min(READ_STEP as u32);
        let b = buf.get_mut(..n as usize).unwrap_or_default();
        p.read(slot, at, b)?;
        h = h.update(b);
        at += n;
    }
    Ok(h.finish())
}

/// What `slot` holds: a valid RP2350 executable's version and TBYB flag.
fn slot_info<P: FwPlatform>(p: &mut P, slot: Slot) -> Result<SlotInfo, P::Error> {
    let size = p.slot_size();
    Ok(match image_def(p, slot, size, None)? {
        Some(d) if d.is_exe() && d.is_rp2350() => SlotInfo {
            version: Some(Version(d.version_u32())),
            tbyb: d.is_tbyb(),
        },
        _ => SlotInfo::default(),
    })
}

/// Whether `slot` still holds exactly the committed image `p`.
fn intact<P: FwPlatform>(plat: &mut P, p: &Pending) -> Result<bool, P::Error> {
    if p.len == 0 || p.len > plat.slot_size() {
        return Ok(false);
    }
    let def = image_def(plat, p.slot, p.len, None)?;
    if !def.is_some_and(|d| acceptable(&d, p.version)) {
        return Ok(false);
    }
    Ok(hash(plat, p.slot, p.len, None)? == p.sha256)
}

impl FwService {
    /// Starts the updater at boot: reads the boot info, the record and the
    /// running image's version, reconciles them ([`FwUpdate::boot`]) and
    /// performs the resulting persist or discard.
    pub fn boot<P: FwPlatform>(p: &mut P) -> Result<Self, P::Error> {
        let boot = p.boot_info()?;
        let mut record = p.load_record()?;
        let running_version = Version(slot_info(p, boot.running)?.version.map_or(0, |v| v.0));
        let pending_intact = match record.pending {
            Some(pd) if !boot.trial && pd.slot != boot.running && !pd.rebooted => intact(p, &pd)?,
            _ => false,
        };
        let facts = BootFacts {
            running: boot.running,
            trial: boot.trial,
            record,
            pending_intact,
        };
        let (state, action) = FwUpdate::boot(&facts);
        match action {
            BootAction::None => {}
            BootAction::Persist(r) => {
                p.persist(&r)?;
                record = r;
            }
            BootAction::Discard(slot, r) => {
                p.persist(&r)?;
                record = r;
                // `FwUpdate::boot` only discards a slot that is not running.
                if slot != boot.running {
                    p.erase(slot, 0)?;
                }
            }
        }
        Ok(FwService {
            state,
            record,
            boot,
            running_version,
            stash: [0xFF; SECTOR as usize],
        })
    }

    /// The update state.
    pub fn state(&self) -> FwUpdate {
        self.state
    }

    /// The in-RAM copy of the persisted record.
    pub fn record(&self) -> FwRecord {
        self.record
    }

    /// This boot.
    pub fn boot_info(&self) -> BootInfo {
        self.boot
    }

    fn env<P: FwPlatform>(&self, p: &P, ctx: &Context) -> Env {
        Env {
            running: self.boot.running,
            running_version: self.running_version,
            record: self.record,
            capacity: p.slot_size(),
            node: ctx.node,
            trust: ctx.trust,
            self_test_ok: ctx.self_test_ok,
        }
    }

    fn guard(&self, slot: Slot) -> Result<(), HandlerError> {
        if slot == self.boot.running {
            Err(HandlerError::Internal)
        } else {
            Ok(())
        }
    }

    /// Handles a request: `None` when `key` is not a `sys/fw/` key, else
    /// the response body length written to `out` or the error status.
    pub fn handle<P: FwPlatform>(
        &mut self,
        p: &mut P,
        ctx: &Context,
        key: Key<'_>,
        body: &[u8],
        out: &mut [u8],
    ) -> Option<Result<usize, HandlerError>> {
        match Command::parse(key, body) {
            Ok(None) => None,
            Err(e) => Some(Err(e)),
            Ok(Some(cmd)) => Some(self.handle_command(p, ctx, &cmd, out)),
        }
    }

    /// Runs one parsed request. On a platform failure:
    ///
    /// * `Begin`, `Write`, `Verify`, `Buy` (the buy itself): the state is
    ///   the one before the request;
    /// * `Install`: stays `Verified` (commit again to retry);
    /// * `Reboot`: stays `Committed`;
    /// * `Discard`: `Idle` once the record is saved, else unchanged;
    /// * a buy that succeeded but whose record could not be saved still
    ///   answers success (the bootrom bought the image; the next boot
    ///   settles the record, [`FwUpdate::boot`]).
    pub fn handle_command<P: FwPlatform>(
        &mut self,
        p: &mut P,
        ctx: &Context,
        cmd: &Command<'_>,
        out: &mut [u8],
    ) -> Result<usize, HandlerError> {
        let env = self.env(p, ctx);
        let before = self.state;
        let (after, effect) = before.step(cmd, &env);
        let effect = match effect {
            Ok(e) => e,
            Err(e) => {
                self.state = after;
                return Err(e);
            }
        };
        match effect {
            Effect::None => {
                self.state = after;
                Ok(0)
            }
            Effect::Info => {
                self.state = after;
                self.info(p, ctx, out)
            }
            Effect::Begin { slot, fresh } => {
                self.guard(slot)?;
                *out.first_mut().ok_or(HandlerError::BufferTooSmall)? = slot as u8;
                if fresh {
                    p.erase(slot, 0).map_err(internal)?;
                    self.stash = [0xFF; SECTOR as usize];
                }
                self.state = after;
                Ok(1)
            }
            Effect::Stash { offset, data } => {
                let at = offset as usize;
                let dst = self
                    .stash
                    .get_mut(at..at + data.len())
                    .ok_or(HandlerError::Internal)?;
                dst.copy_from_slice(data);
                self.state = after;
                Ok(0)
            }
            Effect::Write {
                slot,
                offset,
                data,
                erase,
            } => {
                self.guard(slot)?;
                if let Some(sector) = erase {
                    p.erase(slot, sector).map_err(internal)?;
                }
                p.write(slot, offset, data).map_err(internal)?;
                self.state = after;
                Ok(0)
            }
            Effect::Verify { slot, len } => {
                let sha256 = hash(p, slot, len, Some(&self.stash)).map_err(internal)?;
                let image = image_def(p, slot, len, Some(&self.stash)).map_err(internal)?;
                let (verified, r) = after.verified(&Facts { sha256, image });
                self.state = verified;
                match r? {
                    Effect::Install { slot, len, record } => self.install(p, slot, len, record),
                    _ => Err(HandlerError::Internal),
                }
            }
            Effect::Install { slot, len, record } => {
                self.state = after;
                self.install(p, slot, len, record)
            }
            Effect::Reboot { slot, record } => {
                p.persist(&record).map_err(internal)?;
                self.record = record;
                p.reboot_flash_update(slot).map_err(internal)?;
                self.state = after;
                Ok(0)
            }
            Effect::Buy { record } => {
                p.explicit_buy().map_err(internal)?;
                self.state = after.bought();
                if p.persist(&record).is_ok() {
                    self.record = record;
                }
                Ok(0)
            }
            Effect::Discard { slot, record } => {
                self.guard(slot)?;
                if let Some(r) = record {
                    p.persist(&r).map_err(internal)?;
                    self.record = r;
                }
                self.state = after;
                p.erase(slot, 0).map_err(internal)?;
                Ok(0)
            }
            Effect::Revert => {
                p.reboot_normal().map_err(internal)?;
                self.state = after;
                Ok(0)
            }
        }
    }

    /// [`Effect::Install`]: persist the record (raising `min_version`), then
    /// write sector 0 from the stash and compare it with flash.
    fn install<P: FwPlatform>(
        &mut self,
        p: &mut P,
        slot: Slot,
        len: u32,
        record: FwRecord,
    ) -> Result<usize, HandlerError> {
        self.guard(slot)?;
        if self.record.lowers(&record) {
            return Err(HandlerError::Internal);
        }
        p.persist(&record).map_err(internal)?;
        self.record = record;
        let n = len.min(SECTOR) as usize;
        let first = self.stash.get(..n).ok_or(HandlerError::Internal)?;
        p.erase(slot, 0).map_err(internal)?;
        p.write(slot, 0, first).map_err(internal)?;
        let mut buf = [0u8; READ_STEP];
        // Bounded: at most SECTOR / READ_STEP reads.
        for (i, want) in first.chunks(READ_STEP).enumerate() {
            let got = buf.get_mut(..want.len()).ok_or(HandlerError::Internal)?;
            p.read(slot, (i * READ_STEP) as u32, got)
                .map_err(internal)?;
            if got != want {
                let _ = p.erase(slot, 0);
                return Err(HandlerError::Internal);
            }
        }
        self.state = self.state.installed();
        Ok(0)
    }

    fn info<P: FwPlatform>(
        &self,
        p: &mut P,
        ctx: &Context,
        out: &mut [u8],
    ) -> Result<usize, HandlerError> {
        let a = slot_info(p, Slot::A).map_err(internal)?;
        let b = slot_info(p, Slot::B).map_err(internal)?;
        let (target, version, len, received) = self.state.progress();
        FwInfo {
            state: self.state.code(),
            running: self.boot.running,
            slots: [a, b],
            min_version: self.record.min_version,
            node: ctx.node,
            target,
            version,
            len,
            received,
            self_test_ok: ctx.self_test_ok,
        }
        .encode(out)
        .ok_or(HandlerError::BufferTooSmall)
    }
}
