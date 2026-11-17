//! Payload signing: the [`Signer`] abstraction, key seeds, public keys and
//! signed commits.
//!
//! A [`Signer`] holds one private key and signs messages for the device's
//! pinned key kind. Two exist: a key file ([`Seed`]: the raw 32-byte
//! Ed25519 seed, RFC 8032 private key, nothing else) and a key inside a
//! PKCS#11 token ([`crate::pkcs11::Pkcs11Signer`]: a TPM 2.0 through
//! tpm2-pkcs11, a smart card through OpenSC, SoftHSM2 in tests), which
//! signs P-256 (or Ed25519 where the token has it) without the private key
//! ever leaving the token. [`sign_commit`] signs the device's 54-byte message
//! [`signed_message`] (`b"mw-payload-v1\0" || len || version || sha256`)
//! for one image and returns the [`SignedCommit`] that becomes the 68-byte
//! `sys/loader/commit` body. The default version is the current Unix time
//! in seconds ([`version_at`]), which increases with every upload and fits
//! a `u32` until 2106.
//!
//! Ed25519 signing is deterministic, so the golden vector shared with the
//! browser (`web/test/fixtures/trust-vector.json`) is reproduced exactly;
//! ECDSA is not, so P-256 signatures are checked by verifying them
//! ([`verify`], the host copy of the device's check, used by the PKCS#11
//! signer on every signature it returns and by the fake device).
//!
//! [`sign_control`] signs a control request for `start`/`stop` over
//! ESP-NOW: the 29-byte [`control_message`]
//! (`b"mw-control-v1\0" || op || target MAC || counter u64`) for one
//! device, returned as the [`SignedControl`] that becomes the 72-byte
//! request body. The default counter is the current Unix time in
//! milliseconds ([`counter_at`]); the device accepts only a counter above
//! the last one it accepted, so a request is never accepted twice.

use ed25519_dalek::{Signer as _, SigningKey};
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

/// A private key that signs device messages (payload commits, control
/// requests, firmware commits).
pub trait Signer: fmt::Debug {
    /// The public key to pin with `sys/trust/claim`.
    fn public_key(&self) -> PublicKey;
    /// A 64-byte signature over `message` for [`Signer::public_key`]'s kind
    /// (Ed25519 pure; P-256 ECDSA with SHA-256 as raw `r || s`).
    fn sign(&self, message: &[u8]) -> Result<Signature, SignError>;
}

/// Why signing failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignError {
    /// The image length does not fit the protocol's `u32`.
    TooLarge(usize),
    /// The PKCS#11 token refused or failed (the message says which step);
    /// never carries a PIN.
    Token(String),
}

impl fmt::Display for SignError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SignError::TooLarge(n) => write!(f, "a {n}-byte image is too large to sign"),
            SignError::Token(m) => write!(f, "PKCS#11: {m}"),
        }
    }
}

impl std::error::Error for SignError {}

impl Signer for Seed {
    fn public_key(&self) -> PublicKey {
        Seed::public_key(self)
    }

    fn sign(&self, message: &[u8]) -> Result<Signature, SignError> {
        Ok(Signature(self.signer().sign(message).to_bytes()))
    }
}

/// Whether `signature` over `message` verifies against `key`: the host copy
/// of the device check (`middleware::loader::trust::verify_signature`,
/// which mwctl does not build: it uses no middleware feature). Ed25519
/// `verify_strict`; P-256 ECDSA / SHA-256 over raw `r || s`.
pub fn verify(key: &PublicKey, message: &[u8], signature: &Signature) -> bool {
    match key {
        PublicKey::Ed25519(k) => ed25519_dalek::VerifyingKey::from_bytes(k).is_ok_and(|vk| {
            vk.verify_strict(message, &ed25519_dalek::Signature::from_bytes(&signature.0))
                .is_ok()
        }),
        PublicKey::P256(k) => {
            use p256::ecdsa::signature::Verifier as _;
            let (Ok(vk), Ok(sig)) = (
                p256::ecdsa::VerifyingKey::from_sec1_bytes(k),
                p256::ecdsa::Signature::from_slice(&signature.0),
            ) else {
                return false;
            };
            vk.verify(message, &sig).is_ok()
        }
    }
}

/// Whether `key` is a point of its curve (what a device checks on claim).
pub fn is_valid_key(key: &PublicKey) -> bool {
    match key {
        PublicKey::Ed25519(k) => ed25519_dalek::VerifyingKey::from_bytes(k).is_ok(),
        PublicKey::P256(k) => p256::ecdsa::VerifyingKey::from_sec1_bytes(k).is_ok(),
    }
}

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
        PublicKey::Ed25519(self.signer().verifying_key().to_bytes())
    }
}

/// SHA-256 of an image.
pub fn image_sha256(image: &[u8]) -> [u8; 32] {
    Sha256::digest(image).into()
}

/// Signs `image` as `version`.
pub fn sign_commit(
    image: &[u8],
    signer: &dyn Signer,
    version: Version,
) -> Result<SignedCommit, SignError> {
    let len = u32::try_from(image.len()).map_err(|_| SignError::TooLarge(image.len()))?;
    let message = signed_message(len, version, &image_sha256(image));
    let signature = signer.sign(&message)?;
    Ok(SignedCommit { version, signature })
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
    signer: &dyn Signer,
    op: ControlOp,
    target: &[u8; TARGET_LEN],
    counter: ControlCounter,
) -> Result<SignedControl, SignError> {
    let message = control_message(op, target, counter);
    let signature = signer.sign(&message)?;
    Ok(SignedControl { counter, signature })
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
        assert_eq!(hex(seed.public_key().as_bytes()), PUBLIC_KEY);
        let image = vector_image();
        assert_eq!(hex(&image_sha256(&image)), SHA256);
        let c = sign_commit(&image, &seed, Version(VERSION)).unwrap();
        assert_eq!(hex(&c.encode()), COMMIT_BODY);
        let m = signed_message(1000, Version(VERSION), &image_sha256(&image));
        assert!(verify(&seed.public_key(), &m, &c.signature));
        assert!(!verify(&seed.public_key(), &m[1..], &c.signature));
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
        )
        .unwrap();
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
        )
        .unwrap();
        assert_ne!(start.signature, c.signature);
    }

    /// `web/test/fixtures/trust-vector-p256.json` (TEST KEY ONLY, made
    /// with OpenSSL): the host verifier agrees with it.
    pub const P256_PUBLIC_KEY: &str =
        "020e539e011304a06ad552227a677a388cee251cfe15486e5ae02230a79d590ee0";
    pub const P256_COMMIT_BODY: &str = "605bfe68404856b8d6872cf9fc82259fe1394169328fc855a8d50c24f159352446c3433379c60da8ca20d8e3e3a0f8aafbd8ffaaabf4c26344c1c8b8e1ef806348d46a39";

    #[test]
    fn p256_openssl_vector_verifies_on_the_host() {
        let key = PublicKey::from_bytes(&unhex(P256_PUBLIC_KEY)).unwrap();
        assert!(is_valid_key(&key));
        let body = unhex(P256_COMMIT_BODY);
        let c = SignedCommit::parse_body(&body).unwrap().unwrap();
        let m = signed_message(1000, c.version, &image_sha256(&vector_image()));
        assert!(verify(&key, &m, &c.signature));
        let mut bad = c.signature;
        bad.0[10] ^= 1;
        assert!(!verify(&key, &m, &bad));
        assert!(!verify(&vector_seed().public_key(), &m, &c.signature));
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../web/test/fixtures/trust-vector-p256.json"
        );
        let json = std::fs::read_to_string(path).unwrap();
        assert!(json.contains(P256_PUBLIC_KEY) && json.contains(P256_COMMIT_BODY));
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
