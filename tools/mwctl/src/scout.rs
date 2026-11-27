//! `mwctl scout`: the middleware nodes on the local network (H4).
//!
//! Three sources go into one table:
//!
//! 1. **Zenoh scouting.** A multicast scout (UDP `224.0.0.224:7446`)
//!    collects the `Hello`s of every router and peer that answers scouts
//!    (zenohd does by default) with its locators ([`Heard`]).
//! 2. **Admin space.** A session connected to those locators asks
//!    `@/*/router` and `@/*/peer` for each one's sessions ([`Linked`],
//!    parsed by [`parse_admin`]). This is how clients appear, such as the
//!    RT1050 (its `zenoh_lite` client never answers a scout) or a
//!    mw-posix-node whose session has multicast scouting off.
//! 3. **`sys/info`.** One query on `@mw/node/*/sys/info` reaches every
//!    middleware node's per-node queryable; each reply is the node's id
//!    (from the key, [`node_of_key`]), its `sys/info` and the Zenoh id of
//!    the session that answered ([`Answer`]).
//!
//! [`merge`] joins them into [`Row`]s by Zenoh id and [`format`] prints
//! them. These are pure and unit tested; [`run`] (feature `zenoh`) is the
//! shell that does the network I/O.
//!
//! Not seen: nodes on ESP-NOW (not IP; ask a gateway's `peers`), sessions
//! behind a router that neither answers scouts nor is reached through one
//! that does, and anything on another subnet (multicast stays on the link).

use crate::device::Info;
use crate::report::status_name;
use middleware::SenderId;

/// Default scouting time of `mwctl scout`.
pub const DEFAULT_SECONDS: u64 = 3;

/// A Zenoh `Hello`: a router or peer that answered the scout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heard {
    /// Its Zenoh id, as Zenoh prints it.
    pub zid: String,
    /// `router`, `peer` or `client`.
    pub whatami: String,
    /// Its locators, e.g. `tcp/192.168.1.10:7447`.
    pub locators: Vec<String>,
}

/// A session that a router's or peer's admin space reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Linked {
    /// The Zenoh id of the router or peer reporting it.
    pub via: String,
    /// The session's Zenoh id.
    pub zid: String,
    /// `router`, `peer` or `client`.
    pub whatami: String,
    /// Its link's remote address, as the reporter sees it
    /// (`tcp/192.168.1.50:51234`), if any.
    pub addr: Option<String>,
}

/// One node's answer to `@mw/node/*/sys/info`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    /// The node id from the reply's key.
    pub node: SenderId,
    /// The Zenoh id of the session that replied: the one seen elsewhere
    /// whose node id is this one ([`attribute`]); `None` when no such
    /// session was seen (always so for a posix node, see [`attribute`]).
    pub zid: Option<String>,
    /// The decoded `sys/info`; `None` when the body did not decode.
    pub info: Option<Info>,
}

/// One line of the table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Zenoh id; `None` for a node whose reply named no session.
    pub zid: Option<String>,
    /// `router`, `peer`, `client`, or `?` when only `sys/info` saw it.
    pub whatami: String,
    /// Its addresses: its own locators, else the remote addresses its
    /// routers see.
    pub addrs: Vec<String>,
    /// The routers or peers it is connected to (only for sessions found
    /// through an admin space).
    pub via: Vec<String>,
    /// Its middleware `sys/info`, if it answered.
    pub node: Option<Answer>,
}

/// The node id in a reply key `@mw/node/<zid16>/sys/info`.
pub fn node_of_key(key: &str) -> Option<SenderId> {
    let rest = key.strip_prefix("@mw/node/")?;
    let (hex, tail) = rest.split_once('/')?;
    if tail != "sys/info" {
        return None;
    }
    crate::cli::parse_node(hex).map(SenderId)
}

/// The sessions in one admin-space reply (`@/<zid>/router` or
/// `@/<zid>/peer`): its `sessions` array of `{peer, whatami, links:
/// [{src, dst}]}`. `via` is the reporter's Zenoh id. Anything malformed is
/// skipped, never an error: the reply comes from the network.
#[cfg(feature = "zenoh")]
pub fn parse_admin(via: &str, json: &str) -> Vec<Linked> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let Some(sessions) = v.get("sessions").and_then(|s| s.as_array()) else {
        return Vec::new();
    };
    sessions
        .iter()
        .filter_map(|s| {
            let zid = s.get("peer")?.as_str()?;
            if zid == "unknown" {
                return None;
            }
            let whatami = s.get("whatami").and_then(|w| w.as_str()).unwrap_or("?");
            let addr = s
                .get("links")
                .and_then(|l| l.as_array())
                .and_then(|l| l.first())
                .and_then(|l| l.get("dst"))
                .and_then(|d| d.as_str())
                .map(str::to_owned);
            Some(Linked {
                via: via.to_owned(),
                zid: zid.to_owned(),
                whatami: whatami.to_owned(),
                addr,
            })
        })
        .collect()
}

/// Names the session behind each answer, in two steps:
///
/// 1. A node whose id is a function of its Zenoh id
///    (`middleware::zenoh::sender_id_of`, as on the RT1050's `zenoh_lite` and
///    a default `ZenohTransport`) is matched exactly: `ids` holds that pair
///    for every session seen. This wins, because such a client's reply can
///    reach us through a router, whose id would be the replier id.
/// 2. Otherwise the answer keeps the replier id it arrived with (`a.zid`,
///    zenoh's `Reply::replier_id`). This places a posix node, which declares
///    its queryables under its machine-id node id (`with_sender`,
///    `posix-node/src/node_id.rs`) that no session's hash equals.
pub fn attribute(answers: &mut [Answer], ids: &[(String, SenderId)]) {
    for a in answers {
        if let Some((z, _)) = ids.iter().find(|(_, n)| *n == a.node) {
            a.zid = Some(z.clone());
        }
    }
}

/// The Zenoh id of an admin-space key `@/<zid>/<whatami>`.
pub fn zid_of_admin_key(key: &str) -> Option<&str> {
    let rest = key.strip_prefix("@/")?;
    let (zid, whatami) = rest.split_once('/')?;
    matches!(whatami, "router" | "peer").then_some(zid)
}

fn kind_rank(whatami: &str) -> u8 {
    match whatami {
        "router" => 0,
        "peer" => 1,
        "client" => 2,
        _ => 3,
    }
}

/// Joins the three sources by Zenoh id, leaving out `own` (the scanning
/// session itself). Routers come first, then peers, clients and nodes seen
/// only by `sys/info`; within each, by Zenoh id, then node id.
pub fn merge(heard: &[Heard], linked: &[Linked], answers: &[Answer], own: &str) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    let find = |rows: &[Row], zid: &str| rows.iter().position(|r| r.zid.as_deref() == Some(zid));
    for h in heard.iter().filter(|h| h.zid != own) {
        let i = find(&rows, &h.zid).unwrap_or_else(|| {
            rows.push(Row {
                zid: Some(h.zid.clone()),
                whatami: h.whatami.clone(),
                addrs: Vec::new(),
                via: Vec::new(),
                node: None,
            });
            rows.len() - 1
        });
        for l in &h.locators {
            if !rows[i].addrs.contains(l) {
                rows[i].addrs.push(l.clone());
            }
        }
    }
    for l in linked.iter().filter(|l| l.zid != own) {
        let i = find(&rows, &l.zid).unwrap_or_else(|| {
            rows.push(Row {
                zid: Some(l.zid.clone()),
                whatami: l.whatami.clone(),
                addrs: Vec::new(),
                via: Vec::new(),
                node: None,
            });
            rows.len() - 1
        });
        let r = &mut rows[i];
        if !r.via.contains(&l.via) {
            r.via.push(l.via.clone());
        }
        // A session's own locators (from its Hello) say more than the
        // ephemeral port a router sees it connect from.
        let heard_itself = heard.iter().any(|h| h.zid == l.zid);
        if let Some(a) = &l.addr
            && !heard_itself
            && !r.addrs.contains(a)
        {
            r.addrs.push(a.clone());
        }
    }
    for a in answers {
        match a.zid.as_deref().and_then(|z| find(&rows, z)) {
            Some(i) if rows[i].node.is_none() => rows[i].node = Some(a.clone()),
            _ => rows.push(Row {
                zid: a.zid.clone(),
                whatami: "?".into(),
                addrs: Vec::new(),
                via: Vec::new(),
                node: Some(a.clone()),
            }),
        }
    }
    rows.sort_by(|x, y| {
        kind_rank(&x.whatami)
            .cmp(&kind_rank(&y.whatami))
            .then_with(|| x.zid.cmp(&y.zid))
            .then_with(|| {
                x.node
                    .as_ref()
                    .map(|n| n.node.0)
                    .cmp(&y.node.as_ref().map(|n| n.node.0))
            })
    });
    rows
}

fn hex8(id: SenderId) -> String {
    id.0.iter().map(|b| format!("{b:02x}")).collect()
}

/// Shortens a Zenoh id to its first 8 digits for the VIA column.
fn short(zid: &str) -> &str {
    zid.get(..8).unwrap_or(zid)
}

/// Longest cell printed, in characters; longer ones end in `…`.
pub const MAX_CELL: usize = 64;

/// One cell as it is safe to print. Every field but the node id comes from
/// the network (a scout reply, a router's admin space, a `sys/info`), so
/// control characters, bidi overrides and zero-width characters are shown
/// escaped (`\u{1b}`), never sent to the terminal, and the cell is cut at
/// [`MAX_CELL`] characters.
fn clean(s: &str) -> String {
    let hidden = |c: char| {
        c.is_control()
            || matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}'
                | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
    };
    let mut out = String::new();
    for (n, c) in s.chars().enumerate() {
        if n == MAX_CELL {
            out.push('…');
            break;
        }
        if hidden(c) {
            out.extend(c.escape_unicode());
        } else {
            out.push(c);
        }
    }
    out
}

/// The table: one line per row, columns aligned, then a count line. An
/// empty scan prints only the count line, which says nothing was found.
pub fn format(rows: &[Row]) -> String {
    const HEAD: [&str; 7] = [
        "KIND",
        "ZENOH ID",
        "ADDRESS",
        "VIA",
        "NODE",
        "CHIP",
        "FIRMWARE / PAYLOAD",
    ];
    let cells: Vec<[String; 7]> = rows
        .iter()
        .map(|r| {
            let (node, chip, fw) = match &r.node {
                Some(Answer {
                    node,
                    info: Some(i),
                    ..
                }) => (
                    hex8(*node),
                    i.chip.clone(),
                    format!("{} / {}", i.firmware, status_name(i.status)),
                ),
                Some(Answer {
                    node, info: None, ..
                }) => (hex8(*node), "-".into(), "(sys/info did not decode)".into()),
                None => ("-".into(), "-".into(), "-".into()),
            };
            [
                r.whatami.clone(),
                r.zid.clone().unwrap_or_else(|| "-".into()),
                if r.addrs.is_empty() {
                    "-".into()
                } else {
                    r.addrs.join(",")
                },
                // A session reported by no router or peer, but with an
                // address of its own, was reached directly.
                if r.via.is_empty() && !r.addrs.is_empty() {
                    "direct".into()
                } else if r.via.is_empty() {
                    "-".into()
                } else {
                    r.via.iter().map(|v| short(v)).collect::<Vec<_>>().join(",")
                },
                node,
                chip,
                fw,
            ]
        })
        .map(|c: [String; 7]| c.map(|s| clean(&s)))
        .collect();
    let mut out = String::new();
    if !cells.is_empty() {
        let mut w = HEAD.map(str::len);
        for c in &cells {
            for (k, s) in c.iter().enumerate() {
                w[k] = w[k].max(s.chars().count());
            }
        }
        let line = |out: &mut String, c: &[&str]| {
            let mut s = String::new();
            for (k, v) in c.iter().enumerate() {
                if k + 1 == c.len() {
                    s.push_str(v);
                } else {
                    s.push_str(&format!("{v:<width$}  ", width = w[k]));
                }
            }
            out.push_str(s.trim_end());
            out.push('\n');
        };
        line(&mut out, &HEAD);
        for c in &cells {
            line(&mut out, &c.each_ref().map(String::as_str));
        }
    }
    let sessions = rows.iter().filter(|r| r.zid.is_some()).count();
    let nodes = rows.iter().filter(|r| r.node.is_some()).count();
    out.push_str(&format!(
        "{sessions} Zenoh session{}, {nodes} middleware node{} answered sys/info\n",
        if sessions == 1 { "" } else { "s" },
        if nodes == 1 { "" } else { "s" },
    ));
    out
}

/// Scouts for `seconds`, then asks the admin spaces and `sys/info` of what
/// it found, and returns the merged rows (feature `zenoh`).
#[cfg(feature = "zenoh")]
pub fn run(seconds: u64) -> Result<Vec<Row>, crate::error::Error> {
    shell::run(seconds)
}

#[cfg(feature = "zenoh")]
mod shell {
    use super::{
        Answer, Heard, Linked, Row, attribute, merge, node_of_key, parse_admin, zid_of_admin_key,
    };
    use crate::device::Info;
    use crate::error::Error;
    use std::time::{Duration, Instant};
    use zenoh::config::WhatAmI;
    use zenoh::query::{ConsolidationMode, QueryTarget};

    /// How long the admin and `sys/info` queries wait for replies.
    const QUERY_WAIT: Duration = Duration::from_secs(2);
    /// Time for the session's connections to come up before querying.
    const SETTLE: Duration = Duration::from_millis(500);

    fn zerr(e: impl std::fmt::Display) -> Error {
        Error::Io(std::io::Error::other(format!("zenoh: {e}")))
    }

    pub(super) fn run(seconds: u64) -> Result<Vec<Row>, Error> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(Error::Io)?;
        rt.block_on(scan(Duration::from_secs(seconds)))
    }

    /// The tcp locators to connect to: every router's and peer's.
    fn endpoints(heard: &[Heard]) -> Vec<String> {
        let mut e: Vec<String> = heard
            .iter()
            .filter(|h| h.whatami != "client")
            .flat_map(|h| h.locators.iter())
            .filter(|l| l.starts_with("tcp/"))
            .cloned()
            .collect();
        e.sort();
        e.dedup();
        e
    }

    async fn scan(within: Duration) -> Result<Vec<Row>, Error> {
        // 1. Multicast scouting.
        let mut heard: Vec<Heard> = Vec::new();
        {
            let scout = zenoh::scout(
                WhatAmI::Router | WhatAmI::Peer | WhatAmI::Client,
                zenoh::Config::default(),
            )
            .await
            .map_err(zerr)?;
            let end = Instant::now() + within;
            while let Some(left) = end.checked_duration_since(Instant::now()) {
                match tokio::time::timeout(left, scout.recv_async()).await {
                    Ok(Ok(h)) => {
                        let zid = h.zid().to_string();
                        if heard.iter().all(|x| x.zid != zid) {
                            heard.push(Heard {
                                zid,
                                whatami: h.whatami().to_string(),
                                locators: h.locators().iter().map(|l| l.to_string()).collect(),
                            });
                        }
                    }
                    Ok(Err(_)) | Err(_) => break,
                }
            }
        }
        let connect = endpoints(&heard);
        if connect.is_empty() {
            return Ok(merge(&heard, &[], &[], ""));
        }
        // 2. A peer session on what the scout found; no scouting of its
        // own, no listener, and an unreachable locator is not fatal.
        let mut config = zenoh::Config::default();
        let list = serde_json::to_string(&connect).map_err(zerr)?;
        for (k, v) in [
            ("mode", "\"peer\""),
            ("scouting/multicast/enabled", "false"),
            ("scouting/gossip/enabled", "false"),
            ("listen/endpoints", "[]"),
            ("connect/endpoints", list.as_str()),
            ("connect/timeout_ms", "2000"),
            ("connect/exit_on_failure", "false"),
        ] {
            config.insert_json5(k, v).map_err(zerr)?;
        }
        let session = zenoh::open(config).await.map_err(zerr)?;
        let own = session.zid().to_string();
        tokio::time::sleep(SETTLE).await;
        // 3. Admin spaces: the sessions of every router and peer reached.
        let mut linked: Vec<Linked> = Vec::new();
        for sel in ["@/*/router", "@/*/peer"] {
            let replies = session
                .get(sel)
                .target(QueryTarget::All)
                .consolidation(ConsolidationMode::None)
                .timeout(QUERY_WAIT)
                .await
                .map_err(zerr)?;
            while let Ok(reply) = replies.recv_async().await {
                let Ok(sample) = reply.result() else { continue };
                let Some(via) = zid_of_admin_key(sample.key_expr().as_str()) else {
                    continue;
                };
                let json = sample.payload().try_to_string().unwrap_or_default();
                linked.extend(parse_admin(via, &json));
            }
        }
        // 4. Every middleware node's sys/info.
        let mut answers: Vec<Answer> = Vec::new();
        let replies = session
            .get("@mw/node/*/sys/info")
            .target(QueryTarget::All)
            .consolidation(ConsolidationMode::None)
            .timeout(QUERY_WAIT)
            .await
            .map_err(zerr)?;
        while let Ok(reply) = replies.recv_async().await {
            let Ok(sample) = reply.result() else { continue };
            let Some(node) = node_of_key(sample.key_expr().as_str()) else {
                continue;
            };
            if answers.iter().any(|a| a.node == node) {
                continue;
            }
            answers.push(Answer {
                node,
                // The session that answered; `attribute` prefers an exact
                // node-id match over it.
                zid: reply.replier_id().map(|g| g.zid().to_string()),
                info: Info::decode(&sample.payload().to_bytes()),
            });
        }
        session.close().await.map_err(zerr)?;
        let ids: Vec<(String, middleware::SenderId)> = heard
            .iter()
            .map(|h| h.zid.as_str())
            .chain(linked.iter().map(|l| l.zid.as_str()))
            .filter_map(|z| {
                let id: zenoh::session::ZenohId = z.parse().ok()?;
                Some((z.to_owned(), middleware::zenoh::sender_id_of(id)))
            })
            .collect();
        attribute(&mut answers, &ids);
        Ok(merge(&heard, &linked, &answers, &own))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn h4_connects_to_routers_and_peers_over_tcp_only() {
            let h = |zid: &str, w: &str, l: &[&str]| Heard {
                zid: zid.into(),
                whatami: w.into(),
                locators: l.iter().map(|s| (*s).to_owned()).collect(),
            };
            let heard = [
                h("a1", "router", &["tcp/10.0.0.1:7447", "udp/10.0.0.1:7447"]),
                h("b2", "peer", &["tcp/10.0.0.2:40001", "tcp/10.0.0.1:7447"]),
                h("c3", "client", &["tcp/10.0.0.3:1"]),
            ];
            assert_eq!(
                endpoints(&heard),
                ["tcp/10.0.0.1:7447", "tcp/10.0.0.2:40001"]
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::loader::{Caps, PayloadStatus};

    const NODE: SenderId = SenderId([0x31, 0x04, 0x41, 0xd2, 0x65, 0xf8, 0x24, 0x84]);

    fn info(chip: &str) -> Info {
        Info {
            abi_version: 8,
            capacity: 256 * 1024,
            status: PayloadStatus::Running,
            chip: chip.into(),
            firmware: "0.6.0".into(),
            node: Some(NODE),
            caps: Caps::NONE,
        }
    }

    fn heard(zid: &str, whatami: &str, locs: &[&str]) -> Heard {
        Heard {
            zid: zid.into(),
            whatami: whatami.into(),
            locators: locs.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn linked(via: &str, zid: &str, whatami: &str, addr: &str) -> Linked {
        Linked {
            via: via.into(),
            zid: zid.into(),
            whatami: whatami.into(),
            addr: Some(addr.into()),
        }
    }

    #[test]
    fn h4_node_ids_come_from_sys_info_reply_keys() {
        assert_eq!(
            node_of_key("@mw/node/310441d265f82484/sys/info"),
            Some(NODE)
        );
        for bad in [
            "@mw/node/310441d265f82484/sys/board",
            "@mw/node/310441d265f8248/sys/info",
            "@mw/svc/x/y",
            "mw/node/310441d265f82484/sys/info",
        ] {
            assert_eq!(node_of_key(bad), None, "{bad}");
        }
        assert_eq!(zid_of_admin_key("@/ab12/router"), Some("ab12"));
        assert_eq!(zid_of_admin_key("@/ab12/peer"), Some("ab12"));
        assert_eq!(zid_of_admin_key("@/ab12/router/linkstate"), None);
        assert_eq!(zid_of_admin_key("@/ab12/client"), None);
    }

    #[cfg(feature = "zenoh")]
    #[test]
    fn h4_admin_sessions_are_parsed_and_junk_is_skipped() {
        let json = r#"{"zid":"r1","sessions":[
            {"peer":"c9","whatami":"client","links":[{"src":"tcp/10.0.0.1:7447","dst":"tcp/10.0.0.9:50123"}]},
            {"peer":"unknown","whatami":"peer","links":[]},
            {"whatami":"peer"},
            {"peer":"p2","whatami":"peer","links":[]}
        ]}"#;
        assert_eq!(
            parse_admin("r1", json),
            [
                linked("r1", "c9", "client", "tcp/10.0.0.9:50123"),
                Linked {
                    via: "r1".into(),
                    zid: "p2".into(),
                    whatami: "peer".into(),
                    addr: None
                },
            ]
        );
        assert!(parse_admin("r1", "not json").is_empty());
        assert!(parse_admin("r1", "{\"sessions\":3}").is_empty());
    }

    #[test]
    fn h4_merge_joins_by_zenoh_id_and_leaves_out_the_scanner() {
        let heard = [
            heard("p2", "peer", &["tcp/10.0.0.2:7447"]),
            heard("r1", "router", &["tcp/10.0.0.1:7447"]),
        ];
        let linked = [
            linked("r1", "c9", "client", "tcp/10.0.0.9:50123"),
            linked("r1", "p2", "peer", "tcp/10.0.0.2:41000"),
            linked("r1", "me", "peer", "tcp/10.0.0.5:42000"),
        ];
        let answers = [
            Answer {
                node: NODE,
                zid: Some("c9".into()),
                info: Some(info("imxrt1052")),
            },
            Answer {
                node: SenderId([1; 8]),
                zid: None,
                info: None,
            },
        ];
        let rows = merge(&heard, &linked, &answers, "me");
        let ids: Vec<_> = rows
            .iter()
            .map(|r| (r.whatami.as_str(), r.zid.as_deref()))
            .collect();
        assert_eq!(
            ids,
            [
                ("router", Some("r1")),
                ("peer", Some("p2")),
                ("client", Some("c9")),
                ("?", None)
            ]
        );
        // A peer's own locator, not the port its router sees.
        assert_eq!(rows[1].addrs, ["tcp/10.0.0.2:7447"]);
        assert_eq!(rows[1].via, ["r1"]);
        // A client: the address its router sees, and its sys/info.
        assert_eq!(rows[2].addrs, ["tcp/10.0.0.9:50123"]);
        assert_eq!(rows[2].node.as_ref().map(|a| a.node), Some(NODE));
        assert!(rows.iter().all(|r| r.zid.as_deref() != Some("me")));
    }

    #[test]
    fn h4_answers_are_attributed_by_node_id_before_the_replier_id() {
        let mut answers = [
            // A zenoh_lite client's reply through a router: the replier id
            // is the router's, the node-id match names the client.
            Answer {
                node: NODE,
                zid: Some("router".into()),
                info: None,
            },
            // No node-id match: the replier id stays.
            Answer {
                node: SenderId([7; 8]),
                zid: Some("replier".into()),
                info: None,
            },
            // Neither: unattributed.
            Answer {
                node: SenderId([8; 8]),
                zid: None,
                info: None,
            },
        ];
        attribute(
            &mut answers,
            &[("aa".into(), SenderId([1; 8])), ("c9".into(), NODE)],
        );
        assert_eq!(answers[0].zid.as_deref(), Some("c9"));
        assert_eq!(answers[1].zid.as_deref(), Some("replier"));
        assert_eq!(answers[2].zid, None);
    }

    #[test]
    fn h4_a_posix_node_joins_its_session_by_replier_id() {
        // A posix node answers under its machine-id node id, which is not
        // the hash of any session's zid; the replier id (3fcc, the node's
        // own listening peer session, as seen on the x86 node) places it.
        let mut answers = [Answer {
            node: NODE,
            zid: Some("3fcc".into()),
            info: Some(info("posix")),
        }];
        attribute(
            &mut answers,
            &[
                ("3fcc".into(), SenderId([0xaa; 8])),
                ("c9".into(), SenderId([0xbb; 8])),
            ],
        );
        assert_eq!(answers[0].zid.as_deref(), Some("3fcc"));
        let rows = merge(
            &[heard("3fcc", "peer", &["tcp/192.168.1.123:7447"])],
            &[],
            &answers,
            "me",
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(
            format(&rows),
            "\
KIND  ZENOH ID  ADDRESS                 VIA     NODE              CHIP   FIRMWARE / PAYLOAD
peer  3fcc      tcp/192.168.1.123:7447  direct  310441d265f82484  posix  0.6.0 / running
1 Zenoh session, 1 middleware node answered sys/info
"
        );
    }

    #[test]
    fn h4_network_text_is_escaped_and_cut_before_it_reaches_the_terminal() {
        let row = Row {
            zid: Some("ab\u{1b}[2Jcd".into()),
            whatami: "router\u{202e}x".into(),
            addrs: vec!["x".repeat(MAX_CELL + 10)],
            via: Vec::new(),
            node: None,
        };
        let out = format(&[row]);
        assert!(
            !out.contains('\u{1b}') && !out.contains('\u{202e}'),
            "{out:?}"
        );
        assert!(out.contains("ab\\u{1b}[2Jcd"), "{out:?}");
        assert!(out.contains("router\\u{202e}x"), "{out:?}");
        assert!(
            out.contains(&format!("{}…", "x".repeat(MAX_CELL))),
            "{out:?}"
        );
        assert!(!out.contains(&"x".repeat(MAX_CELL + 1)), "{out:?}");
    }

    #[test]
    fn h4_the_table_is_aligned_and_says_when_nothing_was_found() {
        assert_eq!(
            format(&[]),
            "0 Zenoh sessions, 0 middleware nodes answered sys/info\n"
        );
        let rows = merge(
            &[heard("r1", "router", &["tcp/10.0.0.1:7447"])],
            &[linked("r1", "c9", "client", "tcp/10.0.0.9:50123")],
            &[Answer {
                node: NODE,
                zid: Some("c9".into()),
                info: Some(info("imxrt1052")),
            }],
            "me",
        );
        assert_eq!(
            format(&rows),
            "\
KIND    ZENOH ID  ADDRESS             VIA     NODE              CHIP       FIRMWARE / PAYLOAD
router  r1        tcp/10.0.0.1:7447   direct  -                 -          -
client  c9        tcp/10.0.0.9:50123  r1      310441d265f82484  imxrt1052  0.6.0 / running
2 Zenoh sessions, 1 middleware node answered sys/info
"
        );
    }
}
