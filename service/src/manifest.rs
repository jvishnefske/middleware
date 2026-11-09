//! Kubernetes manifests rendered from a [`ServiceSpec`] (`--print-manifests`).
//!
//! [`render`] takes the spec plus [`ManifestOptions`] and returns one
//! multi-document YAML stream. Every document is a typed serde value
//! (`manifest/objects.rs`), never a string template. Field order is fixed
//! and label maps are sorted, so the same inputs always give the same bytes.
//! The golden tests in `service/tests/manifest.rs` pin those bytes, and
//! `scripts/check-manifests.sh` validates the goldens offline with
//! kubeconform.
//!
//! # Objects, in stream order
//!
//! `<name>` is [`ServiceSpec::k8s_name`] (`demo/echo` → `demo-echo`). The
//! headless Service adds [`HEADLESS_SUFFIX`], so `<name>` may be at most
//! [`MAX_NAME_LEN`] bytes.
//!
//! 1. **ServiceAccount `<name>`** with `automountServiceAccountToken: false`.
//!    The pod's SPIFFE ID is `spiffe://<trust domain>/ns/<ns>/sa/<name>`.
//! 2. **Service `<name>`**: port [`PORT_NAME`] = the spec's port,
//!    `appProtocol: grpc`, target port `grpc`.
//! 3. **Service `<name>-pods`**: the same port, headless (`clusterIP: None`,
//!    `publishNotReadyAddresses: false`). Cluster DNS answers
//!    `_grpc._tcp.<name>-pods.<ns>.svc.cluster.local` with one SRV per
//!    *ready* pod. That is the name callers' `UnicastDnsSd` asks for
//!    ([`ServiceMap::srv_name`](middleware::dns::unicast::ServiceMap::srv_name)
//!    of the `MW_SERVICE_MAP` entry below), so a gather reaches every pod
//!    and a pod that turns `NOT_SERVING` drops out of the answer.
//! 4. **Deployment `<name>`**:
//!    * `replicas`, and a rolling update with `maxSurge: 1`,
//!      `maxUnavailable: 0`.
//!    * Labels ([`labels`]): `app.kubernetes.io/{name,instance,version,managed-by}`,
//!      plus `app` and `version` for Istio telemetry. The selector uses only
//!      `app.kubernetes.io/{name,instance}`.
//!    * The pod also gets the label `sidecar.istio.io/inject: "true"`.
//!      There is no SPIRE annotation: the lab's istiod injects its `spire`
//!      template into every pod.
//!    * One container, named `<name>`, with the image and `imagePullPolicy`
//!      from the options. Port `grpc` is the spec's port.
//!    * The [`Config`](crate::Config) environment ([`environment`]).
//!    * The spec's resources.
//!    * gRPC startup, readiness and liveness probes on the port, with
//!      `service: ""`, so they check the server's overall
//!      `grpc.health.v1` status.
//!    * `terminationGracePeriodSeconds` = the drain grace, rounded up to
//!      whole seconds, plus [`TERMINATION_MARGIN_S`]
//!      ([`termination_grace_seconds`]).
//!    * A restricted security context: uid and gid [`RUN_AS`], non-root,
//!      read-only root, `allowPrivilegeEscalation: false`, every
//!      capability dropped, seccomp `RuntimeDefault`.
//!    * An in-memory `/tmp`.
//!    * When the spec publishes or subscribes, the Zenoh credentials
//!      Secret (see "Zenoh" below).
//! 5. **PodDisruptionBudget `<name>`** (`minAvailable: 1`), only when
//!    `replicas > 1`. With one replica it would block every node drain.
//! 6. **AuthorizationPolicy `<name>`** (`security.istio.io/v1`, `ALLOW`,
//!    selecting the pods). The rule's `from.source.principals` lists
//!    `allow_callers` in Istio's form, without `spiffe://`
//!    ([`istio_principal`]). An empty list renders no `rules`: an `ALLOW`
//!    policy that matches nothing, which denies every caller.
//! 7. **DestinationRule `<name>` and `<name>-pods`** (`networking.istio.io/v1`),
//!    one for each Service's FQDN, with `tls.mode: ISTIO_MUTUAL`.
//!
//! # Probes and the AuthorizationPolicy
//!
//! The sidecar injector rewrites the gRPC probes (the pod annotation
//! `sidecar.istio.io/rewriteAppHTTPProbers: "true"`, Istio's default, is
//! rendered explicitly). The kubelet then probes pilot-agent on port
//! 15020, and pilot-agent makes the gRPC health call to the application
//! over the pod's loopback. Neither hop passes Envoy's inbound listener,
//! so the AuthorizationPolicy never sees them: probes work even when the
//! policy admits no caller, and the policy needs no rule for them.
//! `tests/manifest.rs` pins both the annotation and the rule.
//!
//! # Drain and the sidecar
//!
//! The pod annotation `proxy.istio.io/config` carries a YAML `ProxyConfig`:
//! * `holdApplicationUntilProxyStarts: true`: the application starts only
//!   once Envoy can carry its outbound calls (to OTLP, Zenoh and callees).
//! * `terminationDrainDuration` = the drain grace rounded up plus
//!   [`PROXY_DRAIN_MARGIN_S`] ([`proxy_drain_seconds`]).
//! * `proxyMetadata.EXIT_ON_ZERO_ACTIVE_CONNECTIONS: "true"` and
//!   `MINIMUM_DRAIN_DURATION` set to the same value.
//!
//! With classic sidecars, Envoy and the application both get SIGTERM. The
//! application keeps serving for its drain grace and then exits; Envoy
//! outlives it by the margin. The application still has a route for its
//! last trace export, and Envoy exits as soon as its connections close.
//! Both fit inside `terminationGracePeriodSeconds`. Istio 1.27 and later
//! run the sidecar as a Kubernetes native sidecar on clusters that support
//! it. A native sidecar stops only after the application container. There
//! the hold is implicit and the drain settings are harmless.
//!
//! # Callees: `MW_SERVICE_MAP`
//!
//! Each `calls` kind maps to `<callee>-pods.<ns>:grpc`. By default the
//! callee is named after the kind, through the same convention as
//! `k8s_name` (`demo/echo` → Service `demo-echo` in the rendering
//! namespace): a service that serves the kind it is named after is found
//! with no configuration. A [`Route`] (`--route KIND=SERVICE[.NS]`)
//! overrides the callee for one kind.
//!
//! # Zenoh
//!
//! A spec that publishes or subscribes gets two variables:
//! * `MW_ZENOH_ROUTER`: the option value, or
//!   `tcp/zenohd.<ns>.svc.cluster.local:7447`.
//! * `MW_ZENOH_CREDENTIALS` = [`ZENOH_CREDENTIALS_PATH`].
//!
//! It also gets the Secret [`zenoh_secret_name`] (`zenoh-<name>`, not
//! rendered here: it holds a credential), mounted read-only at
//! [`ZENOH_CREDENTIALS_DIR`]. Its key [`ZENOH_CREDENTIALS_KEY`] becomes the
//! file `credentials`, mode 0440 and group [`RUN_AS`]. The pod does not
//! start without the Secret.
//!
//! # Checks
//!
//! [`render`] fails with an error, never a broken manifest, in these cases:
//! * The spec is invalid ([`ServiceError::Spec`]).
//! * The options are invalid ([`ServiceError::Manifest`], [`ManifestError`]).
//! * The rendered environment does not parse with
//!   [`Config::from_env`](crate::Config::from_env) or leaves a called kind
//!   unmapped ([`ServiceError::Config`], [`ServiceError::Unmapped`]). So
//!   what the manifests set is exactly what the runner accepts.

mod objects;

use std::fmt;

use middleware::grpc::istio::{CLUSTER_DOMAIN, is_dns_label, is_spiffe_id};
use serde::Serialize;

use crate::config::{
    Config, ENV_NAMESPACE, ENV_OTLP_ENDPOINT, ENV_POD_NAME, ENV_PORT, ENV_SERVICE_MAP,
    ENV_ZENOH_CREDENTIALS, ENV_ZENOH_ROUTER,
};
use crate::error::ServiceError;
use crate::run::DEFAULT_NAMESPACE;
use crate::spec::{ServiceSpec, SpecError, k8s_name};
use objects::*;

/// Labels or annotations: a sorted map, so rendering is deterministic.
pub use objects::Map;

/// The OTLP/gRPC endpoint unless [`ManifestOptions::otlp_endpoint`] is set:
/// the lab's Alloy collector, which forwards to Tempo.
pub const DEFAULT_OTLP_ENDPOINT: &str =
    "http://k8s-monitoring-alloy.observability.svc.cluster.local:4317";
/// The Zenoh router's Service name, in the rendering namespace, unless
/// [`ManifestOptions::zenoh_router`] is set.
pub const ZENOH_ROUTER_SERVICE: &str = "zenohd";
/// The Zenoh router's port.
pub const ZENOH_ROUTER_PORT: u16 = 7447;
/// The Service and container port name; the SRV `_grpc` label.
pub const PORT_NAME: &str = "grpc";
/// Appended to `<name>` for the headless Service.
pub const HEADLESS_SUFFIX: &str = "-pods";
/// The longest `<name>` that still leaves room for [`HEADLESS_SUFFIX`] in a
/// 63-byte label.
pub const MAX_NAME_LEN: usize = 63 - HEADLESS_SUFFIX.len();
/// Prefix of the Zenoh credentials Secret's name.
pub const ZENOH_SECRET_PREFIX: &str = "zenoh-";
/// Where the Zenoh credentials Secret is mounted.
pub const ZENOH_CREDENTIALS_DIR: &str = "/var/run/mw/zenoh";
/// The Secret key (and the file name) holding the Zenoh credentials.
pub const ZENOH_CREDENTIALS_KEY: &str = "credentials";
/// `MW_ZENOH_CREDENTIALS`: the mounted credentials file.
pub const ZENOH_CREDENTIALS_PATH: &str = "/var/run/mw/zenoh/credentials";
/// The uid and gid the container runs as (the distroless `nonroot` user).
pub const RUN_AS: u32 = 65532;
/// `terminationGracePeriodSeconds` minus the rounded-up drain grace.
pub const TERMINATION_MARGIN_S: u32 = 10;
/// The sidecar's drain minus the rounded-up drain grace.
pub const PROXY_DRAIN_MARGIN_S: u32 = 2;
/// `app.kubernetes.io/managed-by`.
pub const MANAGED_BY: &str = "mw-service";
/// `app.kubernetes.io/version` when neither the options nor the image tag
/// give a valid label value.
pub const UNVERSIONED: &str = "unversioned";
/// The liveness probe period, seconds.
pub const LIVENESS_PERIOD_S: u32 = 10;

/// `imagePullPolicy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PullPolicy {
    /// `Always`.
    Always,
    /// `IfNotPresent` (the default).
    #[default]
    IfNotPresent,
    /// `Never`: the image must already be on the node (`k3s ctr images
    /// import`), as in the lab.
    Never,
}

impl PullPolicy {
    /// The Kubernetes spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            PullPolicy::Always => "Always",
            PullPolicy::IfNotPresent => "IfNotPresent",
            PullPolicy::Never => "Never",
        }
    }

    /// Parses the Kubernetes spelling. Pure.
    pub fn parse(text: &str) -> Option<PullPolicy> {
        [
            PullPolicy::Always,
            PullPolicy::IfNotPresent,
            PullPolicy::Never,
        ]
        .into_iter()
        .find(|p| p.as_str() == text)
    }
}

/// Which Kubernetes Service serves a called kind, overriding the default
/// named after the kind (module docs, "Callees").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    /// The called kind (`Service/Method`).
    pub kind: String,
    /// The callee's `<name>`. Its headless Service `<name>-pods` is the
    /// target.
    pub service: String,
    /// The callee's namespace; `None` is the rendering namespace.
    pub namespace: Option<String>,
}

impl Route {
    /// Parses `KIND=SERVICE` or `KIND=SERVICE.NS` (the `--route` value).
    /// Only the shape is checked here; [`render`] validates the names. Pure.
    pub fn parse(text: &str) -> Option<Route> {
        let (kind, target) = text.split_once('=')?;
        let (service, namespace) = match target.split_once('.') {
            Some((s, ns)) => (s, Some(ns.to_owned())),
            None => (target, None),
        };
        (!kind.is_empty() && !service.is_empty()).then(|| Route {
            kind: kind.to_owned(),
            service: service.to_owned(),
            namespace,
        })
    }
}

/// What the manifests need beyond the spec. [`ManifestOptions::new`] gives
/// the lab defaults for a namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestOptions {
    /// The namespace the objects are rendered for.
    pub namespace: String,
    /// The container image reference; `None` is `<name>:latest`.
    pub image: Option<String>,
    /// `imagePullPolicy` (`--pull-policy`).
    pub pull_policy: PullPolicy,
    /// `OTEL_EXPORTER_OTLP_ENDPOINT` (`--otlp`); `None` is
    /// [`DEFAULT_OTLP_ENDPOINT`].
    pub otlp_endpoint: Option<String>,
    /// `MW_ZENOH_ROUTER` (`--zenoh-router`); `None` is
    /// `tcp/zenohd.<ns>.svc.cluster.local:7447`.
    pub zenoh_router: Option<String>,
    /// `app.kubernetes.io/version` and `version` (`--app-version`); `None`
    /// is the image tag, or [`UNVERSIONED`].
    pub app_version: Option<String>,
    /// Callee overrides (`--route`, repeatable), at most one per kind.
    pub routes: Vec<Route>,
}

impl ManifestOptions {
    /// The defaults for `namespace`: derived image, `IfNotPresent`, the
    /// lab's OTLP collector and Zenoh router, no routes.
    pub fn new(namespace: impl Into<String>) -> Self {
        ManifestOptions {
            namespace: namespace.into(),
            image: None,
            pull_policy: PullPolicy::default(),
            otlp_endpoint: None,
            zenoh_router: None,
            app_version: None,
            routes: Vec::new(),
        }
    }
}

impl Default for ManifestOptions {
    /// [`ManifestOptions::new`] for [`DEFAULT_NAMESPACE`].
    fn default() -> Self {
        Self::new(DEFAULT_NAMESPACE)
    }
}

/// Why the options cannot be rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ManifestError {
    /// A `<name>` (the service's or a default callee's) is longer than
    /// [`MAX_NAME_LEN`].
    NameTooLong(String),
    /// The namespace is not an RFC 1123 label.
    InvalidNamespace(String),
    /// The image reference is empty or contains whitespace.
    InvalidImage(String),
    /// `--app-version` is not a Kubernetes label value.
    InvalidVersion(String),
    /// A route's kind is not one the spec calls.
    RouteNotCalled(String),
    /// A route's service or namespace is not an RFC 1123 label, or its
    /// service is longer than [`MAX_NAME_LEN`].
    InvalidRoute(String),
    /// Two routes for one kind.
    DuplicateRoute(String),
    /// serde could not serialize an object.
    Serialize(String),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManifestError::NameTooLong(n) => write!(
                f,
                "Kubernetes name {n:?} is longer than {MAX_NAME_LEN} bytes (room for {HEADLESS_SUFFIX:?})"
            ),
            ManifestError::InvalidNamespace(n) => {
                write!(f, "namespace {n:?} is not an RFC 1123 label")
            }
            ManifestError::InvalidImage(i) => write!(f, "image reference {i:?} is malformed"),
            ManifestError::InvalidVersion(v) => {
                write!(f, "version {v:?} is not a Kubernetes label value")
            }
            ManifestError::RouteNotCalled(k) => {
                write!(f, "route for {k:?}, which the service does not call")
            }
            ManifestError::InvalidRoute(r) => write!(
                f,
                "route target {r:?} is not SERVICE[.NAMESPACE] of RFC 1123 labels"
            ),
            ManifestError::DuplicateRoute(k) => write!(f, "two routes for {k:?}"),
            ManifestError::Serialize(e) => write!(f, "serializing a manifest failed: {e}"),
        }
    }
}

impl std::error::Error for ManifestError {}

impl From<ManifestError> for ServiceError {
    fn from(e: ManifestError) -> Self {
        ServiceError::Manifest(e)
    }
}

/// The Istio `principals` form of a SPIFFE ID: `spiffe://td/ns/a/sa/b` →
/// `td/ns/a/sa/b`. `None` when `id` is not a SPIFFE ID. Pure.
pub fn istio_principal(id: &str) -> Option<&str> {
    is_spiffe_id(id)
        .then(|| id.strip_prefix("spiffe://"))
        .flatten()
}

/// The headless Service of `<name>`: `<name>-pods`. Pure.
pub fn headless_name(name: &str) -> String {
    format!("{name}{HEADLESS_SUFFIX}")
}

/// The Zenoh credentials Secret of `<name>`: `zenoh-<name>`. Pure.
pub fn zenoh_secret_name(name: &str) -> String {
    format!("{ZENOH_SECRET_PREFIX}{name}")
}

/// `terminationGracePeriodSeconds`: the drain grace rounded up to whole
/// seconds, plus [`TERMINATION_MARGIN_S`]. Pure.
pub fn termination_grace_seconds(spec: &ServiceSpec) -> u32 {
    drain_seconds(spec) + TERMINATION_MARGIN_S
}

/// The sidecar's `terminationDrainDuration` and `MINIMUM_DRAIN_DURATION`:
/// the drain grace rounded up, plus [`PROXY_DRAIN_MARGIN_S`]. Always below
/// [`termination_grace_seconds`]. Pure.
pub fn proxy_drain_seconds(spec: &ServiceSpec) -> u32 {
    drain_seconds(spec) + PROXY_DRAIN_MARGIN_S
}

/// The liveness failure threshold: enough periods that a pod reporting
/// `NOT_SERVING` while it drains is never killed for it, and at least 3.
/// Pure.
pub fn liveness_failure_threshold(spec: &ServiceSpec) -> u32 {
    (termination_grace_seconds(spec).div_ceil(LIVENESS_PERIOD_S) + 1).max(3)
}

fn drain_seconds(spec: &ServiceSpec) -> u32 {
    spec.drain_grace_ms.div_ceil(1000)
}

/// Whether `s` is a Kubernetes label value: at most 63 bytes of ASCII
/// alphanumerics, `-`, `_` and `.`, beginning and ending alphanumeric
/// (empty is allowed by Kubernetes but refused here). Pure.
pub fn is_label_value(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=63).contains(&b.len())
        && b.first().is_some_and(u8::is_ascii_alphanumeric)
        && b.last().is_some_and(u8::is_ascii_alphanumeric)
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
}

/// The tag of an image reference (`reg:5000/a/b:tag` → `tag`), if it has
/// one and no digest. Pure.
pub fn image_tag(image: &str) -> Option<&str> {
    if image.contains('@') {
        return None;
    }
    let last = image.rsplit('/').next()?;
    last.rsplit_once(':').map(|(_, tag)| tag)
}

/// The labels every object carries: `app.kubernetes.io/{name,instance,
/// version,managed-by}`, `app` and `version`. Pure.
pub fn labels(name: &str, version: &str) -> Map {
    let mut m = selector_labels(name);
    m.insert("app.kubernetes.io/version".into(), version.into());
    m.insert("app.kubernetes.io/managed-by".into(), MANAGED_BY.into());
    m.insert("app".into(), name.into());
    m.insert("version".into(), version.into());
    m
}

/// The labels selecting the pods (stable across versions). Pure.
pub fn selector_labels(name: &str) -> Map {
    Map::from([
        ("app.kubernetes.io/name".to_owned(), name.to_owned()),
        ("app.kubernetes.io/instance".to_owned(), name.to_owned()),
    ])
}

/// The `MW_SERVICE_MAP` targets: each called kind with the callee's
/// headless Service and namespace, as `(kind, service, namespace)` in
/// `calls` order (module docs, "Callees"). Pure.
pub fn callees(
    spec: &ServiceSpec,
    options: &ManifestOptions,
) -> Result<Vec<(String, String, String)>, ManifestError> {
    for (i, route) in options.routes.iter().enumerate() {
        if !spec.calls.contains(&route.kind) {
            return Err(ManifestError::RouteNotCalled(route.kind.clone()));
        }
        if options
            .routes
            .get(..i)
            .is_some_and(|before| before.iter().any(|r| r.kind == route.kind))
        {
            return Err(ManifestError::DuplicateRoute(route.kind.clone()));
        }
        let ns_ok = route.namespace.as_deref().is_none_or(is_dns_label);
        if !ns_ok || !is_dns_label(&route.service) || route.service.len() > MAX_NAME_LEN {
            return Err(ManifestError::InvalidRoute(match &route.namespace {
                Some(ns) => format!("{}.{ns}", route.service),
                None => route.service.clone(),
            }));
        }
    }
    spec.calls
        .iter()
        .map(|kind| {
            let route = options.routes.iter().find(|r| &r.kind == kind);
            let service = match route {
                Some(r) => r.service.clone(),
                None => {
                    let n =
                        k8s_name(kind).ok_or_else(|| ManifestError::NameTooLong(kind.clone()))?;
                    if n.len() > MAX_NAME_LEN {
                        return Err(ManifestError::NameTooLong(n));
                    }
                    n
                }
            };
            let namespace = route
                .and_then(|r| r.namespace.clone())
                .unwrap_or_else(|| options.namespace.clone());
            Ok((kind.clone(), headless_name(&service), namespace))
        })
        .collect()
}

/// The literal environment of the container, in order, as `(name, value)`;
/// `POD_NAMESPACE` and `POD_NAME` come from the downward API and are not
/// listed. Pure.
pub fn environment(
    spec: &ServiceSpec,
    options: &ManifestOptions,
) -> Result<Vec<(&'static str, String)>, ManifestError> {
    let mut env = vec![
        (ENV_PORT, spec.port.to_string()),
        (
            ENV_OTLP_ENDPOINT,
            options
                .otlp_endpoint
                .clone()
                .unwrap_or_else(|| DEFAULT_OTLP_ENDPOINT.to_owned()),
        ),
    ];
    let map = callees(spec, options)?
        .into_iter()
        .map(|(kind, service, ns)| format!("{kind}={service}.{ns}:{PORT_NAME}"))
        .collect::<Vec<_>>();
    if !map.is_empty() {
        env.push((ENV_SERVICE_MAP, map.join(",")));
    }
    if spec.uses_zenoh() {
        let router = options.zenoh_router.clone().unwrap_or_else(|| {
            format!(
                "tcp/{ZENOH_ROUTER_SERVICE}.{}.{CLUSTER_DOMAIN}:{ZENOH_ROUTER_PORT}",
                options.namespace
            )
        });
        env.push((ENV_ZENOH_ROUTER, router));
        env.push((ENV_ZENOH_CREDENTIALS, ZENOH_CREDENTIALS_PATH.to_owned()));
    }
    Ok(env)
}

/// The manifests of `spec` as one YAML stream, each document starting with
/// `---` (module docs).
pub fn render(spec: &ServiceSpec, options: &ManifestOptions) -> Result<String, ServiceError> {
    spec.validate()?;
    let name = spec
        .k8s_name()
        .ok_or_else(|| SpecError::InvalidName(spec.name.clone()))?;
    if name.len() > MAX_NAME_LEN {
        return Err(ManifestError::NameTooLong(name).into());
    }
    let ns = &options.namespace;
    if !is_dns_label(ns) {
        return Err(ManifestError::InvalidNamespace(ns.clone()).into());
    }
    let image = options
        .image
        .clone()
        .unwrap_or_else(|| format!("{name}:latest"));
    if image.is_empty() || image.contains(char::is_whitespace) {
        return Err(ManifestError::InvalidImage(image).into());
    }
    let version = match &options.app_version {
        Some(v) if is_label_value(v) => v.clone(),
        Some(v) => return Err(ManifestError::InvalidVersion(v.clone()).into()),
        None => image_tag(&image)
            .filter(|t| is_label_value(t))
            .unwrap_or(UNVERSIONED)
            .to_owned(),
    };
    let env = environment(spec, options)?;
    check_environment(spec, &name, ns, &env)?;

    let mut out = String::new();
    push_doc(&mut out, &yaml(&service_account(&name, ns, &version))?);
    push_doc(
        &mut out,
        &yaml(&service(&name, ns, &version, spec.port, false))?,
    );
    push_doc(
        &mut out,
        &yaml(&service(
            &headless_name(&name),
            ns,
            &version,
            spec.port,
            true,
        ))?,
    );
    let pod = PodInputs {
        name: &name,
        ns,
        version: &version,
        image,
        pull_policy: options.pull_policy,
        env,
    };
    push_doc(&mut out, &yaml(&deployment(spec, pod)?)?);
    if spec.replicas > 1 {
        push_doc(&mut out, &yaml(&pdb(&name, ns, &version))?);
    }
    push_doc(
        &mut out,
        &yaml(&authorization_policy(spec, &name, ns, &version))?,
    );
    for host in [name.clone(), headless_name(&name)] {
        push_doc(&mut out, &yaml(&destination_rule(&host, ns, &version))?);
    }
    Ok(out)
}

/// Runs the rendered environment through the runner's own parser.
fn check_environment(
    spec: &ServiceSpec,
    name: &str,
    ns: &str,
    env: &[(&'static str, String)],
) -> Result<(), ServiceError> {
    let pod = format!("{name}-0");
    let config = Config::from_env(&|k| match k {
        _ if k == ENV_NAMESPACE => Some(ns.to_owned()),
        _ if k == ENV_POD_NAME => Some(pod.clone()),
        _ => env.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone()),
    })?;
    match config.unmapped_calls(spec).first() {
        Some(kind) => Err(ServiceError::Unmapped((*kind).to_owned())),
        None => Ok(()),
    }
}

fn push_doc(out: &mut String, doc: &str) {
    out.push_str("---\n");
    out.push_str(doc);
}

fn yaml(value: &impl Serialize) -> Result<String, ManifestError> {
    serde_norway::to_string(value).map_err(|e| ManifestError::Serialize(e.to_string()))
}

/// Every object of one service carries that service's labels, the `-pods`
/// Service and DestinationRule included.
fn owner_name(object_name: &str) -> &str {
    object_name
        .strip_suffix(HEADLESS_SUFFIX)
        .unwrap_or(object_name)
}

fn metadata(object: &str, ns: &str, version: &str) -> Metadata {
    Metadata {
        name: object.to_owned(),
        namespace: Some(ns.to_owned()),
        labels: labels(owner_name(object), version),
        annotations: Map::new(),
    }
}

fn service_account(name: &str, ns: &str, version: &str) -> Object<ServiceAccountBody> {
    Object {
        api_version: "v1",
        kind: "ServiceAccount",
        metadata: metadata(name, ns, version),
        body: ServiceAccountBody {
            automount_service_account_token: false,
        },
    }
}

fn service(
    object: &str,
    ns: &str,
    version: &str,
    port: u16,
    headless: bool,
) -> Object<ServiceBody> {
    Object {
        api_version: "v1",
        kind: "Service",
        metadata: metadata(object, ns, version),
        body: ServiceBody {
            spec: ServiceSpecK8s {
                cluster_ip: headless.then_some("None"),
                publish_not_ready_addresses: headless.then_some(false),
                selector: selector_labels(owner_name(object)),
                ports: vec![ServicePort {
                    name: PORT_NAME,
                    app_protocol: "grpc",
                    protocol: "TCP",
                    port,
                    target_port: PORT_NAME,
                }],
            },
        },
    }
}

fn grpc_probe(port: u16, period: u32, timeout: u32, failures: u32) -> Probe {
    Probe {
        grpc: GrpcAction { port, service: "" },
        period_seconds: period,
        timeout_seconds: timeout,
        failure_threshold: failures,
    }
}

fn runtime_default() -> SeccompProfile {
    SeccompProfile {
        kind: "RuntimeDefault",
    }
}

fn proxy_config(spec: &ServiceSpec) -> ProxyConfig {
    let drain = format!("{}s", proxy_drain_seconds(spec));
    ProxyConfig {
        hold_application_until_proxy_starts: true,
        termination_drain_duration: drain.clone(),
        proxy_metadata: ProxyMetadata {
            exit_on_zero_active_connections: "true",
            minimum_drain_duration: drain,
        },
    }
}

/// What the pod template needs beyond the spec.
struct PodInputs<'a> {
    name: &'a str,
    ns: &'a str,
    version: &'a str,
    image: String,
    pull_policy: PullPolicy,
    env: Vec<(&'static str, String)>,
}

fn deployment(
    spec: &ServiceSpec,
    pod: PodInputs<'_>,
) -> Result<Object<DeploymentBody>, ManifestError> {
    let PodInputs {
        name,
        ns,
        version,
        image,
        pull_policy,
        env: literal_env,
    } = pod;
    let field = |name, path| EnvVar {
        name,
        value: None,
        value_from: Some(EnvSource {
            field_ref: FieldRef { field_path: path },
        }),
    };
    let mut literal = literal_env.into_iter().map(|(name, value)| EnvVar {
        name,
        value: Some(value),
        value_from: None,
    });
    // MW_PORT first, then the pod identity, then the rest in table order.
    let mut env: Vec<EnvVar> = literal.by_ref().take(1).collect();
    env.push(field(ENV_NAMESPACE, "metadata.namespace"));
    env.push(field(ENV_POD_NAME, "metadata.name"));
    env.extend(literal);

    let mut volumes = vec![Volume {
        name: "tmp",
        empty_dir: Some(EmptyDir {
            medium: "Memory",
            size_limit: "16Mi",
        }),
        secret: None,
    }];
    let mut mounts = vec![VolumeMount {
        name: "tmp",
        mount_path: "/tmp",
        read_only: false,
    }];
    if spec.uses_zenoh() {
        volumes.push(Volume {
            name: "zenoh-credentials",
            empty_dir: None,
            secret: Some(SecretVolume {
                secret_name: zenoh_secret_name(name),
                default_mode: 0o440,
                optional: false,
                items: vec![KeyToPath {
                    key: ZENOH_CREDENTIALS_KEY,
                    path: ZENOH_CREDENTIALS_KEY,
                }],
            }),
        });
        mounts.push(VolumeMount {
            name: "zenoh-credentials",
            mount_path: ZENOH_CREDENTIALS_DIR,
            read_only: true,
        });
    }

    let mut pod_labels = labels(name, version);
    pod_labels.insert("sidecar.istio.io/inject".into(), "true".into());
    let annotations = Map::from([
        (
            "proxy.istio.io/config".to_owned(),
            yaml(&proxy_config(spec))?,
        ),
        (
            "sidecar.istio.io/rewriteAppHTTPProbers".to_owned(),
            "true".to_owned(),
        ),
        (
            "kubectl.kubernetes.io/default-container".to_owned(),
            name.to_owned(),
        ),
    ]);
    let r = spec.resources;
    Ok(Object {
        api_version: "apps/v1",
        kind: "Deployment",
        metadata: metadata(name, ns, version),
        body: DeploymentBody {
            spec: DeploymentSpec {
                replicas: spec.replicas,
                selector: Selector {
                    match_labels: selector_labels(name),
                },
                strategy: Strategy {
                    kind: "RollingUpdate",
                    rolling_update: RollingUpdate {
                        max_surge: 1,
                        max_unavailable: 0,
                    },
                },
                template: PodTemplate {
                    metadata: TemplateMetadata {
                        labels: pod_labels,
                        annotations,
                    },
                    spec: PodSpec {
                        service_account_name: name.to_owned(),
                        automount_service_account_token: false,
                        enable_service_links: false,
                        termination_grace_period_seconds: termination_grace_seconds(spec),
                        security_context: PodSecurityContext {
                            run_as_non_root: true,
                            run_as_user: RUN_AS,
                            run_as_group: RUN_AS,
                            fs_group: RUN_AS,
                            seccomp_profile: runtime_default(),
                        },
                        containers: vec![Container {
                            name: name.to_owned(),
                            image,
                            image_pull_policy: pull_policy.as_str(),
                            ports: vec![ContainerPort {
                                name: PORT_NAME,
                                container_port: spec.port,
                                protocol: "TCP",
                            }],
                            env,
                            resources: ResourceRequirements {
                                requests: Quantities {
                                    cpu: r.cpu_request.to_string(),
                                    memory: r.memory_request.to_string(),
                                },
                                limits: Quantities {
                                    cpu: r.cpu_limit.to_string(),
                                    memory: r.memory_limit.to_string(),
                                },
                            },
                            startup_probe: grpc_probe(spec.port, 1, 1, 60),
                            readiness_probe: grpc_probe(spec.port, 2, 1, 1),
                            liveness_probe: grpc_probe(
                                spec.port,
                                LIVENESS_PERIOD_S,
                                2,
                                liveness_failure_threshold(spec),
                            ),
                            security_context: ContainerSecurityContext {
                                allow_privilege_escalation: false,
                                read_only_root_filesystem: true,
                                run_as_non_root: true,
                                run_as_user: RUN_AS,
                                run_as_group: RUN_AS,
                                capabilities: Capabilities { drop: vec!["ALL"] },
                                seccomp_profile: runtime_default(),
                            },
                            volume_mounts: mounts,
                        }],
                        volumes,
                    },
                },
            },
        },
    })
}

fn pdb(name: &str, ns: &str, version: &str) -> Object<PdbBody> {
    Object {
        api_version: "policy/v1",
        kind: "PodDisruptionBudget",
        metadata: metadata(name, ns, version),
        body: PdbBody {
            spec: PdbSpec {
                min_available: 1,
                selector: Selector {
                    match_labels: selector_labels(name),
                },
            },
        },
    }
}

fn authorization_policy(
    spec: &ServiceSpec,
    name: &str,
    ns: &str,
    version: &str,
) -> Object<AuthorizationPolicyBody> {
    let principals: Vec<String> = spec
        .allow_callers
        .iter()
        .filter_map(|id| istio_principal(id))
        .map(str::to_owned)
        .collect();
    let rules = if principals.is_empty() {
        Vec::new()
    } else {
        vec![Rule {
            from: vec![RuleFrom {
                source: Source { principals },
            }],
        }]
    };
    Object {
        api_version: "security.istio.io/v1",
        kind: "AuthorizationPolicy",
        metadata: metadata(name, ns, version),
        body: AuthorizationPolicyBody {
            spec: AuthorizationPolicySpec {
                selector: Selector {
                    match_labels: selector_labels(name),
                },
                action: "ALLOW",
                rules,
            },
        },
    }
}

fn destination_rule(host: &str, ns: &str, version: &str) -> Object<DestinationRuleBody> {
    Object {
        api_version: "networking.istio.io/v1",
        kind: "DestinationRule",
        metadata: metadata(host, ns, version),
        body: DestinationRuleBody {
            spec: DestinationRuleSpec {
                host: format!("{host}.{ns}.{CLUSTER_DOMAIN}"),
                traffic_policy: TrafficPolicy {
                    tls: Tls {
                        mode: "ISTIO_MUTUAL",
                    },
                },
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m11_pure_helpers() {
        assert_eq!(
            istio_principal("spiffe://infra.local/ns/apps/sa/demo-caller"),
            Some("infra.local/ns/apps/sa/demo-caller")
        );
        assert_eq!(istio_principal("infra.local/ns/apps/sa/x"), None);
        assert_eq!(PullPolicy::parse("Never"), Some(PullPolicy::Never));
        assert_eq!(PullPolicy::parse("never"), None);
        assert_eq!(PullPolicy::default().as_str(), "IfNotPresent");
        assert_eq!(image_tag("reg:5000/a/b:v1.2"), Some("v1.2"));
        assert_eq!(image_tag("reg:5000/a/b"), None);
        assert_eq!(image_tag("b@sha256:00"), None);
        assert_eq!(
            Route::parse("demo/echo=echo.robots"),
            Some(Route {
                kind: "demo/echo".into(),
                service: "echo".into(),
                namespace: Some("robots".into())
            })
        );
        assert_eq!(Route::parse("demo/echo"), None);
        assert_eq!(Route::parse("=x"), None);
        let mut s = ServiceSpec::new("a/b");
        assert_eq!(
            (termination_grace_seconds(&s), proxy_drain_seconds(&s)),
            (15, 7)
        );
        assert_eq!(liveness_failure_threshold(&s), 3);
        s.drain_grace_ms = 600_000;
        assert_eq!(termination_grace_seconds(&s), 610);
        assert!(liveness_failure_threshold(&s) * LIVENESS_PERIOD_S > 610);
        s.drain_grace_ms = 1;
        assert_eq!(proxy_drain_seconds(&s), 3);
    }
}
