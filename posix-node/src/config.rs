//! [`PosixNodeConfig`]: the node's own settings, parsed purely from its
//! environment by [`PosixNodeConfig::from_env`].
//!
//! | variable | default | format |
//! |----------|---------|--------|
//! | `MW_POSIX_NODE_STATE` | `/var/lib/mw-posix-node` | directory holding the payload store and the trust record (created if missing, locked while the node runs) |
//! | `MW_POSIX_NODE_NODE_ID` | from the machine id | 16 hex digits ([`crate::node_id`]) |
//! | `MW_POSIX_NODE_MACHINE_ID` | `/etc/machine-id` | path of the systemd machine id file the node id is derived from |
//! | `MW_POSIX_NODE_MEMORY_BYTES` | `1048576` (1 MiB) | a payload's largest linear memory, decimal bytes, 65536..=67108864 |
//! | `MW_POSIX_NODE_LOCAL_SERIAL` | none | path of a serial port (a USB gadget's `/dev/ttyGS0`, a PTY) served beside the gateway at 115200 baud, for `mwctl` |
//!
//! The in-process gateway reads its own `MW_GATEWAY_*` variables
//! (`mw_gateway::GatewayConfig`; `MW_GATEWAY_SERIAL` and `MW_GATEWAY_BAUD`
//! are not used, the device is in the process) and the runner its `MW_*`
//! variables (`mw_service::Config`; `MW_ZENOH_ROUTER` is required). One
//! default differs: when `MW_GATEWAY_STATE` is unset the gateway keeps its
//! control counter in `<MW_POSIX_NODE_STATE>/gateway`
//! ([`gateway_state_dir`]), so one directory holds all of the node's
//! state.
//!
//! Unset or empty is the default; a malformed value is a
//! [`PosixNodeConfigError`] naming the variable.
//!
//! # The old names
//!
//! The crate was `mw-pi-node` until it hosted more than Raspberry Pis.
//! Each `MW_POSIX_NODE_*` variable is still read under its old name,
//! `MW_PI_NODE_*`, when the new one is unset; [`PosixNodeConfig::deprecated`]
//! lists the old names that were used, and the binary logs a warning for
//! each. Likewise, when the state directory is the default and
//! `/var/lib/mw-posix-node` does not exist but `/var/lib/mw-pi-node` does,
//! the old directory is used (and reported), so a node keeps its payload,
//! trust record and control counter across the rename. The node id's hash
//! domain did not change ([`crate::node_id::DOMAIN`]).

use std::fmt;
use std::path::{Path, PathBuf};

/// `MW_POSIX_NODE_STATE`.
pub const ENV_STATE: &str = "MW_POSIX_NODE_STATE";
/// `MW_POSIX_NODE_NODE_ID`.
pub const ENV_NODE_ID: &str = "MW_POSIX_NODE_NODE_ID";
/// `MW_POSIX_NODE_MACHINE_ID`.
pub const ENV_MACHINE_ID: &str = "MW_POSIX_NODE_MACHINE_ID";
/// `MW_POSIX_NODE_MEMORY_BYTES`.
pub const ENV_MEMORY_BYTES: &str = "MW_POSIX_NODE_MEMORY_BYTES";
/// `MW_POSIX_NODE_LOCAL_SERIAL`.
pub const ENV_LOCAL_SERIAL: &str = "MW_POSIX_NODE_LOCAL_SERIAL";

/// Default state directory.
pub const DEFAULT_STATE: &str = "/var/lib/mw-posix-node";
/// The default state directory before the rename, still used when it
/// exists and [`DEFAULT_STATE`] does not.
pub const LEGACY_STATE: &str = "/var/lib/mw-pi-node";
/// The variables' prefix before the rename.
pub const LEGACY_PREFIX: &str = "MW_PI_NODE_";
/// Their prefix now.
pub const PREFIX: &str = "MW_POSIX_NODE_";

/// A variable's name before the rename: `MW_POSIX_NODE_X` is `MW_PI_NODE_X`.
pub fn legacy_name(name: &str) -> String {
    match name.strip_prefix(PREFIX) {
        Some(rest) => format!("{LEGACY_PREFIX}{rest}"),
        None => name.to_owned(),
    }
}
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
pub struct PosixNodeConfigError {
    /// The variable.
    pub var: &'static str,
    /// Its value.
    pub value: String,
    /// What is wrong with it.
    pub reason: &'static str,
}

impl fmt::Display for PosixNodeConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}={:?}: {}", self.var, self.value, self.reason)
    }
}

impl std::error::Error for PosixNodeConfigError {}

/// Where the node id comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeIdSource {
    /// `MW_POSIX_NODE_NODE_ID`, already checked.
    Configured(middleware::SenderId),
    /// Derived from the machine id file at this path.
    MachineId(PathBuf),
}

/// The node's settings (module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PosixNodeConfig {
    /// The state directory.
    pub state: PathBuf,
    /// Where the node id comes from.
    pub node_id: NodeIdSource,
    /// A payload's largest linear memory in bytes.
    pub memory_bytes: u32,
    /// A local serial port served beside the gateway.
    pub local_serial: Option<String>,
    /// Old names that were used: `MW_PI_NODE_*` variables, and
    /// [`LEGACY_STATE`] when it was chosen. The binary warns about each.
    pub deprecated: Vec<String>,
}

impl Default for PosixNodeConfig {
    fn default() -> Self {
        PosixNodeConfig {
            state: PathBuf::from(DEFAULT_STATE),
            node_id: NodeIdSource::MachineId(PathBuf::from(DEFAULT_MACHINE_ID)),
            memory_bytes: DEFAULT_MEMORY_BYTES,
            local_serial: None,
            deprecated: Vec::new(),
        }
    }
}

impl PosixNodeConfig {
    /// Parses the variables of the module docs through `var`. Pure.
    pub fn from_env(var: &impl Fn(&str) -> Option<String>) -> Result<Self, PosixNodeConfigError> {
        let used = std::cell::RefCell::new(Vec::new());
        let set = |name: &str| var(name).filter(|v| !v.trim().is_empty());
        let get = |name: &'static str| {
            set(name).or_else(|| {
                let old = legacy_name(name);
                let v = set(&old)?;
                used.borrow_mut().push(old);
                Some(v)
            })
        };
        let err = |var: &'static str, value: &str, reason: &'static str| PosixNodeConfigError {
            var,
            value: value.to_owned(),
            reason,
        };
        let d = PosixNodeConfig::default();
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
        Ok(PosixNodeConfig {
            state,
            node_id,
            memory_bytes,
            local_serial,
            deprecated: used.into_inner(),
        })
    }

    /// The default state directory, unless only the old one exists
    /// (`exists` asks the file system). A configured directory is kept.
    /// Pure.
    pub fn with_legacy_state(mut self, exists: impl Fn(&Path) -> bool) -> Self {
        let old = Path::new(LEGACY_STATE);
        if self.state == Path::new(DEFAULT_STATE) && !exists(&self.state) && exists(old) {
            self.state = old.to_path_buf();
            self.deprecated.push(LEGACY_STATE.to_owned());
        }
        self
    }

    /// [`PosixNodeConfig::from_env`] over the process environment, then
    /// [`PosixNodeConfig::with_legacy_state`] over the file system.
    pub fn from_process_env() -> Result<Self, PosixNodeConfigError> {
        Ok(Self::from_env(&|k| std::env::var(k).ok())?.with_legacy_state(|p| p.exists()))
    }
}

/// The subdirectory of the node's state the gateway's state defaults to.
pub const GATEWAY_STATE_SUBDIR: &str = "gateway";

/// Where the in-process gateway keeps its state: `MW_GATEWAY_STATE` when
/// set (`configured`, the raw variable; the gateway checks it is
/// absolute), else `<node state>/gateway`. Pure.
pub fn gateway_state_dir(configured: Option<&str>, node_state: &Path) -> PathBuf {
    match configured.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) => PathBuf::from(v),
        None => node_state.join(GATEWAY_STATE_SUBDIR),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::SenderId;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn pn1_defaults() {
        let c = PosixNodeConfig::from_env(&env(&[])).expect("defaults");
        assert_eq!(c, PosixNodeConfig::default());
        assert_eq!(c.state, PathBuf::from("/var/lib/mw-posix-node"));
        assert_eq!(
            c.node_id,
            NodeIdSource::MachineId(PathBuf::from("/etc/machine-id"))
        );
        assert_eq!(c.memory_bytes, 1024 * 1024);
        assert_eq!(c.local_serial, None);
        // Empty is unset.
        let c = PosixNodeConfig::from_env(&env(&[(ENV_STATE, ""), (ENV_MEMORY_BYTES, " ")]))
            .expect("empty");
        assert_eq!(c, PosixNodeConfig::default());
    }

    #[test]
    fn pn1_parses_every_variable() {
        let c = PosixNodeConfig::from_env(&env(&[
            (ENV_STATE, "/srv/node"),
            (ENV_MACHINE_ID, "/run/machine-id"),
            (ENV_MEMORY_BYTES, "65536"),
            (ENV_LOCAL_SERIAL, "/dev/ttyGS0"),
        ]))
        .expect("config");
        assert_eq!(c.state, PathBuf::from("/srv/node"));
        assert_eq!(
            c.node_id,
            NodeIdSource::MachineId(PathBuf::from("/run/machine-id"))
        );
        assert_eq!(c.memory_bytes, 65536);
        assert_eq!(c.local_serial.as_deref(), Some("/dev/ttyGS0"));
        // A configured node id wins over the machine id file.
        let c = PosixNodeConfig::from_env(&env(&[
            (ENV_NODE_ID, "00000000000000ff"),
            (ENV_MACHINE_ID, "/run/machine-id"),
            (ENV_MEMORY_BYTES, "67108864"),
        ]))
        .expect("config");
        assert_eq!(
            c.node_id,
            NodeIdSource::Configured(SenderId([0, 0, 0, 0, 0, 0, 0, 0xff]))
        );
        assert_eq!(c.memory_bytes, 64 * 1024 * 1024);
    }

    #[test]
    fn pn1_the_gateway_state_defaults_under_the_node_state() {
        let node = Path::new("/var/lib/mw-posix-node");
        assert_eq!(
            gateway_state_dir(None, node),
            PathBuf::from("/var/lib/mw-posix-node/gateway")
        );
        assert_eq!(
            gateway_state_dir(Some(" "), node),
            PathBuf::from("/var/lib/mw-posix-node/gateway")
        );
        assert_eq!(
            gateway_state_dir(Some("/srv/gw"), node),
            PathBuf::from("/srv/gw")
        );
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
            let e = PosixNodeConfig::from_env(&env(&[(var, value)])).expect_err(value);
            assert_eq!(e.var, var);
            assert_eq!(e.value, value);
            assert!(e.to_string().starts_with(var), "{e}");
        }
    }

    #[test]
    fn pn1_the_old_names_still_work_and_are_reported() {
        let c = PosixNodeConfig::from_env(&env(&[
            ("MW_PI_NODE_STATE", "/srv/old"),
            ("MW_PI_NODE_MEMORY_BYTES", "131072"),
            (ENV_LOCAL_SERIAL, "/dev/ttyGS0"),
            ("MW_PI_NODE_LOCAL_SERIAL", "/dev/ignored"),
        ]))
        .expect("old names");
        assert_eq!(c.state, PathBuf::from("/srv/old"));
        assert_eq!(c.memory_bytes, 131072);
        // The new name wins, and only old names actually used are reported.
        assert_eq!(c.local_serial.as_deref(), Some("/dev/ttyGS0"));
        assert_eq!(
            c.deprecated,
            ["MW_PI_NODE_STATE", "MW_PI_NODE_MEMORY_BYTES"]
        );
        assert!(
            PosixNodeConfig::from_env(&env(&[]))
                .unwrap()
                .deprecated
                .is_empty()
        );
        // An old name's bad value is still refused.
        assert!(PosixNodeConfig::from_env(&env(&[("MW_PI_NODE_MEMORY_BYTES", "7")])).is_err());
        assert_eq!(legacy_name(ENV_NODE_ID), "MW_PI_NODE_NODE_ID");
    }

    #[test]
    fn pn1_an_existing_old_state_directory_is_kept() {
        let d = PosixNodeConfig::default();
        let only_old = |p: &Path| p == Path::new(LEGACY_STATE);
        let c = d.clone().with_legacy_state(only_old);
        assert_eq!(c.state, PathBuf::from(LEGACY_STATE));
        assert_eq!(c.deprecated, [LEGACY_STATE]);
        let both = |_: &Path| true;
        assert_eq!(
            d.clone().with_legacy_state(both).state,
            PathBuf::from(DEFAULT_STATE)
        );
        let neither = |_: &Path| false;
        assert_eq!(
            d.clone().with_legacy_state(neither).state,
            PathBuf::from(DEFAULT_STATE)
        );
        let configured = PosixNodeConfig {
            state: PathBuf::from("/srv/x"),
            ..d
        };
        assert_eq!(
            configured.with_legacy_state(only_old).state,
            PathBuf::from("/srv/x")
        );
    }
}
