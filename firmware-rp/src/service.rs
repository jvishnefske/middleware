//! The request service: the base firmware's request handling and storage
//! effects over any NOR flash.
//!
//! [`Service`] owns the dispatcher state, the payload store and the trust
//! record. [`Service::handle`] takes one request and:
//!
//! 1. hands a `sys/fw/*` key to the firmware-update backend
//!    ([`crate::fw_update`]) with the flash (lent by the payload store),
//!    the node id, the trust store's root of trust and the
//!    [`SelfTest`](crate::fw_update::SelfTest) (USB configured, store and
//!    trust record open); the backend never touches the payload loader
//!    state;
//! 2. otherwise decides it with `esp_base::dispatch::handle_request`, the
//!    ESP base firmware's own pure dispatcher (the same request table,
//!    loader state machine, staging and trust rules), with no re-key window
//!    (the boards have no presence button wired) and no staging (requests
//!    come over USB, never over the air);
//! 3. performs the storage action on the flash, as the esp-base controller
//!    does: erase and write the inactive slot, activate it (with a raised
//!    `min_version` saved inside `PayloadStore::activate_with`, after the
//!    CRC check and before the slot header), save a claimed key. A failed
//!    action replies `Internal` and leaves the state of
//!    `esp_base::dispatch::after_storage_failure` with the trust record on
//!    flash adopted, so RAM never trusts less than a reboot would.
//!
//! When the trust record cannot be read at boot (either sector unreadable,
//! or the region unusable) the service fails closed:
//! the payload requests that install or pin anything
//! ([`loader_needs_trust`]) and the firmware requests that install an
//! image are `Unavailable`, rather than decided as on an unclaimed device.
//!
//! It returns a [`Handled`]: the reply (or [`Reply::Start`]: load and run
//! the payload, then answer), whether a running payload must stop, and
//! [`Note`]s for `sys/log`. The shell (`src/shell/controller.rs`) only
//! moves messages, publishes status changes and runs the payload, so every
//! decision and every flash effect here is unit tested on the host, against
//! the RAM flash model with power-loss injection, and `mwctl` runs against
//! it unchanged (`tests::v9_*`).

use core::fmt;

use embedded_storage::nor_flash::NorFlash;
use esp_base::dispatch::{Action, Context, Outcome, State, after_storage_failure, handle_request};
use esp_base::image::Header;
use esp_base::store::{PayloadStore, StoreError};
use esp_base::trust_store::{Persisted, TrustStore};
use heapless::Vec;
use middleware::loader::trust::TrustState;
use middleware::loader::{PayloadStatus, keys};
use middleware::transport::{HandlerError, Request};
use middleware::{Key, SenderId};

use crate::fw_update::{self, FwContext, FwUpdate, SelfTest};
use crate::layout::FlashMap;

/// Whether a payload request depends on the trust record: installing a
/// payload (`begin`, `chunk`, `commit`, which a claimed device accepts
/// only signed) and pinning a key (`sys/trust/claim`). Without a readable
/// trust record they are refused `Unavailable` (fail closed), instead of
/// running against an `Unclaimed` default that would accept unsigned
/// payloads. `sys/info`, `sys/trust/info`, start and stop do not install
/// anything and still work.
pub fn loader_needs_trust(key: Key<'_>) -> bool {
    key == keys::BEGIN || key == keys::CHUNK || key == keys::COMMIT || key == keys::TRUST_CLAIM
}

/// Whether both trust record sectors can be read (the record's length at
/// the start of each, what `TrustStore::open` reads).
fn trust_readable<F: NorFlash>(flash: &mut F, map: &FlashMap) -> bool {
    let mut buf = [0u8; esp_base::trust_store::RECORD_LEN];
    (0..2u32).all(|s| {
        let at = map.trust.offset.checked_add(s * esp_base::image::SECTOR);
        at.is_some_and(|at| flash.read(at, &mut buf).is_ok())
    })
}

/// What `sys/info` reports about this build, besides the store capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Facts {
    /// Chip name (`rp2040`, `rp2350`).
    pub chip: &'static str,
    /// Firmware version (`<ver>+no-wasm` without the runtime).
    pub firmware: &'static str,
    /// Whether the build runs payloads.
    pub runtime: bool,
    /// The node id ([`crate::node::node_id`]).
    pub node: SenderId,
    /// Largest image accepted (the store slot caps it too).
    pub max_image: u32,
}

/// What [`Service::open`] found on flash, for the boot log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Boot {
    /// The installed image: its header, or why there is none. `None` when
    /// the payload region could not be opened (the loader is disabled).
    pub installed: Option<Result<Header, StoreError>>,
    /// The trust record read at boot; `None` when the trust region could
    /// not be read (payload and firmware installs are then refused).
    pub trust: Option<Persisted>,
}

/// A request's reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    /// Answer now: the body length in the caller's buffer, or the error.
    Now(Result<usize, HandlerError>),
    /// `sys/payload/start` of an installed payload: load and run it, then
    /// answer (`Ok` once it runs, `Internal` if it failed to load).
    Start,
}

/// A notable event, logged on `sys/log`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Note {
    /// An image was installed and its slot made active.
    Installed {
        /// Image length.
        len: u32,
    },
    /// A storage action failed.
    Storage(StoreError),
    /// A signing key was pinned (its first four bytes).
    Pinned {
        /// Key prefix.
        prefix: [u8; 4],
    },
    /// A signed payload was accepted and `min_version` raised.
    SignedAccepted {
        /// The new `min_version`.
        min_version: u32,
    },
    /// The new image replaces the one the running payload came from.
    ReplacesRunning,
    /// A commit was refused by the trust rules.
    CommitDenied {
        /// The `min_version` a signed commit must exceed.
        min_version: u32,
    },
    /// A claim on a claimed device was refused.
    ClaimDenied,
}

impl fmt::Display for Note {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Note::Installed { len } => write!(f, "payload installed ({len} bytes)"),
            Note::Storage(e) => write!(f, "storage: {e}"),
            Note::Pinned { prefix: k } => write!(
                f,
                "trust: key {:02x}{:02x}{:02x}{:02x}... pinned, min_version 0",
                k[0], k[1], k[2], k[3]
            ),
            Note::SignedAccepted { min_version } => {
                write!(f, "trust: signed payload accepted, min_version {min_version}")
            }
            Note::ReplacesRunning => f.write_str("new image active; stopping the payload that ran the old one"),
            Note::CommitDenied { min_version } => write!(
                f,
                "trust: commit denied (needs a valid signature and version > {min_version}); previous payload kept"
            ),
            Note::ClaimDenied => f.write_str(
                "trust: claim denied; the device is claimed (no re-key window on this board: erase the trust region)",
            ),
        }
    }
}

/// Notes from one request (at most three are ever produced together).
pub type Notes = Vec<Note, 4>;

/// What handling one request asks of the shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handled {
    /// The reply.
    pub reply: Reply,
    /// A running payload must stop (before the reply is sent).
    pub stop: bool,
    /// Events to log.
    pub notes: Notes,
}

impl Handled {
    const fn now(reply: Result<usize, HandlerError>) -> Self {
        Handled { reply: Reply::Now(reply), stop: false, notes: Vec::new() }
    }
}

/// Why a start failed, for [`Service::start_failed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartFailure {
    /// The image could not be read or failed its CRC.
    Store,
    /// The heap check refused it, or it failed to compile or instantiate.
    Load,
}

/// The base firmware's request handling over flash `F`, with firmware
/// updates handled by `U`.
pub struct Service<F, U> {
    state: State,
    ctx: Context<'static>,
    store: Option<PayloadStore<F>>,
    trust: Option<TrustStore>,
    fw: U,
    usb_configured: bool,
}

impl<F: NorFlash, U: FwUpdate<F>> Service<F, U> {
    /// Opens the trust record and then the payload store at the regions of
    /// `map` (the trust record is read before the flash moves into the
    /// store, which lends it back for every trust save), verifies the
    /// installed image and builds the boot state.
    pub fn open(mut flash: F, map: &FlashMap, facts: Facts, fw: U) -> (Self, Boot) {
        // `TrustStore::open` reads a sector it cannot read as holding no
        // record (Unclaimed); here an unreadable sector makes the trust
        // record unavailable instead, so the service fails closed.
        let opened = match trust_readable(&mut flash, map) {
            true => TrustStore::open(&mut flash, map.trust.offset, map.trust.size).ok(),
            false => None,
        };
        let (trust, persisted) = match opened {
            Some((t, p)) => (Some(t), Some(p)),
            None => (None, None),
        };
        let mut store = PayloadStore::new(flash, map.payload.offset, map.payload.size, facts.max_image).ok();
        let installed = store.as_mut().map(PayloadStore::verify);
        let p = persisted.unwrap_or_default();
        let state = State::boot(matches!(installed, Some(Ok(_)))).with_trust(p.trust).with_control(p.control);
        let ctx = Context {
            chip: facts.chip,
            firmware: facts.firmware,
            capacity: store.as_ref().map_or(0, PayloadStore::capacity),
            // No esp-base chip profile: `sys/board` is `NotFound`.
            profile: None,
            runtime: facts.runtime,
            rekey_window: false,
            staging: false,
            node: Some(facts.node),
        };
        (Service { state, ctx, store, trust, fw, usb_configured: false }, Boot { installed, trust: persisted })
    }

    /// The dispatcher state.
    pub const fn state(&self) -> State {
        self.state
    }

    /// The payload status.
    pub const fn status(&self) -> PayloadStatus {
        self.state.status()
    }

    /// The largest image the store accepts (0 without a store).
    pub const fn capacity(&self) -> u32 {
        self.ctx.capacity
    }

    /// The payload store, to read the installed image for a start.
    pub fn store_mut(&mut self) -> Option<&mut PayloadStore<F>> {
        self.store.as_mut()
    }

    /// The firmware-update backend.
    pub fn fw(&self) -> &U {
        &self.fw
    }

    /// Records whether the host has configured USB (an input of the
    /// [`SelfTest`]).
    pub fn set_usb_configured(&mut self, configured: bool) {
        self.usb_configured = configured;
    }

    /// The self-test a trial firmware must pass before it is confirmed.
    pub fn self_test(&self) -> SelfTest {
        SelfTest {
            usb_configured: self.usb_configured,
            store_open: self.store.is_some(),
            trust_open: self.trust.is_some(),
        }
    }

    /// The payload was loaded and runs.
    pub fn started(&mut self) {
        self.state = self.state.started();
    }

    /// The payload trapped or ran out of fuel.
    pub fn trapped(&mut self) {
        self.state = self.state.trapped();
    }

    /// A start failed before the payload ran.
    pub fn start_failed(&mut self, why: StartFailure) {
        self.state = match why {
            StartFailure::Store => self.state.storage_failed(),
            StartFailure::Load => self.state.trapped(),
        };
    }

    /// Handles one request; a reply body is written into `out`.
    pub fn handle(&mut self, key: Key<'_>, body: &[u8], out: &mut [u8]) -> Handled {
        if fw_update::is_fw_key(key) {
            let ctx = FwContext {
                node: self.ctx.node.unwrap_or(SenderId([0; 8])),
                trust: self.trust.as_ref().map(|_| self.state.trust()),
                self_test: self.self_test(),
            };
            let flash = self.store.as_mut().map(PayloadStore::flash);
            return Handled::now(self.fw.handle(flash, &ctx, key, body, out));
        }
        if self.trust.is_none() && loader_needs_trust(key) {
            return Handled::now(Err(HandlerError::Unavailable));
        }
        let outcome = handle_request(self.state, &self.ctx, &Request::new(key, body), out);
        if outcome.action == Action::Start {
            return Handled { reply: Reply::Start, stop: false, notes: Vec::new() };
        }
        let mut notes = Notes::new();
        let mut stop = outcome.action.stops_payload();
        let reply = match self.storage(&outcome) {
            Ok(()) => {
                self.note_success(key, &outcome, &mut notes);
                self.state = outcome.state;
                outcome.reply
            }
            Err(e) => {
                let _ = notes.push(Note::Storage(e));
                let (failed, stops) = after_storage_failure(self.state, &outcome.action);
                stop = stops;
                self.state = self.adopt_persisted(failed);
                Err(HandlerError::Internal)
            }
        };
        Handled { reply: Reply::Now(reply), stop, notes }
    }

    /// Performs the outcome's storage action.
    fn storage(&mut self, outcome: &Outcome<'_>) -> Result<(), StoreError> {
        // The counter the next state carries (reset to 0 by a claim).
        let control = outcome.state.control();
        match outcome.action {
            Action::Erase { len, .. } => self.store()?.erase(len),
            Action::Write { offset, data } => self.store()?.write(offset, data),
            Action::Activate { len, crc, trust: None } => self.store()?.activate(len, crc),
            Action::Activate { len, crc, trust: Some(next) } => {
                let (Some(store), Some(ts)) = (self.store.as_mut(), self.trust.as_mut()) else {
                    return Err(StoreError::Flash);
                };
                store.activate_with(len, crc, |flash| ts.save(flash, &Persisted { trust: next, control }))
            }
            Action::Claim { trust } => {
                let (Some(store), Some(ts)) = (self.store.as_mut(), self.trust.as_mut()) else {
                    return Err(StoreError::Flash);
                };
                ts.save(store.flash(), &Persisted { trust, control })
            }
            Action::None | Action::Start | Action::Stop => Ok(()),
        }
    }

    fn store(&mut self) -> Result<&mut PayloadStore<F>, StoreError> {
        self.store.as_mut().ok_or(StoreError::NoImage)
    }

    /// `state` with the trust record on flash adopted.
    fn adopt_persisted(&self, state: State) -> State {
        match self.trust.as_ref() {
            Some(ts) => {
                let p = ts.persisted();
                state.with_trust(p.trust).with_control(p.control)
            }
            None => state,
        }
    }

    /// Notes for an outcome whose action succeeded.
    fn note_success(&self, key: Key<'_>, outcome: &Outcome<'_>, notes: &mut Notes) {
        let mut push = |n| {
            // At most three notes per outcome; the capacity is four.
            let _ = notes.push(n);
        };
        match outcome.action {
            Action::Activate { len, trust, .. } => {
                push(Note::Installed { len });
                if let Some(TrustState::Claimed { min_version, .. }) = trust {
                    push(Note::SignedAccepted { min_version: min_version.0 });
                }
                if self.state.status() == PayloadStatus::Running {
                    push(Note::ReplacesRunning);
                }
            }
            Action::Claim { trust: TrustState::Claimed { key, .. } } => {
                push(Note::Pinned { prefix: [key.0[0], key.0[1], key.0[2], key.0[3]] });
            }
            _ => {}
        }
        if outcome.reply == Err(HandlerError::Denied) {
            if key == keys::COMMIT {
                let min_version = match self.state.trust() {
                    TrustState::Claimed { min_version, .. } => min_version.0,
                    TrustState::Unclaimed => 0,
                };
                push(Note::CommitDenied { min_version });
            } else {
                push(Note::ClaimDenied);
            }
        }
    }
}

#[cfg(test)]
mod wire;

#[cfg(test)]
pub(crate) mod tests {
    extern crate std;

    use super::*;
    use crate::fw_update::{Absent, Unsupported};
    use crate::layout::{Boot as BootKind, Region};
    use crate::ram_flash::RamFlash;
    use ed25519_dalek::{Signer, SigningKey};
    use middleware::crc::Crc32;
    use middleware::loader::DeviceInfo;
    use middleware::loader::sha256::ImageHash;
    use middleware::loader::trust::{PublicKey, SIGNED_COMMIT_LEN, Signature, SignedCommit, Version, signed_message};
    use std::boxed::Box;
    use std::vec::Vec as StdVec;

    /// A small flash map with the real regions' structure: 64 KiB of
    /// payload store (two 32 KiB slots, 28 KiB images) and the trust record.
    pub const MAP: FlashMap = FlashMap {
        flash_size: 0x1_4000,
        boot: BootKind::ImageDef,
        partition_table: None,
        firmware: Region::new(0, 0x2000),
        firmware_b: None,
        fw_record: None,
        payload: Region::new(0x2000, 0x1_0000),
        trust: Region::new(0x1_2000, 0x2000),
    };

    pub type Flash = RamFlash<0x1_4000>;

    pub const NODE: SenderId = SenderId([0xE6, 0x61, 0x38, 0x52, 0x83, 0x1F, 0x2A, 0x0B]);

    pub const FACTS: Facts = Facts {
        chip: "rp2350",
        firmware: "0.1.0",
        runtime: true,
        node: NODE,
        max_image: 64 * 1024,
    };

    type Svc<'f, U> = Service<&'f mut Flash, U>;

    pub fn flash() -> Box<Flash> {
        Box::new(Flash::new())
    }

    fn open<'f, U: FwUpdate<&'f mut Flash>>(flash: &'f mut Flash, facts: Facts, fw: U) -> (Svc<'f, U>, Boot) {
        Service::open(flash, &MAP, facts, fw)
    }

    /// Sends one request; returns the reply with its body copied out.
    fn call<'f, U: FwUpdate<&'f mut Flash>>(s: &mut Svc<'f, U>, key: Key<'_>, body: &[u8]) -> (Result<StdVec<u8>, HandlerError>, Handled) {
        let mut out = [0u8; 128];
        let h = s.handle(key, body, &mut out);
        let r = match h.reply {
            Reply::Now(r) => r.map(|n| out[..n].to_vec()),
            Reply::Start => Ok(StdVec::new()),
        };
        (r, h)
    }

    /// Sends one request that must succeed.
    fn ok<'f, U: FwUpdate<&'f mut Flash>>(s: &mut Svc<'f, U>, key: Key<'_>, body: &[u8]) {
        let (r, _) = call(s, key, body);
        assert!(r.is_ok(), "{}: {r:?}", key.as_str());
    }

    fn begin(len: usize, crc: u32) -> [u8; 8] {
        let mut b = [0u8; 8];
        b[..4].copy_from_slice(&(len as u32).to_le_bytes());
        b[4..].copy_from_slice(&crc.to_le_bytes());
        b
    }

    /// Begin and chunks of `image` (48-byte chunks); commit not sent.
    fn transfer<'f, U: FwUpdate<&'f mut Flash>>(s: &mut Svc<'f, U>, image: &[u8]) {
        assert_eq!(call(s, keys::BEGIN, &begin(image.len(), Crc32::checksum(image))).0, Ok(StdVec::new()));
        for (i, part) in image.chunks(48).enumerate() {
            let mut c = (i as u32 * 48).to_le_bytes().to_vec();
            c.extend_from_slice(part);
            assert_eq!(call(s, keys::CHUNK, &c).0, Ok(StdVec::new()));
        }
    }

    pub fn image(n: usize, seed: u8) -> StdVec<u8> {
        (0..n).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
    }

    fn installed<'f, U: FwUpdate<&'f mut Flash>>(s: &mut Svc<'f, U>) -> Option<StdVec<u8>> {
        let store = s.store_mut()?;
        let h = store.header()?;
        let mut buf = std::vec![0u8; h.len as usize];
        store.load(h, &mut buf).ok()?;
        Some(buf)
    }

    #[test]
    fn v1_info_reports_the_rp_chip_capacity_and_node_id() {
        let mut f = flash();
        let (mut s, boot) = open(&mut f, FACTS, Unsupported);
        assert_eq!(boot.installed, Some(Err(StoreError::NoImage)));
        assert_eq!(boot.trust, Some(Persisted::default()));
        let (r, _) = call(&mut s, keys::INFO, &[]);
        let body = r.unwrap();
        let info = DeviceInfo::decode(&body).unwrap();
        assert_eq!((info.chip, info.firmware), ("rp2350", "0.1.0"));
        assert_eq!(info.capacity, 28 * 1024, "the slot caps max_image");
        assert_eq!(info.status, PayloadStatus::Empty);
        assert_eq!(info.node, Some(NODE));
        let (_, h) = call(&mut s, keys::INFO, &[]);
        assert!(!h.stop && h.notes.is_empty());
    }

    #[test]
    fn v3_an_upload_survives_a_reboot_and_runs_on_start() {
        let img = image(3000, 7);
        let mut f = flash();
        {
            let (mut s, _) = open(&mut f, FACTS, Unsupported);
            transfer(&mut s, &img);
            let (r, h) = call(&mut s, keys::COMMIT, &[]);
            assert_eq!(r, Ok(StdVec::new()));
            assert_eq!(h.notes.as_slice(), &[Note::Installed { len: 3000 }]);
            assert_eq!(s.status(), PayloadStatus::Stopped);
        }
        let (mut s, boot) = open(&mut f, FACTS, Unsupported);
        assert!(matches!(boot.installed, Some(Ok(h)) if h.len == 3000));
        assert_eq!(s.status(), PayloadStatus::Stopped);
        assert_eq!(installed(&mut s).as_deref(), Some(&img[..]));
        let (_, h) = call(&mut s, keys::START, &[]);
        assert_eq!(h.reply, Reply::Start);
        s.started();
        assert_eq!(s.status(), PayloadStatus::Running);
        // A serial begin stops the running payload; the old image stays.
        let (_, h) = call(&mut s, keys::BEGIN, &begin(10, 0));
        assert!(h.stop);
        assert_eq!(s.status(), PayloadStatus::Loading);
        assert_eq!(installed(&mut s).as_deref(), Some(&img[..]));
    }

    #[test]
    fn v3_a_cut_or_spoiled_transfer_keeps_the_previous_image() {
        let old = image(2000, 1);
        let new = image(2500, 2);
        let mut f = flash();
        {
            let (mut s, _) = open(&mut f, FACTS, Unsupported);
            transfer(&mut s, &old);
            ok(&mut s, keys::COMMIT, &[]);
            // Wrong CRC: the commit fails, the old image stays active.
            ok(&mut s, keys::BEGIN, &begin(new.len(), 0xDEAD));
            for (i, part) in new.chunks(48).enumerate() {
                let mut c = (i as u32 * 48).to_le_bytes().to_vec();
                c.extend_from_slice(part);
                ok(&mut s, keys::CHUNK, &c);
            }
            let (r, _) = call(&mut s, keys::COMMIT, &[]);
            assert_eq!(r, Err(HandlerError::InvalidRequest));
            assert_eq!(s.status(), PayloadStatus::Stopped);
            assert_eq!(installed(&mut s).as_deref(), Some(&old[..]));
        }
        // Power fails during the new transfer's writes, then on every
        // later write; the transfer reports Internal.
        {
            let (mut s, _) = open(&mut f, FACTS, Unsupported);
            ok(&mut s, keys::BEGIN, &begin(new.len(), Crc32::checksum(&new)));
            s.store_mut().unwrap().flash().cut_at(3);
            let mut failed = false;
            for (i, part) in new.chunks(48).enumerate() {
                let mut c = (i as u32 * 48).to_le_bytes().to_vec();
                c.extend_from_slice(part);
                let (r, h) = call(&mut s, keys::CHUNK, &c);
                if r == Err(HandlerError::Internal) {
                    assert!(matches!(h.notes.as_slice(), [Note::Storage(StoreError::Flash)]));
                    failed = true;
                    break;
                }
            }
            assert!(failed);
            assert_eq!(s.status(), PayloadStatus::Stopped);
            s.store_mut().unwrap().flash().restore();
        }
        let (mut s, boot) = open(&mut f, FACTS, Unsupported);
        assert!(matches!(boot.installed, Some(Ok(h)) if h.len == 2000));
        assert_eq!(installed(&mut s).as_deref(), Some(&old[..]));
    }

    #[test]
    fn v3_an_image_larger_than_a_slot_is_refused() {
        let mut f = flash();
        let (mut s, _) = open(&mut f, FACTS, Unsupported);
        let (r, _) = call(&mut s, keys::BEGIN, &begin(28 * 1024 + 1, 0));
        assert_eq!(r, Err(HandlerError::BufferTooSmall));
        assert_eq!(s.status(), PayloadStatus::Empty);
    }

    fn signer(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn public(seed: u8) -> PublicKey {
        PublicKey(signer(seed).verifying_key().to_bytes())
    }

    fn signed(seed: u8, image: &[u8], version: u32) -> [u8; SIGNED_COMMIT_LEN] {
        let m = signed_message(image.len() as u32, Version(version), &ImageHash::digest(image));
        let signature = Signature(signer(seed).sign(&m).to_bytes());
        SignedCommit { version: Version(version), signature }.encode()
    }

    #[test]
    fn v4_a_pinned_key_and_its_min_version_survive_reboots() {
        let img = image(1500, 3);
        let mut f = flash();
        {
            let (mut s, _) = open(&mut f, FACTS, Unsupported);
            // Trust on first use: unsigned accepted.
            transfer(&mut s, &img);
            assert_eq!(call(&mut s, keys::COMMIT, &[]).0, Ok(StdVec::new()));
            let (r, h) = call(&mut s, keys::TRUST_CLAIM, &public(1).0);
            assert_eq!(r, Ok(StdVec::new()));
            assert!(matches!(h.notes.as_slice(), [Note::Pinned { .. }]));
        }
        {
            let (mut s, boot) = open(&mut f, FACTS, Unsupported);
            let pinned = TrustState::Claimed { key: public(1), min_version: Version(0) };
            assert_eq!(boot.trust.map(|p| p.trust), Some(pinned));
            // Unsigned, foreign and rollback commits are denied.
            for body in [StdVec::new(), signed(2, &img, 5).to_vec()] {
                transfer(&mut s, &img);
                let (r, h) = call(&mut s, keys::COMMIT, &body);
                assert_eq!(r, Err(HandlerError::Denied));
                assert_eq!(h.notes.as_slice(), &[Note::CommitDenied { min_version: 0 }]);
            }
            transfer(&mut s, &img);
            let (r, h) = call(&mut s, keys::COMMIT, &signed(1, &img, 5));
            assert_eq!(r, Ok(StdVec::new()));
            assert_eq!(
                h.notes.as_slice(),
                &[Note::Installed { len: 1500 }, Note::SignedAccepted { min_version: 5 }]
            );
            // A second claim is refused: no re-key window on this board.
            let (r, h) = call(&mut s, keys::TRUST_CLAIM, &public(2).0);
            assert_eq!(r, Err(HandlerError::Denied));
            assert_eq!(h.notes.as_slice(), &[Note::ClaimDenied]);
        }
        let (mut s, boot) = open(&mut f, FACTS, Unsupported);
        let raised = TrustState::Claimed { key: public(1), min_version: Version(5) };
        assert_eq!(boot.trust.map(|p| p.trust), Some(raised));
        transfer(&mut s, &img);
        assert_eq!(call(&mut s, keys::COMMIT, &signed(1, &img, 5)).0, Err(HandlerError::Denied), "replay");
    }

    #[test]
    fn v4_min_version_reaches_flash_before_the_new_header() {
        let old = image(1000, 4);
        let new = image(1200, 5);
        let mut f = flash();
        {
            let (mut s, _) = open(&mut f, FACTS, Unsupported);
            transfer(&mut s, &old);
            ok(&mut s, keys::COMMIT, &[]);
            ok(&mut s, keys::TRUST_CLAIM, &public(1).0);
            transfer(&mut s, &new);
            // The image needs no final word flush (1200 = 300 words); the
            // trust save is ops 0-2 (erase, fields, magic) and the slot
            // header's first write, op 3, is cut.
            s.store_mut().unwrap().flash().cut_at(3);
            let (r, h) = call(&mut s, keys::COMMIT, &signed(1, &new, 9));
            assert_eq!(r, Err(HandlerError::Internal));
            assert!(matches!(h.notes.as_slice(), [Note::Storage(_)]));
            // RAM adopts what flash holds: the raised min_version.
            let raised = TrustState::Claimed { key: public(1), min_version: Version(9) };
            assert_eq!(s.state().trust(), raised);
            s.store_mut().unwrap().flash().restore();
        }
        let (mut s, boot) = open(&mut f, FACTS, Unsupported);
        assert_eq!(installed(&mut s).as_deref(), Some(&old[..]), "the old image stays active");
        let raised = TrustState::Claimed { key: public(1), min_version: Version(9) };
        assert_eq!(boot.trust.map(|p| p.trust), Some(raised));
    }

    #[test]
    fn v6_sys_fw_keys_go_to_the_update_backend_only() {
        let img = image(800, 6);
        let mut f = flash();
        let (mut s, _) = open(&mut f, FACTS, Unsupported);
        transfer(&mut s, &img);
        let before = s.state();
        for k in crate::fw_update::keys::ALL {
            let (r, h) = call(&mut s, k, b"anything");
            assert_eq!(r, Err(HandlerError::Unavailable), "{}", k.as_str());
            assert!(!h.stop && h.notes.is_empty());
        }
        assert_eq!(s.state(), before, "the payload transfer is untouched");
        assert_eq!(call(&mut s, keys::COMMIT, &[]).0, Ok(StdVec::new()));
        let mut g = flash();
        let (mut s, _) = open(&mut g, FACTS, Absent);
        assert_eq!(call(&mut s, crate::fw_update::keys::INFO, &[]).0, Err(HandlerError::NotFound));
    }

    #[test]
    fn v13_an_unreadable_trust_record_refuses_payload_installs_and_claims() {
        // The trust region lies outside the flash: it cannot be read.
        let map = FlashMap { trust: Region::new(MAP.flash_size, 0x2000), ..MAP };
        let img = image(600, 14);
        let mut f = flash();
        let (mut s, boot) = Service::open(&mut *f, &map, FACTS, Unsupported);
        assert_eq!(boot.trust, None);
        assert!(boot.installed.is_some(), "the store opened");
        let mut chunk = 0u32.to_le_bytes().to_vec();
        chunk.extend_from_slice(&img[..48]);
        let claim = public(1).0;
        for (k, body) in [
            (keys::BEGIN, &begin(img.len(), Crc32::checksum(&img))[..]),
            (keys::CHUNK, &chunk[..]),
            (keys::COMMIT, &[][..]),
            (keys::COMMIT, &signed(1, &img, 5)[..]),
            (keys::TRUST_CLAIM, &claim[..]),
        ] {
            let (r, h) = call(&mut s, k, body);
            assert_eq!(r, Err(HandlerError::Unavailable), "{}", k.as_str());
            assert!(!h.stop && h.notes.is_empty());
        }
        assert_eq!(s.status(), PayloadStatus::Empty);
        // Reads still work.
        assert!(call(&mut s, keys::INFO, &[]).0.is_ok());
        assert!(call(&mut s, keys::TRUST_INFO, &[]).0.is_ok());
        assert!(!s.self_test().ok());
        // With the record readable the same requests go through.
        let mut g = flash();
        let (mut s, _) = open(&mut g, FACTS, Unsupported);
        transfer(&mut s, &img);
        assert_eq!(call(&mut s, keys::COMMIT, &[]).0, Ok(StdVec::new()));
    }

    #[test]
    fn v11_without_the_runtime_payloads_are_stored_but_not_started() {
        let facts = Facts { chip: "rp2040", firmware: "0.1.0+no-wasm", runtime: false, ..FACTS };
        let img = image(900, 8);
        let mut f = flash();
        let (mut s, _) = open(&mut f, facts, Absent);
        transfer(&mut s, &img);
        assert_eq!(call(&mut s, keys::COMMIT, &[]).0, Ok(StdVec::new()));
        let (r, h) = call(&mut s, keys::START, &[]);
        assert_eq!((r, h.reply), (Err(HandlerError::NotFound), Reply::Now(Err(HandlerError::NotFound))));
        assert_eq!(s.status(), PayloadStatus::Stopped);
        let body = call(&mut s, keys::INFO, &[]).0.unwrap();
        assert_eq!(DeviceInfo::decode(&body).unwrap().firmware, "0.1.0+no-wasm");
    }

    #[test]
    fn start_failures_and_traps_update_the_status() {
        let img = image(700, 9);
        let mut f = flash();
        let (mut s, _) = open(&mut f, FACTS, Unsupported);
        transfer(&mut s, &img);
        ok(&mut s, keys::COMMIT, &[]);
        s.start_failed(StartFailure::Load);
        assert_eq!(s.status(), PayloadStatus::Trapped);
        s.started();
        s.trapped();
        assert_eq!(s.status(), PayloadStatus::Trapped);
        let (_, h) = call(&mut s, keys::STOP, &[]);
        assert!(h.stop);
        assert_eq!(s.status(), PayloadStatus::Stopped);
        s.start_failed(StartFailure::Store);
        assert_eq!(s.status(), PayloadStatus::Stopped);
        assert_eq!(s.capacity(), 28 * 1024);
    }

    #[test]
    fn notes_read_as_log_lines() {
        use std::string::ToString;
        assert_eq!(Note::Installed { len: 12 }.to_string(), "payload installed (12 bytes)");
        assert_eq!(Note::Pinned { prefix: [0xAB, 1, 2, 3] }.to_string(), "trust: key ab010203... pinned, min_version 0");
        assert!(Note::CommitDenied { min_version: 4 }.to_string().contains("version > 4"));
        assert!(Note::Storage(StoreError::Crc).to_string().starts_with("storage: "));
    }
}
