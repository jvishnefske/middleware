//! [`ServiceSpec`]: everything a service declares about itself, as plain
//! serializable data, and its validation ([`ServiceSpec::validate`]).
//!
//! The spec is what the [`ServiceBuilder`](crate::ServiceBuilder)
//! accumulates and what `--print-spec` prints; the Kubernetes manifests
//! (`--print-manifests`, [`manifest`](crate::manifest)) and the ops CLI
//! render from it, so the deployed configuration is derived from the code
//! and cannot drift from it.
//!
//! # Determinism
//!
//! Every list keeps declaration order and refuses duplicates
//! ([`SpecError::Duplicate`]), so the same builder calls always give the
//! same spec and the same serialized bytes. There are no maps.
//!
//! # Rules ([`ServiceSpec::validate`])
//!
//! * `name`: a valid [`Key`] whose segments joined by `-`
//!   form an RFC 1123 label (`demo/echo` → `demo-echo`, [`k8s_name`]);
//! * `serves` and `calls`: two-segment keys (`Service/Method`, the form a
//!   gRPC path carries), no duplicates; a kind may not be both served and
//!   called;
//! * `allow_callers`: SPIFFE IDs ([`is_spiffe_id`]). An **empty list admits
//!   no caller**: the manifests allow only the kubelet's health probes;
//! * `publishes` and `subscribes`: key filters in text form ([`parse_filter`]):
//!   `a/b` is exactly that key, `a/b/**` the key and everything below it;
//! * `replicas` ≥ 1, `port` ≠ 0, `drain_grace_ms` ≤ [`MAX_DRAIN_GRACE_MS`];
//! * `resources`: every quantity non-zero and each request at most its
//!   limit.

use middleware::grpc::istio::{is_dns_label, is_spiffe_id};
use middleware::key::{Key, KeyFilter};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Duration;

/// The gRPC port a service listens on unless set with
/// [`ServiceBuilder::port`](crate::ServiceBuilder::port).
pub const DEFAULT_PORT: u16 = 50051;

/// The drain grace unless set with
/// [`ServiceBuilder::drain_grace`](crate::ServiceBuilder::drain_grace): how
/// long a terminating pod keeps serving after reporting `NOT_SERVING`.
pub const DEFAULT_DRAIN_GRACE: Duration = Duration::from_secs(5);

/// The longest drain grace accepted (ten minutes).
pub const MAX_DRAIN_GRACE_MS: u32 = 600_000;

/// CPU in thousandths of a core (Kubernetes `100m`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MilliCpu(pub u32);

impl fmt::Display for MilliCpu {
    /// The Kubernetes quantity, `100m`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}m", self.0)
    }
}

/// Memory in mebibytes (Kubernetes `128Mi`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Mebibytes(pub u32);

impl fmt::Display for Mebibytes {
    /// The Kubernetes quantity, `128Mi`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}Mi", self.0)
    }
}

/// Container resource requests and limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Resources {
    /// CPU the scheduler reserves.
    pub cpu_request: MilliCpu,
    /// CPU the container may use.
    pub cpu_limit: MilliCpu,
    /// Memory the scheduler reserves.
    pub memory_request: Mebibytes,
    /// Memory the container may use before it is killed.
    pub memory_limit: Mebibytes,
}

impl Resources {
    /// 50m / 500m CPU, 64Mi / 256Mi memory.
    pub const DEFAULT: Resources = Resources {
        cpu_request: MilliCpu(50),
        cpu_limit: MilliCpu(500),
        memory_request: Mebibytes(64),
        memory_limit: Mebibytes(256),
    };

    /// Whether every quantity is non-zero and each request at most its
    /// limit. Pure.
    pub const fn is_valid(&self) -> bool {
        self.cpu_request.0 > 0
            && self.memory_request.0 > 0
            && self.cpu_request.0 <= self.cpu_limit.0
            && self.memory_request.0 <= self.memory_limit.0
    }
}

impl Default for Resources {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Everything a service declares; see the module docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceSpec {
    /// The service's name as given to
    /// [`Service::builder`](crate::Service::builder), e.g. `demo/echo`.
    pub name: String,
    /// The kinds this service answers (`Service/Method` keys), in
    /// declaration order.
    pub serves: Vec<String>,
    /// The kinds this service calls (its dependencies), in declaration
    /// order.
    pub calls: Vec<String>,
    /// SPIFFE IDs allowed to call this service; empty admits none.
    pub allow_callers: Vec<String>,
    /// Key filters this service publishes on (text form).
    pub publishes: Vec<String>,
    /// Key filters this service subscribes to (text form).
    pub subscribes: Vec<String>,
    /// Desired pod count.
    pub replicas: u16,
    /// Container resources.
    pub resources: Resources,
    /// How long the service keeps serving after reporting `NOT_SERVING` on
    /// termination, in milliseconds.
    pub drain_grace_ms: u32,
    /// The gRPC (and health) port.
    pub port: u16,
}

impl ServiceSpec {
    /// A spec named `name` declaring nothing yet, with the defaults: one
    /// replica, [`Resources::DEFAULT`], [`DEFAULT_DRAIN_GRACE`],
    /// [`DEFAULT_PORT`]. Not validated.
    pub fn new(name: impl Into<String>) -> Self {
        ServiceSpec {
            name: name.into(),
            serves: Vec::new(),
            calls: Vec::new(),
            allow_callers: Vec::new(),
            publishes: Vec::new(),
            subscribes: Vec::new(),
            replicas: 1,
            resources: Resources::DEFAULT,
            drain_grace_ms: duration_ms(DEFAULT_DRAIN_GRACE),
            port: DEFAULT_PORT,
        }
    }

    /// Checks every rule of the module docs; the first violation found, in
    /// field order. Pure.
    pub fn validate(&self) -> Result<(), SpecError> {
        k8s_name(&self.name).ok_or_else(|| SpecError::InvalidName(self.name.clone()))?;
        check_list(&self.serves, Field::Serves, is_kind)?;
        check_list(&self.calls, Field::Calls, is_kind)?;
        if let Some(both) = self.serves.iter().find(|k| self.calls.contains(k)) {
            return Err(SpecError::ServesAndCalls(both.clone()));
        }
        check_list(&self.allow_callers, Field::AllowCallers, is_spiffe_id)?;
        check_list(&self.publishes, Field::Publishes, |f| {
            parse_filter(f).is_some()
        })?;
        check_list(&self.subscribes, Field::Subscribes, |f| {
            parse_filter(f).is_some()
        })?;
        if self.replicas == 0 {
            return Err(SpecError::ZeroReplicas);
        }
        if !self.resources.is_valid() {
            return Err(SpecError::InvalidResources(self.resources));
        }
        if self.drain_grace_ms > MAX_DRAIN_GRACE_MS {
            return Err(SpecError::DrainGraceTooLong(self.drain_grace_ms));
        }
        if self.port == 0 {
            return Err(SpecError::ZeroPort);
        }
        Ok(())
    }

    /// The Kubernetes object name derived from [`ServiceSpec::name`]
    /// ([`k8s_name`]); `None` for an invalid name.
    pub fn k8s_name(&self) -> Option<String> {
        k8s_name(&self.name)
    }

    /// The drain grace as a duration.
    pub fn drain_grace(&self) -> Duration {
        Duration::from_millis(u64::from(self.drain_grace_ms))
    }

    /// The parsed [`publishes`](Self::publishes) filters (invalid entries
    /// skipped; a validated spec has none).
    pub fn publish_filters(&self) -> impl Iterator<Item = KeyFilter<'_>> {
        self.publishes.iter().filter_map(|f| parse_filter(f))
    }

    /// The parsed [`subscribes`](Self::subscribes) filters (invalid
    /// entries skipped; a validated spec has none).
    pub fn subscribe_filters(&self) -> impl Iterator<Item = KeyFilter<'_>> {
        self.subscribes.iter().filter_map(|f| parse_filter(f))
    }

    /// Whether the service needs a Zenoh session (it publishes or
    /// subscribes).
    pub fn uses_zenoh(&self) -> bool {
        !self.publishes.is_empty() || !self.subscribes.is_empty()
    }

    /// The spec as YAML (deterministic: field order, then declaration
    /// order).
    pub fn to_yaml(&self) -> Result<String, SpecError> {
        serde_norway::to_string(self).map_err(|e| SpecError::Serialize(e.to_string()))
    }
}

/// Milliseconds of `d`, saturating at `u32::MAX`.
pub(crate) fn duration_ms(d: Duration) -> u32 {
    u32::try_from(d.as_millis()).unwrap_or(u32::MAX)
}

/// The Kubernetes name of service `name`: its key segments joined by `-`,
/// if that is an RFC 1123 label (`demo/echo` → `demo-echo`). Pure.
pub fn k8s_name(name: &str) -> Option<String> {
    let key = Key::new(name).ok()?;
    let joined = key.segments().collect::<Vec<_>>().join("-");
    is_dns_label(&joined).then_some(joined)
}

/// Whether `kind` is a kind a service can serve or call over gRPC: a key
/// of exactly two segments (`Service/Method`). Pure.
pub fn is_kind(kind: &str) -> bool {
    Key::new(kind).is_ok_and(|k| k.service_method().is_some())
}

/// Parses a key filter in text form: `a/b/**` is
/// [`KeyFilter::Prefix`]`(a/b)`, anything else must be a key and is
/// [`KeyFilter::Exact`]. Pure.
pub fn parse_filter(text: &str) -> Option<KeyFilter<'_>> {
    match text.strip_suffix("/**") {
        Some(prefix) => Key::new(prefix).ok().map(KeyFilter::Prefix),
        None => Key::new(text).ok().map(KeyFilter::Exact),
    }
}

fn check_list(
    list: &[String],
    field: Field,
    valid: impl Fn(&str) -> bool,
) -> Result<(), SpecError> {
    for (i, item) in list.iter().enumerate() {
        if !valid(item) {
            return Err(SpecError::Invalid {
                field,
                value: item.clone(),
            });
        }
        if list.get(..i).is_some_and(|before| before.contains(item)) {
            return Err(SpecError::Duplicate {
                field,
                value: item.clone(),
            });
        }
    }
    Ok(())
}

/// A list field of [`ServiceSpec`], named in [`SpecError`]s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    /// [`ServiceSpec::serves`].
    Serves,
    /// [`ServiceSpec::calls`].
    Calls,
    /// [`ServiceSpec::allow_callers`].
    AllowCallers,
    /// [`ServiceSpec::publishes`].
    Publishes,
    /// [`ServiceSpec::subscribes`].
    Subscribes,
}

impl fmt::Display for Field {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Field::Serves => "serves",
            Field::Calls => "calls",
            Field::AllowCallers => "allow_callers",
            Field::Publishes => "publishes",
            Field::Subscribes => "subscribes",
        })
    }
}

/// Why a [`ServiceSpec`] is invalid.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SpecError {
    /// The name is not a key whose segments form an RFC 1123 label.
    InvalidName(String),
    /// An entry of a list field breaks that field's rule.
    Invalid {
        /// The field.
        field: Field,
        /// The offending entry.
        value: String,
    },
    /// An entry appears twice in a list field.
    Duplicate {
        /// The field.
        field: Field,
        /// The repeated entry.
        value: String,
    },
    /// A kind is both served and called.
    ServesAndCalls(String),
    /// `replicas` is 0.
    ZeroReplicas,
    /// A resource quantity is zero or a request exceeds its limit.
    InvalidResources(Resources),
    /// `drain_grace_ms` exceeds [`MAX_DRAIN_GRACE_MS`].
    DrainGraceTooLong(u32),
    /// `port` is 0.
    ZeroPort,
    /// The spec could not be serialized.
    Serialize(String),
}

impl fmt::Display for SpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SpecError::InvalidName(n) => write!(
                f,
                "service name {n:?} is not a key whose segments form an RFC 1123 label"
            ),
            SpecError::Invalid { field, value } => match field {
                Field::Serves | Field::Calls => {
                    write!(f, "{field}: {value:?} is not a Service/Method kind")
                }
                Field::AllowCallers => write!(f, "{field}: {value:?} is not a SPIFFE ID"),
                Field::Publishes | Field::Subscribes => {
                    write!(f, "{field}: {value:?} is not a key filter")
                }
            },
            SpecError::Duplicate { field, value } => write!(f, "{field}: {value:?} given twice"),
            SpecError::ServesAndCalls(k) => write!(f, "{k:?} is both served and called"),
            SpecError::ZeroReplicas => f.write_str("replicas must be at least 1"),
            SpecError::InvalidResources(r) => write!(
                f,
                "resources must be non-zero with requests within limits: {r:?}"
            ),
            SpecError::DrainGraceTooLong(ms) => {
                write!(f, "drain grace {ms} ms exceeds {MAX_DRAIN_GRACE_MS} ms")
            }
            SpecError::ZeroPort => f.write_str("port must be non-zero"),
            SpecError::Serialize(e) => write!(f, "serializing the spec failed: {e}"),
        }
    }
}

impl std::error::Error for SpecError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> ServiceSpec {
        let mut s = ServiceSpec::new("demo/echo");
        s.serves.push("demo/echo".into());
        s.calls.push("cam/jpeg".into());
        s.allow_callers
            .push("spiffe://infra.local/ns/apps/sa/demo-caller".into());
        s.publishes.push("app/demo/**".into());
        s.subscribes.push("app/robot/pose".into());
        s
    }

    #[test]
    fn m1_names_map_to_k8s_labels() {
        assert_eq!(k8s_name("demo/echo").as_deref(), Some("demo-echo"));
        assert_eq!(k8s_name("gateway").as_deref(), Some("gateway"));
        for bad in ["", "Demo/echo", "a b", "a_b/c", "-a/b", "demo//echo", "x/"] {
            assert_eq!(k8s_name(bad), None, "{bad:?}");
        }
        let long = format!("{}/b", "a".repeat(62));
        assert_eq!(k8s_name(&long), None, "over 63 bytes once joined");
    }

    #[test]
    fn m1_filters_parse_exact_and_prefix() {
        assert_eq!(
            parse_filter("app/x"),
            Some(KeyFilter::Exact(Key::from_static("app/x")))
        );
        assert_eq!(
            parse_filter("app/**"),
            Some(KeyFilter::Prefix(Key::from_static("app")))
        );
        for bad in ["", "**", "/**", "a b", "app/*"] {
            assert_eq!(parse_filter(bad), None, "{bad:?}");
        }
        assert!(is_kind("demo/echo"));
        assert!(!is_kind("demo"));
        assert!(!is_kind("a/b/c"));
    }

    #[test]
    fn m1_validate_accepts_a_full_spec_and_reports_each_rule() {
        assert_eq!(valid().validate(), Ok(()));
        let field_err = |f: fn(&mut ServiceSpec), want: SpecError| {
            let mut s = valid();
            f(&mut s);
            assert_eq!(s.validate(), Err(want));
        };
        field_err(
            |s| s.name = "Bad".into(),
            SpecError::InvalidName("Bad".into()),
        );
        field_err(
            |s| s.serves.push("one".into()),
            SpecError::Invalid {
                field: Field::Serves,
                value: "one".into(),
            },
        );
        field_err(
            |s| s.serves.push("demo/echo".into()),
            SpecError::Duplicate {
                field: Field::Serves,
                value: "demo/echo".into(),
            },
        );
        field_err(
            |s| s.calls.push("demo/echo".into()),
            SpecError::ServesAndCalls("demo/echo".into()),
        );
        field_err(
            |s| s.allow_callers.push("cluster.local/ns/a/sa/b".into()),
            SpecError::Invalid {
                field: Field::AllowCallers,
                value: "cluster.local/ns/a/sa/b".into(),
            },
        );
        field_err(
            |s| s.publishes.push("bad key".into()),
            SpecError::Invalid {
                field: Field::Publishes,
                value: "bad key".into(),
            },
        );
        field_err(
            |s| s.subscribes.push("app/robot/pose".into()),
            SpecError::Duplicate {
                field: Field::Subscribes,
                value: "app/robot/pose".into(),
            },
        );
        field_err(|s| s.replicas = 0, SpecError::ZeroReplicas);
        field_err(
            |s| s.resources.cpu_request = MilliCpu(1000),
            SpecError::InvalidResources(Resources {
                cpu_request: MilliCpu(1000),
                ..Resources::DEFAULT
            }),
        );
        field_err(
            |s| s.drain_grace_ms = MAX_DRAIN_GRACE_MS + 1,
            SpecError::DrainGraceTooLong(MAX_DRAIN_GRACE_MS + 1),
        );
        field_err(|s| s.port = 0, SpecError::ZeroPort);
    }

    #[test]
    fn m1_resources_render_as_kubernetes_quantities() {
        let r = Resources::DEFAULT;
        assert_eq!(
            (
                r.cpu_request.to_string(),
                r.cpu_limit.to_string(),
                r.memory_request.to_string(),
                r.memory_limit.to_string()
            ),
            ("50m".into(), "500m".into(), "64Mi".into(), "256Mi".into())
        );
        assert!(r.is_valid());
        let zero = Resources {
            memory_request: Mebibytes(0),
            ..r
        };
        assert!(!zero.is_valid());
    }

    #[test]
    fn m1_yaml_is_deterministic_and_round_trips() {
        let s = valid();
        let yaml = s.to_yaml().unwrap();
        assert_eq!(yaml, valid().to_yaml().unwrap());
        let order: Vec<&str> = yaml
            .lines()
            .filter(|l| !l.starts_with(' ') && !l.starts_with('-'))
            .filter_map(|l| l.split(':').next())
            .collect();
        assert_eq!(
            order,
            [
                "name",
                "serves",
                "calls",
                "allow_callers",
                "publishes",
                "subscribes",
                "replicas",
                "resources",
                "drain_grace_ms",
                "port"
            ]
        );
        let back: ServiceSpec = serde_norway::from_str(&yaml).unwrap();
        assert_eq!(back, s);
        assert!(s.uses_zenoh());
        assert_eq!(s.publish_filters().count(), 1);
        assert_eq!(s.subscribe_filters().count(), 1);
        assert_eq!(s.drain_grace(), DEFAULT_DRAIN_GRACE);
        assert_eq!(s.k8s_name().as_deref(), Some("demo-echo"));
    }
}
