//! The Zenoh router (zenohd) with authentication and access control:
//! config, credentials and Kubernetes manifests, rendered from the
//! services' [`ServiceSpec`]s.
//!
//! Without a router, Zenoh identities are self-asserted (the middleware's
//! Zenoh trust model, design.md Z17/Z18): any session may publish on any
//! key, declare any queryable and announce any service. In the cluster,
//! every service session connects in `client` mode to one zenohd, which
//!
//! 1. admits only connections from listed mesh principals: an Istio
//!    AuthorizationPolicy on port [`ZENOH_PORT`] names the SPIFFE
//!    principal of every service that [`uses_zenoh`](ServiceSpec::uses_zenoh),
//!    and, for robots, the ingress gateway's;
//! 2. authenticates each Zenoh link with a per-principal user/password
//!    ([`credentials`]: the username is the service's Kubernetes name);
//! 3. authorises each message by username against rules derived from the
//!    spec ([`acl`]): a service publishes, subscribes, answers, queries and
//!    announces exactly what it declares, browses discovery, and nothing
//!    else (default deny).
//!
//! So an in-cluster Zenoh peer is an *authenticated connection with an
//! authorised key space*: a service cannot publish on another's keys,
//! answer for a kind it does not serve or announce a service it is not.
//! What stays self-asserted is the `SenderId` inside attachments and the
//! session's Zenoh id (see [`acl`]'s limits).
//!
//! # Entry points
//!
//! * [`principals`]: who needs credentials (pure). The ops CLI generates
//!   one password per principal from this list.
//! * [`plan`]: the public half — router config ([`config::render`]) and
//!   the committed manifests ([`k8s::manifests`]); no secrets (pure).
//! * [`render_router`]: the plan plus, from the generated credentials, the
//!   router's dictionary and the Secret manifests (pure; never commit its
//!   secret output).
//! * [`config::client_config`]: a service's client session config from its
//!   credentials file.
//!
//! # Robots
//!
//! A robot reaches zenohd through the Istio ingress gateway (NodePort
//! 30443, container port 15443, mutual TLS with SPIRE SVIDs), so the
//! router's sidecar sees the *gateway's* principal, not the robot's; the
//! gateway's own policy admits only `infra.local/robot/*` there. The robot
//! then authenticates to zenohd as `robot-<name>` with its own grants. The
//! route is [`k8s`]'s `zenohd-robots` Gateway server (protocol `TLS`, mode
//! `ISTIO_MUTUAL`, selected by SNI [`RobotRoute::host`]) and its
//! VirtualService `tcp` route, because the lab's `https-robots` server is
//! `HTTPS` and Istio applies only `http` routes there (see [`k8s`]).

pub mod acl;
pub mod config;
pub mod credentials;
pub mod k8s;

use std::fmt;

use middleware::grpc::istio::is_dns_label;
use serde::Deserialize;

use crate::spec::{ServiceSpec, SpecError};
use acl::{AclModel, Grants, Principal};
use credentials::{Credentials, CredentialsError};

/// The router's Kubernetes name (ServiceAccount, Deployment, Service).
pub const NAME: &str = "zenohd";

/// The router's TCP port, and the Service port named `zenoh`.
pub const ZENOH_PORT: u16 = 7447;

/// The router image (imported into the nodes, never pulled).
pub const DEFAULT_IMAGE: &str = "localhost/mw/zenohd";

/// Where the router reads its config (`zenohd -c`).
pub const CONFIG_PATH: &str = "/etc/zenoh/config.json5";

/// Where the router reads the user/password dictionary.
pub const DICTIONARY_PATH: &str = "/etc/zenoh/auth/dictionary";

/// The ingress gateway's mTLS principal in the lab.
pub const DEFAULT_GATEWAY_PRINCIPAL: &str = "infra.local/ns/istio-ingress/sa/istio-ingressgateway";

/// The SNI a robot sends to select the Zenoh route on the gateway.
pub const DEFAULT_ROBOT_HOST: &str = "zenoh.infra.local";

/// The gateway's robot port (container and Service port; NodePort 30443).
pub const DEFAULT_GATEWAY_PORT: u16 = 15443;

/// The Zenoh locator services connect to: `tcp/zenohd.<ns>.svc.cluster.local:7447`
/// (the value for `MW_ZENOH_ROUTER`).
pub fn router_locator(namespace: &str) -> String {
    format!("tcp/{NAME}.{namespace}.svc.cluster.local:{ZENOH_PORT}")
}

/// Grants added to one service's grants from its spec (for example the
/// MCU gateway's announcements of device kinds).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtraGrant {
    /// The service's Zenoh username (its Kubernetes name).
    pub user: String,
    /// What it may do beyond its spec.
    pub grants: Grants,
}

/// A robot outside the cluster: Zenoh user `robot-<name>`, SPIFFE ID
/// `spiffe://<trust domain>/robot/<name>`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Robot {
    /// The robot's name (an RFC 1123 label, as given to `robot-join`).
    pub name: String,
    /// What it may do.
    pub grants: Grants,
}

impl Robot {
    /// The robot's Zenoh username: `robot-<name>`.
    pub fn user(&self) -> String {
        format!("robot-{}", self.name)
    }
}

/// How robots reach the router through the ingress gateway. Deserializes
/// with every field defaulted.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RobotRoute {
    /// The SNI host the robot sends; the gateway server and VirtualService
    /// host.
    pub host: String,
    /// The gateway port the server is merged into.
    pub gateway_port: u16,
    /// The gateway pods' `istio` label value.
    pub gateway_selector: String,
    /// The gateway's mTLS principal, as the router's sidecar sees it.
    pub gateway_principal: String,
}

impl Default for RobotRoute {
    /// The lab's gateway: `istio: ingressgateway`, port 15443, SNI
    /// [`DEFAULT_ROBOT_HOST`].
    fn default() -> Self {
        RobotRoute {
            host: DEFAULT_ROBOT_HOST.into(),
            gateway_port: DEFAULT_GATEWAY_PORT,
            gateway_selector: "ingressgateway".into(),
            gateway_principal: DEFAULT_GATEWAY_PRINCIPAL.into(),
        }
    }
}

/// Everything the router needs beyond the specs. Deserializes (YAML or
/// JSON, as the example `zenoh_router_manifests` reads it) with every
/// field defaulted.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RouterOptions {
    /// The namespace of the router and the services.
    pub namespace: String,
    /// The router image.
    pub image: String,
    /// The SPIFFE trust domain.
    pub trust_domain: String,
    /// The address the router listens on (`0.0.0.0` in a pod).
    pub listen_host: String,
    /// The port the router listens on.
    pub port: u16,
    /// The dictionary file path in the router's config.
    pub dictionary_file: String,
    /// Grants beyond the specs, per service.
    pub extra_grants: Vec<ExtraGrant>,
    /// Robots allowed in.
    pub robots: Vec<Robot>,
    /// The robots' route through the ingress gateway; `None` renders no
    /// Gateway or VirtualService.
    pub robot_route: Option<RobotRoute>,
}

impl Default for RouterOptions {
    /// The lab: namespace `apps`, [`DEFAULT_IMAGE`], trust domain
    /// `infra.local`, `0.0.0.0:7447`, [`DICTIONARY_PATH`], the default
    /// [`RobotRoute`], no extra grants, no robots.
    fn default() -> Self {
        RouterOptions {
            namespace: "apps".into(),
            image: DEFAULT_IMAGE.into(),
            trust_domain: "infra.local".into(),
            listen_host: "0.0.0.0".into(),
            port: ZENOH_PORT,
            dictionary_file: DICTIONARY_PATH.into(),
            extra_grants: Vec::new(),
            robots: Vec::new(),
            robot_route: Some(RobotRoute::default()),
        }
    }
}

/// The public, committable half of a router deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouterPlan {
    /// Every principal, sorted by user.
    pub principals: Vec<Principal>,
    /// The access-control model.
    pub acl: AclModel,
    /// zenohd's config file.
    pub config_json5: String,
    /// The router's manifests (no Secret contents).
    pub k8s_manifests_yaml: String,
}

/// A full router deployment, including secret material. Only
/// [`RouterPlan`]'s fields may be committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouterBundle {
    /// The public half.
    pub plan: RouterPlan,
    /// The router's `usrpwd` dictionary (secret).
    pub dictionary: String,
    /// The Secrets: the router's dictionary and each service's credentials
    /// file (secret).
    pub secrets_yaml: String,
}

/// The principals of `specs` and `options`, sorted by user: every service
/// that [`uses_zenoh`](ServiceSpec::uses_zenoh) (user = its Kubernetes
/// name, peer = its service account's principal, grants from its spec plus
/// any [`ExtraGrant`]), then every robot (peer = the gateway's principal).
/// Pure.
pub fn principals(
    specs: &[ServiceSpec],
    options: &RouterOptions,
) -> Result<Vec<Principal>, RouterError> {
    let mut out = Vec::new();
    for spec in specs {
        spec.validate()?;
        if !spec.uses_zenoh() {
            continue;
        }
        let user = spec
            .k8s_name()
            .ok_or_else(|| RouterError::Spec(SpecError::InvalidName(spec.name.clone())))?;
        let extra = options
            .extra_grants
            .iter()
            .filter(|e| e.user == user)
            .fold(Grants::default(), |g, e| g.merge(&e.grants));
        out.push(Principal {
            peer: format!(
                "{}/ns/{}/sa/{user}",
                options.trust_domain, options.namespace
            ),
            grants: Grants::from_spec(spec).merge(&extra),
            user,
        });
    }
    if let Some(e) = options
        .extra_grants
        .iter()
        .find(|e| !out.iter().any(|p| p.user == e.user))
    {
        return Err(RouterError::UnknownUser(e.user.clone()));
    }
    if !options.robots.is_empty() && options.robot_route.is_none() {
        return Err(RouterError::NoRobotRoute);
    }
    for robot in &options.robots {
        let user = robot.user();
        if !is_dns_label(&user) {
            return Err(RouterError::InvalidRobot(robot.name.clone()));
        }
        let peer = options
            .robot_route
            .as_ref()
            .map(|r| r.gateway_principal.clone())
            .unwrap_or_default();
        out.push(Principal {
            user,
            peer,
            grants: robot.grants.clone(),
        });
    }
    out.sort_by(|a, b| a.user.cmp(&b.user));
    Ok(out)
}

/// The public half of the deployment (module docs). Pure.
pub fn plan(specs: &[ServiceSpec], options: &RouterOptions) -> Result<RouterPlan, RouterError> {
    let principals = principals(specs, options)?;
    let acl = AclModel::from_principals(&principals)?;
    let config_json5 = config::render(
        &acl,
        &options.listen_host,
        options.port,
        &options.dictionary_file,
    )?;
    let k8s_manifests_yaml = k8s::manifests(options, &principals, &config_json5)?;
    Ok(RouterPlan {
        principals,
        acl,
        config_json5,
        k8s_manifests_yaml,
    })
}

impl RouterPlan {
    /// The dictionary and Secret manifests from one [`Credentials`] per
    /// principal: a missing or unexpected user is an error. Pure.
    pub fn secrets(
        &self,
        options: &RouterOptions,
        credentials: &[Credentials],
    ) -> Result<(String, String), RouterError> {
        if let Some(p) = self
            .principals
            .iter()
            .find(|p| !credentials.iter().any(|c| c.user() == p.user))
        {
            return Err(RouterError::MissingCredentials(p.user.clone()));
        }
        if let Some(c) = credentials
            .iter()
            .find(|c| !self.principals.iter().any(|p| p.user == c.user()))
        {
            return Err(RouterError::UnknownUser(c.user().to_owned()));
        }
        let dictionary = credentials::dictionary(credentials)?;
        let secrets_yaml = k8s::secrets(options, &self.principals, credentials, &dictionary)?;
        Ok((dictionary, secrets_yaml))
    }
}

/// The whole deployment: [`plan`] plus the dictionary and Secrets from
/// `credentials`, one per principal ([`principals`]). Pure.
pub fn render_router(
    specs: &[ServiceSpec],
    options: &RouterOptions,
    credentials: &[Credentials],
) -> Result<RouterBundle, RouterError> {
    let plan = plan(specs, options)?;
    let (dictionary, secrets_yaml) = plan.secrets(options, credentials)?;
    Ok(RouterBundle {
        plan,
        dictionary,
        secrets_yaml,
    })
}

/// Why the router could not be rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RouterError {
    /// A spec is invalid.
    Spec(SpecError),
    /// A grant is not a key filter.
    InvalidGrant(String),
    /// Two principals share a username.
    DuplicateUser(String),
    /// An extra grant or a credential names no principal.
    UnknownUser(String),
    /// A principal has no credentials.
    MissingCredentials(String),
    /// A robot name does not give an RFC 1123 username.
    InvalidRobot(String),
    /// Robots are listed but there is no robot route.
    NoRobotRoute,
    /// Credentials are invalid.
    Credentials(CredentialsError),
    /// Serializing the config or manifests failed.
    Render(String),
}

impl fmt::Display for RouterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RouterError::Spec(e) => write!(f, "invalid service spec: {e}"),
            RouterError::InvalidGrant(g) => write!(f, "zenoh grant {g:?} is not a key filter"),
            RouterError::DuplicateUser(u) => write!(f, "zenoh user {u:?} appears twice"),
            RouterError::UnknownUser(u) => write!(f, "zenoh user {u:?} is not a principal"),
            RouterError::MissingCredentials(u) => {
                write!(f, "zenoh user {u:?} has no credentials")
            }
            RouterError::InvalidRobot(n) => write!(f, "robot name {n:?} is not an RFC 1123 label"),
            RouterError::NoRobotRoute => f.write_str("robots are listed but no robot route is set"),
            RouterError::Credentials(e) => write!(f, "{e}"),
            RouterError::Render(e) => write!(f, "rendering the router failed: {e}"),
        }
    }
}

impl std::error::Error for RouterError {}

impl From<SpecError> for RouterError {
    fn from(e: SpecError) -> Self {
        RouterError::Spec(e)
    }
}

impl From<CredentialsError> for RouterError {
    fn from(e: CredentialsError) -> Self {
        RouterError::Credentials(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use credentials::Password;

    fn specs() -> Vec<ServiceSpec> {
        let mut echo = ServiceSpec::new("demo/echo");
        echo.serves.push("demo/echo".into());
        echo.publishes.push("app/demo/**".into());
        let mut caller = ServiceSpec::new("demo/caller");
        caller.calls.push("demo/echo".into());
        let mut gateway = ServiceSpec::new("mw/gateway");
        gateway.publishes.push("app/dev/**".into());
        vec![echo, caller, gateway]
    }

    fn options() -> RouterOptions {
        RouterOptions {
            extra_grants: vec![ExtraGrant {
                user: "mw-gateway".into(),
                grants: Grants {
                    announces: vec!["cam/**".into()],
                    ..Grants::default()
                },
            }],
            robots: vec![Robot {
                name: "r1".into(),
                grants: Grants {
                    publishes: vec!["app/robot/r1/**".into()],
                    ..Grants::default()
                },
            }],
            ..RouterOptions::default()
        }
    }

    fn creds(user: &str) -> Credentials {
        Credentials::new(user, Password::new("0123456789abcdef-pw").unwrap()).unwrap()
    }

    #[test]
    fn z19_principals_are_zenoh_services_and_robots() {
        let p = principals(&specs(), &options()).unwrap();
        let users: Vec<&str> = p.iter().map(|p| p.user.as_str()).collect();
        assert_eq!(
            users,
            ["demo-echo", "mw-gateway", "robot-r1"],
            "caller uses no zenoh"
        );
        assert_eq!(p[0].peer, "infra.local/ns/apps/sa/demo-echo");
        assert_eq!(p[1].grants.announces, ["cam/**"], "extra grant merged");
        assert_eq!(
            p[2].peer, DEFAULT_GATEWAY_PRINCIPAL,
            "robots arrive via the gateway"
        );
        assert_eq!(
            router_locator("apps"),
            "tcp/zenohd.apps.svc.cluster.local:7447"
        );
    }

    #[test]
    fn z19_principals_refuse_inconsistent_options() {
        let mut o = options();
        o.extra_grants[0].user = "nobody".into();
        assert_eq!(
            principals(&specs(), &o),
            Err(RouterError::UnknownUser("nobody".into()))
        );
        let mut o = options();
        o.robot_route = None;
        assert_eq!(principals(&specs(), &o), Err(RouterError::NoRobotRoute));
        let mut o = options();
        o.robots[0].name = "R_1".into();
        assert_eq!(
            principals(&specs(), &o),
            Err(RouterError::InvalidRobot("R_1".into()))
        );
        let mut bad = specs();
        bad[0].replicas = 0;
        assert_eq!(
            principals(&bad, &options()),
            Err(RouterError::Spec(SpecError::ZeroReplicas))
        );
    }

    #[test]
    fn z20_render_router_needs_exactly_one_credential_per_principal() {
        let all = [creds("demo-echo"), creds("mw-gateway"), creds("robot-r1")];
        let b = render_router(&specs(), &options(), &all).unwrap();
        assert_eq!(b.dictionary.lines().count(), 3);
        assert!(b.dictionary.starts_with("demo-echo:"));
        assert!(!b.plan.k8s_manifests_yaml.contains("0123456789abcdef-pw"));
        assert!(!b.plan.config_json5.contains("0123456789abcdef-pw"));
        assert!(b.secrets_yaml.contains("0123456789abcdef-pw"));
        assert_eq!(
            render_router(&specs(), &options(), &all[..2]),
            Err(RouterError::MissingCredentials("robot-r1".into()))
        );
        let extra = [all.clone().to_vec(), vec![creds("stranger")]].concat();
        assert_eq!(
            render_router(&specs(), &options(), &extra),
            Err(RouterError::UnknownUser("stranger".into()))
        );
    }

    #[test]
    fn z19_config_is_deterministic_json5_zenoh_accepts() {
        let a = plan(&specs(), &options()).unwrap();
        let mut reversed = specs();
        reversed.reverse();
        let b = plan(&reversed, &options()).unwrap();
        assert_eq!(
            a.config_json5, b.config_json5,
            "input order does not matter"
        );
        assert!(a.config_json5.starts_with(config::HEADER));
        let parsed = zenoh::Config::from_json5(&a.config_json5).expect("zenoh parses it");
        assert_eq!(parsed.get_json("mode").unwrap(), r#""router""#);
        assert_eq!(parsed.get_json("access_control/enabled").unwrap(), "true");
        assert_eq!(
            parsed
                .get_json("access_control/default_permission")
                .unwrap(),
            r#""deny""#
        );
        let rules = parsed.get_json("access_control/rules").unwrap();
        assert!(rules.contains("demo-echo/publish-in"), "{rules}");
    }
}
