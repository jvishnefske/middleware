//! Signing keys inside a PKCS#11 token.
//!
//! The private key never leaves the token: a TPM 2.0 through
//! `tpm2-pkcs11` (the release key `mw-release` lives there), a smart card
//! through OpenSC, or SoftHSM2 in the tests. [`Pkcs11Signer`] finds the key
//! by its label (`CKA_LABEL`), reads its public key once, and signs:
//!
//! * a **P-256** key (`CKK_EC` on `prime256v1`) with `CKM_ECDSA` over the
//!   SHA-256 of the message, hashed here so any token that signs raw ECDSA
//!   works; the token's raw `r || s` is normalised to low S and **verified
//!   against the public key before it is returned**, so a misbehaving token
//!   can never hand a device a signature it would refuse;
//! * an **Ed25519** key (`CKK_EC_EDWARDS`) with `CKM_EDDSA` (pure), where
//!   the token has it (TPM 2.0 does not).
//!
//! [`generate`] makes a P-256 (or Ed25519) key pair in the token
//! (`CKA_TOKEN`, `CKA_SENSITIVE`, not `CKA_EXTRACTABLE`) and refuses a label
//! already in use; [`init_token`] initialises a token **only if it is not
//! initialised yet** (initialising one erases it), setting its SO PIN and
//! user PIN.
//!
//! # Settings and PINs
//!
//! The module path, token label and PINs come from the environment, or
//! failing that from a `NAME=value` line of `~/.env` ([`setting`]):
//! [`ENV_MODULE`] (default [`DEFAULT_MODULE`]), [`ENV_TOKEN`] (optional:
//! by default the one initialised token holding the key), [`ENV_PIN`] and,
//! for [`init_token`] only, [`ENV_SO_PIN`]. A PIN is read into an
//! [`AuthPin`] (zeroed on drop) and handed to the module in-process: it
//! never appears in an argument list, a log line or an error message.

use crate::sign::{SignError, Signer, verify};
use cryptoki::context::{CInitializeArgs, Pkcs11};
use cryptoki::mechanism::Mechanism;
use cryptoki::mechanism::eddsa::{EddsaParams, EddsaSignatureScheme};
use cryptoki::object::{Attribute, AttributeType, KeyType, ObjectClass, ObjectHandle};
use cryptoki::session::{Session, UserType};
use cryptoki::slot::Slot;
use cryptoki::types::AuthPin;
use middleware::loader::trust::{KeyKind, P256_KEY_LEN, PublicKey, SIGNATURE_LEN, Signature};
use middleware::secure_chip::{SecureChip, Slot as ChipSlot};
use sha2::{Digest, Sha256};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Environment variable naming the PKCS#11 module (shared library).
pub const ENV_MODULE: &str = "PKCS11_MODULE";
/// Environment variable naming the token label (optional).
pub const ENV_TOKEN: &str = "PKCS11_TOKEN";
/// Environment variable holding the user PIN.
pub const ENV_PIN: &str = "PKCS11_PIN";
/// Environment variable holding the security officer PIN ([`init_token`]).
pub const ENV_SO_PIN: &str = "PKCS11_SO_PIN";
/// The module used when [`ENV_MODULE`] is not set: OpenSC.
pub const DEFAULT_MODULE: &str = "/usr/lib/x86_64-linux-gnu/opensc-pkcs11.so";
/// The release key's label.
pub const DEFAULT_LABEL: &str = "mw-release";
/// DER of the `prime256v1` (secp256r1) OID, `CKA_EC_PARAMS` of a P-256 key.
pub const P256_PARAMS: [u8; 10] = [0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
/// DER of the `id-Ed25519` OID (1.3.101.112), `CKA_EC_PARAMS` of an
/// Ed25519 key.
pub const ED25519_PARAMS: [u8; 5] = [0x06, 0x03, 0x2b, 0x65, 0x70];

fn token_err(step: &str, e: impl fmt::Display) -> SignError {
    SignError::Token(format!("{step}: {e}"))
}

/// The value of `name` in `.env`-style `contents`: the last `NAME=value`
/// (or `export NAME=value`) line, surrounding quotes removed. Pure.
pub fn env_file_value(contents: &str, name: &str) -> Option<String> {
    contents
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
            let value = line.strip_prefix(name)?.strip_prefix('=')?;
            let value = value.trim();
            let unquoted = [('"', '"'), ('\'', '\'')]
                .iter()
                .find_map(|(a, b)| value.strip_prefix(*a)?.strip_suffix(*b))
                .unwrap_or(value);
            Some(unquoted.to_owned())
        })
        .next_back()
}

/// A setting: the environment variable `name`, else its line in `~/.env`.
pub fn setting(name: &str) -> Option<String> {
    if let Ok(v) = std::env::var(name)
        && !v.is_empty()
    {
        return Some(v);
    }
    let home = std::env::var_os("HOME")?;
    let contents = std::fs::read_to_string(Path::new(&home).join(".env")).ok()?;
    env_file_value(&contents, name).filter(|v| !v.is_empty())
}

/// A PIN from [`setting`] `name`; the error names the variable, never a
/// value.
pub fn pin(name: &str) -> Result<AuthPin, SignError> {
    setting(name).map(AuthPin::new).ok_or_else(|| {
        SignError::Token(format!(
            "no PIN: set {name} in the environment or as a {name}=... line in ~/.env (mode 0600)"
        ))
    })
}

/// Where the token is: module and optional token label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    /// The PKCS#11 module.
    pub module: PathBuf,
    /// The token's label; `None` picks the one initialised token that
    /// holds the key.
    pub token: Option<String>,
}

impl Location {
    /// From [`ENV_MODULE`] and [`ENV_TOKEN`] ([`setting`]).
    pub fn from_settings() -> Self {
        Location {
            module: setting(ENV_MODULE)
                .map_or_else(|| PathBuf::from(DEFAULT_MODULE), PathBuf::from),
            token: setting(ENV_TOKEN),
        }
    }

    fn context(&self) -> Result<Pkcs11, SignError> {
        context(&self.module)
    }
}

/// The process's one initialised context per module: `C_Initialize` may run
/// only once per module and process, and a signer keeps its session (and so
/// the context) open while other calls (a key generation) need it too.
fn context(module: &Path) -> Result<Pkcs11, SignError> {
    static CONTEXTS: OnceLock<Mutex<Vec<(PathBuf, Pkcs11)>>> = OnceLock::new();
    let mut all = CONTEXTS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .map_err(|_| SignError::Token("context registry poisoned".into()))?;
    if let Some((_, ctx)) = all.iter().find(|(m, _)| m == module) {
        return Ok(ctx.clone());
    }
    let ctx =
        Pkcs11::new(module).map_err(|e| token_err(&format!("loading {}", module.display()), e))?;
    ctx.initialize(CInitializeArgs::OsThreads)
        .map_err(|e| token_err("C_Initialize", e))?;
    all.push((module.to_owned(), ctx.clone()));
    Ok(ctx)
}

fn token_label(ctx: &Pkcs11, slot: Slot) -> Result<String, SignError> {
    Ok(ctx
        .get_token_info(slot)
        .map_err(|e| token_err("C_GetTokenInfo", e))?
        .label()
        .trim_end()
        .to_owned())
}

/// The initialised slots whose token label is `token` (all of them when
/// `None`).
fn initialised_slots(ctx: &Pkcs11, token: Option<&str>) -> Result<Vec<Slot>, SignError> {
    let mut out = Vec::new();
    for slot in ctx
        .get_slots_with_initialized_token()
        .map_err(|e| token_err("C_GetSlotList", e))?
    {
        if token.is_none_or(|t| token_label(ctx, slot).is_ok_and(|l| l == t)) {
            out.push(slot);
        }
    }
    Ok(out)
}

/// `C_Login`; already logged in (login state is per token and process, so
/// another open session may have done it) counts as success.
fn login(session: &Session, user: UserType, pin: &AuthPin) -> Result<(), SignError> {
    use cryptoki::error::{Error, RvError};
    match session.login(user, Some(pin)) {
        Ok(()) | Err(Error::Pkcs11(RvError::UserAlreadyLoggedIn, _)) => Ok(()),
        Err(e) => Err(token_err("C_Login", e)),
    }
}

fn find_one(
    session: &Session,
    class: ObjectClass,
    label: &str,
) -> Result<Option<ObjectHandle>, SignError> {
    let found = session
        .find_objects(&[Attribute::Class(class), Attribute::Label(label.into())])
        .map_err(|e| token_err("C_FindObjects", e))?;
    match found.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(*one)),
        more => Err(SignError::Token(format!(
            "{} objects are labelled '{label}'; labels must be unique",
            more.len()
        ))),
    }
}

/// Strips the DER OCTET STRING wrapper some modules put around
/// `CKA_EC_POINT` (PKCS#11 requires it; a few return the raw point).
fn unwrap_octet_string(point: &[u8]) -> &[u8] {
    match point {
        [0x04, n, rest @ ..] if usize::from(*n) == rest.len() && *n < 0x80 => rest,
        _ => point,
    }
}

/// The device-wire public key of a token public-key object: P-256 as
/// compressed SEC1, Ed25519 as its 32 bytes. Pure.
pub fn wire_key(key_type: KeyType, params: &[u8], point: &[u8]) -> Result<PublicKey, SignError> {
    let point = unwrap_octet_string(point);
    if key_type == KeyType::EC {
        if params != P256_PARAMS {
            return Err(SignError::Token(
                "the key is EC but not on P-256 (prime256v1), the only curve devices verify".into(),
            ));
        }
        let vk = p256::ecdsa::VerifyingKey::from_sec1_bytes(point)
            .map_err(|_| SignError::Token("CKA_EC_POINT is not a P-256 point".into()))?;
        let compressed = vk.to_encoded_point(true);
        return PublicKey::from_bytes(compressed.as_bytes())
            .ok_or_else(|| SignError::Token("compressing the P-256 point".into()));
    }
    if key_type == KeyType::EC_EDWARDS {
        return <[u8; 32]>::try_from(point)
            .map(PublicKey::Ed25519)
            .map_err(|_| SignError::Token("CKA_EC_POINT is not a 32-byte Ed25519 key".into()));
    }
    Err(SignError::Token(format!(
        "key type {key_type} is neither P-256 nor Ed25519"
    )))
}

fn read_public(session: &Session, object: ObjectHandle) -> Result<PublicKey, SignError> {
    let attrs = session
        .get_attributes(
            object,
            &[
                AttributeType::KeyType,
                AttributeType::EcParams,
                AttributeType::EcPoint,
            ],
        )
        .map_err(|e| token_err("C_GetAttributeValue", e))?;
    let (mut kt, mut params, mut point) = (None, None, None);
    for a in attrs {
        match a {
            Attribute::KeyType(t) => kt = Some(t),
            Attribute::EcParams(p) => params = Some(p),
            Attribute::EcPoint(p) => point = Some(p),
            _ => {}
        }
    }
    match (kt, params, point) {
        (Some(t), Some(params), Some(point)) => wire_key(t, &params, &point),
        _ => Err(SignError::Token(
            "the public key object has no EC parameters or point".into(),
        )),
    }
}

/// A signing key in a PKCS#11 token, logged in for the life of the value.
pub struct Pkcs11Signer {
    session: Session,
    key: ObjectHandle,
    public: PublicKey,
    label: String,
    token: String,
}

impl fmt::Debug for Pkcs11Signer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Pkcs11Signer({} key '{}' in token '{}')",
            self.public.kind().name(),
            self.label,
            self.token
        )
    }
}

impl Pkcs11Signer {
    /// Opens the key labelled `label` at `at`, logging in with `pin`. The
    /// public key object (readable without a PIN) picks the token when
    /// none is named; exactly one token may hold the label.
    pub fn open(at: &Location, label: &str, pin: &AuthPin) -> Result<Self, SignError> {
        let ctx = at.context()?;
        let mut hit = None;
        for slot in initialised_slots(&ctx, at.token.as_deref())? {
            let session = ctx
                .open_ro_session(slot)
                .map_err(|e| token_err("C_OpenSession", e))?;
            if let Some(public) = find_one(&session, ObjectClass::PUBLIC_KEY, label)? {
                if hit.is_some() {
                    return Err(SignError::Token(format!(
                        "more than one token holds '{label}'; set {ENV_TOKEN}"
                    )));
                }
                hit = Some((slot, session, public));
            }
        }
        let Some((slot, session, public_obj)) = hit else {
            return Err(SignError::Token(format!(
                "no key labelled '{label}' in {} (create it with `mwctl key new --pkcs11 {label}`)",
                at.token.as_deref().map_or_else(
                    || "any initialised token".to_owned(),
                    |t| format!("token '{t}'")
                )
            )));
        };
        let public = read_public(&session, public_obj)?;
        login(&session, UserType::User, pin)?;
        let key = find_one(&session, ObjectClass::PRIVATE_KEY, label)?.ok_or_else(|| {
            SignError::Token(format!("'{label}' has a public key but no private key"))
        })?;
        Ok(Pkcs11Signer {
            token: token_label(&ctx, slot)?,
            session,
            key,
            public,
            label: label.to_owned(),
        })
    }
}

impl Signer for Pkcs11Signer {
    fn public_key(&self) -> PublicKey {
        self.public
    }

    fn sign(&self, message: &[u8]) -> Result<Signature, SignError> {
        let raw = match self.public.kind() {
            KeyKind::P256 => {
                let digest: [u8; 32] = Sha256::digest(message).into();
                let raw = self
                    .session
                    .sign(&Mechanism::Ecdsa, self.key, &digest)
                    .map_err(|e| token_err("C_Sign (ECDSA)", e))?;
                let sig = p256::ecdsa::Signature::from_slice(&raw).map_err(|_| {
                    SignError::Token(format!(
                        "the token returned a {}-byte ECDSA signature",
                        raw.len()
                    ))
                })?;
                let sig = sig.normalize_s().unwrap_or(sig);
                <[u8; 64]>::from(sig.to_bytes())
            }
            KeyKind::Ed25519 => {
                let mech = Mechanism::Eddsa(EddsaParams::new(EddsaSignatureScheme::Pure));
                let raw = self
                    .session
                    .sign(&mech, self.key, message)
                    .map_err(|e| token_err("C_Sign (EdDSA)", e))?;
                <[u8; 64]>::try_from(raw.as_slice()).map_err(|_| {
                    SignError::Token(format!(
                        "the token returned a {}-byte EdDSA signature",
                        raw.len()
                    ))
                })?
            }
        };
        let signature = Signature(raw);
        if !verify(&self.public, message, &signature) {
            return Err(SignError::Token(
                "the token's signature does not verify against its public key".into(),
            ));
        }
        Ok(signature)
    }
}

/// The one slot to create in: the named token, or the only initialised one.
fn create_slot(ctx: &Pkcs11, token: Option<&str>) -> Result<Slot, SignError> {
    match initialised_slots(ctx, token)?.as_slice() {
        [one] => Ok(*one),
        [] => Err(SignError::Token(match token {
            Some(t) => format!("no initialised token '{t}'"),
            None => "no initialised token (see `mwctl key init-token`)".into(),
        })),
        _ => Err(SignError::Token(format!(
            "several initialised tokens; set {ENV_TOKEN} to one of their labels"
        ))),
    }
}

/// Generates a `kind` key pair labelled `label` in the token at `at`
/// (the private key sensitive and never extractable) and returns its
/// public key. Refuses a label that is already used.
pub fn generate(
    at: &Location,
    label: &str,
    kind: KeyKind,
    pin: &AuthPin,
) -> Result<PublicKey, SignError> {
    let ctx = at.context()?;
    let slot = create_slot(&ctx, at.token.as_deref())?;
    let session = ctx
        .open_rw_session(slot)
        .map_err(|e| token_err("C_OpenSession", e))?;
    login(&session, UserType::User, pin)?;
    let used = session
        .find_objects(&[Attribute::Label(label.into())])
        .map_err(|e| token_err("C_FindObjects", e))?;
    if !used.is_empty() {
        return Err(SignError::Token(format!(
            "the token already has objects labelled '{label}'; refusing to add another"
        )));
    }
    let (mech, params) = match kind {
        KeyKind::P256 => (Mechanism::EccKeyPairGen, P256_PARAMS.to_vec()),
        KeyKind::Ed25519 => (Mechanism::EccEdwardsKeyPairGen, ED25519_PARAMS.to_vec()),
    };
    let id: Vec<u8> = label.as_bytes().to_vec();
    let public_t = [
        Attribute::Token(true),
        Attribute::Verify(true),
        Attribute::EcParams(params),
        Attribute::Label(label.into()),
        Attribute::Id(id.clone()),
    ];
    let private_t = [
        Attribute::Token(true),
        Attribute::Private(true),
        Attribute::Sensitive(true),
        Attribute::Extractable(false),
        Attribute::Sign(true),
        Attribute::Label(label.into()),
        Attribute::Id(id),
    ];
    let (public, _private) = session
        .generate_key_pair(&mech, &public_t, &private_t)
        .map_err(|e| token_err("C_GenerateKeyPair", e))?;
    read_public(&session, public)
}

/// Initialises the one uninitialised token at `at.module` with label
/// `token`, SO PIN `so_pin` and user PIN `user_pin`. Refuses when no
/// token, or more than one, is uninitialised: initialising a token that is
/// in use would erase its keys.
pub fn init_token(
    module: &Path,
    token: &str,
    so_pin: &AuthPin,
    user_pin: &AuthPin,
) -> Result<(), SignError> {
    let ctx = context(module)?;
    let mut blank = Vec::new();
    for slot in ctx
        .get_slots_with_token()
        .map_err(|e| token_err("C_GetSlotList", e))?
    {
        let info = ctx
            .get_token_info(slot)
            .map_err(|e| token_err("C_GetTokenInfo", e))?;
        if !info.token_initialized() {
            blank.push(slot);
        }
    }
    let slot = match blank.as_slice() {
        [one] => *one,
        [] => {
            return Err(SignError::Token(
                "no uninitialised token; refusing to re-initialise one (that erases its keys)"
                    .into(),
            ));
        }
        _ => {
            return Err(SignError::Token(
                "several uninitialised tokens; initialise one with the module's own tool".into(),
            ));
        }
    };
    ctx.init_token(slot, so_pin, token)
        .map_err(|e| token_err("C_InitToken", e))?;
    // The module may renumber slots after C_InitToken: find it by label.
    let slot = create_slot(&ctx, Some(token))?;
    let session = ctx
        .open_rw_session(slot)
        .map_err(|e| token_err("C_OpenSession", e))?;
    session
        .login(UserType::So, Some(so_pin))
        .map_err(|e| token_err("C_Login (SO)", e))?;
    session
        .init_pin(user_pin)
        .map_err(|e| token_err("C_InitPIN", e))?;
    session.logout().map_err(|e| token_err("C_Logout", e))?;
    Ok(())
}

/// [`SecureChip`] on a PKCS#11 token: a TPM 2.0 through `tpm2-pkcs11`, an
/// HSM, or SoftHSM2 in the tests. This is the one way host code reaches a
/// TPM key: every use of the key goes through the trait.
///
/// The chip holds one key, at [`ChipSlot::DEVICE`]: the token key labelled
/// `label`. [`SecureChip::generate_key`] makes it with [`generate`]
/// (sensitive, never extractable) and refuses a label already in use, so the
/// private key never leaves the token and is never replaced. Other slots hold
/// nothing. Each call opens its own session; none outlives the call.
pub struct Pkcs11Chip {
    at: Location,
    label: String,
    pin: AuthPin,
}

impl fmt::Debug for Pkcs11Chip {
    // The PIN is never printed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Pkcs11Chip(key '{}' at {})", self.label, self.at.module.display())
    }
}

impl Pkcs11Chip {
    /// The key labelled `label` in the token at `at`, used with `pin`.
    pub fn new(at: Location, label: impl Into<String>, pin: AuthPin) -> Self {
        Pkcs11Chip { at, label: label.into(), pin }
    }

    /// The token key's label.
    pub fn label(&self) -> &str {
        &self.label
    }

    fn device(&self, slot: ChipSlot) -> Result<(), SignError> {
        if slot == ChipSlot::DEVICE {
            Ok(())
        } else {
            Err(SignError::Token(format!(
                "slot {} holds no key: this token holds only the device key '{}'",
                slot.0, self.label
            )))
        }
    }

    fn signer(&self) -> Result<Pkcs11Signer, SignError> {
        Pkcs11Signer::open(&self.at, &self.label, &self.pin)
    }
}

impl SecureChip for Pkcs11Chip {
    type Error = SignError;

    fn has_key(&mut self, slot: ChipSlot) -> Result<bool, SignError> {
        if slot != ChipSlot::DEVICE {
            return Ok(false);
        }
        // The public key object is readable without the PIN.
        let ctx = self.at.context()?;
        for s in initialised_slots(&ctx, self.at.token.as_deref())? {
            let session = ctx
                .open_ro_session(s)
                .map_err(|e| token_err("C_OpenSession", e))?;
            if find_one(&session, ObjectClass::PUBLIC_KEY, &self.label)?.is_some() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn generate_key(&mut self, slot: ChipSlot) -> Result<(), SignError> {
        self.device(slot)?;
        generate(&self.at, &self.label, KeyKind::P256, &self.pin).map(|_| ())
    }

    fn public_key(&mut self, slot: ChipSlot) -> Result<[u8; P256_KEY_LEN], SignError> {
        self.device(slot)?;
        match self.signer()?.public_key() {
            PublicKey::P256(k) => Ok(k),
            other => Err(SignError::Token(format!(
                "the key '{}' is {}, not P-256",
                self.label,
                other.kind().name()
            ))),
        }
    }

    fn sign(&mut self, slot: ChipSlot, message: &[u8]) -> Result<[u8; SIGNATURE_LEN], SignError> {
        self.device(slot)?;
        // Pkcs11Signer hashes with SHA-256, normalises to low S and checks
        // the signature against the public key before returning it.
        Ok(self.signer()?.sign(message)?.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_file_lines_are_read_by_name() {
        let f = "# comment\nPKCS11_PIN_OLD=1\nexport PKCS11_PIN='12 34'\nOTHER=x\nPKCS11_MODULE=\"/m.so\"\n";
        assert_eq!(env_file_value(f, "PKCS11_PIN").as_deref(), Some("12 34"));
        assert_eq!(env_file_value(f, "PKCS11_MODULE").as_deref(), Some("/m.so"));
        assert_eq!(env_file_value(f, "PKCS11_TOKEN"), None);
        assert_eq!(
            env_file_value("A=1\nA=2\n", "A").as_deref(),
            Some("2"),
            "last wins"
        );
        assert_eq!(env_file_value("AB=1\n", "A"), None);
    }

    #[test]
    fn public_keys_convert_to_the_device_wire_form() {
        let sk = p256::ecdsa::SigningKey::from_bytes(&[0x20; 32].into()).unwrap();
        let unc = sk.verifying_key().to_encoded_point(false);
        let mut der = vec![0x04, 65];
        der.extend_from_slice(unc.as_bytes());
        let want = "020e539e011304a06ad552227a677a388cee251cfe15486e5ae02230a79d590ee0";
        for point in [der.as_slice(), unc.as_bytes()] {
            let k = wire_key(KeyType::EC, &P256_PARAMS, point).unwrap();
            assert_eq!(crate::sign::hex(k.as_bytes()), want);
        }
        let p384 = [0x06, 0x05, 0x2b, 0x81, 0x04, 0x00, 0x22];
        assert!(wire_key(KeyType::EC, &p384, &der).is_err());
        assert!(wire_key(KeyType::RSA, &P256_PARAMS, &der).is_err());
        let mut ed = vec![0x04, 32];
        ed.extend_from_slice(&[7; 32]);
        assert_eq!(
            wire_key(KeyType::EC_EDWARDS, &ED25519_PARAMS, &ed).unwrap(),
            PublicKey::Ed25519([7; 32])
        );
        assert!(wire_key(KeyType::EC_EDWARDS, &ED25519_PARAMS, &[4, 31, 0]).is_err());
    }

    #[test]
    fn a_missing_pin_names_the_variable_only() {
        let e = pin("MWCTL_TEST_NO_SUCH_PIN_VAR").unwrap_err();
        assert!(e.to_string().contains("MWCTL_TEST_NO_SUCH_PIN_VAR"));
    }

    fn softhsm_module() -> Option<PathBuf> {
        [
            "/usr/lib/softhsm/libsofthsm2.so",
            "/usr/lib/x86_64-linux-gnu/softhsm/libsofthsm2.so",
        ]
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
    }

    /// Runs [`softhsm_token_keys_sign_what_devices_accept`] in a child
    /// test process with `SOFTHSM2_CONF` pointing at a fresh token
    /// directory (the crate forbids `unsafe`, so the variable cannot be set
    /// in this process). Skipped, with a note, without SoftHSM2.
    #[test]
    fn softhsm_end_to_end() {
        softhsm_child("pkcs11::tests::softhsm_token_keys_sign_what_devices_accept");
    }

    /// Runs [`softhsm_chip_keeps_its_key_on_the_token`] the same way.
    #[test]
    fn softhsm_chip_end_to_end() {
        softhsm_child("pkcs11::tests::softhsm_chip_keeps_its_key_on_the_token");
    }

    /// The ignored test `name`, run in a child process on a fresh SoftHSM2
    /// token directory. Skipped, with a note, without SoftHSM2.
    fn softhsm_child(name: &str) {
        if softhsm_module().is_none() {
            // CI installs SoftHSM2 and sets MWCTL_REQUIRE_SOFTHSM, so the
            // token path can never silently stop being tested there.
            assert!(
                std::env::var_os("MWCTL_REQUIRE_SOFTHSM").is_none(),
                "MWCTL_REQUIRE_SOFTHSM is set but SoftHSM2 is not installed"
            );
            eprintln!("SKIPPED: SoftHSM2 is not installed (apt install softhsm2)");
            return;
        }
        let tag = name.rsplit("::").next().unwrap_or(name);
        let dir = std::env::temp_dir().join(format!("mwctl-softhsm-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("tokens")).unwrap();
        let conf = dir.join("softhsm2.conf");
        std::fs::write(
            &conf,
            format!(
                "directories.tokendir = {}\nobjectstore.backend = file\nlog.level = ERROR\n",
                dir.join("tokens").display()
            ),
        )
        .unwrap();
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--ignored", "--nocapture"])
            .env("SOFTHSM2_CONF", &conf)
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let text = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{text}");
        assert!(text.contains("1 passed"), "the child ran the test: {text}");
    }

    #[test]
    fn a_chip_slot_other_than_the_device_never_reaches_the_token() {
        // No such module: any call that loaded it would fail with a load error.
        let at = Location {
            module: PathBuf::from("/nonexistent/no-such-pkcs11.so"),
            token: None,
        };
        let mut chip = Pkcs11Chip::new(at, "kd240-dev", AuthPin::new("secret-pin-42".into()));
        assert!(matches!(chip.has_key(ChipSlot(1)), Ok(false)));
        let made = chip.generate_key(ChipSlot(1)).unwrap_err().to_string();
        assert!(made.contains("holds no key"), "{made}");
        assert!(chip.public_key(ChipSlot(2)).is_err());
        assert!(chip.sign(ChipSlot(3), b"x").is_err());
        assert!(!format!("{chip:?}").contains("secret-pin-42"), "the PIN is not printed");
    }

    /// [`Pkcs11Chip`] on SoftHSM2: the key is made once on the token, a
    /// second `generate_key` is refused, and the chip's signature verifies
    /// against the public key it reports. Run by [`softhsm_chip_end_to_end`].
    #[test]
    #[ignore = "run by softhsm_chip_end_to_end with SOFTHSM2_CONF set"]
    fn softhsm_chip_keeps_its_key_on_the_token() {
        use middleware::secure_chip::ensure_key;
        use p256::ecdsa::signature::Verifier;

        assert!(
            std::env::var_os("SOFTHSM2_CONF").is_some(),
            "run through softhsm_chip_end_to_end"
        );
        let module = softhsm_module().unwrap();
        let so = AuthPin::new("so-test-0000".into());
        let user = AuthPin::new("user-test-1234".into());
        init_token(&module, "mw-chip", &so, &user).unwrap();
        let at = Location {
            module,
            token: Some("mw-chip".into()),
        };
        let mut chip = Pkcs11Chip::new(at.clone(), "kd240-dev", AuthPin::new("user-test-1234".into()));

        assert!(!chip.has_key(ChipSlot::DEVICE).unwrap(), "a new token holds no key");
        let first = ensure_key(&mut chip, ChipSlot::DEVICE).unwrap();
        assert!(chip.has_key(ChipSlot::DEVICE).unwrap());
        assert_eq!(ensure_key(&mut chip, ChipSlot::DEVICE).unwrap(), first, "made once");
        let again = chip.generate_key(ChipSlot::DEVICE).unwrap_err().to_string();
        assert!(again.contains("already"), "{again}");

        let message = b"csr-to-be-signed";
        let raw = chip.sign(ChipSlot::DEVICE, message).unwrap();
        let vk = p256::ecdsa::VerifyingKey::from_sec1_bytes(&first).unwrap();
        let sig = p256::ecdsa::Signature::from_slice(&raw).unwrap();
        vk.verify(message, &sig).expect("the chip's signature verifies against its key");

        let mut wrong = Pkcs11Chip::new(at, "kd240-dev", AuthPin::new("wrong-pin-9".into()));
        let e = wrong.sign(ChipSlot::DEVICE, message).unwrap_err().to_string();
        assert!(e.contains("C_Login") && !e.contains("wrong-pin-9"), "{e}");
    }

    /// The token path end to end on SoftHSM2 (no card or TPM needed):
    /// initialise, generate, sign, and have the **device** code
    /// (`middleware::loader::trust` with `trust`, `fw_ota` on the bootrom
    /// model) accept what the token signed and refuse the rest. Run by
    /// [`softhsm_end_to_end`] (ignored on its own: it needs
    /// `SOFTHSM2_CONF`).
    #[test]
    #[ignore = "run by softhsm_end_to_end with SOFTHSM2_CONF set"]
    fn softhsm_token_keys_sign_what_devices_accept() {
        use crate::device::Device;
        use crate::fake::FakeDevice;
        use crate::fw::{UpdateOptions, load_image};
        use crate::link::{Link, Timeouts};
        use crate::sign::{Seed, sign_commit, sign_control};
        use middleware::fw_ota::sim::{self, Rp2350Sim};
        use middleware::fw_ota::{Slot, StateCode};
        use middleware::loader::ImageDigest;
        use middleware::loader::trust::{
            ControlCounter, ControlOp, TrustState, Version, signed_message,
        };
        use middleware::transport::HandlerError;

        assert!(
            std::env::var_os("SOFTHSM2_CONF").is_some(),
            "run through softhsm_end_to_end"
        );
        let module = softhsm_module().unwrap();

        let so = AuthPin::new("so-test-0000".into());
        let user = AuthPin::new("user-test-1234".into());
        init_token(&module, "mw-test", &so, &user).unwrap();
        let at = Location {
            module: module.clone(),
            token: Some("mw-test".into()),
        };

        // P-256: generate, refuse the label twice, open.
        let public = generate(&at, DEFAULT_LABEL, KeyKind::P256, &user).unwrap();
        assert_eq!(public.kind(), KeyKind::P256);
        assert!(crate::sign::is_valid_key(&public));
        let again = generate(&at, DEFAULT_LABEL, KeyKind::P256, &user).unwrap_err();
        assert!(again.to_string().contains("already"), "{again}");
        let wrong = Pkcs11Signer::open(&at, DEFAULT_LABEL, &AuthPin::new("wrong-pin-9".into()))
            .unwrap_err()
            .to_string();
        assert!(
            wrong.contains("C_Login") && !wrong.contains("wrong-pin-9"),
            "{wrong}"
        );
        let signer = Pkcs11Signer::open(&at, DEFAULT_LABEL, &user).unwrap();
        assert_eq!(signer.public_key(), public);
        assert!(format!("{signer:?}").contains("p256 key 'mw-release'"));
        // A second init_token takes the next free slot, never this token.
        init_token(&module, "mw-other", &so, &user).unwrap();
        assert_eq!(
            Pkcs11Signer::open(&at, DEFAULT_LABEL, &user)
                .unwrap()
                .public_key(),
            public
        );
        let anywhere = Location {
            token: None,
            ..at.clone()
        };
        assert_eq!(
            Pkcs11Signer::open(&anywhere, DEFAULT_LABEL, &user)
                .unwrap()
                .public_key(),
            public
        );
        assert!(Pkcs11Signer::open(&at, "no-such-key", &user).is_err());

        // The device's payload commit check (real middleware code).
        let image: Vec<u8> = (0..5000u32).map(|i| (i * 13) as u8).collect();
        let digest = ImageDigest {
            len: image.len() as u32,
            crc: middleware::crc::Crc32::checksum(&image),
            sha256: crate::sign::image_sha256(&image),
        };
        let claimed = TrustState::Unclaimed.claim(public, false).unwrap();
        let auth = sign_commit(&image, &signer, Version(100)).unwrap();
        let after = claimed.accept_commit(&digest, Some(&auth)).unwrap();
        assert_eq!(
            after,
            TrustState::Claimed {
                key: public,
                min_version: Version(100)
            }
        );
        let denied = Err(HandlerError::Denied);
        assert_eq!(after.accept_commit(&digest, Some(&auth)), denied, "replay");
        let other = sign_commit(&image, &Seed::new([42; 32]), Version(101)).unwrap();
        assert_eq!(
            claimed.accept_commit(&digest, Some(&other)),
            denied,
            "other key"
        );
        let mut tampered = digest;
        tampered.sha256[0] ^= 1;
        assert_eq!(
            claimed.accept_commit(&tampered, Some(&auth)),
            denied,
            "tampered"
        );
        // Low S, always: ECDSA signatures differ, all verify.
        for v in 0..8 {
            let a = sign_commit(&image, &signer, Version(200 + v)).unwrap();
            let m = signed_message(digest.len, a.version, &digest.sha256);
            assert!(crate::sign::verify(&public, &m, &a.signature));
            let s = p256::ecdsa::Signature::from_slice(&a.signature.0).unwrap();
            assert!(s.normalize_s().is_none(), "low S");
        }

        // Control requests.
        let target = [1, 2, 3, 4, 5, 6];
        let c = sign_control(&signer, ControlOp::Stop, &target, ControlCounter(77)).unwrap();
        assert_eq!(
            claimed.accept_control(ControlCounter(76), ControlOp::Stop, &target, &c),
            Ok(ControlCounter(77))
        );
        assert_eq!(
            claimed.accept_control(ControlCounter(77), ControlOp::Stop, &target, &c),
            Err(HandlerError::Denied)
        );

        // A firmware update on the RP2350 bootrom model, claimed by the
        // token key, signed by the token: installed, booted on trial,
        // confirmed. Signed by another key: refused at commit.
        let mut sim = Rp2350Sim::new(256 * 1024);
        sim.program(Slot::A, &sim::image(40_000, 0x0001_0000, false, 1));
        let fake = FakeDevice::new("rp2350", "0.1.0", 4096, true)
            .with_fw(sim)
            .with_claim(public);
        let t = Timeouts {
            request: std::time::Duration::from_millis(50),
            begin: std::time::Duration::from_millis(50),
            retries: 1,
        };
        let mut d = Device::new(Link::new(fake, t));
        let img = load_image(&sim::image(20_000, 0x0001_0001, true, 2), None).unwrap();
        let mallory = Seed::new([42; 32]);
        let err = d
            .fw_update(
                &img,
                UpdateOptions {
                    release: None,
                    key: Some(&mallory),
                    reboot: false,
                    control: None,
                },
                |_| {},
            )
            .unwrap_err();
        assert_eq!(err.status(), Some(HandlerError::Denied), "{err}");
        d.fw_abort().unwrap();
        let done = d
            .fw_update(
                &img,
                UpdateOptions {
                    release: None,
                    key: Some(&signer),
                    reboot: true,
                    control: None,
                },
                |_| {},
            )
            .unwrap();
        assert_eq!(done.slot, Slot::B);
        assert_eq!(d.fw_info().unwrap().state, StateCode::Trial);
        d.fw_confirm(3, || {}).unwrap();
        assert_eq!(d.fw_info().unwrap().state, StateCode::Confirmed);

        // Ed25519 inside the token (where a token has it).
        let ed = generate(&at, "mw-ed", KeyKind::Ed25519, &user).unwrap();
        assert_eq!(ed.kind(), KeyKind::Ed25519);
        let ed_signer = Pkcs11Signer::open(&at, "mw-ed", &user).unwrap();
        let ed_claimed = TrustState::Unclaimed.claim(ed, false).unwrap();
        let a = sign_commit(&image, &ed_signer, Version(5)).unwrap();
        assert!(ed_claimed.accept_commit(&digest, Some(&a)).is_ok());
        assert_eq!(
            claimed.accept_commit(&digest, Some(&a)),
            denied,
            "kinds never cross"
        );

        drop((signer, ed_signer));
    }
}
