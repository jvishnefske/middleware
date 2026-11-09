//! Static endpoints for services that run outside the cluster
//! (`MW_STATIC_ENDPOINTS`): a robot has no cluster DNS to browse, so each
//! called kind is mapped to a fixed dial address (its local Envoy) and the
//! `:authority` that Envoy routes on.
//!
//! # Format
//!
//! `MW_STATIC_ENDPOINTS` is a `,`-separated list (surrounding whitespace
//! ignored, empty entries skipped) of
//!
//! ```text
//! <kind>=[http://]<dial host>:<dial port>@<authority host>:<authority port>
//! ```
//!
//! for example
//! `demo/echo=http://127.0.0.1:15001@demo-echo.apps.svc.cluster.local:50051`.
//! The TCP connection goes to the dial address; every request carries
//! `<authority host>:<authority port>` as `:authority` (h2c, no TLS: the
//! local Envoy originates mTLS with the robot's SVID). The dial host is any
//! host name or IP literal without whitespace, `/` or `@` (IPv6 in
//! brackets); the authority host is an RFC 1123 subdomain of at most
//! [`MAX_HOST_LEN`] bytes (the bound of a [`Directory`] host record). Both
//! ports are non-zero. A kind given twice is an error.
//!
//! # Use
//!
//! [`StaticEndpoint::client`] builds the [`DownstreamClient`] for one entry:
//! a [`MeshClient`] dialing through the address with a one-record
//! [`Directory`] (instance [`STATIC_INSTANCE`], no node, endpoint
//! [`Endpoint::Host`] of the authority, TTL `u32::MAX` seconds, never
//! expired since the client's directory is not refreshed). The runner uses
//! it in place of DNS-SD discovery for every called kind it maps
//! ([`Config::static_endpoint`](crate::Config::static_endpoint)); a kind
//! with a static endpoint needs no `MW_SERVICE_MAP` entry.
//!
//! Added in 0.6 phase 2 (agent E, design.md V5) for `mw-robot`.

use middleware::discovery::directory::MAX_HOST_LEN;
use middleware::discovery::{Directory, Endpoint, Event, Service, ServiceKind};
use middleware::grpc::istio::is_dns_label;
use middleware::grpc::mesh::{MeshClient, MeshDialer};
use middleware::key::Key;

use crate::config::ConfigError;
use crate::ctx::{DIRECTORY_SLOTS, DownstreamClient};

/// `MW_STATIC_ENDPOINTS`.
pub const ENV_STATIC_ENDPOINTS: &str = "MW_STATIC_ENDPOINTS";

/// The instance name of a static endpoint's directory record.
pub const STATIC_INSTANCE: &str = "static";

/// One `MW_STATIC_ENDPOINTS` entry (module docs), validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticEndpoint {
    kind: String,
    dial: String,
    host: String,
    port: u16,
}

impl StaticEndpoint {
    /// The called kind.
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// Where the TCP connection goes (`host:port`).
    pub fn dial(&self) -> &str {
        &self.dial
    }

    /// The `:authority` every request carries (`host:port`).
    pub fn authority(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// The dialer: every channel through [`StaticEndpoint::dial`].
    pub fn dialer(&self) -> MeshDialer {
        MeshDialer::via_sidecar(self.dial.clone())
    }

    /// The one-record directory routing the kind to the authority. Pure.
    pub fn directory(&self) -> Directory<DIRECTORY_SLOTS> {
        let Ok(key) = Key::new(&self.kind) else {
            return Directory::new();
        };
        let service = Service {
            kind: ServiceKind::new(key),
            instance: STATIC_INSTANCE,
            node: None,
            endpoint: Endpoint::Host {
                name: &self.host,
                port: self.port,
            },
            txt: &[],
            ttl_s: u32::MAX,
        };
        Directory::new().observe(&Event::Up(service), 0).0
    }

    /// The downstream client for this entry (module docs). No I/O: the
    /// channel connects on the first request.
    pub fn client(&self) -> DownstreamClient {
        let kind = Key::new(&self.kind).map(ServiceKind::new);
        match kind {
            Ok(kind) => MeshClient::new(kind, self.dialer()).with_directory(self.directory()),
            // Unreachable: `parse` validated the kind.
            Err(_) => MeshClient::new(ServiceKind::from_static("invalid"), self.dialer()),
        }
    }
}

/// Every static endpoint of a deployment, in `MW_STATIC_ENDPOINTS` order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StaticEndpoints(Vec<StaticEndpoint>);

impl StaticEndpoints {
    /// None.
    pub fn new() -> Self {
        Self::default()
    }

    /// Parses an `MW_STATIC_ENDPOINTS` value (module docs). Pure.
    pub fn parse(value: &str) -> Result<Self, ConfigError> {
        let mut out: Vec<StaticEndpoint> = Vec::new();
        for entry in value.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            let e = parse_entry(entry)?;
            if out.iter().any(|o| o.kind == e.kind) {
                return Err(ConfigError::StaticEndpoint {
                    entry: entry.to_owned(),
                    reason: "kind given twice",
                });
            }
            out.push(e);
        }
        Ok(StaticEndpoints(out))
    }

    /// The entry for `kind`.
    pub fn get(&self, kind: &str) -> Option<&StaticEndpoint> {
        self.0.iter().find(|e| e.kind == kind)
    }

    /// The entries, in order.
    pub fn iter(&self) -> impl Iterator<Item = &StaticEndpoint> {
        self.0.iter()
    }

    /// True with no entry.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

fn bad(entry: &str, reason: &'static str) -> ConfigError {
    ConfigError::StaticEndpoint {
        entry: entry.to_owned(),
        reason,
    }
}

/// `host:port` with a non-zero port; the host is returned as written.
fn host_port(s: &str) -> Option<(&str, u16)> {
    let (host, port) = s.rsplit_once(':')?;
    let port = port.parse::<u16>().ok().filter(|p| *p != 0)?;
    (!host.is_empty()).then_some((host, port))
}

fn parse_entry(entry: &str) -> Result<StaticEndpoint, ConfigError> {
    let (kind, target) = entry
        .split_once('=')
        .ok_or_else(|| bad(entry, "not kind=dial@authority"))?;
    let kind = kind.trim();
    Key::new(kind).map_err(|_| bad(entry, "the kind is not a key"))?;
    let (dial, authority) = target
        .trim()
        .split_once('@')
        .ok_or_else(|| bad(entry, "not kind=dial@authority"))?;
    let dial = dial.strip_prefix("http://").unwrap_or(dial);
    let dial_ok = !dial.contains(|c: char| c.is_whitespace() || c == '/' || c == '@')
        && host_port(dial).is_some();
    if !dial_ok {
        return Err(bad(entry, "the dial address is not host:port"));
    }
    let (host, port) =
        host_port(authority).ok_or_else(|| bad(entry, "the authority is not host:port"))?;
    if host.len() > MAX_HOST_LEN || !host.split('.').all(is_dns_label) {
        return Err(bad(
            entry,
            "the authority host is not an RFC 1123 name of at most 64 bytes",
        ));
    }
    Ok(StaticEndpoint {
        kind: kind.to_owned(),
        dial: dial.to_owned(),
        host: host.to_owned(),
        port,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::grpc::mesh::targets;
    use middleware::transport::Target;

    const ECHO: &str = "demo/echo=http://127.0.0.1:15001@demo-echo.apps.svc.cluster.local:50051";

    #[test]
    fn v5_static_endpoints_parse_dial_and_authority() {
        let s = StaticEndpoints::parse(&format!(" {ECHO} , ,cam/jpeg=localhost:9@cam.robots:80"))
            .unwrap();
        let echo = s.get("demo/echo").unwrap();
        assert_eq!(echo.kind(), "demo/echo");
        assert_eq!(echo.dial(), "127.0.0.1:15001");
        assert_eq!(echo.authority(), "demo-echo.apps.svc.cluster.local:50051");
        assert_eq!(echo.dialer(), MeshDialer::via_sidecar("127.0.0.1:15001"));
        assert_eq!(
            echo.dialer().builder(&echo.authority()).authority(),
            "demo-echo.apps.svc.cluster.local:50051"
        );
        assert_eq!(s.get("cam/jpeg").unwrap().dial(), "localhost:9");
        assert_eq!(s.iter().count(), 2);
        assert!(s.get("other/kind").is_none());
        assert!(StaticEndpoints::parse("").unwrap().is_empty());
        // IPv6 dial addresses in brackets.
        let v6 = StaticEndpoints::parse("a/b=[::1]:15001@a.b:1").unwrap();
        assert_eq!(v6.get("a/b").unwrap().dial(), "[::1]:15001");
    }

    #[test]
    fn v5_static_endpoint_errors_name_the_entry() {
        for (value, reason) in [
            ("demo/echo", "not kind=dial@authority"),
            ("demo/echo=127.0.0.1:1", "not kind=dial@authority"),
            ("Bad Kind=127.0.0.1:1@a.b:1", "the kind is not a key"),
            ("a/b=127.0.0.1@a.b:1", "the dial address is not host:port"),
            ("a/b=127.0.0.1:0@a.b:1", "the dial address is not host:port"),
            ("a/b=https://x:1@a.b:1", "the dial address is not host:port"),
            ("a/b=127.0.0.1:1@a.b", "the authority is not host:port"),
            (
                "a/b=127.0.0.1:1@Not_Dns:1",
                "the authority host is not an RFC 1123 name of at most 64 bytes",
            ),
        ] {
            assert_eq!(
                StaticEndpoints::parse(value),
                Err(ConfigError::StaticEndpoint {
                    entry: value.to_owned(),
                    reason
                }),
                "{value}"
            );
        }
        let long = format!("a/b=x:1@{}.example:1", "a".repeat(60));
        assert!(StaticEndpoints::parse(&long).is_err());
        let twice = format!("{ECHO},demo/echo=x:1@a.b:2");
        assert!(matches!(
            StaticEndpoints::parse(&twice),
            Err(ConfigError::StaticEndpoint {
                reason: "kind given twice",
                ..
            })
        ));
    }

    #[test]
    fn v5_static_client_routes_the_kind_to_the_authority() {
        let s = StaticEndpoints::parse(ECHO).unwrap();
        let echo = s.get("demo/echo").unwrap();
        let dir = echo.directory();
        let kind = ServiceKind::from_static("demo/echo");
        assert_eq!(
            targets(&dir, kind, Target::Any),
            [(None, "demo-echo.apps.svc.cluster.local:50051".to_owned())]
        );
        // Never expires.
        assert_eq!(dir.clone().expire(10 * 365 * 24 * 3_600_000).1, 0);
        let client = echo.client();
        assert_eq!(client.directory(), &dir);
    }
}
