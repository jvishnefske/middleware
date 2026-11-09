//! The typed subset of the Kubernetes and Istio object schemas that
//! [`render`](super::render) emits. Plain serde data: field order is
//! serialization order, `None` and empty lists are omitted, and label and
//! annotation maps are `BTreeMap`s (sorted keys), so the YAML is
//! deterministic.
//!
//! Field names follow the upstream schemas (`camelCase`); the vendored
//! JSON schemas under `deploy/schemas/` (kubeconform, `-strict`) reject a
//! misspelt one.

use serde::Serialize;
use std::collections::BTreeMap;

/// Labels or annotations.
pub type Map = BTreeMap<String, String>;

/// `metadata`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Metadata {
    /// Object name.
    pub name: String,
    /// Namespace; `None` inside a pod template.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// Labels.
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub labels: Map,
    /// Annotations.
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub annotations: Map,
}

/// One document of the stream: `apiVersion`, `kind`, `metadata`, then the
/// kind's body (flattened).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Object<B> {
    /// `apiVersion`.
    pub api_version: &'static str,
    /// `kind`.
    pub kind: &'static str,
    /// `metadata`.
    pub metadata: Metadata,
    /// The rest.
    #[serde(flatten)]
    pub body: B,
}

/// ServiceAccount body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceAccountBody {
    /// Always `false`: the service never talks to the API server.
    pub automount_service_account_token: bool,
}

/// Service body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServiceBody {
    /// `spec`.
    pub spec: ServiceSpecK8s,
}

/// Service `spec`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceSpecK8s {
    /// `None` for headless.
    #[serde(rename = "clusterIP", skip_serializing_if = "Option::is_none")]
    pub cluster_ip: Option<&'static str>,
    /// Set on the headless Service only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publish_not_ready_addresses: Option<bool>,
    /// Pod selector.
    pub selector: Map,
    /// Ports.
    pub ports: Vec<ServicePort>,
}

/// A Service port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServicePort {
    /// Port name (the SRV `_<name>` label).
    pub name: &'static str,
    /// `grpc`: Istio treats the port as HTTP/2 gRPC.
    pub app_protocol: &'static str,
    /// `TCP`.
    pub protocol: &'static str,
    /// Service port.
    pub port: u16,
    /// The container port's name.
    pub target_port: &'static str,
}

/// Deployment body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeploymentBody {
    /// `spec`.
    pub spec: DeploymentSpec,
}

/// Deployment `spec`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentSpec {
    /// Pod count.
    pub replicas: u16,
    /// Selector.
    pub selector: Selector,
    /// Rollout strategy.
    pub strategy: Strategy,
    /// Pod template.
    pub template: PodTemplate,
}

/// `selector: {matchLabels}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Selector {
    /// Labels to match.
    pub match_labels: Map,
}

/// Deployment `strategy`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Strategy {
    /// `RollingUpdate`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// Its parameters.
    pub rolling_update: RollingUpdate,
}

/// `rollingUpdate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RollingUpdate {
    /// New pods above `replicas` during a rollout.
    pub max_surge: u16,
    /// Pods below `replicas` during a rollout.
    pub max_unavailable: u16,
}

/// Pod template.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PodTemplate {
    /// Pod metadata (no name or namespace are rendered: `name` is skipped).
    pub metadata: TemplateMetadata,
    /// Pod spec.
    pub spec: PodSpec,
}

/// Pod template metadata: labels and annotations only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TemplateMetadata {
    /// Labels.
    pub labels: Map,
    /// Annotations.
    pub annotations: Map,
}

/// Pod spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PodSpec {
    /// The ServiceAccount (and so the SPIFFE ID).
    pub service_account_name: String,
    /// `false`.
    pub automount_service_account_token: bool,
    /// `false`: no `*_SERVICE_HOST` variables.
    pub enable_service_links: bool,
    /// Drain grace plus margin, seconds.
    pub termination_grace_period_seconds: u32,
    /// Pod security context.
    pub security_context: PodSecurityContext,
    /// The one application container.
    pub containers: Vec<Container>,
    /// Volumes.
    pub volumes: Vec<Volume>,
}

/// Pod `securityContext`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PodSecurityContext {
    /// `true`.
    pub run_as_non_root: bool,
    /// UID.
    pub run_as_user: u32,
    /// GID.
    pub run_as_group: u32,
    /// Group owning mounted volumes.
    pub fs_group: u32,
    /// `RuntimeDefault`.
    pub seccomp_profile: SeccompProfile,
}

/// `seccompProfile`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SeccompProfile {
    /// `RuntimeDefault`.
    #[serde(rename = "type")]
    pub kind: &'static str,
}

/// A container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Container {
    /// Name.
    pub name: String,
    /// Image reference.
    pub image: String,
    /// `Always`, `IfNotPresent` or `Never`.
    pub image_pull_policy: &'static str,
    /// Ports.
    pub ports: Vec<ContainerPort>,
    /// Environment.
    pub env: Vec<EnvVar>,
    /// Resources.
    pub resources: ResourceRequirements,
    /// Startup probe.
    pub startup_probe: Probe,
    /// Readiness probe.
    pub readiness_probe: Probe,
    /// Liveness probe.
    pub liveness_probe: Probe,
    /// Container security context.
    pub security_context: ContainerSecurityContext,
    /// Mounts.
    pub volume_mounts: Vec<VolumeMount>,
}

/// A container port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerPort {
    /// `grpc`.
    pub name: &'static str,
    /// Port.
    pub container_port: u16,
    /// `TCP`.
    pub protocol: &'static str,
}

/// An environment variable: a literal or a downward-API field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvVar {
    /// Name.
    pub name: &'static str,
    /// Literal value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Downward-API source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value_from: Option<EnvSource>,
}

/// `valueFrom`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvSource {
    /// `fieldRef`.
    pub field_ref: FieldRef,
}

/// `fieldRef`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FieldRef {
    /// `metadata.namespace` or `metadata.name`.
    pub field_path: &'static str,
}

/// `resources`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResourceRequirements {
    /// Requests.
    pub requests: Quantities,
    /// Limits.
    pub limits: Quantities,
}

/// CPU and memory quantities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Quantities {
    /// `100m`.
    pub cpu: String,
    /// `128Mi`.
    pub memory: String,
}

/// A native gRPC probe (`grpc.health.v1`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Probe {
    /// The port and health service.
    pub grpc: GrpcAction,
    /// Seconds between probes.
    pub period_seconds: u32,
    /// Seconds before a probe times out.
    pub timeout_seconds: u32,
    /// Consecutive failures that count.
    pub failure_threshold: u32,
}

/// `grpc` probe action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GrpcAction {
    /// The service port.
    pub port: u16,
    /// Empty: the server's overall status.
    pub service: &'static str,
}

/// Container `securityContext`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerSecurityContext {
    /// `false`.
    pub allow_privilege_escalation: bool,
    /// `true`.
    pub read_only_root_filesystem: bool,
    /// `true`.
    pub run_as_non_root: bool,
    /// UID.
    pub run_as_user: u32,
    /// GID.
    pub run_as_group: u32,
    /// Dropped capabilities.
    pub capabilities: Capabilities,
    /// `RuntimeDefault`.
    pub seccomp_profile: SeccompProfile,
}

/// `capabilities`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Capabilities {
    /// `[ALL]`.
    pub drop: Vec<&'static str>,
}

/// A volume mount.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeMount {
    /// Volume name.
    pub name: &'static str,
    /// Mount path.
    pub mount_path: &'static str,
    /// Read-only.
    pub read_only: bool,
}

/// A volume: an in-memory `emptyDir` or a Secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Volume {
    /// Name.
    pub name: &'static str,
    /// `emptyDir`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub empty_dir: Option<EmptyDir>,
    /// `secret`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<SecretVolume>,
}

/// `emptyDir`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmptyDir {
    /// `Memory`.
    pub medium: &'static str,
    /// Size limit.
    pub size_limit: &'static str,
}

/// `secret` volume source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretVolume {
    /// Secret name.
    pub secret_name: String,
    /// Mode of the files (decimal in YAML).
    pub default_mode: u32,
    /// `false`: the pod does not start without it.
    pub optional: bool,
    /// The keys projected, and their file names.
    pub items: Vec<KeyToPath>,
}

/// `items` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KeyToPath {
    /// Secret key.
    pub key: &'static str,
    /// File name.
    pub path: &'static str,
}

/// PodDisruptionBudget body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PdbBody {
    /// `spec`.
    pub spec: PdbSpec,
}

/// PodDisruptionBudget `spec`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PdbSpec {
    /// `1`.
    pub min_available: u16,
    /// Selector.
    pub selector: Selector,
}

/// AuthorizationPolicy body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuthorizationPolicyBody {
    /// `spec`.
    pub spec: AuthorizationPolicySpec,
}

/// AuthorizationPolicy `spec`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuthorizationPolicySpec {
    /// The workload.
    pub selector: Selector,
    /// `ALLOW`.
    pub action: &'static str,
    /// No rules: the policy matches nothing, so (being an `ALLOW` policy
    /// on the workload) it denies every request.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<Rule>,
}

/// An authorization rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Rule {
    /// Sources.
    pub from: Vec<RuleFrom>,
}

/// `from` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuleFrom {
    /// The source.
    pub source: Source,
}

/// `source`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Source {
    /// Peer principals, `<trust-domain>/<path>`.
    pub principals: Vec<String>,
}

/// DestinationRule body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DestinationRuleBody {
    /// `spec`.
    pub spec: DestinationRuleSpec,
}

/// DestinationRule `spec`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DestinationRuleSpec {
    /// The Service FQDN.
    pub host: String,
    /// TLS settings.
    pub traffic_policy: TrafficPolicy,
}

/// `trafficPolicy`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TrafficPolicy {
    /// `tls`.
    pub tls: Tls,
}

/// `tls`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Tls {
    /// `ISTIO_MUTUAL`.
    pub mode: &'static str,
}

/// `proxy.istio.io/config`: the Istio `ProxyConfig` fields the pod
/// overrides, serialized as YAML into the annotation value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyConfig {
    /// Start the application only once Envoy is ready.
    pub hold_application_until_proxy_starts: bool,
    /// How long Envoy drains after SIGTERM (`7s`).
    pub termination_drain_duration: String,
    /// pilot-agent environment.
    pub proxy_metadata: ProxyMetadata,
}

/// `proxyMetadata`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProxyMetadata {
    /// `"true"`: pilot-agent exits once Envoy has no active connections,
    /// but not before `MINIMUM_DRAIN_DURATION`.
    #[serde(rename = "EXIT_ON_ZERO_ACTIVE_CONNECTIONS")]
    pub exit_on_zero_active_connections: &'static str,
    /// The shortest drain (`7s`), so the proxy outlives the app's drain.
    #[serde(rename = "MINIMUM_DRAIN_DURATION")]
    pub minimum_drain_duration: String,
}
