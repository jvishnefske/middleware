//! The runner's node id: the [`SenderId`] its health service sends in the
//! `mw-node` header (`grpc::health::NODE_HEADER`, 16 lowercase hex
//! digits) and its downstream clients send in `mw-client`.
//!
//! In a pod it is derived from the pod's identity, so it is stable for the
//! pod's life and the same in every log line, header and trace: the 64-bit
//! FNV-1a hash of `<POD_NAMESPACE>/<POD_NAME>` (an unset namespace is
//! empty), its eight bytes little endian. Without `POD_NAME` (a process
//! outside Kubernetes, a test) it is a fresh id per run
//! (`grpc::client::fresh_client_id`).

use middleware::dedup::fnv1a64;
use middleware::transport::SenderId;

use crate::config::Config;

/// The node id of pod `name` in `namespace` (module docs). Pure.
pub fn pod_node_id(namespace: Option<&str>, name: &str) -> SenderId {
    let text = format!("{}/{name}", namespace.unwrap_or(""));
    SenderId(fnv1a64(text.as_bytes()).to_le_bytes())
}

/// The node id a run with `config` uses (module docs).
pub fn node_id(config: &Config) -> SenderId {
    match &config.pod_name {
        Some(name) => pod_node_id(config.namespace.as_deref(), name),
        None => middleware::grpc::client::fresh_client_id(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m9_node_id_hashes_the_pod_identity() {
        let a = pod_node_id(Some("apps"), "demo-echo-5d9c-x1");
        assert_eq!(a, pod_node_id(Some("apps"), "demo-echo-5d9c-x1"), "stable");
        assert_ne!(a, pod_node_id(Some("apps"), "demo-echo-5d9c-x2"));
        assert_ne!(a, pod_node_id(Some("other"), "demo-echo-5d9c-x1"));
        assert_eq!(
            pod_node_id(None, "p"),
            SenderId(fnv1a64(b"/p").to_le_bytes())
        );
        let config = Config {
            namespace: Some("apps".into()),
            pod_name: Some("demo-echo-5d9c-x1".into()),
            ..Config::default()
        };
        assert_eq!(node_id(&config), a);
        // Without a pod name: fresh per run.
        assert_ne!(node_id(&Config::default()), node_id(&Config::default()));
    }
}
