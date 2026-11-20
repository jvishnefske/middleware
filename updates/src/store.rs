//! The server's releases on disk (UP5).
//!
//! ```text
//! <dir>/images/<sha256 hex>                     the image bytes
//! <dir>/certs/<channel>/<target>/<kind>.cert    its [cert][sig] (kind: payload, fw)
//! ```
//!
//! A publish is accepted ([`Store::check`]) only when the cert verifies
//! against the **root public key** (the server holds no private key), its
//! channel and target are those of the key it was published on, the image
//! has the cert's length and SHA-256, a firmware cert names one chip (not
//! `any`), and its version is above the stored release of the same kind
//! (devices would refuse a downgrade anyway; refusing it here keeps
//! `latest` monotonic). Files are written to a temporary name and renamed,
//! image first, so a crash never leaves a cert without its image.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use middleware::loader::release::{ANY_TARGET, CertKind, verify_cert};
use middleware::loader::trust::{PublicKey, Version};

use crate::keys::valid_name;
use crate::wire::{CHUNK, Release};

/// Why a publish was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishError {
    /// The channel or target of the key is not a valid name.
    BadKey,
    /// The cert's channel or target differs from the key's.
    WrongPlace,
    /// The root did not sign it.
    Signature,
    /// The image's length or SHA-256 is not the cert's.
    Image,
    /// A firmware release for `any` chip.
    AnyFirmware,
    /// Not above the stored release.
    NotNewer {
        /// The stored version.
        stored: Version,
    },
    /// The store failed.
    Io(String),
}

impl std::fmt::Display for PublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PublishError::BadKey => write!(f, "bad channel or target"),
            PublishError::WrongPlace => write!(f, "cert channel/target differ from the key's"),
            PublishError::Signature => write!(f, "not signed by the root key"),
            PublishError::Image => write!(f, "image length or sha256 differs from the cert"),
            PublishError::AnyFirmware => write!(f, "a firmware release must name one chip"),
            PublishError::NotNewer { stored } => {
                write!(f, "version not above the stored {}", stored.0)
            }
            PublishError::Io(e) => write!(f, "store: {e}"),
        }
    }
}

impl std::error::Error for PublishError {}

/// The release store under one directory, for one root key.
#[derive(Debug)]
pub struct Store {
    dir: PathBuf,
    root: PublicKey,
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).into()
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::File::open(&tmp)?.sync_all()?;
    fs::rename(&tmp, path)?;
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

impl Store {
    /// The store in `dir` (created if missing) for releases signed by
    /// `root`.
    pub fn open(dir: impl Into<PathBuf>, root: PublicKey) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(dir.join("images"))?;
        fs::create_dir_all(dir.join("certs"))?;
        Ok(Store { dir, root })
    }

    /// The root key.
    pub fn root(&self) -> &PublicKey {
        &self.root
    }

    fn cert_path(&self, channel: &str, target: &str, kind: CertKind) -> PathBuf {
        self.dir
            .join("certs")
            .join(channel)
            .join(target)
            .join(format!("{}.cert", kind.name()))
    }

    fn image_path(&self, sha: &[u8; 32]) -> PathBuf {
        self.dir.join("images").join(crate::hex(sha))
    }

    /// The stored release of `kind` for `channel`/`target`, if any (and if
    /// it still verifies: a file that does not is ignored).
    pub fn release(&self, channel: &str, target: &str, kind: CertKind) -> Option<Release> {
        if !(valid_name(channel) && valid_name(target)) {
            return None;
        }
        let body = fs::read(self.cert_path(channel, target, kind)).ok()?;
        let r = Release::from_body(body)?;
        (r.kind() == kind && verify_cert(&self.root, &r.signed())).then_some(r)
    }

    /// The newest release of each kind (payload first).
    pub fn latest(&self, channel: &str, target: &str) -> Vec<Release> {
        [CertKind::Payload, CertKind::Firmware]
            .into_iter()
            .filter_map(|k| self.release(channel, target, k))
            .collect()
    }

    /// Checks a publish of `release` with `image` on `channel`/`target`
    /// (module docs).
    pub fn check(
        &self,
        channel: &str,
        target: &str,
        release: &Release,
        image: &[u8],
    ) -> Result<(), PublishError> {
        if !(valid_name(channel) && valid_name(target)) {
            return Err(PublishError::BadKey);
        }
        let signed = release.signed();
        let c = &signed.cert;
        if c.channel != channel || c.target != target {
            return Err(PublishError::WrongPlace);
        }
        if !verify_cert(&self.root, &signed) {
            return Err(PublishError::Signature);
        }
        if u32::try_from(image.len()).ok() != Some(c.len) || sha256(image) != c.sha256 {
            return Err(PublishError::Image);
        }
        if c.kind == CertKind::Firmware && c.target == ANY_TARGET {
            return Err(PublishError::AnyFirmware);
        }
        if let Some(stored) = self.release(channel, target, c.kind)
            && c.version <= stored.version()
        {
            return Err(PublishError::NotNewer {
                stored: stored.version(),
            });
        }
        Ok(())
    }

    /// Checks and stores a publish; the image first, then the cert.
    pub fn publish(
        &self,
        channel: &str,
        target: &str,
        release: &Release,
        image: &[u8],
    ) -> Result<(), PublishError> {
        self.check(channel, target, release, image)?;
        let io = |e: io::Error| PublishError::Io(e.to_string());
        let c = release.cert();
        write_atomic(&self.image_path(&c.sha256), image).map_err(io)?;
        write_atomic(&self.cert_path(channel, target, c.kind), release.body()).map_err(io)
    }

    /// Up to [`CHUNK`] bytes of image `sha` at `offset` (empty at the
    /// end); `None` when the image is not stored or `offset` is past it.
    pub fn chunk(&self, sha: &[u8; 32], offset: u32) -> Option<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = fs::File::open(self.image_path(sha)).ok()?;
        let len = f.metadata().ok()?.len();
        if u64::from(offset) > len {
            return None;
        }
        f.seek(SeekFrom::Start(offset.into())).ok()?;
        let mut buf = Vec::with_capacity(CHUNK);
        f.take(CHUNK as u64).read_to_end(&mut buf).ok()?;
        Some(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::tests::{cert, seed};
    use middleware::loader::release::SoftwareCert;
    use mwctl::sign::Seed;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mw-updates-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    fn store(name: &str) -> Store {
        Store::open(dir(name), seed().public_key()).unwrap()
    }

    fn signed(c: SoftwareCert<'_>) -> Release {
        Release::sign(&c, &seed()).unwrap()
    }

    #[test]
    fn up5_publish_stores_and_serves_the_latest_of_each_kind() {
        let s = store("ok");
        let img: Vec<u8> = (0..(CHUNK + 10)).map(|i| i as u8).collect();
        let p = signed(cert(CertKind::Payload, "rp2350", 5, &img));
        s.publish("test", "rp2350", &p, &img).unwrap();
        assert_eq!(s.latest("test", "rp2350"), vec![p.clone()]);
        let fw = signed(cert(CertKind::Firmware, "rp2350", 0x0001_0002, &img));
        s.publish("test", "rp2350", &fw, &img).unwrap();
        assert_eq!(s.latest("test", "rp2350"), vec![p.clone(), fw]);
        assert!(s.latest("test", "esp32s3").is_empty());
        assert!(s.latest("stable", "rp2350").is_empty());
        // Chunks.
        let sha = p.cert().sha256;
        assert_eq!(s.chunk(&sha, 0).unwrap(), img[..CHUNK]);
        assert_eq!(s.chunk(&sha, CHUNK as u32).unwrap(), img[CHUNK..]);
        assert_eq!(s.chunk(&sha, img.len() as u32).unwrap(), Vec::<u8>::new());
        assert_eq!(s.chunk(&sha, img.len() as u32 + 1), None);
        assert_eq!(s.chunk(&[0; 32], 0), None);
        // A newer payload replaces the older.
        let p6 = signed(cert(CertKind::Payload, "rp2350", 6, &img));
        s.publish("test", "rp2350", &p6, &img).unwrap();
        assert_eq!(s.release("test", "rp2350", CertKind::Payload), Some(p6));
    }

    #[test]
    fn up5_publish_refusals() {
        let s = store("refuse");
        let img = b"payload".to_vec();
        let good = signed(cert(CertKind::Payload, "rp2350", 5, &img));
        assert_eq!(
            s.publish("stable", "rp2350", &good, &img),
            Err(PublishError::WrongPlace)
        );
        assert_eq!(
            s.publish("test", "esp32s3", &good, &img),
            Err(PublishError::WrongPlace)
        );
        assert_eq!(
            s.publish("Test", "rp2350", &good, &img),
            Err(PublishError::BadKey)
        );
        // Another key, a wrong image.
        let mallory = Release::sign(
            &cert(CertKind::Payload, "rp2350", 5, &img),
            &Seed::new([9; 32]),
        )
        .unwrap();
        assert_eq!(
            s.publish("test", "rp2350", &mallory, &img),
            Err(PublishError::Signature)
        );
        assert_eq!(
            s.publish("test", "rp2350", &good, b"other"),
            Err(PublishError::Image)
        );
        // Firmware for any chip.
        let any = signed(cert(CertKind::Firmware, "any", 5, &img));
        assert_eq!(
            s.publish("test", "any", &any, &img),
            Err(PublishError::AnyFirmware)
        );
        // Downgrade and replay.
        s.publish("test", "rp2350", &good, &img).unwrap();
        assert_eq!(
            s.publish("test", "rp2350", &good, &img),
            Err(PublishError::NotNewer { stored: Version(5) })
        );
        let old = signed(cert(CertKind::Payload, "rp2350", 4, &img));
        assert!(matches!(
            s.publish("test", "rp2350", &old, &img),
            Err(PublishError::NotNewer { .. })
        ));
        // Nothing stored for the refusals.
        assert_eq!(s.latest("test", "rp2350"), vec![good]);
    }

    #[test]
    fn up5_a_cert_file_that_no_longer_verifies_is_ignored() {
        let d = dir("tamper");
        let s = Store::open(&d, seed().public_key()).unwrap();
        let img = b"x".to_vec();
        let good = signed(cert(CertKind::Payload, "rp2350", 5, &img));
        s.publish("test", "rp2350", &good, &img).unwrap();
        let path = d.join("certs/test/rp2350/payload.cert");
        let mut body = fs::read(&path).unwrap();
        *body.last_mut().unwrap() ^= 1;
        fs::write(&path, body).unwrap();
        assert!(s.latest("test", "rp2350").is_empty());
    }
}
