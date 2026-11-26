//! `mw-pki`: the Zenoh mesh's root of trust on a PKCS#11 token.
//!
//! The CA's private key is generated on, and never leaves, a PKCS#11 token:
//! SoftHSM in CI, an HSM or a TPM (tpm2-pkcs11) in production. `mw-pki`
//! signs through the token (`mwctl::pkcs11`, the same signer `mwctl` uses for
//! releases) and writes PEM files. The procedure, and the M-of-N ceremony for
//! production, are in `docs/pki-ceremony.md`.
//!
//! ```text
//! mw-pki ca     --key <LABEL> <OUT-DIR> <CN>           # the root certificate (ca.crt)
//! mw-pki client --key <LABEL> <CA.crt> <CSR> <CN> <OUT> # a kd240-<id> or tool-<id> cert
//! mw-pki server --key <LABEL> <CA.crt> <CSR> <OUT> <SAN>...  # the router's cert
//! ```
//!
//! The device's own key is never an input: `client` and `server` take only a
//! CSR, so a certificate is always for a key its holder made (MESH.md).
//! Token settings: `PKCS11_MODULE` and `PKCS11_TOKEN` (read by mwctl's
//! `Location::from_settings`; `--module` overrides the module), and the PIN
//! in `MW_PKI_PIN` (or in `~/.env`, mode 0600).

use middleware::secure_chip::{SecureChip, Slot};
use mwctl::pkcs11::{Location, Pkcs11Chip, pin};
use p256::elliptic_curve::rand_core::{OsRng, RngCore};
use p256::elliptic_curve::sec1::ToEncodedPoint;
use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, DistinguishedName,
    DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyUsagePurpose, PublicKeyData, SanType,
    SerialNumber, SignatureAlgorithm, SigningKey,
};
use x509_parser::certification_request::X509CertificationRequest;
use x509_parser::oid_registry::{OID_EC_P256, OID_KEY_TYPE_EC_PUBLIC_KEY};
use x509_parser::prelude::{FromDer, X509Certificate};
use x509_parser::x509::SubjectPublicKeyInfo;
use std::cell::RefCell;
use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The PIN variable `mw-pki` reads unless `--pin-env` names another.
const PIN_VAR: &str = "MW_PKI_PIN";

/// The CA's key as rcgen signs with it. The key is on a [`SecureChip`] (the
/// token, [`Pkcs11Chip`], in production: a TPM or an HSM), and every use of
/// it goes through the trait. mw-pki never makes a CA key: the slot must
/// already hold one (`mwctl key new --pkcs11 <LABEL>`, docs/pki-ceremony.md).
struct ChipKey<C: SecureChip> {
    chip: RefCell<C>,
    /// The uncompressed SEC1 point (65 bytes) of the chip's public key.
    point: Vec<u8>,
}

impl<C: SecureChip> ChipKey<C>
where
    C::Error: Display,
{
    /// The key in `chip`'s device slot; `name` says which key in errors.
    fn open(mut chip: C, name: &str) -> Result<Self, String> {
        if !chip.has_key(Slot::DEVICE).map_err(|e| e.to_string())? {
            return Err(format!(
                "no key '{name}' on the token (create it with `mwctl key new --pkcs11 {name}`; mw-pki never makes a CA key)"
            ));
        }
        let compressed = chip.public_key(Slot::DEVICE).map_err(|e| e.to_string())?;
        let point = p256::PublicKey::from_sec1_bytes(&compressed)
            .map_err(|e| e.to_string())?
            .to_encoded_point(false)
            .as_bytes()
            .to_vec();
        Ok(ChipKey { chip: RefCell::new(chip), point })
    }
}

impl<C: SecureChip> PublicKeyData for ChipKey<C> {
    fn der_bytes(&self) -> &[u8] {
        &self.point
    }
    fn algorithm(&self) -> &'static SignatureAlgorithm {
        &rcgen::PKCS_ECDSA_P256_SHA256
    }
}

impl<C: SecureChip> SigningKey for ChipKey<C> {
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

/// The token key `label` at `loc`, unlocked with the PIN in `pin_var`.
fn token_key(loc: &Location, label: &str, pin_var: &str) -> Result<ChipKey<Pkcs11Chip>, String> {
    let pin = pin(pin_var).map_err(|e| e.to_string())?;
    ChipKey::open(Pkcs11Chip::new(loc.clone(), label, pin), label)
}

/// The self-signed CA certificate (PEM) for `key`, named `cn`.
fn ca_cert<C: SecureChip>(key: &ChipKey<C>, cn: &str) -> Result<String, String> {
    let mut params = CertificateParams::default();
    params.distinguished_name = dn(cn);
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    validity(&mut params, 730);
    params.serial_number = Some(random_serial());
    Ok(params.self_signed(key).map_err(|e| e.to_string())?.pem())
}

/// What a certificate is for.
enum Role {
    /// A `kd240-<id>` or `tool-<id>` client.
    Client,
    /// The router, with these SANs (DNS names or IP addresses).
    Server(Vec<String>),
}

/// The certificate (PEM) for the key in `csr_pem`, named `cn` and signed by
/// `key` as the CA in `ca_pem`. Everything in it is set here: the subject,
/// SANs, key usages, EKU and the not-a-CA constraint. Nothing the CSR asks
/// for besides its public key is carried over, so a device cannot request a
/// CA certificate, another name, or extra extensions.
///
/// The CSR's key must be P-256 (what the mesh's devices make), and `ca_pem`
/// must be the certificate of `key` itself: a certificate signed by the chip
/// under another CA's name would not chain to anything.
fn issue<C: SecureChip>(
    key: &ChipKey<C>,
    ca_pem: &str,
    csr_pem: &str,
    cn: &str,
    role: &Role,
) -> Result<String, String> {
    ca_holds_key(ca_pem, &key.point)?;
    csr_key_is_p256(csr_pem)?;
    let issuer = Issuer::from_ca_cert_pem(ca_pem, key).map_err(|e| e.to_string())?;
    let mut csr = CertificateSigningRequestParams::from_pem(csr_pem).map_err(|e| e.to_string())?;
    let mut params = CertificateParams::default();
    match role {
        Role::Server(sans) => {
            params.subject_alt_names = sans
                .iter()
                .map(|s| match s.parse::<std::net::IpAddr>() {
                    Ok(ip) => Ok(SanType::IpAddress(ip)),
                    Err(_) => s
                        .clone()
                        .try_into()
                        .map(SanType::DnsName)
                        .map_err(|e| format!("SAN {s}: {e}")),
                })
                .collect::<Result<_, _>>()?;
            params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        }
        Role::Client => {
            if !client_cn_ok(cn) {
                return Err(format!(
                    "CN '{cn}' is not kd240-<16 lowercase hex digits> or tool-<id> (id [a-z0-9-], at most 63, no leading, trailing or doubled dash)"
                ));
            }
            params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        }
    }
    params.distinguished_name = dn(cn);
    params.is_ca = IsCa::ExplicitNoCa;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    validity(&mut params, 365);
    params.serial_number = Some(random_serial());
    // Only the CSR's public key survives; its requested subject and
    // extensions are dropped with the rest of its parameters.
    csr.params = params;
    Ok(csr.signed_by(&issuer).map_err(|e| e.to_string())?.pem())
}

/// `kd240-<node id>`, the node id exactly 16 lowercase hex digits, or
/// `tool-<id>`, the id 1 to 63 of `[a-z0-9-]` starting with `[a-z0-9]`,
/// with no trailing or doubled dash.
fn client_cn_ok(cn: &str) -> bool {
    let lower_alnum = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    if let Some(id) = cn.strip_prefix("kd240-") {
        return id.len() == 16 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    }
    if let Some(id) = cn.strip_prefix("tool-") {
        let b = id.as_bytes();
        return (1..=63).contains(&b.len())
            && lower_alnum(b[0])
            && !id.ends_with('-')
            && !id.contains("--")
            && b.iter().all(|&c| lower_alnum(c) || c == b'-');
    }
    false
}

/// A fresh random serial number: 20 bytes (RFC 5280's maximum) from the OS
/// CSPRNG, the top bit cleared so it is positive. rcgen would otherwise
/// derive it from the subject key, so every certificate for one CSR would
/// share it.
fn random_serial() -> SerialNumber {
    let mut b = [0u8; 20];
    OsRng.fill_bytes(&mut b);
    b[0] &= 0x7f;
    SerialNumber::from_slice(&b)
}

/// The P-256 public key in `spki`, if it is one.
fn p256_key(spki: &SubjectPublicKeyInfo<'_>) -> Option<p256::PublicKey> {
    let curve = spki.algorithm.parameters.as_ref()?.as_oid().ok()?;
    if spki.algorithm.algorithm != OID_KEY_TYPE_EC_PUBLIC_KEY || curve != OID_EC_P256 {
        return None;
    }
    p256::PublicKey::from_sec1_bytes(&spki.subject_public_key.data).ok()
}

/// The DER of the first PEM block in `pem`.
fn pem_der(pem: &str, what: &str) -> Result<Vec<u8>, String> {
    x509_parser::pem::parse_x509_pem(pem.as_bytes())
        .map(|(_, p)| p.contents)
        .map_err(|e| format!("{what}: not PEM ({e})"))
}

/// The CSR's key is a P-256 key.
fn csr_key_is_p256(csr_pem: &str) -> Result<(), String> {
    let der = pem_der(csr_pem, "the CSR")?;
    let (_, req) = X509CertificationRequest::from_der(&der).map_err(|e| format!("the CSR: {e}"))?;
    p256_key(&req.certification_request_info.subject_pki)
        .map(|_| ())
        .ok_or_else(|| "the CSR's key is not a P-256 key (the mesh's devices make P-256 keys)".into())
}

/// `ca_pem` is the certificate of the key whose uncompressed point is `point`.
fn ca_holds_key(ca_pem: &str, point: &[u8]) -> Result<(), String> {
    let der = pem_der(ca_pem, "the CA certificate")?;
    let (_, ca) = X509Certificate::from_der(&der).map_err(|e| format!("the CA certificate: {e}"))?;
    let chip = p256::PublicKey::from_sec1_bytes(point).map_err(|e| e.to_string())?;
    match p256_key(ca.public_key()) {
        Some(k) if k == chip => Ok(()),
        _ => Err("the CA certificate is not for the token's key (wrong CA.crt or wrong --key)".into()),
    }
}

/// Validity of a new certificate: from an hour ago (clock skew) for `days`.
/// rcgen's default runs 1975 to 4096, which some verifiers refuse.
fn validity(params: &mut CertificateParams, days: i64) {
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::hours(1);
    params.not_after = now + time::Duration::days(days);
}

/// `SEQUENCE { INTEGER r, INTEGER s }` from raw `r || s` (32 bytes each).
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

struct Args {
    module: Option<PathBuf>,
    key: Option<String>,
    pin_var: String,
    positional: Vec<String>,
    san: Vec<String>,
}

fn parse(argv: &[String]) -> Result<(String, Args), String> {
    let cmd = argv.first().ok_or_else(usage)?.clone();
    let mut a = Args {
        module: None,
        key: None,
        pin_var: PIN_VAR.to_owned(),
        positional: Vec::new(),
        san: Vec::new(),
    };
    let mut it = argv[1..].iter();
    while let Some(x) = it.next() {
        match x.as_str() {
            "--module" => a.module = Some(it.next().ok_or("--module needs a path")?.into()),
            "--key" => a.key = Some(it.next().ok_or("--key needs a token key label")?.clone()),
            "--pin-env" => a.pin_var = it.next().ok_or("--pin-env needs a name")?.clone(),
            other => a.positional.push(other.to_owned()),
        }
    }
    if cmd == "server" {
        // server: CA.crt CSR OUT SAN... (the first three stay positional).
        a.san = a.positional.split_off(a.positional.len().min(3));
    }
    Ok((cmd, a))
}

fn usage() -> String {
    "usage: mw-pki ca --key LABEL OUT-DIR CN | client --key LABEL CA.crt CSR CN OUT | server --key LABEL CA.crt CSR OUT SAN...".into()
}

fn read(p: &str) -> Result<String, String> {
    std::fs::read_to_string(p).map_err(|e| format!("{p}: {e}"))
}

fn write_pem(p: &Path, pem: &str) -> Result<(), String> {
    std::fs::write(p, pem).map_err(|e| format!("{}: {e}", p.display()))
}

fn run(argv: &[String]) -> Result<String, String> {
    let (cmd, a) = parse(argv)?;
    let mut loc = Location::from_settings();
    if let Some(m) = &a.module {
        loc.module = m.clone();
    }
    let key_label = a.key.clone().ok_or("--key is required: the token's key label")?;
    let p = a.positional.clone();
    match cmd.as_str() {
        "ca" => {
            if p.len() != 2 {
                return Err(usage());
            }
            let (out, cn) = (PathBuf::from(&p[0]), p[1].clone());
            let key = token_key(&loc, &key_label, &a.pin_var)?;
            let pem = ca_cert(&key, &cn)?;
            std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
            write_pem(&out.join("ca.crt"), &pem)?;
            Ok(format!("CA {cn}: {}/ca.crt (key on the token, label {key_label})", out.display()))
        }
        "client" | "server" => {
            // client: CA.crt CSR CN OUT      server: CA.crt CSR OUT SAN...
            let is_server = cmd == "server";
            let (ca_path, csr_path, out, cn, role) = if is_server {
                if p.len() < 3 || a.san.is_empty() {
                    return Err(usage());
                }
                (p[0].clone(), p[1].clone(), PathBuf::from(&p[2]), a.san[0].clone(), Role::Server(a.san.clone()))
            } else {
                if p.len() != 4 {
                    return Err(usage());
                }
                (p[0].clone(), p[1].clone(), PathBuf::from(&p[3]), p[2].clone(), Role::Client)
            };
            let key = token_key(&loc, &key_label, &a.pin_var)?;
            let pem = issue(&key, &read(&ca_path)?, &read(&csr_path)?, &cn, &role)?;
            write_pem(&out, &pem)?;
            Ok(format!("issued {cn} ({}) to {}", if is_server { "router" } else { "client" }, out.display()))
        }
        _ => Err(usage()),
    }
}

fn dn(cn: &str) -> DistinguishedName {
    let mut d = DistinguishedName::new();
    d.push(DnType::CommonName, cn);
    d
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match run(&argv) {
        Ok(line) => {
            println!("{line}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("mw-pki: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::loader::trust::{P256_KEY_LEN, SIGNATURE_LEN};
    use rcgen::KeyPair;
    use x509_parser::pem::parse_x509_pem;

    /// A chip whose key is in memory: the trait's contract without a token.
    struct SoftChip(Option<p256::ecdsa::SigningKey>);

    impl SecureChip for SoftChip {
        type Error = String;
        fn has_key(&mut self, slot: Slot) -> Result<bool, String> {
            Ok(slot == Slot::DEVICE && self.0.is_some())
        }
        fn generate_key(&mut self, _slot: Slot) -> Result<(), String> {
            Err("tests make the key themselves".into())
        }
        fn public_key(&mut self, _slot: Slot) -> Result<[u8; P256_KEY_LEN], String> {
            let k = self.0.as_ref().ok_or("no key")?;
            let mut out = [0u8; P256_KEY_LEN];
            out.copy_from_slice(k.verifying_key().to_encoded_point(true).as_bytes());
            Ok(out)
        }
        fn sign(&mut self, _slot: Slot, message: &[u8]) -> Result<[u8; SIGNATURE_LEN], String> {
            let k = self.0.as_ref().ok_or("no key")?;
            let sig: p256::ecdsa::Signature = p256::ecdsa::signature::Signer::sign(k, message);
            Ok(sig.to_bytes().into())
        }
    }

    fn ca_key() -> ChipKey<SoftChip> {
        let sk = p256::ecdsa::SigningKey::from_bytes(&[0x42; 32].into()).unwrap();
        ChipKey::open(SoftChip(Some(sk)), "test-ca").unwrap()
    }

    fn der(pem: &str) -> Vec<u8> {
        parse_x509_pem(pem.as_bytes()).unwrap().1.contents
    }

    /// A CSR that asks for more than it should get: a CA, another CN, SANs.
    fn greedy_csr(device: &KeyPair) -> String {
        let mut p = CertificateParams::new(vec!["mesh.updates.example".into()]).unwrap();
        p.distinguished_name = dn("kd240-ffffffffffffffff");
        p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        p.serialize_request(device).unwrap().pem().unwrap()
    }

    #[test]
    fn a_chip_without_a_key_is_refused_and_never_asked_to_make_one() {
        let e = ChipKey::open(SoftChip(None), "mw-zenoh-ca").err().unwrap();
        assert!(e.contains("never makes a CA key"), "{e}");
    }

    #[test]
    fn a_client_certificate_carries_only_what_mw_pki_sets() {
        let key = ca_key();
        let ca_pem = ca_cert(&key, "mw-test-ca").unwrap();
        let device = KeyPair::generate().unwrap();
        let pem = issue(&key, &ca_pem, &greedy_csr(&device), "kd240-0a1b2c3d4e5f6071", &Role::Client).unwrap();

        let leaf_der = der(&pem);
        let ca_der = der(&ca_pem);
        let (_, leaf) = X509Certificate::from_der(&leaf_der).unwrap();
        let (_, ca) = X509Certificate::from_der(&ca_der).unwrap();
        leaf.verify_signature(Some(ca.public_key())).expect("signed by the chip's CA key");
        assert_eq!(leaf.public_key().raw, device.subject_public_key_info().as_slice(), "the CSR's key");
        let cn: Vec<_> = leaf.subject().iter_common_name().map(|a| a.as_str().unwrap()).collect();
        assert_eq!(cn, ["kd240-0a1b2c3d4e5f6071"], "one CN, the one mw-pki was given");
        let bc = leaf.basic_constraints().unwrap().map(|b| b.value.ca);
        assert_eq!(bc, Some(false), "explicitly not a CA, whatever the CSR asked");
        assert!(leaf.subject_alternative_name().unwrap().is_none(), "the CSR's SANs are dropped");
        let eku = leaf.extended_key_usage().unwrap().unwrap().value;
        assert!(eku.client_auth && !eku.server_auth);
    }

    #[test]
    fn client_names_are_role_dash_id() {
        let tool63 = format!("tool-{}", "a".repeat(63));
        for ok in ["kd240-0a1b2c3d4e5f6071", "tool-laptop", "tool-a-1", "tool-7", tool63.as_str()] {
            assert!(client_cn_ok(ok), "{ok}");
        }
        let tool64 = format!("tool-{}", "a".repeat(64));
        for bad in [
            "kd240-",
            "tool-",
            "admin-1",
            "kd240-AB",
            "kd240-x y",
            "tool-a/b",
            "kd240-x,CN=y",
            "kd240---",
            "kd240-0a1b",
            "kd240-0a1b2c3d4e5f60711",
            "kd240-0A1B2C3D4E5F6071",
            "kd240-0a1b2c3d4e5f607g",
            "kd240-0a1b2c3d-e5f6071",
            "tool--",
            "tool--a",
            "tool-a-",
            "tool-a--b",
            "tool-Laptop",
            tool64.as_str(),
        ] {
            assert!(!client_cn_ok(bad), "{bad}");
        }
        let key = ca_key();
        let ca_pem = ca_cert(&key, "mw-test-ca").unwrap();
        let csr = greedy_csr(&KeyPair::generate().unwrap());
        assert!(issue(&key, &ca_pem, &csr, "backend", &Role::Client).is_err());
    }

    #[test]
    fn a_router_certificate_has_the_given_sans_and_server_auth() {
        let key = ca_key();
        let ca_pem = ca_cert(&key, "mw-test-ca").unwrap();
        let csr = greedy_csr(&KeyPair::generate().unwrap());
        let role = Role::Server(vec!["mesh.example".into(), "10.0.0.7".into()]);
        let pem = issue(&key, &ca_pem, &csr, "mesh.example", &role).unwrap();
        let leaf_der = der(&pem);
        let (_, leaf) = X509Certificate::from_der(&leaf_der).unwrap();
        let sans = leaf.subject_alternative_name().unwrap().unwrap().value.general_names.len();
        assert_eq!(sans, 2, "exactly the SANs given, not the CSR's");
        let eku = leaf.extended_key_usage().unwrap().unwrap().value;
        assert!(eku.server_auth && !eku.client_auth);
    }

    fn serial(pem: &str) -> Vec<u8> {
        let d = der(pem);
        X509Certificate::from_der(&d).unwrap().1.raw_serial().to_vec()
    }

    #[test]
    fn every_certificate_gets_its_own_random_serial() {
        let key = ca_key();
        let ca_pem = ca_cert(&key, "mw-test-ca").unwrap();
        assert_ne!(serial(&ca_pem), serial(&ca_cert(&key, "mw-test-ca").unwrap()), "CA certificates too");
        let csr = greedy_csr(&KeyPair::generate().unwrap());
        let cn = "kd240-0a1b2c3d4e5f6071";
        let a = serial(&issue(&key, &ca_pem, &csr, cn, &Role::Client).unwrap());
        let b = serial(&issue(&key, &ca_pem, &csr, cn, &Role::Client).unwrap());
        assert_ne!(a, b, "two certificates for one CSR");
        for s in [&a, &b] {
            assert!(s.len() <= 21 && s.len() >= 16, "about 20 random bytes: {s:?}");
            assert_eq!(s[0] & 0x80, 0, "positive");
        }
    }

    #[test]
    fn only_p256_csrs_are_signed() {
        let key = ca_key();
        let ca_pem = ca_cert(&key, "mw-test-ca").unwrap();
        for alg in [&rcgen::PKCS_ED25519, &rcgen::PKCS_ECDSA_P384_SHA384] {
            let device = KeyPair::generate_for(alg).unwrap();
            let e = issue(&key, &ca_pem, &greedy_csr(&device), "kd240-0a1b2c3d4e5f6071", &Role::Client).unwrap_err();
            assert!(e.contains("P-256"), "{e}");
        }
        let e = issue(&key, &ca_pem, "not a csr", "kd240-0a1b2c3d4e5f6071", &Role::Client).unwrap_err();
        assert!(e.contains("CSR"), "{e}");
    }

    #[test]
    fn the_ca_certificate_must_be_the_chips_own() {
        let key = ca_key();
        let other = ChipKey::open(
            SoftChip(Some(p256::ecdsa::SigningKey::from_bytes(&[0x24; 32].into()).unwrap())),
            "other-ca",
        )
        .unwrap();
        let other_ca = ca_cert(&other, "mw-test-ca").unwrap();
        let csr = greedy_csr(&KeyPair::generate().unwrap());
        let e = issue(&key, &other_ca, &csr, "kd240-0a1b2c3d4e5f6071", &Role::Client).unwrap_err();
        assert!(e.contains("not for the token's key"), "{e}");
        let e = issue(&key, "garbage", &csr, "kd240-0a1b2c3d4e5f6071", &Role::Client).unwrap_err();
        assert!(e.contains("CA certificate"), "{e}");
        assert!(issue(&key, &ca_cert(&key, "mw-test-ca").unwrap(), &csr, "kd240-0a1b2c3d4e5f6071", &Role::Client).is_ok());
    }
}
