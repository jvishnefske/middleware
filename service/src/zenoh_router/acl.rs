//! The router's access-control model: who may do what on which keys.
//!
//! Pure data and pure functions: [`Grants`] (what one principal declares),
//! [`Principal`] (a Zenoh username with its grants), and [`AclModel`], the
//! `access_control` rules, subjects and policies of zenohd computed from
//! the principals ([`AclModel::from_principals`]). [`AclModel::permits`]
//! evaluates the model the way zenoh 1.10's router does, so the unit tests
//! here state what the router enforces and the integration test
//! (`service/tests/zenoh_router.rs`) proves the router agrees.
//!
//! # Zenoh's semantics (1.10)
//!
//! * The router checks every message on every client link twice: on
//!   **ingress** (from the client into the router) and on **egress** (from
//!   the router to the client), each against the rules of the subjects the
//!   link matched. Subjects here match by username only, which zenohd
//!   learns from `usrpwd` authentication.
//! * A rule allows a message when one of its key expressions **includes**
//!   the message's key expression (zenoh's `keyexpr::includes`). So a
//!   subscriber declared on `app/**` is refused by a rule allowing
//!   `app/demo/**`, even though the two intersect.
//! * `default_permission` is `deny`: a link that matched no subject, or a
//!   message no rule allows, is dropped. Every rule here is an `allow`.
//!
//! # What each grant allows
//!
//! Filters are in [`parse_filter`](crate::spec::parse_filter) form: `k` is
//! exactly the key expression `k`, `k/**` is `k` and everything below it.
//!
//! | grant | ingress | egress |
//! |---|---|---|
//! | `publishes` F | `put`, `delete` on F | — |
//! | `subscribes` F | `declare_subscriber` on F | `put`, `delete` on F |
//! | `answers` F | `declare_queryable`, `reply` on F and `@mw/node/*/`F | `query` on the same |
//! | `queries` F | `query` on F and `@mw/node/*/`F | `reply` on the same |
//! | `announces` K | `liveliness_token`, `declare_queryable`, `reply` on `@mw/svc/`K`/*` (`@mw/svc/k/**` for `k/**`) | `query` on the same |
//! | every principal (browse) | `declare_liveliness_subscriber`, `liveliness_query`, `query` on `@mw/svc/**` | `liveliness_token`, `reply` on `@mw/svc/**` |
//!
//! No principal receives other principals' subscriber or queryable
//! declarations (egress `declare_subscriber` / `declare_queryable`): a
//! zenoh 1.10 client sends every put and query to its router regardless
//! (the integration test passes without them), so they would only reveal
//! who listens. The cost: a publisher's or querier's *matching status*
//! never reports remote matches. Liveliness tokens, on the other hand,
//! must reach browsers (egress `liveliness_token`), or discovery sees
//! nothing.
//!
//! A service's grants come from its spec ([`Grants::from_spec`]):
//! `publishes` and `subscribes` as declared, `answers` and `announces` its
//! `serves` kinds, `queries` its `calls` kinds. Anything else, including
//! `sys/**`, the admin space `@/**` and other services' announcements, is
//! denied.
//!
//! # Limits
//!
//! * The per-node key `@mw/node/<zid16>/<key>` is granted with `*` for the
//!   node: zenoh cannot bind a username to the session's Zenoh id (the id
//!   is self-chosen and the ACL has no "own zid" variable), so a principal
//!   allowed to answer `k` may squat on another node's `@mw/node/*/k`.
//!   Z17's replier check still drops squatters that do not claim the id.
//! * Announcing is bound to the kind, not to the instance: any principal
//!   allowed to announce `k` may declare any instance of `k`.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use zenoh::key_expr::keyexpr;

use crate::spec::{ServiceSpec, parse_filter};
use middleware::key::KeyFilter;

use super::RouterError;

/// A Zenoh message kind as named by zenohd's `access_control.rules`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Message {
    /// A publication.
    Put,
    /// A deletion.
    Delete,
    /// A subscriber declaration.
    DeclareSubscriber,
    /// A query (`get`).
    Query,
    /// A queryable declaration.
    DeclareQueryable,
    /// A reply to a query.
    Reply,
    /// A liveliness token declaration.
    LivelinessToken,
    /// A liveliness subscriber declaration.
    DeclareLivelinessSubscriber,
    /// A liveliness query.
    LivelinessQuery,
}

/// The direction of a message relative to the router.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Flow {
    /// From the client into the router.
    Ingress,
    /// From the router to the client.
    Egress,
}

/// A rule's verdict. The model only renders `allow` rules (the default is
/// `deny`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    /// Let the message through.
    Allow,
    /// Drop the message.
    Deny,
}

/// The key space one principal may use (module docs). Every entry is a
/// filter in text form; [`Grants::validate`] checks them. Deserializes
/// with every list defaulted to empty.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Grants {
    /// Keys it may publish (`put`, `delete`) on.
    pub publishes: Vec<String>,
    /// Keys it may subscribe to.
    pub subscribes: Vec<String>,
    /// Keys it may answer queries on (queryables, plus the per-node key).
    pub answers: Vec<String>,
    /// Keys it may query.
    pub queries: Vec<String>,
    /// Service kinds it may announce on `@mw/svc/<kind>/<instance>`.
    pub announces: Vec<String>,
}

impl Grants {
    /// A service's grants from its spec: `publishes`, `subscribes`,
    /// `answers` = `announces` = `serves`, `queries` = `calls`. Pure.
    pub fn from_spec(spec: &ServiceSpec) -> Self {
        Grants {
            publishes: spec.publishes.clone(),
            subscribes: spec.subscribes.clone(),
            answers: spec.serves.clone(),
            queries: spec.calls.clone(),
            announces: spec.serves.clone(),
        }
    }

    /// `self` with every entry of `other` appended that `self` lacks.
    pub fn merge(mut self, other: &Grants) -> Self {
        fn add(into: &mut Vec<String>, from: &[String]) {
            for f in from {
                if !into.contains(f) {
                    into.push(f.clone());
                }
            }
        }
        add(&mut self.publishes, &other.publishes);
        add(&mut self.subscribes, &other.subscribes);
        add(&mut self.answers, &other.answers);
        add(&mut self.queries, &other.queries);
        add(&mut self.announces, &other.announces);
        self
    }

    /// Every entry must parse as a filter ([`parse_filter`]), which also
    /// keeps `@`, `*` and `$` out of the granted key space.
    pub fn validate(&self) -> Result<(), RouterError> {
        let all = [
            &self.publishes,
            &self.subscribes,
            &self.answers,
            &self.queries,
            &self.announces,
        ];
        match all
            .iter()
            .flat_map(|l| l.iter())
            .find(|f| parse_filter(f).is_none())
        {
            Some(bad) => Err(RouterError::InvalidGrant(bad.clone())),
            None => Ok(()),
        }
    }
}

/// A Zenoh principal: the username it authenticates with, the Istio peer
/// principal its connection arrives with (for the router's
/// AuthorizationPolicy), and its grants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    /// The `usrpwd` username (the Kubernetes name of a service,
    /// `robot-<name>` for a robot).
    pub user: String,
    /// The mTLS principal (`<trust domain>/ns/<ns>/sa/<sa>`, no `spiffe://`)
    /// the router's sidecar sees for this principal's connections.
    pub peer: String,
    /// What it may do.
    pub grants: Grants,
}

/// One `access_control.rules` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Rule {
    /// Unique id, `<user>/<grant>-<flow>`.
    pub id: String,
    /// The messages it covers.
    pub messages: Vec<Message>,
    /// The directions it covers.
    pub flows: Vec<Flow>,
    /// Always [`Permission::Allow`].
    pub permission: Permission,
    /// The key expressions it covers (non-empty).
    pub key_exprs: Vec<String>,
}

/// One `access_control.subjects` entry: links authenticated as one user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Subject {
    /// The subject id (the username).
    pub id: String,
    /// The usernames it matches (exactly one).
    pub usernames: Vec<String>,
}

/// One `access_control.policies` entry: a subject's rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Policy {
    /// The policy id (the username).
    pub id: String,
    /// The rule ids.
    pub rules: Vec<String>,
    /// The subject ids (exactly one).
    pub subjects: Vec<String>,
}

/// zenohd's `access_control` section (`enabled: true`,
/// `default_permission: deny` are rendered around it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AclModel {
    /// The rules, grouped by principal in username order.
    pub rules: Vec<Rule>,
    /// One subject per principal.
    pub subjects: Vec<Subject>,
    /// One policy per principal.
    pub policies: Vec<Policy>,
}

/// The key expression of a filter.
fn expr(filter: KeyFilter<'_>) -> String {
    match filter {
        KeyFilter::Exact(k) => k.as_str().to_owned(),
        KeyFilter::Prefix(k) => format!("{k}/**"),
    }
}

/// The per-node queryable keys of a filter: `@mw/node/*/<expr>`.
fn node_expr(filter: KeyFilter<'_>) -> String {
    format!("{}*/{}", middleware::zenoh::wire::NODE_PREFIX, expr(filter))
}

/// The announcement keys of a kind filter: `@mw/svc/<k>/*` (instances of
/// exactly `k`) or `@mw/svc/<k>/**`.
fn service_expr(filter: KeyFilter<'_>) -> String {
    let prefix = middleware::zenoh::wire::SERVICE_PREFIX;
    match filter {
        KeyFilter::Exact(k) => format!("{prefix}{k}/*"),
        KeyFilter::Prefix(k) => format!("{prefix}{k}/**"),
    }
}

/// Every key expression a principal may browse discovery on.
pub fn browse_expr() -> String {
    format!("{}**", middleware::zenoh::wire::SERVICE_PREFIX)
}

fn exprs(list: &[String], map: impl Fn(KeyFilter<'_>) -> String) -> Vec<String> {
    list.iter()
        .filter_map(|f| parse_filter(f))
        .map(map)
        .collect()
}

/// The rules of one principal, in table order (module docs). Rules whose
/// key list would be empty are left out. Pure.
pub fn rules_for(p: &Principal) -> Vec<Rule> {
    use Flow::{Egress, Ingress};
    use Message::*;
    let g = &p.grants;
    let with_node = |list: &[String]| {
        let mut v = exprs(list, expr);
        v.extend(exprs(list, node_expr));
        v
    };
    let publishes = exprs(&g.publishes, expr);
    let subscribes = exprs(&g.subscribes, expr);
    let answers = with_node(&g.answers);
    let queries = with_node(&g.queries);
    let announces = exprs(&g.announces, service_expr);
    let browse = vec![browse_expr()];
    let table: [(&str, Flow, Vec<Message>, &Vec<String>); 11] = [
        ("publish", Ingress, vec![Put, Delete], &publishes),
        ("subscribe", Ingress, vec![DeclareSubscriber], &subscribes),
        ("subscribe", Egress, vec![Put, Delete], &subscribes),
        ("answer", Ingress, vec![DeclareQueryable, Reply], &answers),
        ("answer", Egress, vec![Query], &answers),
        ("query", Ingress, vec![Query], &queries),
        ("query", Egress, vec![Reply], &queries),
        (
            "announce",
            Ingress,
            vec![LivelinessToken, DeclareQueryable, Reply],
            &announces,
        ),
        ("announce", Egress, vec![Query], &announces),
        (
            "browse",
            Ingress,
            vec![DeclareLivelinessSubscriber, LivelinessQuery, Query],
            &browse,
        ),
        ("browse", Egress, vec![LivelinessToken, Reply], &browse),
    ];
    table
        .into_iter()
        .filter(|(_, _, _, keys)| !keys.is_empty())
        .map(|(grant, flow, messages, keys)| Rule {
            id: format!(
                "{}/{grant}-{}",
                p.user,
                match flow {
                    Ingress => "in",
                    Egress => "out",
                }
            ),
            messages,
            flows: vec![flow],
            permission: Permission::Allow,
            key_exprs: keys.clone(),
        })
        .collect()
}

impl AclModel {
    /// The model of `principals`, sorted by username. Two principals with
    /// one username, or a grant that is not a filter, are errors. Pure.
    pub fn from_principals(principals: &[Principal]) -> Result<Self, RouterError> {
        let mut sorted: Vec<&Principal> = principals.iter().collect();
        sorted.sort_by(|a, b| a.user.cmp(&b.user));
        if let Some(w) = sorted.windows(2).find(|w| w[0].user == w[1].user) {
            return Err(RouterError::DuplicateUser(w[0].user.clone()));
        }
        let mut model = AclModel {
            rules: Vec::new(),
            subjects: Vec::new(),
            policies: Vec::new(),
        };
        for p in sorted {
            p.grants.validate()?;
            let rules = rules_for(p);
            model.policies.push(Policy {
                id: p.user.clone(),
                rules: rules.iter().map(|r| r.id.clone()).collect(),
                subjects: vec![p.user.clone()],
            });
            model.subjects.push(Subject {
                id: p.user.clone(),
                usernames: vec![p.user.clone()],
            });
            model.rules.extend(rules);
        }
        Ok(model)
    }

    /// Whether the router lets `message` from (ingress) or to (egress) a
    /// link authenticated as `user` through on `key_expr`, under default
    /// deny: some rule of `user`'s policy names the message and flow and
    /// has a key expression including `key_expr`. An unknown user, or a
    /// `key_expr` that is not a valid Zenoh key expression, is refused.
    /// Pure.
    pub fn permits(&self, user: &str, message: Message, flow: Flow, key_expr: &str) -> bool {
        let Ok(target) = keyexpr::new(key_expr) else {
            return false;
        };
        let rule_ids: BTreeSet<&str> = self
            .policies
            .iter()
            .filter(|p| {
                p.subjects.iter().any(|s| {
                    self.subjects
                        .iter()
                        .any(|sub| &sub.id == s && sub.usernames.iter().any(|u| u == user))
                })
            })
            .flat_map(|p| p.rules.iter().map(String::as_str))
            .collect();
        self.rules
            .iter()
            .filter(|r| rule_ids.contains(r.id.as_str()))
            .filter(|r| r.permission == Permission::Allow)
            .filter(|r| r.messages.contains(&message) && r.flows.contains(&flow))
            .any(|r| {
                r.key_exprs
                    .iter()
                    .filter_map(|k| keyexpr::new(k.as_str()).ok())
                    .any(|k| k.includes(target))
            })
    }

    /// The usernames the model has a subject for, in order.
    pub fn users(&self) -> impl Iterator<Item = &str> {
        self.subjects.iter().map(|s| s.id.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::Flow::{Egress, Ingress};
    use super::Message::*;
    use super::*;

    fn spec(name: &str, f: impl FnOnce(&mut ServiceSpec)) -> ServiceSpec {
        let mut s = ServiceSpec::new(name);
        f(&mut s);
        s
    }

    fn principal(user: &str, grants: Grants) -> Principal {
        Principal {
            user: user.into(),
            peer: format!("infra.local/ns/apps/sa/{user}"),
            grants,
        }
    }

    fn model() -> AclModel {
        let echo = spec("demo/echo", |s| {
            s.serves.push("demo/echo".into());
            s.publishes.push("app/demo/**".into());
            s.subscribes.push("app/robot/pose".into());
        });
        let caller = spec("demo/caller", |s| {
            s.calls.push("demo/echo".into());
            s.subscribes.push("app/demo/**".into());
        });
        AclModel::from_principals(&[
            principal("demo-echo", Grants::from_spec(&echo)),
            principal("demo-caller", Grants::from_spec(&caller)),
        ])
        .unwrap()
    }

    #[test]
    fn z19_a_service_publishes_its_declared_keys_only() {
        let m = model();
        for key in ["app/demo", "app/demo/x", "app/demo/x/y"] {
            assert!(m.permits("demo-echo", Put, Ingress, key), "{key}");
            assert!(m.permits("demo-echo", Delete, Ingress, key), "{key}");
        }
        for key in [
            "app/demox",
            "app/other",
            "app/**",
            "sys/reboot",
            "@/router/x",
        ] {
            assert!(!m.permits("demo-echo", Put, Ingress, key), "{key}");
        }
        assert!(
            !m.permits("demo-caller", Put, Ingress, "app/demo/x"),
            "a subscriber is not a publisher"
        );
        assert!(
            !m.permits("demo-echo", DeclareSubscriber, Egress, "app/demo/x"),
            "subscribers stay hidden"
        );
    }

    #[test]
    fn z19_a_service_subscribes_within_its_declared_filters_only() {
        let m = model();
        assert!(m.permits("demo-caller", DeclareSubscriber, Ingress, "app/demo/**"));
        assert!(m.permits("demo-caller", DeclareSubscriber, Ingress, "app/demo/x"));
        assert!(m.permits("demo-caller", Put, Egress, "app/demo/x"));
        assert!(
            !m.permits("demo-caller", DeclareSubscriber, Ingress, "app/**"),
            "intersecting is not included"
        );
        assert!(!m.permits("demo-caller", DeclareSubscriber, Ingress, "**"));
        assert!(!m.permits("demo-caller", Put, Egress, "app/robot/pose"));
        assert!(m.permits("demo-echo", Put, Egress, "app/robot/pose"));
        assert!(
            !m.permits("demo-echo", Put, Egress, "app/robot/pose/x"),
            "exact"
        );
    }

    #[test]
    fn z19_nobody_touches_sys_or_the_admin_space() {
        let m = model();
        for user in ["demo-echo", "demo-caller"] {
            for msg in [
                Put,
                Delete,
                DeclareSubscriber,
                Query,
                DeclareQueryable,
                Reply,
                LivelinessToken,
                DeclareLivelinessSubscriber,
                LivelinessQuery,
            ] {
                for flow in [Ingress, Egress] {
                    for key in ["sys/reboot", "sys/**", "@/**", "@/router/local"] {
                        assert!(
                            !m.permits(user, msg, flow, key),
                            "{user} {msg:?} {flow:?} {key}"
                        );
                    }
                }
            }
        }
        assert!(
            !m.permits("stranger", Put, Ingress, "app/demo/x"),
            "no subject"
        );
        assert!(
            !m.permits("demo-echo", Put, Ingress, "not a//key"),
            "invalid"
        );
    }

    #[test]
    fn z19_every_principal_can_browse_discovery() {
        let m = model();
        for user in ["demo-echo", "demo-caller"] {
            for key in ["@mw/svc/demo/echo/*", "@mw/svc/demo/echo/**", "@mw/svc/**"] {
                assert!(m.permits(user, DeclareLivelinessSubscriber, Ingress, key));
                assert!(m.permits(user, LivelinessQuery, Ingress, key));
            }
            assert!(m.permits(user, Query, Ingress, "@mw/svc/demo/echo/pod-1"));
            assert!(m.permits(user, LivelinessToken, Egress, "@mw/svc/demo/echo/pod-1"));
            assert!(m.permits(user, Reply, Egress, "@mw/svc/demo/echo/pod-1"));
        }
    }

    #[test]
    fn z19_only_the_server_of_a_kind_announces_and_answers_it() {
        let m = model();
        let svc = "@mw/svc/demo/echo/pod-1";
        assert!(m.permits("demo-echo", LivelinessToken, Ingress, svc));
        assert!(m.permits("demo-echo", DeclareQueryable, Ingress, svc));
        assert!(m.permits("demo-echo", Reply, Ingress, svc));
        assert!(m.permits("demo-echo", Query, Egress, svc));
        assert!(
            !m.permits(
                "demo-echo",
                LivelinessToken,
                Ingress,
                "@mw/svc/demo/echo/x/y"
            ),
            "exact kind: one instance chunk"
        );
        assert!(!m.permits("demo-echo", LivelinessToken, Ingress, "@mw/svc/cam/jpeg/u"));
        for msg in [LivelinessToken, DeclareQueryable, Reply] {
            assert!(!m.permits("demo-caller", msg, Ingress, svc), "{msg:?}");
        }
        let node = "@mw/node/0011223344556677/demo/echo";
        assert!(m.permits("demo-echo", DeclareQueryable, Ingress, "demo/echo"));
        assert!(m.permits("demo-echo", DeclareQueryable, Ingress, node));
        assert!(m.permits("demo-caller", Query, Ingress, "demo/echo"));
        assert!(m.permits("demo-caller", Query, Ingress, node));
        assert!(m.permits("demo-caller", Reply, Egress, node));
        assert!(!m.permits("demo-caller", DeclareQueryable, Ingress, node));
        assert!(
            !m.permits("demo-echo", Query, Ingress, "demo/echo"),
            "not a caller"
        );
    }

    #[test]
    fn z19_model_is_sorted_and_refuses_bad_input() {
        let m = model();
        assert_eq!(m.users().collect::<Vec<_>>(), ["demo-caller", "demo-echo"]);
        assert_eq!(m.policies.len(), 2);
        let ids: BTreeSet<&str> = m.rules.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids.len(), m.rules.len(), "unique rule ids");
        assert!(m.rules.iter().all(|r| !r.key_exprs.is_empty()));
        assert!(ids.contains("demo-echo/publish-in"));
        assert!(
            !ids.contains("demo-caller/publish-in"),
            "empty rules left out"
        );
        let p = principal("a", Grants::default());
        assert_eq!(
            AclModel::from_principals(&[p.clone(), p]),
            Err(RouterError::DuplicateUser("a".into()))
        );
        let bad = principal(
            "a",
            Grants {
                publishes: vec!["@mw/svc/**".into()],
                ..Grants::default()
            },
        );
        assert_eq!(
            AclModel::from_principals(&[bad]),
            Err(RouterError::InvalidGrant("@mw/svc/**".into()))
        );
        let merged = Grants::from_spec(&ServiceSpec::new("x")).merge(&Grants {
            announces: vec!["cam/**".into(), "cam/**".into()],
            ..Grants::default()
        });
        assert_eq!(merged.announces, ["cam/**"]);
    }
}
