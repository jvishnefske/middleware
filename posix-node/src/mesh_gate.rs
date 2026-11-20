//! The gate for the node's `sys/...` requests over Zenoh (PN23, PN24):
//! which of them a Zenoh peer may make, decided before the
//! [`Service`](crate::service::Service) sees them, so a refused request
//! leaves the state, the flash and the running payload unchanged. Pure.
//!
//! Anyone on the network can send a query and Zenoh authenticates nobody
//! here, so the rules are a radio peer's on the ESP base firmware
//! (`esp_base::ota`, whose checks this reuses) and the RT1050's over Zenoh
//! (ZN5, `firmware-imxrt/src/zenoh_mgmt.rs`), with one deliberate
//! difference: the read-only description keys answer in any trust state,
//! so a network finds an unclaimed node, its payload interface and its
//! control-module points (`mwctl scout`, a browser's autoprobe).
//!
//! | request over Zenoh | unclaimed | claimed |
//! |---|---|---|
//! | [`READ_ANY`]: `sys/info`, `sys/trust/info`, `sys/manifest`, `sys/manifest/cm` | answered | answered |
//! | `sys/payload/binding` | `Denied` | answered |
//! | `sys/loader/begin`, `chunk` | `Denied` | staged beside the running payload; a `chunk` from a requester that does not own the transfer is `InvalidRequest` ([`Refusal::NotOwner`]) |
//! | `sys/loader/commit` | `Denied` | the owner's, signed; the owner's unsigned commit is `Denied` and ends the transfer ([`Refusal::abandons_transfer`]) |
//! | `sys/payload/start`, `stop` | `Denied` | a signed control body naming [`address`] with a counter above the last: [`Admission::Control`] |
//! | `sys/payload/bind` | `Denied` | signed only (the dispatcher checks the signature and version) |
//! | anything else (`sys/trust/claim` among them) | `Denied` | `Denied` |
//!
//! All Zenoh requesters are one requester ([`origin`]), distinct from the
//! serial links (the gateway's `gw/sys` and the local link).
//!
//! A node whose trust record could not be read reports Unclaimed, so it
//! answers only [`READ_ANY`] (fail closed).

use esp_base::ota::{self, Mac, Origin};
use middleware::loader::binding::BindRequest;
use middleware::loader::keys;
use middleware::transport::HandlerError;
use middleware::{Key, SenderId};

pub use esp_base::ota::{Admission, Facts};

/// The description keys answered in any trust state.
pub const READ_ANY: [Key<'static>; 4] = [keys::INFO, keys::TRUST_INFO, keys::MANIFEST, keys::CONTROL_MODULES];

/// The keys a claimed node hands to `esp_base::ota::admit`: the payload
/// transfer and the signed start and stop.
const OTA_KEYS: [Key<'static>; 5] = [keys::BEGIN, keys::CHUNK, keys::COMMIT, keys::START, keys::STOP];

/// The 6-byte address a signed control request must name: the first six
/// bytes of the node id `sys/info` reports, as through `gw/sys`
/// (`mw_gateway::update`), not the RT1050's `02` and last five.
pub const fn address(node: SenderId) -> Mac {
    let n = node.0;
    [n[0], n[1], n[2], n[3], n[4], n[5]]
}

/// The origin the service gives Zenoh requests: every Zenoh peer is one
/// requester, named by the node's own [`address`].
pub const fn origin(own: Mac) -> Origin {
    Origin::Air(own)
}

/// Why a request over Zenoh is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The node has no pinned key: only [`READ_ANY`] is answered.
    Unclaimed,
    /// Never accepted over Zenoh (see the module docs).
    NotOverMesh,
    /// A start or stop without a well-formed signed body.
    UnsignedControl,
    /// A signed start or stop that does not verify: another key, another
    /// node, another command, or a counter not above the last.
    ControlRefused,
    /// The transfer owner's commit is unsigned (or malformed); it ends the
    /// transfer.
    UnsignedCommit,
    /// A `chunk` or `commit` while another requester owns the transfer.
    NotOwner,
    /// A `sys/payload/bind` without a signature (or malformed).
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

    /// Whether the transfer in progress must end
    /// ([`Service::abandon_transfer`](crate::service::Service::abandon_transfer)).
    pub const fn abandons_transfer(self) -> bool {
        matches!(self, Refusal::UnsignedCommit)
    }

    /// A short reason for the log.
    pub const fn reason(self) -> &'static str {
        match self {
            Refusal::Unclaimed => "node unclaimed; only its description keys over Zenoh",
            Refusal::NotOverMesh => "not allowed over Zenoh (use a serial link)",
            Refusal::UnsignedControl => "start/stop over Zenoh must be signed by the pinned key",
            Refusal::ControlRefused => {
                "control signature invalid (pinned key, this node, this command) or counter not above the last"
            }
            Refusal::UnsignedCommit => "payload commit over Zenoh must be signed; transfer ended",
            Refusal::NotOwner => "another requester owns the transfer",
            Refusal::UnsignedBinding => "a payload binding over Zenoh must be signed by the pinned key",
        }
    }
}

/// Decides a request on `key` with `body` over Zenoh; `owner` is the
/// payload transfer's owner
/// ([`Service::loader_owner`](crate::service::Service::loader_owner)) and
/// `facts.own` the node's [`address`]. See the module docs.
pub fn admit(facts: &Facts, owner: Option<Origin>, key: Key<'_>, body: &[u8]) -> Result<Admission, Refusal> {
    if READ_ANY.contains(&key) {
        return Ok(Admission::Dispatch);
    }
    if !facts.trust.is_claimed() {
        return Err(Refusal::Unclaimed);
    }
    if key == keys::BINDING {
        return Ok(Admission::Dispatch);
    }
    if key == keys::BIND {
        // Signed only; the dispatcher verifies the signature and version.
        return match BindRequest::parse(body) {
            Some(BindRequest { auth: Some(_), .. }) => Ok(Admission::Dispatch),
            _ => Err(Refusal::UnsignedBinding),
        };
    }
    if !OTA_KEYS.contains(&key) {
        return Err(Refusal::NotOverMesh);
    }
    // The radio gate's own checks: transfer ownership, a signed commit, a
    // verified signed start/stop.
    match ota::admit(facts, owner, origin(facts.own), key, body) {
        Ok(admission) => Ok(admission),
        Err(ota::Refusal::UnsignedControl) => Err(Refusal::UnsignedControl),
        Err(ota::Refusal::ControlRefused) => Err(Refusal::ControlRefused),
        Err(ota::Refusal::UnsignedCommit) => Err(Refusal::UnsignedCommit),
        Err(ota::Refusal::NotOwner) => Err(Refusal::NotOwner),
        Err(ota::Refusal::Unclaimed | ota::Refusal::NotOverAir) => Err(Refusal::NotOverMesh),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::loader::binding::SignedBinding;
    use middleware::loader::trust::{ControlCounter, ControlOp, Signature, TrustState, Version};
    use mwctl::sign::{Seed, sign_commit, sign_control};

    const NODE: SenderId = SenderId([0x5e, 0x11, 0x0d, 0xe0, 0x2a, 0x9c, 0x41, 0x07]);
    const OWN: Mac = [0x5e, 0x11, 0x0d, 0xe0, 0x2a, 0x9c];

    fn seed(b: u8) -> Seed {
        Seed::new([b; 32])
    }

    fn unclaimed() -> Facts {
        Facts { trust: TrustState::Unclaimed, control: ControlCounter(0), own: OWN }
    }

    fn claimed(control: u64) -> Facts {
        Facts {
            trust: TrustState::Claimed { key: seed(1).public_key(), min_version: Version(5) },
            control: ControlCounter(control),
            own: OWN,
        }
    }

    fn control(signer: u8, op: ControlOp, target: &Mac, counter: u64) -> Vec<u8> {
        sign_control(&seed(signer), op, target, ControlCounter(counter)).unwrap().encode().to_vec()
    }

    fn commit(signer: u8) -> Vec<u8> {
        sign_commit(&[1, 2, 3], &seed(signer), Version(6)).unwrap().encode().to_vec()
    }

    fn bind_record() -> Vec<u8> {
        mwctl::bindspec::record([7; 32], 3, &[]).expect("record")
    }

    fn signed_bind() -> Vec<u8> {
        let mut b = bind_record();
        b.extend_from_slice(&SignedBinding { version: Version(3), signature: Signature([9; 64]) }.encode());
        b
    }

    fn k(s: &str) -> Key<'_> {
        Key::new(s).expect("key")
    }

    /// Everything that is not a description key nor served at all.
    const NEVER: [&str; 12] = [
        "sys/trust/claim",
        "sys/board",
        "sys/peers",
        "sys/log",
        "sys/payload/status",
        "sys/fw/info",
        "sys/fw/begin",
        "sys/fw/reboot",
        "sys/link/sub",
        "sys/zenoh/endpoint",
        "sys/net/addr",
        "app/pi/echo",
    ];

    #[test]
    fn pn24_the_control_address_is_the_first_six_bytes_of_the_node_id() {
        assert_eq!(address(NODE), OWN);
        assert_eq!(origin(OWN), Origin::Air(OWN));
        assert_ne!(origin(OWN), Origin::Serial);
    }

    #[test]
    fn pn23_description_keys_answer_in_any_trust_state() {
        for f in [unclaimed(), claimed(0)] {
            for key in READ_ANY {
                assert_eq!(admit(&f, None, key, &[]), Ok(Admission::Dispatch), "{key:?}");
                assert_eq!(admit(&f, Some(Origin::Serial), key, &[]), Ok(Admission::Dispatch), "{key:?}");
            }
        }
    }

    #[test]
    fn pn23_an_unclaimed_node_refuses_everything_else() {
        let f = unclaimed();
        let start = control(1, ControlOp::Start, &OWN, 10);
        let cases: [(Key<'_>, Vec<u8>); 8] = [
            (keys::BINDING, vec![]),
            (keys::BEGIN, vec![0; 8]),
            (keys::CHUNK, vec![0; 8]),
            (keys::COMMIT, commit(1)),
            (keys::START, start.clone()),
            (keys::STOP, control(1, ControlOp::Stop, &OWN, 11)),
            (keys::BIND, signed_bind()),
            (keys::TRUST_CLAIM, seed(1).public_key().as_bytes().to_vec()),
        ];
        for (key, body) in &cases {
            let r = admit(&f, None, *key, body);
            assert_eq!(r, Err(Refusal::Unclaimed), "{key:?}");
            assert_eq!(r.unwrap_err().error(), HandlerError::Denied);
        }
    }

    #[test]
    fn pn22_keys_outside_the_table_never_pass() {
        for f in [unclaimed(), claimed(0)] {
            for key in NEVER {
                let r = admit(&f, None, k(key), &[]).expect_err(key);
                assert_eq!(r.error(), HandlerError::Denied, "{key}");
                assert!(!r.abandons_transfer());
            }
        }
        assert_eq!(admit(&claimed(0), None, keys::TRUST_CLAIM, &[0; 33]), Err(Refusal::NotOverMesh));
    }

    #[test]
    fn pn23_a_claimed_node_stages_transfers_and_reads_its_binding() {
        let f = claimed(0);
        assert_eq!(admit(&f, None, keys::BINDING, &[]), Ok(Admission::Dispatch));
        assert_eq!(admit(&f, None, keys::BEGIN, &[0; 8]), Ok(Admission::Dispatch));
        let mine = Some(origin(OWN));
        assert_eq!(admit(&f, mine, keys::CHUNK, &[0; 8]), Ok(Admission::Dispatch));
        assert_eq!(admit(&f, mine, keys::COMMIT, &commit(1)), Ok(Admission::Dispatch));
        // The dispatcher checks the commit's key and version; the gate only
        // that it is signed.
        assert_eq!(admit(&f, mine, keys::COMMIT, &commit(2)), Ok(Admission::Dispatch));
    }

    #[test]
    fn pn23_the_owners_unsigned_commit_ends_the_transfer() {
        let f = claimed(0);
        let mine = Some(origin(OWN));
        for body in [vec![], vec![1, 2, 3]] {
            let r = admit(&f, mine, keys::COMMIT, &body);
            assert_eq!(r, Err(Refusal::UnsignedCommit));
            assert!(r.unwrap_err().abandons_transfer());
            assert_eq!(r.unwrap_err().error(), HandlerError::Denied);
        }
        // With no transfer in progress too.
        assert_eq!(admit(&f, None, keys::COMMIT, &[]), Err(Refusal::UnsignedCommit));
    }

    #[test]
    fn pn25_another_requesters_transfer_is_not_continued() {
        let f = claimed(0);
        for key in [keys::CHUNK, keys::COMMIT] {
            let r = admit(&f, Some(Origin::Serial), key, &commit(1));
            assert_eq!(r, Err(Refusal::NotOwner), "{key:?}");
            assert_eq!(r.unwrap_err().error(), HandlerError::InvalidRequest);
            assert!(!r.unwrap_err().abandons_transfer());
        }
        // A begin takes it over (the service then names the new owner).
        assert_eq!(admit(&f, Some(Origin::Serial), keys::BEGIN, &[0; 8]), Ok(Admission::Dispatch));
    }

    #[test]
    fn pn24_start_and_stop_are_signed_for_this_node_with_a_fresh_counter() {
        let f = claimed(100);
        for (key, op) in [(keys::START, ControlOp::Start), (keys::STOP, ControlOp::Stop)] {
            assert_eq!(admit(&f, None, key, &[]), Err(Refusal::UnsignedControl), "{key:?}");
            assert_eq!(admit(&f, None, key, &[0; 7]), Err(Refusal::UnsignedControl), "{key:?}");
            assert_eq!(
                admit(&f, None, key, &control(1, op, &OWN, 101)),
                Ok(Admission::Control { counter: ControlCounter(101) }),
                "{key:?}"
            );
            // Not above the last.
            assert_eq!(admit(&f, None, key, &control(1, op, &OWN, 100)), Err(Refusal::ControlRefused));
            // Another key.
            assert_eq!(admit(&f, None, key, &control(2, op, &OWN, 101)), Err(Refusal::ControlRefused));
            // The RT1050 rule's address: 02 and the node id's last five.
            let rt = [0x02, NODE.0[3], NODE.0[4], NODE.0[5], NODE.0[6], NODE.0[7]];
            assert_eq!(admit(&f, None, key, &control(1, op, &rt, 101)), Err(Refusal::ControlRefused));
        }
        // Another command.
        assert_eq!(admit(&f, None, keys::START, &control(1, ControlOp::Stop, &OWN, 101)), Err(Refusal::ControlRefused));
    }

    #[test]
    fn pn23_a_binding_must_be_signed() {
        let f = claimed(0);
        assert_eq!(admit(&f, None, keys::BIND, &bind_record()), Err(Refusal::UnsignedBinding));
        assert_eq!(admit(&f, None, keys::BIND, &[1, 2]), Err(Refusal::UnsignedBinding));
        assert_eq!(admit(&f, None, keys::BIND, &signed_bind()), Ok(Admission::Dispatch));
    }
}
