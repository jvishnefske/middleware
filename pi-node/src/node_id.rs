//! The node id: the 8 bytes `sys/info` ends with, the gateway binds the
//! node's beacons to (E21, RG9) and a payload's `node_id` returns.
//!
//! A Linux host has no base MAC or chip unique id, so the id comes from a
//! stable per-installation source, in this order:
//!
//! 1. `MW_PI_NODE_NODE_ID`, 16 hex digits ([`parse_node_id`]), for a host
//!    that must keep an id across reinstalls or that has no machine id;
//! 2. the systemd machine id (`/etc/machine-id`, 32 lowercase hex digits,
//!    generated once at first boot and stable across reboots), hashed:
//!    the first 8 bytes of SHA-256 over [`DOMAIN`] followed by the 32
//!    digits ([`from_machine_id`]). Hashing keeps the machine id itself
//!    off the wire (systemd asks that it not be exposed), and the domain
//!    separates this use from any other derived from the same id.
//!
//! Without either the node does not start: an id that changed on every
//! start would rebind the gateway and orphan the cluster's view of it.
//! Everything here is pure; reading the file is the binary's job.

use middleware::SenderId;
use sha2::{Digest, Sha256};
use std::fmt;

/// Hashed in front of the machine id.
pub const DOMAIN: &[u8] = b"mw-pi-node/node-id/v1\0";

/// Why no node id could be had.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeIdError {
    /// The configured id is not 16 hex digits.
    Configured(String),
    /// The machine id is not 32 hex digits.
    MachineId(String),
    /// The id is all zeros, which reads as "no node" to people and tools.
    Zero,
}

impl fmt::Display for NodeIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NodeIdError::Configured(v) => write!(f, "node id {v:?} is not 16 hex digits"),
            NodeIdError::MachineId(v) => write!(f, "machine id {v:?} is not 32 hex digits"),
            NodeIdError::Zero => f.write_str("the node id is all zeros"),
        }
    }
}

impl std::error::Error for NodeIdError {}

fn hex_digits(s: &str, n: usize) -> Option<Vec<u8>> {
    if s.len() != n || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..n / 2)
        .map(|i| s.get(2 * i..2 * i + 2).and_then(|p| u8::from_str_radix(p, 16).ok()))
        .collect()
}

fn nonzero(id: [u8; 8]) -> Result<SenderId, NodeIdError> {
    if id == [0; 8] {
        return Err(NodeIdError::Zero);
    }
    Ok(SenderId(id))
}

/// Parses a configured id: 16 hex digits, either case, surrounding blanks
/// ignored, not all zeros.
pub fn parse_node_id(text: &str) -> Result<SenderId, NodeIdError> {
    let t = text.trim();
    let bytes = hex_digits(t, 16).ok_or_else(|| NodeIdError::Configured(t.to_owned()))?;
    let id: [u8; 8] = bytes.try_into().map_err(|_| NodeIdError::Configured(t.to_owned()))?;
    nonzero(id)
}

/// The id derived from a machine id file's contents (32 hex digits and a
/// newline, as systemd writes it; case is folded so the same id gives the
/// same node however it was written).
pub fn from_machine_id(contents: &str) -> Result<SenderId, NodeIdError> {
    let t = contents.trim();
    if hex_digits(t, 32).is_none() {
        return Err(NodeIdError::MachineId(t.to_owned()));
    }
    let digest = Sha256::new()
        .chain_update(DOMAIN)
        .chain_update(t.to_ascii_lowercase().as_bytes())
        .finalize();
    let mut id = [0u8; 8];
    id.copy_from_slice(digest.get(..8).unwrap_or(&[0; 8]));
    nonzero(id)
}

/// The node id: the configured one when set, else the one derived from
/// the machine id file's contents (`None` when it could not be read).
pub fn resolve(configured: Option<&str>, machine_id: Option<&str>) -> Result<SenderId, NodeIdError> {
    match (configured, machine_id) {
        (Some(c), _) => parse_node_id(c),
        (None, Some(m)) => from_machine_id(m),
        (None, None) => Err(NodeIdError::MachineId(String::new())),
    }
}

/// `id` as 16 lowercase hex digits (the form `MW_PI_NODE_NODE_ID` takes).
pub fn hex(id: SenderId) -> String {
    id.0.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MACHINE_ID: &str = "4f6a1c0e9b2d47e8a3c5d7f9e1b3a5c7\n";

    #[test]
    fn pn2_machine_id_hashes_to_a_stable_node_id() {
        let a = from_machine_id(MACHINE_ID).expect("id");
        // Stable: the same input, written in either case, gives the same id.
        assert_eq!(from_machine_id(&MACHINE_ID.to_ascii_uppercase()), Ok(a));
        assert_eq!(from_machine_id(MACHINE_ID.trim()), Ok(a));
        // The id is the first 8 bytes of SHA-256(DOMAIN || id), and not
        // simply the id's own leading bytes.
        let digest = Sha256::new()
            .chain_update(DOMAIN)
            .chain_update(MACHINE_ID.trim().as_bytes())
            .finalize();
        assert_eq!(&a.0[..], &digest[..8]);
        assert_ne!(a.0, [0x4f, 0x6a, 0x1c, 0x0e, 0x9b, 0x2d, 0x47, 0xe8]);
        // Another machine, another node.
        let b = from_machine_id("4f6a1c0e9b2d47e8a3c5d7f9e1b3a5c8").expect("id");
        assert_ne!(a, b);
        for bad in ["", "uninitialized", "4f6a1c0e9b2d47e8a3c5d7f9e1b3a5c", "4f6a1c0e9b2d47e8a3c5d7f9e1b3a5c7a", "zf6a1c0e9b2d47e8a3c5d7f9e1b3a5c7"] {
            assert!(matches!(from_machine_id(bad), Err(NodeIdError::MachineId(_))), "{bad:?}");
        }
    }

    #[test]
    fn pn2_a_configured_node_id_wins_and_is_checked() {
        let id = parse_node_id(" 0123456789ABCDEF\n").expect("id");
        assert_eq!(id, SenderId([0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]));
        assert_eq!(hex(id), "0123456789abcdef");
        assert_eq!(resolve(Some("0123456789abcdef"), Some(MACHINE_ID)), Ok(id));
        assert_eq!(resolve(None, Some(MACHINE_ID)), from_machine_id(MACHINE_ID));
        assert!(matches!(resolve(None, None), Err(NodeIdError::MachineId(_))));
        for bad in ["", "0123", "0123456789abcdeg", "0123456789abcdef00", "+123456789abcdef"] {
            assert!(matches!(parse_node_id(bad), Err(NodeIdError::Configured(_))), "{bad:?}");
        }
        assert_eq!(parse_node_id("0000000000000000"), Err(NodeIdError::Zero));
    }
}
