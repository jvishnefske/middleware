//! Signed payload updates with a root of trust pinned on first use.
//!
//! A device starts **Unclaimed**. The first `sys/trust/claim` pins an
//! Ed25519 public key (held by the browser or `mwctl`); from then on the
//! device is **Claimed** and installs a payload only when the commit is
//! signed by that key with a version strictly greater than the last one
//! installed. Re-pinning a key needs physical presence: the firmware
//! asserts presence (on the ESP base firmware, the BOOT button held for
//! [`PRESENCE_HOLD_MS`] after boot), which opens a re-key [`Window`] of
//! [`REKEY_WINDOW_MS`] during which one claim is accepted.
//!
//! # Wire format
//!
//! | key                 | request body                                 | response body |
//! |---------------------|----------------------------------------------|---------------|
//! | `sys/trust/info`    | empty                                        | [`TrustInfo`]: `[state u8][min_version u32][key 32, iff state != 0]` |
//! | `sys/trust/claim`   | `[public key 32]`                            | empty         |
//! | `sys/loader/commit` | empty, or [`SignedCommit`]: `[version u32][signature 64]` | empty |
//!
//! Any other body length is `InvalidRequest`. `state` is 0 Unclaimed,
//! 1 Claimed, 2 Claimed with the re-key window open ([`TrustCode`]).
//!
//! # What is signed
//!
//! Ed25519 (RFC 8032, pure, no prehash) over the 54-byte
//! [`signed_message`]:
//! `b"mw-payload-v1\0" || len u32 LE || version u32 LE || sha256(image)`.
//! The device hashes the image while chunks stream in (the loader enforces
//! their order), so nothing is read back for the check.
//!
//! # Rules ([`TrustState`])
//!
//! * [`TrustState::claim`]: accepted when Unclaimed, or when Claimed and
//!   the re-key window is open; it stores the key and resets
//!   `min_version` to 0. Otherwise `Denied`. A key that is not a valid
//!   Ed25519 point is `InvalidRequest`.
//! * [`TrustState::accept_commit`]: when Claimed, the commit must be
//!   signed, verify (`verify_strict`) against the pinned key, and carry
//!   `version > min_version`; the new state has `min_version = version`.
//!   Any failure is `Denied`. When Unclaimed every commit is accepted and
//!   any signature ignored: trust on first use, so an unclaimed device is
//!   as open as one without this feature.
//!
//! # Signed control requests
//!
//! Over a radio anyone can send a request with any source address, so a
//! claimed device takes `sys/payload/start` and `sys/payload/stop` over
//! the air only with a [`SignedControl`] body,
//! `[counter u64 LE][signature 64]` (72 bytes): Ed25519 by the pinned key
//! over the 29-byte [`control_message`]
//! `b"mw-control-v1\0" || op u8 || target MAC 6 || counter u64 LE`, where
//! `op` is [`ControlOp`] (1 start, 2 stop) and the target is the device's
//! own station MAC. [`TrustState::accept_control`] accepts it only when the
//! signature verifies and `counter` is strictly greater than the last
//! accepted [`ControlCounter`], which the firmware persists with the trust
//! record before acting: a captured request cannot be replayed (same or
//! lower counter), redirected to another device (other MAC) or turned
//! into the other command (other `op`). Hosts use the Unix time in
//! milliseconds as the counter. Over a serial port these keys keep their
//! empty bodies (physical access is the authority there).
//!
//! The value types and the codecs are always built (hosts encode requests
//! with them); the transitions that need cryptography, [`TrustState::claim`],
//! [`TrustState::accept_commit`] and [`TrustState::accept_control`], need
//! the `trust` feature. Everything here is pure: the firmware persists the
//! state and asserts presence.

use crate::transport::HandlerError;

/// Domain-separation prefix of the signed message.
pub const MESSAGE_TAG: &[u8; 14] = b"mw-payload-v1\0";
/// Length of [`signed_message`].
pub const MESSAGE_LEN: usize = 54;
/// Ed25519 public key length.
pub const KEY_LEN: usize = 32;
/// Ed25519 signature length.
pub const SIGNATURE_LEN: usize = 64;
/// Length of a signed `sys/loader/commit` body.
pub const SIGNED_COMMIT_LEN: usize = 4 + SIGNATURE_LEN;
/// How long the BOOT button must be held to assert presence.
pub const PRESENCE_HOLD_MS: u64 = 3_000;
/// How long the re-key window stays open after presence is asserted.
pub const REKEY_WINDOW_MS: u64 = 60_000;
/// Longest `sys/trust/info` response.
pub const INFO_MAX: usize = 5 + KEY_LEN;
/// Domain-separation prefix of a signed control request.
pub const CONTROL_TAG: &[u8; 14] = b"mw-control-v1\0";
/// Length of a control request's target: the device's station MAC.
pub const TARGET_LEN: usize = 6;
/// Length of [`control_message`].
pub const CONTROL_MESSAGE_LEN: usize = CONTROL_TAG.len() + 1 + TARGET_LEN + 8;
/// Length of a signed `sys/payload/start` or `stop` body.
pub const SIGNED_CONTROL_LEN: usize = 8 + SIGNATURE_LEN;

/// An Ed25519 public key (compressed Edwards point).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicKey(pub [u8; KEY_LEN]);

/// An Ed25519 signature `R || S`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Signature(pub [u8; SIGNATURE_LEN]);

/// A payload version: strictly increasing per pinned key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Version(pub u32);

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    let b = bytes.get(at..at + 4)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

impl PublicKey {
    /// Parses a `sys/trust/claim` body: exactly [`KEY_LEN`] bytes.
    pub fn parse_claim(body: &[u8]) -> Result<Self, HandlerError> {
        <[u8; KEY_LEN]>::try_from(body).map(PublicKey).map_err(|_| HandlerError::InvalidRequest)
    }
}

/// The authorisation carried by a signed `sys/loader/commit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignedCommit {
    /// Version of the image being committed.
    pub version: Version,
    /// Signature over [`signed_message`].
    pub signature: Signature,
}

impl SignedCommit {
    /// Parses a commit body: empty is an unsigned commit (`None`),
    /// [`SIGNED_COMMIT_LEN`] bytes a signed one; anything else is
    /// `InvalidRequest`.
    pub fn parse_body(body: &[u8]) -> Result<Option<Self>, HandlerError> {
        if body.is_empty() {
            return Ok(None);
        }
        if body.len() != SIGNED_COMMIT_LEN {
            return Err(HandlerError::InvalidRequest);
        }
        let version = u32_at(body, 0).ok_or(HandlerError::InvalidRequest)?;
        let signature = body
            .get(4..)
            .and_then(|s| <[u8; SIGNATURE_LEN]>::try_from(s).ok())
            .ok_or(HandlerError::InvalidRequest)?;
        Ok(Some(SignedCommit { version: Version(version), signature: Signature(signature) }))
    }

    /// The commit body `[version u32 LE][signature 64]`.
    pub fn encode(&self) -> [u8; SIGNED_COMMIT_LEN] {
        let mut out = [0u8; SIGNED_COMMIT_LEN];
        let (v, s) = out.split_at_mut(4);
        v.copy_from_slice(&self.version.0.to_le_bytes());
        s.copy_from_slice(&self.signature.0);
        out
    }
}

/// The message a payload signature covers:
/// `MESSAGE_TAG || len u32 LE || version u32 LE || sha256` (54 bytes).
pub fn signed_message(len: u32, version: Version, sha256: &[u8; 32]) -> [u8; MESSAGE_LEN] {
    let mut m = [0u8; MESSAGE_LEN];
    let (tag, rest) = m.split_at_mut(MESSAGE_TAG.len());
    tag.copy_from_slice(MESSAGE_TAG);
    let (l, rest) = rest.split_at_mut(4);
    l.copy_from_slice(&len.to_le_bytes());
    let (v, digest) = rest.split_at_mut(4);
    v.copy_from_slice(&version.0.to_le_bytes());
    digest.copy_from_slice(sha256);
    m
}

/// A control request's replay counter: strictly increasing per device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct ControlCounter(pub u64);

/// What a signed control request asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ControlOp {
    /// `sys/payload/start`.
    Start = 1,
    /// `sys/payload/stop`.
    Stop = 2,
}

/// The authorisation carried by a signed `sys/payload/start` or `stop`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignedControl {
    /// Replay counter; must exceed the last one the device accepted.
    pub counter: ControlCounter,
    /// Signature over [`control_message`].
    pub signature: Signature,
}

impl SignedControl {
    /// Parses a start or stop body: empty is unsigned (`None`),
    /// [`SIGNED_CONTROL_LEN`] bytes a signed request; anything else is
    /// `InvalidRequest`.
    pub fn parse_body(body: &[u8]) -> Result<Option<Self>, HandlerError> {
        if body.is_empty() {
            return Ok(None);
        }
        let bad = HandlerError::InvalidRequest;
        if body.len() != SIGNED_CONTROL_LEN {
            return Err(bad);
        }
        let counter = body.get(..8).and_then(|c| <[u8; 8]>::try_from(c).ok()).ok_or(bad)?;
        let signature = body
            .get(8..)
            .and_then(|s| <[u8; SIGNATURE_LEN]>::try_from(s).ok())
            .ok_or(bad)?;
        Ok(Some(SignedControl {
            counter: ControlCounter(u64::from_le_bytes(counter)),
            signature: Signature(signature),
        }))
    }

    /// The body `[counter u64 LE][signature 64]`.
    pub fn encode(&self) -> [u8; SIGNED_CONTROL_LEN] {
        let mut out = [0u8; SIGNED_CONTROL_LEN];
        let (c, s) = out.split_at_mut(8);
        c.copy_from_slice(&self.counter.0.to_le_bytes());
        s.copy_from_slice(&self.signature.0);
        out
    }
}

/// The message a control signature covers:
/// `CONTROL_TAG || op u8 || target MAC 6 || counter u64 LE` (29 bytes).
pub fn control_message(
    op: ControlOp,
    target: &[u8; TARGET_LEN],
    counter: ControlCounter,
) -> [u8; CONTROL_MESSAGE_LEN] {
    let mut m = [0u8; CONTROL_MESSAGE_LEN];
    let (tag, rest) = m.split_at_mut(CONTROL_TAG.len());
    tag.copy_from_slice(CONTROL_TAG);
    let (o, rest) = rest.split_at_mut(1);
    o[0] = op as u8;
    let (t, c) = rest.split_at_mut(TARGET_LEN);
    t.copy_from_slice(target);
    c.copy_from_slice(&counter.0.to_le_bytes());
    m
}

/// The re-key window: open for [`REKEY_WINDOW_MS`] after presence, on a
/// monotonic millisecond clock (milliseconds since boot).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Window {
    /// No re-key allowed.
    #[default]
    Closed,
    /// A claim is allowed before `until_ms`.
    Open {
        /// First instant (ms) at which the window is closed again.
        until_ms: u64,
    },
}

impl Window {
    /// Presence asserted at `now_ms`: open until `now_ms + REKEY_WINDOW_MS`.
    pub const fn opened_at(now_ms: u64) -> Self {
        Window::Open { until_ms: now_ms.saturating_add(REKEY_WINDOW_MS) }
    }

    /// Whether a claim is allowed at `now_ms`.
    pub const fn is_open(&self, now_ms: u64) -> bool {
        match self {
            Window::Closed => false,
            Window::Open { until_ms } => now_ms < *until_ms,
        }
    }

    /// `Closed` once `now_ms` reaches the end of the window, else `self`.
    pub const fn expire(self, now_ms: u64) -> Self {
        if self.is_open(now_ms) { self } else { Window::Closed }
    }
}

/// The persisted root of trust.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrustState {
    /// No key pinned: every commit is accepted (trust on first use).
    #[default]
    Unclaimed,
    /// A key is pinned.
    Claimed {
        /// The pinned key.
        key: PublicKey,
        /// Version of the last signed image installed (0 after a claim); the
        /// next must be strictly greater.
        min_version: Version,
    },
}

/// `state` byte of `sys/trust/info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TrustCode {
    /// No key pinned.
    Unclaimed = 0,
    /// A key is pinned.
    Claimed = 1,
    /// A key is pinned and the re-key window is open.
    RekeyOpen = 2,
}

impl TrustCode {
    /// Parses the state byte.
    pub const fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(TrustCode::Unclaimed),
            1 => Some(TrustCode::Claimed),
            2 => Some(TrustCode::RekeyOpen),
            _ => None,
        }
    }
}

/// `sys/trust/info` response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustInfo {
    /// Persisted state.
    pub state: TrustState,
    /// Whether the re-key window is open (reported only when Claimed).
    pub rekey_open: bool,
}

impl TrustInfo {
    /// The wire state byte.
    pub const fn code(&self) -> TrustCode {
        match (self.state, self.rekey_open) {
            (TrustState::Unclaimed, _) => TrustCode::Unclaimed,
            (TrustState::Claimed { .. }, false) => TrustCode::Claimed,
            (TrustState::Claimed { .. }, true) => TrustCode::RekeyOpen,
        }
    }

    /// Encodes `[state][min_version u32 LE][key 32 iff state != 0]` into
    /// `out`; `None` if `out` is too small. Unclaimed reports
    /// `min_version` 0.
    pub fn encode(&self, out: &mut [u8]) -> Option<usize> {
        let (min, key) = match self.state {
            TrustState::Unclaimed => (0, None),
            TrustState::Claimed { key, min_version } => (min_version.0, Some(key)),
        };
        *out.first_mut()? = self.code() as u8;
        out.get_mut(1..5)?.copy_from_slice(&min.to_le_bytes());
        match key {
            None => Some(5),
            Some(k) => {
                out.get_mut(5..5 + KEY_LEN)?.copy_from_slice(&k.0);
                Some(5 + KEY_LEN)
            }
        }
    }

    /// Decodes a `sys/trust/info` response; the length must match the state.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let code = TrustCode::from_byte(*bytes.first()?)?;
        let min_version = Version(u32_at(bytes, 1)?);
        match code {
            TrustCode::Unclaimed if bytes.len() == 5 => {
                Some(TrustInfo { state: TrustState::Unclaimed, rekey_open: false })
            }
            TrustCode::Claimed | TrustCode::RekeyOpen if bytes.len() == 5 + KEY_LEN => {
                let key = PublicKey(<[u8; KEY_LEN]>::try_from(bytes.get(5..)?).ok()?);
                Some(TrustInfo {
                    state: TrustState::Claimed { key, min_version },
                    rekey_open: code == TrustCode::RekeyOpen,
                })
            }
            _ => None,
        }
    }
}

impl TrustState {
    /// The `sys/trust/info` view of this state.
    pub const fn info(&self, rekey_open: bool) -> TrustInfo {
        TrustInfo { state: *self, rekey_open }
    }

    /// Whether a key is pinned.
    pub const fn is_claimed(&self) -> bool {
        matches!(self, TrustState::Claimed { .. })
    }
}

#[cfg(feature = "trust")]
mod verify {
    use super::*;
    use crate::loader::ImageDigest;
    use ed25519_dalek::VerifyingKey;

    fn verifying_key(key: &PublicKey) -> Option<VerifyingKey> {
        VerifyingKey::from_bytes(&key.0).ok()
    }

    impl TrustState {
        /// `sys/trust/claim` with `key`, given whether the re-key window is
        /// open now. Accepted when Unclaimed or when the window is open: the
        /// key is pinned with `min_version` 0 (the caller then closes the
        /// window). Refused with `Denied` otherwise; a key that does not
        /// decode as an Ed25519 point is `InvalidRequest`.
        pub fn claim(self, key: PublicKey, rekey_open: bool) -> Result<TrustState, HandlerError> {
            if verifying_key(&key).is_none() {
                return Err(HandlerError::InvalidRequest);
            }
            match self {
                TrustState::Claimed { .. } if !rekey_open => Err(HandlerError::Denied),
                _ => Ok(TrustState::Claimed { key, min_version: Version(0) }),
            }
        }

        /// Decides a `sys/loader/commit` of the completed `image`. Unclaimed:
        /// accepted unchanged (TOFU; `auth` ignored). Claimed: `auth` must be
        /// present, have `version > min_version` and a signature over
        /// [`signed_message`] that verifies (strictly) against the pinned
        /// key; the result has `min_version = version`. Every failure is
        /// `Denied`.
        pub fn accept_commit(
            self,
            image: &ImageDigest,
            auth: Option<&SignedCommit>,
        ) -> Result<TrustState, HandlerError> {
            let TrustState::Claimed { key, min_version } = self else {
                return Ok(self);
            };
            let denied = HandlerError::Denied;
            let auth = auth.ok_or(denied)?;
            if auth.version <= min_version {
                return Err(denied);
            }
            let vk = verifying_key(&key).ok_or(denied)?;
            let message = signed_message(image.len, auth.version, &image.sha256);
            let sig = ed25519_dalek::Signature::from_bytes(&auth.signature.0);
            vk.verify_strict(&message, &sig).map_err(|_| denied)?;
            Ok(TrustState::Claimed { key, min_version: auth.version })
        }

        /// Decides a signed control request `op` addressed to `target`
        /// (the device's own MAC), given the `last` counter accepted.
        /// Accepted only when Claimed, `auth.counter > last` and the
        /// signature over [`control_message`] verifies (strictly) against
        /// the pinned key; returns the counter to persist before acting.
        /// Every failure, and any request to an unclaimed device (no key
        /// to check against), is `Denied`.
        pub fn accept_control(
            &self,
            last: ControlCounter,
            op: ControlOp,
            target: &[u8; TARGET_LEN],
            auth: &SignedControl,
        ) -> Result<ControlCounter, HandlerError> {
            let denied = HandlerError::Denied;
            let TrustState::Claimed { key, .. } = self else {
                return Err(denied);
            };
            if auth.counter <= last {
                return Err(denied);
            }
            let vk = verifying_key(key).ok_or(denied)?;
            let message = control_message(op, target, auth.counter);
            let sig = ed25519_dalek::Signature::from_bytes(&auth.signature.0);
            vk.verify_strict(&message, &sig).map_err(|_| denied)?;
            Ok(auth.counter)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `web/test/fixtures/trust-vector.json`, the golden vector shared with
    /// the browser (TEST KEY ONLY: seed 00..1f).
    #[cfg_attr(not(feature = "trust"), allow(dead_code))]
    pub(crate) mod vector {
        pub const SEED: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
        pub const PUBLIC_KEY: &str = "03a107bff3ce10be1d70dd18e74bc09967e4d6309ba50d5f1ddc8664125531b8";
        pub const IMAGE_LEN: u32 = 1000;
        pub const VERSION: u32 = 1_761_500_000;
        pub const SHA256: &str = "1e9bc38cbf860b9ec31918b065f9b52476c549a782e0e7990bed8ce3868d2371";
        pub const MESSAGE: &str = "6d772d7061796c6f61642d763100e8030000605bfe681e9bc38cbf860b9ec31918b065f9b52476c549a782e0e7990bed8ce3868d2371";
        pub const SIGNATURE: &str = "ca90e0aef84dfa5173aed3af1738933d2cebbd3f74012fe398847dad9db8b092fe91b9beaa7b59c6665f41b3abd352ddf890668cf994125438aa084c6a7b9f07";
        pub const COMMIT_BODY: &str = "605bfe68ca90e0aef84dfa5173aed3af1738933d2cebbd3f74012fe398847dad9db8b092fe91b9beaa7b59c6665f41b3abd352ddf890668cf994125438aa084c6a7b9f07";

        /// Image byte `i` is `(i * 7 + 3) & 0xff`.
        pub fn image() -> [u8; IMAGE_LEN as usize] {
            core::array::from_fn(|i| (i * 7 + 3) as u8)
        }

        /// Decodes hex into `N` bytes.
        pub fn hex<const N: usize>(s: &str) -> [u8; N] {
            assert_eq!(s.len(), 2 * N, "hex length");
            core::array::from_fn(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
        }
    }

    #[test]
    fn signed_message_matches_golden_vector() {
        let sha = vector::hex::<32>(vector::SHA256);
        let m = signed_message(vector::IMAGE_LEN, Version(vector::VERSION), &sha);
        assert_eq!(m, vector::hex::<MESSAGE_LEN>(vector::MESSAGE));
        assert_eq!(&m[..14], MESSAGE_TAG);
    }

    #[test]
    fn commit_bodies_parse_and_encode() {
        assert_eq!(SignedCommit::parse_body(&[]), Ok(None));
        let body = vector::hex::<SIGNED_COMMIT_LEN>(vector::COMMIT_BODY);
        let c = SignedCommit::parse_body(&body).unwrap().unwrap();
        assert_eq!(c.version, Version(vector::VERSION));
        assert_eq!(c.signature.0, vector::hex::<64>(vector::SIGNATURE));
        assert_eq!(c.encode(), body);
        for len in [1, 4, 64, 67, 69] {
            assert_eq!(
                SignedCommit::parse_body(&[0; 70][..len]),
                Err(HandlerError::InvalidRequest),
                "len {len}"
            );
        }
    }

    #[test]
    fn claim_bodies_must_be_32_bytes() {
        assert_eq!(PublicKey::parse_claim(&[7; 32]), Ok(PublicKey([7; 32])));
        assert_eq!(PublicKey::parse_claim(&[7; 31]), Err(HandlerError::InvalidRequest));
        assert_eq!(PublicKey::parse_claim(&[7; 33]), Err(HandlerError::InvalidRequest));
        assert_eq!(PublicKey::parse_claim(&[]), Err(HandlerError::InvalidRequest));
    }

    #[test]
    fn window_opens_for_sixty_seconds() {
        let w = Window::opened_at(1_000);
        assert!(w.is_open(1_000));
        assert!(w.is_open(60_999));
        assert!(!w.is_open(61_000));
        assert_eq!(w.expire(30_000), w);
        assert_eq!(w.expire(61_000), Window::Closed);
        assert!(!Window::Closed.is_open(0));
        assert!(Window::opened_at(u64::MAX).is_open(u64::MAX - 1));
    }

    #[test]
    fn trust_info_round_trips_every_state() {
        let key = PublicKey([9; 32]);
        let claimed = TrustState::Claimed { key, min_version: Version(77) };
        let mut buf = [0u8; INFO_MAX];
        for (info, code, len) in [
            (TrustState::Unclaimed.info(false), TrustCode::Unclaimed, 5),
            (TrustState::Unclaimed.info(true), TrustCode::Unclaimed, 5),
            (claimed.info(false), TrustCode::Claimed, 37),
            (claimed.info(true), TrustCode::RekeyOpen, 37),
        ] {
            assert_eq!(info.code(), code);
            let n = info.encode(&mut buf).unwrap();
            assert_eq!(n, len);
            assert_eq!(buf[0], code as u8);
            let back = TrustInfo::decode(&buf[..n]).unwrap();
            assert_eq!(back.state, info.state);
            assert_eq!(back.code(), code);
        }
        let n = claimed.info(false).encode(&mut buf).unwrap();
        assert_eq!(&buf[1..5], &77u32.to_le_bytes());
        assert_eq!(&buf[5..n], &[9; 32]);
        assert!(claimed.info(false).encode(&mut buf[..36]).is_none());
        assert!(TrustInfo::decode(&buf[..36]).is_none());
        assert!(TrustInfo::decode(&[0, 0, 0, 0]).is_none());
        assert!(TrustInfo::decode(&[3, 0, 0, 0, 0]).is_none());
        assert!(TrustInfo::decode(&[0; 37]).is_none(), "unclaimed carries no key");
        assert!(TrustInfo::decode(&[1, 0, 0, 0, 0]).is_none(), "claimed carries a key");
        assert!(!TrustState::Unclaimed.is_claimed());
        assert!(claimed.is_claimed());
    }

    /// Control-request golden vector (TEST KEY ONLY: seed 00..1f), shared
    /// with mwctl's `sign` tests.
    #[cfg_attr(not(feature = "trust"), allow(dead_code))]
    pub(crate) mod control_vector {
        pub const TARGET: [u8; 6] = [0xe4, 0xb0, 0x63, 0x81, 0xd3, 0x24];
        pub const COUNTER: u64 = 1_761_500_000_123;
        pub const MESSAGE: &str = "6d772d636f6e74726f6c2d76310002e4b06381d3247bef94219a010000";
        pub const SIGNATURE: &str = "aeffa9bd89e94d48b5577de27cb123b6cdd6450b3e0cf2f6c13eb07827d4938a345233eb01f44746c8c56c056c6b0cbd62544876010bc75352efaab0636b2b01";
    }

    #[test]
    fn control_message_and_bodies_have_the_documented_layout() {
        let m = control_message(ControlOp::Stop, &control_vector::TARGET, ControlCounter(control_vector::COUNTER));
        assert_eq!(m.len(), 29);
        assert_eq!(&m[..14], CONTROL_TAG);
        assert_eq!(m[14], 2);
        assert_eq!(&m[15..21], &control_vector::TARGET);
        assert_eq!(&m[21..], &control_vector::COUNTER.to_le_bytes());
        assert_eq!(control_message(ControlOp::Start, &[0; 6], ControlCounter(0))[14], 1);
        assert_ne!(&CONTROL_TAG[..], &MESSAGE_TAG[..], "control and payload signatures never mix");

        assert_eq!(SignedControl::parse_body(&[]), Ok(None));
        let c = SignedControl { counter: ControlCounter(0x0102_0304_0506_0708), signature: Signature([9; 64]) };
        let body = c.encode();
        assert_eq!(body.len(), SIGNED_CONTROL_LEN);
        assert_eq!(&body[..8], &[8, 7, 6, 5, 4, 3, 2, 1]);
        assert_eq!(&body[8..], &[9; 64]);
        assert_eq!(SignedControl::parse_body(&body), Ok(Some(c)));
        for len in [1, 8, 64, 68, 71, 73] {
            assert_eq!(
                SignedControl::parse_body(&[0; 80][..len]),
                Err(HandlerError::InvalidRequest),
                "len {len}"
            );
        }
    }

    #[cfg(feature = "trust")]
    mod crypto {
        use super::super::*;
        use super::vector;
        use crate::loader::sha256::ImageHash;
        use crate::loader::ImageDigest;
        use ed25519_dalek::{Signer, SigningKey};

        fn signer() -> SigningKey {
            SigningKey::from_bytes(&vector::hex::<32>(vector::SEED))
        }

        fn key_of(s: &SigningKey) -> PublicKey {
            PublicKey(s.verifying_key().to_bytes())
        }

        fn digest(image: &[u8]) -> ImageDigest {
            ImageDigest {
                len: image.len() as u32,
                crc: crate::crc::Crc32::checksum(image),
                sha256: ImageHash::digest(image),
            }
        }

        fn sign(s: &SigningKey, image: &ImageDigest, version: u32) -> SignedCommit {
            let m = signed_message(image.len, Version(version), &image.sha256);
            SignedCommit { version: Version(version), signature: Signature(s.sign(&m).to_bytes()) }
        }

        fn claimed(min: u32) -> TrustState {
            TrustState::Claimed { key: key_of(&signer()), min_version: Version(min) }
        }

        #[test]
        fn golden_vector_verifies() {
            let s = signer();
            assert_eq!(key_of(&s).0, vector::hex::<32>(vector::PUBLIC_KEY));
            let image = vector::image();
            let d = digest(&image);
            assert_eq!(d.sha256, vector::hex::<32>(vector::SHA256));
            let body = vector::hex::<SIGNED_COMMIT_LEN>(vector::COMMIT_BODY);
            let auth = SignedCommit::parse_body(&body).unwrap().unwrap();
            assert_eq!(sign(&s, &d, vector::VERSION), auth, "Ed25519 is deterministic");
            assert_eq!(
                claimed(0).accept_commit(&d, Some(&auth)),
                Ok(claimed(vector::VERSION)),
            );
        }

        #[test]
        fn claim_transitions() {
            let k = key_of(&signer());
            let other = key_of(&SigningKey::from_bytes(&[42; 32]));
            let first = TrustState::Unclaimed.claim(k, false).unwrap();
            assert_eq!(first, TrustState::Claimed { key: k, min_version: Version(0) });
            assert_eq!(TrustState::Unclaimed.claim(k, true), Ok(first));
            // Claimed, window closed: refused, even for the same key.
            assert_eq!(claimed(5).claim(other, false), Err(HandlerError::Denied));
            assert_eq!(claimed(5).claim(k, false), Err(HandlerError::Denied));
            // Window open: re-key, min_version reset.
            assert_eq!(
                claimed(5).claim(other, true),
                Ok(TrustState::Claimed { key: other, min_version: Version(0) })
            );
            // Not a curve point (y = 2 has no x on edwards25519).
            let mut bad = [0u8; 32];
            bad[0] = 2;
            assert_eq!(
                TrustState::Unclaimed.claim(PublicKey(bad), false),
                Err(HandlerError::InvalidRequest)
            );
        }

        #[test]
        fn unclaimed_accepts_any_commit() {
            let d = digest(b"anything");
            assert_eq!(TrustState::Unclaimed.accept_commit(&d, None), Ok(TrustState::Unclaimed));
            let junk = SignedCommit { version: Version(1), signature: Signature([0; 64]) };
            assert_eq!(
                TrustState::Unclaimed.accept_commit(&d, Some(&junk)),
                Ok(TrustState::Unclaimed),
                "the signature is ignored, not checked"
            );
        }

        #[test]
        fn claimed_commit_denials() {
            let s = signer();
            let image = vector::image();
            let d = digest(&image);
            let denied = Err(HandlerError::Denied);
            // Unsigned.
            assert_eq!(claimed(0).accept_commit(&d, None), denied);
            // Replay of the same version, and rollback.
            let v10 = sign(&s, &d, 10);
            assert_eq!(claimed(0).accept_commit(&d, Some(&v10)), Ok(claimed(10)));
            assert_eq!(claimed(10).accept_commit(&d, Some(&v10)), denied);
            assert_eq!(claimed(11).accept_commit(&d, Some(&v10)), denied);
            assert_eq!(claimed(9).accept_commit(&d, Some(&v10)), Ok(claimed(10)));
            // Signed by another key.
            let mallory = SigningKey::from_bytes(&[42; 32]);
            assert_eq!(claimed(0).accept_commit(&d, Some(&sign(&mallory, &d, 10))), denied);
            // Version field altered after signing.
            let bumped = SignedCommit { version: Version(11), ..v10 };
            assert_eq!(claimed(0).accept_commit(&d, Some(&bumped)), denied);
            // Different image, length or hash.
            let mut other = image;
            other[500] ^= 1;
            assert_eq!(claimed(0).accept_commit(&digest(&other), Some(&v10)), denied);
            let short = ImageDigest { len: d.len - 1, ..d };
            assert_eq!(claimed(0).accept_commit(&short, Some(&v10)), denied);
            // Corrupt signature.
            let mut sig = v10.signature.0;
            sig[3] ^= 0x10;
            let broken = SignedCommit { signature: Signature(sig), ..v10 };
            assert_eq!(claimed(0).accept_commit(&d, Some(&broken)), denied);
            // Non-canonical S (S + l) is refused by verify_strict.
            let mut malleable = v10.signature.0;
            add_group_order(&mut malleable[32..]);
            let m = SignedCommit { signature: Signature(malleable), ..v10 };
            assert_eq!(claimed(0).accept_commit(&d, Some(&m)), denied);
            // A pinned key that is not a valid point can verify nothing.
            let mut bad = [0u8; 32];
            bad[0] = 2;
            let broken_key = TrustState::Claimed { key: PublicKey(bad), min_version: Version(0) };
            assert_eq!(broken_key.accept_commit(&d, Some(&v10)), denied);
        }

        fn sign_control(s: &SigningKey, op: ControlOp, target: &[u8; 6], counter: u64) -> SignedControl {
            let m = control_message(op, target, ControlCounter(counter));
            SignedControl { counter: ControlCounter(counter), signature: Signature(s.sign(&m).to_bytes()) }
        }

        #[test]
        fn control_golden_vector_verifies() {
            use super::control_vector as v;
            let s = signer();
            let auth = sign_control(&s, ControlOp::Stop, &v::TARGET, v::COUNTER);
            let m = control_message(ControlOp::Stop, &v::TARGET, ControlCounter(v::COUNTER));
            assert_eq!(m, vector::hex::<CONTROL_MESSAGE_LEN>(v::MESSAGE));
            assert_eq!(auth.signature.0, vector::hex::<64>(v::SIGNATURE));
            assert_eq!(
                claimed(5).accept_control(ControlCounter(v::COUNTER - 1), ControlOp::Stop, &v::TARGET, &auth),
                Ok(ControlCounter(v::COUNTER))
            );
        }

        #[test]
        fn control_requests_need_the_pinned_key_this_device_this_op_and_a_new_counter() {
            let s = signer();
            let me = [1, 2, 3, 4, 5, 6];
            let denied = Err(HandlerError::Denied);
            let t = claimed(0);
            let ok = sign_control(&s, ControlOp::Start, &me, 10);
            assert_eq!(t.accept_control(ControlCounter(0), ControlOp::Start, &me, &ok), Ok(ControlCounter(10)));
            assert_eq!(t.accept_control(ControlCounter(9), ControlOp::Start, &me, &ok), Ok(ControlCounter(10)));
            // Replay (same counter) and older counters.
            assert_eq!(t.accept_control(ControlCounter(10), ControlOp::Start, &me, &ok), denied);
            assert_eq!(t.accept_control(ControlCounter(11), ControlOp::Start, &me, &ok), denied);
            // Another device, the other command, a bumped counter.
            assert_eq!(t.accept_control(ControlCounter(0), ControlOp::Start, &[1, 2, 3, 4, 5, 7], &ok), denied);
            assert_eq!(t.accept_control(ControlCounter(0), ControlOp::Stop, &me, &ok), denied);
            let bumped = SignedControl { counter: ControlCounter(11), ..ok };
            assert_eq!(t.accept_control(ControlCounter(0), ControlOp::Start, &me, &bumped), denied);
            // Another key, a corrupt or non-canonical signature.
            let mallory = SigningKey::from_bytes(&[42; 32]);
            let forged = sign_control(&mallory, ControlOp::Start, &me, 10);
            assert_eq!(t.accept_control(ControlCounter(0), ControlOp::Start, &me, &forged), denied);
            let mut sig = ok.signature.0;
            sig[40] ^= 1;
            let broken = SignedControl { signature: Signature(sig), ..ok };
            assert_eq!(t.accept_control(ControlCounter(0), ControlOp::Start, &me, &broken), denied);
            let mut malleable = ok.signature.0;
            add_group_order(&mut malleable[32..]);
            let m = SignedControl { signature: Signature(malleable), ..ok };
            assert_eq!(t.accept_control(ControlCounter(0), ControlOp::Start, &me, &m), denied);
            // A payload signature is never a control signature.
            let commit_sig = sign(&s, &digest(b"x"), 10).signature;
            let cross = SignedControl { counter: ControlCounter(10), signature: commit_sig };
            assert_eq!(t.accept_control(ControlCounter(0), ControlOp::Start, &me, &cross), denied);
            // Unclaimed: nothing to verify against.
            assert_eq!(TrustState::Unclaimed.accept_control(ControlCounter(0), ControlOp::Start, &me, &ok), denied);
        }

        /// Adds the group order l to a little-endian scalar in place.
        fn add_group_order(s: &mut [u8]) {
            const L: [u8; 32] = [
                0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9,
                0xde, 0x14, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10,
            ];
            let mut carry = 0u16;
            for (b, l) in s.iter_mut().zip(L) {
                let sum = u16::from(*b) + u16::from(l) + carry;
                *b = sum as u8;
                carry = sum >> 8;
            }
        }
    }
}
