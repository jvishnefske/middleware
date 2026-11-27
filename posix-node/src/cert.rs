//! The board's own mutual-TLS identity (MESH.md, "Certificates").
//!
//! The board makes its key itself and keeps it in its state directory, mode
//! 0600. It never sends the key: `sys/cert/csr` returns a CSR for the key, the
//! server signs it, and `sys/cert/install` takes the signed chain back. The
//! certificate's CN must be `kd240-<node id>`, the name the ACL binds to this
//! board's keys, so a board can present only its own identity.
//!
//! `install` writes three files in the store: `cert` (the chain, leaf first),
//! `ca` (the root) and `env` (the router URL and the TLS directory, for the
//! service's EnvironmentFile). It is refused over the network on a claimed board
//! until installs are signed by the pinned key; over the local serial port it is
//! the physical authority, as for the rest of the board.
//!
//! The node reads these files when it starts, so a new certificate takes effect
//! after a restart.

use std::cell::RefCell;
use std::io::Write;
use std::path::{Path, PathBuf};

use middleware::Key;
use middleware::SenderId;
use middleware::loader::trust::{P256_KEY_LEN, TrustState};
use middleware::secure_chip::{SecureChip, Slot, ensure_key};
use middleware::transport::HandlerError;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::pkcs8::DecodePrivateKey;
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, PublicKeyData, SigningKey};
use x509_parser::prelude::{FromDer, X509Certificate};

/// The file names in the store.
pub const KEY: &str = "key";
pub const CERT: &str = "cert";
pub const CA: &str = "ca";
pub const ENV: &str = "env";

/// `sys/cert/csr` and `sys/cert/install`.
pub const CSR_KEY: &str = "sys/cert/csr";
pub const INSTALL_KEY: &str = "sys/cert/install";

/// Where the board's private key lives. Every use goes through
/// [`SecureChip`], so the key never leaves the backend.
#[derive(Debug, Clone)]
pub enum Chip {
    /// A PKCS#8 PEM file in the store, mode 0600: the default.
    File(PathBuf),
    /// A key in a PKCS#11 token (a TPM through tpm2-pkcs11 on Linux), under
    /// `label`. The PIN comes from `PKCS11_PIN`, as for `mwctl`.
    #[cfg(target_os = "linux")]
    Pkcs11 {
        at: mwctl::pkcs11::Location,
        label: String,
    },
}

/// The board's certificate store: `dir` holds the files above.
#[derive(Debug)]
pub struct CertStore {
    dir: PathBuf,
    node: SenderId,
    chip: Chip,
}

impl CertStore {
    /// A store whose key is a file in `dir`.
    pub fn new(dir: impl Into<PathBuf>, node: SenderId) -> Self {
        let dir = dir.into();
        let chip = Chip::File(dir.join(KEY));
        CertStore { dir, node, chip }
    }

    /// A store with the key on `chip`.
    pub fn with_chip(dir: impl Into<PathBuf>, node: SenderId, chip: Chip) -> Self {
        CertStore {
            dir: dir.into(),
            node,
            chip,
        }
    }

    /// Picks the backend from the environment. `MW_CERT_PKCS11_LABEL` names
    /// a token key (Linux only; the token and module come from `mwctl`'s
    /// `PKCS11_*` settings); unset, the key is a file.
    pub fn from_env(
        dir: impl Into<PathBuf>,
        node: SenderId,
        var: impl Fn(&str) -> Option<String>,
    ) -> Self {
        let dir = dir.into();
        #[cfg(target_os = "linux")]
        if let Some(label) = var("MW_CERT_PKCS11_LABEL").filter(|l| !l.is_empty()) {
            let at = mwctl::pkcs11::Location::from_settings();
            return CertStore::with_chip(dir, node, Chip::Pkcs11 { at, label });
        }
        let _ = var;
        CertStore::new(dir, node)
    }

    /// The CN this board's certificate must carry.
    pub fn cn(&self) -> String {
        format!("kd240-{}", crate::node_id::hex(self.node))
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// The CSR for the board's key, PEM (`sys/cert/csr`).
    fn csr(&self, out: &mut [u8]) -> Result<usize, HandlerError> {
        let key = chip_key(self.chip.clone())?;
        let mut params = CertificateParams::default();
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, self.cn());
        params.distinguished_name = dn;
        let csr = params
            .serialize_request(&key)
            .map_err(|_| HandlerError::Internal)?;
        let pem = csr.pem().map_err(|_| HandlerError::Internal)?;
        copy_out(pem.as_bytes(), out)
    }

    /// Installs the signed chain and the router URL (`sys/cert/install`).
    /// The body is `router=<locator>` on the first line, then the chain's PEM:
    /// the board's certificate, then the CA that signed it.
    fn install(
        &self,
        body: &[u8],
        trust: Option<&TrustState>,
        network: bool,
    ) -> Result<(), HandlerError> {
        // Fail closed: over the network only a board that is known to be
        // unclaimed takes an install. A claimed board needs signed installs
        // (not built yet), and an unknown trust state (`None`) is refused.
        if network && !matches!(trust, Some(TrustState::Unclaimed)) {
            return Err(HandlerError::Denied);
        }
        let text = std::str::from_utf8(body).map_err(|_| HandlerError::InvalidRequest)?;
        let (first, pem) = text.split_once('\n').ok_or(HandlerError::InvalidRequest)?;
        let router = first
            .strip_prefix("router=")
            .ok_or(HandlerError::InvalidRequest)?;
        if router.is_empty() || router.contains(['\n', '"', '\\', '#', ';', '=', '|', ' ']) {
            return Err(HandlerError::InvalidRequest);
        }
        let blocks = pem_blocks(pem)?;
        if blocks.len() != 2 {
            return Err(HandlerError::InvalidRequest); // the certificate, then its CA
        }
        let (leaf_der, ca_der) = (&blocks[0], &blocks[1]);
        let (_, leaf) =
            X509Certificate::from_der(leaf_der).map_err(|_| HandlerError::InvalidRequest)?;
        let (_, ca) =
            X509Certificate::from_der(ca_der).map_err(|_| HandlerError::InvalidRequest)?;
        // The key the certificate names is the board's own key.
        let key = chip_key(self.chip.clone())?;
        if leaf.public_key().raw != key.subject_public_key_info().as_slice() {
            return Err(HandlerError::Denied);
        }
        // The CN binds the certificate to this board.
        let cn = leaf
            .subject()
            .iter_common_name()
            .next()
            .and_then(|a| a.as_str().ok());
        if cn != Some(self.cn().as_str()) {
            return Err(HandlerError::Denied);
        }
        // Signed by the CA it comes with, which must be a CA.
        if leaf.issuer() != ca.subject() {
            return Err(HandlerError::Denied);
        }
        let is_ca = ca
            .basic_constraints()
            .ok()
            .flatten()
            .is_some_and(|bc| bc.value.ca);
        if !is_ca {
            return Err(HandlerError::Denied);
        }
        if leaf.verify_signature(Some(ca.public_key())).is_err() {
            return Err(HandlerError::Denied);
        }
        std::fs::create_dir_all(&self.dir).map_err(|_| HandlerError::Internal)?;
        write_atomic(&self.path(CA), pem_of(ca_der).as_bytes(), 0o644)?;
        write_atomic(
            &self.path(CERT),
            format!("{}{}", pem_of(leaf_der), pem_of(ca_der)).as_bytes(),
            0o644,
        )?;
        let env = format!(
            "MW_ZENOH_ROUTER={router}\nMW_ZENOH_TLS_DIR={}\n",
            self.dir.display()
        );
        write_atomic(&self.path(ENV), env.as_bytes(), 0o644)?;
        Ok(())
    }

    /// One `sys/cert/*` request. `trust` is the root of trust (installs refuse
    /// without it); `network` is whether the request came over the network.
    pub fn handle(
        &self,
        key: Key<'_>,
        body: &[u8],
        trust: Option<&TrustState>,
        network: bool,
        out: &mut [u8],
    ) -> Result<usize, HandlerError> {
        match key.as_str() {
            CSR_KEY => self.csr(out),
            INSTALL_KEY => {
                self.install(body, trust, network)?;
                Ok(0)
            }
            _ => Err(HandlerError::NotFound),
        }
    }
}

impl SecureChip for Chip {
    type Error = HandlerError;

    fn has_key(&mut self, slot: Slot) -> Result<bool, HandlerError> {
        if slot != Slot::DEVICE {
            return Ok(false);
        }
        match self {
            Chip::File(path) => Ok(path.exists()),
            // A token that cannot be opened holds no usable key: the next
            // `generate_key` then says why (its PIN, its module).
            #[cfg(target_os = "linux")]
            Chip::Pkcs11 { at, label } => Ok(token::open(at, label).is_ok()),
        }
    }

    fn generate_key(&mut self, slot: Slot) -> Result<(), HandlerError> {
        if slot != Slot::DEVICE {
            return Err(HandlerError::InvalidRequest);
        }
        match self {
            Chip::File(path) => {
                if path.exists() {
                    return Err(HandlerError::Denied);
                }
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir).map_err(|_| HandlerError::Internal)?;
                }
                let key = KeyPair::generate().map_err(|_| HandlerError::Internal)?;
                write_atomic(path, key.serialize_pem().as_bytes(), 0o600)
            }
            #[cfg(target_os = "linux")]
            Chip::Pkcs11 { at, label } => token::generate(at, label),
        }
    }

    fn public_key(&mut self, slot: Slot) -> Result<[u8; P256_KEY_LEN], HandlerError> {
        if slot != Slot::DEVICE {
            return Err(HandlerError::InvalidRequest);
        }
        let mut out = [0u8; P256_KEY_LEN];
        match self {
            Chip::File(path) => {
                let sk = file_key(path)?;
                let point = sk.verifying_key().to_encoded_point(true);
                out.copy_from_slice(point.as_bytes());
            }
            #[cfg(target_os = "linux")]
            Chip::Pkcs11 { at, label } => return token::public(at, label),
        }
        Ok(out)
    }

    fn sign(&mut self, slot: Slot, message: &[u8]) -> Result<[u8; 64], HandlerError> {
        if slot != Slot::DEVICE {
            return Err(HandlerError::InvalidRequest);
        }
        match self {
            Chip::File(path) => {
                let sk = file_key(path)?;
                let sig: p256::ecdsa::Signature =
                    p256::ecdsa::signature::Signer::sign(&sk, message);
                let mut raw = [0u8; 64];
                raw.copy_from_slice(&sig.to_bytes());
                Ok(raw)
            }
            #[cfg(target_os = "linux")]
            Chip::Pkcs11 { at, label } => token::sign(at, label, message),
        }
    }
}

/// The file key, parsed from its PKCS#8 PEM.
fn file_key(path: &Path) -> Result<p256::ecdsa::SigningKey, HandlerError> {
    let pem = std::fs::read_to_string(path).map_err(|_| HandlerError::Internal)?;
    p256::ecdsa::SigningKey::from_pkcs8_pem(&pem).map_err(|_| HandlerError::Internal)
}

/// The PKCS#11 calls, each opening its own session (the token is the
/// authority; no key or session outlives a request).
#[cfg(target_os = "linux")]
mod token {
    use super::*;
    use middleware::loader::trust::{KeyKind, PublicKey};
    use mwctl::pkcs11::{Location, Pkcs11Signer};
    use mwctl::sign::Signer as _;

    /// A token failure, logged (the error never carries a PIN) and reported to
    /// the requester as `Internal`.
    fn fail(step: &str, e: impl std::fmt::Display) -> HandlerError {
        eprintln!("mw-posix-node: cert: token {step}: {e}");
        HandlerError::Internal
    }

    pub fn open(at: &Location, label: &str) -> Result<Pkcs11Signer, HandlerError> {
        let pin = mwctl::pkcs11::pin(mwctl::pkcs11::ENV_PIN).map_err(|e| fail("pin", e))?;
        Pkcs11Signer::open(at, label, &pin).map_err(|e| fail("open", e))
    }

    pub fn generate(at: &Location, label: &str) -> Result<(), HandlerError> {
        let pin = mwctl::pkcs11::pin(mwctl::pkcs11::ENV_PIN).map_err(|e| fail("pin", e))?;
        mwctl::pkcs11::generate(at, label, KeyKind::P256, &pin)
            .map(|_| ())
            .map_err(|e| fail("generate", e))
    }

    pub fn public(at: &Location, label: &str) -> Result<[u8; P256_KEY_LEN], HandlerError> {
        match open(at, label)?.public_key() {
            PublicKey::P256(k) => Ok(k),
            _ => Err(HandlerError::Internal),
        }
    }

    pub fn sign(at: &Location, label: &str, message: &[u8]) -> Result<[u8; 64], HandlerError> {
        open(at, label)?
            .sign(message)
            .map(|s| s.0)
            .map_err(|e| fail("sign", e))
    }
}

/// The board's key as rcgen signs: the public point (uncompressed, what a
/// certificate carries) and signatures made on the chip.
struct ChipKey {
    chip: RefCell<Chip>,
    point: Vec<u8>,
}

impl PublicKeyData for ChipKey {
    fn der_bytes(&self) -> &[u8] {
        &self.point
    }
    fn algorithm(&self) -> &'static rcgen::SignatureAlgorithm {
        &rcgen::PKCS_ECDSA_P256_SHA256
    }
}

impl SigningKey for ChipKey {
    fn sign(&self, msg: &[u8]) -> Result<Vec<u8>, rcgen::Error> {
        // The chip hashes with SHA-256 and returns raw r||s; certificates
        // carry the DER form.
        let raw = self
            .chip
            .borrow_mut()
            .sign(Slot::DEVICE, msg)
            .map_err(|_| rcgen::Error::RemoteKeyError)?;
        Ok(der_ecdsa(&raw))
    }
}

/// The board's key on `chip`, made first if the chip has none.
fn chip_key(mut chip: Chip) -> Result<ChipKey, HandlerError> {
    let compressed = ensure_key(&mut chip, Slot::DEVICE)?;
    let point = p256::PublicKey::from_sec1_bytes(&compressed)
        .map_err(|_| HandlerError::Internal)?
        .to_encoded_point(false)
        .as_bytes()
        .to_vec();
    Ok(ChipKey {
        chip: RefCell::new(chip),
        point,
    })
}

/// ECDSA `r || s` as the DER `SEQUENCE { INTEGER r, INTEGER s }` a certificate
/// carries. Both integers are 32 bytes at most, so short-form lengths suffice.
fn der_ecdsa(raw: &[u8; 64]) -> Vec<u8> {
    fn int(b: &[u8]) -> Vec<u8> {
        let mut v: &[u8] = b;
        while v.len() > 1 && v[0] == 0 {
            v = &v[1..];
        }
        let mut out = vec![0x02];
        if v[0] & 0x80 != 0 {
            out.push(v.len() as u8 + 1);
            out.push(0);
        } else {
            out.push(v.len() as u8);
        }
        out.extend_from_slice(v);
        out
    }
    let body = [int(&raw[..32]), int(&raw[32..])].concat();
    let mut out = vec![0x30, body.len() as u8];
    out.extend_from_slice(&body);
    out
}

fn copy_out(bytes: &[u8], out: &mut [u8]) -> Result<usize, HandlerError> {
    let dst = out
        .get_mut(..bytes.len())
        .ok_or(HandlerError::BufferTooSmall)?;
    dst.copy_from_slice(bytes);
    Ok(bytes.len())
}

fn pem_of(der: &[u8]) -> String {
    let b64 = base64_std(der);
    let mut s = String::from("-----BEGIN CERTIFICATE-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        s.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        s.push('\n');
    }
    s.push_str("-----END CERTIFICATE-----\n");
    s
}

/// The DER of each CERTIFICATE block in `pem`.
fn pem_blocks(pem: &str) -> Result<Vec<Vec<u8>>, HandlerError> {
    let mut out = Vec::new();
    let mut body = None::<String>;
    for line in pem.lines() {
        match line.trim() {
            "-----BEGIN CERTIFICATE-----" => body = Some(String::new()),
            "-----END CERTIFICATE-----" => {
                let b = body.take().ok_or(HandlerError::InvalidRequest)?;
                out.push(base64_decode(&b).ok_or(HandlerError::InvalidRequest)?);
            }
            l => {
                if let Some(b) = body.as_mut() {
                    b.push_str(l);
                } else if !l.is_empty() {
                    return Err(HandlerError::InvalidRequest);
                }
            }
        }
    }
    if body.is_some() {
        return Err(HandlerError::InvalidRequest);
    }
    Ok(out)
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_std(data: &[u8]) -> String {
    let mut out = String::new();
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= c.len() {
                out.push(B64[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    for c in s.bytes().filter(|c| !c.is_ascii_whitespace() && *c != b'=') {
        let v = B64.iter().position(|&x| x == c)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Writes `data` to `path` through a temporary file and a rename, so a power
/// cut leaves the old file or the new one, never half of each.
fn write_atomic(path: &Path, data: &[u8], mode: u32) -> Result<(), HandlerError> {
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(&tmp)
        .map_err(|_| HandlerError::Internal)?;
    f.write_all(data).map_err(|_| HandlerError::Internal)?;
    f.sync_all().map_err(|_| HandlerError::Internal)?;
    drop(f);
    std::fs::rename(&tmp, path).map_err(|_| HandlerError::Internal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::loader::trust::{PublicKey, Version};
    use rcgen::{BasicConstraints, CertificateSigningRequestParams, IsCa, Issuer};

    const NODE: SenderId = SenderId([0x0a, 0x1b, 0x2c, 0x3d, 0x4e, 0x5f, 0x60, 0x71]);

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mw-cert-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// The CA of the tests: a root that signs the board's CSR as `cn`.
    fn ca() -> (Issuer<'static, KeyPair>, String) {
        let key = KeyPair::generate().unwrap();
        let mut p = CertificateParams::default();
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "mw-test-ca");
        p.distinguished_name = dn;
        p.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        let cert = p.self_signed(&key).unwrap();
        let pem = cert.pem();
        (Issuer::new(p, key), pem)
    }

    fn csr_of(store: &CertStore) -> String {
        let mut out = vec![0u8; 4096];
        let n = store
            .handle(Key::from_static(CSR_KEY), &[], None, false, &mut out)
            .unwrap();
        String::from_utf8(out[..n].to_vec()).unwrap()
    }

    /// The chain body: `router=` then the leaf (the CSR signed as `cn`), then the CA.
    fn body(
        csr_pem: &str,
        cn: &str,
        issuer: &Issuer<'static, KeyPair>,
        ca_pem: &str,
        router: &str,
    ) -> Vec<u8> {
        let mut csr = CertificateSigningRequestParams::from_pem(csr_pem).unwrap();
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, cn);
        csr.params.distinguished_name = dn;
        let leaf = csr.signed_by(issuer).unwrap();
        format!("router={router}\n{}{}", leaf.pem(), ca_pem).into_bytes()
    }

    fn install(
        store: &CertStore,
        body: &[u8],
        trust: Option<&TrustState>,
        network: bool,
    ) -> Result<usize, HandlerError> {
        let mut out = [0u8; 8];
        store.handle(
            Key::from_static(INSTALL_KEY),
            body,
            trust,
            network,
            &mut out,
        )
    }

    /// The DER of the CSR's PEM block.
    fn csr_der(csr_pem: &str) -> Vec<u8> {
        let body: String = csr_pem
            .lines()
            .filter(|l| !l.starts_with("-----"))
            .collect();
        base64_decode(&body).unwrap()
    }

    #[test]
    fn the_csr_is_signed_by_the_key_it_names() {
        // The request's own signature checks against the public key it
        // carries, which is the file key the chip holds (not a second key).
        let dir = tmp("csr-sig");
        let store = CertStore::new(&dir, NODE);
        let csr = csr_of(&store);
        let der = csr_der(&csr);
        let (_, req) =
            x509_parser::certification_request::X509CertificationRequest::from_der(&der).unwrap();
        req.verify_signature().expect("the CSR signature verifies");
        let file_key = file_key(&dir.join(KEY)).unwrap();
        let point = file_key.verifying_key().to_encoded_point(false);
        assert_eq!(
            req.certification_request_info
                .subject_pki
                .subject_public_key
                .data
                .as_ref(),
            point.as_bytes()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_key_backend_is_a_file_unless_a_token_label_is_named() {
        let dir = tmp("backend");
        let file = CertStore::from_env(&dir, NODE, |_| None);
        assert!(matches!(file.chip, Chip::File(ref p) if p == &dir.join(KEY)));
        let empty = CertStore::from_env(&dir, NODE, |k| {
            (k == "MW_CERT_PKCS11_LABEL").then(String::new)
        });
        assert!(
            matches!(empty.chip, Chip::File(_)),
            "an empty label is no label"
        );
        #[cfg(target_os = "linux")]
        {
            let tok = CertStore::from_env(&dir, NODE, |k| {
                (k == "MW_CERT_PKCS11_LABEL").then(|| "kd240-dev".into())
            });
            assert!(matches!(tok.chip, Chip::Pkcs11 { ref label, .. } if label == "kd240-dev"));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The CSR's subject public key bytes (the uncompressed point).
    fn csr_point(csr_pem: &str) -> Vec<u8> {
        let der = csr_der(csr_pem);
        let (_, req) =
            x509_parser::certification_request::X509CertificationRequest::from_der(&der).unwrap();
        req.certification_request_info
            .subject_pki
            .subject_public_key
            .data
            .to_vec()
    }

    /// A key in a SoftHSM2 token, the same path a TPM takes through
    /// tpm2-pkcs11. Ignored in the ordinary run: it needs a token, so it is
    /// run with the environment set by the caller, as mwctl's token test is:
    ///
    ///   SOFTHSM2_CONF=<conf> PKCS11_MODULE=<libsofthsm2.so> \
    ///   PKCS11_TOKEN=<label> PKCS11_PIN=<pin> \
    ///   cargo test -p mw-posix-node --lib -- --ignored a_token_key
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "needs a PKCS#11 token; see the comment above"]
    fn a_token_key_makes_the_csr_and_signs_it() {
        let dir = tmp("token");
        let label = "mw-cert-test";
        let chip = Chip::Pkcs11 {
            at: mwctl::pkcs11::Location::from_settings(),
            label: label.into(),
        };
        let store = CertStore::with_chip(&dir, NODE, chip);
        let first = csr_of(&store);
        let der = csr_der(&first);
        let (_, req) =
            x509_parser::certification_request::X509CertificationRequest::from_der(&der).unwrap();
        req.verify_signature()
            .expect("the token's signature verifies the CSR");
        assert!(
            !dir.join(KEY).exists(),
            "the private key never reaches the store"
        );
        // The key is made once: a second request names the same key.
        assert_eq!(csr_point(&csr_of(&store)), csr_point(&first));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_board_makes_its_key_once_and_installs_a_chain_it_signed_for() {
        let dir = tmp("round");
        let store = CertStore::new(&dir, NODE);
        let csr1 = csr_of(&store);
        assert!(csr1.contains("BEGIN CERTIFICATE REQUEST"));
        let key1 = std::fs::read_to_string(dir.join(KEY)).unwrap();
        let _csr2 = csr_of(&store);
        assert_eq!(
            std::fs::read_to_string(dir.join(KEY)).unwrap(),
            key1,
            "the key is made once and kept"
        );
        let (issuer, ca_pem) = ca();
        let b = body(&csr1, &store.cn(), &issuer, &ca_pem, "tls/mesh.example:443");
        assert_eq!(
            install(&store, &b, Some(&TrustState::Unclaimed), false),
            Ok(0)
        );
        let env = std::fs::read_to_string(dir.join(ENV)).unwrap();
        assert!(env.contains("MW_ZENOH_ROUTER=tls/mesh.example:443\n"));
        assert!(env.contains(&format!("MW_ZENOH_TLS_DIR={}", dir.display())));
        assert!(
            std::fs::read_to_string(dir.join(CERT))
                .unwrap()
                .matches("BEGIN CERTIFICATE")
                .count()
                == 2
        );
        let mode = std::fs::metadata(dir.join(KEY)).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            mode.permissions().mode() & 0o777,
            0o600,
            "the key is private"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_certificate_must_name_this_board_and_hold_its_key() {
        let dir = tmp("refuse");
        let store = CertStore::new(&dir, NODE);
        let csr = csr_of(&store);
        let (issuer, ca_pem) = ca();
        let other_board = body(&csr, "kd240-ffffffffffffffff", &issuer, &ca_pem, "tls/x:1");
        assert_eq!(
            install(&store, &other_board, None, false),
            Err(HandlerError::Denied),
            "another board's CN"
        );
        // A certificate for a key the board does not hold.
        let stranger = KeyPair::generate().unwrap();
        let mut sp = CertificateSigningRequestParams::from_pem(&csr).unwrap();
        sp.params.distinguished_name = {
            let mut d = DistinguishedName::new();
            d.push(DnType::CommonName, store.cn());
            d
        };
        let other_key_csr = {
            let p = CertificateParams::default();
            p.serialize_request(&stranger).unwrap().pem().unwrap()
        };
        let forged = body(&other_key_csr, &store.cn(), &issuer, &ca_pem, "tls/x:1");
        assert_eq!(
            install(&store, &forged, None, false),
            Err(HandlerError::Denied),
            "not the board's key"
        );
        assert!(
            !dir.join(CERT).exists(),
            "nothing is installed on a refusal"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_network_install_needs_the_pinned_key_on_a_claimed_board() {
        let dir = tmp("net");
        let store = CertStore::new(&dir, NODE);
        let csr = csr_of(&store);
        let (issuer, ca_pem) = ca();
        let b = body(&csr, &store.cn(), &issuer, &ca_pem, "tls/x:1");
        let claimed = TrustState::Claimed {
            key: PublicKey::Ed25519([0; 32]),
            min_version: Version(0),
        };
        assert_eq!(
            install(&store, &b, Some(&claimed), true),
            Err(HandlerError::Denied)
        );
        assert_eq!(
            install(&store, &b, Some(&claimed), false),
            Ok(0),
            "over serial the port is the authority"
        );
        assert_eq!(
            install(&store, &b, None, true),
            Err(HandlerError::Denied),
            "an unknown trust state is refused over the network"
        );
        assert_eq!(
            install(&store, &b, Some(&TrustState::Unclaimed), true),
            Ok(0),
            "an unclaimed board takes a network install"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bad_router_locator_or_chain_is_refused() {
        let dir = tmp("bad");
        let store = CertStore::new(&dir, NODE);
        let csr = csr_of(&store);
        let (issuer, ca_pem) = ca();
        let spaced = body(&csr, &store.cn(), &issuer, &ca_pem, "tls/x:1 extra");
        assert_eq!(
            install(&store, &spaced, None, false),
            Err(HandlerError::InvalidRequest)
        );
        let leaf_only = format!(
            "router=tls/x:1\n{}",
            csr_pem_leaf(&csr, &store.cn(), &issuer)
        );
        assert_eq!(
            install(&store, leaf_only.as_bytes(), None, false),
            Err(HandlerError::InvalidRequest)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn csr_pem_leaf(csr: &str, cn: &str, issuer: &Issuer<'static, KeyPair>) -> String {
        let mut c = CertificateSigningRequestParams::from_pem(csr).unwrap();
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, cn);
        c.params.distinguished_name = dn;
        c.signed_by(issuer).unwrap().pem()
    }

    #[test]
    fn base64_round_trips() {
        for data in [
            &b""[..],
            b"a",
            b"ab",
            b"abc",
            b"abcd",
            &[0u8, 255, 17, 42][..],
        ] {
            assert_eq!(base64_decode(&base64_std(data)).unwrap(), data);
        }
    }
}
