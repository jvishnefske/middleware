//! Zenoh credentials (pure): the Secrets `mw zenoh apply` sends to the
//! cluster.
//!
//! Each service is a zenohd user named after the service (its Kubernetes
//! name, which `mw` takes to be the binary's name, as `mw apply` does).
//! The shell draws [`PASSWORD_BYTES`] random bytes per service (getrandom)
//! and passes them here; [`password`] hex-encodes them, so a password is
//! visible ASCII without the `:` the formats below use as a separator, and
//! its 48 characters are within `mw-service`'s 16..=256 bound.
//! [`secrets_manifest`] renders one `v1` `List` (JSON, which `kubectl
//! apply -f -` reads like YAML) in the format of `mw-service`'s
//! `zenoh_router::credentials` module (MS10, MS31, Z19):
//!
//! * [`DICTIONARY_SECRET`], key [`DICTIONARY_KEY`]: zenohd's `usrpwd`
//!   dictionary, one `user:password` line per service sorted by user,
//!   mounted by the router (`deploy/zenoh/zenohd.yaml`);
//! * `zenoh-<svc>` ([`service_secret`]), key [`SERVICE_KEY`]: that
//!   service's credentials file, the one line `user:password`, mounted by
//!   its Deployment and named by `MW_ZENOH_CREDENTIALS`.
//!
//! The dictionary holds exactly the services given, so `mw zenoh apply`
//! must name every Zenoh service of the namespace each time (the router's
//! ACL is re-rendered from the same list). Robot users (`robot-<name>`)
//! are not provisioned here.
//!
//! Every Secret is labelled `app.kubernetes.io/managed-by=mw-service`; the
//! per-service ones also carry `app.kubernetes.io/name=<svc>`, so `mw down`
//! removes them with the service. The manifest goes to kubectl's standard
//! input only; it is never written to disk or printed.

use crate::plan::{NAME_LABEL, Name};
use serde_json::{Value, json};

/// The router's dictionary Secret (`mw-service`'s
/// `credentials::DICTIONARY_SECRET`).
pub const DICTIONARY_SECRET: &str = "zenohd-auth";
/// The dictionary's key in [`DICTIONARY_SECRET`] (`DICTIONARY_KEY`).
pub const DICTIONARY_KEY: &str = "dictionary";
/// The credential key in each service's Secret (`SECRET_KEY`).
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

/// One credentials line, `user:password\n`.
fn line(user: &Name, password: &str) -> String {
    format!("{user}:{password}\n")
}

/// The Secret list for `creds` (service, password) in `ns` (module docs).
pub fn secrets_manifest(ns: &Name, creds: &[(Name, String)]) -> String {
    let managed = || json!({ "app.kubernetes.io/managed-by": "mw-service" });
    let mut sorted: Vec<&(Name, String)> = creds.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let dictionary: String = sorted.iter().map(|(s, p)| line(s, p)).collect();
    let mut items = vec![secret(
        DICTIONARY_SECRET,
        ns,
        managed(),
        DICTIONARY_KEY,
        dictionary,
    )];
    for (svc, pw) in creds {
        let mut labels = managed();
        labels[NAME_LABEL] = json!(svc.as_str());
        items.push(secret(
            &service_secret(svc),
            ns,
            labels,
            SERVICE_KEY,
            line(svc, pw),
        ));
    }
    let list = json!({ "apiVersion": "v1", "kind": "List", "items": items });
    format!("{list:#}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ms63_passwords_are_hex() {
        let mut b = [0u8; PASSWORD_BYTES];
        b[0] = 0xab;
        b[23] = 0x01;
        let p = password(&b);
        assert_eq!(p.len(), 48);
        assert!(p.starts_with("ab00") && p.ends_with("01"));
    }

    #[test]
    fn ms63_secrets_hold_the_sorted_dictionary_and_per_service_credentials() {
        let ns = Name::parse("namespace", "apps").unwrap();
        let a = Name::parse("service", "demo-echo").unwrap();
        let b = Name::parse("service", "demo-caller").unwrap();
        let text = secrets_manifest(&ns, &[(a, "p1".into()), (b, "p2".into())]);
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["kind"], "List");
        let items = v["items"].as_array().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0]["metadata"]["name"], "zenohd-auth");
        assert_eq!(items[0]["metadata"]["namespace"], "apps");
        assert_eq!(
            items[0]["stringData"]["dictionary"],
            "demo-caller:p2\ndemo-echo:p1\n"
        );
        assert_eq!(items[1]["metadata"]["name"], "zenoh-demo-echo");
        assert_eq!(items[1]["stringData"]["credentials"], "demo-echo:p1\n");
        assert_eq!(
            items[1]["metadata"]["labels"]["app.kubernetes.io/name"],
            "demo-echo"
        );
        assert_eq!(
            items[2]["metadata"]["labels"]["app.kubernetes.io/managed-by"],
            "mw-service"
        );
        assert_eq!(items[2]["stringData"]["credentials"], "demo-caller:p2\n");
        assert!(items[0]["metadata"]["labels"].get(NAME_LABEL).is_none());
    }
}
