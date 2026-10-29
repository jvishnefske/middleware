//! [`ServiceMap`]: which Kubernetes Service, namespace and port serve each
//! [`ServiceKind`], and the SRV name that finds them. Pure, fixed capacity.
//!
//! Kubernetes publishes, for every named port of a Service (RFC 2782 form,
//! Kubernetes DNS specification §2.4):
//!
//! ```text
//! _<port-name>._<protocol>.<service>.<namespace>.svc.<cluster-domain>
//! ```
//!
//! For a headless Service (`clusterIP: None`) it answers one SRV record per
//! ready pod, each targeting the pod's own name; for a normal Service, one
//! SRV targeting the Service. [`ServiceMap::srv_name`] builds that name with
//! protocol `_tcp` (every middleware kind is a `_tcp` DNS-SD type, see
//! [`record`](crate::discovery::record)) in the map's cluster domain,
//! [`DEFAULT_CLUSTER_DOMAIN`] unless set.
//!
//! # Validation
//!
//! * service and namespace: RFC 1123 labels, as Kubernetes requires — 1..=63
//!   lowercase ASCII letters, digits and `-`, starting and ending with a
//!   letter or digit ([`MapError::InvalidLabel`]);
//! * port name: an IANA service name, as Kubernetes requires — 1..=15
//!   lowercase letters, digits and `-`, at least one letter, no leading,
//!   trailing or doubled `-` ([`MapError::InvalidPortName`]);
//!   [`ServiceMap::with_kind_port`] uses the kind's DNS-SD service name
//!   (`cam/jpeg` → `cam-jpeg`), which always qualifies;
//! * cluster domain: labels of 1..=63 ASCII letters, digits and `-`, a
//!   trailing dot dropped, at most [`MAX_CLUSTER_DOMAIN_LEN`] bytes so that
//!   every SRV name fits 253 ([`MapError::InvalidDomain`]);
//! * kind: at most [`MAX_KIND_LEN`] bytes ([`MapError::KindTooLong`]).
//!
//! Mapping a kind again replaces its entry; a new kind when all `M` slots
//! are taken is [`MapError::Full`].

use heapless::{String, Vec};

use crate::discovery::ServiceKind;
use crate::discovery::directory::MAX_KIND_LEN;
use crate::discovery::record::{MAX_SERVICE_NAME_LEN, service_name};
use crate::key::{AsKey, KeyBuf};

/// The cluster domain Kubernetes uses unless configured otherwise.
pub const DEFAULT_CLUSTER_DOMAIN: &str = "cluster.local";
/// Longest RFC 1123 label (service, namespace).
pub const MAX_LABEL_LEN: usize = 63;
/// Longest cluster domain: 253 minus the longest prefix
/// `_<15>._tcp.<63>.<63>.svc.`.
pub const MAX_CLUSTER_DOMAIN_LEN: usize =
    MAX_SRV_NAME_LEN - (1 + MAX_SERVICE_NAME_LEN + 6 + MAX_LABEL_LEN + 1 + MAX_LABEL_LEN + 5);
/// Longest SRV query name, as dotted text.
pub const MAX_SRV_NAME_LEN: usize = 253;
/// Kinds a [`ServiceMap`] holds by default.
pub const DEFAULT_MAP_CAPACITY: usize = 8;

/// Why a mapping or name could not be made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MapError {
    /// A service or namespace is not an RFC 1123 label.
    InvalidLabel,
    /// A port name is not an IANA service name.
    InvalidPortName,
    /// The cluster domain is malformed or too long.
    InvalidDomain,
    /// The kind is longer than [`MAX_KIND_LEN`] bytes.
    KindTooLong,
    /// The kind has no DNS-SD service name (for
    /// [`ServiceMap::with_kind_port`]).
    Unmappable,
    /// All slots are taken.
    Full,
    /// The kind is not in the map.
    Unmapped,
    /// The output buffer is too small.
    BufferTooSmall,
}

/// Where a kind is served: a Kubernetes Service, its namespace and the
/// name of its port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceRef<'a> {
    /// The Service name (`metadata.name`).
    pub service: &'a str,
    /// The namespace.
    pub namespace: &'a str,
    /// The port's `name` in the Service spec.
    pub port_name: &'a str,
}

fn is_rfc1123_label(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=MAX_LABEL_LEN).contains(&b.len())
        && b.iter()
            .all(|&c| matches!(c, b'a'..=b'z' | b'0'..=b'9' | b'-'))
        && b.first().is_some_and(u8::is_ascii_alphanumeric)
        && b.last().is_some_and(u8::is_ascii_alphanumeric)
}

fn is_iana_service_name(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=MAX_SERVICE_NAME_LEN).contains(&b.len())
        && b.iter()
            .all(|&c| matches!(c, b'a'..=b'z' | b'0'..=b'9' | b'-'))
        && b.iter().any(u8::is_ascii_lowercase)
        && b.first() != Some(&b'-')
        && b.last() != Some(&b'-')
        && !s.contains("--")
}

fn is_domain(s: &str) -> bool {
    s.len() <= MAX_CLUSTER_DOMAIN_LEN
        && s.split('.').all(|l| {
            (1..=MAX_LABEL_LEN).contains(&l.len())
                && l.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
        })
}

/// Writes `parts` one after another into `out` and returns them as text.
fn concat<'o>(out: &'o mut [u8], parts: &[&str]) -> Result<&'o str, MapError> {
    let mut at = 0;
    for part in parts {
        let end = at + part.len();
        out.get_mut(at..end)
            .ok_or(MapError::BufferTooSmall)?
            .copy_from_slice(part.as_bytes());
        at = end;
    }
    let out: &'o [u8] = out;
    core::str::from_utf8(out.get(..at).ok_or(MapError::BufferTooSmall)?)
        .map_err(|_| MapError::BufferTooSmall)
}

/// The SRV name of `target` in `domain`:
/// `_<port>._tcp.<service>.<namespace>.svc.<domain>`, written into `out`
/// (at most [`MAX_SRV_NAME_LEN`] bytes). The parts are validated as in the
/// module docs.
pub fn srv_name<'o>(
    target: ServiceRef<'_>,
    domain: &str,
    out: &'o mut [u8],
) -> Result<&'o str, MapError> {
    let target = checked(target)?;
    let domain = domain.strip_suffix('.').unwrap_or(domain);
    if !is_domain(domain) {
        return Err(MapError::InvalidDomain);
    }
    concat(
        out,
        &[
            "_",
            target.port_name,
            "._tcp.",
            target.service,
            ".",
            target.namespace,
            ".svc.",
            domain,
        ],
    )
}

fn checked(target: ServiceRef<'_>) -> Result<ServiceRef<'_>, MapError> {
    if !is_rfc1123_label(target.service) || !is_rfc1123_label(target.namespace) {
        return Err(MapError::InvalidLabel);
    }
    if !is_iana_service_name(target.port_name) {
        return Err(MapError::InvalidPortName);
    }
    Ok(target)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    kind: KeyBuf<MAX_KIND_LEN>,
    service: String<MAX_LABEL_LEN>,
    namespace: String<MAX_LABEL_LEN>,
    port_name: String<MAX_SERVICE_NAME_LEN>,
}

impl Entry {
    fn target(&self) -> ServiceRef<'_> {
        ServiceRef {
            service: &self.service,
            namespace: &self.namespace,
            port_name: &self.port_name,
        }
    }

    fn kind(&self) -> ServiceKind<'_> {
        ServiceKind::new(self.kind.as_key())
    }
}

/// Up to `M` kinds and the Kubernetes Service serving each; see the module
/// docs. Built by value: each `with*` call returns the new map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceMap<const M: usize = DEFAULT_MAP_CAPACITY> {
    domain: String<MAX_CLUSTER_DOMAIN_LEN>,
    entries: Vec<Entry, M>,
}

impl<const M: usize> Default for ServiceMap<M> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const M: usize> ServiceMap<M> {
    /// Most kinds held.
    pub const CAPACITY: usize = M;

    /// An empty map in [`DEFAULT_CLUSTER_DOMAIN`].
    pub fn new() -> Self {
        let mut domain = String::new();
        // Fits: 13 bytes.
        let _ = domain.push_str(DEFAULT_CLUSTER_DOMAIN);
        ServiceMap {
            domain,
            entries: Vec::new(),
        }
    }

    /// The map in cluster domain `domain` (e.g. from
    /// [`ResolvConf::cluster_domain`](super::ResolvConf::cluster_domain)).
    pub fn cluster_domain(self, domain: &str) -> Result<Self, MapError> {
        let domain = domain.strip_suffix('.').unwrap_or(domain);
        if !is_domain(domain) {
            return Err(MapError::InvalidDomain);
        }
        let domain = String::try_from(domain).map_err(|_| MapError::InvalidDomain)?;
        Ok(ServiceMap { domain, ..self })
    }

    /// The map with `kind` served by `target`, replacing an earlier entry
    /// for `kind`.
    pub fn with(self, kind: ServiceKind<'_>, target: ServiceRef<'_>) -> Result<Self, MapError> {
        let target = checked(target)?;
        let label = |s: &str| String::try_from(s).map_err(|_| MapError::InvalidLabel);
        let entry = Entry {
            kind: KeyBuf::from_key(kind.key()).map_err(|_| MapError::KindTooLong)?,
            service: label(target.service)?,
            namespace: label(target.namespace)?,
            port_name: String::try_from(target.port_name).map_err(|_| MapError::InvalidPortName)?,
        };
        let mut entries = self.entries;
        match entries.iter().position(|e| e.kind == entry.kind) {
            Some(i) => {
                if let Some(slot) = entries.get_mut(i) {
                    *slot = entry;
                }
            }
            None => entries.push(entry).map_err(|_| MapError::Full)?,
        }
        Ok(ServiceMap { entries, ..self })
    }

    /// The map with `kind` served by `service` in `namespace` on the port
    /// named after the kind's DNS-SD service name (`cam/jpeg` →
    /// `cam-jpeg`).
    pub fn with_kind_port(
        self,
        kind: ServiceKind<'_>,
        service: &str,
        namespace: &str,
    ) -> Result<Self, MapError> {
        let mut port = [0u8; MAX_SERVICE_NAME_LEN];
        let port_name = service_name(kind, &mut port).map_err(|_| MapError::Unmappable)?;
        self.with(
            kind,
            ServiceRef {
                service,
                namespace,
                port_name,
            },
        )
    }

    /// The cluster domain.
    pub fn domain(&self) -> &str {
        &self.domain
    }

    /// Where `kind` is served, if mapped.
    pub fn get(&self, kind: ServiceKind<'_>) -> Option<ServiceRef<'_>> {
        self.entries
            .iter()
            .find(|e| e.kind == kind.key())
            .map(Entry::target)
    }

    /// The mapped kinds, in insertion order.
    pub fn kinds(&self) -> impl Iterator<Item = ServiceKind<'_>> {
        self.entries.iter().map(Entry::kind)
    }

    /// Kinds held.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no kind is mapped.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The SRV name that finds `kind`'s endpoints ([`srv_name`] in this
    /// map's domain), written into `out`; [`MapError::Unmapped`] if the
    /// kind is not in the map.
    pub fn srv_name<'o>(
        &self,
        kind: ServiceKind<'_>,
        out: &'o mut [u8],
    ) -> Result<&'o str, MapError> {
        let target = self.get(kind).ok_or(MapError::Unmapped)?;
        srv_name(target, &self.domain, out)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec as StdVec;

    const CAM: ServiceKind<'static> = ServiceKind::from_static("cam/jpeg");
    const BUS: ServiceKind<'static> = ServiceKind::from_static("mw/bus");

    fn web(port_name: &'static str) -> ServiceRef<'static> {
        ServiceRef {
            service: "cam",
            namespace: "media",
            port_name,
        }
    }

    #[test]
    fn srv_names_follow_the_kubernetes_dns_schema() {
        let mut out = [0u8; MAX_SRV_NAME_LEN];
        let map = ServiceMap::<2>::new()
            .with(CAM, web("http"))
            .unwrap()
            .with_kind_port(BUS, "bus", "infra")
            .unwrap();
        assert_eq!(map.domain(), "cluster.local");
        assert_eq!(
            map.srv_name(CAM, &mut out),
            Ok("_http._tcp.cam.media.svc.cluster.local")
        );
        assert_eq!(
            map.srv_name(BUS, &mut out),
            Ok("_mw-bus._tcp.bus.infra.svc.cluster.local")
        );
        let map = map.cluster_domain("k8s.example.").unwrap();
        assert_eq!(
            map.srv_name(CAM, &mut out),
            Ok("_http._tcp.cam.media.svc.k8s.example")
        );
        assert_eq!(map.kinds().collect::<StdVec<_>>(), [CAM, BUS]);
        assert_eq!(map.get(BUS).map(|t| t.port_name), Some("mw-bus"));
        // Mapping again replaces; a third kind does not fit.
        let map = map.with(CAM, web("grpc")).unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(map.get(CAM), Some(web("grpc")));
        assert_eq!(
            map.clone()
                .with(ServiceKind::from_static("x/y"), web("http"))
                .unwrap_err(),
            MapError::Full
        );
        assert_eq!(
            map.srv_name(ServiceKind::from_static("x/y"), &mut out),
            Err(MapError::Unmapped)
        );
        assert_eq!(
            map.srv_name(CAM, &mut out[..20]),
            Err(MapError::BufferTooSmall)
        );
        // The longest parts still fit 253 bytes.
        let l63 = "a".repeat(63);
        let longest = ServiceRef {
            service: &l63,
            namespace: &l63,
            port_name: "abcdefghijklmno",
        };
        let half = "d".repeat((MAX_CLUSTER_DOMAIN_LEN - 1) / 2);
        let domain = std::format!("{half}.{half}");
        assert_eq!(domain.len(), MAX_CLUSTER_DOMAIN_LEN);
        assert_eq!(
            srv_name(longest, &domain, &mut out).map(str::len),
            Ok(MAX_SRV_NAME_LEN)
        );
    }

    #[test]
    fn invalid_parts_are_refused() {
        let map = ServiceMap::<1>::new();
        for port in [
            "",
            "HTTP",
            "-http",
            "http-",
            "h--p",
            "1234",
            "abcdefghijklmnop",
            "a_b",
        ] {
            assert_eq!(
                map.clone().with(CAM, web(port)).unwrap_err(),
                MapError::InvalidPortName,
                "{port}"
            );
        }
        let l64 = "a".repeat(64);
        for label in ["", "Cam", "-cam", "cam-", "ca.m", l64.as_str()] {
            let bad = ServiceRef {
                service: label,
                ..web("http")
            };
            assert_eq!(
                map.clone().with(CAM, bad).unwrap_err(),
                MapError::InvalidLabel
            );
            let bad = ServiceRef {
                namespace: label,
                ..web("http")
            };
            assert_eq!(
                map.clone().with(CAM, bad).unwrap_err(),
                MapError::InvalidLabel
            );
        }
        let long = std::format!("{}.{}", "d".repeat(50), "d".repeat(49));
        for domain in ["", "a..b", "a b", long.as_str()] {
            assert_eq!(
                map.clone().cluster_domain(domain).unwrap_err(),
                MapError::InvalidDomain,
                "{domain}"
            );
        }
        assert_eq!(
            map.clone()
                .with_kind_port(ServiceKind::from_static("Cam/x"), "cam", "media")
                .unwrap_err(),
            MapError::Unmappable
        );
        let long_kind = std::format!("k/{}", "a".repeat(MAX_KIND_LEN));
        let long_kind = crate::key::Key::new(&long_kind).unwrap();
        assert_eq!(
            map.with(ServiceKind::new(long_kind), web("http"))
                .unwrap_err(),
            MapError::KindTooLong
        );
    }
}
