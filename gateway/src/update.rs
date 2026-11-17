//! The update gate: which `gw/sys` update requests reach the device, for
//! whom. Pure; the shell ([`crate::shell`]) forwards what [`UpdateGate::admit`]
//! admits and reports the device's answer back.
//!
//! # Why the gateway decides
//!
//! The gateway reaches the device over its serial port, where the device
//! grants physical-access authority: a serial `sys/loader/begin` stops the
//! running payload, `start` and `stop` take empty bodies, and an unclaimed
//! device accepts any commit (trust on first use). Everything a mesh caller
//! sends through `gw/sys` would arrive with that authority, so the gateway
//! applies the device's own over-the-air rules (`esp_base::ota::admit`,
//! design.md "Over-the-air payload update") to it, with the caller's
//! authenticated SPIFFE ID (`RequestMeta::principal`, from the sidecar)
//! standing for the radio peer's MAC, and only for principals in the
//! updater allowlist (`MW_GATEWAY_UPDATERS`, empty by default: updates
//! disabled). The device still verifies every signed commit itself; the
//! gateway only ever refuses more.
//!
//! # Policy
//!
//! | `gw/sys` key | not an updater | updater, device not known claimed | updater, claimed device |
//! |--------------|----------------|-----------------------------------|-------------------------|
//! | `sys/trust/info`, `sys/fw/info`, `sys/payload/status` | `Denied` | forwarded (read only) | forwarded (read only) |
//! | `sys/loader/begin`, `sys/fw/begin` | `Denied` | `Denied` | starts a transfer owned by the caller; while another caller's transfer is live, `InvalidRequest` and that transfer is untouched |
//! | `sys/loader/chunk`, `sys/fw/chunk` | `Denied` | `Denied` | the owner of a transfer of that kind only, else `InvalidRequest` |
//! | `sys/loader/commit` | `Denied` | `Denied` | owner first (else `InvalidRequest`, transfer untouched); then a signed body (`[version][signature]`) only: unsigned is `Denied` and ends the owner's transfer; forwarded, the device verifies the signature; the transfer ends with the answer |
//! | `sys/fw/commit` | `Denied` | `Denied` | as `sys/loader/commit`, with the 64-byte firmware signature |
//! | `sys/fw/reboot`, `confirm`, `abort` | `Denied` | `Denied` | forwarded unless another caller's transfer is live (`InvalidRequest`); `abort` ends the caller's own |
//! | `sys/payload/start`, `stop` | `Denied` | `Denied` | a signed control body only (`[counter u64][signature 64]` by the pinned key over `mw-control-v1`, op, the device's MAC, counter), verified here with the device's own check for the pinned key's kind (Ed25519 or ECDSA P-256, `loader::trust::verify_signature`), counter strictly above the last accepted; forwarded with an empty body (the serial form). Unsigned or not verifying: `Denied` |
//! | `sys/trust/claim` | `Denied` | `Denied` | `Denied` (physical presence only) |
//!
//! Every other `sys/...` key is the plain allowlist's (`SysAllow`:
//! `sys/info`, `sys/log`); `sys/trust/claim` and the keys above never pass
//! through it, whatever `MW_GATEWAY_SYS_ALLOW` lists.
//!
//! **Unsigned start and stop are refused for updaters too.** Over the
//! serial port the device would run them unsigned; through the mesh the
//! gateway requires what the device requires over the air, so a stolen
//! updater identity alone cannot stop or restart a payload, and a captured
//! request cannot be replayed. The control target is the device's MAC:
//! the first six bytes of the node id it reports in `sys/info` (the ESP
//! base firmware's node id is its station MAC). `mw-update` signs with the
//! current Unix time in milliseconds, as `mwctl` does.
//!
//! **The counter floor is persisted and fails closed.** A captured signed
//! start/stop must never be accepted twice, also across gateway restarts
//! on a host without a real-time clock (a Pi Zero 2 W or Pi 3 boots near
//! 1970, or at a stale `fake-hwclock` time, until NTP syncs). So:
//!
//! * every accepted counter is written durably by the shell **before** the
//!   request is forwarded ([`Ticket::persist`], [`crate::state::ControlStore`]);
//!   if the write fails the request is refused `Unavailable` and the
//!   counter rolled back ([`UpdateGate::unpersisted`]), never forwarded;
//! * at start the floor is the larger of the persisted counter and the
//!   clock, the clock counting only when it is past a sane lower bound
//!   ([`crate::state::floor_at_start`]);
//! * with neither (no state and an unsynced or unreadable clock, or an
//!   unreadable state file) there is no floor and every signed start/stop
//!   is `Unavailable` ([`Refusal::NoControlFloor`]) until a floor exists;
//!   nothing ever falls back to 0.
//!
//! One floor serves whichever device is on the port: a counter accepted
//! for one device only raises the bar for the next, and a captured request
//! cannot be used on another device anyway (its MAC is signed). Uploads do
//! not depend on the floor: a signed commit's version is checked (and
//! persisted) by the device itself, so `begin`/`chunk`/`commit` stay
//! available without one.
//!
//! **"Claimed" is what the device last said.** The shell asks
//! `sys/trust/info` after binding and on every recheck, and every
//! forwarded `sys/trust/info` answer updates it ([`UpdateGate::observe_trust`]);
//! a device that never answered it (or answers `NotFound`) is not known
//! claimed. A different device on the port ([`UpdateGate::forget_device`])
//! clears the trust facts and the transfer.
//!
//! **Begin through the gateway is a serial begin**: the device stops its
//! payload while it loads (no staging beside it, unlike over the air). That
//! is why it needs an authenticated updater; the signed commit is still
//! the only thing that installs.
//!
//! # Bounds and timing
//!
//! One transfer at a time. A transfer the owner leaves idle for
//! [`TRANSFER_IDLE_MS`] is abandoned: the next `begin` from any updater
//! takes over. Every mutating request must carry an `x-request-id`
//! (`InvalidRequest` without); a resend with the same id never reaches the
//! device twice ([`replay`]). A device body that would not fit one serial
//! frame (`MAX_RAW_FRAME`) is `InvalidRequest` before it is sent. Every
//! decision is counted in [`UpdateCounters`].

pub mod replay;

use middleware::SenderId;
use middleware::fw_ota::wire::keys as fw;
use middleware::key::Key;
use middleware::loader::keys;
use middleware::loader::trust::{
    ControlCounter, ControlOp, PublicKey, SIGNATURE_LEN, SignedCommit, SignedControl, TrustInfo,
    TrustState, control_message, verify_signature,
};
use middleware::serial::frame::{FRAME_OVERHEAD, MAX_RAW_FRAME};
use middleware::transport::HandlerError;

use replay::{Fingerprint, Lookup, Replay};

/// Most updater SPIFFE IDs.
pub const MAX_UPDATERS: usize = 8;
/// How long an idle transfer stays its owner's, in milliseconds.
pub const TRANSFER_IDLE_MS: u64 = 60_000;

/// An update request, by key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateOp {
    /// `sys/trust/info`.
    TrustInfo,
    /// `sys/payload/status`.
    PayloadStatus,
    /// `sys/loader/begin`.
    LoaderBegin,
    /// `sys/loader/chunk`.
    LoaderChunk,
    /// `sys/loader/commit`.
    LoaderCommit,
    /// `sys/payload/start`.
    Start,
    /// `sys/payload/stop`.
    Stop,
    /// `sys/fw/info`.
    FwInfo,
    /// `sys/fw/begin`.
    FwBegin,
    /// `sys/fw/chunk`.
    FwChunk,
    /// `sys/fw/commit`.
    FwCommit,
    /// `sys/fw/reboot`.
    FwReboot,
    /// `sys/fw/confirm`.
    FwConfirm,
    /// `sys/fw/abort`.
    FwAbort,
}

/// Where a `gw/sys` key belongs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SysClass {
    /// An update request: [`UpdateGate::admit`] decides.
    Update(UpdateOp),
    /// Never through the gateway (`sys/trust/claim`, an unknown
    /// `sys/fw/...` key).
    Never,
    /// Not an update key: the plain allowlist decides.
    Plain,
}

/// Which allowed key is which.
pub fn classify(key: Key<'_>) -> SysClass {
    const TABLE: [(Key<'static>, UpdateOp); 14] = [
        (keys::TRUST_INFO, UpdateOp::TrustInfo),
        (keys::STATUS, UpdateOp::PayloadStatus),
        (keys::BEGIN, UpdateOp::LoaderBegin),
        (keys::CHUNK, UpdateOp::LoaderChunk),
        (keys::COMMIT, UpdateOp::LoaderCommit),
        (keys::START, UpdateOp::Start),
        (keys::STOP, UpdateOp::Stop),
        (fw::INFO, UpdateOp::FwInfo),
        (fw::BEGIN, UpdateOp::FwBegin),
        (fw::CHUNK, UpdateOp::FwChunk),
        (fw::COMMIT, UpdateOp::FwCommit),
        (fw::REBOOT, UpdateOp::FwReboot),
        (fw::CONFIRM, UpdateOp::FwConfirm),
        (fw::ABORT, UpdateOp::FwAbort),
    ];
    if let Some((_, op)) = TABLE.iter().find(|(k, _)| *k == key) {
        return SysClass::Update(*op);
    }
    let s = key.as_str();
    if key == keys::TRUST_CLAIM || s.starts_with("sys/fw/") || s.starts_with("sys/trust/") {
        SysClass::Never
    } else {
        SysClass::Plain
    }
}

/// What kind of image a transfer carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferKind {
    /// A WASM payload (`sys/loader/*`).
    Payload,
    /// RP2350 firmware (`sys/fw/*`).
    Firmware,
}

impl UpdateOp {
    /// Read-only requests: forwarded to any updater, claimed or not, with
    /// no request id needed.
    pub const fn read_only(self) -> bool {
        matches!(
            self,
            UpdateOp::TrustInfo | UpdateOp::PayloadStatus | UpdateOp::FwInfo
        )
    }

    /// The transfer kind a begin, chunk or commit belongs to.
    pub const fn transfer(self) -> Option<TransferKind> {
        match self {
            UpdateOp::LoaderBegin | UpdateOp::LoaderChunk | UpdateOp::LoaderCommit => {
                Some(TransferKind::Payload)
            }
            UpdateOp::FwBegin | UpdateOp::FwChunk | UpdateOp::FwCommit => {
                Some(TransferKind::Firmware)
            }
            _ => None,
        }
    }

    const fn is_begin(self) -> bool {
        matches!(self, UpdateOp::LoaderBegin | UpdateOp::FwBegin)
    }

    const fn is_commit(self) -> bool {
        matches!(self, UpdateOp::LoaderCommit | UpdateOp::FwCommit)
    }
}

/// The updater allowlist: SPIFFE IDs, at most [`MAX_UPDATERS`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Updaters(Vec<String>);

impl Updaters {
    /// Nobody: updates disabled.
    pub fn none() -> Self {
        Self::default()
    }

    /// Exactly `ids`; `None` unless each is a `spiffe://` ID and there are
    /// at most [`MAX_UPDATERS`].
    pub fn new<I, S>(ids: I) -> Option<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let ids: Vec<String> = ids.into_iter().map(Into::into).collect();
        let ok = ids.len() <= MAX_UPDATERS
            && ids
                .iter()
                .all(|i| i.len() > "spiffe://".len() && i.starts_with("spiffe://"));
        ok.then_some(Updaters(ids))
    }

    /// Whether `principal` may update.
    pub fn contains(&self, principal: &str) -> bool {
        self.0.iter().any(|i| i == principal)
    }

    /// The IDs.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }

    /// True when updates are disabled.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// What the gateway knows of the device's root of trust.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeviceTrust {
    /// Never answered `sys/trust/info` (or answered with an error).
    #[default]
    Unknown,
    /// No key pinned.
    Unclaimed,
    /// This key is pinned.
    Claimed(PublicKey),
}

/// The transfer in progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transfer {
    /// Its owner's SPIFFE ID.
    pub owner: String,
    /// What it carries.
    pub kind: TransferKind,
    /// When its owner last sent a request for it (gateway milliseconds).
    pub last_ms: u64,
}

impl Transfer {
    fn live(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.last_ms) < TRANSFER_IDLE_MS
    }
}

/// Who sends an update request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Caller<'a> {
    /// The authenticated SPIFFE ID, if any.
    pub principal: Option<&'a str>,
    /// The caller's `x-request-id`, if any.
    pub request_id: Option<&'a str>,
}

/// Why an update request is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// No principal, or one not in the updater allowlist.
    NotUpdater,
    /// Never through the gateway.
    Never,
    /// The device is not known to be claimed.
    Unclaimed,
    /// The device's node id is not known yet (control target).
    Unbound,
    /// No control counter floor is known (no persisted state and an
    /// unsynced clock), or the accepted counter could not be persisted.
    NoControlFloor,
    /// A mutating request without an `x-request-id`.
    NoRequestId,
    /// The request id names another request of this caller.
    ReusedId,
    /// The first copy of this request is still at the device.
    InFlight,
    /// Another caller's transfer is live.
    Busy,
    /// A chunk or commit from a caller that does not own a transfer of
    /// that kind.
    NotOwner,
    /// The device body would not fit one serial frame.
    TooLarge,
    /// An unsigned or malformed commit (ends the owner's transfer).
    UnsignedCommit,
    /// An unsigned or malformed start or stop.
    UnsignedControl,
    /// A signed start or stop that does not verify, or whose counter is
    /// not above the last accepted.
    ControlRefused,
}

impl Refusal {
    /// The status the caller gets.
    pub const fn error(self) -> HandlerError {
        match self {
            Refusal::NotUpdater
            | Refusal::Never
            | Refusal::Unclaimed
            | Refusal::UnsignedCommit
            | Refusal::UnsignedControl
            | Refusal::ControlRefused => HandlerError::Denied,
            Refusal::NoRequestId
            | Refusal::ReusedId
            | Refusal::Busy
            | Refusal::NotOwner
            | Refusal::TooLarge => HandlerError::InvalidRequest,
            Refusal::Unbound | Refusal::InFlight | Refusal::NoControlFloor => {
                HandlerError::Unavailable
            }
        }
    }

    /// A short reason for the log.
    pub const fn reason(self) -> &'static str {
        match self {
            Refusal::NotUpdater => "caller is not an updater (MW_GATEWAY_UPDATERS)",
            Refusal::Never => "never through the gateway",
            Refusal::Unclaimed => "device not claimed; updates need a pinned key",
            Refusal::Unbound => "device node id not known yet",
            Refusal::NoControlFloor => {
                "no durable control counter floor (no gateway state and the clock is not set)"
            }
            Refusal::NoRequestId => "update requests need an x-request-id",
            Refusal::ReusedId => "request id reused for another request",
            Refusal::InFlight => "the first copy of this request is still at the device",
            Refusal::Busy => "another updater's transfer is in progress",
            Refusal::NotOwner => "caller does not own the transfer",
            Refusal::TooLarge => "request does not fit one serial frame",
            Refusal::UnsignedCommit => "commit through the gateway must be signed",
            Refusal::UnsignedControl => "start/stop through the gateway must be signed",
            Refusal::ControlRefused => {
                "control signature invalid (pinned key, this device, this command) or counter not above the last"
            }
        }
    }
}

/// What to remember about a forwarded request until the device answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ticket {
    op: UpdateOp,
    principal: String,
    request_id: Option<String>,
    /// An accepted control counter and the floor before it.
    control: Option<(ControlCounter, ControlCounter)>,
}

impl Ticket {
    /// The request.
    pub fn op(&self) -> UpdateOp {
        self.op
    }

    /// The control counter the shell must write durably before it
    /// forwards this request (an accepted start or stop); on failure it
    /// reports [`UpdateGate::unpersisted`] instead of forwarding.
    pub fn persist(&self) -> Option<ControlCounter> {
        self.control.map(|(c, _)| c)
    }
}

/// How an admitted update request proceeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// Send `body` to the device; report its answer with
    /// [`UpdateGate::complete`] (or [`UpdateGate::abandon`] if none comes).
    Forward {
        /// The device request's body (empty for a verified start or stop).
        body: Vec<u8>,
        /// What to report back.
        ticket: Ticket,
    },
    /// Answer with this (the device answered this request before).
    Replay(Result<Vec<u8>, HandlerError>),
}

/// Update decisions (saturating).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UpdateCounters {
    /// Requests forwarded to the device.
    pub forwarded: u32,
    /// Resends answered from the replay cache.
    pub replayed: u32,
    /// Requests refused `Denied`.
    pub denied: u32,
    /// Requests refused otherwise (`InvalidRequest`, `Unavailable`).
    pub refused: u32,
    /// Forwarded requests the device never answered.
    pub abandoned: u32,
    /// Accepted starts/stops refused because their counter could not be
    /// persisted.
    pub unpersisted: u32,
}

fn inc(n: u32) -> u32 {
    n.saturating_add(1)
}

/// The gate (module docs).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UpdateGate {
    updaters: Updaters,
    trust: DeviceTrust,
    transfer: Option<Transfer>,
    control: Option<ControlCounter>,
    replay: Replay,
    counters: UpdateCounters,
}

/// The first six bytes of a node id: the control target.
pub fn control_target(node: SenderId) -> [u8; 6] {
    let b = node.0;
    [b[0], b[1], b[2], b[3], b[4], b[5]]
}

/// Verifies a signed control request against `key` with the device's own
/// check ([`verify_signature`]): the algorithm of the key's kind, Ed25519
/// `verify_strict` or ECDSA P-256 / SHA-256, so the gate admits exactly
/// what the device would.
fn verify_control(key: &PublicKey, op: ControlOp, target: &[u8; 6], auth: &SignedControl) -> bool {
    let message = control_message(op, target, auth.counter);
    verify_signature(key, &message, &auth.signature)
}

impl UpdateGate {
    /// Updates disabled: every update request is `Denied`.
    pub fn disabled() -> Self {
        Self::default()
    }

    /// Updates for `updaters`, with start/stop counters required above
    /// `control_floor`; `None` refuses every start/stop until a floor is
    /// known (see the module docs and [`crate::state::floor_at_start`]).
    pub fn new(updaters: Updaters, control_floor: Option<ControlCounter>) -> Self {
        UpdateGate {
            updaters,
            control: control_floor,
            ..Self::default()
        }
    }

    /// The allowlist.
    pub fn updaters(&self) -> &Updaters {
        &self.updaters
    }

    /// The device's root of trust as last seen.
    pub fn trust(&self) -> DeviceTrust {
        self.trust
    }

    /// The transfer in progress, if any (live or not).
    pub fn transfer(&self) -> Option<&Transfer> {
        self.transfer.as_ref()
    }

    /// The last accepted control counter (or the floor); `None` when no
    /// floor is known.
    pub fn control_counter(&self) -> Option<ControlCounter> {
        self.control
    }

    /// The counters.
    pub fn counters(&self) -> UpdateCounters {
        self.counters
    }

    fn count(self, f: impl FnOnce(&mut UpdateCounters)) -> Self {
        let mut counters = self.counters;
        f(&mut counters);
        UpdateGate { counters, ..self }
    }

    fn refuse(self, why: Refusal) -> (Self, Result<Admission, Refusal>) {
        let g = if why.error() == HandlerError::Denied {
            self.count(|c| c.denied = inc(c.denied))
        } else {
            self.count(|c| c.refused = inc(c.refused))
        };
        (g, Err(why))
    }

    /// Takes a `sys/trust/info` answer in.
    pub fn observe_trust(self, result: Result<&[u8], HandlerError>) -> Self {
        let trust = match result.ok().and_then(TrustInfo::decode) {
            Some(TrustInfo {
                state: TrustState::Claimed { key, .. },
                ..
            }) => DeviceTrust::Claimed(key),
            Some(_) => DeviceTrust::Unclaimed,
            None => DeviceTrust::Unknown,
        };
        UpdateGate { trust, ..self }
    }

    /// Another device is on the port: what was known of the previous one
    /// goes (trust facts, transfer).
    pub fn forget_device(self) -> Self {
        UpdateGate {
            trust: DeviceTrust::Unknown,
            transfer: None,
            ..self
        }
    }

    /// Drops stale replay entries at `now_ms`.
    pub fn expire(self, now_ms: u64) -> Self {
        UpdateGate {
            replay: self.replay.expire(now_ms),
            ..self
        }
    }

    /// Decides an update request `op` on `key` with `body` from `caller`,
    /// for the device bound as `device`, at `now_ms` (module docs). The
    /// checks run in this order: updater; replay (a resend is answered
    /// before anything else); request id; claimed; size; transfer
    /// ownership; signatures.
    pub fn admit(
        self,
        caller: Caller<'_>,
        op: UpdateOp,
        key: Key<'_>,
        body: &[u8],
        device: Option<SenderId>,
        now_ms: u64,
    ) -> (Self, Result<Admission, Refusal>) {
        let Some(principal) = caller.principal.filter(|p| self.updaters.contains(p)) else {
            return self.refuse(Refusal::NotUpdater);
        };
        if FRAME_OVERHEAD + key.as_bytes().len() + body.len() > MAX_RAW_FRAME {
            return self.refuse(Refusal::TooLarge);
        }
        let ticket = |request_id: Option<&str>, control| Ticket {
            op,
            principal: principal.to_owned(),
            request_id: request_id.map(str::to_owned),
            control,
        };
        if op.read_only() {
            let g = self.count(|c| c.forwarded = inc(c.forwarded));
            return (
                g,
                Ok(Admission::Forward {
                    body: body.to_vec(),
                    ticket: ticket(None, None),
                }),
            );
        }
        let Some(request_id) = caller.request_id else {
            return self.refuse(Refusal::NoRequestId);
        };
        let print = Fingerprint::of(key, body);
        match self.replay.lookup(principal, request_id, &print) {
            Lookup::Fresh => {}
            Lookup::InFlight => return self.refuse(Refusal::InFlight),
            Lookup::Mismatch => return self.refuse(Refusal::ReusedId),
            Lookup::Replay(r) => {
                let g = self.count(|c| c.replayed = inc(c.replayed));
                return (g, Ok(Admission::Replay(r)));
            }
        }
        let DeviceTrust::Claimed(pinned) = self.trust else {
            return self.refuse(Refusal::Unclaimed);
        };
        // (caller owns it, its kind) of the live transfer, if any.
        let held = self
            .transfer
            .as_ref()
            .filter(|t| t.live(now_ms))
            .map(|t| (t.owner == principal, t.kind));
        let others = held.is_some_and(|(mine, _)| !mine);
        let mut gate = self;
        let mut out = body.to_vec();
        let mut control = None;
        if op.is_begin() {
            if others {
                return gate.refuse(Refusal::Busy);
            }
        } else if let Some(kind) = op.transfer() {
            let owns = held == Some((true, kind));
            if !owns {
                return gate.refuse(Refusal::NotOwner);
            }
            if op.is_commit() {
                let signed = match op {
                    UpdateOp::LoaderCommit => matches!(SignedCommit::parse_body(body), Ok(Some(_))),
                    _ => body.len() == SIGNATURE_LEN,
                };
                if !signed {
                    gate.transfer = None;
                    return gate.refuse(Refusal::UnsignedCommit);
                }
            }
        } else if matches!(op, UpdateOp::Start | UpdateOp::Stop) {
            let Some(node) = device else {
                return gate.refuse(Refusal::Unbound);
            };
            let ctl = if op == UpdateOp::Start {
                ControlOp::Start
            } else {
                ControlOp::Stop
            };
            let Ok(Some(auth)) = SignedControl::parse_body(body) else {
                return gate.refuse(Refusal::UnsignedControl);
            };
            let Some(floor) = gate.control else {
                return gate.refuse(Refusal::NoControlFloor);
            };
            if auth.counter <= floor || !verify_control(&pinned, ctl, &control_target(node), &auth)
            {
                return gate.refuse(Refusal::ControlRefused);
            }
            // Adopted now; the shell persists it before acting, as the
            // device does.
            gate.control = Some(auth.counter);
            control = Some((auth.counter, floor));
            out = Vec::new();
        } else if others {
            // reboot, confirm, abort.
            return gate.refuse(Refusal::Busy);
        }
        if let Some(kind) = op.transfer() {
            gate.transfer = Some(Transfer {
                owner: principal.to_owned(),
                kind,
                last_ms: now_ms,
            });
        }
        gate.replay = gate.replay.record(principal, request_id, print, now_ms);
        let gate = gate.count(|c| c.forwarded = inc(c.forwarded));
        (
            gate,
            Ok(Admission::Forward {
                body: out,
                ticket: ticket(Some(request_id), control),
            }),
        )
    }

    /// The device answered a forwarded request: keeps the answer for
    /// resends and moves the transfer on (a failed begin, any commit, an
    /// abort or reboot end it).
    pub fn complete(
        self,
        ticket: &Ticket,
        result: Result<&[u8], HandlerError>,
        now_ms: u64,
    ) -> Self {
        let mut gate = self;
        if ticket.op == UpdateOp::TrustInfo {
            gate = gate.observe_trust(result);
        }
        let mine = gate
            .transfer
            .as_ref()
            .is_some_and(|t| t.owner == ticket.principal);
        let ends = ticket.op.is_commit()
            || (ticket.op.is_begin() && result.is_err())
            || matches!(ticket.op, UpdateOp::FwAbort | UpdateOp::FwReboot);
        if mine && ends {
            gate.transfer = None;
        } else if mine && ticket.op.transfer().is_some() {
            gate.transfer = gate.transfer.map(|t| Transfer {
                last_ms: now_ms,
                ..t
            });
        }
        if let Some(id) = &ticket.request_id {
            gate.replay = gate.replay.answer(&ticket.principal, id, result, now_ms);
        }
        gate
    }

    /// The shell could not persist the ticket's control counter, so the
    /// request was not forwarded: the counter is rolled back (if nothing
    /// newer was accepted since), the request forgotten so a resend is
    /// decided afresh, and the refusal counted.
    pub fn unpersisted(self, ticket: &Ticket) -> Self {
        let mut gate = self.count(|c| c.unpersisted = inc(c.unpersisted));
        if let Some((counter, prev)) = ticket.control
            && gate.control == Some(counter)
        {
            gate.control = Some(prev);
        }
        if let Some(id) = &ticket.request_id {
            gate.replay = gate.replay.forget(&ticket.principal, id);
        }
        gate
    }

    /// The device never answered a forwarded request: its resend will be
    /// forwarded again (the transfer stays its owner's until idle).
    pub fn abandon(self, ticket: &Ticket) -> Self {
        let mut gate = self.count(|c| c.abandoned = inc(c.abandoned));
        if let Some(id) = &ticket.request_id {
            gate.replay = gate.replay.forget(&ticket.principal, id);
        }
        gate
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use middleware::loader::trust::{Signature, Version};

    const UPD: &str = "spiffe://infra.local/ns/ops/sa/updater";
    const UPD2: &str = "spiffe://infra.local/ns/ops/sa/updater-2";
    const STRANGER: &str = "spiffe://infra.local/ns/apps/sa/demo-caller";
    const NODE: SenderId = SenderId::from_mac([0x24, 0x6F, 0x28, 0xAA, 0xBB, 0xCC]);
    const FLOOR: u64 = 1_761_500_000_000;

    fn seed() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn pinned() -> PublicKey {
        PublicKey::Ed25519(seed().verifying_key().to_bytes())
    }

    fn trust_info(state: TrustState) -> Vec<u8> {
        let mut out = [0u8; 64];
        let n = TrustInfo {
            state,
            rekey_open: false,
        }
        .encode(&mut out)
        .unwrap();
        out[..n].to_vec()
    }

    fn claimed() -> UpdateGate {
        let g = UpdateGate::new(
            Updaters::new([UPD, UPD2]).unwrap(),
            Some(ControlCounter(FLOOR)),
        );
        let info = trust_info(TrustState::Claimed {
            key: pinned(),
            min_version: Version(0),
        });
        g.observe_trust(Ok(&info))
    }

    fn as_(principal: &'static str, id: &'static str) -> Caller<'static> {
        Caller {
            principal: Some(principal),
            request_id: Some(id),
        }
    }

    fn key_of(op: UpdateOp) -> Key<'static> {
        match op {
            UpdateOp::TrustInfo => keys::TRUST_INFO,
            UpdateOp::PayloadStatus => keys::STATUS,
            UpdateOp::LoaderBegin => keys::BEGIN,
            UpdateOp::LoaderChunk => keys::CHUNK,
            UpdateOp::LoaderCommit => keys::COMMIT,
            UpdateOp::Start => keys::START,
            UpdateOp::Stop => keys::STOP,
            UpdateOp::FwInfo => fw::INFO,
            UpdateOp::FwBegin => fw::BEGIN,
            UpdateOp::FwChunk => fw::CHUNK,
            UpdateOp::FwCommit => fw::COMMIT,
            UpdateOp::FwReboot => fw::REBOOT,
            UpdateOp::FwConfirm => fw::CONFIRM,
            UpdateOp::FwAbort => fw::ABORT,
        }
    }

    fn admit(
        g: UpdateGate,
        c: Caller<'_>,
        op: UpdateOp,
        body: &[u8],
    ) -> (UpdateGate, Result<Admission, Refusal>) {
        g.admit(c, op, key_of(op), body, Some(NODE), 1_000)
    }

    fn forwarded(r: &Result<Admission, Refusal>) -> Option<(&[u8], &Ticket)> {
        match r {
            Ok(Admission::Forward { body, ticket }) => Some((body, ticket)),
            _ => None,
        }
    }

    fn signed_commit() -> Vec<u8> {
        SignedCommit {
            version: Version(5),
            signature: Signature([1; 64]),
        }
        .encode()
        .to_vec()
    }

    fn control(op: ControlOp, counter: u64, target: [u8; 6]) -> Vec<u8> {
        let m = control_message(op, &target, ControlCounter(counter));
        SignedControl {
            counter: ControlCounter(counter),
            signature: Signature(seed().sign(&m).to_bytes()),
        }
        .encode()
        .to_vec()
    }

    const ALL: [UpdateOp; 14] = [
        UpdateOp::TrustInfo,
        UpdateOp::PayloadStatus,
        UpdateOp::LoaderBegin,
        UpdateOp::LoaderChunk,
        UpdateOp::LoaderCommit,
        UpdateOp::Start,
        UpdateOp::Stop,
        UpdateOp::FwInfo,
        UpdateOp::FwBegin,
        UpdateOp::FwChunk,
        UpdateOp::FwCommit,
        UpdateOp::FwReboot,
        UpdateOp::FwConfirm,
        UpdateOp::FwAbort,
    ];

    #[test]
    fn rg20_keys_classify_and_claim_never_passes() {
        for op in ALL {
            assert_eq!(classify(key_of(op)), SysClass::Update(op));
        }
        assert_eq!(classify(keys::TRUST_CLAIM), SysClass::Never);
        assert_eq!(classify(Key::from_static("sys/fw/flash")), SysClass::Never);
        assert_eq!(
            classify(Key::from_static("sys/trust/rekey")),
            SysClass::Never
        );
        assert_eq!(classify(keys::INFO), SysClass::Plain);
        assert_eq!(classify(keys::LOG), SysClass::Plain);
        assert!(Updaters::new(["demo-caller"]).is_none());
        assert!(Updaters::new(["spiffe://"]).is_none());
        let nine: Vec<String> = (0..9).map(|i| format!("spiffe://td/u{i}")).collect();
        assert!(Updaters::new(nine).is_none());
        assert!(Updaters::none().is_empty());
    }

    #[test]
    fn rg20_only_updaters_pass_and_disabled_denies_all() {
        for op in ALL {
            for caller in [
                Caller::default(),
                as_(STRANGER, "x-1"),
                Caller {
                    principal: None,
                    request_id: Some("x-1"),
                },
            ] {
                let (g, r) = admit(claimed(), caller, op, b"");
                assert_eq!(r, Err(Refusal::NotUpdater), "{op:?}");
                assert_eq!(g.counters().denied, 1);
            }
            let (_, r) = admit(UpdateGate::disabled(), as_(UPD, "x-1"), op, b"");
            assert_eq!(r, Err(Refusal::NotUpdater), "{op:?} disabled");
        }
        assert_eq!(Refusal::NotUpdater.error(), HandlerError::Denied);
    }

    #[test]
    fn rg20_read_only_requests_need_no_claim() {
        let unclaimed = UpdateGate::new(Updaters::new([UPD]).unwrap(), Some(ControlCounter(0)));
        for op in [
            UpdateOp::TrustInfo,
            UpdateOp::PayloadStatus,
            UpdateOp::FwInfo,
        ] {
            let (_, r) = admit(
                unclaimed.clone(),
                Caller {
                    principal: Some(UPD),
                    request_id: None,
                },
                op,
                b"",
            );
            assert_eq!(forwarded(&r).map(|(b, _)| b), Some(&b""[..]), "{op:?}");
        }
        // Mutations need a claimed device, and a request id first.
        for op in ALL.into_iter().filter(|o| !o.read_only()) {
            let (_, r) = admit(unclaimed.clone(), as_(UPD, "u-1"), op, b"");
            assert_eq!(r, Err(Refusal::Unclaimed), "{op:?}");
            let (_, r) = admit(
                claimed(),
                Caller {
                    principal: Some(UPD),
                    request_id: None,
                },
                op,
                b"",
            );
            assert_eq!(r, Err(Refusal::NoRequestId), "{op:?}");
        }
        let info = trust_info(TrustState::Unclaimed);
        assert_eq!(
            unclaimed.clone().observe_trust(Ok(&info)).trust(),
            DeviceTrust::Unclaimed
        );
        assert_eq!(
            claimed().observe_trust(Err(HandlerError::NotFound)).trust(),
            DeviceTrust::Unknown
        );
        // A forwarded trust/info answer updates the facts.
        let (g, r) = admit(
            unclaimed,
            Caller {
                principal: Some(UPD),
                request_id: None,
            },
            UpdateOp::TrustInfo,
            b"",
        );
        let (_, t) = forwarded(&r).unwrap();
        let claimed_info = trust_info(TrustState::Claimed {
            key: pinned(),
            min_version: Version(0),
        });
        let g = g.complete(t, Ok(&claimed_info), 1_000);
        assert_eq!(g.trust(), DeviceTrust::Claimed(pinned()));
    }

    #[test]
    fn rg20_one_transfer_owned_by_its_caller() {
        let (g, r) = admit(claimed(), as_(UPD, "a-1"), UpdateOp::LoaderBegin, b"begin");
        let (_, t) = forwarded(&r).unwrap();
        let g = g.complete(t, Ok(b""), 1_000);
        assert_eq!(g.transfer().map(|t| t.owner.as_str()), Some(UPD));
        // Another updater cannot begin, chunk or commit, nor abort or
        // reboot the firmware meanwhile; the transfer is untouched.
        for (op, body) in [
            (UpdateOp::LoaderBegin, &b"begin"[..]),
            (UpdateOp::FwBegin, b"begin"),
            (UpdateOp::FwAbort, b""),
            (UpdateOp::FwReboot, b""),
            (UpdateOp::FwConfirm, b""),
        ] {
            let (g2, r) = admit(g.clone(), as_(UPD2, "b-1"), op, body);
            assert_eq!(r, Err(Refusal::Busy), "{op:?}");
            assert_eq!(g2.transfer(), g.transfer());
        }
        for (op, body) in [
            (UpdateOp::LoaderChunk, &b"\0\0\0\0x"[..]),
            (UpdateOp::LoaderCommit, &signed_commit()[..]),
            (UpdateOp::LoaderCommit, b""),
        ] {
            let (g2, r) = admit(g.clone(), as_(UPD2, "b-2"), op, body);
            assert_eq!(r, Err(Refusal::NotOwner), "{op:?}");
            assert_eq!(
                g2.transfer(),
                g.transfer(),
                "a stray never ends the owner's transfer"
            );
        }
        // The owner's firmware chunk is not part of a payload transfer.
        let (_, r) = admit(g.clone(), as_(UPD, "a-x"), UpdateOp::FwChunk, b"\0\0\0\0x");
        assert_eq!(r, Err(Refusal::NotOwner));
        // The owner continues and commits; the transfer ends with the answer.
        let (g, r) = admit(g, as_(UPD, "a-2"), UpdateOp::LoaderChunk, b"\0\0\0\0x");
        let (_, t) = forwarded(&r).unwrap();
        let g = g.complete(t, Ok(b""), 2_000);
        let (g, r) = admit(g, as_(UPD, "a-3"), UpdateOp::LoaderCommit, &signed_commit());
        let (body, t) = forwarded(&r).unwrap();
        assert_eq!(
            body,
            &signed_commit()[..],
            "the device verifies the signature"
        );
        let g = g.complete(t, Err(HandlerError::Denied), 3_000);
        assert_eq!(g.transfer(), None);
        // A failed begin ends its transfer too.
        let (g, r) = admit(g, as_(UPD2, "b-3"), UpdateOp::FwBegin, b"x");
        let (_, t) = forwarded(&r).unwrap();
        let g = g.complete(t, Err(HandlerError::BufferTooSmall), 3_000);
        assert_eq!(g.transfer(), None);
        assert_eq!(Refusal::Busy.error(), HandlerError::InvalidRequest);
        assert_eq!(Refusal::NotOwner.error(), HandlerError::InvalidRequest);
    }

    #[test]
    fn rg20_an_idle_transfer_is_taken_over() {
        let (g, r) = claimed().admit(
            as_(UPD, "a-1"),
            UpdateOp::LoaderBegin,
            keys::BEGIN,
            b"b",
            Some(NODE),
            0,
        );
        let (_, t) = forwarded(&r).unwrap();
        let g = g.complete(t, Ok(b""), 0);
        let late = TRANSFER_IDLE_MS;
        let (g, r) = g.admit(
            as_(UPD2, "b-1"),
            UpdateOp::LoaderBegin,
            keys::BEGIN,
            b"b",
            Some(NODE),
            late - 1,
        );
        assert_eq!(r, Err(Refusal::Busy));
        let (g, r) = g.admit(
            as_(UPD2, "b-2"),
            UpdateOp::LoaderBegin,
            keys::BEGIN,
            b"b",
            Some(NODE),
            late,
        );
        assert!(forwarded(&r).is_some());
        assert_eq!(g.transfer().map(|t| t.owner.as_str()), Some(UPD2));
        // The old owner's chunk is refused.
        let (_, r) = g.admit(
            as_(UPD, "a-2"),
            UpdateOp::LoaderChunk,
            keys::CHUNK,
            b"\0\0\0\0x",
            Some(NODE),
            late,
        );
        assert_eq!(r, Err(Refusal::NotOwner));
    }

    #[test]
    fn rg20_commits_must_be_signed() {
        for (begin, commit, good) in [
            (
                UpdateOp::LoaderBegin,
                UpdateOp::LoaderCommit,
                signed_commit(),
            ),
            (
                UpdateOp::FwBegin,
                UpdateOp::FwCommit,
                vec![9u8; SIGNATURE_LEN],
            ),
        ] {
            let (g, r) = admit(claimed(), as_(UPD, "a-1"), begin, b"b");
            let g = g.complete(forwarded(&r).unwrap().1, Ok(b"\x01"), 1_000);
            for bad in [&b""[..], &[0u8; 10][..]] {
                let (g2, r) = admit(g.clone(), as_(UPD, "a-2"), commit, bad);
                assert_eq!(r, Err(Refusal::UnsignedCommit), "{commit:?} {bad:?}");
                assert_eq!(g2.transfer(), None, "the owner's unsigned commit ends it");
                assert_eq!(g2.counters().denied, 1);
            }
            let (_, r) = admit(g, as_(UPD, "a-3"), commit, &good);
            assert_eq!(forwarded(&r).map(|(b, _)| b.to_vec()), Some(good));
        }
    }

    #[test]
    fn rg20_start_and_stop_need_a_verified_signed_control() {
        let target = control_target(NODE);
        let g = claimed();
        // Unsigned (the serial form) or malformed: Denied.
        for body in [&b""[..], &[1u8; 9][..]] {
            let (_, r) = admit(g.clone(), as_(UPD, "c-1"), UpdateOp::Start, body);
            assert_eq!(r, Err(Refusal::UnsignedControl));
        }
        // Wrong op, wrong device, another key, an old counter: Denied.
        let other_key = {
            let k = SigningKey::from_bytes(&[8u8; 32]);
            let m = control_message(ControlOp::Start, &target, ControlCounter(FLOOR + 5));
            SignedControl {
                counter: ControlCounter(FLOOR + 5),
                signature: Signature(k.sign(&m).to_bytes()),
            }
            .encode()
            .to_vec()
        };
        for (op, body) in [
            (UpdateOp::Start, control(ControlOp::Stop, FLOOR + 5, target)),
            (
                UpdateOp::Start,
                control(ControlOp::Start, FLOOR + 5, [2, 0, 0, 0, 0, 9]),
            ),
            (UpdateOp::Start, other_key),
            (UpdateOp::Start, control(ControlOp::Start, FLOOR, target)),
            (UpdateOp::Stop, control(ControlOp::Stop, FLOOR - 1, target)),
        ] {
            let (_, r) = admit(g.clone(), as_(UPD, "c-2"), op, &body);
            assert_eq!(r, Err(Refusal::ControlRefused), "{op:?}");
        }
        // A good one goes out with an empty body; its counter is used up.
        let start = control(ControlOp::Start, FLOOR + 5, target);
        let (g, r) = admit(g, as_(UPD, "c-3"), UpdateOp::Start, &start);
        let (body, t) = forwarded(&r).unwrap();
        assert!(body.is_empty(), "serial start takes an empty body");
        let g = g.complete(t, Ok(b""), 1_000);
        assert_eq!(
            t.persist(),
            Some(ControlCounter(FLOOR + 5)),
            "persisted before forwarding"
        );
        assert_eq!(g.control_counter(), Some(ControlCounter(FLOOR + 5)));
        let (g, r) = admit(g, as_(UPD, "c-4"), UpdateOp::Start, &start);
        assert_eq!(
            r,
            Err(Refusal::ControlRefused),
            "a captured start cannot be replayed"
        );
        let (_, r) = admit(
            g.clone(),
            as_(UPD, "c-5"),
            UpdateOp::Stop,
            &control(ControlOp::Stop, FLOOR + 6, target),
        );
        assert!(forwarded(&r).is_some());
        // Without a bound device there is no target.
        let (_, r) = g.admit(
            as_(UPD, "c-6"),
            UpdateOp::Stop,
            keys::STOP,
            &control(ControlOp::Stop, FLOOR + 7, target),
            None,
            0,
        );
        assert_eq!(r, Err(Refusal::Unbound));
    }

    #[test]
    fn rg20_a_p256_pinned_key_is_verified_as_the_device_does() {
        use mwctl::sign::{P256Key, Signer as _, sign_control};
        // TEST KEY ONLY: the OpenSSL vector's scalar.
        let key = P256Key::from_scalar(&[0x20; 32]).unwrap();
        let state = TrustState::Claimed {
            key: key.public_key(),
            min_version: Version(0),
        };
        let g = UpdateGate::new(Updaters::new([UPD]).unwrap(), Some(ControlCounter(FLOOR)))
            .observe_trust(Ok(&trust_info(state)));
        assert_eq!(g.trust(), DeviceTrust::Claimed(key.public_key()));
        let target = control_target(NODE);
        let p256 = |op: ControlOp, counter: u64, target: [u8; 6]| {
            sign_control(&key, op, &target, ControlCounter(counter))
                .unwrap()
                .encode()
                .to_vec()
        };
        let start = p256(ControlOp::Start, FLOOR + 5, target);
        let mut forged = start.clone();
        forged[20] ^= 1;
        let refused = [
            forged,
            // Another P-256 key, the Ed25519 test key, the wrong op or
            // device, an old counter.
            sign_control(
                &P256Key::from_scalar(&[0x21; 32]).unwrap(),
                ControlOp::Start,
                &target,
                ControlCounter(FLOOR + 5),
            )
            .unwrap()
            .encode()
            .to_vec(),
            control(ControlOp::Start, FLOOR + 5, target),
            p256(ControlOp::Stop, FLOOR + 5, target),
            p256(ControlOp::Start, FLOOR + 5, [2, 0, 0, 0, 0, 9]),
            p256(ControlOp::Start, FLOOR, target),
        ];
        for body in &refused {
            let (_, r) = admit(g.clone(), as_(UPD, "p-1"), UpdateOp::Start, body);
            assert_eq!(r, Err(Refusal::ControlRefused));
            // The device's own rule refuses the same request.
            let auth = SignedControl::parse_body(body).unwrap().unwrap();
            assert!(
                state
                    .accept_control(ControlCounter(FLOOR), ControlOp::Start, &target, &auth)
                    .is_err()
            );
        }
        let (g, r) = admit(g, as_(UPD, "p-2"), UpdateOp::Start, &start);
        let (body, t) = forwarded(&r).expect("a signed P-256 start passes");
        assert!(body.is_empty());
        assert_eq!(t.persist(), Some(ControlCounter(FLOOR + 5)));
        let g = g.complete(t, Ok(b""), 1_000);
        let auth = SignedControl::parse_body(&start).unwrap().unwrap();
        assert_eq!(
            state.accept_control(ControlCounter(FLOOR), ControlOp::Start, &target, &auth),
            Ok(ControlCounter(FLOOR + 5))
        );
        let (_, r) = admit(g, as_(UPD, "p-3"), UpdateOp::Start, &start);
        assert_eq!(r, Err(Refusal::ControlRefused), "used up");
    }

    #[test]
    fn rg21_resends_are_answered_once_and_oversized_bodies_refused() {
        let (g, r) = admit(claimed(), as_(UPD, "a-1"), UpdateOp::LoaderBegin, b"b");
        let t = forwarded(&r).unwrap().1.clone();
        // While at the device, a resend is Unavailable (retriable).
        let (g, r) = admit(g, as_(UPD, "a-1"), UpdateOp::LoaderBegin, b"b");
        assert_eq!(r, Err(Refusal::InFlight));
        assert_eq!(Refusal::InFlight.error(), HandlerError::Unavailable);
        // Answered (even after the caller gave up): the resend is replayed.
        let g = g.complete(&t, Ok(b""), 1_000);
        let (g, r) = admit(g, as_(UPD, "a-1"), UpdateOp::LoaderBegin, b"b");
        assert_eq!(r, Ok(Admission::Replay(Ok(Vec::new()))));
        let (g, r) = admit(g, as_(UPD, "a-1"), UpdateOp::LoaderBegin, b"other");
        assert_eq!(r, Err(Refusal::ReusedId));
        // Never answered: the resend goes out again.
        let (g, r) = admit(g, as_(UPD, "a-2"), UpdateOp::LoaderChunk, b"\0\0\0\0x");
        let t = forwarded(&r).unwrap().1.clone();
        let g = g.abandon(&t);
        let (g, r) = admit(g, as_(UPD, "a-2"), UpdateOp::LoaderChunk, b"\0\0\0\0x");
        assert!(forwarded(&r).is_some());
        let c = g.counters();
        assert_eq!((c.replayed, c.abandoned, c.forwarded), (1, 1, 3));
        // A body that would not fit one serial frame never goes out.
        let big = vec![0u8; MAX_RAW_FRAME];
        let (_, r) = admit(g, as_(UPD, "a-3"), UpdateOp::LoaderChunk, &big);
        assert_eq!(r, Err(Refusal::TooLarge));
        // A full 512-byte chunk fits.
        let mut chunk = vec![0u8; 4 + middleware::loader::CHUNK_MAX];
        chunk[0] = 1;
        let (g, r) = admit(claimed(), as_(UPD, "z-1"), UpdateOp::FwBegin, b"b");
        let g = g.complete(forwarded(&r).unwrap().1, Ok(b"\x01"), 1_000);
        let (_, r) = admit(g, as_(UPD, "z-2"), UpdateOp::FwChunk, &chunk);
        assert!(forwarded(&r).is_some());
    }

    #[test]
    fn rg27_without_a_floor_start_and_stop_fail_closed_and_uploads_stay() {
        let target = control_target(NODE);
        let info = trust_info(TrustState::Claimed {
            key: pinned(),
            min_version: Version(0),
        });
        let g = UpdateGate::new(Updaters::new([UPD]).unwrap(), None).observe_trust(Ok(&info));
        for (op, ctl) in [
            (UpdateOp::Start, ControlOp::Start),
            (UpdateOp::Stop, ControlOp::Stop),
        ] {
            let (g2, r) = admit(
                g.clone(),
                as_(UPD, "n-1"),
                op,
                &control(ctl, FLOOR + 5, target),
            );
            assert_eq!(r, Err(Refusal::NoControlFloor), "{op:?}");
            assert_eq!(Refusal::NoControlFloor.error(), HandlerError::Unavailable);
            assert_eq!(g2.counters().refused, 1);
            assert_eq!(g2.control_counter(), None, "never falls back to 0");
        }
        // Uploads rely on the device's own version check.
        let (_, r) = admit(g, as_(UPD, "n-2"), UpdateOp::LoaderBegin, b"b");
        assert!(forwarded(&r).is_some());
    }

    #[test]
    fn rg27_an_unpersisted_counter_is_rolled_back_and_nothing_forwarded() {
        let target = control_target(NODE);
        let start = control(ControlOp::Start, FLOOR + 5, target);
        let (g, r) = admit(claimed(), as_(UPD, "p-1"), UpdateOp::Start, &start);
        let t = forwarded(&r).unwrap().1.clone();
        assert_eq!(t.persist(), Some(ControlCounter(FLOOR + 5)));
        // The shell's write failed: back to the old floor, counted.
        let g = g.unpersisted(&t);
        assert_eq!(g.control_counter(), Some(ControlCounter(FLOOR)));
        assert_eq!(g.counters().unpersisted, 1);
        // The resend is decided afresh (and may succeed once the disk does).
        let (g, r) = admit(g, as_(UPD, "p-1"), UpdateOp::Start, &start);
        assert!(forwarded(&r).is_some());
        assert_eq!(g.control_counter(), Some(ControlCounter(FLOOR + 5)));
        // Reads and uploads carry nothing to persist.
        let (_, r) = admit(g, as_(UPD, "p-2"), UpdateOp::LoaderBegin, b"b");
        assert_eq!(forwarded(&r).unwrap().1.persist(), None);
    }

    #[test]
    fn rg20_another_device_forgets_trust_and_transfer() {
        let (g, r) = admit(claimed(), as_(UPD, "a-1"), UpdateOp::LoaderBegin, b"b");
        let g = g.complete(forwarded(&r).unwrap().1, Ok(b""), 1_000);
        let g = g.forget_device();
        assert_eq!((g.trust(), g.transfer()), (DeviceTrust::Unknown, None));
        let (_, r) = admit(g, as_(UPD, "a-2"), UpdateOp::LoaderChunk, b"\0\0\0\0x");
        assert_eq!(r, Err(Refusal::Unclaimed));
    }
}
