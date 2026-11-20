//! The updates zenohd's config (UP8, WS1).
//!
//! [`render`] writes the router config as JSON (zenohd reads JSON5):
//! `router` mode, listening on `tcp/0.0.0.0:7447` (the server, in the
//! cluster) and `ws/0.0.0.0:7448` (clients, through Traefik, which ends
//! TLS; [`middleware::zenoh::wsbridge`]), multicast scouting and gossip
//! off, every link authenticated by `usrpwd` against the dictionary, and
//! `access_control` with `default_permission: deny` and one subject per
//! role:
//!
//! | role | allowed |
//! |------|---------|
//! | server | answer (`declare_queryable`, `reply`) `latest/**`, `img/**`; subscribe `publish/**`, `status/**`; put `latest/**` |
//! | publisher | put `publish/**`; query `latest/**` (to see its release land) |
//! | updater | query `latest/**`, `img/**`; subscribe `latest/**`; put `status/**` |
//!
//! (all under `@mw/updates/`). Zenoh 1.10 checks each message on ingress
//! (client to router) and egress (router to client), and a rule allows a
//! key expression it *includes*; so each grant is a pair of rules, as in
//! `mw-service`'s mesh router.

use serde::Serialize;

use crate::keys::ROOT;

/// The usernames of each role (the dictionary's users).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Users {
    /// The `mw-updates` server.
    pub server: Vec<String>,
    /// Who may publish releases.
    pub publishers: Vec<String>,
    /// Devices' updaters.
    pub updaters: Vec<String>,
}

/// The TCP listener (the server, inside the cluster).
pub const TCP_PORT: u16 = 7447;
/// The WebSocket listener (clients, behind Traefik).
pub const WS_PORT: u16 = 7448;

#[derive(Clone, Serialize)]
struct Rule {
    id: String,
    messages: Vec<&'static str>,
    flows: Vec<&'static str>,
    permission: &'static str,
    key_exprs: Vec<String>,
}

#[derive(Serialize)]
struct Subject {
    id: &'static str,
    usernames: Vec<String>,
}

#[derive(Serialize)]
struct Policy {
    rules: Vec<String>,
    subjects: Vec<&'static str>,
}

fn under(paths: &[&str]) -> Vec<String> {
    paths.iter().map(|p| format!("{ROOT}/{p}/**")).collect()
}

/// One grant: `ingress` messages from the client on `paths`, and `egress`
/// messages to it on the same.
fn grant(
    role: &str,
    name: &str,
    ingress: &[&'static str],
    egress: &[&'static str],
    paths: &[&str],
) -> Vec<Rule> {
    let mut out = Vec::new();
    for (flow, messages) in [("ingress", ingress), ("egress", egress)] {
        if !messages.is_empty() {
            out.push(Rule {
                id: format!(
                    "{role}/{name}-{}",
                    if flow == "ingress" { "in" } else { "out" }
                ),
                messages: messages.to_vec(),
                flows: vec![flow],
                permission: "allow",
                key_exprs: under(paths),
            });
        }
    }
    out
}

fn rules_of(role: &str) -> Vec<Rule> {
    match role {
        "server" => [
            grant(
                role,
                "answer",
                &["declare_queryable", "reply"],
                &["query"],
                &["latest", "img"],
            ),
            grant(
                role,
                "subscribe",
                &["declare_subscriber"],
                &["put"],
                &["publish", "status"],
            ),
            grant(role, "publish", &["put"], &[], &["latest"]),
        ]
        .concat(),
        "publisher" => [
            grant(role, "publish", &["put"], &[], &["publish"]),
            grant(role, "query", &["query"], &["reply"], &["latest"]),
        ]
        .concat(),
        _ => [
            grant(role, "query", &["query"], &["reply"], &["latest", "img"]),
            grant(
                role,
                "subscribe",
                &["declare_subscriber"],
                &["put"],
                &["latest"],
            ),
            grant(role, "status", &["put"], &[], &["status"]),
        ]
        .concat(),
    }
}

/// The zenohd config for `users`, reading the dictionary at
/// `dictionary_file` (module docs). Pure; the same input gives the same
/// bytes.
pub fn render(users: &Users, dictionary_file: &str) -> String {
    let roles: [(&'static str, &[String]); 3] = [
        ("server", &users.server),
        ("publisher", &users.publishers),
        ("updater", &users.updaters),
    ];
    let mut rules = Vec::new();
    let mut subjects = Vec::new();
    let mut policies = Vec::new();
    for (role, names) in roles {
        if names.is_empty() {
            continue;
        }
        let mut names = names.to_vec();
        names.sort();
        names.dedup();
        let r = rules_of(role);
        policies.push(Policy {
            rules: r.iter().map(|r| r.id.clone()).collect(),
            subjects: vec![role],
        });
        rules.extend(r);
        subjects.push(Subject {
            id: role,
            usernames: names,
        });
    }
    let config = serde_json::json!({
        "mode": "router",
        "listen": { "endpoints": [
            format!("tcp/0.0.0.0:{TCP_PORT}"),
            format!("ws/0.0.0.0:{WS_PORT}"),
        ] },
        "connect": { "endpoints": [] },
        "scouting": { "multicast": { "enabled": false }, "gossip": { "enabled": false } },
        "transport": { "auth": { "usrpwd": { "dictionary_file": dictionary_file } } },
        "access_control": {
            "enabled": true,
            "default_permission": "deny",
            "rules": rules,
            "subjects": subjects,
            "policies": policies,
        },
    });
    let json = serde_json::to_string_pretty(&config).expect("plain JSON values");
    format!("// zenohd config generated by `mw-updates router-config`; do not edit.\n{json}\n")
}

/// Parses a credentials file: one line `<user>:<password>` (the newline
/// may be missing).
pub fn parse_credentials(text: &str) -> Option<(String, String)> {
    let line = text.strip_suffix('\n').unwrap_or(text);
    if line.contains('\n') {
        return None;
    }
    let (user, password) = line.split_once(':')?;
    (!user.is_empty() && !password.is_empty()).then(|| (user.to_owned(), password.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn users() -> Users {
        Users {
            server: vec!["mw-updates".into()],
            publishers: vec!["release".into()],
            updaters: vec!["pi-b".into(), "pi-a".into()],
        }
    }

    #[test]
    fn up8_router_config_parses_and_is_deterministic() {
        let a = render(&users(), "/etc/zenoh/auth/dictionary");
        assert_eq!(a, render(&users(), "/etc/zenoh/auth/dictionary"));
        let c = zenoh::Config::from_json5(&a).expect("zenoh reads the config");
        let s = c.to_string();
        assert!(s.contains("ws/0.0.0.0:7448"), "{s}");
        // Sorted usernames; every policy names rules that exist.
        let v: serde_json::Value = serde_json::from_str(a.split_once('\n').unwrap().1).unwrap();
        let acl = &v["access_control"];
        assert_eq!(
            acl["subjects"][2]["usernames"],
            serde_json::json!(["pi-a", "pi-b"])
        );
        let ids: Vec<&str> = acl["rules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap())
            .collect();
        for p in acl["policies"].as_array().unwrap() {
            for r in p["rules"].as_array().unwrap() {
                assert!(ids.contains(&r.as_str().unwrap()));
            }
        }
        // Only the server may answer.
        let answer: Vec<&str> = acl["rules"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| {
                r["messages"]
                    .as_array()
                    .unwrap()
                    .contains(&"declare_queryable".into())
            })
            .map(|r| r["id"].as_str().unwrap())
            .collect();
        assert_eq!(answer, ["server/answer-in"]);
    }

    #[test]
    fn credentials_parse() {
        assert_eq!(
            parse_credentials("pi-a:secret-password\n"),
            Some(("pi-a".into(), "secret-password".into()))
        );
        assert_eq!(parse_credentials("a:b"), Some(("a".into(), "b".into())));
        assert_eq!(parse_credentials("a:b\nc:d"), None);
        assert_eq!(parse_credentials(":b"), None);
        assert_eq!(parse_credentials("nocolon"), None);
    }
}
