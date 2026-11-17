//! Zenoh credentials (pure): the Secrets `mw zenoh apply` sends to the
//! cluster.
//!
//! Each service is a zenohd user named after the service. The shell draws
//! [`PASSWORD_BYTES`] random bytes per service (getrandom) and passes them
//! here; [`password`] hex-encodes them, so a password never contains the
//! `:` or newline the formats below use as separators.
//! [`secrets_manifest`] renders one `v1` `List` (JSON, which `kubectl
//! apply -f -` reads like YAML) of:
//!
//! * [`ROUTER_SECRET`], key [`ROUTER_KEY`]: zenohd's user/password
//!   dictionary, one `user:password` line per service, mounted by the
//!   router;
//! * `zenoh-<svc>` ([`service_secret`]), key [`SERVICE_KEY`]: that
//!   service's two lines `user\npassword\n`, mounted by its Deployment and
//!   named by `MW_ZENOH_CREDENTIALS`.
//!
//! Every Secret is labelled `app.kubernetes.io/managed-by=mw-service`; the
//! per-service ones also carry `app.kubernetes.io/name=<svc>`, so `mw down`
//! removes them with the service. The manifest goes to kubectl's standard
//! input only; it is never written to disk or printed.

use crate::plan::{NAME_LABEL, Name};
use serde_json::{Value, json};

/// The router's dictionary Secret.
pub const ROUTER_SECRET: &str = "zenohd-credentials";
/// The dictionary's key in [`ROUTER_SECRET`].
pub const ROUTER_KEY: &str = "credentials.txt";
/// The credential key in each service's Secret.
pub const SERVICE_KEY: &str = "credentials";
/// Random bytes per password (192 bits).
pub const PASSWORD_BYTES: usize = 24;

/// `zenoh-<svc>`.
pub fn service_secret(svc: &Name) -> String {
    format!("zenoh-{svc}")
}

/// Lowercase hex of `bytes`.
pub fn password(bytes: &[u8; PASSWORD_BYTES]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn secret(name: &str, ns: &Name, labels: Value, key: &str, value: String) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": { "name": name, "namespace": ns.as_str(), "labels": labels },
        "type": "Opaque",
        "stringData": { key: value },
    })
}

/// The Secret list for `creds` (service, password) in `ns` (module docs).
pub fn secrets_manifest(ns: &Name, creds: &[(Name, String)]) -> String {
    let managed = || json!({ "app.kubernetes.io/managed-by": "mw-service" });
    let dictionary: String = creds.iter().map(|(s, p)| format!("{s}:{p}\n")).collect();
    let mut items = vec![secret(ROUTER_SECRET, ns, managed(), ROUTER_KEY, dictionary)];
    for (svc, pw) in creds {
        let mut labels = managed();
        labels[NAME_LABEL] = json!(svc.as_str());
        items.push(secret(
            &service_secret(svc),
            ns,
            labels,
            SERVICE_KEY,
            format!("{svc}\n{pw}\n"),
        ));
    }
    let list = json!({ "apiVersion": "v1", "kind": "List", "items": items });
    format!("{list:#}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m53_passwords_are_hex() {
        let mut b = [0u8; PASSWORD_BYTES];
        b[0] = 0xab;
        b[23] = 0x01;
        let p = password(&b);
        assert_eq!(p.len(), 48);
        assert!(p.starts_with("ab00") && p.ends_with("01"));
    }

    #[test]
    fn m53_secrets_hold_the_dictionary_and_per_service_credentials() {
        let ns = Name::parse("namespace", "apps").unwrap();
        let a = Name::parse("service", "demo-echo").unwrap();
        let b = Name::parse("service", "demo-caller").unwrap();
        let text = secrets_manifest(&ns, &[(a, "p1".into()), (b, "p2".into())]);
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["kind"], "List");
        let items = v["items"].as_array().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0]["metadata"]["name"], ROUTER_SECRET);
        assert_eq!(items[0]["metadata"]["namespace"], "apps");
        assert_eq!(
            items[0]["stringData"][ROUTER_KEY],
            "demo-echo:p1\ndemo-caller:p2\n"
        );
        assert_eq!(items[1]["metadata"]["name"], "zenoh-demo-echo");
        assert_eq!(items[1]["stringData"]["credentials"], "demo-echo\np1\n");
        assert_eq!(
            items[1]["metadata"]["labels"]["app.kubernetes.io/name"],
            "demo-echo"
        );
        assert_eq!(
            items[2]["metadata"]["labels"]["app.kubernetes.io/managed-by"],
            "mw-service"
        );
        assert_eq!(items[2]["stringData"]["credentials"], "demo-caller\np2\n");
        assert!(items[0]["metadata"]["labels"].get(NAME_LABEL).is_none());
    }
}
