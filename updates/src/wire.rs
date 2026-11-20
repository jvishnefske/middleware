//! The payloads of the updates keys (crate docs). Pure.

use middleware::loader::release::{
    CertKind, RELEASE_MESSAGE_MAX, SignedCert, SoftwareCert, release_message,
};
use middleware::loader::trust::{Signature, Version};
use mwctl::sign::{SignError, Signer};

/// Largest image chunk one `img` query returns.
pub const CHUNK: usize = 64 * 1024;

/// A release: the commit body `[cert][sig 64]`, validated on construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    body: Vec<u8>,
}

impl Release {
    /// A release from its body; `None` when it is not exactly a
    /// `[cert][sig]`.
    pub fn from_body(body: Vec<u8>) -> Option<Self> {
        SignedCert::decode(&body)?;
        Some(Release { body })
    }

    /// Signs `cert` with `signer` over the release message.
    pub fn sign(cert: &SoftwareCert<'_>, signer: &dyn Signer) -> Result<Self, SignError> {
        let mut buf = [0u8; RELEASE_MESSAGE_MAX];
        let msg = release_message(cert, &mut buf)
            .ok_or_else(|| SignError::Token("the cert does not encode".into()))?;
        let signature: Signature = signer.sign(msg)?;
        let mut body = vec![0u8; middleware::loader::release::COMMIT_BODY_MAX];
        let n = SignedCert {
            cert: *cert,
            signature,
        }
        .encode(&mut body)
        .ok_or_else(|| SignError::Token("the cert does not encode".into()))?;
        body.truncate(n);
        Ok(Release { body })
    }

    /// The commit body.
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// The parsed cert and signature.
    pub fn signed(&self) -> SignedCert<'_> {
        SignedCert::decode(&self.body).expect("validated in from_body")
    }

    /// The cert.
    pub fn cert(&self) -> SoftwareCert<'_> {
        self.signed().cert
    }

    /// Payload or firmware.
    pub fn kind(&self) -> CertKind {
        self.cert().kind
    }

    /// The release version.
    pub fn version(&self) -> Version {
        self.cert().version
    }
}

/// `latest`: `[n u16 LE][release n]` per release.
pub fn encode_latest(releases: &[Release]) -> Vec<u8> {
    let mut out = Vec::new();
    for r in releases {
        out.extend_from_slice(&(r.body.len() as u16).to_le_bytes());
        out.extend_from_slice(&r.body);
    }
    out
}

/// Decodes a `latest` answer; `None` when malformed.
pub fn decode_latest(mut bytes: &[u8]) -> Option<Vec<Release>> {
    let mut out = Vec::new();
    while !bytes.is_empty() {
        let n = u16::from_le_bytes(bytes.get(..2)?.try_into().ok()?) as usize;
        let body = bytes.get(2..2 + n)?;
        out.push(Release::from_body(body.to_vec())?);
        bytes = &bytes[2 + n..];
    }
    Some(out)
}

/// `publish`: `[n u16 LE][release n][image]`.
pub fn encode_publish(release: &Release, image: &[u8]) -> Vec<u8> {
    let mut out = encode_latest(std::slice::from_ref(release));
    out.extend_from_slice(image);
    out
}

/// Decodes a `publish` payload into the release and the image.
pub fn decode_publish(bytes: &[u8]) -> Option<(Release, &[u8])> {
    let n = u16::from_le_bytes(bytes.get(..2)?.try_into().ok()?) as usize;
    let release = Release::from_body(bytes.get(2..2 + n)?.to_vec())?;
    Some((release, bytes.get(2 + n..)?))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use mwctl::sign::Seed;

    pub fn seed() -> Seed {
        Seed::new([7; 32])
    }

    pub fn cert(
        kind: CertKind,
        target: &'static str,
        version: u32,
        image: &[u8],
    ) -> SoftwareCert<'static> {
        SoftwareCert {
            kind,
            target,
            channel: "test",
            version: Version(version),
            len: image.len() as u32,
            sha256: mwctl::sign::image_sha256(image),
        }
    }

    #[test]
    fn up4_latest_and_publish_round_trip() {
        let img = b"image bytes".to_vec();
        let a = Release::sign(&cert(CertKind::Payload, "any", 3, &img), &seed()).unwrap();
        let b = Release::sign(&cert(CertKind::Firmware, "rp2350", 4, &img), &seed()).unwrap();
        let both = vec![a.clone(), b.clone()];
        assert_eq!(decode_latest(&encode_latest(&both)), Some(both));
        assert_eq!(decode_latest(&[]), Some(vec![]));
        let mut torn = encode_latest(std::slice::from_ref(&a));
        torn.pop();
        assert_eq!(decode_latest(&torn), None);
        let p = encode_publish(&a, &img);
        let (r, i) = decode_publish(&p).unwrap();
        assert_eq!((r, i), (a.clone(), img.as_slice()));
        assert_eq!(a.kind(), CertKind::Payload);
        assert_eq!(b.version(), Version(4));
        assert!(Release::from_body(vec![1, 2, 3]).is_none());
    }
}
