//! Payload signing (pure): key seeds, public keys and signed commits.
//!
//! A signing key file holds the raw 32-byte Ed25519 seed (RFC 8032 private
//! key), nothing else. [`sign_commit`] signs the device's 54-byte message
//! [`signed_message`] (`b"mw-payload-v1\0" || len || version || sha256`)
//! for one image and returns the [`SignedCommit`] that becomes the 68-byte
//! `sys/loader/commit` body. The default version is the current Unix time
//! in seconds ([`version_at`]), which increases with every upload and fits
//! a `u32` until 2106.
//!
//! Signing is deterministic (Ed25519), so the golden vector shared with the
//! browser (`web/test/fixtures/trust-vector.json`) is reproduced exactly.
//!
//! [`sign_control`] signs a control request for `start`/`stop` over
//! ESP-NOW: the 29-byte [`control_message`]
//! (`b"mw-control-v1\0" || op || target MAC || counter u64`) for one
//! device, returned as the [`SignedControl`] that becomes the 72-byte
//! request body. The default counter is the current Unix time in
//! milliseconds ([`counter_at`]); the device accepts only a counter above
//! the last one it accepted, so a request is never accepted twice.

use ed25519_dalek::{Signer, SigningKey};
use middleware::loader::trust::{
    ControlCounter, ControlOp, PublicKey, Signature, SignedCommit, SignedControl, TARGET_LEN,
    Version, control_message, signed_message,
};
use sha2::{Digest, Sha256};
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

/// Length of a key file: the raw Ed25519 seed.
pub const SEED_LEN: usize = 32;

/// An Ed25519 signing seed.
#[derive(Clone, PartialEq, Eq)]
pub struct Seed([u8; SEED_LEN]);

impl fmt::Debug for Seed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Seed(..)")
    }
}

/// Why a key file or version could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyError {
    /// The key file is not exactly [`SEED_LEN`] bytes.
    Length(usize),
    /// The clock is before 1970 or after 2106 (no `u32` Unix time).
    Clock,
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyError::Length(n) => write!(
                f,
                "a key file holds exactly {SEED_LEN} raw bytes (an Ed25519 seed); this one has {n}"
            ),
            KeyError::Clock => f.write_str(
                "the system clock gives no u32 Unix time for the default version; pass --version",
            ),
        }
    }
}

impl std::error::Error for KeyError {}

impl Seed {
    /// A seed from raw bytes.
    pub const fn new(bytes: [u8; SEED_LEN]) -> Self {
        Seed(bytes)
    }

    /// Parses a key file's contents.
    pub fn from_file_bytes(bytes: &[u8]) -> Result<Self, KeyError> {
        <[u8; SEED_LEN]>::try_from(bytes)
            .map(Seed)
            .map_err(|_| KeyError::Length(bytes.len()))
    }

    /// The raw seed, as written to a key file.
    pub fn bytes(&self) -> &[u8; SEED_LEN] {
        &self.0
    }

    fn signer(&self) -> SigningKey {
        SigningKey::from_bytes(&self.0)
    }

    /// The public key to pin with `sys/trust/claim`.
    pub fn public_key(&self) -> PublicKey {
        PublicKey(self.signer().verifying_key().to_bytes())
    }
}

/// SHA-256 of an image.
pub fn image_sha256(image: &[u8]) -> [u8; 32] {
    Sha256::digest(image).into()
}

/// Signs `image` as `version`. `None` when the image length does not fit
/// the protocol's `u32`.
pub fn sign_commit(image: &[u8], seed: &Seed, version: Version) -> Option<SignedCommit> {
    let len = u32::try_from(image.len()).ok()?;
    let message = signed_message(len, version, &image_sha256(image));
    let signature = Signature(seed.signer().sign(&message).to_bytes());
    Some(SignedCommit { version, signature })
}

/// The default version at `now`: Unix time in seconds.
pub fn version_at(now: SystemTime) -> Result<Version, KeyError> {
    let secs = now
        .duration_since(UNIX_EPOCH)
        .map_err(|_| KeyError::Clock)?
        .as_secs();
    u32::try_from(secs)
        .map(Version)
        .map_err(|_| KeyError::Clock)
}

/// Signs control request `op` for the device with MAC `target` as
/// `counter`.
pub fn sign_control(
    seed: &Seed,
    op: ControlOp,
    target: &[u8; TARGET_LEN],
    counter: ControlCounter,
) -> SignedControl {
    let message = control_message(op, target, counter);
    let signature = Signature(seed.signer().sign(&message).to_bytes());
    SignedControl { counter, signature }
}

/// The default control counter at `now`: Unix time in milliseconds.
pub fn counter_at(now: SystemTime) -> Result<ControlCounter, KeyError> {
    let ms = now
        .duration_since(UNIX_EPOCH)
        .map_err(|_| KeyError::Clock)?
        .as_millis();
    u64::try_from(ms)
        .map(ControlCounter)
        .map_err(|_| KeyError::Clock)
}

/// Lower-case hex.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::time::Duration;

    /// `web/test/fixtures/trust-vector.json` (TEST KEY ONLY).
    pub const SEED: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    pub const PUBLIC_KEY: &str = "03a107bff3ce10be1d70dd18e74bc09967e4d6309ba50d5f1ddc8664125531b8";
    pub const VERSION: u32 = 1_761_500_000;
    pub const SHA256: &str = "1e9bc38cbf860b9ec31918b065f9b52476c549a782e0e7990bed8ce3868d2371";
    pub const COMMIT_BODY: &str = "605bfe68ca90e0aef84dfa5173aed3af1738933d2cebbd3f74012fe398847dad9db8b092fe91b9beaa7b59c6665f41b3abd352ddf890668cf994125438aa084c6a7b9f07";

    pub fn vector_image() -> Vec<u8> {
        (0..1000usize).map(|i| (i * 7 + 3) as u8).collect()
    }

    pub fn unhex(s: &str) -> Vec<u8> {
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    }

    pub fn vector_seed() -> Seed {
        Seed::from_file_bytes(&unhex(SEED)).unwrap()
    }

    #[test]
    fn golden_vector_commit_body_is_reproduced() {
        let seed = vector_seed();
        assert_eq!(hex(&seed.public_key().0), PUBLIC_KEY);
        let image = vector_image();
        assert_eq!(hex(&image_sha256(&image)), SHA256);
        let c = sign_commit(&image, &seed, Version(VERSION)).unwrap();
        assert_eq!(hex(&c.encode()), COMMIT_BODY);
    }

    #[test]
    fn golden_vector_file_matches_the_constants() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../web/test/fixtures/trust-vector.json"
        );
        let json = std::fs::read_to_string(path).unwrap();
        for c in [SEED, PUBLIC_KEY, SHA256, COMMIT_BODY] {
            assert!(json.contains(c), "{c} not in {path}");
        }
        assert!(json.contains(&VERSION.to_string()));
    }

    /// The control-request vector of `middleware::loader::trust` (same
    /// test seed, op Stop, target e4:b0:63:81:d3:24).
    pub const CONTROL_COUNTER: u64 = 1_761_500_000_123;
    pub const CONTROL_SIGNATURE: &str = "aeffa9bd89e94d48b5577de27cb123b6cdd6450b3e0cf2f6c13eb07827d4938a345233eb01f44746c8c56c056c6b0cbd62544876010bc75352efaab0636b2b01";
    pub const CONTROL_TARGET: [u8; 6] = [0xe4, 0xb0, 0x63, 0x81, 0xd3, 0x24];

    #[test]
    fn golden_control_vector_is_reproduced() {
        let c = sign_control(
            &vector_seed(),
            ControlOp::Stop,
            &CONTROL_TARGET,
            ControlCounter(CONTROL_COUNTER),
        );
        assert_eq!(hex(&c.signature.0), CONTROL_SIGNATURE);
        let body = c.encode();
        assert_eq!(&body[..8], &CONTROL_COUNTER.to_le_bytes());
        assert_eq!(hex(&body[8..]), CONTROL_SIGNATURE);
        // Bound to the op and the device.
        let start = sign_control(
            &vector_seed(),
            ControlOp::Start,
            &CONTROL_TARGET,
            ControlCounter(CONTROL_COUNTER),
        );
        assert_ne!(start.signature, c.signature);
    }

    #[test]
    fn default_counter_is_unix_milliseconds() {
        let t = UNIX_EPOCH + Duration::from_millis(1_761_500_000_123);
        assert_eq!(counter_at(t), Ok(ControlCounter(1_761_500_000_123)));
        assert_eq!(
            counter_at(UNIX_EPOCH - Duration::from_secs(1)),
            Err(KeyError::Clock)
        );
    }

    #[test]
    fn key_files_must_be_32_bytes() {
        assert_eq!(Seed::from_file_bytes(&[0; 31]), Err(KeyError::Length(31)));
        assert_eq!(Seed::from_file_bytes(&[0; 33]), Err(KeyError::Length(33)));
        assert!(KeyError::Length(64).to_string().contains("exactly 32"));
        assert_eq!(Seed::new([1; 32]).bytes(), &[1; 32]);
        assert_eq!(
            format!("{:?}", Seed::new([1; 32])),
            "Seed(..)",
            "never printed"
        );
    }

    #[test]
    fn default_version_is_unix_seconds() {
        let t = UNIX_EPOCH + Duration::from_secs(1_761_500_000);
        assert_eq!(version_at(t), Ok(Version(1_761_500_000)));
        assert_eq!(
            version_at(UNIX_EPOCH - Duration::from_secs(1)),
            Err(KeyError::Clock)
        );
        let after_2106 = UNIX_EPOCH + Duration::from_secs(u64::from(u32::MAX) + 1);
        assert_eq!(version_at(after_2106), Err(KeyError::Clock));
    }
}
