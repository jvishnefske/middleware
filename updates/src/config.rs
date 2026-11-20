//! The binaries' environment, parsed purely from a lookup function.
//!
//! `mw-updater`:
//!
//! | variable | default | meaning |
//! |----------|---------|---------|
//! | `MW_UPDATES_HOST` | — | the channel's host, e.g. `updates.example.com`: `wss://<host>/zenoh` |
//! | `MW_UPDATES_ADDR` | resolve the host | `ip:port` to dial instead (a LAN without hairpin NAT) |
//! | `MW_UPDATES_ENDPOINT` | — | a Zenoh locator to connect to directly instead of the host |
//! | `MW_UPDATES_CHANNEL` | `stable` | the channel |
//! | `MW_UPDATES_ROOT_KEY` | — (required) | the root public key, hex (32 bytes Ed25519, 33 P-256) |
//! | `MW_ZENOH_CREDENTIALS` | none | a file holding one line `<user>:<password>` |
//! | `MW_UPDATER_DEVICES` | — (required) | serial ports, comma-separated |
//! | `MW_UPDATER_BAUD` | `115200` | their baud rate |
//! | `MW_UPDATES_POLL` | `3600` | seconds between passes (a publish starts one at once) |
//! | `MW_UPDATER_NODE` | `/etc/hostname` | the name it reports status under |
//!
//! `mw-updates` (the server): `MW_UPDATES_DATA` (default `/data`),
//! `MW_UPDATES_ROOT_KEY`, `MW_ZENOH_ROUTER` (default
//! `tcp/127.0.0.1:7447`), `MW_ZENOH_CREDENTIALS`.
//!
//! `mw-release` reads the updater's route variables and
//! `MW_ZENOH_CREDENTIALS`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use middleware::loader::trust::PublicKey;

use crate::client::Route;
use crate::keys::valid_name;

/// A malformed or missing variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    /// The variable.
    pub var: &'static str,
    /// What is wrong.
    pub why: String,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.var, self.why)
    }
}

impl std::error::Error for ConfigError {}

fn err(var: &'static str, why: impl Into<String>) -> ConfigError {
    ConfigError {
        var,
        why: why.into(),
    }
}

/// Lookup of one variable; unset and empty are the same.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

fn get(env: Env<'_>, var: &str) -> Option<String> {
    env(var).filter(|v| !v.trim().is_empty())
}

/// The route from `MW_UPDATES_ENDPOINT`, or `MW_UPDATES_HOST` with
/// `MW_UPDATES_ADDR`.
pub fn route(env: Env<'_>) -> Result<Route, ConfigError> {
    if let Some(e) = get(env, "MW_UPDATES_ENDPOINT") {
        return Ok(Route::Direct(e));
    }
    let host = get(env, "MW_UPDATES_HOST")
        .ok_or_else(|| err("MW_UPDATES_HOST", "required (or MW_UPDATES_ENDPOINT)"))?;
    if host.contains(['/', ':', ' ']) {
        return Err(err(
            "MW_UPDATES_HOST",
            "a host name, without scheme or path",
        ));
    }
    let addr = match get(env, "MW_UPDATES_ADDR") {
        None => None,
        Some(a) => Some(
            a.parse::<SocketAddr>()
                .map_err(|_| err("MW_UPDATES_ADDR", "ip:port"))?,
        ),
    };
    Ok(Route::Wss { host, addr })
}

/// The root key from `MW_UPDATES_ROOT_KEY`.
pub fn root_key(env: Env<'_>) -> Result<PublicKey, ConfigError> {
    let v =
        get(env, "MW_UPDATES_ROOT_KEY").ok_or_else(|| err("MW_UPDATES_ROOT_KEY", "required"))?;
    crate::parse_root_key(&v).ok_or_else(|| {
        err(
            "MW_UPDATES_ROOT_KEY",
            "hex of a 32-byte Ed25519 or 33-byte P-256 public key",
        )
    })
}

/// The credentials file named by `MW_ZENOH_CREDENTIALS`, read through
/// `read`.
pub fn credentials(
    env: Env<'_>,
    read: &dyn Fn(&str) -> std::io::Result<String>,
) -> Result<Option<(String, String)>, ConfigError> {
    let Some(path) = get(env, "MW_ZENOH_CREDENTIALS") else {
        return Ok(None);
    };
    let var = "MW_ZENOH_CREDENTIALS";
    let text = read(&path).map_err(|e| err(var, format!("{path}: {e}")))?;
    crate::router::parse_credentials(&text)
        .map(Some)
        .ok_or_else(|| err(var, "one line <user>:<password>"))
}

/// `mw-updater`'s settings.
#[derive(Debug, Clone)]
pub struct UpdaterConfig {
    /// How to reach the channel.
    pub route: Route,
    /// The channel.
    pub channel: String,
    /// The root key.
    pub root: PublicKey,
    /// The serial ports.
    pub devices: Vec<String>,
    /// Their baud rate.
    pub baud: u32,
    /// Time between passes.
    pub poll: Duration,
    /// The status name.
    pub node: String,
}

impl UpdaterConfig {
    /// Parses the variables; `hostname` is the fallback node name.
    pub fn from_env(env: Env<'_>, hostname: &str) -> Result<Self, ConfigError> {
        let channel = get(env, "MW_UPDATES_CHANNEL").unwrap_or_else(|| "stable".into());
        if !valid_name(&channel) {
            return Err(err("MW_UPDATES_CHANNEL", "1-32 of a-z 0-9 . _ -"));
        }
        let devices: Vec<String> = get(env, "MW_UPDATER_DEVICES")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect();
        if devices.is_empty() {
            return Err(err("MW_UPDATER_DEVICES", "at least one serial port"));
        }
        let num = |var: &'static str, default: u64, range: std::ops::RangeInclusive<u64>| match get(
            env, var,
        ) {
            None => Ok(default),
            Some(v) => v
                .trim()
                .parse::<u64>()
                .ok()
                .filter(|n| range.contains(n))
                .ok_or_else(|| err(var, format!("{}..={}", range.start(), range.end()))),
        };
        let poll = num("MW_UPDATES_POLL", 3600, 10..=7 * 86400)?;
        let baud = num("MW_UPDATER_BAUD", 115_200, 1200..=4_000_000)? as u32;
        let node = get(env, "MW_UPDATER_NODE").unwrap_or_else(|| hostname.trim().to_lowercase());
        if !valid_name(&node) {
            return Err(err(
                "MW_UPDATER_NODE",
                "1-32 of a-z 0-9 . _ - (set it explicitly)",
            ));
        }
        Ok(UpdaterConfig {
            route: route(env)?,
            channel,
            root: root_key(env)?,
            devices,
            baud,
            poll: Duration::from_secs(poll),
            node,
        })
    }
}

/// `mw-updates`' settings.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// The store directory.
    pub data: PathBuf,
    /// The root key.
    pub root: PublicKey,
    /// The router's locator.
    pub router: String,
}

impl ServerConfig {
    /// Parses the variables.
    pub fn from_env(env: Env<'_>) -> Result<Self, ConfigError> {
        Ok(ServerConfig {
            data: PathBuf::from(get(env, "MW_UPDATES_DATA").unwrap_or_else(|| "/data".into())),
            root: root_key(env)?,
            router: get(env, "MW_ZENOH_ROUTER").unwrap_or_else(|| "tcp/127.0.0.1:7447".into()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = "03a107bff3ce10be1d70dd18e74bc09967e4d6309ba50d5f1ddc8664125531b8";

    fn env_of(vars: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| {
            vars.iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn up6_updater_config_parses_with_defaults() {
        let e = env_of(&[
            ("MW_UPDATES_HOST", "updates.example.com"),
            ("MW_UPDATES_ROOT_KEY", ROOT),
            ("MW_UPDATER_DEVICES", "/dev/ttyACM0, /run/mw/node-pty"),
        ]);
        let c = UpdaterConfig::from_env(&e, "Pi-3\n").unwrap();
        assert!(
            matches!(c.route, Route::Wss { ref host, addr: None } if host == "updates.example.com")
        );
        assert_eq!(c.channel, "stable");
        assert_eq!(c.devices, ["/dev/ttyACM0", "/run/mw/node-pty"]);
        assert_eq!(
            (c.baud, c.poll.as_secs(), c.node.as_str()),
            (115_200, 3600, "pi-3")
        );
    }

    #[test]
    fn up6_updater_config_errors_name_the_variable() {
        let missing = env_of(&[
            ("MW_UPDATES_ROOT_KEY", ROOT),
            ("MW_UPDATER_DEVICES", "/dev/x"),
        ]);
        assert_eq!(
            UpdaterConfig::from_env(&missing, "h").unwrap_err().var,
            "MW_UPDATES_HOST"
        );
        let bad_key = env_of(&[
            ("MW_UPDATES_HOST", "h"),
            ("MW_UPDATES_ROOT_KEY", "00"),
            ("MW_UPDATER_DEVICES", "/dev/x"),
        ]);
        assert_eq!(
            UpdaterConfig::from_env(&bad_key, "h").unwrap_err().var,
            "MW_UPDATES_ROOT_KEY"
        );
        let url = env_of(&[
            ("MW_UPDATES_HOST", "https://h/zenoh"),
            ("MW_UPDATES_ROOT_KEY", ROOT),
            ("MW_UPDATER_DEVICES", "/dev/x"),
        ]);
        assert_eq!(
            UpdaterConfig::from_env(&url, "h").unwrap_err().var,
            "MW_UPDATES_HOST"
        );
        let direct = env_of(&[
            ("MW_UPDATES_ENDPOINT", "tcp/10.0.0.2:7447"),
            ("MW_UPDATES_ROOT_KEY", ROOT),
            ("MW_UPDATER_DEVICES", "/dev/x"),
            ("MW_UPDATES_POLL", "5"),
        ]);
        assert_eq!(
            UpdaterConfig::from_env(&direct, "h").unwrap_err().var,
            "MW_UPDATES_POLL"
        );
        let addr = env_of(&[
            ("MW_UPDATES_HOST", "h"),
            ("MW_UPDATES_ADDR", "192.168.1.185:443"),
        ]);
        assert!(matches!(route(&addr), Ok(Route::Wss { addr: Some(_), .. })));
    }

    #[test]
    fn credentials_file_is_read_and_checked() {
        let e = env_of(&[("MW_ZENOH_CREDENTIALS", "/run/creds")]);
        let ok = |_: &str| Ok("pi:secret\n".to_owned());
        assert_eq!(
            credentials(&e, &ok).unwrap(),
            Some(("pi".into(), "secret".into()))
        );
        let bad = |_: &str| Ok("nocolon".to_owned());
        assert!(credentials(&e, &bad).is_err());
        let none = env_of(&[]);
        assert_eq!(credentials(&none, &ok).unwrap(), None);
    }
}
