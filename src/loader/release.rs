//! Software certificates: a release signed once by the root key, valid on
//! every device of its target (UP1–UP3).
//!
//! A pushed update is signed for one device: a payload commit
//! ([`super::trust::SignedCommit`]) covers the image and a version, a
//! firmware commit ([`crate::fw_ota::wire::signed_message`]) also the node
//! id and the slot. A release served by the updates channel is fetched by
//! many devices, so its signature cannot name any one of them. A
//! [`SoftwareCert`] says instead what the image is for:
//!
//! | field     | encoding                     | meaning |
//! |-----------|------------------------------|---------|
//! | `kind`    | u8 ([`CertKind`])            | payload (1), firmware (2) or the Linux root (3, `os`) |
//! | `target`  | `[len u8][1..=32 bytes UTF-8]` | the `chip` a device reports in `sys/info` (`rp2350`, `linux-aarch64`); a payload may say [`ANY_TARGET`] |
//! | `channel` | `[len u8][1..=32 bytes UTF-8]` | the updates channel (`stable`, `test`); devices do not check it |
//! | `version` | u32 LE                       | strictly above the device's floor |
//! | `len`     | u32 LE                       | image length |
//! | `sha256`  | 32 bytes                     | SHA-256 of the image |
//!
//! The root signs [`release_message`], `b"mw-release-v1\0" || cert`, with
//! the claimed key's algorithm (Ed25519 or P-256, 64-byte signature). The
//! `sys/loader/commit` and `sys/fw/commit` bodies accept `[cert][sig 64]`
//! ([`SignedCert`]) beside their per-device forms. A cert body is at least
//! [`CERT_MIN`] + 64 = 109 bytes, so it never has the 68 bytes of a signed
//! payload commit or the 64 of a signed firmware commit.
//!
//! A device accepts a cert when it is claimed by the key that signed it,
//! the cert's kind is the one the key was sent on, its target is the
//! device's chip (or `any` for a payload), its length and hash are those
//! of the image it received, and its version is above the floor (the
//! counter that already forbids rollback of signed payloads; for firmware
//! the larger of the persisted floor and the running version). An
//! unclaimed device accepts any commit, as before (trust on first use).

use super::trust::{SIGNATURE_LEN, Signature, SignedCommit, Version};
use crate::transport::HandlerError;

/// Domain-separation prefix of [`release_message`].
pub const RELEASE_TAG: &[u8; 14] = b"mw-release-v1\0";
/// Longest target or channel name.
pub const NAME_MAX: usize = 32;
/// Shortest encoded cert (one-byte names).
pub const CERT_MIN: usize = 1 + 2 + 2 + 4 + 4 + 32;
/// Longest encoded cert.
pub const CERT_MAX: usize = 1 + 2 * (1 + NAME_MAX) + 4 + 4 + 32;
/// Longest [`release_message`].
pub const RELEASE_MESSAGE_MAX: usize = RELEASE_TAG.len() + CERT_MAX;
/// The payload target every device matches.
pub const ANY_TARGET: &str = "any";

/// What a cert releases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum CertKind {
    /// A WASM payload, installed through `sys/loader/*`.
    Payload = 1,
    /// A firmware image, installed through `sys/fw/*` into the inactive slot.
    Firmware = 2,
    /// A Linux root (a squashfs store), installed through `sys/os/*` into
    /// the inactive A/B slot of the card (design.md, "Linux A/B root").
    Os = 3,
}

impl CertKind {
    /// Parses the kind byte.
    pub const fn from_byte(b: u8) -> Option<Self> {
        match b {
            1 => Some(CertKind::Payload),
            2 => Some(CertKind::Firmware),
            3 => Some(CertKind::Os),
            _ => None,
        }
    }

    /// Lower-case name (`payload`, `fw`), as keys and files spell it.
    pub const fn name(self) -> &'static str {
        match self {
            CertKind::Payload => "payload",
            CertKind::Firmware => "fw",
            CertKind::Os => "os",
        }
    }

    /// The kind of a name written by [`CertKind::name`].
    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "payload" => Some(CertKind::Payload),
            "fw" | "firmware" => Some(CertKind::Firmware),
            "os" => Some(CertKind::Os),
            _ => None,
        }
    }
}

/// One release: what an image is, for which devices. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SoftwareCert<'a> {
    /// Payload or firmware.
    pub kind: CertKind,
    /// The `sys/info` chip it is built for, or [`ANY_TARGET`] (payloads).
    pub target: &'a str,
    /// The updates channel it was published on.
    pub channel: &'a str,
    /// Release version.
    pub version: Version,
    /// Image length.
    pub len: u32,
    /// SHA-256 of the image.
    pub sha256: [u8; 32],
}

fn valid_name(s: &str) -> bool {
    (1..=NAME_MAX).contains(&s.len())
}

impl<'a> SoftwareCert<'a> {
    /// Encoded length; `None` when a name is empty or too long.
    pub fn encoded_len(&self) -> Option<usize> {
        (valid_name(self.target) && valid_name(self.channel))
            .then(|| 1 + 2 + self.target.len() + self.channel.len() + 4 + 4 + 32)
    }

    /// Encodes into `out`; `None` when `out` is too small or a name is
    /// empty or longer than [`NAME_MAX`].
    pub fn encode(&self, out: &mut [u8]) -> Option<usize> {
        let n = self.encoded_len()?;
        let out = out.get_mut(..n)?;
        let mut at = 0;
        let mut put = |b: &[u8]| {
            out[at..at + b.len()].copy_from_slice(b);
            at += b.len();
        };
        put(&[self.kind as u8, self.target.len() as u8]);
        put(self.target.as_bytes());
        put(&[self.channel.len() as u8]);
        put(self.channel.as_bytes());
        put(&self.version.0.to_le_bytes());
        put(&self.len.to_le_bytes());
        put(&self.sha256);
        Some(n)
    }

    /// Decodes a cert at the start of `bytes`, returning it and its length.
    /// `None` for an unknown kind, an empty, oversized or non-UTF-8 name, or
    /// too few bytes.
    pub fn decode(bytes: &'a [u8]) -> Option<(Self, usize)> {
        let kind = CertKind::from_byte(*bytes.first()?)?;
        let mut at = 1;
        let mut name = || -> Option<&'a str> {
            let n = *bytes.get(at)? as usize;
            let s = bytes.get(at + 1..at + 1 + n)?;
            at += 1 + n;
            let s = core::str::from_utf8(s).ok()?;
            valid_name(s).then_some(s)
        };
        let target = name()?;
        let channel = name()?;
        let u32_at = |i: usize| -> Option<u32> {
            Some(u32::from_le_bytes(bytes.get(i..i + 4)?.try_into().ok()?))
        };
        let version = Version(u32_at(at)?);
        let len = u32_at(at + 4)?;
        let sha256 = bytes.get(at + 8..at + 40)?.try_into().ok()?;
        Some((
            SoftwareCert {
                kind,
                target,
                channel,
                version,
                len,
                sha256,
            },
            at + 40,
        ))
    }

    /// Whether a device reporting `chip` may install this release.
    pub fn is_for(&self, chip: &str) -> bool {
        self.target == chip || (self.kind == CertKind::Payload && self.target == ANY_TARGET)
    }
}

/// The message the root signs: `RELEASE_TAG || cert`, written into `out`
/// (at least [`RELEASE_MESSAGE_MAX`] bytes). `None` for a cert that does
/// not encode.
pub fn release_message<'o>(cert: &SoftwareCert<'_>, out: &'o mut [u8]) -> Option<&'o [u8]> {
    let (tag, rest) = out.split_at_mut_checked(RELEASE_TAG.len())?;
    tag.copy_from_slice(RELEASE_TAG);
    let n = cert.encode(rest)?;
    out.get(..RELEASE_TAG.len() + n)
}

/// A cert and the root's signature over [`release_message`]: the
/// `[cert][sig 64]` commit body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignedCert<'a> {
    /// The release.
    pub cert: SoftwareCert<'a>,
    /// The root's signature.
    pub signature: Signature,
}

impl<'a> SignedCert<'a> {
    /// Parses exactly `[cert][sig 64]`; `None` otherwise (trailing bytes
    /// included).
    pub fn decode(body: &'a [u8]) -> Option<Self> {
        let (cert, n) = SoftwareCert::decode(body)?;
        let signature = <[u8; SIGNATURE_LEN]>::try_from(body.get(n..)?).ok()?;
        Some(SignedCert {
            cert,
            signature: Signature(signature),
        })
    }

    /// Encodes `[cert][sig]` into `out`.
    pub fn encode(&self, out: &mut [u8]) -> Option<usize> {
        let n = self.cert.encode(out)?;
        out.get_mut(n..n + SIGNATURE_LEN)?
            .copy_from_slice(&self.signature.0);
        Some(n + SIGNATURE_LEN)
    }
}

/// The authorization in a `sys/loader/commit` body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitAuth<'a> {
    /// `[version u32][sig 64]`: signed for this one image (a push).
    Signed(SignedCommit),
    /// `[cert][sig 64]`: a release from the updates channel.
    Cert(SignedCert<'a>),
}

/// Longest `sys/loader/commit` or `sys/fw/commit` body.
pub const COMMIT_BODY_MAX: usize = CERT_MAX + SIGNATURE_LEN;

impl<'a> CommitAuth<'a> {
    /// Parses a `sys/loader/commit` body: empty is unsigned (`None`), 68
    /// bytes a [`SignedCommit`], anything that parses as `[cert][sig]` a
    /// [`SignedCert`]; anything else is `InvalidRequest`.
    pub fn parse_body(body: &'a [u8]) -> Result<Option<Self>, HandlerError> {
        if body.len() == super::trust::SIGNED_COMMIT_LEN || body.is_empty() {
            return Ok(SignedCommit::parse_body(body)?.map(CommitAuth::Signed));
        }
        SignedCert::decode(body)
            .map(|c| Some(CommitAuth::Cert(c)))
            .ok_or(HandlerError::InvalidRequest)
    }

    /// Encodes the body into `out`.
    pub fn encode(&self, out: &mut [u8]) -> Option<usize> {
        match self {
            CommitAuth::Signed(s) => {
                let b = s.encode();
                out.get_mut(..b.len())?.copy_from_slice(&b);
                Some(b.len())
            }
            CommitAuth::Cert(c) => c.encode(out),
        }
    }
}

#[cfg(feature = "trust")]
mod verify {
    use super::*;
    use crate::loader::ImageDigest;
    use crate::loader::trust::{PublicKey, TrustState, verify_signature};

    /// Whether `signed` is signed by `key` over [`release_message`].
    pub fn verify_cert(key: &PublicKey, signed: &SignedCert<'_>) -> bool {
        let mut buf = [0u8; RELEASE_MESSAGE_MAX];
        release_message(&signed.cert, &mut buf)
            .is_some_and(|m| verify_signature(key, m, &signed.signature))
    }

    impl TrustState {
        /// Decides a `sys/loader/commit` of `image` on a device reporting
        /// `chip`, whichever form `auth` has: a [`SignedCommit`] as
        /// [`TrustState::accept_commit`], a [`SignedCert`] as
        /// [`TrustState::accept_cert`]. Unclaimed accepts anything.
        pub fn accept_commit_auth(
            self,
            image: &ImageDigest,
            auth: Option<&CommitAuth<'_>>,
            chip: &str,
        ) -> Result<TrustState, HandlerError> {
            match auth {
                Some(CommitAuth::Cert(c)) => self.accept_cert(image, c, chip),
                Some(CommitAuth::Signed(s)) => self.accept_commit(image, Some(s)),
                None => self.accept_commit(image, None),
            }
        }

        /// Decides a payload release: when Claimed, `signed` must be a
        /// payload cert for `chip` (or `any`), with `image`'s length and
        /// hash, a version above `min_version`, and a signature by the
        /// pinned key; the result has `min_version = version`. Every
        /// failure is `Denied`. Unclaimed: accepted unchanged.
        pub fn accept_cert(
            self,
            image: &ImageDigest,
            signed: &SignedCert<'_>,
            chip: &str,
        ) -> Result<TrustState, HandlerError> {
            let TrustState::Claimed { key, min_version } = self else {
                return Ok(self);
            };
            let c = &signed.cert;
            let ok = c.kind == CertKind::Payload
                && c.is_for(chip)
                && c.len == image.len
                && c.sha256 == image.sha256
                && c.version > min_version
                && verify_cert(&key, signed);
            if !ok {
                return Err(HandlerError::Denied);
            }
            Ok(TrustState::Claimed {
                key,
                min_version: c.version,
            })
        }
    }
}

#[cfg(feature = "trust")]
pub use verify::verify_cert;

#[cfg(test)]
mod tests {
    use super::*;

    fn cert() -> SoftwareCert<'static> {
        SoftwareCert {
            kind: CertKind::Payload,
            target: "linux-x86_64",
            channel: "test",
            version: Version(7),
            len: 1000,
            sha256: [0xab; 32],
        }
    }

    #[test]
    fn up1_cert_round_trips_and_rejects_bad_bytes() {
        let c = cert();
        let mut out = [0u8; CERT_MAX];
        let n = c.encode(&mut out).unwrap();
        assert_eq!(n, c.encoded_len().unwrap());
        assert_eq!(SoftwareCert::decode(&out[..n]), Some((c, n)));
        // Truncated anywhere.
        for i in 0..n {
            assert_eq!(SoftwareCert::decode(&out[..i]), None, "len {i}");
        }
        // Unknown kind, empty or long name, bad UTF-8.
        let mut bad = out;
        bad[0] = 9;
        assert_eq!(SoftwareCert::decode(&bad[..n]), None);
        let empty = SoftwareCert { target: "", ..c };
        assert_eq!(empty.encode(&mut out), None);
        let long = SoftwareCert {
            channel: "x".repeat(33).leak(),
            ..c
        };
        assert_eq!(long.encode(&mut [0; 200]), None);
        let mut utf = [0u8; CERT_MAX];
        let n = c.encode(&mut utf).unwrap();
        utf[2] = 0xff;
        assert_eq!(SoftwareCert::decode(&utf[..n]), None);
        // Longest names fit CERT_MAX.
        let max = SoftwareCert {
            target: "t".repeat(32).leak(),
            channel: "c".repeat(32).leak(),
            ..c
        };
        assert_eq!(max.encode(&mut out), Some(CERT_MAX));
    }

    #[test]
    fn up1_release_message_is_tag_then_cert() {
        let c = cert();
        let mut m = [0u8; RELEASE_MESSAGE_MAX];
        let msg = release_message(&c, &mut m).unwrap().to_vec();
        assert_eq!(&msg[..14], RELEASE_TAG);
        let mut out = [0u8; CERT_MAX];
        let n = c.encode(&mut out).unwrap();
        assert_eq!(&msg[14..], &out[..n]);
    }

    #[test]
    fn up1_targets() {
        let c = cert();
        assert!(c.is_for("linux-x86_64"));
        assert!(!c.is_for("rp2350"));
        let any = SoftwareCert {
            target: ANY_TARGET,
            ..c
        };
        assert!(any.is_for("rp2350"));
        let fw_any = SoftwareCert {
            kind: CertKind::Firmware,
            ..any
        };
        assert!(!fw_any.is_for("rp2350"), "firmware is never for any chip");
        let os = SoftwareCert {
            kind: CertKind::Os,
            target: "linux-aarch64",
            ..c
        };
        assert!(os.is_for("linux-aarch64"));
        assert!(!os.is_for("linux-x86_64"));
        let os_any = SoftwareCert { target: ANY_TARGET, ..os };
        assert!(!os_any.is_for("linux-aarch64"), "a Linux root is never for any chip");
    }

    #[test]
    fn up1_os_kind_round_trips_and_is_named() {
        assert_eq!(CertKind::from_byte(3), Some(CertKind::Os));
        assert_eq!(CertKind::from_byte(4), None);
        assert_eq!(CertKind::Os as u8, 3);
        assert_eq!(CertKind::Os.name(), "os");
        assert_eq!(CertKind::from_name("os"), Some(CertKind::Os));
        let c = SoftwareCert { kind: CertKind::Os, target: "linux-aarch64", ..cert() };
        let mut out = [0u8; CERT_MAX];
        let n = c.encode(&mut out).unwrap();
        assert_eq!(out[0], 3);
        assert_eq!(SoftwareCert::decode(&out[..n]), Some((c, n)));
    }

    #[test]
    fn up2_commit_bodies_of_both_forms() {
        assert_eq!(CommitAuth::parse_body(&[]), Ok(None));
        let s = SignedCommit {
            version: Version(3),
            signature: Signature([5; 64]),
        };
        let b = s.encode();
        assert_eq!(CommitAuth::parse_body(&b), Ok(Some(CommitAuth::Signed(s))));
        let sc = SignedCert {
            cert: cert(),
            signature: Signature([6; 64]),
        };
        let mut out = [0u8; COMMIT_BODY_MAX];
        let n = CommitAuth::Cert(sc).encode(&mut out).unwrap();
        assert!(n >= CERT_MIN + SIGNATURE_LEN);
        assert_eq!(
            CommitAuth::parse_body(&out[..n]),
            Ok(Some(CommitAuth::Cert(sc)))
        );
        // Truncated or with a trailing byte.
        for body in [&out[..n - 1], &out[..n + 1]] {
            assert_eq!(
                CommitAuth::parse_body(body),
                Err(HandlerError::InvalidRequest)
            );
        }
        for len in [1, 4, 64, 67, 69, CERT_MIN + 63] {
            assert_eq!(
                CommitAuth::parse_body(&[1; 200][..len]),
                Err(HandlerError::InvalidRequest),
                "len {len}"
            );
        }
    }

    #[cfg(feature = "trust")]
    mod signed {
        use super::*;
        use crate::loader::ImageDigest;
        use crate::loader::sha256::ImageHash;
        use crate::loader::trust::{PublicKey, TrustState};
        use ed25519_dalek::{Signer, SigningKey};

        const IMAGE: &[u8] = b"a payload image";
        const CHIP: &str = "linux-x86_64";

        fn digest() -> ImageDigest {
            ImageDigest {
                len: IMAGE.len() as u32,
                crc: crate::crc::Crc32::checksum(IMAGE),
                sha256: ImageHash::digest(IMAGE),
            }
        }

        fn root() -> SigningKey {
            SigningKey::from_bytes(&[7; 32])
        }

        fn claimed(min: u32) -> TrustState {
            TrustState::Claimed {
                key: PublicKey::Ed25519(root().verifying_key().to_bytes()),
                min_version: Version(min),
            }
        }

        fn sign<'a>(s: &SigningKey, cert: SoftwareCert<'a>) -> SignedCert<'a> {
            let mut m = [0u8; RELEASE_MESSAGE_MAX];
            let msg = release_message(&cert, &mut m).unwrap();
            SignedCert {
                cert,
                signature: Signature(s.sign(msg).to_bytes()),
            }
        }

        fn good() -> SoftwareCert<'static> {
            let d = digest();
            SoftwareCert {
                kind: CertKind::Payload,
                target: CHIP,
                channel: "test",
                version: Version(10),
                len: d.len,
                sha256: d.sha256,
            }
        }

        #[test]
        fn up3_payload_cert_accepted_and_raises_the_floor() {
            let d = digest();
            let sc = sign(&root(), good());
            assert_eq!(claimed(0).accept_cert(&d, &sc, CHIP), Ok(claimed(10)));
            let any = sign(
                &root(),
                SoftwareCert {
                    target: ANY_TARGET,
                    ..good()
                },
            );
            assert_eq!(claimed(9).accept_cert(&d, &any, "esp32s3"), Ok(claimed(10)));
            // Through the commit body.
            let mut out = [0u8; COMMIT_BODY_MAX];
            let n = CommitAuth::Cert(sc).encode(&mut out).unwrap();
            let auth = CommitAuth::parse_body(&out[..n]).unwrap();
            assert_eq!(
                claimed(0).accept_commit_auth(&d, auth.as_ref(), CHIP),
                Ok(claimed(10))
            );
            // Unclaimed: trust on first use.
            assert_eq!(
                TrustState::Unclaimed.accept_cert(&d, &sc, "other"),
                Ok(TrustState::Unclaimed)
            );
        }

        #[test]
        fn up3_payload_cert_denials() {
            let d = digest();
            let denied = Err(HandlerError::Denied);
            let with = |c: SoftwareCert<'static>| sign(&root(), c);
            // Wrong target.
            assert_eq!(claimed(0).accept_cert(&d, &with(good()), "rp2350"), denied);
            // Wrong sha, wrong length.
            let sha = with(SoftwareCert {
                sha256: [0; 32],
                ..good()
            });
            assert_eq!(claimed(0).accept_cert(&d, &sha, CHIP), denied);
            let len = with(SoftwareCert {
                len: d.len + 1,
                ..good()
            });
            assert_eq!(claimed(0).accept_cert(&d, &len, CHIP), denied);
            // Downgrade and replay.
            assert_eq!(claimed(10).accept_cert(&d, &with(good()), CHIP), denied);
            assert_eq!(claimed(11).accept_cert(&d, &with(good()), CHIP), denied);
            // A firmware cert on the payload loader.
            let fw = with(SoftwareCert {
                kind: CertKind::Firmware,
                ..good()
            });
            assert_eq!(claimed(0).accept_cert(&d, &fw, CHIP), denied);
            // Another key.
            let mallory = sign(&SigningKey::from_bytes(&[42; 32]), good());
            assert_eq!(claimed(0).accept_cert(&d, &mallory, CHIP), denied);
            // A field changed after signing (the channel).
            let mut moved = with(good());
            moved.cert.channel = "stable";
            assert_eq!(claimed(0).accept_cert(&d, &moved, CHIP), denied);
        }

        #[test]
        fn up3_p256_root_signs_certs() {
            use p256::ecdsa::signature::Signer as _;
            let sk = p256::ecdsa::SigningKey::from_slice(&[9; 32]).unwrap();
            let pk = sk.verifying_key().to_encoded_point(true);
            let key = PublicKey::from_bytes(pk.as_bytes()).unwrap();
            let t = TrustState::Claimed {
                key,
                min_version: Version(0),
            };
            let mut m = [0u8; RELEASE_MESSAGE_MAX];
            let msg = release_message(&good(), &mut m).unwrap();
            let sig: p256::ecdsa::Signature = sk.sign(msg);
            let sc = SignedCert {
                cert: good(),
                signature: Signature(sig.to_bytes().into()),
            };
            assert!(t.accept_cert(&digest(), &sc, CHIP).is_ok());
            // An Ed25519 signature never verifies against a P-256 root.
            let ed = sign(&root(), good());
            assert_eq!(
                t.accept_cert(&digest(), &ed, CHIP),
                Err(HandlerError::Denied)
            );
        }
    }
}
