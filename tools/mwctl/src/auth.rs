//! `mwctl auth`: sign in to an OpenID Connect provider (Keycloak) from the
//! command line, for services that take a bearer token (the admin server's
//! certificate enrolment).
//!
//! The provider is named only by the environment, never in this repository:
//! [`ENV_ISSUER`] is the issuer URL (for Keycloak,
//! `https://<auth host>/realms/<realm>`), [`ENV_CLIENT_ID`] the public client
//! (default [`DEFAULT_CLIENT_ID`]).
//!
//! `login` uses the OAuth 2.0 Device Authorization Grant (RFC 8628): mwctl
//! prints a URL and a code, the operator approves in any browser (the
//! machine running mwctl needs none, so it works over SSH), and mwctl polls
//! for the tokens. `token` prints a valid access token, refreshing it first
//! when it is about to expire; it is the only command that prints a token.
//! `logout` forgets the sign-in and revokes the refresh token where the
//! provider offers revocation.
//!
//! Tokens are kept in [`ENV_STORE`] (default
//! `$XDG_CONFIG_HOME/mwctl/auth.json`, else `~/.config/mwctl/auth.json`),
//! mode 0600 in a 0700 directory, one entry per issuer and client. The
//! directory must not be a symlink nor group- or world-writable; every write
//! goes to a new private file that is renamed over the store, and each
//! read-modify-write (a refresh included) holds an exclusive lock file
//! beside the store, so concurrent `mwctl auth token` runs do not race.
//! Every URL
//! must be `https://`, except plain `http://` to `localhost` or `127.0.0.1`
//! (tests, a local proxy). Errors never contain a token.

use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The issuer URL.
pub const ENV_ISSUER: &str = "MW_AUTH_ISSUER";
/// The public client mwctl signs in as.
pub const ENV_CLIENT_ID: &str = "MW_AUTH_CLIENT_ID";
/// Where the tokens are kept.
pub const ENV_STORE: &str = "MW_AUTH_STORE";
/// The client when [`ENV_CLIENT_ID`] is unset.
pub const DEFAULT_CLIENT_ID: &str = "mwctl";
/// The scopes asked for: the services check the email.
pub const SCOPE: &str = "openid email";
/// An access token this close to expiry (seconds) is refreshed before use.
pub const EARLY: u64 = 30;
/// RFC 8628's grant type.
pub const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// The longest `expires_in` believed (seconds): a day.
pub const MAX_EXPIRES_IN: u64 = 86_400;
/// The longest device-login polling interval (seconds), `slow_down` included.
pub const MAX_INTERVAL: u64 = 60;
/// How long a store lock held by another mwctl is waited for: longer than
/// one refresh (the HTTP timeout is 20 s).
const LOCK_WAIT: Duration = Duration::from_secs(25);

/// Which provider and client, and where the tokens are kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The issuer, without a trailing `/`.
    pub issuer: String,
    /// The client id.
    pub client_id: String,
    /// The token store file.
    pub store: PathBuf,
}

impl Config {
    /// From the environment (`var` looks a name up).
    pub fn from_env(var: impl Fn(&str) -> Option<String>) -> Result<Config, String> {
        let issuer = var(ENV_ISSUER).filter(|s| !s.is_empty()).ok_or_else(|| {
            format!("{ENV_ISSUER} is not set: the OpenID Connect issuer, e.g. https://<auth host>/realms/<realm>")
        })?;
        let issuer = issuer.trim_end_matches('/').to_owned();
        check_url(&issuer)?;
        let client_id = var(ENV_CLIENT_ID)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_CLIENT_ID.to_owned());
        let store = match var(ENV_STORE).filter(|s| !s.is_empty()) {
            Some(p) => PathBuf::from(p),
            None => {
                let base = var("XDG_CONFIG_HOME")
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from)
                    .or_else(|| var("HOME").filter(|s| !s.is_empty()).map(|h| PathBuf::from(h).join(".config")))
                    .ok_or_else(|| format!("neither XDG_CONFIG_HOME nor HOME is set; set {ENV_STORE}"))?;
                base.join("mwctl").join("auth.json")
            }
        };
        Ok(Config { issuer, client_id, store })
    }

    fn entry(&self) -> String {
        format!("{} {}", self.issuer, self.client_id)
    }
}

/// `https://` with a host, or `http://` to this machine only.
pub fn check_url(url: &str) -> Result<(), String> {
    if url.strip_prefix("https://").is_some_and(|rest| !rest.is_empty() && !rest.starts_with('/')) {
        return Ok(());
    }
    if let Some(rest) = url.strip_prefix("http://") {
        let authority = rest.split('/').next().unwrap_or("");
        let host = authority.rsplit_once(':').map_or(authority, |(h, _)| h);
        if !authority.contains('@') && (host == "localhost" || host == "127.0.0.1") {
            return Ok(());
        }
    }
    Err(format!("'{url}' is not an https URL (plain http is allowed only to localhost)"))
}

/// The provider's endpoints, from its discovery document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    /// RFC 8628's device authorization endpoint.
    pub device: String,
    /// The token endpoint.
    pub token: String,
    /// RFC 7009's revocation endpoint, where offered.
    pub revoke: Option<String>,
}

/// The endpoints in `doc`, which must name `issuer` as its issuer.
pub fn endpoints(doc: &Value, issuer: &str) -> Result<Endpoints, String> {
    match doc["issuer"].as_str() {
        Some(i) if i.trim_end_matches('/') == issuer => {}
        Some(i) => return Err(format!("the discovery document is for issuer '{i}', not '{issuer}'")),
        None => return Err("the discovery document names no issuer".into()),
    }
    let get = |k: &str| doc[k].as_str().map(str::to_owned);
    let device = get("device_authorization_endpoint").ok_or(
        "the provider offers no device login: enable the OAuth 2.0 Device Authorization Grant on the client",
    )?;
    let token = get("token_endpoint").ok_or("the discovery document has no token_endpoint")?;
    check_url(&device)?;
    check_url(&token)?;
    let revoke = get("revocation_endpoint").filter(|u| check_url(u).is_ok());
    Ok(Endpoints { device, token, revoke })
}

/// A sign-in's tokens.
#[derive(Clone, PartialEq, Eq)]
pub struct Tokens {
    /// The bearer token.
    pub access: String,
    /// The refresh token, if the provider gave one.
    pub refresh: Option<String>,
    /// When the access token expires (Unix seconds).
    pub expires_at: u64,
}

impl std::fmt::Debug for Tokens {
    // Never the tokens themselves.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Tokens(expires_at {}, refresh: {})", self.expires_at, self.refresh.is_some())
    }
}

/// The tokens in a successful token response received at `now`.
pub fn tokens(body: &Value, now: u64) -> Result<Tokens, String> {
    let access = body["access_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("the token response has no access_token")?;
    if let Some(t) = body["token_type"].as_str()
        && !t.eq_ignore_ascii_case("bearer")
    {
        return Err(format!("the token is of type '{t}', not Bearer"));
    }
    Ok(Tokens {
        access: access.to_owned(),
        refresh: body["refresh_token"].as_str().filter(|s| !s.is_empty()).map(str::to_owned),
        expires_at: now.saturating_add(body["expires_in"].as_u64().unwrap_or(60).min(MAX_EXPIRES_IN)),
    })
}

/// What one poll of the token endpoint during a device login means.
#[derive(Debug, PartialEq, Eq)]
pub enum Poll {
    /// Approved: the tokens.
    Done(Tokens),
    /// Not approved yet: poll again after the interval.
    Pending,
    /// Polled too fast: add 5 s to the interval (RFC 8628 3.5).
    SlowDown,
    /// The login is over without tokens.
    Failed(String),
}

/// The meaning of a token endpoint answer (`status`, `body`) at `now`.
pub fn poll_outcome(status: u16, body: &Value, now: u64) -> Poll {
    if (200..300).contains(&status) {
        return match tokens(body, now) {
            Ok(t) => Poll::Done(t),
            Err(e) => Poll::Failed(e),
        };
    }
    match body["error"].as_str() {
        Some("authorization_pending") => Poll::Pending,
        Some("slow_down") => Poll::SlowDown,
        Some("access_denied") => Poll::Failed("the sign-in was refused".into()),
        Some("expired_token") => {
            Poll::Failed("the code expired before it was approved; run `mwctl auth login` again".into())
        }
        _ => Poll::Failed(provider_error(status, body)),
    }
}

/// An error answer as text: the error code and description, never a token.
fn provider_error(status: u16, body: &Value) -> String {
    match (body["error"].as_str(), body["error_description"].as_str()) {
        (Some(e), Some(d)) => format!("the provider answered {e}: {d}"),
        (Some(e), None) => format!("the provider answered {e}"),
        _ => format!("the provider answered HTTP {status}"),
    }
}

/// `application/x-www-form-urlencoded` of `pairs`.
pub fn form(pairs: &[(&str, &str)]) -> String {
    fn enc(s: &str, out: &mut String) {
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(b as char),
                b' ' => out.push('+'),
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
    }
    let mut out = String::new();
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        enc(k, &mut out);
        out.push('=');
        enc(v, &mut out);
    }
    out
}

/// The polling interval a device authorization answer asks for: 5 s by
/// default, at least 1 s and at most [`MAX_INTERVAL`].
pub fn device_interval(d: &Value) -> u64 {
    d["interval"].as_u64().unwrap_or(5).clamp(1, MAX_INTERVAL)
}

/// The interval after a `slow_down` answer (RFC 8628 3.5: 5 s more), at most
/// [`MAX_INTERVAL`].
pub fn slowed(interval: u64) -> u64 {
    interval.saturating_add(5).min(MAX_INTERVAL)
}

/// Unix seconds now.
pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

// ---------------------------------------------------------------------------
// The token store
// ---------------------------------------------------------------------------

fn load(path: &Path) -> Result<Map<String, Value>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(m)) => Ok(m),
            _ => Err(format!("{} is not a token store; remove it and sign in again", path.display())),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// The store's directory, made (mode 0700) if missing. Refused when it is a
/// symlink or, on unix, writable by the group or others: whoever can write
/// it could swap the store or its lock.
fn store_dir(path: &Path) -> Result<PathBuf, String> {
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let err = |e: std::io::Error| format!("{}: {e}", dir.display());
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(&dir).map_err(err)?;
    let meta = std::fs::symlink_metadata(&dir).map_err(err)?;
    if meta.file_type().is_symlink() {
        return Err(format!("{} is a symlink; the token store needs a real directory", dir.display()));
    }
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", dir.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o022 != 0 {
            return Err(format!(
                "{} is writable by the group or others (mode {mode:o}); run: chmod 700 {}",
                dir.display(),
                dir.display()
            ));
        }
    }
    Ok(dir)
}

/// `bytes` in a new file at `p`, mode 0600 on unix, synced to disk. Refuses
/// a path that already exists, a symlink included (`create_new`), so a file
/// planted there is neither reused nor followed.
fn write_new_private(p: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let mut f = o.open(p)?;
    #[cfg(unix)]
    {
        // Explicitly, whatever the umask or the platform did with the mode.
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    std::io::Write::write_all(&mut f, bytes)?;
    f.sync_all()
}

/// A name for a new file beside `path` that no other writer uses.
fn unique_beside(path: &Path, suffix: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    let name = path.file_name().map_or_else(|| "auth.json".into(), |n| n.to_string_lossy().into_owned());
    let n = N.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!(".{name}.{}.{nanos}.{n}.{suffix}", std::process::id()))
}

fn store(path: &Path, all: &Map<String, Value>) -> Result<(), String> {
    let err = |e: std::io::Error| format!("{}: {e}", path.display());
    let dir = store_dir(path)?;
    let text = serde_json::to_string_pretty(&Value::Object(all.clone())).map_err(|e| e.to_string())?;
    let mut tmp = unique_beside(path, "tmp");
    let mut tries = 0;
    loop {
        match write_new_private(&tmp, text.as_bytes()) {
            Ok(()) => break,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && tries < 8 => {
                tries += 1;
                tmp = unique_beside(path, "tmp");
            }
            Err(e) => {
                if e.kind() != std::io::ErrorKind::AlreadyExists {
                    let _ = std::fs::remove_file(&tmp);
                }
                return Err(format!("{}: {e}", tmp.display()));
            }
        }
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err(e));
    }
    // The rename itself survives a crash once the directory is synced.
    #[cfg(unix)]
    if let Ok(d) = std::fs::File::open(&dir) {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// An exclusive hold on the token store: a lock file beside it, made with
/// `create_new` and removed when dropped.
struct StoreLock(PathBuf);

impl StoreLock {
    /// Takes the lock for the store at `path`, waiting up to `wait` for
    /// another mwctl to release it.
    fn take(path: &Path, wait: Duration) -> Result<StoreLock, String> {
        store_dir(path)?;
        let name = path.file_name().map_or_else(|| "auth.json".into(), |n| n.to_string_lossy().into_owned());
        let lock = path.with_file_name(format!("{name}.lock"));
        let start = std::time::Instant::now();
        loop {
            match write_new_private(&lock, std::process::id().to_string().as_bytes()) {
                Ok(()) => return Ok(StoreLock(lock)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if start.elapsed() >= wait {
                        return Err(format!(
                            "{} is locked by another mwctl; if none is running, remove {}",
                            path.display(),
                            lock.display()
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(format!("{}: {e}", lock.display())),
            }
        }
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn lock(cfg: &Config) -> Result<StoreLock, String> {
    StoreLock::take(&cfg.store, LOCK_WAIT)
}

/// The saved sign-in for `cfg`, if any.
pub fn saved(cfg: &Config) -> Result<Option<Tokens>, String> {
    let all = load(&cfg.store)?;
    let Some(e) = all.get(&cfg.entry()) else { return Ok(None) };
    let access = e["access_token"].as_str().unwrap_or_default().to_owned();
    Ok(Some(Tokens {
        access,
        refresh: e["refresh_token"].as_str().map(str::to_owned),
        expires_at: e["expires_at"].as_u64().unwrap_or(0),
    }))
}

/// Saves `t` as `cfg`'s sign-in; the caller holds the store lock.
fn save_locked(cfg: &Config, t: &Tokens) -> Result<(), String> {
    let mut all = load(&cfg.store)?;
    all.insert(
        cfg.entry(),
        json!({ "access_token": t.access, "refresh_token": t.refresh, "expires_at": t.expires_at }),
    );
    store(&cfg.store, &all)
}

/// Saves `t` as `cfg`'s sign-in.
pub fn save(cfg: &Config, t: &Tokens) -> Result<(), String> {
    let _lock = lock(cfg)?;
    save_locked(cfg, t)
}

/// Removes `cfg`'s sign-in and returns it.
pub fn forget(cfg: &Config) -> Result<Option<Tokens>, String> {
    let _lock = lock(cfg)?;
    let old = saved(cfg)?;
    if old.is_some() {
        let mut all = load(&cfg.store)?;
        all.remove(&cfg.entry());
        store(&cfg.store, &all)?;
    }
    Ok(old)
}

// ---------------------------------------------------------------------------
// The provider
// ---------------------------------------------------------------------------

fn client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        // A redirect could carry a POSTed code or refresh token elsewhere.
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("mwctl/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("HTTP client: {e}"))
}

fn send(req: reqwest::blocking::RequestBuilder, what: &str) -> Result<(u16, Value), String> {
    let resp = req.send().map_err(|e| format!("{what}: {}", e.without_url()))?;
    let status = resp.status().as_u16();
    let bytes = resp.bytes().map_err(|e| format!("{what}: {}", e.without_url()))?;
    Ok((status, serde_json::from_slice(&bytes).unwrap_or(Value::Null)))
}

fn post(c: &reqwest::blocking::Client, url: &str, pairs: &[(&str, &str)], what: &str) -> Result<(u16, Value), String> {
    let req = c
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(reqwest::header::ACCEPT, "application/json")
        .body(form(pairs));
    send(req, what)
}

/// The provider's endpoints for `cfg`.
pub fn discover(c: &reqwest::blocking::Client, cfg: &Config) -> Result<Endpoints, String> {
    let url = format!("{}/.well-known/openid-configuration", cfg.issuer);
    let (status, doc) = send(c.get(&url).header(reqwest::header::ACCEPT, "application/json"), "discovery")?;
    if status != 200 {
        return Err(format!("discovery at {url} answered HTTP {status}"));
    }
    endpoints(&doc, &cfg.issuer)
}

/// The device login: `show` is given the line telling the operator where to
/// approve. Saves and returns the tokens.
pub fn login(cfg: &Config, show: &mut dyn FnMut(&str)) -> Result<Tokens, String> {
    let c = client()?;
    let ep = discover(&c, cfg)?;
    let (status, d) = post(&c, &ep.device, &[("client_id", &cfg.client_id), ("scope", SCOPE)], "device login")?;
    if !(200..300).contains(&status) {
        return Err(provider_error(status, &d));
    }
    let field = |k: &str| d[k].as_str().filter(|s| !s.is_empty()).map(str::to_owned);
    let device_code = field("device_code").ok_or("the device login answer has no device_code")?;
    let user_code = field("user_code").ok_or("the device login answer has no user_code")?;
    let uri = field("verification_uri").ok_or("the device login answer has no verification_uri")?;
    check_url(&uri)?;
    let open = field("verification_uri_complete").filter(|u| check_url(u).is_ok()).unwrap_or(uri);
    let mut interval = device_interval(&d);
    let deadline = now().saturating_add(d["expires_in"].as_u64().unwrap_or(600).min(MAX_EXPIRES_IN));
    show(&format!("To sign in, open {open} and confirm the code {user_code}"));
    loop {
        if now() >= deadline {
            return Err("the code expired before it was approved; run `mwctl auth login` again".into());
        }
        std::thread::sleep(Duration::from_secs(interval));
        let pairs = [("grant_type", DEVICE_GRANT), ("device_code", device_code.as_str()), ("client_id", &cfg.client_id)];
        let (status, body) = post(&c, &ep.token, &pairs, "token")?;
        match poll_outcome(status, &body, now()) {
            Poll::Done(t) => {
                save(cfg, &t)?;
                return Ok(t);
            }
            Poll::Pending => {}
            Poll::SlowDown => interval = slowed(interval),
            Poll::Failed(e) => return Err(e),
        }
    }
}

fn not_signed_in(cfg: &Config) -> String {
    format!("not signed in to {}: run `mwctl auth login`", cfg.issuer)
}

/// A valid access token for `cfg`: the saved one, or a refreshed one when it
/// expires within [`EARLY`] seconds.
pub fn access_token(cfg: &Config) -> Result<String, String> {
    let t = saved(cfg)?.ok_or_else(|| not_signed_in(cfg))?;
    if t.expires_at > now().saturating_add(EARLY) && !t.access.is_empty() {
        return Ok(t.access);
    }
    // One refresh at a time: another mwctl may be renewing (and the provider
    // rotating) this refresh token. Read it again under the lock; it may be
    // fresh now.
    let _lock = lock(cfg)?;
    let t = saved(cfg)?.ok_or_else(|| not_signed_in(cfg))?;
    if t.expires_at > now().saturating_add(EARLY) && !t.access.is_empty() {
        return Ok(t.access);
    }
    let refresh = t
        .refresh
        .ok_or_else(|| format!("the sign-in to {} has expired: run `mwctl auth login`", cfg.issuer))?;
    let c = client()?;
    let ep = discover(&c, cfg)?;
    let pairs = [("grant_type", "refresh_token"), ("refresh_token", refresh.as_str()), ("client_id", &cfg.client_id)];
    let (status, body) = post(&c, &ep.token, &pairs, "token refresh")?;
    if !(200..300).contains(&status) {
        return Err(format!(
            "the sign-in to {} could not be renewed ({}): run `mwctl auth login`",
            cfg.issuer,
            provider_error(status, &body)
        ));
    }
    let mut new = tokens(&body, now())?;
    if new.refresh.is_none() {
        new.refresh = Some(refresh);
    }
    save_locked(cfg, &new)?;
    Ok(new.access)
}

/// Forgets `cfg`'s sign-in, revoking its refresh token where the provider
/// offers revocation. Says what was done.
pub fn logout(cfg: &Config) -> Result<String, String> {
    let Some(t) = forget(cfg)? else {
        return Ok(format!("not signed in to {}", cfg.issuer));
    };
    let revoked = t.refresh.as_deref().is_some_and(|refresh| {
        client()
            .and_then(|c| {
                let ep = discover(&c, cfg)?;
                let url = ep.revoke.ok_or_else(String::new)?;
                let pairs = [("token", refresh), ("token_type_hint", "refresh_token"), ("client_id", &cfg.client_id)];
                post(&c, &url, &pairs, "revocation").map(|(s, _)| (200..300).contains(&s))
            })
            .unwrap_or(false)
    });
    Ok(if revoked {
        format!("signed out of {} (the refresh token is revoked)", cfg.issuer)
    } else {
        format!("signed out of {} (forgotten here; the provider did not revoke it)", cfg.issuer)
    })
}

/// One line about `cfg`'s sign-in; never a token.
pub fn status(cfg: &Config) -> Result<String, String> {
    Ok(match saved(cfg)? {
        None => not_signed_in(cfg),
        Some(t) => {
            let left = t.expires_at.saturating_sub(now());
            let renew = if t.refresh.is_some() { ", renewable" } else { "" };
            if left == 0 {
                format!("signed in to {} as client {}: the access token has expired{renew}", cfg.issuer, cfg.client_id)
            } else {
                format!("signed in to {} as client {}: the access token expires in {left} s{renew}", cfg.issuer, cfg.client_id)
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| (*v).to_owned())
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mwctl-auth-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn the_issuer_comes_only_from_the_environment() {
        let e = Config::from_env(env(&[("HOME", "/h")])).unwrap_err();
        assert!(e.contains(ENV_ISSUER), "{e}");
        let c = Config::from_env(env(&[(ENV_ISSUER, "https://auth.example/realms/r/"), ("HOME", "/h")])).unwrap();
        assert_eq!(c.issuer, "https://auth.example/realms/r");
        assert_eq!(c.client_id, DEFAULT_CLIENT_ID);
        assert_eq!(c.store, PathBuf::from("/h/.config/mwctl/auth.json"));
        let c = Config::from_env(env(&[
            (ENV_ISSUER, "https://a.example"),
            (ENV_CLIENT_ID, "tools"),
            ("XDG_CONFIG_HOME", "/x"),
            ("HOME", "/h"),
        ]))
        .unwrap();
        assert_eq!((c.client_id.as_str(), c.store), ("tools", PathBuf::from("/x/mwctl/auth.json")));
    }

    #[test]
    fn only_https_or_local_http_is_accepted() {
        for ok in ["https://auth.example/realms/r", "http://localhost:8080/r", "http://127.0.0.1/x"] {
            assert!(check_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "http://auth.example/r",
            "http://localhost.evil.example/r",
            "http://127.0.0.1@evil.example/r",
            "ftp://x",
            "https://",
            "https:///path",
            "auth.example",
        ] {
            assert!(check_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn discovery_must_be_for_the_issuer_and_offer_a_device_login() {
        let iss = "https://auth.example/realms/r";
        let good = json!({ "issuer": iss, "device_authorization_endpoint": format!("{iss}/device"),
                           "token_endpoint": format!("{iss}/token"), "revocation_endpoint": "http://evil.example/revoke" });
        let ep = endpoints(&good, iss).unwrap();
        assert_eq!(ep.token, format!("{iss}/token"));
        assert_eq!(ep.revoke, None, "a plain-http revocation endpoint is not used");
        let other = json!({ "issuer": "https://evil.example", "device_authorization_endpoint": "https://e/d", "token_endpoint": "https://e/t" });
        assert!(endpoints(&other, iss).unwrap_err().contains("not"));
        let no_device = json!({ "issuer": iss, "token_endpoint": format!("{iss}/token") });
        assert!(endpoints(&no_device, iss).unwrap_err().contains("Device Authorization"));
        let http_token = json!({ "issuer": iss, "device_authorization_endpoint": format!("{iss}/d"), "token_endpoint": "http://evil.example/t" });
        assert!(endpoints(&http_token, iss).is_err());
    }

    #[test]
    fn polls_mean_what_rfc_8628_says() {
        let ok = json!({ "access_token": "a", "refresh_token": "r", "expires_in": 300, "token_type": "Bearer" });
        assert_eq!(
            poll_outcome(200, &ok, 1000),
            Poll::Done(Tokens { access: "a".into(), refresh: Some("r".into()), expires_at: 1300 })
        );
        assert_eq!(poll_outcome(400, &json!({ "error": "authorization_pending" }), 0), Poll::Pending);
        assert_eq!(poll_outcome(400, &json!({ "error": "slow_down" }), 0), Poll::SlowDown);
        assert!(matches!(poll_outcome(400, &json!({ "error": "access_denied" }), 0), Poll::Failed(e) if e.contains("refused")));
        assert!(matches!(poll_outcome(400, &json!({ "error": "expired_token" }), 0), Poll::Failed(e) if e.contains("expired")));
        assert!(matches!(poll_outcome(500, &Value::Null, 0), Poll::Failed(e) if e.contains("500")));
        let huge = json!({ "access_token": "a", "expires_in": u64::MAX });
        assert!(
            matches!(poll_outcome(200, &huge, 1000), Poll::Done(t) if t.expires_at == 1000 + MAX_EXPIRES_IN),
            "expires_in is capped at a day"
        );
        let t = tokens(&json!({ "access_token": "a", "expires_in": 300 }), u64::MAX).unwrap();
        assert_eq!(t.expires_at, u64::MAX, "saturates instead of overflowing");
        let mac = json!({ "access_token": "a", "token_type": "MAC" });
        assert!(matches!(poll_outcome(200, &mac, 0), Poll::Failed(e) if e.contains("Bearer")));
    }

    #[test]
    fn the_polling_interval_is_bounded() {
        assert_eq!(device_interval(&json!({})), 5);
        assert_eq!(device_interval(&json!({ "interval": 0 })), 1);
        assert_eq!(device_interval(&json!({ "interval": u64::MAX })), MAX_INTERVAL);
        assert_eq!(slowed(5), 10);
        assert_eq!(slowed(58), MAX_INTERVAL);
        assert_eq!(slowed(u64::MAX), MAX_INTERVAL);
    }

    #[test]
    fn forms_are_url_encoded() {
        assert_eq!(form(&[("a", "b c"), ("u", "urn:x/y&z=1")]), "a=b+c&u=urn%3Ax%2Fy%26z%3D1");
    }

    #[test]
    fn tokens_are_never_printed() {
        let t = Tokens { access: "secret-access".into(), refresh: Some("secret-refresh".into()), expires_at: 5 };
        let s = format!("{t:?}");
        assert!(!s.contains("secret"), "{s}");
    }

    #[test]
    fn the_store_is_private_and_keeps_issuers_apart() {
        let dir = tmp("store");
        let path = dir.join("cfg/mwctl/auth.json");
        let a = Config { issuer: "https://a".into(), client_id: "mwctl".into(), store: path.clone() };
        let b = Config { issuer: "https://b".into(), ..a.clone() };
        assert_eq!(saved(&a).unwrap(), None);
        let ta = Tokens { access: "A".into(), refresh: Some("RA".into()), expires_at: 9 };
        save(&a, &ta).unwrap();
        save(&b, &Tokens { access: "B".into(), refresh: None, expires_at: 7 }).unwrap();
        assert_eq!(saved(&a).unwrap(), Some(ta.clone()));
        assert_eq!(saved(&b).unwrap().unwrap().access, "B");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        }
        assert_eq!(forget(&a).unwrap(), Some(ta));
        assert_eq!(saved(&a).unwrap(), None);
        assert!(saved(&b).unwrap().is_some(), "the other issuer's sign-in is kept");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A one-thread HTTP/1.1 provider on 127.0.0.1: discovery, device
    /// authorization, and a token endpoint that is pending once, then
    /// approves; refresh and revocation. Records the token requests.
    struct FakeProvider {
        issuer: String,
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl FakeProvider {
        fn start() -> FakeProvider {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let issuer = format!("http://127.0.0.1:{}/realms/test", listener.local_addr().unwrap().port());
            let seen = Arc::new(Mutex::new(Vec::new()));
            let (iss, log) = (issuer.clone(), seen.clone());
            std::thread::spawn(move || {
                let mut polls = 0;
                for stream in listener.incoming() {
                    let Ok(mut s) = stream else { continue };
                    let mut r = BufReader::new(s.try_clone().unwrap());
                    let mut line = String::new();
                    r.read_line(&mut line).unwrap();
                    let path = line.split_whitespace().nth(1).unwrap_or("").to_owned();
                    let mut len = 0;
                    loop {
                        let mut h = String::new();
                        r.read_line(&mut h).unwrap();
                        if h == "\r\n" || h.is_empty() {
                            break;
                        }
                        if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                            len = v.trim().parse().unwrap();
                        }
                    }
                    let mut body = vec![0; len];
                    r.read_exact(&mut body).unwrap();
                    let body = String::from_utf8(body).unwrap();
                    let (code, out) = match path.trim_start_matches("/realms/test") {
                        "/.well-known/openid-configuration" => (200, json!({
                            "issuer": iss,
                            "device_authorization_endpoint": format!("{iss}/device"),
                            "token_endpoint": format!("{iss}/token"),
                            "revocation_endpoint": format!("{iss}/revoke"),
                        })),
                        "/device" => (200, json!({
                            "device_code": "dc-1", "user_code": "WXYZ-1234",
                            "verification_uri": format!("{iss}/device-ui"), "interval": 1, "expires_in": 30,
                        })),
                        "/token" => {
                            log.lock().unwrap().push(body.clone());
                            if body.contains("grant_type=refresh_token") {
                                (200, json!({ "access_token": "access-2", "expires_in": 300, "token_type": "Bearer" }))
                            } else {
                                polls += 1;
                                if polls == 1 {
                                    (400, json!({ "error": "authorization_pending" }))
                                } else {
                                    // Already expired, so the next `token` must refresh.
                                    (200, json!({ "access_token": "access-1", "refresh_token": "refresh-1",
                                                  "expires_in": 0, "token_type": "Bearer" }))
                                }
                            }
                        }
                        "/revoke" => {
                            log.lock().unwrap().push(format!("revoke {body}"));
                            (200, Value::Null)
                        }
                        _ => (404, Value::Null),
                    };
                    let text = if out.is_null() { String::new() } else { out.to_string() };
                    let _ = write!(
                        s,
                        "HTTP/1.1 {code} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                        text.len()
                    );
                }
            });
            FakeProvider { issuer, seen }
        }
    }

    #[test]
    fn login_token_refresh_and_logout_against_a_provider() {
        let p = FakeProvider::start();
        let dir = tmp("flow");
        let cfg = Config { issuer: p.issuer.clone(), client_id: "mwctl".into(), store: dir.join("auth.json") };
        assert!(access_token(&cfg).unwrap_err().contains("auth login"));

        let mut shown = Vec::new();
        let t = login(&cfg, &mut |l| shown.push(l.to_owned())).unwrap();
        assert_eq!(t.access, "access-1");
        assert!(shown[0].contains("WXYZ-1234") && shown[0].contains("/device-ui"), "{shown:?}");
        let polls = p.seen.lock().unwrap().clone();
        assert_eq!(polls.len(), 2, "pending once, then approved");
        assert!(polls[1].contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"));
        assert!(polls[1].contains("device_code=dc-1") && polls[1].contains("client_id=mwctl"));

        // Expired at once: `token` refreshes, keeps the refresh token the
        // provider did not rotate, and saves the new access token.
        assert_eq!(access_token(&cfg).unwrap(), "access-2");
        assert!(p.seen.lock().unwrap().last().unwrap().contains("refresh_token=refresh-1"));
        assert_eq!(saved(&cfg).unwrap().unwrap().refresh.as_deref(), Some("refresh-1"));
        assert_eq!(access_token(&cfg).unwrap(), "access-2", "valid now: no second refresh");
        assert!(status(&cfg).unwrap().contains("expires in"));

        let out = logout(&cfg).unwrap();
        assert!(out.contains("revoked"), "{out}");
        assert!(p.seen.lock().unwrap().last().unwrap().starts_with("revoke token=refresh-1"));
        assert_eq!(saved(&cfg).unwrap(), None);
        assert!(logout(&cfg).unwrap().contains("not signed in"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn one(dir: &Path) -> Config {
        Config { issuer: "https://a".into(), client_id: "mwctl".into(), store: dir.join("auth.json") }
    }

    fn tok() -> Tokens {
        Tokens { access: "A".into(), refresh: None, expires_at: 9 }
    }

    #[cfg(unix)]
    #[test]
    fn a_planted_world_writable_tmp_file_does_not_loosen_the_store() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp("planted");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let planted = dir.join("auth.tmp");
        std::fs::write(&planted, "planted").unwrap();
        std::fs::set_permissions(&planted, std::fs::Permissions::from_mode(0o666)).unwrap();
        save(&one(&dir), &tok()).unwrap();
        let store = dir.join("auth.json");
        assert_eq!(std::fs::metadata(&store).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_to_string(&planted).unwrap(), "planted", "not reused");
        // And a temporary file that already exists is never opened.
        assert!(write_new_private(&planted, b"x").is_err());
        assert_eq!(std::fs::read_to_string(&planted).unwrap(), "planted");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_tmp_file_is_not_followed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp("symlink");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let victim = tmp("symlink-victim");
        std::fs::create_dir_all(&victim).unwrap();
        let target = victim.join("target");
        std::fs::write(&target, "untouched").unwrap();
        let link = dir.join("auth.tmp");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let dangling = dir.join("dangling.tmp");
        std::os::unix::fs::symlink(victim.join("new"), &dangling).unwrap();
        save(&one(&dir), &tok()).unwrap();
        assert!(write_new_private(&link, b"x").is_err(), "an existing symlink is refused");
        assert!(write_new_private(&dangling, b"x").is_err(), "a dangling one too");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "untouched");
        assert!(!victim.join("new").exists());
        assert_eq!(saved(&one(&dir)).unwrap(), Some(tok()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&victim);
    }

    #[cfg(unix)]
    #[test]
    fn a_group_writable_or_symlinked_store_directory_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp("groupw");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o770)).unwrap();
        let e = save(&one(&dir), &tok()).unwrap_err();
        assert!(e.contains("writable by the group or others"), "{e}");
        assert!(!dir.join("auth.json").exists());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o707)).unwrap();
        assert!(save(&one(&dir), &tok()).is_err(), "world-writable too");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let real = tmp("linked-real");
        std::fs::create_dir_all(&real).unwrap();
        let linked = dir.join("linked");
        std::os::unix::fs::symlink(&real, &linked).unwrap();
        let e = save(&one(&linked), &tok()).unwrap_err();
        assert!(e.contains("symlink"), "{e}");
        assert!(!real.join("auth.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&real);
    }

    #[test]
    fn the_store_lock_is_exclusive_and_released() {
        let dir = tmp("lock");
        let cfg = one(&dir);
        let held = StoreLock::take(&cfg.store, Duration::ZERO).unwrap();
        let e = StoreLock::take(&cfg.store, Duration::from_millis(120)).err().unwrap();
        assert!(e.contains("locked by another mwctl"), "{e}");
        drop(held);
        assert!(!dir.join("auth.json.lock").exists(), "removed after use");
        // Concurrent writers each keep their entry: no lost update.
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let cfg = Config { issuer: format!("https://i{i}"), ..cfg.clone() };
                std::thread::spawn(move || save(&cfg, &tok()).unwrap())
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(load(&cfg.store).unwrap().len(), 8);
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left, [std::ffi::OsString::from("auth.json")], "no temporary or lock file left");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
