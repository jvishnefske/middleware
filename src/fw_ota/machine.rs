//! The pure update state machine.
//!
//! [`FwUpdate`] is a value; every transition consumes it and returns the
//! next value with an [`Effect`] for the shell
//! (`service::FwService`, feature `trust`) to perform. Nothing here does I/O:
//! the shell reports what it found ([`Facts`]) back through
//! [`FwUpdate::verified`] and reconciles a boot through [`FwUpdate::boot`].
//!
//! The universal rule: **a refused request leaves the state unchanged**,
//! except a commit whose image fails verification (wrong hash, missing or
//! wrong IMAGE_DEF), which abandons the transfer because the bytes on flash
//! are known to be wrong.
//!
//! Chunks are tracked in a [`ChunkSet`] (one bit per [`CHUNK_LEN`] chunk;
//! a resent chunk is acknowledged without being written again). Flash is
//! erased in [`SECTOR`] units just before the first chunk of each sector is
//! written ([`Effect::Write::erase`]), computed from the set alone. Sector 0
//! (which holds the image's first metadata block) is erased at `begin` and
//! its chunks are only *stashed* ([`Effect::Stash`]); it is written to flash
//! by [`Effect::Install`] after the whole image is verified and the commit
//! authorized, so the inactive slot never holds a bootable image that was
//! not authorized.

use super::picobin::{ImageDef, SENTINEL_FROM};
use super::record::{FwRecord, Pending};
use super::wire::{Command, StateCode, signed_message};
use super::{CHUNK_LEN, CHUNKS_PER_SECTOR, MAX_CHUNKS, MAX_IMAGE, SECTOR, Slot};
use crate::loader::trust::{Signature, TrustState, Version};
use crate::transport::{HandlerError, SenderId};

/// Which chunks of an image have been written: one bit per chunk.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ChunkSet {
    bits: [u32; MAX_CHUNKS / 32],
    count: u32,
}

impl core::fmt::Debug for ChunkSet {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ChunkSet")
            .field("count", &self.count)
            .finish_non_exhaustive()
    }
}

impl ChunkSet {
    /// No chunk.
    pub const EMPTY: ChunkSet = ChunkSet {
        bits: [0; MAX_CHUNKS / 32],
        count: 0,
    };

    /// Whether chunk `index` is in the set (out of range: no).
    pub fn contains(&self, index: u32) -> bool {
        self.bits
            .get(index as usize / 32)
            .is_some_and(|w| w & (1 << (index % 32)) != 0)
    }

    /// The set with chunk `index` added (unchanged when out of range or
    /// already present).
    pub fn with(self, index: u32) -> ChunkSet {
        let mut next = self;
        if let Some(w) = next.bits.get_mut(index as usize / 32)
            && *w & (1 << (index % 32)) == 0
        {
            *w |= 1 << (index % 32);
            next.count += 1;
        }
        next
    }

    /// Number of chunks in the set.
    pub const fn count(&self) -> u32 {
        self.count
    }

    /// Whether any chunk of sector `sector` is in the set.
    pub fn touches_sector(&self, sector: u32) -> bool {
        // CHUNKS_PER_SECTOR (8) divides 32: a sector's bits share one word.
        let Some(first) = sector.checked_mul(CHUNKS_PER_SECTOR) else {
            return false;
        };
        let mask = ((1u64 << CHUNKS_PER_SECTOR) - 1) as u32;
        self.bits
            .get(first as usize / 32)
            .is_some_and(|w| (w >> (first % 32)) & mask != 0)
    }
}

/// A transfer in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transfer {
    /// Target slot (the inactive one).
    pub slot: Slot,
    /// Image length.
    pub len: u32,
    /// Declared version.
    pub version: Version,
    /// Declared SHA-256.
    pub sha256: [u8; 32],
    /// Chunks written (or stashed, in sector 0).
    pub written: ChunkSet,
}

impl Transfer {
    /// Number of chunks of the image.
    pub const fn chunks(&self) -> u32 {
        self.len.div_ceil(CHUNK_LEN as u32)
    }

    /// Whether every chunk has arrived.
    pub const fn complete(&self) -> bool {
        self.written.count() == self.chunks()
    }

    /// Bytes received so far.
    pub fn received(&self) -> u32 {
        let full = self.written.count().saturating_mul(CHUNK_LEN as u32);
        let last = self.chunks().saturating_sub(1);
        let short = (CHUNK_LEN as u32 - self.len % CHUNK_LEN as u32) % CHUNK_LEN as u32;
        if self.written.contains(last) {
            full.saturating_sub(short)
        } else {
            full
        }
    }

    const fn image(&self) -> Image {
        Image {
            slot: self.slot,
            len: self.len,
            version: self.version,
            sha256: self.sha256,
        }
    }
}

/// A complete, verified image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Image {
    /// Its slot.
    pub slot: Slot,
    /// Length.
    pub len: u32,
    /// Version.
    pub version: Version,
    /// SHA-256 (as transferred, TBYB flag still set).
    pub sha256: [u8; 32],
}

impl Image {
    fn pending(&self, rebooted: bool) -> Pending {
        Pending {
            slot: self.slot,
            version: self.version,
            len: self.len,
            sha256: self.sha256,
            rebooted,
        }
    }
}

/// Update progress. See the [`super`] module docs for the diagram.
///
/// `Receiving` carries the 1 KiB [`ChunkSet`]; the other variants are
/// small. The value lives in one place (the service) and the crate has no
/// heap to box it in, so the size difference is accepted.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FwUpdate {
    /// Nothing in progress.
    Idle,
    /// Receiving chunks.
    Receiving(Transfer),
    /// Verified and authorized; [`Effect::Install`] has not completed.
    Verified(Image),
    /// Installed and recorded; awaiting `sys/fw/reboot`.
    Committed(Image),
    /// Booted the new image on trial (the bootrom reports a buy pending).
    Trial {
        /// The running (trial) slot.
        slot: Slot,
        /// Its version, when this device committed it.
        version: Option<Version>,
    },
    /// The trial image was bought.
    Confirmed {
        /// Its slot (now running).
        slot: Slot,
        /// Its version, when known.
        version: Option<Version>,
    },
    /// A committed image did not stick: its trial ended without a buy.
    RolledBack {
        /// The slot that held it.
        slot: Slot,
        /// Its version.
        version: Version,
    },
}

/// What the state machine needs to know about the device, supplied by the
/// shell on every step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Env {
    /// The slot the firmware booted from.
    pub running: Slot,
    /// The running image's version (`major << 16 | minor`; 0 without one).
    pub running_version: Version,
    /// The persisted record.
    pub record: FwRecord,
    /// Largest image the inactive slot holds (the partition size).
    pub capacity: u32,
    /// This device's node id (bound into the signature).
    pub node: SenderId,
    /// The root of trust (the same record the payload loader uses).
    pub trust: TrustState,
    /// The running firmware's self-test passed.
    pub self_test_ok: bool,
}

impl Env {
    /// A new image's version must exceed this: the larger of the persisted
    /// `min_version` and the running image's version.
    pub fn floor(&self) -> Version {
        self.record.min_version.max(self.running_version)
    }
}

/// What the shell found when it read the transferred image back
/// ([`Effect::Verify`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Facts {
    /// SHA-256 of the image (sector 0 from the stash, the rest from flash).
    pub sha256: [u8; 32],
    /// Its IMAGE_DEF, if it has one.
    pub image: Option<ImageDef>,
}

/// What the shell must do after a transition. Each variant names the slot
/// it touches; the shell refuses (`Internal`) any that is the running slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect<'a> {
    /// Nothing; reply success with an empty body.
    None,
    /// Reply with `sys/fw/info` ([`super::wire::FwInfo`]).
    Info,
    /// A transfer into `slot` started: when `fresh`, clear the sector-0
    /// stash and erase sector 0 of `slot`; when not (a resumed transfer),
    /// nothing. Reply `[slot u8]`.
    Begin {
        /// Target slot.
        slot: Slot,
        /// A new transfer (else the same transfer resumed).
        fresh: bool,
    },
    /// Copy `data` into the sector-0 stash at `offset`.
    Stash {
        /// Offset in the image (< [`SECTOR`]).
        offset: u32,
        /// Bytes.
        data: &'a [u8],
    },
    /// Erase sector `erase` of `slot` if given, then write `data` at
    /// `offset` of `slot`.
    Write {
        /// Target slot.
        slot: Slot,
        /// Offset in the image.
        offset: u32,
        /// Bytes.
        data: &'a [u8],
        /// The sector to erase first (its first chunk).
        erase: Option<u32>,
    },
    /// Hash the `len`-byte image (sector 0 from the stash, the rest from
    /// `slot`), read its IMAGE_DEF, and pass the [`Facts`] to
    /// [`FwUpdate::verified`].
    Verify {
        /// Target slot.
        slot: Slot,
        /// Image length.
        len: u32,
    },
    /// Persist `record`, then erase sector 0 of `slot`, write the stash
    /// there and read it back to compare; then call [`FwUpdate::installed`].
    Install {
        /// Target slot.
        slot: Slot,
        /// Image length.
        len: u32,
        /// The record to persist first (raises `min_version`).
        record: FwRecord,
    },
    /// Persist `record`, then request a flash-update reboot into `slot`.
    Reboot {
        /// The committed slot.
        slot: Slot,
        /// The record (pending marked rebooted).
        record: FwRecord,
    },
    /// Call the bootrom's explicit buy, then persist `record`.
    Buy {
        /// The record (no update pending).
        record: FwRecord,
    },
    /// Persist `record` if given, then erase sector 0 of `slot`.
    Discard {
        /// The abandoned slot.
        slot: Slot,
        /// The record to persist first.
        record: Option<FwRecord>,
    },
    /// Reboot normally (the unbought trial image is skipped by the bootrom).
    Revert,
}

/// What [`FwUpdate::boot`] asks of the shell at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootAction {
    /// Nothing.
    None,
    /// Persist the record.
    Persist(FwRecord),
    /// Persist the record, then erase sector 0 of the slot.
    Discard(Slot, FwRecord),
}

/// What the shell knows at boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootFacts {
    /// The slot the bootrom booted.
    pub running: Slot,
    /// The bootrom reports a buy pending (a TBYB image on trial).
    pub trial: bool,
    /// The persisted record.
    pub record: FwRecord,
    /// For a pending, not yet rebooted update in the other slot: whether
    /// that slot still holds exactly the committed image (hash and length).
    pub pending_intact: bool,
}

type Step<'a> = (FwUpdate, Result<Effect<'a>, HandlerError>);

/// Checks a commit's authorization against the root of trust: unclaimed
/// devices accept any commit (trust on first use, as for payloads); a
/// claimed device needs a signature by its pinned key that verifies
/// strictly over `message`. Without the `trust` feature nothing can be
/// verified, so a claimed device refuses every commit.
fn authorize(
    trust: &TrustState,
    signature: Option<&Signature>,
    message: &[u8],
) -> Result<(), HandlerError> {
    let TrustState::Claimed { key, .. } = trust else {
        return Ok(());
    };
    let signature = signature.ok_or(HandlerError::Denied)?;
    verify(&key.0, &signature.0, message)
}

#[cfg(feature = "trust")]
fn verify(key: &[u8; 32], signature: &[u8; 64], message: &[u8]) -> Result<(), HandlerError> {
    let vk = ed25519_dalek::VerifyingKey::from_bytes(key).map_err(|_| HandlerError::Denied)?;
    let sig = ed25519_dalek::Signature::from_bytes(signature);
    vk.verify_strict(message, &sig)
        .map_err(|_| HandlerError::Denied)
}

#[cfg(not(feature = "trust"))]
fn verify(_key: &[u8; 32], _signature: &[u8; 64], _message: &[u8]) -> Result<(), HandlerError> {
    Err(HandlerError::Denied)
}

/// Whether `def` is an image this device may install as `version`: an
/// RP2350 executable flagged TBYB, without a rollback version, whose
/// `major << 16 | minor` equals the declared version, and whose block loop
/// has a block in the first sector's last page
/// ([`super::picobin::SENTINEL_FROM`], so a torn explicit buy cannot leave
/// a bootable image).
pub fn acceptable(def: &ImageDef, version: Version) -> bool {
    def.is_exe()
        && def.is_rp2350()
        && def.is_tbyb()
        && def
            .version
            .is_some_and(|v| v.rollback.is_none() && v.as_u32() == version.0)
        && def.window_tail >= SENTINEL_FROM
}

impl FwUpdate {
    /// The `sys/fw/info` state byte.
    pub const fn code(&self) -> StateCode {
        match self {
            FwUpdate::Idle => StateCode::Idle,
            FwUpdate::Receiving(_) => StateCode::Receiving,
            FwUpdate::Verified(_) => StateCode::Verified,
            FwUpdate::Committed(_) => StateCode::Committed,
            FwUpdate::Trial { .. } => StateCode::Trial,
            FwUpdate::Confirmed { .. } => StateCode::Confirmed,
            FwUpdate::RolledBack { .. } => StateCode::RolledBack,
        }
    }

    /// The state after a boot, and what the shell must persist (see the
    /// power-loss table in the [`super`] module docs).
    pub fn boot(facts: &BootFacts) -> (FwUpdate, BootAction) {
        let record = facts.record;
        let pending = record.pending;
        if facts.trial {
            let version = pending
                .filter(|p| p.slot == facts.running)
                .map(|p| p.version);
            return (
                FwUpdate::Trial {
                    slot: facts.running,
                    version,
                },
                BootAction::None,
            );
        }
        let Some(p) = pending else {
            return (FwUpdate::Idle, BootAction::None);
        };
        let settled = record.settled();
        if p.slot == facts.running {
            // Bought (the TBYB flag is clear, so the bootrom booted it
            // normally) before the record was settled.
            (
                FwUpdate::Confirmed {
                    slot: p.slot,
                    version: Some(p.version),
                },
                BootAction::Persist(settled),
            )
        } else if p.rebooted {
            (
                FwUpdate::RolledBack {
                    slot: p.slot,
                    version: p.version,
                },
                BootAction::Persist(settled),
            )
        } else if facts.pending_intact {
            let image = Image {
                slot: p.slot,
                len: p.len,
                version: p.version,
                sha256: p.sha256,
            };
            (FwUpdate::Committed(image), BootAction::None)
        } else {
            (FwUpdate::Idle, BootAction::Discard(p.slot, settled))
        }
    }

    /// Applies a request.
    pub fn step<'a>(self, cmd: &Command<'a>, env: &Env) -> Step<'a> {
        match *cmd {
            Command::Info => (self, Ok(Effect::Info)),
            Command::Begin {
                len,
                version,
                sha256,
            } => self.begin(len, version, sha256, env),
            Command::Chunk { offset, data } => self.chunk(offset, data),
            Command::Commit { signature } => self.commit(signature.as_ref(), env),
            Command::Reboot => match self {
                FwUpdate::Committed(image) => {
                    let record = FwRecord {
                        pending: Some(image.pending(true)),
                        ..env.record
                    };
                    (
                        self,
                        Ok(Effect::Reboot {
                            slot: image.slot,
                            record,
                        }),
                    )
                }
                _ => (self, Err(HandlerError::InvalidRequest)),
            },
            Command::Confirm => match self {
                FwUpdate::Trial { .. } if !env.self_test_ok => {
                    (self, Err(HandlerError::Unavailable))
                }
                FwUpdate::Trial { .. } => (
                    self,
                    Ok(Effect::Buy {
                        record: env.record.settled(),
                    }),
                ),
                FwUpdate::Confirmed { .. } => (self, Ok(Effect::None)),
                _ => (self, Err(HandlerError::InvalidRequest)),
            },
            Command::Abort => match self {
                FwUpdate::Receiving(Transfer { slot, .. }) => {
                    (FwUpdate::Idle, Ok(Effect::Discard { slot, record: None }))
                }
                // Install may have persisted the pending update already.
                FwUpdate::Verified(Image { slot, .. }) => (
                    FwUpdate::Idle,
                    Ok(Effect::Discard {
                        slot,
                        record: Some(env.record.settled()),
                    }),
                ),
                FwUpdate::Committed(image) => (
                    FwUpdate::Idle,
                    Ok(Effect::Discard {
                        slot: image.slot,
                        record: Some(env.record.settled()),
                    }),
                ),
                FwUpdate::Trial { .. } => (self, Ok(Effect::Revert)),
                FwUpdate::Idle | FwUpdate::Confirmed { .. } | FwUpdate::RolledBack { .. } => {
                    (self, Ok(Effect::None))
                }
            },
        }
    }

    fn begin<'a>(self, len: u32, version: Version, sha256: [u8; 32], env: &Env) -> Step<'a> {
        match self {
            FwUpdate::Committed(_) | FwUpdate::Trial { .. } => {
                return (self, Err(HandlerError::InvalidRequest));
            }
            _ => {}
        }
        if len == 0 || len > env.capacity.min(MAX_IMAGE) {
            return (self, Err(HandlerError::BufferTooSmall));
        }
        if version <= env.floor() {
            return (self, Err(HandlerError::Denied));
        }
        let slot = env.running.other();
        if let FwUpdate::Receiving(t) = self
            && t.slot == slot
            && t.len == len
            && t.version == version
            && t.sha256 == sha256
        {
            return (self, Ok(Effect::Begin { slot, fresh: false }));
        }
        let t = Transfer {
            slot,
            len,
            version,
            sha256,
            written: ChunkSet::EMPTY,
        };
        (
            FwUpdate::Receiving(t),
            Ok(Effect::Begin { slot, fresh: true }),
        )
    }

    fn chunk<'a>(self, offset: u32, data: &'a [u8]) -> Step<'a> {
        let FwUpdate::Receiving(t) = self else {
            return (self, Err(HandlerError::InvalidRequest));
        };
        let chunk = CHUNK_LEN as u32;
        let expected = t.len.saturating_sub(offset).min(chunk);
        if !offset.is_multiple_of(chunk) || offset >= t.len || data.len() as u32 != expected {
            return (self, Err(HandlerError::InvalidRequest));
        }
        let index = offset / chunk;
        if t.written.contains(index) {
            return (self, Ok(Effect::None));
        }
        let next = FwUpdate::Receiving(Transfer {
            written: t.written.with(index),
            ..t
        });
        let sector = offset / SECTOR;
        if sector == 0 {
            return (next, Ok(Effect::Stash { offset, data }));
        }
        let erase = (!t.written.touches_sector(sector)).then_some(sector);
        (
            next,
            Ok(Effect::Write {
                slot: t.slot,
                offset,
                data,
                erase,
            }),
        )
    }

    fn commit<'a>(self, signature: Option<&Signature>, env: &Env) -> Step<'a> {
        match self {
            FwUpdate::Committed(_) => (self, Ok(Effect::None)),
            FwUpdate::Verified(image) => {
                let record = FwRecord {
                    min_version: image.version,
                    pending: Some(image.pending(false)),
                };
                (
                    self,
                    Ok(Effect::Install {
                        slot: image.slot,
                        len: image.len,
                        record,
                    }),
                )
            }
            FwUpdate::Receiving(t) => {
                if !t.complete() {
                    return (self, Err(HandlerError::InvalidRequest));
                }
                if t.version <= env.floor() {
                    return (self, Err(HandlerError::Denied));
                }
                let message = signed_message(t.len, t.version, t.slot, &env.node, &t.sha256);
                if let Err(e) = authorize(&env.trust, signature, &message) {
                    return (self, Err(e));
                }
                (
                    self,
                    Ok(Effect::Verify {
                        slot: t.slot,
                        len: t.len,
                    }),
                )
            }
            _ => (self, Err(HandlerError::InvalidRequest)),
        }
    }

    /// The result of [`Effect::Verify`]: the transfer becomes
    /// [`FwUpdate::Verified`] with an [`Effect::Install`] when the hash
    /// matches the declared one and the IMAGE_DEF is [`acceptable`];
    /// otherwise it is abandoned (`Idle`, `InvalidRequest`). Any other
    /// state is unchanged (`InvalidRequest`).
    pub fn verified<'a>(self, facts: &Facts) -> Step<'a> {
        let FwUpdate::Receiving(t) = self else {
            return (self, Err(HandlerError::InvalidRequest));
        };
        let ok = facts.sha256 == t.sha256 && facts.image.is_some_and(|d| acceptable(&d, t.version));
        if !ok {
            return (FwUpdate::Idle, Err(HandlerError::InvalidRequest));
        }
        let image = t.image();
        let record = FwRecord {
            min_version: image.version,
            pending: Some(image.pending(false)),
        };
        (
            FwUpdate::Verified(image),
            Ok(Effect::Install {
                slot: image.slot,
                len: image.len,
                record,
            }),
        )
    }

    /// [`Effect::Install`] completed: `Verified` becomes `Committed`.
    pub fn installed(self) -> FwUpdate {
        match self {
            FwUpdate::Verified(image) => FwUpdate::Committed(image),
            other => other,
        }
    }

    /// [`Effect::Buy`] completed: `Trial` becomes `Confirmed`.
    pub fn bought(self) -> FwUpdate {
        match self {
            FwUpdate::Trial { slot, version } => FwUpdate::Confirmed { slot, version },
            other => other,
        }
    }

    /// The slot, version, length and received bytes this state reports in
    /// `sys/fw/info`.
    pub fn progress(&self) -> (Option<Slot>, Version, u32, u32) {
        match *self {
            FwUpdate::Idle => (None, Version(0), 0, 0),
            FwUpdate::Receiving(t) => (Some(t.slot), t.version, t.len, t.received()),
            FwUpdate::Verified(i) | FwUpdate::Committed(i) => {
                (Some(i.slot), i.version, i.len, i.len)
            }
            FwUpdate::Trial { slot, version } | FwUpdate::Confirmed { slot, version } => {
                (Some(slot), version.unwrap_or_default(), 0, 0)
            }
            FwUpdate::RolledBack { slot, version } => (Some(slot), version, 0, 0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fw_ota::picobin::{EXE_TBYB, ImageVersion, RP2350_ARM_S_EXE};

    const NODE: SenderId = SenderId([0x02, 0xFA, 0x4E, 0, 0, 2, 0, 0]);

    fn env() -> Env {
        Env {
            running: Slot::A,
            running_version: Version(0x0001_0000),
            record: FwRecord::EMPTY,
            capacity: 64 * 1024,
            node: NODE,
            trust: TrustState::Unclaimed,
            self_test_ok: true,
        }
    }

    fn begin(len: u32, version: u32) -> Command<'static> {
        Command::Begin {
            len,
            version: Version(version),
            sha256: [1; 32],
        }
    }

    fn def(version: u32, flags: u16) -> ImageDef {
        ImageDef {
            flags,
            version: Some(ImageVersion {
                major: (version >> 16) as u16,
                minor: version as u16,
                rollback: None,
            }),
            type_offset: 4,
            window_tail: SENTINEL_FROM,
        }
    }

    fn facts(version: u32) -> Facts {
        Facts {
            sha256: [1; 32],
            image: Some(def(version, RP2350_ARM_S_EXE | EXE_TBYB)),
        }
    }

    /// Sends every chunk of a `len`-byte image (sectors 1.. first, then
    /// sector 0), returning the state and the effects' erase sectors.
    fn fill(mut s: FwUpdate, len: u32, env: &Env) -> (FwUpdate, [u32; 32], usize) {
        let data = [0u8; CHUNK_LEN];
        let mut erased = [0u32; 32];
        let mut n = 0;
        let chunks = len.div_ceil(CHUNK_LEN as u32);
        let order = (CHUNKS_PER_SECTOR..chunks).chain(0..CHUNKS_PER_SECTOR.min(chunks));
        for i in order {
            let offset = i * CHUNK_LEN as u32;
            let size = (len - offset).min(CHUNK_LEN as u32) as usize;
            let (next, e) = s.step(
                &Command::Chunk {
                    offset,
                    data: &data[..size],
                },
                env,
            );
            match e {
                Ok(Effect::Write {
                    erase: Some(sec),
                    slot,
                    ..
                }) => {
                    assert_eq!(slot, Slot::B);
                    erased[n] = sec;
                    n += 1;
                }
                Ok(Effect::Write { erase: None, .. }) | Ok(Effect::Stash { .. }) => {}
                other => panic!("chunk {i}: {other:?}"),
            }
            s = next;
        }
        (s, erased, n)
    }

    #[test]
    fn y3_happy_path_reaches_committed_with_sector_erases_computed() {
        let env = env();
        let len = 3 * SECTOR + 100;
        let (s, e) = FwUpdate::Idle.step(&begin(len, 0x0001_0001), &env);
        assert_eq!(
            e,
            Ok(Effect::Begin {
                slot: Slot::B,
                fresh: true
            })
        );
        assert_eq!(s.code(), StateCode::Receiving);
        let (s, erased, n) = fill(s, len, &env);
        // One erase per sector, on its first chunk; sector 0 was erased at
        // begin and is only stashed.
        assert_eq!(&erased[..n], &[1, 2, 3]);
        assert_eq!(
            s.progress(),
            (Some(Slot::B), Version(0x0001_0001), len, len)
        );
        let (s, e) = s.step(&Command::Commit { signature: None }, &env);
        assert_eq!(e, Ok(Effect::Verify { slot: Slot::B, len }));
        let (s, e) = s.verified(&facts(0x0001_0001));
        let image = Image {
            slot: Slot::B,
            len,
            version: Version(0x0001_0001),
            sha256: [1; 32],
        };
        let record = FwRecord {
            min_version: image.version,
            pending: Some(image.pending(false)),
        };
        assert_eq!(
            e,
            Ok(Effect::Install {
                slot: Slot::B,
                len,
                record
            })
        );
        assert_eq!(s, FwUpdate::Verified(image));
        let s = s.installed();
        assert_eq!(s, FwUpdate::Committed(image));
        // A resent commit is acknowledged.
        assert_eq!(
            s.step(&Command::Commit { signature: None }, &env),
            (s, Ok(Effect::None))
        );
        let env2 = Env { record, ..env };
        let (s2, e) = s.step(&Command::Reboot, &env2);
        assert_eq!(s2, s);
        assert_eq!(
            e,
            Ok(Effect::Reboot {
                slot: Slot::B,
                record: FwRecord {
                    pending: Some(image.pending(true)),
                    ..record
                }
            })
        );
    }

    #[test]
    fn y3_chunks_are_idempotent_and_bounded() {
        let env = env();
        let len = SECTOR + 700; // chunks 0..=9, the last 188 bytes
        let (s, _) = FwUpdate::Idle.step(&begin(len, 0x0001_0001), &env);
        let data = [3u8; CHUNK_LEN];
        let c = |offset: u32, n: usize| Command::Chunk {
            offset,
            data: &data[..n],
        };
        let (s1, e) = s.step(&c(SECTOR, 512), &env);
        assert!(matches!(e, Ok(Effect::Write { erase: Some(1), .. })));
        // Resend: acknowledged, nothing written, state unchanged.
        assert_eq!(s1.step(&c(SECTOR, 512), &env), (s1, Ok(Effect::None)));
        // The second chunk of the sector is written without an erase.
        let (s2, e) = s1.step(&c(SECTOR + 512, 188), &env);
        assert!(
            matches!(e, Ok(Effect::Write { erase: None, offset, .. }) if offset == SECTOR + 512)
        );
        // Misaligned (overlapping), out of range, wrong length: refused,
        // state unchanged.
        for (off, n) in [
            (SECTOR + 1, 511),
            (256, 512),
            (len, 1),
            (u32::MAX - 511, 512),
            (0, 511),
            (SECTOR + 512, 187),
            (SECTOR + 512, 189),
        ] {
            assert_eq!(
                s2.step(&c(off, n), &env),
                (s2, Err(HandlerError::InvalidRequest)),
                "{off} {n}"
            );
        }
        assert_eq!(s2.progress().3, 512 + 188);
        // Commit before every chunk arrived: refused, state unchanged.
        assert_eq!(
            s2.step(&Command::Commit { signature: None }, &env),
            (s2, Err(HandlerError::InvalidRequest))
        );
        // Chunks outside a transfer.
        assert_eq!(
            FwUpdate::Idle.step(&c(0, 512), &env),
            (FwUpdate::Idle, Err(HandlerError::InvalidRequest))
        );
    }

    #[test]
    fn y3_begin_targets_the_inactive_slot_and_checks_size_and_version() {
        let env = env();
        let (_, e) = FwUpdate::Idle.step(
            &begin(100, 0x0001_0001),
            &Env {
                running: Slot::B,
                ..env
            },
        );
        assert_eq!(
            e,
            Ok(Effect::Begin {
                slot: Slot::A,
                fresh: true
            })
        );
        assert_eq!(
            FwUpdate::Idle.step(&begin(0, 0x0002_0000), &env).1,
            Err(HandlerError::BufferTooSmall)
        );
        assert_eq!(
            FwUpdate::Idle
                .step(&begin(env.capacity + 1, 0x0002_0000), &env)
                .1,
            Err(HandlerError::BufferTooSmall)
        );
        let big = Env {
            capacity: u32::MAX,
            ..env
        };
        assert_eq!(
            FwUpdate::Idle
                .step(&begin(MAX_IMAGE + 1, 0x0002_0000), &big)
                .1,
            Err(HandlerError::BufferTooSmall)
        );
        // Anti-rollback: not above the running version, nor above min_version.
        assert_eq!(
            FwUpdate::Idle.step(&begin(100, 0x0001_0000), &env).1,
            Err(HandlerError::Denied)
        );
        assert_eq!(
            FwUpdate::Idle.step(&begin(100, 0x0000_ffff), &env).1,
            Err(HandlerError::Denied)
        );
        let floor = Env {
            record: FwRecord {
                min_version: Version(0x0003_0000),
                pending: None,
            },
            ..env
        };
        assert_eq!(
            FwUpdate::Idle.step(&begin(100, 0x0002_0000), &floor).1,
            Err(HandlerError::Denied)
        );
        assert_eq!(floor.floor(), Version(0x0003_0000));
        // The same begin resumes; a different one restarts.
        let (s, _) = FwUpdate::Idle.step(&begin(1000, 0x0001_0001), &env);
        let (s, _) = s.step(
            &Command::Chunk {
                offset: 512,
                data: &[0; 488],
            },
            &env,
        );
        let (s2, e) = s.step(&begin(1000, 0x0001_0001), &env);
        assert_eq!(
            (s2, e),
            (
                s,
                Ok(Effect::Begin {
                    slot: Slot::B,
                    fresh: false
                })
            )
        );
        let (s3, e) = s.step(&begin(1000, 0x0001_0002), &env);
        assert_eq!(
            e,
            Ok(Effect::Begin {
                slot: Slot::B,
                fresh: true
            })
        );
        assert_eq!(s3.progress().3, 0);
        // A begin in Committed or Trial is refused (abort first).
        let image = Image {
            slot: Slot::B,
            len: 1,
            version: Version(9),
            sha256: [0; 32],
        };
        for s in [
            FwUpdate::Committed(image),
            FwUpdate::Trial {
                slot: Slot::A,
                version: None,
            },
        ] {
            assert_eq!(
                s.step(&begin(100, 0x0002_0000), &env),
                (s, Err(HandlerError::InvalidRequest))
            );
        }
    }

    #[test]
    fn y3_verification_failures_abandon_the_transfer() {
        let env = env();
        let (s, _) = FwUpdate::Idle.step(&begin(600, 0x0001_0001), &env);
        let (s, ..) = fill(s, 600, &env);
        let (s, e) = s.step(&Command::Commit { signature: None }, &env);
        assert!(matches!(e, Ok(Effect::Verify { .. })));
        let wrong_hash = Facts {
            sha256: [2; 32],
            ..facts(0x0001_0001)
        };
        let no_def = Facts {
            image: None,
            ..facts(0x0001_0001)
        };
        let other_version = facts(0x0001_0002);
        let not_tbyb = Facts {
            image: Some(def(0x0001_0001, RP2350_ARM_S_EXE)),
            ..facts(0)
        };
        let rp2040 = Facts {
            image: Some(def(0x0001_0001, 0x0021 | EXE_TBYB)),
            ..facts(0)
        };
        let mut rollback = def(0x0001_0001, RP2350_ARM_S_EXE | EXE_TBYB);
        rollback.version = rollback.version.map(|v| ImageVersion {
            rollback: Some(1),
            ..v
        });
        let rollback = Facts {
            image: Some(rollback),
            ..facts(0)
        };
        for f in [
            wrong_hash,
            no_def,
            other_version,
            not_tbyb,
            rp2040,
            rollback,
        ] {
            assert_eq!(
                s.verified(&f),
                (FwUpdate::Idle, Err(HandlerError::InvalidRequest)),
                "{f:?}"
            );
        }
        assert_eq!(
            FwUpdate::Idle.verified(&facts(1)),
            (FwUpdate::Idle, Err(HandlerError::InvalidRequest))
        );
        // A commit whose version fell below a floor raised meanwhile.
        let raised = Env {
            running_version: Version(0x0001_0001),
            ..env
        };
        assert_eq!(
            s.step(&Command::Commit { signature: None }, &raised),
            (s, Err(HandlerError::Denied))
        );
    }

    #[cfg(feature = "trust")]
    #[test]
    fn y4_claimed_devices_need_a_signature_binding_slot_node_and_version() {
        use crate::loader::trust::PublicKey;
        use ed25519_dalek::{Signer, SigningKey};
        let sk = SigningKey::from_bytes(&[7; 32]);
        let key = PublicKey(sk.verifying_key().to_bytes());
        let env = Env {
            trust: TrustState::Claimed {
                key,
                min_version: Version(0),
            },
            ..env()
        };
        let (s, _) = FwUpdate::Idle.step(&begin(600, 0x0001_0001), &env);
        let (s, ..) = fill(s, 600, &env);
        let sign = |slot, node: &SenderId, version| {
            Some(Signature(
                sk.sign(&signed_message(600, Version(version), slot, node, &[1; 32]))
                    .to_bytes(),
            ))
        };
        let good = sign(Slot::B, &NODE, 0x0001_0001);
        let other = SigningKey::from_bytes(&[8; 32]);
        let foreign = Some(Signature(
            other
                .sign(&signed_message(
                    600,
                    Version(0x0001_0001),
                    Slot::B,
                    &NODE,
                    &[1; 32],
                ))
                .to_bytes(),
        ));
        for bad in [
            None,
            sign(Slot::A, &NODE, 0x0001_0001),
            sign(Slot::B, &SenderId([0; 8]), 0x0001_0001),
            sign(Slot::B, &NODE, 0x0001_0002),
            foreign,
            Some(Signature([0; 64])),
        ] {
            assert_eq!(
                s.step(&Command::Commit { signature: bad }, &env),
                (s, Err(HandlerError::Denied))
            );
        }
        assert_eq!(
            s.step(&Command::Commit { signature: good }, &env).1,
            Ok(Effect::Verify {
                slot: Slot::B,
                len: 600
            })
        );
        // Unclaimed: any commit is accepted (trust on first use).
        assert!(matches!(
            s.step(
                &Command::Commit { signature: None },
                &Env {
                    trust: TrustState::Unclaimed,
                    ..env
                }
            )
            .1,
            Ok(Effect::Verify { .. })
        ));
    }

    #[cfg(not(feature = "trust"))]
    #[test]
    fn y4_without_crypto_a_claimed_device_refuses_every_commit() {
        use crate::loader::trust::PublicKey;
        let env = Env {
            trust: TrustState::Claimed {
                key: PublicKey([1; 32]),
                min_version: Version(0),
            },
            ..env()
        };
        let (s, _) = FwUpdate::Idle.step(&begin(600, 0x0001_0001), &env);
        let (s, ..) = fill(s, 600, &env);
        assert_eq!(
            s.step(
                &Command::Commit {
                    signature: Some(Signature([0; 64]))
                },
                &env
            )
            .1,
            Err(HandlerError::Denied)
        );
    }

    #[test]
    fn y5_reboot_confirm_and_abort_follow_the_state() {
        let env = env();
        let image = Image {
            slot: Slot::B,
            len: 10,
            version: Version(0x0001_0001),
            sha256: [4; 32],
        };
        let receiving = FwUpdate::Receiving(Transfer {
            slot: Slot::B,
            len: 10,
            version: Version(2),
            sha256: [0; 32],
            written: ChunkSet::EMPTY,
        });
        let trial = FwUpdate::Trial {
            slot: Slot::B,
            version: Some(image.version),
        };
        // Reboot only when committed.
        for s in [FwUpdate::Idle, receiving, FwUpdate::Verified(image), trial] {
            assert_eq!(
                s.step(&Command::Reboot, &env),
                (s, Err(HandlerError::InvalidRequest))
            );
        }
        // Confirm only on trial, after the self-test; idempotent once bought.
        for s in [
            FwUpdate::Idle,
            receiving,
            FwUpdate::Committed(image),
            FwUpdate::RolledBack {
                slot: Slot::B,
                version: Version(1),
            },
        ] {
            assert_eq!(
                s.step(&Command::Confirm, &env),
                (s, Err(HandlerError::InvalidRequest))
            );
        }
        let untested = Env {
            self_test_ok: false,
            ..env
        };
        assert_eq!(
            trial.step(&Command::Confirm, &untested),
            (trial, Err(HandlerError::Unavailable))
        );
        let pending = FwRecord {
            min_version: image.version,
            pending: Some(image.pending(true)),
        };
        let (s, e) = trial.step(
            &Command::Confirm,
            &Env {
                record: pending,
                ..env
            },
        );
        assert_eq!(
            e,
            Ok(Effect::Buy {
                record: pending.settled()
            })
        );
        let s = s.bought();
        assert_eq!(
            s,
            FwUpdate::Confirmed {
                slot: Slot::B,
                version: Some(image.version)
            }
        );
        assert_eq!(s.step(&Command::Confirm, &env), (s, Ok(Effect::None)));
        // Abort.
        assert_eq!(
            receiving.step(&Command::Abort, &env),
            (
                FwUpdate::Idle,
                Ok(Effect::Discard {
                    slot: Slot::B,
                    record: None
                })
            )
        );
        assert_eq!(
            FwUpdate::Committed(image).step(
                &Command::Abort,
                &Env {
                    record: pending,
                    ..env
                }
            ),
            (
                FwUpdate::Idle,
                Ok(Effect::Discard {
                    slot: Slot::B,
                    record: Some(pending.settled())
                })
            )
        );
        assert_eq!(
            trial.step(&Command::Abort, &env),
            (trial, Ok(Effect::Revert))
        );
        assert_eq!(
            FwUpdate::Idle.step(&Command::Abort, &env),
            (FwUpdate::Idle, Ok(Effect::None))
        );
        assert_eq!(
            FwUpdate::Idle.step(&Command::Info, &env),
            (FwUpdate::Idle, Ok(Effect::Info))
        );
        // installed / bought leave other states alone.
        assert_eq!(FwUpdate::Idle.installed(), FwUpdate::Idle);
        assert_eq!(FwUpdate::Idle.bought(), FwUpdate::Idle);
    }

    #[test]
    fn y6_boot_reconciles_every_record_with_the_bootrom_state() {
        let image = Image {
            slot: Slot::B,
            len: 10,
            version: Version(0x0001_0001),
            sha256: [4; 32],
        };
        let rec = |rebooted| FwRecord {
            min_version: image.version,
            pending: Some(image.pending(rebooted)),
        };
        let facts = |running, trial, record, pending_intact| BootFacts {
            running,
            trial,
            record,
            pending_intact,
        };
        let settled = rec(false).settled();
        // Nothing pending.
        assert_eq!(
            FwUpdate::boot(&facts(Slot::A, false, settled, false)),
            (FwUpdate::Idle, BootAction::None)
        );
        // Trial of our image, and a TBYB image nobody committed here.
        assert_eq!(
            FwUpdate::boot(&facts(Slot::B, true, rec(true), false)),
            (
                FwUpdate::Trial {
                    slot: Slot::B,
                    version: Some(image.version)
                },
                BootAction::None
            )
        );
        assert_eq!(
            FwUpdate::boot(&facts(Slot::A, true, settled, false)),
            (
                FwUpdate::Trial {
                    slot: Slot::A,
                    version: None
                },
                BootAction::None
            )
        );
        // Bought before the record was settled.
        assert_eq!(
            FwUpdate::boot(&facts(Slot::B, false, rec(true), false)),
            (
                FwUpdate::Confirmed {
                    slot: Slot::B,
                    version: Some(image.version)
                },
                BootAction::Persist(settled)
            )
        );
        // The trial ended without a buy.
        assert_eq!(
            FwUpdate::boot(&facts(Slot::A, false, rec(true), true)),
            (
                FwUpdate::RolledBack {
                    slot: Slot::B,
                    version: image.version
                },
                BootAction::Persist(settled)
            )
        );
        // Committed, power lost before the reboot: still committed if the
        // slot is intact, else discarded.
        assert_eq!(
            FwUpdate::boot(&facts(Slot::A, false, rec(false), true)),
            (FwUpdate::Committed(image), BootAction::None)
        );
        assert_eq!(
            FwUpdate::boot(&facts(Slot::A, false, rec(false), false)),
            (FwUpdate::Idle, BootAction::Discard(Slot::B, settled))
        );
    }

    #[test]
    fn y3_chunk_set_counts_and_sectors() {
        let s = ChunkSet::EMPTY.with(9).with(9).with(MAX_CHUNKS as u32);
        assert_eq!(s.count(), 1);
        assert!(s.contains(9) && !s.contains(8) && !s.contains(u32::MAX));
        assert!(s.touches_sector(1) && !s.touches_sector(0) && !s.touches_sector(2));
        assert!(!s.touches_sector(u32::MAX / 8));
        let last = ChunkSet::EMPTY.with(MAX_CHUNKS as u32 - 1);
        assert!(last.touches_sector(MAX_CHUNKS as u32 / CHUNKS_PER_SECTOR - 1));
    }
}
