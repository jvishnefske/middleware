//! The request service of the RT1050 base firmware (RT12): the base
//! firmware's request handling and storage effects over the HyperFlash,
//! the counterpart of firmware-rp's `rp_base::service`.
//!
//! The shell-facing API is two calls:
//!
//! * [`Service::open`] at boot: takes the flash (any
//!   `embedded_storage` [`NorFlash`] with 256 KiB erase blocks, the
//!   HyperFlash driver on the board), the flash map
//!   ([`crate::layout::RT1050_EVKB`]), the build's [`Facts`] and the
//!   bootloader's [`Handoff`], and returns the service and the [`Boot`]
//!   facts for the boot log (the installed payload, the trust record, the
//!   A/B boot), or [`OpenError`] for a map that fails
//!   [`FlashMap::check`].
//! * [`Service::handle`] per request: key, body and the reply buffer in,
//!   a [`Handled`] out: the reply (or [`Reply::Start`]: load and run the
//!   payload, then answer), whether a running payload must stop, [`Note`]s
//!   for `sys/log`, a reset to perform once the reply has left, and
//!   whether the trial watchdog may be released.
//!
//! The rest of the API mirrors firmware-rp's for the payload runner
//! ([`Service::started`], [`Service::trapped`], [`Service::start_failed`],
//! [`Service::run_ended`], [`Service::route_idle`],
//! [`Service::store_mut`]) and the self-test ([`Service::set_link_ready`]),
//! and adds the Zenoh client's two needs: [`Service::zenoh_endpoint`], the
//! configured endpoint, and [`Service::accept_control`], which persists the
//! counter of a signed control request the Zenoh gate admitted
//! (`crate::zenoh_mgmt::Admission::Control`) before it is dispatched.
//!
//! # Dispatch
//!
//! 0. `sys/zenoh/endpoint` ([`crate::config::ZENOH_ENDPOINT`]) is the
//!    board configuration's ([`crate::config`]): an empty body reads the
//!    endpoint (its string, `tcp/<ipv4>:<port>`; `NotFound` when none is
//!    configured); a body sets it: `InvalidRequest` unless it is a valid
//!    endpoint ([`crate::config::Endpoint::parse`]: the body is malformed,
//!    the transport's code for that, as for any other undecodable request
//!    body), else it is persisted and the reply is `Ok` and empty (the
//!    same endpoint again writes nothing), `Internal` if the flash failed
//!    (the previous endpoint stays, on flash and here). When the
//!    configuration region could not be read at boot both are
//!    `Unavailable`, so an unreadable record is never overwritten blind.
//!    It needs no trust: the serial links carry the authority of someone
//!    holding the board, as for `sys/trust/claim`; over Zenoh it is refused
//!    before the service sees it (`crate::zenoh_mgmt`). The shell reads the
//!    endpoint with [`Service::zenoh_endpoint`] (a set is also a
//!    [`Note::ZenohEndpoint`]).
//! 1. `sys/fw/*` goes to the A/B updater only ([`fw::Updater`]:
//!    `middleware::fw_ota`'s `FwService::<RtImage>` over
//!    [`crate::ab::RtPlatform`] on the raw flash), never to the payload
//!    dispatcher, so it never touches the payload loader, store or status.
//! 2. Everything else (`sys/info`, `sys/loader/*`, `sys/payload/*`,
//!    `sys/trust/*`, unknown keys) is decided by
//!    `esp_base::dispatch::handle_request`, the ESP base firmware's own pure
//!    dispatcher, exactly as on the RP base firmware: no re-key window
//!    (no presence button is wired), staging only for requests over Zenoh
//!    (see *Origins and transfers* below), no esp-base chip profile
//!    (`sys/board` is `NotFound`). `sys/info` reports [`Facts::chip`],
//!    [`Facts::firmware`], the payload capacity, the status, the node id
//!    and, with the runtime ([`Facts::runtime`]), [`RUNTIME_CAPS`].
//! 3. The storage action is performed as the RP and ESP controllers do:
//!    erase and write the inactive payload slot, activate it (a raised
//!    `min_version` saved inside `PayloadStore::activate_with`, after the
//!    CRC check and before the slot header), save a claimed key. A failed
//!    action replies `Internal` and leaves the state of
//!    `esp_base::dispatch::after_storage_failure` with the trust record on
//!    flash adopted, so RAM never trusts less than a reboot would.
//!
//! # Origins and transfers
//!
//! The shell names the requester of each request with
//! [`Service::set_origin`]: [`Origin::Serial`] for the USB CDC and LPUART
//! links (one requester: whoever holds the board), [`Origin::Air`] with
//! the board's Ethernet MAC for the Zenoh link (every Zenoh peer is one
//! requester: queries name no authenticated sender). The Zenoh gate
//! (`crate::zenoh_mgmt`) has admitted a Zenoh request before it gets here.
//!
//! * Over Zenoh a payload upload **stages** beside the running payload
//!   (`Context::staging`, as over ESP-NOW): `begin` and `chunk` write the
//!   inactive slot only and the payload keeps running until a commit
//!   signed by the pinned key activates the new image.
//! * One payload transfer and one firmware transfer exist at a time. The
//!   requester whose `begin` started a transfer owns it
//!   (`esp_base::ota::owner_after` for the loader; the same rule for
//!   `sys/fw/*`); a `begin` from anyone takes it over, and a `chunk` or
//!   `commit` from another requester is `InvalidRequest` without touching
//!   the transfer, so a Zenoh peer cannot spoil a serial upload with
//!   chunks, nor the other way round. [`Service::abandon_transfer`] ends
//!   the payload transfer when the gate refused its owner's unsigned
//!   commit (`esp_base::ota::Refusal::abandons_transfer`).
//!
//! When the trust record cannot be read at boot (either sector
//! unreadable) the service fails closed, as RP V13: the payload requests
//! that install or pin anything ([`loader_needs_trust`]) and the firmware
//! requests that install an image ([`fw::needs_trust`]) are
//! `Unavailable`, rather than decided as on an unclaimed device.
//!
//! # Flash
//!
//! The service owns the flash through [`flash::RtFlash`], which the payload
//! store owns and lends:
//!
//! * the trust record: `esp_base::trust_store` at the trust region's
//!   offset, its two 4 KiB sectors one per 256 KiB block
//!   ([`crate::layout::SectorPerBlock`]);
//! * the payload store: `esp_base::store` on the payload region, one block
//!   per payload slot ([`crate::layout::BlockErase`]), images capped at
//!   [`crate::layout::PAYLOAD_CAPACITY`] and [`Facts::max_image`];
//! * the A/B updater: the raw flash ([`flash::RtFlash::raw`]) for the
//!   update record and the inactive firmware slot;
//! * the board configuration: the raw flash at the configuration region
//!   ([`crate::config::load`], [`crate::config::save`]).
//!
//! Every erase is whole blocks of its own region.
//!
//! # Shared with firmware-rp, and what is not
//!
//! The decisions are shared: `esp_base::dispatch`, `store`, `trust_store`,
//! `serve` and `image` (as firmware-rp uses them), and
//! `middleware::fw_ota`. The thin glue that performs a dispatch outcome on
//! the stores (this module's [`Service::handle`] and [`Note`]) follows
//! firmware-rp's `service` closely but is not taken from `rp_base`: that
//! service is built over one flash shared by both stores and an RP flash
//! map, its firmware-update seam returns only a reply (the RP2350 resets
//! through its boot ROM seam) and its self-test is USB's, while here the
//! two stores need two different block views of one flash, the updater the
//! raw flash, and the reset comes back to the shell as a value.
//!
//! Requests on keys a payload serves (ABI 5) are routed before this by the
//! shell with `esp_base::serve`: the service keeps the keys of the payload
//! run that ended ([`Service::run_ended`]) so they answer `Unavailable`
//! while it does not run ([`Service::route_idle`]), and clears them when the
//! image changes.

pub mod flash;
pub mod fw;

use core::fmt;

use embedded_storage::nor_flash::NorFlash;
use esp_base::dispatch::{Action, Context, Outcome, State, after_storage_failure, handle_request};
use esp_base::image::Header;
pub use esp_base::ota::Origin;
use esp_base::serve::{Route, ServedKeys, route_request, served_after};
use esp_base::store::{PayloadStore, StoreError};
use esp_base::trust_store::{PARTITION_LEN, Persisted, RECORD_LEN, TrustStore};
use heapless::Vec;
use middleware::fw_ota::BootInfo;
use middleware::loader::trust::TrustState;
use middleware::loader::{Caps, Loader, PayloadStatus, keys};
use middleware::transport::{HandlerError, Request};
use middleware::{Key, SenderId};

use middleware::loader::trust::ControlCounter;

use crate::ab::Handoff;
use crate::ab::io::IoError;
use crate::ab::platform::ResetRequest;
use crate::config::{self, Config, Endpoint, ZENOH_ENDPOINT};
use crate::layout::{FlashMap, LayoutError, PAYLOAD_CAPACITY, Region, SECTOR};
use flash::RtFlash;
use fw::{FwBootError, FwContext, SelfTest, Updater};

/// Whether a payload request depends on the trust record: installing a
/// payload (`begin`, `chunk`, `commit`, which a claimed device accepts
/// only signed) and pinning a key (`sys/trust/claim`). Without a readable
/// trust record they are refused `Unavailable` (fail closed).
pub fn loader_needs_trust(key: Key<'_>) -> bool {
    key == keys::BEGIN || key == keys::CHUNK || key == keys::COMMIT || key == keys::TRUST_CLAIM
}

/// Whether both trust record sectors can be read (the record at the start
/// of each, what `TrustStore::open` reads).
fn trust_readable<F: NorFlash>(flash: &mut F, base: u32) -> bool {
    let mut buf = [0u8; RECORD_LEN];
    (0..2u32).all(|s| flash.read(base + s * SECTOR, &mut buf).is_ok())
}

/// The `sys/info` capabilities of a build with the runtime: requests on
/// payload keys (`SERVE`), the FXOS8700CQ source route (`SOURCE_IMU`,
/// SC7) and the microphone route (`SOURCE_MICROPHONE`, SC8).
pub const RUNTIME_CAPS: Caps = Caps::SERVE
    .with(Caps::SOURCE_IMU)
    .with(Caps::SOURCE_MICROPHONE);

/// What `sys/info` reports about this build, besides the store capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Facts {
    /// Chip name (`rt1050`, `crate::chip::RT1050.name`).
    pub chip: &'static str,
    /// Firmware version.
    pub firmware: &'static str,
    /// Whether the build runs payloads. With the runtime, requests on keys
    /// the payload serves are routed to it, and `sys/info` says so
    /// (`Caps::SERVE`); without it `sys/payload/start` is `NotFound`.
    pub runtime: bool,
    /// The node id (from the chip's unique id, read by the shell).
    pub node: SenderId,
    /// Largest image accepted (`crate::chip::RT1050.max_image`); the store
    /// caps it at [`PAYLOAD_CAPACITY`] too.
    pub max_image: u32,
}

/// What [`Service::open`] found on flash, for the boot log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Boot<E> {
    /// The installed payload image: its header, or why there is none.
    pub installed: Result<Header, StoreError>,
    /// The trust record read at boot; `None` when the trust region could
    /// not be read (payload and firmware installs are then refused).
    pub trust: Option<Persisted>,
    /// This boot as the A/B updater saw it, or why it did not start
    /// (`sys/fw/*` is then `Unavailable`).
    pub fw: Result<BootInfo, FwBootError<E>>,
    /// The Zenoh endpoint from the configuration record (`None`: not
    /// configured), or why the record could not be read
    /// (`sys/zenoh/endpoint` is then `Unavailable`).
    pub zenoh: Result<Option<Endpoint>, IoError<E>>,
}

/// Why [`Service::open`] refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenError {
    /// The flash map fails [`FlashMap::check`].
    Layout(LayoutError),
    /// The payload store refused its region.
    Store(StoreError),
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
    /// The firmware will reset once the reply has left.
    Reset(ResetRequest),
    /// The trial firmware was confirmed.
    Confirmed,
    /// A new Zenoh endpoint was saved (`sys/zenoh/endpoint`).
    ZenohEndpoint(Endpoint),
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
            Note::Reset(ResetRequest::Update(slot)) => {
                write!(f, "firmware: resetting into slot {} on trial", slot.name())
            }
            Note::Reset(ResetRequest::Normal) => f.write_str("firmware: resetting"),
            Note::Confirmed => f.write_str("firmware: trial confirmed"),
            Note::ZenohEndpoint(e) => write!(f, "zenoh: endpoint {e} saved"),
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
    /// Reset once the reply has left (flush the link first): into the
    /// bootloader, which boots the pending slot on trial
    /// ([`ResetRequest::Update`]) or as its record says
    /// ([`ResetRequest::Normal`]).
    pub reset: Option<ResetRequest>,
    /// The trial firmware was confirmed by this request: stop the trial
    /// watchdog.
    pub release_watchdog: bool,
}

impl Handled {
    const fn now(reply: Result<usize, HandlerError>) -> Self {
        Handled {
            reply: Reply::Now(reply),
            stop: false,
            notes: Vec::new(),
            reset: None,
            release_watchdog: false,
        }
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

/// The base firmware's request handling over the HyperFlash `F`.
pub struct Service<F> {
    state: State,
    ctx: Context<'static>,
    store: PayloadStore<RtFlash<F>>,
    trust: Option<TrustStore>,
    fw: Updater,
    link_ready: bool,
    /// The served keys of the payload run that ended.
    served: ServedKeys,
    /// The configuration region.
    config_region: Region,
    /// The configuration record on flash; `None` when it could not be read
    /// at boot.
    config: Option<config::Stored>,
    /// The requester of the request being handled ([`Service::set_origin`]).
    origin: Origin,
    /// The requester that owns the payload transfer in progress.
    owner: Option<Origin>,
    /// The requester that owns the firmware transfer in progress.
    fw_owner: Option<Origin>,
}

impl<F: NorFlash> Service<F> {
    /// Opens the services on `flash` at the regions of `map`: the trust
    /// record (read before anything else, so an unreadable one fails
    /// closed), the A/B updater from the bootloader's `handoff` (`None`
    /// when no bootloader ran), and the payload store, whose installed
    /// image is verified. See the module docs.
    pub fn open(
        flash: F,
        map: &FlashMap,
        facts: Facts,
        handoff: Option<Handoff>,
    ) -> Result<(Self, Boot<F::Error>), OpenError> {
        map.check().map_err(OpenError::Layout)?;
        let mut flash = RtFlash::new(flash, map);
        // `TrustStore::open` reads a sector it cannot read as holding no
        // record (Unclaimed); here an unreadable sector makes the trust
        // record unavailable instead, so the service fails closed.
        let base = map.trust.offset;
        let opened = match trust_readable(&mut flash, base) {
            true => TrustStore::open(&mut flash, base, PARTITION_LEN).ok(),
            false => None,
        };
        let (trust, persisted) = match opened {
            Some((t, p)) => (Some(t), Some(p)),
            None => (None, None),
        };
        let (fw, fw_boot) = Updater::boot(flash.raw(), map.ab(), handoff);
        let (config, zenoh) = match config::load(flash.raw(), map.config) {
            Ok(c) => (Some(c), Ok(c.endpoint())),
            Err(e) => (None, Err(e)),
        };
        let limit = facts.max_image.min(PAYLOAD_CAPACITY);
        let mut store = PayloadStore::new(flash, map.payload.offset, map.payload.size, limit)
            .map_err(OpenError::Store)?;
        let installed = store.verify();
        let p = persisted.unwrap_or_default();
        let state = State::boot(installed.is_ok())
            .with_trust(p.trust)
            .with_control(p.control);
        let ctx = Context {
            chip: facts.chip,
            firmware: facts.firmware,
            capacity: store.capacity(),
            // No esp-base chip profile: `sys/board` is `NotFound`.
            profile: None,
            runtime: facts.runtime,
            rekey_window: false,
            staging: false,
            node: Some(facts.node),
            caps: if facts.runtime {
                RUNTIME_CAPS
            } else {
                Caps::NONE
            },
        };
        let svc = Service {
            state,
            ctx,
            store,
            trust,
            fw,
            link_ready: false,
            served: ServedKeys::new(),
            config_region: map.config,
            config,
            origin: Origin::Serial,
            owner: None,
            fw_owner: None,
        };
        Ok((
            svc,
            Boot {
                installed,
                trust: persisted,
                fw: fw_boot,
                zenoh,
            },
        ))
    }

    /// The dispatcher state.
    pub const fn state(&self) -> State {
        self.state
    }

    /// The payload status.
    pub const fn status(&self) -> PayloadStatus {
        self.state.status()
    }

    /// The node id `sys/info` reports (and the payload's `node_id`).
    pub fn node(&self) -> SenderId {
        self.ctx.node.unwrap_or(SenderId([0; 8]))
    }

    /// The served keys of the payload run that ended (empty after an
    /// upload, and before the first run).
    pub fn served(&self) -> &ServedKeys {
        &self.served
    }

    /// A payload run ended (stopped, trapped, replaced) having served
    /// `keys`; their requests answer `Unavailable` until the next run or
    /// an upload.
    pub fn run_ended(&mut self, keys: ServedKeys) {
        self.served = keys;
    }

    /// Where a request goes while no payload runs: `Unavailable` on the
    /// last run's served keys, the service otherwise.
    pub fn route_idle(&self, key: Key<'_>) -> Route {
        route_request(key, self.status(), self.served.iter())
    }

    /// The largest image the store accepts.
    pub const fn capacity(&self) -> u32 {
        self.ctx.capacity
    }

    /// The payload store, to read the installed image for a start.
    pub fn store_mut(&mut self) -> &mut PayloadStore<RtFlash<F>> {
        &mut self.store
    }

    /// The A/B updater.
    pub fn fw(&self) -> &Updater {
        &self.fw
    }

    /// The configured Zenoh endpoint (`sys/zenoh/endpoint`), the one the
    /// Zenoh client connects to; `None` when none is configured or the
    /// configuration record could not be read.
    pub fn zenoh_endpoint(&self) -> Option<Endpoint> {
        self.config.and_then(|c| c.endpoint())
    }

    /// Persists `counter`, the counter of a signed control request the
    /// Zenoh gate admitted (`crate::zenoh_mgmt::Admission::Control`), with
    /// the trust record (the trust state unchanged), then adopts it, so a
    /// replay of the same request is refused from now on, across reboots.
    /// Call it before dispatching the request; on an error do not dispatch
    /// it (reply `Internal`).
    ///
    /// `Rollback` for a counter not above the last accepted one (nothing
    /// is written); `Flash` when the trust record is unavailable or its
    /// save failed (the state then keeps the record on flash).
    pub fn accept_control(&mut self, counter: ControlCounter) -> Result<(), StoreError> {
        if counter <= self.state.control() {
            return Err(StoreError::Rollback);
        }
        let ts = self.trust.as_mut().ok_or(StoreError::Flash)?;
        let next = Persisted {
            trust: self.state.trust(),
            control: counter,
        };
        match ts.save(self.store.flash(), &next) {
            Ok(()) => {
                self.state = self.state.with_control(counter);
                Ok(())
            }
            Err(e) => {
                self.state = self.adopt_persisted(self.state);
                Err(e)
            }
        }
    }

    /// Names the requester of the requests that follow (see the module
    /// docs): the shell sets it before every request.
    pub fn set_origin(&mut self, origin: Origin) {
        self.origin = origin;
    }

    /// The requester of the request being handled.
    pub const fn origin(&self) -> Origin {
        self.origin
    }

    /// The requester that owns the payload transfer in progress (`None`
    /// when the loader is idle).
    pub const fn loader_owner(&self) -> Option<Origin> {
        self.owner
    }

    /// The requester that owns the firmware transfer in progress.
    pub const fn fw_owner(&self) -> Option<Origin> {
        self.fw_owner
    }

    /// Ends the payload transfer in progress, as a refused commit does: the
    /// Zenoh gate refused its owner's unsigned commit. The running payload
    /// keeps running (a Zenoh transfer only ever wrote the inactive slot).
    pub fn abandon_transfer(&mut self) {
        self.state = self.state.storage_failed();
        self.owner = None;
    }

    /// Whether `key` continues a transfer another requester owns.
    fn foreign(
        &self,
        owner: Option<Origin>,
        key: Key<'_>,
        chunk: Key<'_>,
        commit: Key<'_>,
    ) -> bool {
        owner.is_some_and(|o| o != self.origin) && (key == chunk || key == commit)
    }

    /// Records whether the protocol link is up (an input of the
    /// [`SelfTest`]).
    pub fn set_link_ready(&mut self, ready: bool) {
        self.link_ready = ready;
    }

    /// The self-test a trial firmware must pass before it is confirmed.
    pub fn self_test(&self) -> SelfTest {
        SelfTest {
            link_ready: self.link_ready,
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
        if key == ZENOH_ENDPOINT {
            return self.handle_zenoh_endpoint(body, out);
        }
        if fw::is_fw_key(key) {
            return self.handle_fw(key, body, out);
        }
        if self.trust.is_none() && loader_needs_trust(key) {
            return Handled::now(Err(HandlerError::Unavailable));
        }
        if self.foreign(self.owner, key, keys::CHUNK, keys::COMMIT) {
            return Handled::now(Err(HandlerError::InvalidRequest));
        }
        let ctx = Context {
            staging: matches!(self.origin, Origin::Air(_)),
            ..self.ctx
        };
        let outcome = handle_request(self.state, &ctx, &Request::new(key, body), out);
        let h = self.perform(key, outcome);
        let receiving = matches!(self.state.loader(), Loader::Receiving { .. });
        self.owner = esp_base::ota::owner_after(self.owner, self.origin, key, receiving);
        h
    }

    /// Performs a dispatch outcome (storage, notes, state).
    fn perform(&mut self, key: Key<'_>, outcome: Outcome<'_>) -> Handled {
        if outcome.action == Action::Start {
            return Handled {
                reply: Reply::Start,
                ..Handled::now(Ok(0))
            };
        }
        let mut notes = Notes::new();
        let mut stop = outcome.action.stops_payload();
        let reply = match self.storage(&outcome) {
            Ok(()) => {
                self.note_success(key, &outcome, &mut notes);
                self.state = outcome.state;
                self.served = served_after(&outcome.action, core::mem::take(&mut self.served));
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
        Handled {
            reply: Reply::Now(reply),
            stop,
            notes,
            reset: None,
            release_watchdog: false,
        }
    }

    /// A `sys/zenoh/endpoint` request: read (empty body) or set the
    /// endpoint. See the module docs.
    fn handle_zenoh_endpoint(&mut self, body: &[u8], out: &mut [u8]) -> Handled {
        let Some(current) = self.config else {
            return Handled::now(Err(HandlerError::Unavailable));
        };
        if body.is_empty() {
            let reply = match current.endpoint() {
                Some(e) => e.write_to(out).ok_or(HandlerError::BufferTooSmall),
                None => Err(HandlerError::NotFound),
            };
            return Handled::now(reply);
        }
        let endpoint = match Endpoint::parse_bytes(body) {
            Ok(e) => e,
            Err(_) => return Handled::now(Err(HandlerError::InvalidRequest)),
        };
        if current.endpoint() == Some(endpoint) {
            return Handled::now(Ok(0));
        }
        let next = Config { endpoint };
        let mut h = Handled::now(Ok(0));
        match config::save(self.store.flash().raw(), self.config_region, &current, next) {
            Ok(saved) => {
                self.config = Some(saved);
                let _ = h.notes.push(Note::ZenohEndpoint(endpoint));
            }
            Err(_) => {
                h.reply = Reply::Now(Err(HandlerError::Internal));
                let _ = h.notes.push(Note::Storage(StoreError::Flash));
            }
        }
        h
    }

    /// A `sys/fw/*` request, to the A/B updater only.
    fn handle_fw(&mut self, key: Key<'_>, body: &[u8], out: &mut [u8]) -> Handled {
        use middleware::fw_ota::keys as fwk;
        if self.foreign(self.fw_owner, key, fwk::CHUNK, fwk::COMMIT) {
            return Handled::now(Err(HandlerError::InvalidRequest));
        }
        let h = self.handle_fw_owned(key, body, out);
        let receiving = matches!(
            self.fw.state(),
            Some(middleware::fw_ota::FwUpdate::Receiving(_))
        );
        // The loader's ownership rule (`esp_base::ota::owner_after`) for
        // the firmware transfer.
        self.fw_owner = match (receiving, key == fwk::BEGIN) {
            (false, _) => None,
            (true, true) => Some(self.origin),
            (true, false) => self.fw_owner,
        };
        h
    }

    fn handle_fw_owned(&mut self, key: Key<'_>, body: &[u8], out: &mut [u8]) -> Handled {
        let ctx = FwContext {
            node: self.node(),
            trust: self.trust.as_ref().map(|_| self.state.trust()),
            self_test: self.self_test(),
        };
        let h = self
            .fw
            .handle(self.store.flash().raw(), &ctx, key, body, out);
        let mut notes = Notes::new();
        // At most two notes; the capacity is four.
        if let Some(r) = h.reset {
            let _ = notes.push(Note::Reset(r));
        }
        if h.release_watchdog {
            let _ = notes.push(Note::Confirmed);
        }
        Handled {
            reply: Reply::Now(h.reply),
            stop: false,
            notes,
            reset: h.reset,
            release_watchdog: h.release_watchdog,
        }
    }

    /// Performs the outcome's storage action.
    fn storage(&mut self, outcome: &Outcome<'_>) -> Result<(), StoreError> {
        // The counter the next state carries (reset to 0 by a claim).
        let control = outcome.state.control();
        match outcome.action {
            Action::Erase { len, .. } => self.store.erase(len),
            Action::Write { offset, data } => self.store.write(offset, data),
            Action::Activate {
                len,
                crc,
                trust: None,
            } => self.store.activate(len, crc),
            Action::Activate {
                len,
                crc,
                trust: Some(next),
            } => {
                let ts = self.trust.as_mut().ok_or(StoreError::Flash)?;
                self.store.activate_with(len, crc, |flash| {
                    ts.save(
                        flash,
                        &Persisted {
                            trust: next,
                            control,
                        },
                    )
                })
            }
            Action::Claim { trust } => {
                let ts = self.trust.as_mut().ok_or(StoreError::Flash)?;
                ts.save(self.store.flash(), &Persisted { trust, control })
            }
            Action::None | Action::Start | Action::Stop => Ok(()),
        }
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
                    push(Note::SignedAccepted {
                        min_version: min_version.0,
                    });
                }
                if self.state.status() == PayloadStatus::Running {
                    push(Note::ReplacesRunning);
                }
            }
            Action::Claim {
                trust: TrustState::Claimed { key, .. },
            } => {
                push(Note::Pinned {
                    prefix: [
                        key.as_bytes()[0],
                        key.as_bytes()[1],
                        key.as_bytes()[2],
                        key.as_bytes()[3],
                    ],
                });
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
pub(crate) mod tests;
#[cfg(test)]
mod wire;
