//! Over-the-air payload updates: which requests a radio peer may make, and
//! which requester owns a loader transfer.
//!
//! The radio shell (`src/shell/radio.rs`, feature `espnow`) answers
//! ESP-NOW requests (`middleware::rpc` frames addressed to this station)
//! by handing them to the controller like serial requests, tagged with
//! [`Origin::Air`] and the requester's MAC. Before dispatch the controller
//! asks [`admit`]. ESP-NOW has no authentication and its source MAC is
//! spoofable, so over the air nothing that changes what runs is accepted
//! without a signature by the pinned key:
//!
//! | request over the air        | unclaimed | claimed                         |
//! |-----------------------------|-----------|---------------------------------|
//! | `sys/info`, `sys/trust/info` | `Denied` | allowed (read only)             |
//! | `sys/loader/begin`          | `Denied`  | allowed: stages into the inactive slot beside the running payload (`Context::staging`), taking the transfer over |
//! | `sys/loader/chunk`          | `Denied`  | the transfer's owner only, else `InvalidRequest` ([`Refusal::NotOwner`]) |
//! | `sys/loader/commit`         | `Denied`  | owner checked first ([`Refusal::NotOwner`], loader untouched); then signed only (the dispatcher applies `TrustState::accept_commit`: pinned key, version above the last); the owner's unsigned commit is `Denied` and ends the transfer ([`Refusal::abandons_transfer`]) |
//! | `sys/payload/start`, `stop` | `Denied`  | a [`SignedControl`] body only: `TrustState::accept_control` (pinned key, this device's MAC, this command, counter above the last); the counter is persisted, then the request is dispatched with an empty body ([`Admission::Control`]). Unsigned or refused: `Denied` |
//! | `sys/trust/claim`           | `Denied`  | `Denied` (never over the air)   |
//! | any other key               | `Denied`  | `Denied`                        |
//!
//! Serial requests keep their rules: [`admit`] only applies the transfer
//! ownership check to them (physical access is the authority there).
//!
//! An unclaimed device refuses the air entirely, `sys/info` included:
//! trust on first use is for the serial port, where someone holds the
//! board, and an unclaimed device has nothing to verify a signature with.
//! Without the `trust` feature the dispatcher's trust state is always
//! Unclaimed, so such a build refuses every request over the air.
//!
//! # What a radio attacker can and cannot do
//!
//! Without the key: read `sys/info` and `sys/trust/info`; send a `begin`
//! (which takes the transfer over and erases the *inactive* slot) or, by
//! spoofing the owner's MAC, chunks that spoil the staged image, whose
//! commit then fails its CRC or signature check. That is a denial of
//! service of the *update* only: the active image and the running payload
//! are never touched until a commit signed by the pinned key with a fresh
//! version activates the staged image. Starting, stopping and installing
//! need the key; a captured signed request cannot be replayed (counter or
//! version already used), redirected to another device (MAC in the
//! signed control message; the image hash in a signed commit) or turned
//! into another command.
//!
//! # One loader, two kinds of requester
//!
//! Serial and radio share the dispatcher's single loader. The requester
//! whose `sys/loader/begin` started the transfer in progress owns it
//! ([`owner_after`]); a `begin` from anyone restarts the loader and takes
//! the transfer over, so a serial `begin` aborts an over-the-air upload and
//! the other way round. A `chunk` or `commit` from anyone else is refused
//! with `InvalidRequest` ([`Refusal::NotOwner`]) *without* touching the
//! loader, so the abandoned requester's late chunks cannot break the new
//! transfer; this is checked before the signature rule, so a non-owner's
//! unsigned commit cannot end the owner's transfer either. Both serial
//! ports count as one requester ([`Origin::Serial`]); radio requesters
//! are told apart by MAC.

use middleware::Key;
use middleware::loader::keys;
use middleware::loader::trust::{ControlCounter, ControlOp, SignedCommit, SignedControl, TrustState};
use middleware::transport::HandlerError;

/// A 48-bit ESP-NOW (Wi-Fi MAC) address.
pub type Mac = [u8; 6];

/// Where a request came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// A serial port (UART0 or USB-Serial-JTAG).
    Serial,
    /// An ESP-NOW peer with this MAC.
    Air(Mac),
}

/// The system requests a radio peer may make on a claimed device.
pub const AIR_KEYS: [Key<'static>; 7] = [
    keys::INFO,
    keys::TRUST_INFO,
    keys::BEGIN,
    keys::CHUNK,
    keys::COMMIT,
    keys::START,
    keys::STOP,
];

/// What the device knows when it decides: its root of trust, the last
/// accepted control counter and its own station MAC (the target a signed
/// control request must name).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Facts {
    /// The persisted root of trust.
    pub trust: TrustState,
    /// The last accepted control counter.
    pub control: ControlCounter,
    /// This device's ESP-NOW (station) MAC.
    pub own: Mac,
}

/// How an admitted request proceeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Dispatch it as received.
    Dispatch,
    /// A signed `start`/`stop` was verified: persist `counter` (and adopt
    /// it), then dispatch the key with an empty body. If persisting fails,
    /// answer `Internal` and do nothing.
    Control {
        /// The counter to persist before acting.
        counter: ControlCounter,
    },
}

/// Why a request is refused before dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Over the air to a device with no pinned key.
    Unclaimed,
    /// A key that never travels over the air (`sys/trust/claim`, anything
    /// not in [`AIR_KEYS`]).
    NotOverAir,
    /// An unsigned (or malformed) commit over the air from the transfer's
    /// owner, or with no transfer in progress.
    UnsignedCommit,
    /// An unsigned (or malformed) start or stop over the air.
    UnsignedControl,
    /// A signed start or stop that does not verify: another key, another
    /// device, another command, or a counter not above the last.
    ControlRefused,
    /// A chunk or commit from a requester that does not own the transfer.
    NotOwner,
}

impl Refusal {
    /// The status sent back.
    pub const fn error(self) -> HandlerError {
        match self {
            Refusal::NotOwner => HandlerError::InvalidRequest,
            Refusal::Unclaimed
            | Refusal::NotOverAir
            | Refusal::UnsignedCommit
            | Refusal::UnsignedControl
            | Refusal::ControlRefused => HandlerError::Denied,
        }
    }

    /// Whether the refusal ends the transfer in progress, as a commit the
    /// dispatcher refuses does: the owner's unsigned commit over the air is
    /// the end of that upload ([`admit`] checks ownership first, so it
    /// never comes from anyone else). The other refusals leave the loader
    /// untouched (a stray from a non-owner must not break the owner's
    /// transfer).
    pub const fn abandons_transfer(self) -> bool {
        matches!(self, Refusal::UnsignedCommit)
    }

    /// A short reason for the log.
    pub const fn reason(self) -> &'static str {
        match self {
            Refusal::Unclaimed => "device unclaimed; nothing is accepted over the air",
            Refusal::NotOverAir => "not allowed over the air",
            Refusal::UnsignedCommit => "commit over the air must be signed",
            Refusal::UnsignedControl => "start/stop over the air must be signed by the pinned key",
            Refusal::ControlRefused => {
                "control signature invalid (pinned key, this device, this command) or counter not above the last"
            }
            Refusal::NotOwner => "another requester owns the transfer",
        }
    }
}

/// Whether `key` continues a transfer (only its owner may send it).
fn continues_transfer(key: Key<'_>) -> bool {
    key == keys::CHUNK || key == keys::COMMIT
}

/// The control command a key names, if any.
fn control_op(key: Key<'_>) -> Option<ControlOp> {
    if key == keys::START {
        Some(ControlOp::Start)
    } else if key == keys::STOP {
        Some(ControlOp::Stop)
    } else {
        None
    }
}

/// Verifies a signed control body (feature `trust`).
#[cfg(feature = "trust")]
fn check_control(facts: &Facts, op: ControlOp, body: &[u8]) -> Result<Admission, Refusal> {
    let auth = match SignedControl::parse_body(body) {
        Ok(Some(auth)) => auth,
        _ => return Err(Refusal::UnsignedControl),
    };
    facts
        .trust
        .accept_control(facts.control, op, &facts.own, &auth)
        .map(|counter| Admission::Control { counter })
        .map_err(|_| Refusal::ControlRefused)
}

/// Without `trust` nothing can be verified (and the device is never
/// claimed, so this is unreachable through [`admit`]).
#[cfg(not(feature = "trust"))]
fn check_control(_facts: &Facts, _op: ControlOp, body: &[u8]) -> Result<Admission, Refusal> {
    match SignedControl::parse_body(body) {
        Ok(Some(_)) => Err(Refusal::ControlRefused),
        _ => Err(Refusal::UnsignedControl),
    }
}

/// Decides whether a request on `key` with `body` from `origin` may be
/// dispatched, given what the device knows and the owner of the transfer
/// in progress (`None` when the loader is idle). The checks run in this
/// order: over the air, claimed and an air key; for everyone, ownership of
/// a `chunk` or `commit`; over the air, a signed commit, and a verified
/// signed `start`/`stop`. See the module docs.
pub fn admit(
    facts: &Facts,
    owner: Option<Origin>,
    origin: Origin,
    key: Key<'_>,
    body: &[u8],
) -> Result<Admission, Refusal> {
    let air = matches!(origin, Origin::Air(_));
    if air {
        if !facts.trust.is_claimed() {
            return Err(Refusal::Unclaimed);
        }
        if !AIR_KEYS.contains(&key) {
            return Err(Refusal::NotOverAir);
        }
    }
    if let Some(o) = owner
        && o != origin
        && continues_transfer(key)
    {
        return Err(Refusal::NotOwner);
    }
    if !air {
        return Ok(Admission::Dispatch);
    }
    if key == keys::COMMIT && !matches!(SignedCommit::parse_body(body), Ok(Some(_))) {
        return Err(Refusal::UnsignedCommit);
    }
    match control_op(key) {
        Some(op) => check_control(facts, op, body),
        None => Ok(Admission::Dispatch),
    }
}

/// The transfer's owner after a request on `key` from `origin` was
/// dispatched and the loader is (`receiving`) or is not mid-transfer: the
/// requester of a `begin` that started one, unchanged while it continues,
/// nobody once it ends.
pub fn owner_after(owner: Option<Origin>, origin: Origin, key: Key<'_>, receiving: bool) -> Option<Origin> {
    match (receiving, key == keys::BEGIN) {
        (false, _) => None,
        (true, true) => Some(origin),
        (true, false) => owner,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::loader::trust::{PublicKey, SIGNED_COMMIT_LEN, SIGNED_CONTROL_LEN, Version};

    const HOST: Origin = Origin::Air([0x5c, 0xcf, 0x7f, 0x72, 0xf6, 0x54]);
    const OTHER: Origin = Origin::Air([2, 0, 0, 0, 0, 9]);
    const ME: Mac = [0xe4, 0xb0, 0x63, 0x81, 0xd3, 0x24];

    fn facts(trust: TrustState) -> Facts {
        Facts { trust, control: ControlCounter(0), own: ME }
    }

    fn claimed() -> Facts {
        facts(TrustState::Claimed { key: PublicKey([7; 32]), min_version: Version(10) })
    }

    fn k(s: &str) -> Key<'_> {
        Key::new(s).expect("valid")
    }

    const DISPATCH: Result<Admission, Refusal> = Ok(Admission::Dispatch);

    #[test]
    fn o6_an_unclaimed_device_refuses_the_air_entirely() {
        let f = facts(TrustState::Unclaimed);
        for key in AIR_KEYS.iter().chain(&[keys::TRUST_CLAIM, k("app/x")]) {
            assert_eq!(admit(&f, None, HOST, *key, &[]), Err(Refusal::Unclaimed), "{key:?}");
            assert_eq!(admit(&f, None, Origin::Serial, *key, &[]), DISPATCH, "serial: {key:?}");
        }
        assert_eq!(Refusal::Unclaimed.error(), HandlerError::Denied);
    }

    #[test]
    fn o6_a_claimed_device_allows_only_the_update_keys_over_the_air() {
        let f = claimed();
        let signed = [0u8; SIGNED_COMMIT_LEN];
        for key in [keys::INFO, keys::TRUST_INFO, keys::BEGIN, keys::CHUNK, keys::COMMIT] {
            let body: &[u8] = if key == keys::COMMIT { &signed } else { &[] };
            assert_eq!(admit(&f, None, HOST, key, body), DISPATCH, "{key:?}");
        }
        for key in [keys::TRUST_CLAIM, keys::LOG, keys::STATUS, k("sys/other"), k("app/x"), k("sys")] {
            assert_eq!(admit(&f, None, HOST, key, &[7; 32]), Err(Refusal::NotOverAir), "{key:?}");
        }
        assert_eq!(admit(&f, None, Origin::Serial, keys::TRUST_CLAIM, &[7; 32]), DISPATCH, "serial may claim");
        assert_eq!(Refusal::NotOverAir.error(), HandlerError::Denied);
    }

    #[test]
    fn o6_a_commit_over_the_air_must_be_signed() {
        let f = claimed();
        assert_eq!(admit(&f, Some(HOST), HOST, keys::COMMIT, &[]), Err(Refusal::UnsignedCommit));
        assert_eq!(admit(&f, Some(HOST), HOST, keys::COMMIT, &[0; 3]), Err(Refusal::UnsignedCommit));
        assert_eq!(admit(&f, None, HOST, keys::COMMIT, &[]), Err(Refusal::UnsignedCommit));
        assert_eq!(admit(&f, Some(HOST), HOST, keys::COMMIT, &[0; SIGNED_COMMIT_LEN]), DISPATCH);
        // Serial keeps the dispatcher's rules (an unclaimed device accepts
        // an unsigned commit there).
        let unclaimed = facts(TrustState::Unclaimed);
        assert_eq!(admit(&unclaimed, Some(Origin::Serial), Origin::Serial, keys::COMMIT, &[]), DISPATCH);
        assert_eq!(admit(&f, Some(Origin::Serial), Origin::Serial, keys::COMMIT, &[]), DISPATCH);
        assert_eq!(Refusal::UnsignedCommit.error(), HandlerError::Denied);
        assert!(Refusal::UnsignedCommit.reason().contains("signed"));
        assert!(Refusal::UnsignedCommit.abandons_transfer());
        for r in [
            Refusal::NotOwner,
            Refusal::Unclaimed,
            Refusal::NotOverAir,
            Refusal::UnsignedControl,
            Refusal::ControlRefused,
        ] {
            assert!(!r.abandons_transfer(), "{r:?}");
        }
    }

    #[test]
    fn o15_ownership_is_checked_before_the_signature_rule() {
        let f = claimed();
        // A non-owner's unsigned commit is NotOwner, which leaves the
        // owner's transfer alone; only the owner's own unsigned commit
        // ends it.
        let r = admit(&f, Some(HOST), OTHER, keys::COMMIT, &[]);
        assert_eq!(r, Err(Refusal::NotOwner));
        assert!(!Refusal::NotOwner.abandons_transfer());
        assert_eq!(admit(&f, Some(Origin::Serial), HOST, keys::COMMIT, &[]), Err(Refusal::NotOwner));
        assert_eq!(admit(&f, Some(HOST), HOST, keys::COMMIT, &[]), Err(Refusal::UnsignedCommit));
    }

    #[test]
    fn o16_start_and_stop_over_the_air_need_a_signed_body() {
        let f = claimed();
        for key in [keys::START, keys::STOP] {
            assert_eq!(admit(&f, None, HOST, key, &[]), Err(Refusal::UnsignedControl), "{key:?}");
            assert_eq!(admit(&f, None, HOST, key, &[1; 5]), Err(Refusal::UnsignedControl), "{key:?}");
            // Right length, no valid signature.
            assert_eq!(admit(&f, None, HOST, key, &[0; SIGNED_CONTROL_LEN]), Err(Refusal::ControlRefused));
            // Serial is unchanged: no signature needed.
            assert_eq!(admit(&f, None, Origin::Serial, key, &[]), DISPATCH, "{key:?}");
        }
        assert_eq!(Refusal::UnsignedControl.error(), HandlerError::Denied);
        assert_eq!(Refusal::ControlRefused.error(), HandlerError::Denied);
    }

    #[cfg(feature = "trust")]
    #[test]
    fn o16_a_signed_control_names_this_device_this_command_and_a_new_counter() {
        use ed25519_dalek::{Signer, SigningKey};
        use middleware::loader::trust::{Signature, control_message};
        let s = SigningKey::from_bytes(&[3; 32]);
        let key = PublicKey(s.verifying_key().to_bytes());
        let f = Facts {
            trust: TrustState::Claimed { key, min_version: Version(1) },
            control: ControlCounter(100),
            own: ME,
        };
        let body = |op, target: &Mac, n: u64| {
            let m = control_message(op, target, ControlCounter(n));
            SignedControl { counter: ControlCounter(n), signature: Signature(s.sign(&m).to_bytes()) }.encode()
        };
        assert_eq!(
            admit(&f, None, HOST, keys::STOP, &body(ControlOp::Stop, &ME, 101)),
            Ok(Admission::Control { counter: ControlCounter(101) })
        );
        assert_eq!(
            admit(&f, Some(HOST), OTHER, keys::START, &body(ControlOp::Start, &ME, 5000)),
            Ok(Admission::Control { counter: ControlCounter(5000) }),
            "any peer holding a signature may send it; the owner rule is for transfers"
        );
        let refused = Err(Refusal::ControlRefused);
        assert_eq!(admit(&f, None, HOST, keys::STOP, &body(ControlOp::Stop, &ME, 100)), refused, "replay");
        assert_eq!(admit(&f, None, HOST, keys::STOP, &body(ControlOp::Stop, &ME, 7)), refused, "older");
        assert_eq!(admit(&f, None, HOST, keys::START, &body(ControlOp::Stop, &ME, 101)), refused, "other command");
        let elsewhere = [0xe4, 0xb0, 0x63, 0x81, 0xd3, 0x25];
        assert_eq!(admit(&f, None, HOST, keys::STOP, &body(ControlOp::Stop, &elsewhere, 101)), refused, "other device");
        let mallory = SigningKey::from_bytes(&[4; 32]);
        let m = control_message(ControlOp::Stop, &ME, ControlCounter(101));
        let forged = SignedControl { counter: ControlCounter(101), signature: Signature(mallory.sign(&m).to_bytes()) };
        assert_eq!(admit(&f, None, HOST, keys::STOP, &forged.encode()), refused, "other key");
    }

    #[test]
    fn o7_only_the_owner_continues_a_transfer() {
        let f = claimed();
        let signed = [0u8; SIGNED_COMMIT_LEN];
        assert_eq!(admit(&f, Some(HOST), OTHER, keys::CHUNK, &[0; 8]), Err(Refusal::NotOwner));
        assert_eq!(admit(&f, Some(HOST), Origin::Serial, keys::CHUNK, &[0; 8]), Err(Refusal::NotOwner));
        assert_eq!(admit(&f, Some(Origin::Serial), HOST, keys::COMMIT, &signed), Err(Refusal::NotOwner));
        assert_eq!(admit(&f, Some(HOST), HOST, keys::CHUNK, &[0; 8]), DISPATCH);
        // Anyone may begin (taking the transfer over) or ask for info.
        assert_eq!(admit(&f, Some(HOST), Origin::Serial, keys::BEGIN, &[0; 8]), DISPATCH);
        assert_eq!(admit(&f, Some(Origin::Serial), HOST, keys::BEGIN, &[0; 8]), DISPATCH);
        assert_eq!(admit(&f, Some(HOST), OTHER, keys::INFO, &[]), DISPATCH);
        // With no transfer in progress the loader itself refuses strays.
        assert_eq!(admit(&f, None, OTHER, keys::CHUNK, &[0; 8]), DISPATCH);
        assert_eq!(Refusal::NotOwner.error(), HandlerError::InvalidRequest);
    }

    #[test]
    fn o7_begin_takes_the_transfer_and_its_end_frees_it() {
        assert_eq!(owner_after(None, HOST, keys::BEGIN, true), Some(HOST));
        assert_eq!(owner_after(Some(HOST), Origin::Serial, keys::BEGIN, true), Some(Origin::Serial));
        assert_eq!(owner_after(Some(HOST), HOST, keys::CHUNK, true), Some(HOST));
        assert_eq!(owner_after(Some(HOST), OTHER, keys::INFO, true), Some(HOST));
        assert_eq!(owner_after(Some(HOST), HOST, keys::COMMIT, false), None, "committed or failed");
        assert_eq!(owner_after(None, HOST, keys::BEGIN, false), None, "refused begin");
    }

    /// The controller's per-request sequence over the real dispatcher,
    /// with storage actions assumed to succeed.
    #[cfg(feature = "trust")]
    mod session {
        use super::*;
        use crate::dispatch::{Action, Context, State, handle_request};
        use ed25519_dalek::{Signer, SigningKey};
        use middleware::crc::Crc32;
        use middleware::loader::sha256::ImageHash;
        use middleware::loader::trust::{Signature, control_message, signed_message};
        use middleware::loader::{Loader, PayloadStatus};

        const CTX: Context<'static> = Context {
            chip: "esp32s3",
            firmware: "0.1.0",
            capacity: 4096,
            runtime: true,
            rekey_window: false,
            staging: false,
        };

        struct Dev {
            state: State,
            owner: Option<Origin>,
            /// Whether a payload is running in the shell's session.
            running: bool,
        }

        impl Dev {
            fn claimed() -> Self {
                let key = PublicKey(SigningKey::from_bytes(&[1; 32]).verifying_key().to_bytes());
                let trust = TrustState::Claimed { key, min_version: Version(0) };
                Dev { state: State::boot(false).with_trust(trust), owner: None, running: false }
            }

            /// The controller's `handle`: admit, persist a control counter,
            /// dispatch (staging over the air), track the owner and whether
            /// the payload runs.
            fn request(&mut self, origin: Origin, key: Key<'_>, body: &[u8]) -> Result<(), HandlerError> {
                let facts = Facts { trust: self.state.trust(), control: self.state.control(), own: ME };
                let body = match admit(&facts, self.owner, origin, key, body) {
                    Err(r) => {
                        if r.abandons_transfer() {
                            self.state = self.state.storage_failed();
                            self.owner = None;
                        }
                        return Err(r.error());
                    }
                    Ok(Admission::Control { counter }) => {
                        self.state = self.state.with_control(counter);
                        &[][..]
                    }
                    Ok(Admission::Dispatch) => body,
                };
                let ctx = Context { staging: matches!(origin, Origin::Air(_)), ..CTX };
                let mut out = [0u8; 128];
                let o = handle_request(self.state, &ctx, key, body, &mut out);
                self.state = o.state;
                if o.action.stops_payload() {
                    self.running = false;
                }
                if o.action == Action::Start {
                    self.state = self.state.started();
                    self.running = true;
                }
                let receiving = matches!(self.state.loader(), Loader::Receiving { .. });
                self.owner = owner_after(self.owner, origin, key, receiving);
                o.reply.map(drop)
            }

            fn begin(&mut self, origin: Origin, image: &[u8]) -> Result<(), HandlerError> {
                let mut b = [0u8; 8];
                b[..4].copy_from_slice(&(image.len() as u32).to_le_bytes());
                b[4..].copy_from_slice(&Crc32::checksum(image).to_le_bytes());
                self.request(origin, keys::BEGIN, &b)
            }

            fn chunk(&mut self, origin: Origin, image: &[u8], i: usize) -> Result<(), HandlerError> {
                let part = image.chunks(40).nth(i).expect("chunk");
                let mut b = [0u8; 44];
                b[..4].copy_from_slice(&((i * 40) as u32).to_le_bytes());
                b[4..4 + part.len()].copy_from_slice(part);
                self.request(origin, keys::CHUNK, &b[..4 + part.len()])
            }

            fn commit(&mut self, origin: Origin, image: &[u8], version: u32) -> Result<(), HandlerError> {
                let m = signed_message(image.len() as u32, Version(version), &ImageHash::digest(image));
                let signature = Signature(SigningKey::from_bytes(&[1; 32]).sign(&m).to_bytes());
                self.request(origin, keys::COMMIT, &SignedCommit { version: Version(version), signature }.encode())
            }

            fn control(&mut self, origin: Origin, op: ControlOp, counter: u64) -> Result<(), HandlerError> {
                let m = control_message(op, &ME, ControlCounter(counter));
                let signature = Signature(SigningKey::from_bytes(&[1; 32]).sign(&m).to_bytes());
                let body = SignedControl { counter: ControlCounter(counter), signature }.encode();
                let key = if op == ControlOp::Start { keys::START } else { keys::STOP };
                self.request(origin, key, &body)
            }

            /// Installs and starts `image` over serial.
            fn install_running(&mut self, image: &[u8], version: u32) {
                self.begin(Origin::Serial, image).unwrap();
                for i in 0..image.len().div_ceil(40) {
                    self.chunk(Origin::Serial, image, i).unwrap();
                }
                self.commit(Origin::Serial, image, version).unwrap();
                self.request(Origin::Serial, keys::START, &[]).unwrap();
                assert!(self.running);
            }
        }

        fn image(seed: u8) -> [u8; 100] {
            core::array::from_fn(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        }

        #[test]
        fn o7_a_serial_begin_aborts_an_air_transfer() {
            let (a, b) = (image(1), image(2));
            let mut d = Dev::claimed();
            d.begin(HOST, &a).unwrap();
            d.chunk(HOST, &a, 0).unwrap();
            d.begin(Origin::Serial, &b).unwrap();
            assert_eq!(d.owner, Some(Origin::Serial));
            // The abandoned air upload's late chunk and commit do not touch
            // the serial transfer.
            assert_eq!(d.chunk(HOST, &a, 1), Err(HandlerError::InvalidRequest));
            assert_eq!(d.commit(HOST, &a, 5), Err(HandlerError::InvalidRequest));
            assert_eq!(d.state.status(), PayloadStatus::Loading);
            for i in 0..3 {
                d.chunk(Origin::Serial, &b, i).unwrap();
            }
            d.commit(Origin::Serial, &b, 5).unwrap();
            assert_eq!((d.state.status(), d.owner), (PayloadStatus::Stopped, None));
        }

        #[test]
        fn o7_an_air_begin_aborts_a_serial_transfer_and_completes_signed() {
            let (a, b) = (image(3), image(4));
            let mut d = Dev::claimed();
            d.begin(Origin::Serial, &a).unwrap();
            d.chunk(Origin::Serial, &a, 0).unwrap();
            d.begin(HOST, &b).unwrap();
            assert_eq!(d.chunk(Origin::Serial, &a, 1), Err(HandlerError::InvalidRequest));
            assert_eq!(d.chunk(OTHER, &b, 0), Err(HandlerError::InvalidRequest), "another radio peer");
            for i in 0..3 {
                d.chunk(HOST, &b, i).unwrap();
            }
            assert_eq!(d.request(HOST, keys::COMMIT, &[]), Err(HandlerError::Denied), "unsigned over the air");
            // ...which ends the owner's transfer, like a refused commit on
            // serial.
            assert_eq!((d.state.status(), d.owner), (PayloadStatus::Empty, None));
            assert_eq!(d.chunk(HOST, &b, 0), Err(HandlerError::InvalidRequest), "no transfer any more");
            d.begin(HOST, &b).unwrap();
            for i in 0..3 {
                d.chunk(HOST, &b, i).unwrap();
            }
            d.commit(HOST, &b, 7).unwrap();
            assert_eq!(d.state.status(), PayloadStatus::Stopped);
            assert!(matches!(d.state.trust(), TrustState::Claimed { min_version: Version(7), .. }));
            // Replaying the same signed upload is a stale version: Denied.
            d.begin(HOST, &b).unwrap();
            for i in 0..3 {
                d.chunk(HOST, &b, i).unwrap();
            }
            assert_eq!(d.commit(HOST, &b, 7), Err(HandlerError::Denied));
            assert_eq!(d.state.status(), PayloadStatus::Stopped, "the installed image stays");
            assert_eq!(d.request(HOST, keys::TRUST_CLAIM, &[9; 32]), Err(HandlerError::Denied));
            assert_eq!(d.request(HOST, keys::START, &[]), Err(HandlerError::Denied), "unsigned start");
            d.control(HOST, ControlOp::Start, 1).unwrap();
            assert_eq!(d.state.status(), PayloadStatus::Running);
        }

        #[test]
        fn o14_an_air_transfer_never_disturbs_the_running_payload_before_a_signed_commit() {
            let (a, b) = (image(5), image(6));
            let mut d = Dev::claimed();
            d.install_running(&a, 3);
            // Begin and chunks from the air, never committed.
            d.begin(OTHER, &b).unwrap();
            for i in 0..2 {
                d.chunk(OTHER, &b, i).unwrap();
            }
            assert!(d.running);
            assert_eq!(d.state.status(), PayloadStatus::Running);
            // Spoofed or bad commits: still running.
            assert_eq!(d.request(OTHER, keys::COMMIT, &[]), Err(HandlerError::Denied));
            assert!(d.running && d.state.status() == PayloadStatus::Running);
            d.begin(OTHER, &b).unwrap();
            for i in 0..3 {
                d.chunk(OTHER, &b, i).unwrap();
            }
            assert_eq!(d.commit(OTHER, &b, 3), Err(HandlerError::Denied), "stale version");
            assert!(d.running && d.state.status() == PayloadStatus::Running);
            // Unsigned stop and start: Denied, still running.
            assert_eq!(d.request(OTHER, keys::STOP, &[]), Err(HandlerError::Denied));
            assert!(d.running);
            // The signed commit replaces it: the old payload stops.
            d.begin(HOST, &b).unwrap();
            for i in 0..3 {
                d.chunk(HOST, &b, i).unwrap();
                assert!(d.running, "chunk {i}");
            }
            d.commit(HOST, &b, 4).unwrap();
            assert!(!d.running);
            assert_eq!(d.state.status(), PayloadStatus::Stopped);
        }

        #[test]
        fn o15_a_non_owners_unsigned_commit_leaves_the_owners_transfer_to_complete() {
            let b = image(7);
            let mut d = Dev::claimed();
            d.begin(HOST, &b).unwrap();
            d.chunk(HOST, &b, 0).unwrap();
            assert_eq!(d.request(OTHER, keys::COMMIT, &[]), Err(HandlerError::InvalidRequest), "NotOwner");
            assert_eq!(d.owner, Some(HOST));
            assert!(matches!(d.state.loader(), Loader::Receiving { received: 40, .. }));
            for i in 1..3 {
                d.chunk(HOST, &b, i).unwrap();
            }
            d.commit(HOST, &b, 9).unwrap();
            assert_eq!((d.state.status(), d.owner), (PayloadStatus::Stopped, None));
        }

        #[test]
        fn o16_signed_start_and_stop_act_once_and_replays_are_denied() {
            let a = image(8);
            let mut d = Dev::claimed();
            d.install_running(&a, 2);
            d.control(HOST, ControlOp::Stop, 1000).unwrap();
            assert_eq!((d.running, d.state.control()), (false, ControlCounter(1000)));
            assert_eq!(d.control(HOST, ControlOp::Start, 1000), Err(HandlerError::Denied), "same counter");
            assert_eq!(d.control(OTHER, ControlOp::Stop, 999), Err(HandlerError::Denied), "older");
            assert!(!d.running);
            d.control(OTHER, ControlOp::Start, 1001).unwrap();
            assert!(d.running);
            assert_eq!(d.control(HOST, ControlOp::Stop, 1001), Err(HandlerError::Denied), "replayed counter");
            assert!(d.running);
            // Serial needs no signature and leaves the counter alone.
            d.request(Origin::Serial, keys::STOP, &[]).unwrap();
            assert_eq!((d.running, d.state.control()), (false, ControlCounter(1001)));
        }
    }
}
