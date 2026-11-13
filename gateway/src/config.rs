//! [`GatewayConfig`]: the gateway's own settings, parsed purely from its
//! environment by [`GatewayConfig::from_env`] (the runner's `MW_*`
//! variables are `mw_service::Config`'s).
//!
//! | variable | default | format |
//! |----------|---------|--------|
//! | `MW_GATEWAY_SERIAL` | `/dev/ttyUSB0` | path of the device's serial port (opened as is, never searched for) |
//! | `MW_GATEWAY_BAUD` | `115200` | decimal, non-zero |
//! | `MW_GATEWAY_NAMESPACE` | none | one key segment: the device's `app/<rest>` is `app/<ns>/<rest>` in the cluster |
//! | `MW_GATEWAY_DOWNLINK` | none | `,`-separated filters (`key` or `key/**`) under the cluster prefix (`app` or `app/<ns>`), at most 8 |
//! | `MW_GATEWAY_SYS_ALLOW` | `sys/info,sys/log` | `,`-separated exact `sys/...` keys `gw/sys` passes, at most 8 |
//! | `MW_GATEWAY_DEVICE_MAC` | none | `aa:bb:cc:dd:ee:ff`: bind only to this device |
//! | `MW_GATEWAY_SYS_TIMEOUT_MS` | `2000` | decimal milliseconds a `gw/sys` request waits, 1..=60000 |
//! | `MW_GATEWAY_ALLOW_CALLERS` | none | `,`-separated SPIFFE IDs admitted to `gw/sys` |
//!
//! Unset or empty is the default; a malformed value is a
//! [`GatewayConfigError`] naming the variable.

use middleware::SenderId;
use middleware::key::Key;
use std::fmt;
use std::time::Duration;

use crate::bridge::{Bridge, KeyMap, MAX_DOWNLINK_FILTERS, OwnedFilter, SysAllow};

/// `MW_GATEWAY_SERIAL`.
pub const ENV_SERIAL: &str = "MW_GATEWAY_SERIAL";
/// `MW_GATEWAY_BAUD`.
pub const ENV_BAUD: &str = "MW_GATEWAY_BAUD";
/// `MW_GATEWAY_NAMESPACE`.
pub const ENV_NAMESPACE: &str = "MW_GATEWAY_NAMESPACE";
/// `MW_GATEWAY_DOWNLINK`.
pub const ENV_DOWNLINK: &str = "MW_GATEWAY_DOWNLINK";
/// `MW_GATEWAY_SYS_ALLOW`.
pub const ENV_SYS_ALLOW: &str = "MW_GATEWAY_SYS_ALLOW";
/// `MW_GATEWAY_DEVICE_MAC`.
pub const ENV_DEVICE_MAC: &str = "MW_GATEWAY_DEVICE_MAC";
/// `MW_GATEWAY_SYS_TIMEOUT_MS`.
pub const ENV_SYS_TIMEOUT_MS: &str = "MW_GATEWAY_SYS_TIMEOUT_MS";
/// `MW_GATEWAY_ALLOW_CALLERS`.
pub const ENV_ALLOW_CALLERS: &str = "MW_GATEWAY_ALLOW_CALLERS";

/// The default serial port.
pub const DEFAULT_SERIAL: &str = "/dev/ttyUSB0";
/// The default baud rate (the ESP base firmware's console UART).
pub const DEFAULT_BAUD: u32 = 115_200;

/// A malformed gateway variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayConfigError {
    /// The variable.
    pub var: &'static str,
    /// Its value.
    pub value: String,
    /// What is wrong.
    pub reason: &'static str,
}

impl fmt::Display for GatewayConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}={:?}: {}", self.var, self.value, self.reason)
    }
}

impl std::error::Error for GatewayConfigError {}

/// The gateway's settings (module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayConfig {
    /// The serial port.
    pub serial: String,
    /// Its baud rate.
    pub baud: u32,
    /// Device ↔ cluster keys.
    pub map: KeyMap,
    /// Cluster filters relayed to the device.
    pub downlink: Vec<OwnedFilter>,
    /// What `gw/sys` passes.
    pub sys_allow: SysAllow,
    /// The only device to bind to.
    pub device_mac: Option<[u8; 6]>,
    /// How long `gw/sys` waits for the device.
    pub sys_timeout: Duration,
    /// SPIFFE IDs admitted to `gw/sys`.
    pub allow_callers: Vec<String>,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        GatewayConfig {
            serial: DEFAULT_SERIAL.to_owned(),
            baud: DEFAULT_BAUD,
            map: KeyMap::identity(),
            downlink: Vec::new(),
            sys_allow: SysAllow::default(),
            device_mac: None,
            sys_timeout: crate::shell::SYS_TIMEOUT,
            allow_callers: Vec::new(),
        }
    }
}

fn list(value: &str) -> impl Iterator<Item = &str> {
    value.split(',').map(str::trim).filter(|e| !e.is_empty())
}

/// Parses `aa:bb:cc:dd:ee:ff` (hex, either case).
pub fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let mut mac = [0u8; 6];
    let mut parts = s.split(':');
    for b in mac.iter_mut() {
        let p = parts.next()?;
        if p.len() != 2 {
            return None;
        }
        *b = u8::from_str_radix(p, 16).ok()?;
    }
    parts.next().is_none().then_some(mac)
}

/// The text of a filter: `key` or `key/**` (the form
/// `mw_service::spec::parse_filter` reads).
pub fn filter_text(f: &OwnedFilter) -> String {
    match f {
        middleware::key::Filter::Exact(k) => k.as_str().to_owned(),
        middleware::key::Filter::Prefix(k) => format!("{}/**", k.as_str()),
    }
}

impl GatewayConfig {
    /// Parses the variables of the module docs through `var`. Pure.
    pub fn from_env(var: &impl Fn(&str) -> Option<String>) -> Result<Self, GatewayConfigError> {
        let get = |name: &'static str| var(name).filter(|v| !v.is_empty());
        let err = |var: &'static str, value: &str, reason: &'static str| GatewayConfigError {
            var,
            value: value.to_owned(),
            reason,
        };
        let d = GatewayConfig::default();
        let serial = get(ENV_SERIAL).unwrap_or(d.serial);
        let baud = match get(ENV_BAUD) {
            Some(v) => v
                .trim()
                .parse::<u32>()
                .ok()
                .filter(|b| *b > 0)
                .ok_or_else(|| err(ENV_BAUD, &v, "not a positive number"))?,
            None => d.baud,
        };
        let map = match get(ENV_NAMESPACE) {
            Some(v) => KeyMap::namespaced(v.trim())
                .ok_or_else(|| err(ENV_NAMESPACE, &v, "not one key segment"))?,
            None => d.map,
        };
        let mut downlink = Vec::new();
        if let Some(v) = get(ENV_DOWNLINK) {
            for entry in list(&v) {
                let f = mw_service::spec::parse_filter(entry)
                    .and_then(|f| f.to_buf().ok())
                    .ok_or_else(|| err(ENV_DOWNLINK, &v, "an entry is not key or key/**"))?;
                if map.to_device(f.key()).is_none() {
                    return Err(err(
                        ENV_DOWNLINK,
                        &v,
                        "an entry is not under the cluster prefix (app or app/<namespace>)",
                    ));
                }
                if downlink.len() >= MAX_DOWNLINK_FILTERS {
                    return Err(err(ENV_DOWNLINK, &v, "more than 8 filters"));
                }
                downlink.push(f);
            }
        }
        let sys_allow = match get(ENV_SYS_ALLOW) {
            Some(v) => {
                let keys: Option<Vec<Key<'_>>> = list(&v).map(|k| Key::new(k).ok()).collect();
                keys.and_then(SysAllow::new)
                    .ok_or_else(|| err(ENV_SYS_ALLOW, &v, "not up to 8 sys/... keys"))?
            }
            None => d.sys_allow,
        };
        let device_mac = get(ENV_DEVICE_MAC)
            .map(|v| {
                parse_mac(v.trim()).ok_or_else(|| err(ENV_DEVICE_MAC, &v, "not aa:bb:cc:dd:ee:ff"))
            })
            .transpose()?;
        let sys_timeout = match get(ENV_SYS_TIMEOUT_MS) {
            Some(v) => v
                .trim()
                .parse::<u64>()
                .ok()
                .filter(|ms| (1..=60_000).contains(ms))
                .map(Duration::from_millis)
                .ok_or_else(|| err(ENV_SYS_TIMEOUT_MS, &v, "not 1..=60000 milliseconds"))?,
            None => d.sys_timeout,
        };
        let allow_callers = match get(ENV_ALLOW_CALLERS) {
            Some(v) => {
                let ids: Vec<String> = list(&v).map(str::to_owned).collect();
                if !ids.iter().all(|i| i.starts_with("spiffe://")) {
                    return Err(err(ENV_ALLOW_CALLERS, &v, "not SPIFFE IDs"));
                }
                ids
            }
            None => Vec::new(),
        };
        Ok(GatewayConfig {
            serial,
            baud,
            map,
            downlink,
            sys_allow,
            device_mac,
            sys_timeout,
            allow_callers,
        })
    }

    /// [`GatewayConfig::from_env`] over the process environment.
    pub fn from_process_env() -> Result<Self, GatewayConfigError> {
        Self::from_env(&|k| std::env::var(k).ok())
    }

    /// The bridge these settings describe.
    pub fn bridge(&self) -> Bridge {
        Bridge::new(
            self.map.clone(),
            self.downlink.clone(),
            self.sys_allow.clone(),
            self.device_mac.map(SenderId::from_mac),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn v15_defaults() {
        let c = GatewayConfig::from_env(&env(&[])).unwrap();
        assert_eq!(c, GatewayConfig::default());
        assert_eq!((c.serial.as_str(), c.baud), ("/dev/ttyUSB0", 115_200));
        assert!(c.downlink.is_empty() && c.allow_callers.is_empty());
        assert_eq!(c.sys_allow, SysAllow::default());
        assert_eq!(c.bridge().device(), None);
    }

    #[test]
    fn v15_parses_every_variable() {
        let c = GatewayConfig::from_env(&env(&[
            ("MW_GATEWAY_SERIAL", "/dev/ttyACM0"),
            ("MW_GATEWAY_BAUD", "921600"),
            ("MW_GATEWAY_NAMESPACE", "bench"),
            (
                "MW_GATEWAY_DOWNLINK",
                "app/bench/robot/cmd/**, app/bench/led",
            ),
            ("MW_GATEWAY_SYS_ALLOW", "sys/info"),
            ("MW_GATEWAY_DEVICE_MAC", "24:6F:28:aa:bb:cc"),
            ("MW_GATEWAY_SYS_TIMEOUT_MS", "500"),
            (
                "MW_GATEWAY_ALLOW_CALLERS",
                "spiffe://infra.local/ns/apps/sa/demo-caller",
            ),
        ]))
        .unwrap();
        assert_eq!((c.serial.as_str(), c.baud), ("/dev/ttyACM0", 921_600));
        assert_eq!(c.map.namespace(), Some("bench"));
        let texts: Vec<_> = c.downlink.iter().map(filter_text).collect();
        assert_eq!(texts, ["app/bench/robot/cmd/**", "app/bench/led"]);
        assert_eq!(
            c.sys_allow.keys().map(|k| k.as_str()).collect::<Vec<_>>(),
            ["sys/info"]
        );
        assert_eq!(c.device_mac, Some([0x24, 0x6F, 0x28, 0xAA, 0xBB, 0xCC]));
        assert_eq!(c.sys_timeout, Duration::from_millis(500));
        assert_eq!(c.allow_callers.len(), 1);
    }

    #[test]
    fn v15_malformed_values_name_the_variable() {
        for (var, value) in [
            ("MW_GATEWAY_BAUD", "0"),
            ("MW_GATEWAY_BAUD", "fast"),
            ("MW_GATEWAY_NAMESPACE", "a/b"),
            ("MW_GATEWAY_DOWNLINK", "sys/info"),
            ("MW_GATEWAY_DOWNLINK", "app/x/*"),
            ("MW_GATEWAY_SYS_ALLOW", "sys/info,app/x"),
            ("MW_GATEWAY_DEVICE_MAC", "24:6f:28"),
            ("MW_GATEWAY_SYS_TIMEOUT_MS", "0"),
            ("MW_GATEWAY_ALLOW_CALLERS", "demo-caller"),
        ] {
            let e = GatewayConfig::from_env(&env(&[(var, value)])).unwrap_err();
            assert_eq!((e.var, e.value.as_str()), (var, value));
            assert!(e.to_string().starts_with(var));
        }
        // With a namespace, downlink filters live under app/<ns>.
        let e = GatewayConfig::from_env(&env(&[
            ("MW_GATEWAY_NAMESPACE", "bench"),
            ("MW_GATEWAY_DOWNLINK", "app/robot/cmd"),
        ]))
        .unwrap_err();
        assert_eq!(e.var, "MW_GATEWAY_DOWNLINK");
        let nine = (0..9)
            .map(|i| format!("app/k{i}"))
            .collect::<Vec<_>>()
            .join(",");
        assert!(GatewayConfig::from_env(&env(&[("MW_GATEWAY_DOWNLINK", &nine)])).is_err());
        assert_eq!(parse_mac("00:11:22:33:44:55:66"), None);
        assert_eq!(parse_mac("0:11:22:33:44:55"), None);
    }
}
