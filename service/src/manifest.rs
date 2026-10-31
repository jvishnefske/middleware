//! Kubernetes manifests rendered from a [`ServiceSpec`] (`--print-manifests`).
//!
//! The entry point is [`render`]: the spec plus [`ManifestOptions`] in, a
//! multi-document YAML stream out. Planned objects (design.md M11–M15):
//! ServiceAccount, Deployment (sidecar injection, SPIRE template, `grpc`
//! probes on the service port, drain-derived termination grace, the
//! [`Config`](crate::Config) environment, restricted security context),
//! Service and headless Service (port `grpc`, `appProtocol: grpc`),
//! AuthorizationPolicy from `allow_callers` (kubelet probes implicit; an
//! empty list admits no caller), DestinationRule `ISTIO_MUTUAL` and a
//! PodDisruptionBudget.
//!
//! Phase 1 of 0.6 fixes the signature only: [`render`] returns
//! [`ServiceError::Unimplemented`].

use crate::error::ServiceError;
use crate::spec::ServiceSpec;

/// What the manifests need beyond the spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestOptions {
    /// The namespace the objects are rendered for.
    pub namespace: String,
    /// The container image reference; `None` lets the renderer derive one
    /// from the service name.
    pub image: Option<String>,
}

/// The manifests of `spec` as one YAML stream (module docs). Not
/// implemented yet: always [`ServiceError::Unimplemented`].
pub fn render(spec: &ServiceSpec, options: &ManifestOptions) -> Result<String, ServiceError> {
    let _ = (spec, options);
    Err(ServiceError::Unimplemented("manifest rendering (M11)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_is_a_typed_placeholder() {
        let r = render(
            &ServiceSpec::new("demo/echo"),
            &ManifestOptions {
                namespace: "apps".into(),
                image: None,
            },
        );
        assert!(matches!(r, Err(ServiceError::Unimplemented(_))));
    }
}
