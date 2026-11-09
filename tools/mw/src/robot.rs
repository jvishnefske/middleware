//! Robot enrollment (pure): the lab's `robot-join` output and the files
//! a robot reads.
//!
//! `just helm::robot-join <name> <selector>` prints, among whatever
//! `spire-server entry create` reports:
//!
//! ```text
//! join token (single use, 1h): <token>
//! SPIRE server: any node address, port 30081; mesh for robots: port 30443
//! trust bundle (SPIFFE format, trust_bundle_format = "spiffe"):
//! {"keys": [...], ...}
//! ```
//!
//! [`parse_join`] reads the token (also from a bare `Token: <token>` line,
//! the `spire-server token generate` form), the two ports (the contract's
//! 30081 and 30443 when the line is missing) and the first JSON object
//! after the bundle header (or anywhere, without one), which must hold a
//! `keys` array. Other lines are ignored.
//!
//! [`files`] renders the enrollment the robot VM reads from
//! [`ROBOT_DIR`]: `join-token` (mode 0600), `bundle.spiffe` and
//! `enrollment.env` (`KEY=value` lines: name, SPIFFE ids, trust domain,
//! SPIRE server and mesh gateway address and port).

use crate::plan::Name;
use serde_json::Value;
use std::fmt;

/// Where the robot VM reads its enrollment.
pub const ROBOT_DIR: &str = "/var/lib/mw-robot";
/// The enrollment files, in the order [`files`] returns them.
pub const FILE_NAMES: [&str; 3] = ["join-token", "bundle.spiffe", "enrollment.env"];
/// The lab's trust domain.
pub const TRUST_DOMAIN: &str = "infra.local";
/// SPIRE server NodePort when the output does not say.
pub const DEFAULT_SPIRE_PORT: u16 = 30081;
/// Robot mTLS gateway NodePort when the output does not say.
pub const DEFAULT_MESH_PORT: u16 = 30443;

/// What `robot-join` handed out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Join {
    /// The single-use join token.
    pub token: String,
    /// SPIRE server port on any node.
    pub spire_port: u16,
    /// Robot mesh gateway port on any node.
    pub mesh_port: u16,
    /// The trust bundle, SPIFFE (JWKS) JSON as printed.
    pub bundle: String,
}

/// Why the output could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinError {
    /// No token line, or an empty or malformed token.
    NoToken,
    /// No JSON trust bundle with a `keys` array.
    NoBundle,
}

impl fmt::Display for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JoinError::NoToken => f.write_str("robot-join printed no join token"),
            JoinError::NoBundle => {
                f.write_str("robot-join printed no SPIFFE trust bundle (JSON with \"keys\")")
            }
        }
    }
}

impl std::error::Error for JoinError {}

fn token(out: &str) -> Result<String, JoinError> {
    let line = out.lines().map(str::trim).find(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("join token") || l.starts_with("token:")
    });
    let t = line
        .and_then(|l| l.rsplit(':').next())
        .map(str::trim)
        .ok_or(JoinError::NoToken)?;
    let ok =
        (1..=128).contains(&t.len()) && t.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-');
    if ok {
        Ok(t.to_owned())
    } else {
        Err(JoinError::NoToken)
    }
}

fn last_number(s: &str) -> Option<u16> {
    let end = s.rfind(|c: char| c.is_ascii_digit())? + 1;
    let start = s[..end]
        .rfind(|c: char| !c.is_ascii_digit())
        .map_or(0, |i| i + 1);
    s[start..end].parse().ok()
}

fn ports(out: &str) -> (u16, u16) {
    let mut spire = DEFAULT_SPIRE_PORT;
    let mut mesh = DEFAULT_MESH_PORT;
    for line in out.lines() {
        let l = line.to_ascii_lowercase();
        if !(l.contains("spire server") && l.contains("port")) {
            continue;
        }
        for seg in l.split(';') {
            let Some(n) = last_number(seg).filter(|_| seg.contains("port")) else {
                continue;
            };
            if seg.contains("mesh") || seg.contains("robots") {
                mesh = n;
            } else if seg.contains("spire") {
                spire = n;
            }
        }
    }
    (spire, mesh)
}

fn bundle(out: &str) -> Result<String, JoinError> {
    let after_header = out
        .lines()
        .position(|l| l.trim().to_ascii_lowercase().starts_with("trust bundle"))
        .map(|i| out.lines().skip(i + 1).collect::<Vec<_>>().join("\n"));
    let text = after_header.as_deref().unwrap_or(out);
    let start = text.find('{').ok_or(JoinError::NoBundle)?;
    let rest = &text[start..];
    let mut stream = serde_json::Deserializer::from_str(rest).into_iter::<Value>();
    let v = stream
        .next()
        .and_then(Result::ok)
        .ok_or(JoinError::NoBundle)?;
    let end = stream.byte_offset();
    if v.get("keys").and_then(Value::as_array).is_none() {
        return Err(JoinError::NoBundle);
    }
    Ok(rest[..end].trim().to_owned())
}

/// Parses `robot-join` output (module docs).
pub fn parse_join(out: &str) -> Result<Join, JoinError> {
    let token = token(out)?;
    let (spire_port, mesh_port) = ports(out);
    let bundle = bundle(out)?;
    Ok(Join {
        token,
        spire_port,
        mesh_port,
        bundle,
    })
}

/// One file to write: name, contents, Unix mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    /// File name inside the robot directory.
    pub name: &'static str,
    /// Contents.
    pub contents: String,
    /// Mode.
    pub mode: u32,
}

/// The enrollment files for robot `name`, whose SPIRE server and mesh
/// gateway are at `node_address` (module docs).
pub fn files(name: &Name, join: &Join, node_address: &str) -> [File; 3] {
    let [token, bundle, env] = FILE_NAMES;
    let id = format!("spiffe://{TRUST_DOMAIN}/robot/{name}");
    [
        File {
            name: token,
            contents: format!("{}\n", join.token),
            mode: 0o600,
        },
        File {
            name: bundle,
            contents: format!("{}\n", join.bundle),
            mode: 0o644,
        },
        File {
            name: env,
            contents: format!(
                "MW_ROBOT_NAME={name}\n\
                 MW_ROBOT_SPIFFE_ID={id}\n\
                 MW_ROBOT_AGENT_ID={id}/agent\n\
                 MW_TRUST_DOMAIN={TRUST_DOMAIN}\n\
                 MW_SPIRE_SERVER_ADDRESS={node_address}\n\
                 MW_SPIRE_SERVER_PORT={}\n\
                 MW_GATEWAY_ADDRESS={node_address}\n\
                 MW_GATEWAY_PORT={}\n",
                join.spire_port, join.mesh_port
            ),
            mode: 0o644,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUT: &str = include_str!("../tests/fixtures/robot-join.txt");

    #[test]
    fn m52_parses_the_lab_output_with_extra_lines() {
        let j = parse_join(OUT).unwrap();
        assert_eq!(j.token, "7c0b6e1a-4a8d-4f61-9b36-2f1d0c9e8a77");
        assert_eq!((j.spire_port, j.mesh_port), (30081, 30443));
        assert!(j.bundle.starts_with('{') && j.bundle.ends_with('}'));
        let v: Value = serde_json::from_str(&j.bundle).unwrap();
        assert_eq!(v["keys"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn m52_tolerates_other_ports_bare_tokens_and_no_header() {
        let out = "Token: abc-123\nSPIRE server: any node address, port 31081; mesh for robots: port 31443\nnoise\n{\"keys\":[]} trailing\n";
        let j = parse_join(out).unwrap();
        assert_eq!(j.token, "abc-123");
        assert_eq!((j.spire_port, j.mesh_port), (31081, 31443));
        assert_eq!(j.bundle, "{\"keys\":[]}");
        let no_ports = "join token (single use, 1h): t1\ntrust bundle:\n{\"keys\":[]}";
        let j = parse_join(no_ports).unwrap();
        assert_eq!((j.spire_port, j.mesh_port), (30081, 30443));
    }

    #[test]
    fn m52_rejects_missing_token_or_bundle() {
        assert_eq!(
            parse_join("join token (single use, 1h): \ntrust bundle:\n{\"keys\":[]}"),
            Err(JoinError::NoToken)
        );
        assert_eq!(parse_join("{\"keys\":[]}"), Err(JoinError::NoToken));
        assert_eq!(
            parse_join("join token (single use, 1h): t\ntrust bundle:\n"),
            Err(JoinError::NoBundle)
        );
        assert_eq!(
            parse_join("join token (single use, 1h): t\ntrust bundle:\n{\"x\":1}"),
            Err(JoinError::NoBundle)
        );
        assert_eq!(
            parse_join("join token (single use, 1h): t; rm\ntrust bundle:\n{\"keys\":[]}"),
            Err(JoinError::NoToken)
        );
    }

    #[test]
    fn m52_files_follow_the_robot_dir_contract() {
        let j = Join {
            token: "tok".into(),
            spire_port: 30081,
            mesh_port: 30443,
            bundle: "{\"keys\":[]}".into(),
        };
        let name = Name::parse("robot", "r1").unwrap();
        let [t, b, e] = files(&name, &j, "192.168.66.11");
        assert_eq!(
            (t.name, t.contents.as_str(), t.mode),
            ("join-token", "tok\n", 0o600)
        );
        assert_eq!(
            (b.name, b.contents.as_str()),
            ("bundle.spiffe", "{\"keys\":[]}\n")
        );
        assert_eq!(
            e.contents,
            "MW_ROBOT_NAME=r1\nMW_ROBOT_SPIFFE_ID=spiffe://infra.local/robot/r1\nMW_ROBOT_AGENT_ID=spiffe://infra.local/robot/r1/agent\nMW_TRUST_DOMAIN=infra.local\nMW_SPIRE_SERVER_ADDRESS=192.168.66.11\nMW_SPIRE_SERVER_PORT=30081\nMW_GATEWAY_ADDRESS=192.168.66.11\nMW_GATEWAY_PORT=30443\n"
        );
    }
}
