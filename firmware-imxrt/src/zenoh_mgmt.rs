//! The gate for management requests over Zenoh (ZN4, ZN5): which `sys/...`
//! queries a Zenoh peer may make, decided before
//! [`Service::handle`](crate::service::Service::handle)
//! is called, so a refused request never reaches the
//! service and leaves the board unchanged.
//!
//! The board's `sys/...` keys are queryable as per-node keys,
//! `@mw/node/<zid16>/sys/...` ([`management_key`]). Anyone on the network
//! can send a query, and Zenoh (client mode, no TLS) authenticates nobody,
//! so the rules are those of a radio peer on the ESP base firmware
//! (`esp_base::ota`), whose checks this gate reuses: nothing that changes
//! what runs is accepted without a signature by the pinned key. One
//! difference: an unclaimed board still answers its two identity queries,
//! so a Zenoh network can find it and see that it is unclaimed.
//!
//! | request over Zenoh                   | unclaimed | claimed |
//! |--------------------------------------|-----------|---------|
//! | `sys/info`, `sys/trust/info`         | answered  | answered |
//! | `sys/board`, `sys/manifest`, `sys/payload/binding`, `sys/fw/info` | `Denied`  | answered (read only) |
//! | `sys/payload/bind`                   | `Denied`  | signed only ([`Refusal::UnsignedBinding`] otherwise); the dispatcher's `TrustState::accept_binding` checks the pinned key and that the version is above the stored binding's for that image |
//! | `sys/loader/begin`, `chunk`          | `Denied`  | staged into the inactive payload slot beside the running payload (the service dispatches Zenoh requests with `Context::staging`); a `chunk` from a requester that does not own the transfer is `InvalidRequest` ([`Refusal::NotOwner`]) |
//! | `sys/loader/commit`                  | `Denied`  | the owner's, and signed ([`SignedCommit`](middleware::loader::trust::SignedCommit): the dispatcher's `TrustState::accept_commit`, pinned key, version above the last); the owner's unsigned commit is `Denied` and ends the transfer ([`Refusal::abandons_transfer`]) |
//! | `sys/fw/begin`, `chunk`, `commit`    | `Denied`  | to the A/B updater: the transfer belongs to the requester whose `begin` started it (the service refuses another's `chunk`/`commit` with `InvalidRequest`), and `commit` needs the firmware signature by the pinned key (`middleware::fw_ota`: image, version, slot and node bound) |
//! | `sys/payload/start`, `stop`, `sys/fw/reboot`, `confirm`, `abort` | `Denied` | a [`SignedControl`](middleware::loader::trust::SignedControl) body only (`TrustState::accept_control`: the pinned key, this board's [`Facts::own`] address, this command, a counter above the last): [`Admission::Control`], dispatched with an empty body once the counter is persisted. Unsigned or refused: `Denied` |
//! | anything else under `sys/`           | `Denied`  | `Denied` |
//!
//! Every refusal but [`Refusal::NotOwner`] is [`HandlerError::Denied`]
//! ([`Refusal::error`]): the request was well formed but policy refuses
//! it, as over the air.
//!
//! What a network attacker without the key can do is what a radio
//! attacker can on the ESP: read the identity, send a `begin` (payload or
//! firmware) that takes the transfer over and erases the *inactive* slot
//! (discarding an update in progress, or one committed and awaiting its
//! reboot), or chunks that spoil a transfer it started itself. That denies
//! an *update*, never what runs: activating a payload, installing,
//! booting, confirming or abandoning firmware, starting and stopping all
//! need the key, and a captured signed request cannot be replayed (version
//! or counter used), redirected (the board's address or node id is signed)
//! or turned into another command (the op or the image is signed).
//!
//! What stays off the network:
//!
//! * `sys/trust/claim`: pinning a key needs the serial port, where someone
//!   holds the board.
//! * `sys/zenoh/endpoint`, `sys/link/*` (the serial links' filters,
//!   [`crate::links`]) and every other system key.
//!
//! A board whose trust record could not be read reports Unclaimed in its
//! dispatcher state, so it answers only the two identity queries (fail
//! closed).
//!
//! # Use
//!
//! ```text
//! let key = management_key(query_expr, svc.node())?;           // else: not ours
//! svc.set_origin(Origin::Air(enet_mac));
//! match admit(&facts(svc.state(), enet_mac), svc.loader_owner(), key, body) {
//!     Err(r)                          => if r.abandons_transfer() { svc.abandon_transfer() }
//!                                        reply r.error(), log r.reason()
//!     Ok(Admission::Dispatch)         => svc.handle(key, body, out)
//!     Ok(Admission::Control { counter }) => persist counter with the trust
//!                                        record (Internal and stop if that
//!                                        fails), then svc.handle(key, &[], out)
//! }
//! ```

use esp_base::dispatch::State;
use esp_base::ota::{self, Mac, Origin};
use middleware::Key;
use middleware::fw_ota::keys as fw_keys;
use middleware::loader::binding::BindRequest;
use middleware::loader::trust::{ControlOp, SignedControl};
use middleware::loader::{is_system_key, keys};
use middleware::transport::{HandlerError, SenderId};
use middleware::zenoh_wire::parse_node_key;

pub use esp_base::ota::{Admission, Facts};

/// The requests an unclaimed board answers over Zenoh.
pub const UNCLAIMED_KEYS: [Key<'static>; 2] = [keys::INFO, keys::TRUST_INFO];

/// The read-only requests a claimed board answers over Zenoh.
pub const READ_ONLY_KEYS: [Key<'static>; 6] =
    [keys::INFO, keys::TRUST_INFO, keys::BOARD, keys::MANIFEST, keys::BINDING, fw_keys::INFO];

/// The payload transfer requests a claimed board accepts over Zenoh
/// (staged; the commit signed).
pub const LOADER_KEYS: [Key<'static>; 3] = [keys::BEGIN, keys::CHUNK, keys::COMMIT];

/// The firmware transfer requests a claimed board accepts over Zenoh (the
/// commit signed, checked by the updater).
pub const FW_TRANSFER_KEYS: [Key<'static>; 3] = [fw_keys::BEGIN, fw_keys::CHUNK, fw_keys::COMMIT];

/// The requests a claimed board accepts over Zenoh with a signed control
/// body by the pinned key ([`Admission::Control`]).
pub const SIGNED_KEYS: [Key<'static>; 5] = [
    keys::START,
    keys::STOP,
    fw_keys::REBOOT,
    fw_keys::CONFIRM,
    fw_keys::ABORT,
];

/// What the gate needs to know: the dispatcher's trust state and last
/// accepted control counter ([`Service::state`](crate::service::Service::state)),
/// and `own`, the 6-byte address a signed control request must name: the
/// board's Ethernet MAC address.
pub const fn facts(state: State, own: Mac) -> Facts {
    Facts {
        trust: state.trust(),
        control: state.control(),
        own,
    }
}

/// The origin the service gives Zenoh requests: every Zenoh peer is one
/// requester, named by the board's own Ethernet MAC.
pub const fn origin(own: Mac) -> Origin {
    Origin::Air(own)
}

/// The system key a Zenoh query addresses to this node: `expr` is
/// `@mw/node/<zid16>/sys/...` with `node`'s id. `None` for another node's
/// key, a key outside `sys/`, or anything that is not a per-node key.
pub fn management_key(expr: &str, node: SenderId) -> Option<Key<'_>> {
    let (id, key) = parse_node_key(expr)?;
    (id == node && is_system_key(key)).then_some(key)
}

/// Why a management request over Zenoh is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The board has no pinned key: only [`UNCLAIMED_KEYS`] are answered.
    Unclaimed,
    /// A request that is never accepted over Zenoh (see the module docs).
    NotOverZenoh,
    /// A control request (start, stop, firmware reboot/confirm/abort)
    /// without a well-formed signed body.
    UnsignedControl,
    /// A signed control request that does not verify: another key, another
    /// board, another command, or a counter not above the last.
    ControlRefused,
    /// The payload transfer owner's commit over Zenoh is unsigned (or
    /// malformed); it ends the transfer.
    UnsignedCommit,
    /// A payload `chunk` or `commit` while another requester (a serial
    /// link) owns the transfer.
    NotOwner,
    /// A `sys/payload/bind` without a signature (or malformed); the
    /// dispatcher then checks the signature itself (BD2).
    UnsignedBinding,
}

impl Refusal {
    /// The status sent back: `InvalidRequest` for [`Refusal::NotOwner`]
    /// (as over the air), else `Denied`.
    pub const fn error(self) -> HandlerError {
        match self {
            Refusal::NotOwner => HandlerError::InvalidRequest,
            _ => HandlerError::Denied,
        }
    }

    /// Whether the shell must end the payload transfer
    /// ([`Service::abandon_transfer`](crate::service::Service::abandon_transfer)):
    /// the owner's unsigned commit, as `esp_base::ota`.
    pub const fn abandons_transfer(self) -> bool {
        matches!(self, Refusal::UnsignedCommit)
    }

    /// A short reason for the log.
    pub const fn reason(self) -> &'static str {
        match self {
            Refusal::Unclaimed => "board unclaimed; only sys/info and sys/trust/info over Zenoh",
            Refusal::NotOverZenoh => "not allowed over Zenoh (use a serial link)",
            Refusal::UnsignedControl => {
                "start/stop and fw reboot/confirm/abort over Zenoh must be signed by the pinned key"
            }
            Refusal::ControlRefused => {
                "control signature invalid (pinned key, this board, this command) or counter not above the last"
            }
            Refusal::UnsignedCommit => "payload commit over Zenoh must be signed; transfer ended",
            Refusal::NotOwner => "another requester owns the transfer",
            Refusal::UnsignedBinding => "a payload binding over Zenoh must be signed by the pinned key",
        }
    }
}

/// Decides a management request on `key` with `body` over Zenoh, before
/// the service sees it; `owner` is the payload transfer's owner
/// ([`Service::loader_owner`](crate::service::Service::loader_owner)). See
/// the module docs.
pub fn admit(
    facts: &Facts,
    owner: Option<Origin>,
    key: Key<'_>,
    body: &[u8],
) -> Result<Admission, Refusal> {
    if UNCLAIMED_KEYS.contains(&key) {
        return Ok(Admission::Dispatch);
    }
    if !facts.trust.is_claimed() {
        return Err(Refusal::Unclaimed);
    }
    if READ_ONLY_KEYS.contains(&key) || FW_TRANSFER_KEYS.contains(&key) {
        return Ok(Admission::Dispatch);
    }
    if key == keys::BIND {
        // Signed only; the dispatcher verifies the signature and version
        // (it dispatches Zenoh requests with `Context::staging`).
        return match BindRequest::parse(body) {
            Some(BindRequest { auth: Some(_), .. }) => Ok(Admission::Dispatch),
            _ => Err(Refusal::UnsignedBinding),
        };
    }
    if LOADER_KEYS.contains(&key) || key == keys::START || key == keys::STOP {
        // The radio gate's own checks: claimed, an air key, transfer
        // ownership, a signed commit, a verified signed start/stop.
        return match ota::admit(facts, owner, origin(facts.own), key, body) {
            Ok(admission) => Ok(admission),
            Err(ota::Refusal::UnsignedControl) => Err(Refusal::UnsignedControl),
            Err(ota::Refusal::ControlRefused) => Err(Refusal::ControlRefused),
            Err(ota::Refusal::UnsignedCommit) => Err(Refusal::UnsignedCommit),
            Err(ota::Refusal::NotOwner) => Err(Refusal::NotOwner),
            Err(ota::Refusal::Unclaimed | ota::Refusal::NotOverAir) => Err(Refusal::NotOverZenoh),
        };
    }
    match ControlOp::for_key(key.as_str()) {
        Some(op) => check_fw_control(facts, op, body),
        None => Err(Refusal::NotOverZenoh),
    }
}

/// A signed `sys/fw/reboot|confirm|abort`.
fn check_fw_control(facts: &Facts, op: ControlOp, body: &[u8]) -> Result<Admission, Refusal> {
    let Ok(Some(auth)) = SignedControl::parse_body(body) else {
        return Err(Refusal::UnsignedControl);
    };
    facts
        .trust
        .accept_control(facts.control, op, &facts.own, &auth)
        .map(|counter| Admission::Control { counter })
        .map_err(|_| Refusal::ControlRefused)
}

/// A payload subscription key, as the Zenoh link declares it.
pub type SubKey = heapless::String<{ middleware::key::MAX_KEY_LEN }>;
/// The payload's subscriptions (at most `MAX_SUBSCRIPTIONS`).
pub type SubSet = heapless::Vec<SubKey, { middleware::wasm_abi::MAX_SUBSCRIPTIONS }>;

/// The set the Zenoh link declares subscribers for, from the payload's
/// subscription keys: valid non-`sys` keys, each once, in order (a `sys/`
/// publication from the network never reaches a payload, so declaring it
/// would only draw traffic).
pub fn subscription_set<'a>(keys: impl Iterator<Item = &'a str>) -> SubSet {
    let mut set = SubSet::new();
    for k in keys {
        let Ok(key) = Key::new(k) else { continue };
        if is_system_key(key) || set.iter().any(|s| s.as_str() == k) {
            continue;
        }
        let Ok(s) = SubKey::try_from(k) else { continue };
        let _ = set.push(s);
    }
    set
}

/// What changes when the declared set `old` becomes `new`: the keys to
/// undeclare (in `old` only) and to declare (in `new` only).
pub fn subscription_diff(old: &SubSet, new: &SubSet) -> (SubSet, SubSet) {
    let missing = |from: &SubSet, other: &SubSet| {
        from.iter()
            .filter(|k| !other.contains(k))
            .cloned()
            .collect::<SubSet>()
    };
    (missing(old, new), missing(new, old))
}

#[cfg(test)]
mod tests {
    //! ZN4 and ZN5 against the real request service on the RAM HyperFlash
    //! model.

    extern crate std;

    use super::*;
    use crate::ab::Handoff;
    use crate::ab::test_flash::BlockFlash;
    use crate::layout::{FlashMap, RT1050_EVKB};
    use crate::service::{Facts as SvcFacts, Reply, Service};
    use ed25519_dalek::{Signer, SigningKey};
    use esp_base::ota::Origin;
    use esp_base::store::StoreError;
    use middleware::crc::Crc32;
    use middleware::fw_ota::Slot;
    use middleware::loader::PayloadStatus;
    use middleware::loader::sha256::ImageHash;
    use middleware::loader::trust::{
        ControlCounter, ControlOp, PublicKey, Signature, SignedCommit, SignedControl, TrustCode,
        TrustState, Version, control_message, signed_message,
    };
    use middleware::zenoh_wire::node_key_buf;
    use std::vec::Vec as StdVec;

    const MAP: FlashMap = RT1050_EVKB;
    const NODE: SenderId = SenderId([0x02, 0x10, 0x50, 0x0B, 0x83, 0x1F, 0x2A, 0x0B]);
    const MAC: Mac = [0x02, 0x10, 0x50, 0x0b, 0x83, 0x1f];
    const FACTS: SvcFacts = SvcFacts {
        chip: "rt1050",
        firmware: "0.1.0",
        runtime: true,
        node: NODE,
        max_image: 64 * 1024,
    };
    const FACTORY: Option<Handoff> = Some(Handoff {
        slot: Slot::A,
        trial: false,
    });

    type Svc = Service<BlockFlash>;

    fn open() -> Svc {
        let flash = BlockFlash::erased(MAP.slot_b.end() as usize);
        Service::open(flash, &MAP, FACTS, FACTORY).unwrap().0
    }

    fn k(s: &str) -> Key<'_> {
        Key::new(s).expect("valid key")
    }

    fn signer(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn public(seed: u8) -> PublicKey {
        PublicKey::Ed25519(signer(seed).verifying_key().to_bytes())
    }

    fn control(seed: u8, op: ControlOp, target: &Mac, counter: u64) -> StdVec<u8> {
        let m = control_message(op, target, ControlCounter(counter));
        let signature = Signature(signer(seed).sign(&m).to_bytes());
        SignedControl {
            counter: ControlCounter(counter),
            signature,
        }
        .encode()
        .to_vec()
    }

    fn image() -> StdVec<u8> {
        (0..600u32).map(|i| (i as u8).wrapping_mul(13)).collect()
    }

    fn begin_body(img: &[u8]) -> StdVec<u8> {
        let mut b = (img.len() as u32).to_le_bytes().to_vec();
        b.extend_from_slice(&Crc32::checksum(img).to_le_bytes());
        b
    }

    fn signed_commit(seed: u8, img: &[u8], version: u32) -> StdVec<u8> {
        let m = signed_message(img.len() as u32, Version(version), &ImageHash::digest(img));
        SignedCommit {
            version: Version(version),
            signature: Signature(signer(seed).sign(&m).to_bytes()),
        }
        .encode()
        .to_vec()
    }

    type Answer = Result<StdVec<u8>, HandlerError>;

    fn dispatch(s: &mut Svc, key: Key<'_>, body: &[u8]) -> (Answer, crate::service::Handled) {
        let mut out = [0u8; 128];
        let h = s.handle(key, body, &mut out);
        let a = match h.reply {
            Reply::Now(r) => r.map(|n| out.get(..n).unwrap_or(&[]).to_vec()),
            Reply::Start => Ok(StdVec::new()),
        };
        (a, h)
    }

    /// A request over a serial link: straight to the service.
    fn serial(s: &mut Svc, key: Key<'_>, body: &[u8]) -> Answer {
        s.set_origin(Origin::Serial);
        dispatch(s, key, body).0
    }

    /// A request over Zenoh, as the shell handles it: the gate first, the
    /// service only if admitted (a verified control request has its
    /// counter persisted by `Service::accept_control`, then is dispatched
    /// with an empty body; `Internal` if the counter could not be saved),
    /// with the Zenoh origin. Also returns what the service asked of the
    /// shell (a reset).
    fn zenoh_h(
        s: &mut Svc,
        key: Key<'_>,
        body: &[u8],
    ) -> (Answer, Option<crate::service::Handled>) {
        s.set_origin(origin(MAC));
        match admit(&facts(s.state(), MAC), s.loader_owner(), key, body) {
            Err(r) => {
                if r.abandons_transfer() {
                    s.abandon_transfer();
                }
                (Err(r.error()), None)
            }
            Ok(Admission::Dispatch) => {
                let (a, h) = dispatch(s, key, body);
                (a, Some(h))
            }
            Ok(Admission::Control { counter }) => match s.accept_control(counter) {
                Ok(()) => {
                    let (a, h) = dispatch(s, key, &[]);
                    (a, Some(h))
                }
                Err(_) => (Err(HandlerError::Internal), None),
            },
        }
    }

    fn zenoh(s: &mut Svc, key: Key<'_>, body: &[u8]) -> Answer {
        zenoh_h(s, key, body).0
    }

    /// The service reopened on a copy of its flash (a reboot).
    fn reboot(s: &mut Svc) -> Svc {
        let flash = s.store_mut().flash().raw().clone();
        Service::open(flash, &MAP, FACTS, FACTORY).unwrap().0
    }

    /// What a refused request must not change: the dispatcher state, the
    /// flash (every erase and write is logged), the installed image and
    /// the A/B updater's view.
    fn snapshot(s: &mut Svc) -> (State, usize, Option<u32>, Result<StdVec<u8>, HandlerError>) {
        let ops = s.store_mut().flash().raw().log.len();
        let installed = s.store_mut().header().map(|h| h.len);
        let fw = serial(s, fw_keys::INFO, &[]);
        (s.state(), ops, installed, fw)
    }

    /// Requests that would change the board, with well-formed bodies.
    fn changing(img: &[u8]) -> StdVec<(Key<'static>, StdVec<u8>)> {
        let mut chunk = 0u32.to_le_bytes().to_vec();
        chunk.extend_from_slice(img.get(..48).unwrap_or(&[]));
        let mut fw_begin = 4096u32.to_le_bytes().to_vec();
        fw_begin.extend_from_slice(&0x0002_0000u32.to_le_bytes());
        fw_begin.extend_from_slice(&[0; 32]);
        std::vec![
            (keys::BEGIN, begin_body(img)),
            (keys::CHUNK, chunk),
            (keys::COMMIT, signed_commit(1, img, 9)),
            (keys::COMMIT, StdVec::new()),
            (keys::TRUST_CLAIM, public(2).as_bytes().to_vec()),
            (keys::STOP, StdVec::new()),
            (keys::START, StdVec::new()),
            (fw_keys::BEGIN, fw_begin),
            (fw_keys::CHUNK, StdVec::new()),
            (fw_keys::COMMIT, [0u8; 64].to_vec()),
            (fw_keys::REBOOT, StdVec::new()),
            (fw_keys::CONFIRM, StdVec::new()),
            (fw_keys::ABORT, StdVec::new()),
            (crate::links::keys::SUB, b"**".to_vec()),
            (k("sys/other"), StdVec::new()),
        ]
    }

    #[test]
    fn zn4_an_unclaimed_board_answers_only_info_and_trust_info() {
        let mut s = open();
        let img = image();
        // The same bytes as over serial.
        for key in UNCLAIMED_KEYS {
            assert_eq!(zenoh(&mut s, key, &[]), serial(&mut s, key, &[]), "{key:?}");
            assert!(zenoh(&mut s, key, &[]).is_ok());
        }
        let before = snapshot(&mut s);
        let mut refused = changing(&img);
        refused.push((keys::BOARD, StdVec::new()));
        refused.push((keys::MANIFEST, StdVec::new()));
        refused.push((fw_keys::INFO, StdVec::new()));
        refused.push((keys::STOP, control(1, ControlOp::Stop, &MAC, 1)));
        for (key, body) in &refused {
            assert_eq!(
                admit(&facts(s.state(), MAC), None, *key, body),
                Err(Refusal::Unclaimed),
                "{key:?}"
            );
            assert_eq!(
                zenoh(&mut s, *key, body),
                Err(HandlerError::Denied),
                "{key:?}"
            );
            assert_eq!(snapshot(&mut s), before, "{key:?} changed the board");
        }
        // In particular nobody claimed it over the network.
        assert_eq!(s.state().trust(), TrustState::Unclaimed);
        let info = zenoh(&mut s, keys::TRUST_INFO, &[]).unwrap();
        assert_eq!(info.first(), Some(&(TrustCode::Unclaimed as u8)));
        // The serial port still has the authority (trust on first use).
        assert_eq!(
            serial(&mut s, keys::TRUST_CLAIM, public(1).as_bytes()),
            Ok(StdVec::new())
        );
        assert!(s.state().trust().is_claimed());
    }

    /// A `sys/payload/bind` body for [`image`] at `version`, signed by
    /// key `seed` (unsigned without one), and the record alone.
    fn bind_body(seed: Option<u8>, version: u32) -> (StdVec<u8>, StdVec<u8>) {
        use middleware::loader::binding::{BINDING_MAX, Binding, SignedBinding, Target, binding_message};
        let sha = ImageHash::digest(&image());
        let mut b = Binding::new(sha, Version(version));
        b.bind("cmd", Target::Key(Key::from_static("app/cmd"))).unwrap();
        let mut out = [0u8; BINDING_MAX];
        let n = b.encode(&mut out).unwrap();
        let record = out[..n].to_vec();
        let mut body = record.clone();
        if let Some(seed) = seed {
            let m = binding_message(&sha, Version(version), &ImageHash::digest(&record));
            let auth = SignedBinding {
                version: Version(version),
                signature: Signature(signer(seed).sign(&m).to_bytes()),
            };
            body.extend_from_slice(&auth.encode());
        }
        (body, record)
    }

    #[test]
    fn bd2_over_zenoh_a_binding_must_be_signed_and_survives_a_reboot() {
        let mut s = claimed_running();
        let (unsigned, _) = bind_body(None, 1);
        assert_eq!(zenoh(&mut s, keys::BIND, &unsigned), Err(HandlerError::Denied));
        assert_eq!(
            admit(&facts(s.state(), MAC), None, keys::BIND, &unsigned),
            Err(Refusal::UnsignedBinding)
        );
        let (mallory, _) = bind_body(Some(2), 1);
        assert_eq!(zenoh(&mut s, keys::BIND, &mallory), Err(HandlerError::Denied));
        assert_eq!(zenoh(&mut s, keys::BINDING, &[]), Err(HandlerError::NotFound));
        let (signed, record) = bind_body(Some(1), 1);
        assert_eq!(zenoh(&mut s, keys::BIND, &signed), Ok(StdVec::new()));
        assert_eq!(zenoh(&mut s, keys::BINDING, &[]), Ok(record.clone()));
        // A replay, then a newer version.
        assert_eq!(zenoh(&mut s, keys::BIND, &signed), Err(HandlerError::Denied));
        let (v2, record2) = bind_body(Some(1), 2);
        assert_eq!(zenoh(&mut s, keys::BIND, &v2), Ok(StdVec::new()));
        let mut r = reboot(&mut s);
        assert_eq!(serial(&mut r, keys::BINDING, &[]), Ok(record2));
        assert_eq!(r.state().binding().map(|m| m.version), Some(Version(2)));
        assert_eq!(zenoh(&mut r, keys::BIND, &v2), Err(HandlerError::Denied));
        // Over serial an unsigned binding with a higher version is taken.
        let (v3, record3) = bind_body(None, 3);
        assert_eq!(serial(&mut r, keys::BIND, &v3), Ok(StdVec::new()));
        let mut out = [0u8; 128];
        let n = r.binding_record(&mut out).unwrap();
        assert_eq!(&out[..n], &record3[..]);
        // An unclaimed board takes nothing over Zenoh.
        let mut u = open();
        assert_eq!(zenoh(&mut u, keys::BIND, &signed), Err(HandlerError::Denied));
    }

    /// A claimed board (key 1) with a signed payload installed, running.
    fn claimed_running() -> Svc {
        let mut s = open();
        let img = image();
        assert_eq!(
            serial(&mut s, keys::TRUST_CLAIM, public(1).as_bytes()),
            Ok(StdVec::new())
        );
        assert_eq!(
            serial(&mut s, keys::BEGIN, &begin_body(&img)),
            Ok(StdVec::new())
        );
        for (i, part) in img.chunks(48).enumerate() {
            let mut c = (i as u32 * 48).to_le_bytes().to_vec();
            c.extend_from_slice(part);
            assert_eq!(serial(&mut s, keys::CHUNK, &c), Ok(StdVec::new()));
        }
        assert_eq!(
            serial(&mut s, keys::COMMIT, &signed_commit(1, &img, 5)),
            Ok(StdVec::new())
        );
        assert_eq!(serial(&mut s, keys::START, &[]), Ok(StdVec::new()));
        s.started();
        assert_eq!(s.status(), PayloadStatus::Running);
        s
    }

    #[test]
    fn zn4_a_claimed_board_answers_read_only_requests() {
        let mut s = claimed_running();
        let before = snapshot(&mut s);
        for key in READ_ONLY_KEYS {
            assert_eq!(
                admit(&facts(s.state(), MAC), None, key, &[]),
                Ok(Admission::Dispatch),
                "{key:?}"
            );
            assert_eq!(zenoh(&mut s, key, &[]), serial(&mut s, key, &[]), "{key:?}");
        }
        assert_eq!(snapshot(&mut s), before);
    }

    #[test]
    fn zn4_a_refused_request_leaves_a_claimed_board_unchanged() {
        let mut s = claimed_running();
        let img = image();
        let before = snapshot(&mut s);
        let other_board: Mac = [2, 0, 0, 0, 0, 9];
        // What the gate refuses on a claimed board: everything that would
        // change it without a valid signature (the transfers it admits are
        // ZN5's tests).
        let mut refused: StdVec<(Key<'static>, StdVec<u8>)> = changing(&img)
            .into_iter()
            .filter(|(k, b)| {
                !(*k == keys::BEGIN
                    || *k == keys::CHUNK
                    || (*k == keys::COMMIT && !b.is_empty())
                    || FW_TRANSFER_KEYS.contains(k))
            })
            .collect();
        refused.push((keys::COMMIT, signed_commit(1, &img, 9)[..20].to_vec()));
        // Signed, but by another key, for another board, for the other
        // command, or malformed.
        refused.push((keys::STOP, control(2, ControlOp::Stop, &MAC, 1)));
        refused.push((keys::STOP, control(1, ControlOp::Stop, &other_board, 1)));
        refused.push((keys::STOP, control(1, ControlOp::Start, &MAC, 1)));
        refused.push((keys::STOP, StdVec::from([0u8; 3])));
        for op in [
            ControlOp::FwReboot,
            ControlOp::FwConfirm,
            ControlOp::FwAbort,
        ] {
            let key = k(match op {
                ControlOp::FwReboot => "sys/fw/reboot",
                ControlOp::FwConfirm => "sys/fw/confirm",
                _ => "sys/fw/abort",
            });
            refused.push((key, control(2, op, &MAC, 1)));
            refused.push((key, control(1, op, &other_board, 1)));
            refused.push((key, control(1, ControlOp::Stop, &MAC, 1)));
        }
        for (key, body) in &refused {
            assert!(
                admit(&facts(s.state(), MAC), None, *key, body).is_err(),
                "{key:?}"
            );
            assert_eq!(
                zenoh(&mut s, *key, body),
                Err(HandlerError::Denied),
                "{key:?}"
            );
            assert_eq!(snapshot(&mut s), before, "{key:?} changed the board");
        }
        assert_eq!(s.status(), PayloadStatus::Running);
        // The same unsigned stop over serial does change it: the gate is
        // what refused.
        assert_eq!(serial(&mut s, keys::STOP, &[]), Ok(StdVec::new()));
        assert_eq!(s.status(), PayloadStatus::Stopped);
    }

    #[test]
    fn zn4_refusals_name_their_reason() {
        let f = facts(claimed_running().state(), MAC);
        let img = image();
        assert_eq!(
            admit(&f, None, keys::BEGIN, &begin_body(&img)),
            Ok(Admission::Dispatch)
        );
        assert_eq!(
            admit(&f, None, keys::TRUST_CLAIM, public(2).as_bytes()),
            Err(Refusal::NotOverZenoh)
        );
        assert_eq!(
            admit(&f, None, fw_keys::REBOOT, &[]),
            Err(Refusal::UnsignedControl)
        );
        assert_eq!(
            admit(&f, None, keys::STOP, &[]),
            Err(Refusal::UnsignedControl)
        );
        assert_eq!(
            admit(&f, None, keys::STOP, &control(2, ControlOp::Stop, &MAC, 1)),
            Err(Refusal::ControlRefused)
        );
        assert_eq!(
            admit(&f, Some(origin(MAC)), keys::COMMIT, &[]),
            Err(Refusal::UnsignedCommit)
        );
        assert_eq!(
            admit(&f, Some(Origin::Serial), keys::CHUNK, &[0; 8]),
            Err(Refusal::NotOwner)
        );
        for r in [
            Refusal::Unclaimed,
            Refusal::NotOverZenoh,
            Refusal::UnsignedControl,
            Refusal::ControlRefused,
            Refusal::UnsignedCommit,
            Refusal::NotOwner,
        ] {
            let want = match r {
                Refusal::NotOwner => HandlerError::InvalidRequest,
                _ => HandlerError::Denied,
            };
            assert_eq!(r.error(), want);
            assert_eq!(r.abandons_transfer(), r == Refusal::UnsignedCommit);
            assert!(!r.reason().is_empty());
        }
    }

    #[test]
    fn zn4_a_start_or_stop_signed_by_the_pinned_key_is_admitted_once() {
        let mut s = claimed_running();
        let f = facts(s.state(), MAC);
        assert_eq!(f.control, ControlCounter(0));
        let stop = control(1, ControlOp::Stop, &MAC, 7);
        assert_eq!(
            admit(&f, None, keys::STOP, &stop),
            Ok(Admission::Control {
                counter: ControlCounter(7)
            })
        );
        // Once the counter is persisted, the same request is a replay.
        let after = Facts {
            control: ControlCounter(7),
            ..f
        };
        assert_eq!(
            admit(&after, None, keys::STOP, &stop),
            Err(Refusal::ControlRefused)
        );
        let start = control(1, ControlOp::Start, &MAC, 8);
        assert_eq!(
            admit(&after, None, keys::START, &start),
            Ok(Admission::Control {
                counter: ControlCounter(8)
            })
        );
        // Dispatched with an empty body, it stops the payload.
        assert_eq!(zenoh(&mut s, keys::STOP, &stop), Ok(StdVec::new()));
        assert_eq!(s.status(), PayloadStatus::Stopped);
    }

    #[test]
    fn zn4_management_keys_are_this_nodes_system_keys() {
        let ours = node_key_buf(NODE, keys::INFO).unwrap();
        assert_eq!(management_key(&ours, NODE), Some(keys::INFO));
        let fw = node_key_buf(NODE, fw_keys::INFO).unwrap();
        assert_eq!(management_key(&fw, NODE), Some(fw_keys::INFO));
        let theirs = node_key_buf(SenderId([9; 8]), keys::INFO).unwrap();
        assert_eq!(management_key(&theirs, NODE), None);
        let app = node_key_buf(NODE, k("sensor/imu")).unwrap();
        assert_eq!(management_key(&app, NODE), None);
        for junk in [
            "",
            "sys/info",
            "@mw/node/",
            "@mw/node/0210500b831f2a0b",
            "@mw/node/0210500b831f2a0b/",
            "@mw/node/zz10500b831f2a0b/sys/info",
            "@mw/node/0210500b831f2a0b/sys//info",
            "@mw/node/\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}/sys/info",
        ] {
            assert_eq!(management_key(junk, NODE), None, "{junk:?}");
        }
    }

    #[test]
    fn zn4_no_body_panics_the_gate() {
        let f = facts(claimed_running().state(), MAC);
        let bodies: [&[u8]; 5] = [&[], &[0xff; 1], &[0; 71], &[0; 72], &[0xff; 200]];
        for key in changing(&image())
            .iter()
            .map(|(k, _)| *k)
            .chain(READ_ONLY_KEYS)
        {
            for body in bodies {
                for owner in [None, Some(Origin::Serial), Some(origin(MAC))] {
                    let _ = admit(&f, owner, key, body);
                    let _ = admit(&facts(State::boot(false), MAC), owner, key, body);
                }
            }
        }
    }

    #[test]
    fn zn4_the_zenoh_endpoint_is_never_read_or_set_over_zenoh() {
        use crate::config::{Endpoint, ZENOH_ENDPOINT};
        let set = b"tcp/10.0.0.9:7447";
        // Unclaimed: refused like every key but the identity queries.
        let s = open();
        for body in [&[][..], set] {
            assert_eq!(
                admit(&facts(s.state(), MAC), None, ZENOH_ENDPOINT, body),
                Err(Refusal::Unclaimed)
            );
        }
        // Claimed: never over Zenoh, signed or not.
        let mut s = claimed_running();
        assert_eq!(
            serial(&mut s, ZENOH_ENDPOINT, b"tcp/10.0.0.1:7447"),
            Ok(StdVec::new())
        );
        let before = snapshot(&mut s);
        let signed = control(1, ControlOp::Stop, &MAC, 1);
        for body in [&[][..], set, &signed] {
            assert_eq!(
                admit(&facts(s.state(), MAC), None, ZENOH_ENDPOINT, body),
                Err(Refusal::NotOverZenoh)
            );
            assert_eq!(
                zenoh(&mut s, ZENOH_ENDPOINT, body),
                Err(HandlerError::Denied)
            );
            assert_eq!(snapshot(&mut s), before);
        }
        let mine = Endpoint::parse("tcp/10.0.0.1:7447").ok();
        assert_eq!(s.zenoh_endpoint(), mine);
        assert_eq!(reboot(&mut s).zenoh_endpoint(), mine);
        // Its per-node key is a management key like any other: the gate,
        // not the key filter, is what refuses it.
        let expr = node_key_buf(NODE, ZENOH_ENDPOINT).unwrap();
        assert_eq!(management_key(&expr, NODE), Some(ZENOH_ENDPOINT));
    }

    #[test]
    fn zn4_an_accepted_control_counter_refuses_its_replay_across_reboots() {
        let mut s = claimed_running();
        let trust = s.state().trust();
        let stop = control(1, ControlOp::Stop, &MAC, 7);
        let counter = match admit(&facts(s.state(), MAC), None, keys::STOP, &stop) {
            Ok(Admission::Control { counter }) => counter,
            other => panic!("not admitted: {other:?}"),
        };
        assert_eq!(counter, ControlCounter(7));
        assert_eq!(s.accept_control(counter), Ok(()));
        assert_eq!(s.state().control(), ControlCounter(7));
        assert_eq!(s.state().trust(), trust, "the trust state is unchanged");
        assert_eq!(serial(&mut s, keys::STOP, &[]), Ok(StdVec::new()));
        assert_eq!(s.status(), PayloadStatus::Stopped);
        // The same request again is a replay, at the gate and at the
        // service (a counter not above the last writes nothing).
        assert_eq!(
            admit(&facts(s.state(), MAC), None, keys::STOP, &stop),
            Err(Refusal::ControlRefused)
        );
        let ops = s.store_mut().flash().raw().log.len();
        for c in [7, 6, 0] {
            assert_eq!(
                s.accept_control(ControlCounter(c)),
                Err(StoreError::Rollback),
                "{c}"
            );
        }
        assert_eq!(s.store_mut().flash().raw().log.len(), ops);
        // After a reboot the counter is still 7: the replay stays refused,
        // the pinned key and min_version are kept.
        let mut s = reboot(&mut s);
        assert_eq!(s.state().control(), ControlCounter(7));
        assert_eq!(s.state().trust(), trust);
        assert_eq!(zenoh(&mut s, keys::STOP, &stop), Err(HandlerError::Denied));
        // The next counter goes through the shell's path and persists.
        let start = control(1, ControlOp::Start, &MAC, 8);
        assert_eq!(zenoh(&mut s, keys::START, &start), Ok(StdVec::new()));
        assert_eq!(s.state().control(), ControlCounter(8));
        assert_eq!(
            zenoh(&mut s, keys::START, &start),
            Err(HandlerError::Denied)
        );
        let s = reboot(&mut s);
        assert_eq!(s.state().control(), ControlCounter(8));
    }

    #[test]
    fn zn4_accept_control_keeps_the_old_counter_when_its_save_is_cut() {
        // A cut while saving the counter: Flash, and the state keeps the
        // record on flash (the old counter), before and after a reboot.
        let mut s = claimed_running();
        s.store_mut().flash().raw().cut_at(0);
        assert_eq!(s.accept_control(ControlCounter(3)), Err(StoreError::Flash));
        s.store_mut().flash().raw().restore();
        assert_eq!(s.state().control(), ControlCounter(0));
        let mut s = reboot(&mut s);
        assert_eq!(s.state().control(), ControlCounter(0));
        // The request whose counter could not be saved was not dispatched
        // by the shell, so it can be sent again.
        let stop = control(1, ControlOp::Stop, &MAC, 3);
        assert_eq!(zenoh(&mut s, keys::STOP, &stop), Ok(StdVec::new()));
        assert_eq!(s.state().control(), ControlCounter(3));
    }

    fn chunks(img: &[u8]) -> StdVec<StdVec<u8>> {
        img.chunks(48)
            .enumerate()
            .map(|(i, part)| {
                let mut c = (i as u32 * 48).to_le_bytes().to_vec();
                c.extend_from_slice(part);
                c
            })
            .collect()
    }

    fn other_image() -> StdVec<u8> {
        (0..700u32)
            .map(|i| (i as u8).wrapping_mul(7) ^ 0x5a)
            .collect()
    }

    #[test]
    fn zn5_a_payload_upload_over_zenoh_stages_and_needs_a_signed_commit() {
        let mut s = claimed_running();
        let img = other_image();
        let before_header = s.store_mut().header().map(|h| h.len);
        // Begin and chunks stage beside the running payload.
        assert_eq!(
            zenoh(&mut s, keys::BEGIN, &begin_body(&img)),
            Ok(StdVec::new())
        );
        assert_eq!(
            s.status(),
            PayloadStatus::Running,
            "staging keeps it running"
        );
        assert_eq!(s.loader_owner(), Some(origin(MAC)));
        for c in chunks(&img) {
            assert_eq!(zenoh(&mut s, keys::CHUNK, &c), Ok(StdVec::new()));
        }
        // The owner's unsigned commit: Denied, and the transfer ends; the
        // payload still runs, the installed image is the old one.
        assert_eq!(zenoh(&mut s, keys::COMMIT, &[]), Err(HandlerError::Denied));
        assert_eq!(s.loader_owner(), None);
        assert_eq!(s.status(), PayloadStatus::Running);
        assert_eq!(s.store_mut().header().map(|h| h.len), before_header);
        // Signed by another key, or with a used version: Denied by the
        // dispatcher's trust rules, nothing activated.
        for bad in [signed_commit(2, &img, 9), signed_commit(1, &img, 5)] {
            assert_eq!(
                zenoh(&mut s, keys::BEGIN, &begin_body(&img)),
                Ok(StdVec::new())
            );
            for c in chunks(&img) {
                assert_eq!(zenoh(&mut s, keys::CHUNK, &c), Ok(StdVec::new()));
            }
            assert_eq!(zenoh(&mut s, keys::COMMIT, &bad), Err(HandlerError::Denied));
            assert_eq!(s.status(), PayloadStatus::Running);
            assert_eq!(s.store_mut().header().map(|h| h.len), before_header);
        }
        // A tampered chunk: the signed commit fails its CRC.
        assert_eq!(
            zenoh(&mut s, keys::BEGIN, &begin_body(&img)),
            Ok(StdVec::new())
        );
        for (i, mut c) in chunks(&img).into_iter().enumerate() {
            if i == 3 {
                c[10] ^= 1;
            }
            assert_eq!(zenoh(&mut s, keys::CHUNK, &c), Ok(StdVec::new()));
        }
        assert!(zenoh(&mut s, keys::COMMIT, &signed_commit(1, &img, 9)).is_err());
        assert_eq!(s.store_mut().header().map(|h| h.len), before_header);
        // Properly signed with a fresh version: activated; the old payload
        // stops (the shell starts the new one when asked).
        assert_eq!(
            zenoh(&mut s, keys::BEGIN, &begin_body(&img)),
            Ok(StdVec::new())
        );
        for c in chunks(&img) {
            assert_eq!(zenoh(&mut s, keys::CHUNK, &c), Ok(StdVec::new()));
        }
        assert_eq!(
            zenoh(&mut s, keys::COMMIT, &signed_commit(1, &img, 9)),
            Ok(StdVec::new())
        );
        assert_eq!(
            s.store_mut().header().map(|h| h.len),
            Some(img.len() as u32)
        );
        assert_eq!(s.status(), PayloadStatus::Stopped);
        assert!(matches!(
            s.state().trust(),
            TrustState::Claimed {
                min_version: Version(9),
                ..
            }
        ));
        // A replay of the whole upload is refused (version 9 used).
        assert_eq!(
            zenoh(&mut s, keys::BEGIN, &begin_body(&img)),
            Ok(StdVec::new())
        );
        for c in chunks(&img) {
            assert_eq!(zenoh(&mut s, keys::CHUNK, &c), Ok(StdVec::new()));
        }
        assert_eq!(
            zenoh(&mut s, keys::COMMIT, &signed_commit(1, &img, 9)),
            Err(HandlerError::Denied)
        );
        // It survives a reboot.
        let mut s = reboot(&mut s);
        assert_eq!(
            s.store_mut().header().map(|h| h.len),
            Some(img.len() as u32)
        );
    }

    #[test]
    fn zn5_a_transfer_belongs_to_the_requester_that_began_it() {
        let mut s = claimed_running();
        let img = other_image();
        let cs = chunks(&img);
        // Serial begins; Zenoh chunks and commits are refused untouched.
        assert_eq!(
            serial(&mut s, keys::BEGIN, &begin_body(&img)),
            Ok(StdVec::new())
        );
        assert_eq!(s.loader_owner(), Some(Origin::Serial));
        assert_eq!(
            zenoh(&mut s, keys::CHUNK, &cs[0]),
            Err(HandlerError::InvalidRequest)
        );
        assert_eq!(
            zenoh(&mut s, keys::COMMIT, &signed_commit(1, &img, 9)),
            Err(HandlerError::InvalidRequest)
        );
        assert_eq!(
            zenoh(&mut s, keys::COMMIT, &[]),
            Err(HandlerError::InvalidRequest)
        );
        for c in &cs {
            assert_eq!(serial(&mut s, keys::CHUNK, c), Ok(StdVec::new()));
        }
        assert_eq!(
            serial(&mut s, keys::COMMIT, &signed_commit(1, &img, 9)),
            Ok(StdVec::new())
        );
        // Zenoh begins; serial chunks are refused, a serial begin takes
        // the transfer back.
        assert_eq!(
            zenoh(&mut s, keys::BEGIN, &begin_body(&img)),
            Ok(StdVec::new())
        );
        assert_eq!(
            serial(&mut s, keys::CHUNK, &cs[0]),
            Err(HandlerError::InvalidRequest)
        );
        assert_eq!(
            serial(&mut s, keys::BEGIN, &begin_body(&img)),
            Ok(StdVec::new())
        );
        assert_eq!(
            zenoh(&mut s, keys::CHUNK, &cs[0]),
            Err(HandlerError::InvalidRequest)
        );
        assert_eq!(serial(&mut s, keys::CHUNK, &cs[0]), Ok(StdVec::new()));
    }

    fn fw_signed_commit(seed: u8, img: &[u8], version: u32, slot: Slot) -> StdVec<u8> {
        use middleware::fw_ota::wire::{Command, signed_message as fw_message};
        let m = fw_message(
            img.len() as u32,
            Version(version),
            slot,
            &NODE,
            &ImageHash::digest(img),
        );
        let signature = Some(Signature(signer(seed).sign(&m).to_bytes()));
        crate::service::tests::fw_body(&Command::Commit { signature })
    }

    fn fw_transfer(s: &mut Svc, img: &[u8], over_zenoh: bool) -> Answer {
        use crate::service::tests::{V1_1, fw_body};
        use middleware::fw_ota::CHUNK_LEN;
        use middleware::fw_ota::wire::Command;
        let send = |s: &mut Svc, key, body: &[u8]| match over_zenoh {
            true => zenoh(s, key, body),
            false => serial(s, key, body),
        };
        let begin = fw_body(&Command::Begin {
            len: img.len() as u32,
            version: Version(V1_1),
            sha256: ImageHash::digest(img),
        });
        let slot = send(s, fw_keys::BEGIN, &begin)?;
        for (i, data) in img.chunks(CHUNK_LEN).enumerate() {
            let offset = (i * CHUNK_LEN) as u32;
            send(
                s,
                fw_keys::CHUNK,
                &fw_body(&Command::Chunk { offset, data }),
            )?;
        }
        Ok(slot)
    }

    fn fw_flash() -> BlockFlash {
        use crate::service::tests::{V1_0, slot_image};
        let mut f = BlockFlash::erased(MAP.slot_b.end() as usize);
        f.program(MAP.ab().a.offset as usize, &slot_image(V1_0, 1, 2000));
        f
    }

    /// A claimed board (key 1) whose slot A holds firmware 1.0.
    fn claimed_fw() -> Svc {
        let mut s = Service::open(fw_flash(), &MAP, FACTS, FACTORY).unwrap().0;
        assert_eq!(
            serial(&mut s, keys::TRUST_CLAIM, public(1).as_bytes()),
            Ok(StdVec::new())
        );
        s
    }

    #[test]
    fn zn5_a_firmware_update_over_zenoh_needs_the_signed_commit_and_signed_reboot() {
        use crate::ab::platform::ResetRequest;
        use crate::service::tests::{V1_1, slot_image};
        use middleware::fw_ota::FwUpdate;
        let mut s = claimed_fw();
        let v2 = slot_image(V1_1, 7, 3000);
        assert_eq!(fw_transfer(&mut s, &v2, true), Ok(std::vec![Slot::B as u8]));
        assert_eq!(s.fw_owner(), Some(origin(MAC)));
        // Unsigned, other key, wrong slot: Denied, nothing committed.
        for bad in [
            crate::service::tests::fw_body(&middleware::fw_ota::wire::Command::Commit {
                signature: None,
            }),
            fw_signed_commit(2, &v2, V1_1, Slot::B),
            fw_signed_commit(1, &v2, V1_1, Slot::A),
        ] {
            assert_eq!(
                zenoh(&mut s, fw_keys::COMMIT, &bad),
                Err(HandlerError::Denied)
            );
            assert!(!matches!(s.fw().state(), Some(FwUpdate::Committed(_))));
            assert_eq!(fw_transfer(&mut s, &v2, true), Ok(std::vec![Slot::B as u8]));
        }
        // A serial chunk or commit cannot touch the Zenoh transfer.
        assert_eq!(
            serial(
                &mut s,
                fw_keys::COMMIT,
                &fw_signed_commit(1, &v2, V1_1, Slot::B)
            ),
            Err(HandlerError::InvalidRequest)
        );
        assert_eq!(
            zenoh(
                &mut s,
                fw_keys::COMMIT,
                &fw_signed_commit(1, &v2, V1_1, Slot::B)
            ),
            Ok(StdVec::new())
        );
        assert!(matches!(s.fw().state(), Some(FwUpdate::Committed(_))));
        assert_eq!(s.fw_owner(), None);
        // Reboot: unsigned or replayed is Denied; signed resets into B.
        let (r, h) = zenoh_h(&mut s, fw_keys::REBOOT, &[]);
        assert_eq!((r, h.is_none()), (Err(HandlerError::Denied), true));
        let reboot_body = control(1, ControlOp::FwReboot, &MAC, 40);
        let (r, h) = zenoh_h(&mut s, fw_keys::REBOOT, &reboot_body);
        assert_eq!(r, Ok(StdVec::new()));
        assert_eq!(h.unwrap().reset, Some(ResetRequest::Update(Slot::B)));
        assert_eq!(s.state().control(), ControlCounter(40));
        assert_eq!(
            zenoh(&mut s, fw_keys::REBOOT, &reboot_body),
            Err(HandlerError::Denied)
        );
        // The trial boots; confirm over Zenoh must be signed too.
        let mut f = s.store_mut().flash().raw().clone();
        let d = crate::service::tests::bootloader(&mut f);
        let mut s = Service::open(f, &MAP, FACTS, d.handoff()).unwrap().0;
        s.set_link_ready(true);
        assert_eq!(
            zenoh(&mut s, fw_keys::CONFIRM, &[]),
            Err(HandlerError::Denied)
        );
        assert_eq!(
            zenoh(
                &mut s,
                fw_keys::CONFIRM,
                &control(1, ControlOp::FwConfirm, &MAC, 40)
            ),
            Err(HandlerError::Denied),
            "the counter survived the reboot"
        );
        assert_eq!(
            zenoh(
                &mut s,
                fw_keys::CONFIRM,
                &control(1, ControlOp::FwReboot, &MAC, 41)
            ),
            Err(HandlerError::Denied),
            "a reboot signature is not a confirm"
        );
        let (r, h) = zenoh_h(
            &mut s,
            fw_keys::CONFIRM,
            &control(1, ControlOp::FwConfirm, &MAC, 41),
        );
        assert_eq!(r, Ok(StdVec::new()));
        assert!(h.unwrap().release_watchdog);
        assert!(matches!(
            s.fw().state(),
            Some(FwUpdate::Confirmed { slot: Slot::B, .. })
        ));
    }

    #[test]
    fn zn5_a_signed_abort_over_zenoh_ends_a_firmware_transfer() {
        use crate::service::tests::{V1_1, slot_image};
        use middleware::fw_ota::FwUpdate;
        let mut s = claimed_fw();
        let v2 = slot_image(V1_1, 9, 1500);
        fw_transfer(&mut s, &v2, false).unwrap();
        assert_eq!(s.fw_owner(), Some(Origin::Serial));
        assert_eq!(
            zenoh(&mut s, fw_keys::ABORT, &[]),
            Err(HandlerError::Denied)
        );
        assert!(matches!(s.fw().state(), Some(FwUpdate::Receiving(_))));
        assert_eq!(
            zenoh(
                &mut s,
                fw_keys::ABORT,
                &control(1, ControlOp::FwAbort, &MAC, 5)
            ),
            Ok(StdVec::new())
        );
        assert!(!matches!(s.fw().state(), Some(FwUpdate::Receiving(_))));
        assert_eq!(s.fw_owner(), None);
    }

    #[test]
    fn gap_payload_subscriptions_become_zenoh_declarations() {
        let set =
            subscription_set(["app/cmd", "sys/info", "app/cmd", "bad//key", "app/led"].into_iter());
        let keys: std::vec::Vec<&str> = set.iter().map(|k| k.as_str()).collect();
        assert_eq!(keys, ["app/cmd", "app/led"], "valid non-sys keys, once");
        let next = subscription_set(["app/led", "app/new"].into_iter());
        let (undeclare, declare) = subscription_diff(&set, &next);
        assert_eq!(
            undeclare
                .iter()
                .map(|k| k.as_str())
                .collect::<std::vec::Vec<_>>(),
            ["app/cmd"]
        );
        assert_eq!(
            declare
                .iter()
                .map(|k| k.as_str())
                .collect::<std::vec::Vec<_>>(),
            ["app/new"]
        );
        // A stopped payload: everything goes.
        let (undeclare, declare) = subscription_diff(&next, &SubSet::new());
        assert_eq!((undeclare.len(), declare.len()), (2, 0));
        // A reconnect: everything is declared again.
        let (undeclare, declare) = subscription_diff(&SubSet::new(), &next);
        assert_eq!((undeclare.len(), declare.len()), (0, 2));
        assert_eq!(
            subscription_diff(&next, &next),
            (SubSet::new(), SubSet::new())
        );
    }
}
