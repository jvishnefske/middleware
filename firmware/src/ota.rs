//! Over-the-air payload updates: which requests a radio peer may make, and
//! which requester owns a loader transfer.
//!
//! The radio shell (`src/shell/radio.rs`, feature `espnow`) answers
//! ESP-NOW requests (`middleware::rpc` frames addressed to this station)
//! by handing them to the controller like serial requests, tagged with
//! [`Origin::Air`] and the requester's MAC. Before dispatch the controller
//! asks [`admit`]:
//!
//! | request over the air      | unclaimed | claimed                         |
//! |---------------------------|-----------|---------------------------------|
//! | `sys/info`                | `Denied`  | allowed                         |
//! | `sys/trust/info`          | `Denied`  | allowed                         |
//! | `sys/loader/begin`        | `Denied`  | allowed                         |
//! | `sys/loader/chunk`        | `Denied`  | allowed for the transfer's owner |
//! | `sys/loader/commit`       | `Denied`  | signed body only (the dispatcher then applies `TrustState::accept_commit`: pinned key, version above the last); owner only. An unsigned one is `Denied` and ends the transfer ([`Refusal::abandons_transfer`]), as a refused commit does on serial |
//! | `sys/payload/start`, `stop` | `Denied` | allowed                        |
//! | `sys/trust/claim`         | `Denied`  | `Denied` (never over the air)   |
//! | any other key             | `Denied`  | `Denied`                        |
//!
//! An unclaimed device refuses the air entirely: trust on first use is for
//! the serial port, where someone holds the board. Without the `trust`
//! feature the dispatcher's trust state is always Unclaimed, so such a
//! build refuses every request over the air.
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
//! transfer. Both serial ports count as one requester ([`Origin::Serial`]),
//! as before; radio requesters are told apart by MAC.

use middleware::Key;
use middleware::loader::keys;
use middleware::loader::trust::{SignedCommit, TrustState};
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

/// Why a request is refused before dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Over the air to a device with no pinned key.
    Unclaimed,
    /// A key that never travels over the air (`sys/trust/claim`, anything
    /// not in [`AIR_KEYS`]).
    NotOverAir,
    /// An unsigned commit over the air.
    Unsigned,
    /// A chunk or commit from a requester that does not own the transfer.
    NotOwner,
}

impl Refusal {
    /// The status sent back.
    pub const fn error(self) -> HandlerError {
        match self {
            Refusal::Unclaimed | Refusal::NotOverAir | Refusal::Unsigned => HandlerError::Denied,
            Refusal::NotOwner => HandlerError::InvalidRequest,
        }
    }

    /// Whether the refusal ends the transfer in progress, as a commit the
    /// dispatcher refuses does: an unsigned commit over the air is the
    /// end of that upload. The other refusals leave the loader untouched
    /// (a stray from a non-owner must not break the owner's transfer).
    pub const fn abandons_transfer(self) -> bool {
        matches!(self, Refusal::Unsigned)
    }

    /// A short reason for the log.
    pub const fn reason(self) -> &'static str {
        match self {
            Refusal::Unclaimed => "device unclaimed; nothing is accepted over the air",
            Refusal::NotOverAir => "not allowed over the air",
            Refusal::Unsigned => "commit over the air must be signed",
            Refusal::NotOwner => "another requester owns the transfer",
        }
    }
}

/// Whether `key` continues a transfer (only its owner may send it).
fn continues_transfer(key: Key<'_>) -> bool {
    key == keys::CHUNK || key == keys::COMMIT
}

/// Decides whether a request on `key` with `body` from `origin` may be
/// dispatched, given the root of trust and the owner of the transfer in
/// progress (`None` when the loader is idle). See the module docs.
pub fn admit(
    trust: &TrustState,
    owner: Option<Origin>,
    origin: Origin,
    key: Key<'_>,
    body: &[u8],
) -> Result<(), Refusal> {
    if let Origin::Air(_) = origin {
        if !trust.is_claimed() {
            return Err(Refusal::Unclaimed);
        }
        if !AIR_KEYS.contains(&key) {
            return Err(Refusal::NotOverAir);
        }
        if key == keys::COMMIT && !matches!(SignedCommit::parse_body(body), Ok(Some(_))) {
            return Err(Refusal::Unsigned);
        }
    }
    match owner {
        Some(o) if o != origin && continues_transfer(key) => Err(Refusal::NotOwner),
        _ => Ok(()),
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
    use middleware::loader::trust::{PublicKey, SIGNED_COMMIT_LEN, Version};

    const HOST: Origin = Origin::Air([0x5c, 0xcf, 0x7f, 0x72, 0xf6, 0x54]);
    const OTHER: Origin = Origin::Air([2, 0, 0, 0, 0, 9]);

    fn claimed() -> TrustState {
        TrustState::Claimed { key: PublicKey([7; 32]), min_version: Version(10) }
    }

    fn k(s: &str) -> Key<'_> {
        Key::new(s).expect("valid")
    }

    #[test]
    fn o6_an_unclaimed_device_refuses_the_air_entirely() {
        for key in AIR_KEYS.iter().chain(&[keys::TRUST_CLAIM, k("app/x")]) {
            assert_eq!(admit(&TrustState::Unclaimed, None, HOST, *key, &[]), Err(Refusal::Unclaimed), "{key:?}");
            assert_eq!(admit(&TrustState::Unclaimed, None, Origin::Serial, *key, &[]), Ok(()), "serial: {key:?}");
        }
        assert_eq!(Refusal::Unclaimed.error(), HandlerError::Denied);
    }

    #[test]
    fn o6_a_claimed_device_allows_only_the_update_keys_over_the_air() {
        let t = claimed();
        let signed = [0u8; SIGNED_COMMIT_LEN];
        for key in AIR_KEYS {
            let body: &[u8] = if key == keys::COMMIT { &signed } else { &[] };
            assert_eq!(admit(&t, None, HOST, key, body), Ok(()), "{key:?}");
        }
        for key in [keys::TRUST_CLAIM, keys::LOG, keys::STATUS, k("sys/other"), k("app/x"), k("sys")] {
            assert_eq!(admit(&t, None, HOST, key, &[7; 32]), Err(Refusal::NotOverAir), "{key:?}");
        }
        assert_eq!(admit(&t, None, Origin::Serial, keys::TRUST_CLAIM, &[7; 32]), Ok(()), "serial may claim");
        assert_eq!(Refusal::NotOverAir.error(), HandlerError::Denied);
    }

    #[test]
    fn o6_a_commit_over_the_air_must_be_signed() {
        let t = claimed();
        assert_eq!(admit(&t, Some(HOST), HOST, keys::COMMIT, &[]), Err(Refusal::Unsigned));
        assert_eq!(admit(&t, Some(HOST), HOST, keys::COMMIT, &[0; 3]), Err(Refusal::Unsigned));
        assert_eq!(admit(&t, Some(HOST), HOST, keys::COMMIT, &[0; SIGNED_COMMIT_LEN]), Ok(()));
        // Serial keeps the dispatcher's rules (an unclaimed device accepts
        // an unsigned commit there).
        assert_eq!(admit(&TrustState::Unclaimed, Some(Origin::Serial), Origin::Serial, keys::COMMIT, &[]), Ok(()));
        assert_eq!(Refusal::Unsigned.error(), HandlerError::Denied);
        assert!(Refusal::Unsigned.reason().contains("signed"));
        assert!(Refusal::Unsigned.abandons_transfer());
        assert!(!Refusal::NotOwner.abandons_transfer() && !Refusal::Unclaimed.abandons_transfer());
    }

    #[test]
    fn o7_only_the_owner_continues_a_transfer() {
        let t = claimed();
        let signed = [0u8; SIGNED_COMMIT_LEN];
        assert_eq!(admit(&t, Some(HOST), OTHER, keys::CHUNK, &[0; 8]), Err(Refusal::NotOwner));
        assert_eq!(admit(&t, Some(HOST), Origin::Serial, keys::CHUNK, &[0; 8]), Err(Refusal::NotOwner));
        assert_eq!(admit(&t, Some(Origin::Serial), HOST, keys::COMMIT, &signed), Err(Refusal::NotOwner));
        assert_eq!(admit(&t, Some(HOST), HOST, keys::CHUNK, &[0; 8]), Ok(()));
        // Anyone may begin (taking the transfer over) or ask for info.
        assert_eq!(admit(&t, Some(HOST), Origin::Serial, keys::BEGIN, &[0; 8]), Ok(()));
        assert_eq!(admit(&t, Some(Origin::Serial), HOST, keys::BEGIN, &[0; 8]), Ok(()));
        assert_eq!(admit(&t, Some(HOST), OTHER, keys::INFO, &[]), Ok(()));
        // With no transfer in progress the loader itself refuses strays.
        assert_eq!(admit(&t, None, OTHER, keys::CHUNK, &[0; 8]), Ok(()));
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
        use crate::dispatch::{Context, State, handle_request};
        use ed25519_dalek::{Signer, SigningKey};
        use middleware::crc::Crc32;
        use middleware::loader::sha256::ImageHash;
        use middleware::loader::trust::{Signature, signed_message};
        use middleware::loader::{Loader, PayloadStatus};

        const CTX: Context<'static> =
            Context { chip: "esp32s3", firmware: "0.1.0", capacity: 4096, runtime: true, rekey_window: false };

        struct Dev {
            state: State,
            owner: Option<Origin>,
        }

        impl Dev {
            fn claimed() -> Self {
                let key = PublicKey(SigningKey::from_bytes(&[1; 32]).verifying_key().to_bytes());
                let trust = TrustState::Claimed { key, min_version: Version(0) };
                Dev { state: State::boot(false).with_trust(trust), owner: None }
            }

            fn request(&mut self, origin: Origin, key: Key<'_>, body: &[u8]) -> Result<(), HandlerError> {
                if let Err(r) = admit(&self.state.trust(), self.owner, origin, key, body) {
                if r.abandons_transfer() {
                    self.state = self.state.storage_failed();
                    self.owner = None;
                }
                return Err(r.error());
            }
                let mut out = [0u8; 128];
                let o = handle_request(self.state, &CTX, key, body, &mut out);
                self.state = o.state;
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
            // ...which ends the transfer, like a refused commit on serial.
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
            assert_eq!(d.request(HOST, keys::START, &[]), Ok(()));
        }

    }
}
