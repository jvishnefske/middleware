//! [`PiNodeConfig`]: the node's own settings, parsed purely from its
//! environment by [`PiNodeConfig::from_env`].
//!
//! | variable | default | format |
//! |----------|---------|--------|
//! | `MW_PI_NODE_STATE` | `/var/lib/mw-pi-node` | directory holding the payload store and the trust record (created if missing, locked while the node runs) |
//! | `MW_PI_NODE_NODE_ID` | from the machine id | 16 hex digits ([`crate::node_id`]) |
//! | `MW_PI_NODE_MACHINE_ID` | `/etc/machine-id` | path of the systemd machine id file the node id is derived from |
//! | `MW_PI_NODE_MEMORY_BYTES` | `1048576` (1 MiB) | a payload's largest linear memory, decimal bytes, 65536..=67108864 |
//! | `MW_PI_NODE_LOCAL_SERIAL` | none | path of a serial port (a USB gadget's `/dev/ttyGS0`, a PTY) served beside the gateway at 115200 baud, for `mwctl` |
//!
//! The in-process gateway reads its own `MW_GATEWAY_*` variables
//! (`mw_gateway::GatewayConfig`; `MW_GATEWAY_SERIAL` and `MW_GATEWAY_BAUD`
//! are not used, the device is in the process) and the runner its `MW_*`
//! variables (`mw_service::Config`; `MW_ZENOH_ROUTER` is required).
//!
//! Unset or empty is the default; a malformed value is a
//! [`PiNodeConfigError`] naming the variable.

use std::fmt;
use std::path::PathBuf;

/// `MW_PI_NODE_STATE`.
pub const ENV_STATE: &str = "MW_PI_NODE_STATE";
/// `MW_PI_NODE_NODE_ID`.
pub const ENV_NODE_ID: &str = "MW_PI_NODE_NODE_ID";
/// `MW_PI_NODE_MACHINE_ID`.
pub const ENV_MACHINE_ID: &str = "MW_PI_NODE_MACHINE_ID";
/// `MW_PI_NODE_MEMORY_BYTES`.
pub const ENV_MEMORY_BYTES: &str = "MW_PI_NODE_MEMORY_BYTES";
/// `MW_PI_NODE_LOCAL_SERIAL`.
pub const ENV_LOCAL_SERIAL: &str = "MW_PI_NODE_LOCAL_SERIAL";

/// Default state directory.
pub const DEFAULT_STATE: &str = "/var/lib/mw-pi-node";
/// Default machine id file.
pub const DEFAULT_MACHINE_ID: &str = "/etc/machine-id";
/// Default payload memory cap.
pub const DEFAULT_MEMORY_BYTES: u32 = 1024 * 1024;
/// Smallest memory cap: one 64 KiB page.
pub const MIN_MEMORY_BYTES: u32 = 64 * 1024;
/// Largest memory cap.
pub const MAX_MEMORY_BYTES: u32 = 64 * 1024 * 1024;

/// A variable whose value could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiNodeConfigError {
    /// The variable.
    pub var: &'static str,
    /// Its value.
    pub value: String,
    /// What is wrong with it.
    pub reason: &'static str,
}

impl fmt::Display for PiNodeConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}={:?}: {}", self.var, self.value, self.reason)
    }
}

impl std::error::Error for PiNodeConfigError {}

/// Where the node id comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeIdSource {
    /// `MW_PI_NODE_NODE_ID`, already checked.
    Configured(middleware::SenderId),
    /// Derived from the machine id file at this path.
    MachineId(PathBuf),
}

/// The node's settings (module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiNodeConfig {
    /// The state directory.
    pub state: PathBuf,
    /// Where the node id comes from.
    pub node_id: NodeIdSource,
    /// A payload's largest linear memory in bytes.
    pub memory_bytes: u32,
    /// A local serial port served beside the gateway.
    pub local_serial: Option<String>,
}

impl Default for PiNodeConfig {
    fn default() -> Self {
        PiNodeConfig {
            state: PathBuf::from(DEFAULT_STATE),
            node_id: NodeIdSource::MachineId(PathBuf::from(DEFAULT_MACHINE_ID)),
            memory_bytes: DEFAULT_MEMORY_BYTES,
            local_serial: None,
        }
    }
}

impl PiNodeConfig {
    /// Parses the variables of the module docs through `var`. Pure.
    pub fn from_env(var: &impl Fn(&str) -> Option<String>) -> Result<Self, PiNodeConfigError> {
        let get = |name: &'static str| var(name).filter(|v| !v.trim().is_empty());
        let err = |var: &'static str, value: &str, reason: &'static str| PiNodeConfigError {
            var,
            value: value.to_owned(),
            reason,
        };
        let d = PiNodeConfig::default();
        let state = get(ENV_STATE).map_or(d.state, |v| PathBuf::from(v.trim()));
        let node_id = match (get(ENV_NODE_ID), get(ENV_MACHINE_ID)) {
            (Some(v), _) => NodeIdSource::Configured(
                crate::node_id::parse_node_id(&v)
                    .map_err(|_| err(ENV_NODE_ID, &v, "not 16 hex digits (and not all zeros)"))?,
            ),
            (None, Some(path)) => NodeIdSource::MachineId(PathBuf::from(path.trim())),
            (None, None) => d.node_id,
        };
        let memory_bytes = match get(ENV_MEMORY_BYTES) {
            Some(v) => v
                .trim()
                .parse::<u32>()
                .ok()
                .filter(|n| (MIN_MEMORY_BYTES..=MAX_MEMORY_BYTES).contains(n))
                .ok_or_else(|| err(ENV_MEMORY_BYTES, &v, "not 65536..=67108864 bytes"))?,
            None => d.memory_bytes,
        };
        let local_serial = get(ENV_LOCAL_SERIAL).map(|v| v.trim().to_owned());
        Ok(PiNodeConfig {
            state,
            node_id,
            memory_bytes,
            local_serial,
        })
    }

    /// [`PiNodeConfig::from_env`] over the process environment.
    pub fn from_process_env() -> Result<Self, PiNodeConfigError> {
        Self::from_env(&|k| std::env::var(k).ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::SenderId;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| (*v).to_owned())
    }

    #[test]
    fn pn1_defaults() {
        let c = PiNodeConfig::from_env(&env(&[])).expect("defaults");
        assert_eq!(c, PiNodeConfig::default());
        assert_eq!(c.state, PathBuf::from("/var/lib/mw-pi-node"));
        assert_eq!(c.node_id, NodeIdSource::MachineId(PathBuf::from("/etc/machine-id")));
        assert_eq!(c.memory_bytes, 1024 * 1024);
        assert_eq!(c.local_serial, None);
        // Empty is unset.
        let c = PiNodeConfig::from_env(&env(&[(ENV_STATE, ""), (ENV_MEMORY_BYTES, " ")])).expect("empty");
        assert_eq!(c, PiNodeConfig::default());
    }

    #[test]
    fn pn1_parses_every_variable() {
        let c = PiNodeConfig::from_env(&env(&[
            (ENV_STATE, "/srv/node"),
            (ENV_MACHINE_ID, "/run/machine-id"),
            (ENV_MEMORY_BYTES, "65536"),
            (ENV_LOCAL_SERIAL, "/dev/ttyGS0"),
        ]))
        .expect("config");
        assert_eq!(c.state, PathBuf::from("/srv/node"));
        assert_eq!(c.node_id, NodeIdSource::MachineId(PathBuf::from("/run/machine-id")));
        assert_eq!(c.memory_bytes, 65536);
        assert_eq!(c.local_serial.as_deref(), Some("/dev/ttyGS0"));
        // A configured node id wins over the machine id file.
        let c = PiNodeConfig::from_env(&env(&[
            (ENV_NODE_ID, "00000000000000ff"),
            (ENV_MACHINE_ID, "/run/machine-id"),
            (ENV_MEMORY_BYTES, "67108864"),
        ]))
        .expect("config");
        assert_eq!(c.node_id, NodeIdSource::Configured(SenderId([0, 0, 0, 0, 0, 0, 0, 0xff])));
        assert_eq!(c.memory_bytes, 64 * 1024 * 1024);
    }

    #[test]
    fn pn1_malformed_values_name_the_variable() {
        for (var, value) in [
            (ENV_NODE_ID, "xyz"),
            (ENV_NODE_ID, "0000000000000000"),
            (ENV_MEMORY_BYTES, "65535"),
            (ENV_MEMORY_BYTES, "67108865"),
            (ENV_MEMORY_BYTES, "1MiB"),
        ] {
            let e = PiNodeConfig::from_env(&env(&[(var, value)])).expect_err(value);
            assert_eq!(e.var, var);
            assert_eq!(e.value, value);
            assert!(e.to_string().starts_with(var), "{e}");
        }
    }
}
