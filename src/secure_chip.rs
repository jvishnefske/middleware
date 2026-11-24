//! Key custody on a secure chip: a TPM, a secure element or an HSM holds the
//! private key, and the board sees only its public half and signatures.
//!
//! The trait is the one shape every backend implements. Keys are P-256 (the
//! kind hardware tokens sign; see [`crate::loader::trust`]): the public key is
//! the compressed SEC1 point, and a signature is ECDSA with SHA-256 over the
//! message, as raw `r || s`. Each backend says where its keys live: a PKCS#11
//! token (a TPM through tpm2-pkcs11 on Linux), or a key file on the host.
//!
//! The backend hashes the message itself, so a caller never passes a digest
//! and cannot sign the wrong one. A certificate signing request (CSR) is the
//! same operation: the request's signed part is the message.
//!
//! `no_std`, no heap, no dependencies. Feature `secure-chip`.

use crate::loader::trust::{P256_KEY_LEN, SIGNATURE_LEN};

/// A key's place on the chip. Backends name their keys from the slot;
/// [`Slot::DEVICE`] is the board's own identity (its mutual-TLS key).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Slot(pub u8);

impl Slot {
    /// The board's own identity key.
    pub const DEVICE: Slot = Slot(0);
}

/// A chip that holds P-256 keys and signs with them. The private key never
/// leaves the chip: no method returns it.
pub trait SecureChip {
    /// Why the chip failed. Carries no key material.
    type Error: core::fmt::Debug;

    /// Whether the slot holds a key.
    fn has_key(&mut self, slot: Slot) -> Result<bool, Self::Error>;

    /// Makes the slot's key. Refused when the slot already holds one, so a
    /// key is never replaced by accident.
    fn generate_key(&mut self, slot: Slot) -> Result<(), Self::Error>;

    /// The slot's public key: the compressed SEC1 point (33 bytes, prefix
    /// `02` or `03`).
    fn public_key(&mut self, slot: Slot) -> Result<[u8; P256_KEY_LEN], Self::Error>;

    /// A signature over `message`: ECDSA P-256 with SHA-256, raw `r || s`.
    fn sign(&mut self, slot: Slot, message: &[u8]) -> Result<[u8; SIGNATURE_LEN], Self::Error>;
}

/// The slot's public key, making the key first if the chip has none. A
/// second call returns the same key.
pub fn ensure_key<C: SecureChip>(chip: &mut C, slot: Slot) -> Result<[u8; P256_KEY_LEN], C::Error> {
    if !chip.has_key(slot)? {
        chip.generate_key(slot)?;
    }
    chip.public_key(slot)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A chip that makes one key, in memory, and signs with a fixed value.
    #[derive(Default)]
    struct Fake {
        generated: u32,
        key: Option<[u8; P256_KEY_LEN]>,
        last_message: Vec<u8>,
    }

    #[derive(Debug, PartialEq)]
    enum FakeError {
        Exists,
        Empty,
    }

    impl SecureChip for Fake {
        type Error = FakeError;

        fn has_key(&mut self, slot: Slot) -> Result<bool, FakeError> {
            Ok(slot == Slot::DEVICE && self.key.is_some())
        }

        fn generate_key(&mut self, _slot: Slot) -> Result<(), FakeError> {
            if self.key.is_some() {
                return Err(FakeError::Exists);
            }
            self.generated += 1;
            let mut k = [0x11u8; P256_KEY_LEN];
            k[0] = 0x02;
            self.key = Some(k);
            Ok(())
        }

        fn public_key(&mut self, slot: Slot) -> Result<[u8; P256_KEY_LEN], FakeError> {
            if slot != Slot::DEVICE {
                return Err(FakeError::Empty);
            }
            self.key.ok_or(FakeError::Empty)
        }

        fn sign(&mut self, slot: Slot, message: &[u8]) -> Result<[u8; SIGNATURE_LEN], FakeError> {
            if slot != Slot::DEVICE || self.key.is_none() {
                return Err(FakeError::Empty);
            }
            self.last_message = message.to_vec();
            Ok([0xab; SIGNATURE_LEN])
        }
    }

    #[test]
    fn ensure_key_makes_a_key_once_and_returns_it_again() {
        let mut chip = Fake::default();
        let first = ensure_key(&mut chip, Slot::DEVICE).unwrap();
        let second = ensure_key(&mut chip, Slot::DEVICE).unwrap();
        assert_eq!(first, second);
        assert_eq!(chip.generated, 1);
    }

    #[test]
    fn a_chip_with_a_key_is_not_asked_to_make_one() {
        let mut chip = Fake { key: Some([0x02; P256_KEY_LEN]), ..Fake::default() };
        ensure_key(&mut chip, Slot::DEVICE).unwrap();
        assert_eq!(chip.generated, 0);
    }

    #[test]
    fn a_second_generate_is_refused_and_leaves_the_key() {
        let mut chip = Fake::default();
        chip.generate_key(Slot::DEVICE).unwrap();
        let before = chip.key;
        assert_eq!(chip.generate_key(Slot::DEVICE), Err(FakeError::Exists));
        assert_eq!(chip.key, before);
    }

    #[test]
    fn sign_passes_the_whole_message_and_returns_raw_r_s() {
        let mut chip = Fake::default();
        chip.generate_key(Slot::DEVICE).unwrap();
        let sig = chip.sign(Slot::DEVICE, b"csr-tbs").unwrap();
        assert_eq!(sig, [0xab; SIGNATURE_LEN]);
        assert_eq!(chip.last_message, b"csr-tbs");
    }

    #[test]
    fn other_slots_are_not_the_device_key() {
        let mut chip = Fake::default();
        chip.generate_key(Slot::DEVICE).unwrap();
        assert_eq!(chip.has_key(Slot(1)), Ok(false));
        assert_eq!(chip.sign(Slot(1), b"x"), Err(FakeError::Empty));
    }
}
