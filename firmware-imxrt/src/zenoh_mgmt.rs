//! The gate for management requests over Zenoh (ZN4): which `sys/...`
//! queries a Zenoh peer may make, decided before
//! [`Service::handle`](crate::service::Service::handle)
//! is called, so a refused request never reaches the
//! service and leaves the board unchanged.
//!
//! The board's `sys/...` keys are queryable as per-node keys,
//! `@mw/node/<zid16>/sys/...` ([`management_key`]). Anyone on the network
//! can send a query, and Zenoh (client mode, no TLS) authenticates nobody,
//! so the rules are those of a radio peer on the ESP base firmware
//! (`esp_base::ota`), whose signature checks this gate reuses, with one
//! difference: an unclaimed board still answers its two identity queries,
//! so a Zenoh network can find it and see that it is unclaimed.
//!
//! | request over Zenoh                   | unclaimed | claimed |
//! |--------------------------------------|-----------|---------|
//! | `sys/info`, `sys/trust/info`         | answered  | answered |
//! | `sys/board`, `sys/fw/info`           | `Denied`  | answered (read only) |
//! | `sys/payload/start`, `stop`          | `Denied`  | a [`SignedControl`](middleware::loader::trust::SignedControl) body only, verified by `esp_base::ota::admit` (`TrustState::accept_control`: the pinned key, this board's [`Facts::own`] address, this command, a counter above the last): [`Admission::Control`]. Unsigned or refused: `Denied` |
//! | anything else under `sys/`           | `Denied`  | `Denied` |
//!
//! Every refusal is [`HandlerError::Denied`] ([`Refusal::error`]): the
//! request was well formed but policy refuses it, as over the air.
//!
//! What "anything else" covers, and why it has no signed form here:
//!
//! * `sys/trust/claim`: never over the network (as over the air); pinning
//!   a key needs the serial port, where someone holds the board.
//! * `sys/loader/begin`, `chunk`, `commit`: the service decides uploads
//!   without staging (`Context::staging` is false), so an unsigned
//!   `begin` stops the running payload; only a signed commit could
//!   authorize the upload, and it comes last. Uploading over Zenoh needs a
//!   staging dispatch in the service first (then `esp_base::ota`'s upload
//!   rules apply: `begin`/`chunk` stage beside the payload, the commit
//!   must be signed).
//! * `sys/fw/begin`, `chunk`: they restart the A/B updater's transfer and
//!   erase the inactive slot's first sector, discarding a committed update
//!   awaiting its reboot; `sys/fw/reboot`, `confirm` and `abort` have no
//!   signed form at all (`middleware::fw_ota`). Firmware updates stay on
//!   the serial links.
//! * `sys/link/*` (the serial links' filters, [`crate::links`]) and every
//!   other system key.
//!
//! A board whose trust record could not be read reports Unclaimed in its
//! dispatcher state, so it answers only the two identity queries (fail
//! closed).
//!
//! # Use
//!
//! ```text
//! let key = management_key(query_expr, svc.node())?;           // else: not ours
//! match admit(&facts(svc.state(), enet_mac), key, body) {
//!     Err(r)                          => reply r.error(), log r.reason()
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
use middleware::loader::{is_system_key, keys};
use middleware::transport::{HandlerError, SenderId};
use middleware::zenoh_wire::parse_node_key;

pub use esp_base::ota::{Admission, Facts};

/// The requests an unclaimed board answers over Zenoh.
pub const UNCLAIMED_KEYS: [Key<'static>; 2] = [keys::INFO, keys::TRUST_INFO];

/// The read-only requests a claimed board answers over Zenoh.
pub const READ_ONLY_KEYS: [Key<'static>; 4] =
    [keys::INFO, keys::TRUST_INFO, keys::BOARD, fw_keys::INFO];

/// The requests a claimed board accepts over Zenoh with a signature by the
/// pinned key ([`Admission::Control`]).
pub const SIGNED_KEYS: [Key<'static>; 2] = [keys::START, keys::STOP];

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
    /// A start or stop without a well-formed signed body.
    UnsignedControl,
    /// A signed start or stop that does not verify: another key, another
    /// board, another command, or a counter not above the last.
    ControlRefused,
}

impl Refusal {
    /// The status sent back: always `Denied`.
    pub const fn error(self) -> HandlerError {
        HandlerError::Denied
    }

    /// A short reason for the log.
    pub const fn reason(self) -> &'static str {
        match self {
            Refusal::Unclaimed => "board unclaimed; only sys/info and sys/trust/info over Zenoh",
            Refusal::NotOverZenoh => "not allowed over Zenoh (use a serial link)",
            Refusal::UnsignedControl => "start/stop over Zenoh must be signed by the pinned key",
            Refusal::ControlRefused => {
                "control signature invalid (pinned key, this board, this command) or counter not above the last"
            }
        }
    }
}

/// Decides a management request on `key` with `body` over Zenoh, before
/// the service sees it. See the module docs.
pub fn admit(facts: &Facts, key: Key<'_>, body: &[u8]) -> Result<Admission, Refusal> {
    if UNCLAIMED_KEYS.contains(&key) {
        return Ok(Admission::Dispatch);
    }
    if !facts.trust.is_claimed() {
        return Err(Refusal::Unclaimed);
    }
    if READ_ONLY_KEYS.contains(&key) {
        return Ok(Admission::Dispatch);
    }
    if !SIGNED_KEYS.contains(&key) {
        return Err(Refusal::NotOverZenoh);
    }
    // The radio gate's own check: claimed, an air key, no transfer owner
    // involved, so it is exactly the signed control verification.
    match ota::admit(facts, None, Origin::Air(facts.own), key, body) {
        Ok(admission) => Ok(admission),
        Err(ota::Refusal::UnsignedControl) => Err(Refusal::UnsignedControl),
        Err(ota::Refusal::ControlRefused) => Err(Refusal::ControlRefused),
        Err(_) => Err(Refusal::NotOverZenoh),
    }
}

#[cfg(test)]
mod tests {
    //! ZN4 against the real request service on the RAM HyperFlash model.

    extern crate std;

    use super::*;
    use crate::ab::Handoff;
    use crate::ab::test_flash::BlockFlash;
    use crate::layout::{FlashMap, RT1050_EVKB};
    use crate::service::{Facts as SvcFacts, Reply, Service};
    use ed25519_dalek::{Signer, SigningKey};
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
        PublicKey(signer(seed).verifying_key().to_bytes())
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

    /// A request over a serial link: straight to the service.
    fn serial(s: &mut Svc, key: Key<'_>, body: &[u8]) -> Result<StdVec<u8>, HandlerError> {
        let mut out = [0u8; 128];
        match s.handle(key, body, &mut out).reply {
            Reply::Now(r) => r.map(|n| out.get(..n).unwrap_or(&[]).to_vec()),
            Reply::Start => Ok(StdVec::new()),
        }
    }

    /// A request over Zenoh: the gate first, the service only if admitted
    /// (a verified control request is dispatched with an empty body, as
    /// the shell does after persisting its counter).
    fn zenoh(s: &mut Svc, key: Key<'_>, body: &[u8]) -> Result<StdVec<u8>, HandlerError> {
        match admit(&facts(s.state(), MAC), key, body) {
            Err(r) => Err(r.error()),
            Ok(Admission::Dispatch) => serial(s, key, body),
            Ok(Admission::Control { .. }) => serial(s, key, &[]),
        }
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
            (keys::TRUST_CLAIM, public(2).0.to_vec()),
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
        refused.push((fw_keys::INFO, StdVec::new()));
        refused.push((keys::STOP, control(1, ControlOp::Stop, &MAC, 1)));
        for (key, body) in &refused {
            assert_eq!(
                admit(&facts(s.state(), MAC), *key, body),
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
            serial(&mut s, keys::TRUST_CLAIM, &public(1).0),
            Ok(StdVec::new())
        );
        assert!(s.state().trust().is_claimed());
    }

    /// A claimed board (key 1) with a signed payload installed, running.
    fn claimed_running() -> Svc {
        let mut s = open();
        let img = image();
        assert_eq!(
            serial(&mut s, keys::TRUST_CLAIM, &public(1).0),
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
                admit(&facts(s.state(), MAC), key, &[]),
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
        let mut refused = changing(&img);
        // Signed, but by another key, for another board, for the other
        // command, or malformed.
        refused.push((keys::STOP, control(2, ControlOp::Stop, &MAC, 1)));
        refused.push((keys::STOP, control(1, ControlOp::Stop, &other_board, 1)));
        refused.push((keys::STOP, control(1, ControlOp::Start, &MAC, 1)));
        refused.push((keys::STOP, StdVec::from([0u8; 3])));
        for (key, body) in &refused {
            assert!(
                admit(&facts(s.state(), MAC), *key, body).is_err(),
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
            admit(&f, keys::BEGIN, &begin_body(&img)),
            Err(Refusal::NotOverZenoh)
        );
        assert_eq!(
            admit(&f, keys::TRUST_CLAIM, &public(2).0),
            Err(Refusal::NotOverZenoh)
        );
        assert_eq!(admit(&f, fw_keys::REBOOT, &[]), Err(Refusal::NotOverZenoh));
        assert_eq!(admit(&f, keys::STOP, &[]), Err(Refusal::UnsignedControl));
        assert_eq!(
            admit(&f, keys::STOP, &control(2, ControlOp::Stop, &MAC, 1)),
            Err(Refusal::ControlRefused)
        );
        for r in [
            Refusal::Unclaimed,
            Refusal::NotOverZenoh,
            Refusal::UnsignedControl,
            Refusal::ControlRefused,
        ] {
            assert_eq!(r.error(), HandlerError::Denied);
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
            admit(&f, keys::STOP, &stop),
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
            admit(&after, keys::STOP, &stop),
            Err(Refusal::ControlRefused)
        );
        let start = control(1, ControlOp::Start, &MAC, 8);
        assert_eq!(
            admit(&after, keys::START, &start),
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
                let _ = admit(&f, key, body);
                let _ = admit(&facts(State::boot(false), MAC), key, body);
            }
        }
    }
}
