//! [`RobotConfig`]: the robot's settings, parsed purely by
//! [`RobotConfig::from_env`], and the defaults it fills into the runner's
//! [`Config`] for running behind the robot's local Envoy
//! ([`RobotConfig::runner_config`]).
//!
//! | variable | default | format |
//! |----------|---------|--------|
//! | `MW_ROBOT_NAME` | (required) | RFC 1123 label: the robot's name, as in its SPIFFE ID `spiffe://infra.local/robot/<name>` |
//! | `MW_ROBOT_PERIOD_MS` | `1000` | milliseconds between ticks, 10..=3600000 |
//! | `MW_ROBOT_CALL` | `demo/echo` | the `Service/Method` kind called each tick |
//!
//! And of the runner's variables:
//!
//! | variable | default on a robot |
//! |----------|--------------------|
//! | `MW_ZENOH_ROUTER` | [`DEFAULT_ZENOH_ROUTER`], the local Envoy's TCP listener to zenohd |
//! | `MW_STATIC_ENDPOINTS` | `<call>=127.0.0.1:15001@<call-k8s-name>.apps.svc.cluster.local:50051` ([`default_endpoint`]), the local Envoy's h2c listener |
//!
//! A robot has no cluster DNS, so a called kind that neither
//! `MW_STATIC_ENDPOINTS` nor `MW_SERVICE_MAP` maps gets the default static
//! endpoint; one that is mapped keeps its mapping.

use middleware::grpc::istio::is_dns_label;
use middleware::key::Key;
use mw_service::{Config, StaticEndpoints};
use std::fmt;
use std::time::Duration;

/// `MW_ROBOT_NAME`.
pub const ENV_NAME: &str = "MW_ROBOT_NAME";
/// `MW_ROBOT_PERIOD_MS`.
pub const ENV_PERIOD_MS: &str = "MW_ROBOT_PERIOD_MS";
/// `MW_ROBOT_CALL`.
pub const ENV_CALL: &str = "MW_ROBOT_CALL";

/// The kind called by default.
pub const DEFAULT_CALL: &str = "demo/echo";
/// The tick period by default.
pub const DEFAULT_PERIOD: Duration = Duration::from_secs(1);
/// The local Envoy's h2c listener (to the cluster's ingress gateway).
pub const ENVOY_H2C: &str = "127.0.0.1:15001";
/// The local Envoy's TCP listener to zenohd, as a Zenoh locator.
pub const DEFAULT_ZENOH_ROUTER: &str = "tcp/127.0.0.1:17447";
/// The namespace and port of the default `:authority`.
pub const DEFAULT_AUTHORITY_SUFFIX: &str = "apps.svc.cluster.local:50051";

/// A malformed or missing robot variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RobotConfigError {
    /// The variable.
    pub var: &'static str,
    /// Its value (empty when unset).
    pub value: String,
    /// What is wrong.
    pub reason: &'static str,
}

impl fmt::Display for RobotConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}={:?}: {}", self.var, self.value, self.reason)
    }
}

impl std::error::Error for RobotConfigError {}

/// The robot's settings (module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RobotConfig {
    /// The robot's name.
    pub name: String,
    /// Time between ticks.
    pub period: Duration,
    /// The kind called each tick.
    pub call: String,
}

/// The default static endpoint entry for `kind`: the local Envoy, with
/// `:authority` `<kind segments joined by ->.apps.svc.cluster.local:50051`.
pub fn default_endpoint(kind: &str) -> String {
    let host = kind.replace('/', "-");
    format!("{kind}={ENVOY_H2C}@{host}.{DEFAULT_AUTHORITY_SUFFIX}")
}

impl RobotConfig {
    /// Parses the variables of the module docs through `var`. Pure.
    pub fn from_env(var: &impl Fn(&str) -> Option<String>) -> Result<Self, RobotConfigError> {
        let get = |name: &'static str| var(name).filter(|v| !v.is_empty());
        let err = |var: &'static str, value: &str, reason: &'static str| RobotConfigError {
            var,
            value: value.to_owned(),
            reason,
        };
        let name = get(ENV_NAME).ok_or_else(|| err(ENV_NAME, "", "required"))?;
        if !is_dns_label(&name) {
            return Err(err(ENV_NAME, &name, "not an RFC 1123 label"));
        }
        let period = match get(ENV_PERIOD_MS) {
            Some(v) => v
                .trim()
                .parse::<u64>()
                .ok()
                .filter(|ms| (10..=3_600_000).contains(ms))
                .map(Duration::from_millis)
                .ok_or_else(|| err(ENV_PERIOD_MS, &v, "not 10..=3600000 milliseconds"))?,
            None => DEFAULT_PERIOD,
        };
        let call = get(ENV_CALL).unwrap_or_else(|| DEFAULT_CALL.to_owned());
        if !mw_service::spec::is_kind(&call) {
            return Err(err(ENV_CALL, &call, "not a Service/Method kind"));
        }
        Ok(RobotConfig { name, period, call })
    }

    /// [`RobotConfig::from_env`] over the process environment.
    pub fn from_process_env() -> Result<Self, RobotConfigError> {
        Self::from_env(&|k| std::env::var(k).ok())
    }

    /// The service name: `robot/<name>`.
    pub fn service_name(&self) -> String {
        format!("robot/{}", self.name)
    }

    /// The telemetry key: `app/robot/<name>/state`.
    pub fn state_key(&self) -> String {
        format!("app/robot/{}/state", self.name)
    }

    /// `config` with the robot defaults filled in (module docs). Pure.
    pub fn runner_config(&self, config: Config) -> Config {
        let zenoh_router = config
            .zenoh_router
            .clone()
            .or_else(|| Some(DEFAULT_ZENOH_ROUTER.to_owned()));
        let mapped = config.static_endpoint(&self.call).is_some()
            || Key::new(&self.call).is_ok_and(|k| {
                config
                    .service_map
                    .get(middleware::discovery::ServiceKind::new(k))
                    .is_some()
            });
        let static_endpoints = if mapped {
            config.static_endpoints
        } else {
            let existing = config
                .static_endpoints
                .iter()
                .map(|e| format!("{}={}@{}", e.kind(), e.dial(), e.authority()));
            let all: Vec<String> = existing.chain([default_endpoint(&self.call)]).collect();
            // Every entry was valid and the call kind is new, so this parses;
            // on the impossible failure keep what was configured.
            StaticEndpoints::parse(&all.join(",")).unwrap_or(config.static_endpoints)
        };
        Config {
            zenoh_router,
            static_endpoints,
            ..config
        }
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
    fn v2_robot_config_parses_and_requires_a_name() {
        let c = RobotConfig::from_env(&env(&[("MW_ROBOT_NAME", "r1")])).unwrap();
        assert_eq!(
            c,
            RobotConfig {
                name: "r1".into(),
                period: DEFAULT_PERIOD,
                call: "demo/echo".into()
            }
        );
        assert_eq!(c.service_name(), "robot/r1");
        assert_eq!(c.state_key(), "app/robot/r1/state");
        let c = RobotConfig::from_env(&env(&[
            ("MW_ROBOT_NAME", "arm-2"),
            ("MW_ROBOT_PERIOD_MS", "250"),
            ("MW_ROBOT_CALL", "cam/jpeg"),
        ]))
        .unwrap();
        assert_eq!(
            (c.period, c.call.as_str()),
            (Duration::from_millis(250), "cam/jpeg")
        );
        for (pairs, var) in [
            (&[][..], "MW_ROBOT_NAME"),
            (&[("MW_ROBOT_NAME", "R1")][..], "MW_ROBOT_NAME"),
            (
                &[("MW_ROBOT_NAME", "r1"), ("MW_ROBOT_PERIOD_MS", "5")][..],
                "MW_ROBOT_PERIOD_MS",
            ),
            (
                &[("MW_ROBOT_NAME", "r1"), ("MW_ROBOT_CALL", "echo")][..],
                "MW_ROBOT_CALL",
            ),
        ] {
            let e = RobotConfig::from_env(&env(pairs)).unwrap_err();
            assert_eq!(e.var, var);
            assert!(e.to_string().starts_with(var));
        }
    }

    #[test]
    fn v3_runner_defaults_point_at_the_local_envoy() {
        let r = RobotConfig::from_env(&env(&[("MW_ROBOT_NAME", "r1")])).unwrap();
        let c = r.runner_config(Config::default());
        assert_eq!(c.zenoh_router.as_deref(), Some("tcp/127.0.0.1:17447"));
        let e = c.static_endpoint("demo/echo").unwrap();
        assert_eq!(e.dial(), "127.0.0.1:15001");
        assert_eq!(e.authority(), "demo-echo.apps.svc.cluster.local:50051");
        // What is configured wins.
        let configured = Config::from_env(&|k| match k {
            "MW_ZENOH_ROUTER" => Some("tcp/10.0.0.1:7447".into()),
            "MW_STATIC_ENDPOINTS" => {
                Some("demo/echo=127.0.0.1:9@echo.robots.infra.local:443".into())
            }
            _ => None,
        })
        .unwrap();
        let c = r.runner_config(configured.clone());
        assert_eq!(c, configured);
        // Another static endpoint is kept next to the default one.
        let other = Config::from_env(&|k| {
            (k == "MW_STATIC_ENDPOINTS").then(|| "cam/jpeg=127.0.0.1:9@cam.apps:1".to_owned())
        })
        .unwrap();
        let c = r.runner_config(other);
        assert!(
            c.static_endpoint("cam/jpeg").is_some() && c.static_endpoint("demo/echo").is_some()
        );
        // A service map entry also counts as mapped.
        let mapped = Config::from_env(&|k| {
            (k == "MW_SERVICE_MAP").then(|| "demo/echo=demo-echo.apps:grpc".to_owned())
        })
        .unwrap();
        assert!(
            r.runner_config(mapped)
                .static_endpoint("demo/echo")
                .is_none()
        );
    }
}
