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
//! Token settings: `MW_PKCS11_MODULE` and `MW_PKCS11_TOKEN` (mwctl), and the
//! PIN in `MW_PKI_PIN` (or in `~/.env`, mode 0600).

use mwctl::pkcs11::{Location, Pkcs11Signer, pin};
use mwctl::sign::Signer;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, DistinguishedName,
    DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyUsagePurpose, PublicKeyData, SanType,
    SignatureAlgorithm, SigningKey,
};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The PIN variable `mw-pki` reads unless `--pin-env` names another.
const PIN_VAR: &str = "MW_PKI_PIN";

/// A key on the token, as rcgen signs with it.
struct TokenKey {
    signer: Pkcs11Signer,
    /// The uncompressed SEC1 point (65 bytes) of the token's public key.
    point: Vec<u8>,
}

impl TokenKey {
    fn open(loc: &Location, label: &str, pin_var: &str) -> Result<Self, String> {
        let pin = pin(pin_var).map_err(|e| e.to_string())?;
        let signer = Pkcs11Signer::open(loc, label, &pin).map_err(|e| e.to_string())?;
        let point = match signer.public_key() {
            middleware::loader::trust::PublicKey::P256(c) => {
                let pk = p256::PublicKey::from_sec1_bytes(&c).map_err(|e| e.to_string())?;
                pk.to_encoded_point(false).as_bytes().to_vec()
            }
            other => return Err(format!("the key '{label}' is {}, not P-256", other.kind().name())),
        };
        Ok(TokenKey { signer, point })
    }
}

impl PublicKeyData for TokenKey {
    fn der_bytes(&self) -> &[u8] {
        &self.point
    }
    fn algorithm(&self) -> &'static SignatureAlgorithm {
        &rcgen::PKCS_ECDSA_P256_SHA256
    }
}

impl SigningKey for TokenKey {
    fn sign(&self, msg: &[u8]) -> Result<Vec<u8>, rcgen::Error> {
        // The token hashes with SHA-256 and returns raw r||s; certificates
        // carry the DER form.
        let sig = self.signer.sign(msg).map_err(|_| rcgen::Error::RemoteKeyError)?;
        Ok(der_ecdsa(&sig.0))
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
            let key = TokenKey::open(&loc, &key_label, &a.pin_var)?;
            let mut params = CertificateParams::default();
            params.distinguished_name = dn(&cn);
            params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
            params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
            validity(&mut params, 730);
            let cert = params.self_signed(&key).map_err(|e| e.to_string())?;
            std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
            write_pem(&out.join("ca.crt"), &cert.pem())?;
            Ok(format!("CA {cn}: {}/ca.crt (key on the token, label {key_label})", out.display()))
        }
        "client" | "server" => {
            // client: CA.crt CSR CN OUT      server: CA.crt CSR OUT SAN...
            let is_server = cmd == "server";
            let (ca_path, csr_path, out, cn, sans) = if is_server {
                if p.len() < 3 || a.san.is_empty() {
                    return Err(usage());
                }
                (p[0].clone(), p[1].clone(), PathBuf::from(&p[2]), a.san[0].clone(), a.san.clone())
            } else {
                if p.len() != 4 {
                    return Err(usage());
                }
                (p[0].clone(), p[1].clone(), PathBuf::from(&p[3]), p[2].clone(), Vec::new())
            };
            let key = TokenKey::open(&loc, &key_label, &a.pin_var)?;
            let ca_pem = read(&ca_path)?;
            let issuer = Issuer::from_ca_cert_pem(&ca_pem, &key).map_err(|e| e.to_string())?;
            let csr_pem = read(&csr_path)?;
            let mut csr = CertificateSigningRequestParams::from_pem(&csr_pem).map_err(|e| e.to_string())?;
            if is_server {
                csr.params.subject_alt_names = sans
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
                csr.params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
            } else {
                if !(cn.starts_with("kd240-") || cn.starts_with("tool-")) {
                    return Err(format!("CN '{cn}' is not <role>-<id> (kd240 or tool)"));
                }
                csr.params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
            }
            csr.params.distinguished_name = dn(&cn);
            csr.params.is_ca = IsCa::NoCa;
            csr.params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
            validity(&mut csr.params, 365);
            let cert = csr.signed_by(&issuer).map_err(|e| e.to_string())?;
            write_pem(&out, &cert.pem())?;
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
