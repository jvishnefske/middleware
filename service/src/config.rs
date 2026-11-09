//! [`Config`]: the deployment-specific settings a service reads from its
//! environment (12-factor), parsed purely by [`Config::from_env`].
//!
//! | variable | field | format |
//! |----------|-------|--------|
//! | `MW_PORT` | [`Config::port`] | decimal `u16`; overrides the spec's port (0: any free port, for tests) |
//! | `POD_NAMESPACE` | [`Config::namespace`] | RFC 1123 label (downward API) |
//! | `POD_NAME` | [`Config::pod_name`] | RFC 1123 subdomain, at most 253 bytes (downward API) |
//! | `OTEL_EXPORTER_OTLP_ENDPOINT` | [`Config::otlp_endpoint`] | `http://` or `https://` URL; unset: no trace export |
//! | `MW_ZENOH_ROUTER` | [`Config::zenoh_router`] | Zenoh locator `<proto>/<address>`, e.g. `tcp/zenohd.apps:7447` |
//! | `MW_ZENOH_CREDENTIALS` | [`Config::zenoh_credentials`] | path of the mounted credentials file |
//! | `MW_SERVICE_MAP` | [`Config::service_map`] | `kind=service.namespace:port_name,...` |
//! | `MW_DRAIN_GRACE_MS` | [`Config::drain_grace`] | decimal milliseconds; overrides the spec's drain grace |
//! | `MW_STATIC_ENDPOINTS` | [`Config::static_endpoints`] | `kind=[http://]dial_host:port@authority_host:port,...` ([`endpoints`](crate::endpoints)) |
//!
//! An unset or empty variable is `None` (an empty map for
//! `MW_SERVICE_MAP`). A set but malformed one is a [`ConfigError`] naming
//! the variable: a pod with a broken environment fails at start, loudly,
//! instead of running half configured.
//!
//! `MW_SERVICE_MAP` entries are separated by `,` (surrounding whitespace
//! ignored, empty entries skipped). Each maps a kind to the Kubernetes
//! Service that serves it, e.g.
//! `demo/echo=demo-echo.apps:grpc,cam/jpeg=cam.robots:grpc`, and becomes an
//! entry of a [`ServiceMap`] (validated there: RFC 1123 service and
//! namespace, IANA port name). A kind given twice is an error.
//!
//! `MW_STATIC_ENDPOINTS` is for services running outside the cluster (a
//! robot behind its local Envoy): each entry maps a called kind to a fixed
//! dial address and `:authority` instead of DNS-SD discovery (format in
//! [`endpoints`](crate::endpoints)). A kind with a static endpoint needs no
//! `MW_SERVICE_MAP` entry; given both, the static endpoint is used
//! ([`Config::static_endpoint`]).

use middleware::dns::unicast::{MapError, ServiceMap, ServiceRef};
use middleware::grpc::istio::is_dns_label;
use middleware::key::Key;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use crate::endpoints::{ENV_STATIC_ENDPOINTS, StaticEndpoint, StaticEndpoints};
use crate::spec::{MAX_DRAIN_GRACE_MS, ServiceSpec};

/// The kinds `MW_SERVICE_MAP` can map.
pub const SERVICE_MAP_CAPACITY: usize = 16;

/// The service map type of [`Config::service_map`].
pub type ConfigServiceMap = ServiceMap<SERVICE_MAP_CAPACITY>;

/// `MW_PORT`.
pub const ENV_PORT: &str = "MW_PORT";
/// `POD_NAMESPACE`.
pub const ENV_NAMESPACE: &str = "POD_NAMESPACE";
/// `POD_NAME`.
pub const ENV_POD_NAME: &str = "POD_NAME";
/// `OTEL_EXPORTER_OTLP_ENDPOINT`.
pub const ENV_OTLP_ENDPOINT: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";
/// `MW_ZENOH_ROUTER`.
pub const ENV_ZENOH_ROUTER: &str = "MW_ZENOH_ROUTER";
/// `MW_ZENOH_CREDENTIALS`.
pub const ENV_ZENOH_CREDENTIALS: &str = "MW_ZENOH_CREDENTIALS";
/// `MW_SERVICE_MAP`.
pub const ENV_SERVICE_MAP: &str = "MW_SERVICE_MAP";
/// `MW_DRAIN_GRACE_MS`.
pub const ENV_DRAIN_GRACE_MS: &str = "MW_DRAIN_GRACE_MS";

/// Every variable [`Config::from_env`] reads, in table order.
pub const ENV_VARS: [&str; 9] = [
    ENV_PORT,
    ENV_NAMESPACE,
    ENV_POD_NAME,
    ENV_OTLP_ENDPOINT,
    ENV_ZENOH_ROUTER,
    ENV_ZENOH_CREDENTIALS,
    ENV_SERVICE_MAP,
    ENV_DRAIN_GRACE_MS,
    ENV_STATIC_ENDPOINTS,
];

/// The settings of one deployment of a service (module docs). The fields
/// hold validated values when made by [`Config::from_env`];
/// [`Config::default`] is "nothing set".
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Config {
    /// Port override.
    pub port: Option<u16>,
    /// The pod's namespace.
    pub namespace: Option<String>,
    /// The pod's name.
    pub pod_name: Option<String>,
    /// Where to export traces (OTLP over gRPC).
    pub otlp_endpoint: Option<String>,
    /// The Zenoh router to connect to.
    pub zenoh_router: Option<String>,
    /// The Zenoh credentials file.
    pub zenoh_credentials: Option<PathBuf>,
    /// Which Kubernetes Service serves each called kind.
    pub service_map: ConfigServiceMap,
    /// Drain grace override.
    pub drain_grace: Option<Duration>,
    /// Fixed endpoints of called kinds, for running off cluster.
    pub static_endpoints: StaticEndpoints,
}

impl Config {
    /// Parses the variables of the module docs, read through `var`. Pure:
    /// pass `|k| std::env::var(k).ok()` for the process environment
    /// ([`Config::from_process_env`]) or a lookup table in tests.
    pub fn from_env(var: &impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let get = |name: &'static str| var(name).filter(|v| !v.is_empty());
        let port = get(ENV_PORT)
            .map(|v| {
                v.trim()
                    .parse::<u16>()
                    .map_err(|_| invalid(ENV_PORT, &v, "not a port number"))
            })
            .transpose()?;
        let namespace = get(ENV_NAMESPACE)
            .map(|v| checked(ENV_NAMESPACE, v, is_dns_label, "not an RFC 1123 label"))
            .transpose()?;
        let pod_name = get(ENV_POD_NAME)
            .map(|v| {
                checked(
                    ENV_POD_NAME,
                    v,
                    is_dns_subdomain,
                    "not an RFC 1123 subdomain",
                )
            })
            .transpose()?;
        let otlp_endpoint = get(ENV_OTLP_ENDPOINT)
            .map(|v| checked(ENV_OTLP_ENDPOINT, v, is_http_url, "not an http(s) URL"))
            .transpose()?;
        let zenoh_router = get(ENV_ZENOH_ROUTER)
            .map(|v| checked(ENV_ZENOH_ROUTER, v, is_zenoh_locator, "not a Zenoh locator"))
            .transpose()?;
        let zenoh_credentials = get(ENV_ZENOH_CREDENTIALS).map(PathBuf::from);
        let service_map = match get(ENV_SERVICE_MAP) {
            Some(v) => parse_service_map(&v)?,
            None => ConfigServiceMap::new(),
        };
        let drain_grace = get(ENV_DRAIN_GRACE_MS)
            .map(|v| match v.trim().parse::<u32>() {
                Ok(ms) if ms <= MAX_DRAIN_GRACE_MS => Ok(Duration::from_millis(u64::from(ms))),
                _ => Err(invalid(
                    ENV_DRAIN_GRACE_MS,
                    &v,
                    "not a number of milliseconds up to 600000",
                )),
            })
            .transpose()?;
        let static_endpoints = match get(ENV_STATIC_ENDPOINTS) {
            Some(v) => StaticEndpoints::parse(&v)?,
            None => StaticEndpoints::new(),
        };
        Ok(Config {
            port,
            namespace,
            pod_name,
            otlp_endpoint,
            zenoh_router,
            zenoh_credentials,
            service_map,
            drain_grace,
            static_endpoints,
        })
    }

    /// [`Config::from_env`] over the process environment.
    pub fn from_process_env() -> Result<Self, ConfigError> {
        Self::from_env(&|k| std::env::var(k).ok())
    }

    /// The port to listen on: the override, else the spec's.
    pub fn port_for(&self, spec: &ServiceSpec) -> u16 {
        self.port.unwrap_or(spec.port)
    }

    /// The drain grace: the override, else the spec's.
    pub fn drain_grace_for(&self, spec: &ServiceSpec) -> Duration {
        self.drain_grace.unwrap_or_else(|| spec.drain_grace())
    }

    /// The static endpoint of the called kind `kind`, if
    /// `MW_STATIC_ENDPOINTS` maps it; the runner then dials it instead of
    /// discovering the kind through `MW_SERVICE_MAP`.
    pub fn static_endpoint(&self, kind: &str) -> Option<&StaticEndpoint> {
        self.static_endpoints.get(kind)
    }

    /// The kinds `spec` calls that neither the service map nor the static
    /// endpoints map, in the spec's order. A runner refuses to start with
    /// any (it could not find them). Pure.
    pub fn unmapped_calls<'s>(&self, spec: &'s ServiceSpec) -> Vec<&'s str> {
        spec.calls
            .iter()
            .filter(|k| self.static_endpoint(k).is_none())
            .filter(|k| {
                Key::new(k).map_or(true, |key| {
                    self.service_map
                        .get(middleware::discovery::ServiceKind::new(key))
                        .is_none()
                })
            })
            .map(String::as_str)
            .collect()
    }
}

fn invalid(var: &'static str, value: &str, reason: &'static str) -> ConfigError {
    ConfigError::Invalid {
        var,
        value: value.to_owned(),
        reason,
    }
}

fn checked(
    var: &'static str,
    value: String,
    ok: impl Fn(&str) -> bool,
    reason: &'static str,
) -> Result<String, ConfigError> {
    if ok(&value) {
        Ok(value)
    } else {
        Err(invalid(var, &value, reason))
    }
}

/// RFC 1123 subdomain: dot-separated labels, at most 253 bytes.
fn is_dns_subdomain(s: &str) -> bool {
    s.len() <= 253 && s.split('.').all(is_dns_label)
}

fn is_http_url(s: &str) -> bool {
    let rest = s
        .strip_prefix("http://")
        .or_else(|| s.strip_prefix("https://"));
    rest.is_some_and(|r| !r.is_empty() && !r.contains(char::is_whitespace))
}

fn is_zenoh_locator(s: &str) -> bool {
    s.split_once('/').is_some_and(|(proto, addr)| {
        !proto.is_empty()
            && proto
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            && !addr.is_empty()
            && !addr.contains(char::is_whitespace)
    })
}

/// Parses an `MW_SERVICE_MAP` value (module docs). Pure.
pub fn parse_service_map(value: &str) -> Result<ConfigServiceMap, ConfigError> {
    let mut map = ConfigServiceMap::new();
    for entry in value.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let malformed = || ConfigError::ServiceMap {
            entry: entry.to_owned(),
            error: None,
        };
        let (kind, target) = entry.split_once('=').ok_or_else(malformed)?;
        let (service_ns, port_name) = target.trim().rsplit_once(':').ok_or_else(malformed)?;
        let (service, namespace) = service_ns.split_once('.').ok_or_else(malformed)?;
        let kind = Key::new(kind.trim()).map_err(|_| malformed())?;
        let kind = middleware::discovery::ServiceKind::new(kind);
        if map.get(kind).is_some() {
            return Err(ConfigError::DuplicateKind(kind.as_str().to_owned()));
        }
        map = map
            .with(
                kind,
                ServiceRef {
                    service,
                    namespace,
                    port_name,
                },
            )
            .map_err(|e| ConfigError::ServiceMap {
                entry: entry.to_owned(),
                error: Some(e),
            })?;
    }
    Ok(map)
}

/// Why the environment could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConfigError {
    /// A variable is set to a malformed value.
    Invalid {
        /// The variable.
        var: &'static str,
        /// Its value.
        value: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// An `MW_SERVICE_MAP` entry is malformed (`error: None`) or refused by
    /// the [`ServiceMap`] (`Some`).
    ServiceMap {
        /// The entry.
        entry: String,
        /// The map's reason, if it got that far.
        error: Option<MapError>,
    },
    /// `MW_SERVICE_MAP` maps a kind twice.
    DuplicateKind(String),
    /// An `MW_STATIC_ENDPOINTS` entry is malformed or repeats a kind.
    StaticEndpoint {
        /// The entry.
        entry: String,
        /// What is wrong with it.
        reason: &'static str,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Invalid { var, value, reason } => {
                write!(f, "{var}={value:?}: {reason}")
            }
            ConfigError::ServiceMap { entry, error: None } => write!(
                f,
                "{ENV_SERVICE_MAP} entry {entry:?} is not kind=service.namespace:port_name"
            ),
            ConfigError::ServiceMap {
                entry,
                error: Some(e),
            } => write!(f, "{ENV_SERVICE_MAP} entry {entry:?}: {e:?}"),
            ConfigError::DuplicateKind(k) => write!(f, "{ENV_SERVICE_MAP} maps {k:?} twice"),
            ConfigError::StaticEndpoint { entry, reason } => {
                write!(f, "{ENV_STATIC_ENDPOINTS} entry {entry:?}: {reason}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::discovery::ServiceKind;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn m2_nothing_set_is_the_default() {
        assert_eq!(Config::from_env(&env(&[])), Ok(Config::default()));
        let empty: Vec<(&str, &str)> = ENV_VARS.iter().map(|v| (*v, "")).collect();
        assert_eq!(Config::from_env(&env(&empty)), Ok(Config::default()));
    }

    #[test]
    fn m2_parses_every_variable() {
        let c = Config::from_env(&env(&[
            ("MW_PORT", "8080"),
            ("POD_NAMESPACE", "apps"),
            ("POD_NAME", "demo-echo-7d9f-x2x"),
            (
                "OTEL_EXPORTER_OTLP_ENDPOINT",
                "http://k8s-monitoring-alloy.observability.svc.cluster.local:4317",
            ),
            ("MW_ZENOH_ROUTER", "tcp/zenohd.apps.svc.cluster.local:7447"),
            ("MW_ZENOH_CREDENTIALS", "/var/run/secrets/zenoh/credentials"),
            (
                "MW_SERVICE_MAP",
                " demo/echo=demo-echo.apps:grpc , cam/jpeg=cam.robots:cam-jpeg,",
            ),
            ("MW_DRAIN_GRACE_MS", "2500"),
        ]))
        .expect("valid");
        assert_eq!(c.port, Some(8080));
        assert_eq!(c.namespace.as_deref(), Some("apps"));
        assert_eq!(c.pod_name.as_deref(), Some("demo-echo-7d9f-x2x"));
        assert!(c.otlp_endpoint.unwrap().ends_with(":4317"));
        assert_eq!(
            c.zenoh_router.as_deref(),
            Some("tcp/zenohd.apps.svc.cluster.local:7447")
        );
        assert_eq!(
            c.zenoh_credentials,
            Some(PathBuf::from("/var/run/secrets/zenoh/credentials"))
        );
        assert_eq!(c.service_map.len(), 2);
        let echo = c
            .service_map
            .get(ServiceKind::from_static("demo/echo"))
            .unwrap();
        assert_eq!(
            (echo.service, echo.namespace, echo.port_name),
            ("demo-echo", "apps", "grpc")
        );
        let cam = c
            .service_map
            .get(ServiceKind::from_static("cam/jpeg"))
            .unwrap();
        assert_eq!(
            (cam.service, cam.namespace, cam.port_name),
            ("cam", "robots", "cam-jpeg")
        );
        assert_eq!(c.drain_grace, Some(Duration::from_millis(2500)));
    }

    #[test]
    fn m2_malformed_values_name_the_variable() {
        for (var, value) in [
            ("MW_PORT", "65536"),
            ("MW_PORT", "http"),
            ("POD_NAMESPACE", "Apps"),
            ("POD_NAME", "a..b"),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "alloy:4317"),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://"),
            ("MW_ZENOH_ROUTER", "zenohd:7447"),
            ("MW_ZENOH_ROUTER", "TCP/x"),
            ("MW_DRAIN_GRACE_MS", "-1"),
            ("MW_DRAIN_GRACE_MS", "600001"),
        ] {
            match Config::from_env(&env(&[(var, value)])) {
                Err(ConfigError::Invalid {
                    var: v, value: got, ..
                }) => {
                    assert_eq!((v, got.as_str()), (var, value));
                }
                other => panic!("{var}={value}: {other:?}"),
            }
        }
    }

    #[test]
    fn m2_service_map_entries_are_validated() {
        for bad in [
            "demo/echo",
            "demo/echo=demo-echo:grpc",
            "demo/echo=demo-echo.apps",
            "bad key=a.b:grpc",
        ] {
            assert_eq!(
                parse_service_map(bad),
                Err(ConfigError::ServiceMap {
                    entry: bad.to_owned(),
                    error: None
                }),
                "{bad:?}"
            );
        }
        assert_eq!(
            parse_service_map("demo/echo=Demo.apps:grpc"),
            Err(ConfigError::ServiceMap {
                entry: "demo/echo=Demo.apps:grpc".into(),
                error: Some(MapError::InvalidLabel)
            })
        );
        assert_eq!(
            parse_service_map("a/b=s.n:p,a/b=t.n:p"),
            Err(ConfigError::DuplicateKind("a/b".into()))
        );
        assert!(parse_service_map(" , ").unwrap().is_empty());
        let full: Vec<String> = (0..=SERVICE_MAP_CAPACITY)
            .map(|i| format!("k/{i}=s.n:grpc"))
            .collect();
        assert!(matches!(
            parse_service_map(&full.join(",")),
            Err(ConfigError::ServiceMap {
                error: Some(MapError::Full),
                ..
            })
        ));
    }

    #[test]
    fn m2_overrides_and_unmapped_calls() {
        let mut spec = ServiceSpec::new("demo/caller");
        spec.calls = vec!["demo/echo".into(), "cam/jpeg".into()];
        let c =
            Config::from_env(&env(&[("MW_SERVICE_MAP", "demo/echo=demo-echo.apps:grpc")])).unwrap();
        assert_eq!(c.unmapped_calls(&spec), ["cam/jpeg"]);
        assert_eq!(c.port_for(&spec), spec.port);
        assert_eq!(c.drain_grace_for(&spec), spec.drain_grace());
        let o = Config {
            port: Some(0),
            drain_grace: Some(Duration::from_millis(10)),
            ..Config::default()
        };
        assert_eq!(o.port_for(&spec), 0);
        assert_eq!(o.drain_grace_for(&spec), Duration::from_millis(10));
    }

    #[test]
    fn v5_static_endpoints_map_calls_off_cluster() {
        let mut spec = ServiceSpec::new("robot/r1");
        spec.calls = vec!["demo/echo".into(), "cam/jpeg".into()];
        let c = Config::from_env(&env(&[(
            "MW_STATIC_ENDPOINTS",
            "demo/echo=http://127.0.0.1:15001@demo-echo.apps.svc.cluster.local:50051",
        )]))
        .unwrap();
        assert_eq!(c.unmapped_calls(&spec), ["cam/jpeg"]);
        let e = c.static_endpoint("demo/echo").unwrap();
        assert_eq!(e.dial(), "127.0.0.1:15001");
        assert!(c.static_endpoint("cam/jpeg").is_none());
        let bad = Config::from_env(&env(&[("MW_STATIC_ENDPOINTS", "demo/echo=nowhere")]));
        let err = bad.unwrap_err();
        assert!(
            err.to_string().starts_with("MW_STATIC_ENDPOINTS entry"),
            "{err}"
        );
    }
}
