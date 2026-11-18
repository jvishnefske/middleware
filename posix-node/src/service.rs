//! The request service: the base firmware's request handling and storage
//! effects over the node's flash.
//!
//! [`Service`] owns the dispatcher state, the payload store and the trust
//! record, at the regions of [`crate::layout`]. [`Service::handle`] takes
//! one request and:
//!
//! 1. decides it with `esp_base::dispatch::handle_request`, the base
//!    firmware's own pure dispatcher (the request table, loader state
//!    machine, payload lifecycle and trust rules of esp-base and rp-base),
//!    with no re-key window (a Linux host has no presence button: re-key by
//!    stopping the node and deleting the trust sectors, `flash/sector-0128`
//!    and `flash/sector-0129`) and no staging (requests come over the
//!    serial protocol, never over the air);
//! 2. performs the storage action, as rp-base's service does: erase and
//!    write the inactive slot, activate it (a raised `min_version` saved
//!    inside `PayloadStore::activate_with`, after the CRC check and before
//!    the slot header), save a claimed key; then, for every action but a
//!    chunk write, makes the flash durable ([`Durable::sync`]) before the
//!    reply goes out. A failed action or sync replies `Internal` and
//!    leaves the state of `esp_base::dispatch::after_storage_failure` with
//!    the trust record on flash adopted, so memory never trusts less than
//!    a restart would.
//!
//! `sys/fw/*` reaches the dispatcher like any other `sys/` key and is
//! `NotFound`: the node is a board without firmware OTA (rp-base's
//! `Absent` backend). The binary is updated by the host's package manager.
//!
//! When the trust record cannot be read at start the service fails
//! closed: the requests that install or pin anything ([`loader_needs_trust`])
//! are `Unavailable`, rather than decided as on an unclaimed device.
//!
//! It returns a [`Handled`]: the reply (or [`Reply::Start`]: load and run
//! the payload, then answer), whether a running payload must stop, and
//! [`Note`]s for `sys/log`. Requests on keys a payload serves (ABI 5) are
//! routed before this, by [`crate::controller`] with `esp_base::serve`;
//! the service keeps the keys of the run that ended ([`Service::run_ended`])
//! so they answer `Unavailable` while it does not run
//! ([`Service::route_idle`]), and clears them when the image changes.
//! `sys/info` reports `Caps::SERVE` after the node id.

use core::fmt;

use embedded_storage::nor_flash::NorFlash;
use esp_base::binding_store::{self, BindingStore};
use esp_base::dispatch::{Action, Context, Outcome, State, after_storage_failure, handle_request};
use esp_base::image::Header;
use esp_base::serve::{Route, ServedKeys, route_request, served_after};
use esp_base::store::{PayloadStore, StoreError};
use esp_base::trust_store::{Persisted, RECORD_LEN, TrustStore};
use middleware::loader::manifest::{
    BoardName, ControlModules, Groups, Limits, Link, LinkKind, Manifest,
};
use middleware::loader::trust::TrustState;
use middleware::loader::{PayloadStatus, keys};
use middleware::transport::{HandlerError, Request};
use middleware::{Key, SenderId};

use crate::flash::Durable;
use crate::layout::{
    BINDING_OFFSET, BINDING_SIZE, MAX_IMAGE, PAYLOAD_OFFSET, PAYLOAD_SIZE, TRUST_OFFSET, TRUST_SIZE,
};

/// Whether a request depends on the trust record: installing a payload
/// (`begin`, `chunk`, `commit`) and pinning a key (`sys/trust/claim`).
pub fn loader_needs_trust(key: Key<'_>) -> bool {
    key == keys::BEGIN || key == keys::CHUNK || key == keys::COMMIT || key == keys::TRUST_CLAIM
}

/// What `sys/info` reports about this node, besides the store capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Facts {
    /// The platform ([`CHIP`]).
    pub chip: &'static str,
    /// The node's version.
    pub firmware: &'static str,
    /// The node id ([`crate::node_id`]).
    pub node: SenderId,
    /// The board, for the manifest's `BOARD` record ([`crate::facts`]).
    pub board: Option<BoardName>,
    /// The driver's control-module points, for the manifest's
    /// `CONTROL_MODULES` record ([`crate::facts_with_driver`]).
    pub control: Option<ControlModules>,
}

/// The `sys/info` chip name of this build: `linux-` and the architecture
/// (`std::env::consts::ARCH`): `linux-aarch64` (Pi Zero 2 W, Pi 3 and
/// later on a 64-bit OS), `linux-arm` (32-bit Raspberry Pi OS),
/// `linux-x86_64`; `host` on anything else.
pub const CHIP: &str = if cfg!(not(target_os = "linux")) {
    "host"
} else if cfg!(target_arch = "aarch64") {
    "linux-aarch64"
} else if cfg!(target_arch = "arm") {
    "linux-arm"
} else if cfg!(target_arch = "x86_64") {
    "linux-x86_64"
} else {
    "host"
};

/// What the node offers payloads (`sys/manifest`, MF3), next to
/// `PiDevice`: software timers and requests on payload keys; every
/// peripheral is `Unsupported` (see `crate::device`), so no other group
/// and no source; the serial link to the gateway.
pub const MANIFEST: Manifest = Manifest::new(middleware::wasm_abi::ABI_VERSION)
    .with_groups(Groups::TIMERS.with(Groups::SERVE).with(Groups::CHANNELS))
    .with_link(Link {
        id: 0,
        kind: LinkKind::SERIAL,
    })
    .with_limits(Limits {
        routes: 0,
        subscriptions: middleware::wasm_abi::SLOT_MAX as u8,
        ..Limits::ABI
    });

/// [`MANIFEST`] with the board named and the control modules counted, for
/// the life of the process. A process opens its service once (the tests a
/// few times), so the few hundred bytes leaked per open are bounded.
pub fn manifest_for(
    board: Option<BoardName>,
    control: Option<ControlModules>,
) -> &'static Manifest {
    if board.is_none() && control.is_none() {
        return &MANIFEST;
    }
    let mut m = MANIFEST;
    if let Some(b) = board {
        m = m.with_board(b);
    }
    if let Some(c) = control {
        m = m.with_control(c);
    }
    Box::leak(Box::new(m))
}

/// What [`Service::open`] found on flash, for the start log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Boot {
    /// The installed image: its header, or why there is none; `None` when
    /// the payload region could not be opened.
    pub installed: Option<Result<Header, StoreError>>,
    /// The trust record; `None` when it could not be read (installs are
    /// then refused).
    pub trust: Option<Persisted>,
}

/// A request's reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    /// Answer now: the body length in the caller's buffer, or the error.
    Now(Result<usize, HandlerError>),
    /// `sys/payload/start` of an installed payload: load and run it, then
    /// answer.
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
    /// A claim on a claimed node was refused.
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
                "trust: claim denied; the node is claimed (no re-key window: delete the trust sectors while it is stopped)",
            ),
        }
    }
}

/// What handling one request asks of the shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handled {
    /// The reply.
    pub reply: Reply,
    /// A running payload must stop (before the reply is sent).
    pub stop: bool,
    /// Events to log (at most three).
    pub notes: Vec<Note>,
}

impl Handled {
    /// An immediate reply, nothing else.
    pub fn now(reply: Result<usize, HandlerError>) -> Self {
        Handled {
            reply: Reply::Now(reply),
            stop: false,
            notes: Vec::new(),
        }
    }
}

/// Why a start failed, for [`Service::start_failed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartFailure {
    /// The image could not be read or failed its CRC.
    Store,
    /// It failed to compile or instantiate, or `init` trapped.
    Load,
}

/// The base firmware's request handling over flash `F`.
pub struct Service<F> {
    state: State,
    ctx: Context<'static>,
    store: Option<PayloadStore<F>>,
    trust: Option<TrustStore>,
    /// The payload's channel binding (BD3); `None` when its region could
    /// not be read (binds are then `Unavailable`).
    binding: Option<BindingStore>,
    /// The served keys of the payload run that ended.
    served: ServedKeys,
}

/// Whether both trust sectors can be read.
fn trust_readable<F: NorFlash>(flash: &mut F) -> bool {
    let mut buf = [0u8; RECORD_LEN];
    (0..2u32).all(|s| {
        flash
            .read(TRUST_OFFSET + s * esp_base::image::SECTOR, &mut buf)
            .is_ok()
    })
}

impl<F: NorFlash + Durable> Service<F> {
    /// Opens the trust record and then the payload store (which keeps the
    /// flash and lends it back for every trust save), verifies the
    /// installed image and builds the start state.
    pub fn open(mut flash: F, facts: Facts) -> (Self, Boot) {
        let opened = match trust_readable(&mut flash) {
            true => TrustStore::open(&mut flash, TRUST_OFFSET, TRUST_SIZE).ok(),
            false => None,
        };
        let (trust, persisted) = match opened {
            Some((t, p)) => (Some(t), Some(p)),
            None => (None, None),
        };
        let binding = BindingStore::open(&mut flash, BINDING_OFFSET, BINDING_SIZE).ok();
        let mut store = PayloadStore::new(flash, PAYLOAD_OFFSET, PAYLOAD_SIZE, MAX_IMAGE).ok();
        let installed = store.as_mut().map(PayloadStore::verify);
        let p = persisted.unwrap_or_default();
        let state = State::boot(matches!(installed, Some(Ok(_))))
            .with_trust(p.trust)
            .with_control(p.control)
            .bound(binding.and_then(|b| b.mark()));
        let ctx = Context {
            chip: facts.chip,
            firmware: facts.firmware,
            capacity: store.as_ref().map_or(0, PayloadStore::capacity),
            // No esp-base chip profile: `sys/board` is NotFound, as on rp-base.
            profile: None,
            runtime: true,
            rekey_window: false,
            staging: false,
            node: Some(facts.node),
            manifest: Some(manifest_for(facts.board, facts.control)),
        };
        let svc = Service {
            state,
            ctx,
            store,
            trust,
            binding,
            served: ServedKeys::new(),
        };
        (
            svc,
            Boot {
                installed,
                trust: persisted,
            },
        )
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

    /// The largest image the store accepts (0 without a store).
    pub const fn capacity(&self) -> u32 {
        self.ctx.capacity
    }

    /// The served keys of the payload run that ended.
    pub fn served(&self) -> &ServedKeys {
        &self.served
    }

    /// A payload run ended having served `keys`.
    pub fn run_ended(&mut self, keys: ServedKeys) {
        self.served = keys;
    }

    /// Where a request goes while no payload runs: `Unavailable` on the
    /// last run's served keys, the service otherwise.
    pub fn route_idle(&self, key: Key<'_>) -> Route {
        route_request(key, self.status(), self.served.iter())
    }

    /// The payload store, to read the installed image for a start.
    pub fn store_mut(&mut self) -> Option<&mut PayloadStore<F>> {
        self.store.as_mut()
    }

    /// Makes everything written durable (at shutdown).
    pub fn sync(&mut self) -> Result<(), StoreError> {
        match self.store.as_mut() {
            Some(s) => s.flash().sync().map_err(|_| StoreError::Flash),
            None => Ok(()),
        }
    }

    /// Copies the stored binding record into `out` (at least
    /// `middleware::loader::binding::BINDING_MAX` bytes) for a start; its
    /// length, `None` without one or when it cannot be read.
    pub fn binding_record(&mut self, out: &mut [u8]) -> Option<usize> {
        let (b, st) = (self.binding.as_ref()?, self.store.as_mut()?);
        b.load(st.flash(), out).ok().flatten()
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
        if self.trust.is_none() && loader_needs_trust(key) {
            return Handled::now(Err(HandlerError::Unavailable));
        }
        let outcome = handle_request(self.state, &self.ctx, &Request::new(key, body), out);
        if matches!(outcome.action, Action::Bind { .. } | Action::Binding) {
            let done = match self.store.as_mut() {
                Some(st) => {
                    binding_store::perform(self.binding.as_mut(), st.flash(), &outcome, out)
                }
                None => Some((outcome.state, Err(HandlerError::Unavailable))),
            };
            if let Some((next, mut reply)) = done {
                if matches!(outcome.action, Action::Bind { .. })
                    && reply.is_ok()
                    && self.sync().is_err()
                {
                    reply = Err(HandlerError::Internal);
                }
                self.state = next;
                return Handled::now(reply);
            }
        }
        if outcome.action == Action::Start {
            return Handled {
                reply: Reply::Start,
                stop: false,
                notes: Vec::new(),
            };
        }
        let mut notes = Vec::new();
        let mut stop = outcome.action.stops_payload();
        let reply = match self.storage(&outcome) {
            Ok(()) => {
                self.note_success(key, &outcome, &mut notes);
                self.state = outcome.state;
                self.served = served_after(&outcome.action, core::mem::take(&mut self.served));
                outcome.reply
            }
            Err(e) => {
                notes.push(Note::Storage(e));
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
        }
    }

    /// Performs the outcome's storage action, then syncs (all but writes).
    fn storage(&mut self, outcome: &Outcome<'_>) -> Result<(), StoreError> {
        let control = outcome.state.control();
        match outcome.action {
            Action::Erase { len, .. } => self.store()?.erase(len),
            Action::Write { offset, data } => return self.store()?.write(offset, data),
            Action::Activate {
                len,
                crc,
                trust: None,
            } => self.store()?.activate(len, crc),
            Action::Activate {
                len,
                crc,
                trust: Some(next),
            } => {
                let (Some(store), Some(ts)) = (self.store.as_mut(), self.trust.as_mut()) else {
                    return Err(StoreError::Flash);
                };
                store.activate_with(len, crc, |flash| {
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
                let (Some(store), Some(ts)) = (self.store.as_mut(), self.trust.as_mut()) else {
                    return Err(StoreError::Flash);
                };
                ts.save(store.flash(), &Persisted { trust, control })
            }
            // Performed by `binding_store::perform` in `handle`.
            Action::None | Action::Start | Action::Stop | Action::Bind { .. } | Action::Binding => {
                return Ok(());
            }
        }?;
        self.sync()
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
    fn note_success(&self, key: Key<'_>, outcome: &Outcome<'_>, notes: &mut Vec<Note>) {
        match outcome.action {
            Action::Activate { len, trust, .. } => {
                notes.push(Note::Installed { len });
                if let Some(TrustState::Claimed { min_version, .. }) = trust {
                    notes.push(Note::SignedAccepted {
                        min_version: min_version.0,
                    });
                }
                if self.state.status() == PayloadStatus::Running {
                    notes.push(Note::ReplacesRunning);
                }
            }
            Action::Claim {
                trust: TrustState::Claimed { key, .. },
            } => {
                notes.push(Note::Pinned {
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
                notes.push(Note::CommitDenied { min_version });
            } else {
                notes.push(Note::ClaimDenied);
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
