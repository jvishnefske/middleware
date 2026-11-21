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
//! 4. **mDNS** (feature `mdns`). Beside the scouting, a one-shot querier
//!    asks for `_mw-node._tcp` each second and collects the `mw/node`
//!    records ([`MdnsNode`], `middleware::mdns::node`): the RT1050 and
//!    posix-nodes that announce themselves, including those behind a
//!    router nothing here reaches (the KD240's loopback zenohd). Nothing
//!    learned from mDNS is connected to, its failures are notes on stderr,
//!    and its text only fills its own cells.
//!
//! [`merge`] joins the first three into [`Row`]s by Zenoh id,
//! [`merge_mdns`] adds the records, and [`format()`] prints them. These are
//! pure and unit tested; [`run`] (feature `zenoh`) is the shell that does
//! the network I/O.
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
    /// session was seen.
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
    /// Its mDNS `mw/node` record, if one was matched to it ([`merge_mdns`]).
    pub mdns: Option<MdnsNode>,
    /// How many mDNS records claimed this row when more than one did (then
    /// none is attached, and each is an `mdns` row carrying the same
    /// count); 0 otherwise.
    pub mdns_conflict: usize,
}

/// A node's `_mw-node._tcp` record on mDNS (`middleware::mdns::node`):
/// unauthenticated text from the network, shown only in its own cells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdnsNode {
    /// The DNS-SD instance label, e.g. `posix-0102030405060708`.
    pub instance: String,
    /// Its address (from the A record), else the SRV host and port.
    pub addr: Option<String>,
    /// TXT `id`: the node id.
    pub id: SenderId,
    /// TXT `sender`: the id its Zenoh session is addressed by.
    pub sender: Option<SenderId>,
    /// TXT `chip`.
    pub chip: String,
    /// TXT `board`.
    pub board: String,
    /// TXT `fw`.
    pub fw: Option<String>,
    /// TXT `zenoh`: a locator it says it is reached through.
    pub zenoh: Option<String>,
}

/// Most mDNS records kept from one scan.
pub const MAX_MDNS: usize = 256;

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

/// Names the session behind each answer. A node's id is a function of
/// its Zenoh id (`middleware::zenoh::sender_id_of`, the same on the
/// RT1050's `zenoh_lite`), so `ids` holds that pair for every session seen
/// and an answer gets the Zenoh id whose node id it carries.
pub fn attribute(answers: &mut [Answer], ids: &[(String, SenderId)]) {
    for a in answers {
        a.zid = ids
            .iter()
            .find(|(_, n)| *n == a.node)
            .map(|(z, _)| z.clone());
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
        "mdns" => 4,
        _ => 3,
    }
}

/// Routers first, then peers, clients, nodes seen only by `sys/info` and
/// those seen only on mDNS; within each, by Zenoh id, node id, then mDNS
/// instance.
fn sort_rows(rows: &mut [Row]) {
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
            .then_with(|| {
                x.mdns
                    .as_ref()
                    .map(|m| &m.instance)
                    .cmp(&y.mdns.as_ref().map(|m| &m.instance))
            })
    });
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
                mdns: None,
                mdns_conflict: 0,
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
                mdns: None,
                mdns_conflict: 0,
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
                mdns: None,
                mdns_conflict: 0,
            }),
        }
    }
    sort_rows(&mut rows);
    rows
}

/// Joins the mDNS records `found` (at most [`MAX_MDNS`]) to `rows`. A
/// record claims the row whose sender id (its `sys/info` reply key, else
/// the id of its session's Zenoh id in `ids`) is the record's `sender`,
/// else the row whose `sys/info` reports the record's `id`. A row claimed
/// by exactly one record gets it; a row claimed by more gets none (TXT is
/// unauthenticated: the first to answer may be the spoof), and every
/// claimant becomes a row of its own, all marked with the count
/// ([`Row::mdns_conflict`]). A record that claims nothing is a row of its
/// own, kind `mdns`, after the others. Nothing else in a row changes.
pub fn merge_mdns(mut rows: Vec<Row>, found: &[MdnsNode], ids: &[(String, SenderId)]) -> Vec<Row> {
    let sender_of_session = |zid: &Option<String>| {
        zid.as_ref()
            .and_then(|z| ids.iter().find(|(i, _)| i == z).map(|(_, s)| *s))
    };
    let found = &found[..found.len().min(MAX_MDNS)];
    // Each record's claim, against the rows as Zenoh saw them.
    let claims: Vec<Option<usize>> = found
        .iter()
        .map(|f| {
            let by_sender = f.sender.and_then(|s| {
                rows.iter().position(|r| {
                    r.node.as_ref().is_some_and(|n| n.node == s)
                        || sender_of_session(&r.zid) == Some(s)
                })
            });
            by_sender.or_else(|| {
                rows.iter().position(|r| {
                    r.node.as_ref().is_some_and(|n| {
                        n.node == f.id || n.info.as_ref().and_then(|i| i.node) == Some(f.id)
                    })
                })
            })
        })
        .collect();
    let claimants = |i: usize| claims.iter().filter(|c| **c == Some(i)).count();
    let mut lone = Vec::new();
    for (f, claim) in found.iter().zip(&claims) {
        match claim.map(|i| (i, claimants(i))) {
            Some((i, 1)) => rows[i].mdns = Some(f.clone()),
            Some((i, n)) => {
                rows[i].mdns_conflict = n;
                lone.push((f, n));
            }
            None => lone.push((f, 0)),
        }
    }
    for (f, n) in lone {
        rows.push(Row {
            zid: None,
            whatami: "mdns".into(),
            addrs: Vec::new(),
            via: Vec::new(),
            node: None,
            mdns: Some(f.clone()),
            mdns_conflict: n,
        });
    }
    sort_rows(&mut rows);
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
/// the network (a scout reply, a router's admin space, a `sys/info`, an
/// mDNS record), so control characters, bidi marks and overrides
/// (including U+061C), zero-width characters, the line and paragraph
/// separators (U+2028, U+2029) and the soft hyphen (U+00AD) are shown
/// escaped (`\u{1b}`), never sent to the terminal, and the cell is cut at
/// [`MAX_CELL`] characters.
fn clean(s: &str) -> String {
    let hidden = |c: char| {
        c.is_control()
            || matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}'
                | '\u{2066}'..='\u{2069}' | '\u{FEFF}' | '\u{061C}'
                | '\u{2028}' | '\u{2029}' | '\u{00AD}')
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
    const HEAD: [&str; 8] = [
        "KIND",
        "ZENOH ID",
        "ADDRESS",
        "MDNS",
        "VIA",
        "NODE",
        "CHIP",
        "FIRMWARE / PAYLOAD",
    ];
    let cells: Vec<[String; 8]> = rows
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
                // Seen by nothing but mDNS: its record's cells.
                None => match &r.mdns {
                    Some(m) => (
                        hex8(m.sender.unwrap_or(m.id)),
                        format!("{} ({})", m.chip, m.board),
                        format!("{} / -", m.fw.as_deref().unwrap_or("?")),
                    ),
                    None => ("-".into(), "-".into(), "-".into()),
                },
            };
            let mdns = match &r.mdns {
                Some(m) => {
                    let addr = m.addr.as_deref().unwrap_or("?");
                    let cell = match &m.zenoh {
                        Some(z) => format!("{addr} via {z}"),
                        None => addr.to_owned(),
                    };
                    // First, so the cut at MAX_CELL never hides it.
                    if r.mdns_conflict > 0 {
                        format!("(conflict) {cell}")
                    } else {
                        cell
                    }
                }
                None if r.mdns_conflict > 0 => format!("conflict: {} records", r.mdns_conflict),
                None => "-".into(),
            };
            [
                r.whatami.clone(),
                r.zid.clone().unwrap_or_else(|| "-".into()),
                if r.addrs.is_empty() {
                    "-".into()
                } else {
                    r.addrs.join(",")
                },
                mdns,
                if r.via.is_empty() {
                    "-".into()
                } else {
                    r.via.iter().map(|v| short(v)).collect::<Vec<_>>().join(",")
                },
                node,
                chip,
                fw,
            ]
        })
        .map(|c: [String; 8]| c.map(|s| clean(&s)))
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
    let announced = rows.iter().filter(|r| r.mdns.is_some()).count();
    out.push_str(&format!(
        "{sessions} Zenoh session{}, {nodes} middleware node{} answered sys/info, {announced} announced on mDNS\n",
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

/// The mDNS browse of `scout` (features `zenoh` and `mdns`): a one-shot
/// querier for `_mw-node._tcp` beside the Zenoh scouting. It only reads:
/// nothing it learns is connected to.
#[cfg(all(feature = "zenoh", feature = "mdns"))]
mod mdns_shell {
    use super::{MAX_MDNS, MdnsNode};
    use middleware::discovery::{Browse, Endpoint, Event, Service};
    use middleware::key::KeyFilter;
    use middleware::mdns::node::{self, NodeTxt, NodeTxtError};
    use middleware::mdns::socket::{MAX_EVENT_LEN, MdnsSocket, MdnsSocketBuilder};
    use std::time::Duration;
    use tokio::time::Instant;

    /// How often the query is repeated (a one-shot querier sees only the
    /// answers to its own queries).
    const REQUERY: Duration = Duration::from_secs(1);
    /// Receive errors in a row after which the browse gives up.
    const MAX_ERRORS: u32 = 16;

    /// What a browse found, and what went wrong (for stderr).
    #[derive(Debug, Default)]
    pub(super) struct Browsed {
        pub nodes: Vec<MdnsNode>,
        /// Instances whose record is not a node's.
        pub skipped: usize,
        /// More than [`MAX_MDNS`] instances answered; the later ones were
        /// dropped ([`truncation_note`]).
        pub truncated: bool,
        pub notes: Vec<String>,
    }

    /// Browses with a socket from `builder` for `within`.
    pub(super) async fn browse_with(builder: MdnsSocketBuilder, within: Duration) -> Browsed {
        let mut sock = match builder.build().await {
            Ok(s) => s,
            Err(e) => {
                return Browsed {
                    notes: vec![format!("mdns: {e} (no mDNS browse)")],
                    ..Browsed::default()
                };
            }
        };
        let mut found = browse_on(&mut sock, within).await;
        if !cfg!(target_os = "linux") {
            found.notes.push(
                "mdns: answers are accepted from loopback and link-local sources only here \
                 (the LAN's subnets are read on Linux)"
                    .into(),
            );
        }
        found
    }

    /// [`browse_with`] the default socket: an ephemeral port on `0.0.0.0`,
    /// queries to `224.0.0.251:5353`, on-link answers only.
    pub(super) async fn browse(within: Duration) -> Browsed {
        browse_with(MdnsSocket::builder(), within).await
    }

    /// Queries at once and every second until `within` passes, collecting
    /// node records by instance (a goodbye removes one).
    pub(super) async fn browse_on<B: Browse>(sock: &mut B, within: Duration) -> Browsed
    where
        B::Error: std::fmt::Display,
    {
        let end = Instant::now() + within;
        let mut found = Browsed::default();
        let mut next_query = Instant::now();
        let mut errors = 0u32;
        // Instances that are not node records, each counted once.
        let mut skipped: Vec<String> = Vec::new();
        while Instant::now() < end {
            if Instant::now() >= next_query {
                if let Err(e) = sock.browse(KeyFilter::Exact(node::KIND.key())).await {
                    found.notes.push(format!("mdns: query: {e}"));
                    break;
                }
                next_query += REQUERY;
            }
            let mut buf = [0u8; MAX_EVENT_LEN];
            let event =
                match tokio::time::timeout_at(next_query.min(end), sock.next(&mut buf)).await {
                    Err(_) => continue,
                    Ok(Err(e)) => {
                        errors += 1;
                        if errors == MAX_ERRORS {
                            found.notes.push(format!("mdns: receive: {e}"));
                            break;
                        }
                        continue;
                    }
                    Ok(Ok(event)) => event,
                };
            errors = 0;
            match event {
                Event::Up(s) if s.kind == node::KIND && !s.is_goodbye() => match to_node(&s) {
                    Ok(n) => match found.nodes.iter().position(|m| m.instance == n.instance) {
                        Some(i) => found.nodes[i] = n,
                        None if found.nodes.len() < MAX_MDNS => found.nodes.push(n),
                        // Never silent: a flood could crowd real nodes out.
                        None => found.truncated = true,
                    },
                    Err(_) => {
                        if skipped.len() < MAX_MDNS && !skipped.iter().any(|i| i == s.instance) {
                            skipped.push(s.instance.to_owned());
                        }
                    }
                },
                Event::Down { instance, .. } => found.nodes.retain(|m| m.instance != instance),
                Event::Up(_) => {}
            }
        }
        found.skipped = skipped.len();
        found
    }

    /// The note when [`Browsed::truncated`].
    pub(super) fn truncation_note() -> String {
        format!("mdns: more than {MAX_MDNS} _mw-node._tcp instances; extra ones ignored")
    }

    /// A resolved instance as a node record.
    pub(super) fn to_node(s: &Service<'_>) -> Result<MdnsNode, NodeTxtError> {
        let t = NodeTxt::parse(s.txt().map_err(|_| NodeTxtError::Malformed("txt"))?)?;
        let addr = match s.endpoint {
            Endpoint::Socket(a) if a.port() == node::NO_PORT => Some(a.ip().to_string()),
            Endpoint::Socket(a) => Some(a.to_string()),
            Endpoint::Host { name, port } => Some(format!("{name}:{port}")),
            Endpoint::Locator(l) => Some(l.to_owned()),
            Endpoint::Local => None,
        };
        Ok(MdnsNode {
            instance: s.instance.to_owned(),
            addr,
            id: t.id,
            sender: t.sender,
            chip: t.chip.to_owned(),
            board: t.board.to_owned(),
            fw: t.fw.map(str::to_owned),
            zenoh: t.zenoh.map(str::to_owned),
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use middleware::SenderId;
        use middleware::discovery::{Announce, Endpoint, Service};
        use middleware::dns::wire::TxtWriter;
        use middleware::key::KeyFilter;
        use middleware::mdns::announce::MdnsAnnouncer;
        use middleware::mdns::node::{self, NodeTxt};
        use middleware::mdns::socket::MdnsSocket;
        use std::net::SocketAddr;

        const ID: SenderId = SenderId([1, 2, 3, 4, 5, 6, 7, 8]);
        const SENDER: SenderId = SenderId([0x28, 0x0b, 0x4b, 0x25, 0x02, 0x17, 0x64, 0xe2]);

        fn at(ip: &str) -> Endpoint<'static> {
            Endpoint::Socket(format!("{ip}:0").parse().unwrap())
        }

        /// A browser and an announcer aimed at each other on loopback (no
        /// multicast); the announcer answers in the background.
        async fn pair(
            services: Vec<(String, Vec<u8>)>,
        ) -> (MdnsSocket, tokio::task::JoinHandle<()>) {
            let announcer_socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            let browser = MdnsSocket::builder()
                .bind("127.0.0.1:0".parse().unwrap())
                .destination(announcer_socket.local_addr().unwrap())
                .build()
                .await
                .unwrap();
            let mut a = MdnsAnnouncer::builder()
                .destination(browser.local_addr().unwrap())
                .host("posix-0102030405060708.local")
                .build_on(announcer_socket)
                .unwrap();
            let task = tokio::spawn(async move {
                for (label, txt) in &services {
                    let s = node::service(label, at("127.0.0.1"), txt, ID);
                    a.announce(&s).await.unwrap();
                }
                loop {
                    if a.step().await.is_err() {
                        return;
                    }
                }
            });
            (browser, task)
        }

        fn record(chip: &str) -> Vec<u8> {
            let mut out = [0u8; 192];
            NodeTxt {
                id: ID,
                chip,
                board: "unknown",
                fw: Some("0.6.0"),
                sender: Some(SENDER),
                zenoh: None,
            }
            .encode(&mut out)
            .unwrap()
            .to_vec()
        }

        #[tokio::test]
        async fn d35_browse_collects_nodes_from_a_loopback_announcer() {
            let (mut b, task) = pair(vec![(
                "posix-0102030405060708".into(),
                record("linux-x86_64"),
            )])
            .await;
            let found = browse_on(&mut b, Duration::from_millis(2500)).await;
            task.abort();
            assert_eq!(
                found.nodes,
                [MdnsNode {
                    instance: "posix-0102030405060708".into(),
                    addr: Some("127.0.0.1".into()),
                    id: ID,
                    sender: Some(SENDER),
                    chip: "linux-x86_64".into(),
                    board: "unknown".into(),
                    fw: Some("0.6.0".into()),
                    zenoh: None,
                }]
            );
            assert_eq!(found.skipped, 0);
            assert!(found.notes.is_empty(), "{:?}", found.notes);
        }

        #[tokio::test]
        async fn d38_a_hostile_announcer_is_parsed_or_skipped() {
            let mut bad = [0u8; 64];
            let bad = TxtWriter::new(&mut bad)
                .entry(b"txtvers", b"1")
                .unwrap()
                .entry(b"id", b"NOT-HEX")
                .unwrap()
                .entry(b"chip", b"x")
                .unwrap()
                .entry(b"board", b"y")
                .unwrap()
                .finish()
                .as_bytes()
                .to_vec();
            let (mut b, task) = pair(vec![
                ("posix-bad".into(), bad),
                ("posix-hostile".into(), record("\u{1b}[2J\u{202e}evil")),
            ])
            .await;
            let found = browse_on(&mut b, Duration::from_millis(2500)).await;
            task.abort();
            assert_eq!(found.nodes.len(), 1, "{:?}", found.nodes);
            assert_eq!(found.nodes[0].instance, "posix-hostile");
            assert!(found.skipped >= 1);
            let out =
                super::super::format(&super::super::merge_mdns(Vec::new(), &found.nodes, &[]));
            assert!(
                !out.contains('\u{1b}') && !out.contains('\u{202e}'),
                "{out:?}"
            );
            assert!(out.contains("\\u{1b}[2J\\u{202e}evil"), "{out:?}");
        }

        #[tokio::test]
        async fn d35_a_bind_error_is_a_note_not_an_error() {
            // TEST-NET-3: on no interface of this host.
            let builder =
                MdnsSocket::builder().bind("203.0.113.1:0".parse::<SocketAddr>().unwrap());
            let found = browse_with(builder, Duration::from_millis(200)).await;
            assert!(found.nodes.is_empty());
            assert_eq!(found.notes.len(), 1, "{:?}", found.notes);
            assert!(found.notes[0].starts_with("mdns: "), "{:?}", found.notes);
        }

        /// A browser that hands out `count` distinct node records, one per
        /// `next`, then waits.
        struct Flood {
            count: u32,
            sent: u32,
            txt: Vec<u8>,
        }

        impl Browse for Flood {
            type Error = std::io::Error;

            async fn browse(&mut self, _: KeyFilter<'_>) -> Result<(), std::io::Error> {
                Ok(())
            }

            async fn next<'b>(&mut self, buf: &'b mut [u8]) -> Result<Event<'b>, std::io::Error> {
                if self.sent == self.count {
                    std::future::pending::<()>().await;
                }
                self.sent += 1;
                let label = format!("posix-{:016x}", self.sent);
                let (name, rest) = buf.split_at_mut(label.len());
                name.copy_from_slice(label.as_bytes());
                rest[..self.txt.len()].copy_from_slice(&self.txt);
                let txt = &rest[..self.txt.len()];
                let instance = std::str::from_utf8(name).unwrap();
                Ok(Event::Up(node::service(instance, at("10.0.0.9"), txt, ID)))
            }

            fn try_next<'b>(
                &mut self,
                _: &'b mut [u8],
            ) -> Result<Option<Event<'b>>, std::io::Error> {
                Ok(None)
            }
        }

        #[tokio::test]
        async fn d38_more_than_256_instances_are_cut_with_a_note() {
            let mut flood = Flood {
                count: MAX_MDNS as u32 + 40,
                sent: 0,
                txt: record("linux-aarch64"),
            };
            let found = browse_on(&mut flood, Duration::from_millis(300)).await;
            assert_eq!(flood.sent, MAX_MDNS as u32 + 40);
            assert_eq!(found.nodes.len(), MAX_MDNS);
            assert!(found.truncated);
            // Exactly at the cap: nothing was cut.
            let mut flood = Flood {
                count: MAX_MDNS as u32,
                sent: 0,
                txt: record("linux-aarch64"),
            };
            let found = browse_on(&mut flood, Duration::from_millis(300)).await;
            assert_eq!(found.nodes.len(), MAX_MDNS);
            assert!(!found.truncated);
            assert_eq!(
                truncation_note(),
                "mdns: more than 256 _mw-node._tcp instances; extra ones ignored"
            );
        }

        #[test]
        fn d38_a_goodbye_or_a_non_node_record_is_not_a_node() {
            let txt = record("linux-aarch64");
            let s = node::service("posix-0102030405060708", at("10.0.0.2"), &txt, ID);
            assert_eq!(to_node(&s).unwrap().addr.as_deref(), Some("10.0.0.2"));
            let host = Service {
                endpoint: Endpoint::Host {
                    name: "posix-x.local",
                    port: 0,
                },
                ..s
            };
            assert_eq!(
                to_node(&host).unwrap().addr.as_deref(),
                Some("posix-x.local:0")
            );
            let junk = Service {
                txt: b"\x05ver=5",
                ..s
            };
            assert!(to_node(&junk).is_err());
        }
    }
}

#[cfg(feature = "zenoh")]
mod shell {
    use super::{
        Answer, Heard, Linked, MdnsNode, Row, attribute, merge, merge_mdns, node_of_key,
        parse_admin, zid_of_admin_key,
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

    /// The mDNS browse running beside the Zenoh scan (feature `mdns`).
    struct MdnsTask(#[cfg(feature = "mdns")] tokio::task::JoinHandle<super::mdns_shell::Browsed>);

    impl MdnsTask {
        fn start(within: Duration) -> Self {
            #[cfg(feature = "mdns")]
            return MdnsTask(tokio::spawn(super::mdns_shell::browse(within)));
            #[cfg(not(feature = "mdns"))]
            {
                let _ = within;
                MdnsTask()
            }
        }

        /// What it found; its notes go to stderr, never an error.
        async fn finish(self) -> Vec<MdnsNode> {
            #[cfg(feature = "mdns")]
            {
                let found = match self.0.await {
                    Ok(f) => f,
                    Err(e) => {
                        eprintln!("mdns: {e}");
                        return Vec::new();
                    }
                };
                for note in &found.notes {
                    eprintln!("{note}");
                }
                if found.truncated {
                    eprintln!("{}", super::mdns_shell::truncation_note());
                }
                if found.skipped > 0 {
                    eprintln!(
                        "mdns: skipped {} _mw-node._tcp record(s) that are not node records",
                        found.skipped
                    );
                }
                found.nodes
            }
            #[cfg(not(feature = "mdns"))]
            Vec::new()
        }
    }

    /// The node id of every session seen, from its Zenoh id.
    fn session_ids(heard: &[Heard], linked: &[Linked]) -> Vec<(String, middleware::SenderId)> {
        heard
            .iter()
            .map(|h| h.zid.as_str())
            .chain(linked.iter().map(|l| l.zid.as_str()))
            .filter_map(|z| {
                let id: zenoh::session::ZenohId = z.parse().ok()?;
                Some((z.to_owned(), middleware::zenoh::sender_id_of(id)))
            })
            .collect()
    }

    async fn scan(within: Duration) -> Result<Vec<Row>, Error> {
        // 0. The mDNS browse, for as long as the scouting (feature `mdns`).
        let mdns = MdnsTask::start(within);
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
            let rows = merge(&heard, &[], &[], "");
            let ids = session_ids(&heard, &[]);
            return Ok(merge_mdns(rows, &mdns.finish().await, &ids));
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
                zid: None,
                info: Info::decode(&sample.payload().to_bytes()),
            });
        }
        session.close().await.map_err(zerr)?;
        let ids = session_ids(&heard, &linked);
        attribute(&mut answers, &ids);
        let rows = merge(&heard, &linked, &answers, &own);
        Ok(merge_mdns(rows, &mdns.finish().await, &ids))
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
    fn h4_answers_are_attributed_by_node_id() {
        let mut answers = [
            Answer {
                node: NODE,
                zid: None,
                info: None,
            },
            Answer {
                node: SenderId([7; 8]),
                zid: Some("stale".into()),
                info: None,
            },
        ];
        attribute(
            &mut answers,
            &[("aa".into(), SenderId([1; 8])), ("c9".into(), NODE)],
        );
        assert_eq!(answers[0].zid.as_deref(), Some("c9"));
        assert_eq!(answers[1].zid, None);
    }

    #[test]
    fn h4_network_text_is_escaped_and_cut_before_it_reaches_the_terminal() {
        let row = Row {
            zid: Some("ab\u{1b}[2Jcd".into()),
            whatami: "router\u{202e}x".into(),
            addrs: vec!["x".repeat(MAX_CELL + 10)],
            via: Vec::new(),
            node: None,
            mdns: None,
            mdns_conflict: 0,
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
            "0 Zenoh sessions, 0 middleware nodes answered sys/info, 0 announced on mDNS\n"
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
KIND    ZENOH ID  ADDRESS             MDNS  VIA  NODE              CHIP       FIRMWARE / PAYLOAD
router  r1        tcp/10.0.0.1:7447   -     -    -                 -          -
client  c9        tcp/10.0.0.9:50123  -     r1   310441d265f82484  imxrt1052  0.6.0 / running
2 Zenoh sessions, 1 middleware node answered sys/info, 0 announced on mDNS
"
        );
    }

    const POSIX: SenderId = SenderId([1, 2, 3, 4, 5, 6, 7, 8]);
    const POSIX_SENDER: SenderId = SenderId([0x28, 0x0b, 0x4b, 0x25, 0x02, 0x17, 0x64, 0xe2]);

    fn found(instance: &str, id: SenderId, sender: Option<SenderId>) -> MdnsNode {
        MdnsNode {
            instance: instance.into(),
            addr: Some("192.168.1.20".into()),
            id,
            sender,
            chip: "linux-aarch64".into(),
            board: "ZynqMP KD240 revA/B/1".into(),
            fw: Some("0.6.0".into()),
            zenoh: None,
        }
    }

    /// A router, an RT1050 client that answered sys/info (sender = node
    /// id), a posix-node client that answered (sender != node id) and a
    /// client that did not answer.
    fn zenoh_rows() -> Vec<Row> {
        let posix_info = Info {
            node: Some(POSIX),
            ..info("linux-aarch64")
        };
        merge(
            &[heard("r1", "router", &["tcp/10.0.0.1:7447"])],
            &[
                linked("r1", "c9", "client", "tcp/10.0.0.9:50123"),
                linked("r1", "c7", "client", "tcp/10.0.0.7:50124"),
                linked("r1", "c5", "client", "tcp/10.0.0.5:50125"),
            ],
            &[
                Answer {
                    node: NODE,
                    zid: Some("c9".into()),
                    info: Some(info("imxrt1052")),
                },
                Answer {
                    node: POSIX_SENDER,
                    zid: Some("c7".into()),
                    info: Some(posix_info),
                },
            ],
            "me",
        )
    }

    fn row<'r>(rows: &'r [Row], zid: &str) -> &'r Row {
        rows.iter().find(|r| r.zid.as_deref() == Some(zid)).unwrap()
    }

    #[test]
    fn d36_merge_by_sender_matches_the_zenoh_row() {
        // Through the sys/info reply key.
        let f = found("posix-0102030405060708", POSIX, Some(POSIX_SENDER));
        let rows = merge_mdns(zenoh_rows(), std::slice::from_ref(&f), &[]);
        assert_eq!(rows.len(), 4);
        assert_eq!(row(&rows, "c7").mdns.as_ref(), Some(&f));
        // Through the session's id, for a session that did not answer.
        let quiet = SenderId([5; 8]);
        let g = found("posix-0505050505050505", SenderId([6; 8]), Some(quiet));
        let rows = merge_mdns(
            zenoh_rows(),
            std::slice::from_ref(&g),
            &[("c5".into(), quiet)],
        );
        assert_eq!(rows.len(), 4);
        assert_eq!(row(&rows, "c5").mdns.as_ref(), Some(&g));
        // The RT1050: sender and node id are one.
        let h = found("rt1050-310441d265f82484", NODE, Some(NODE));
        let rows = merge_mdns(zenoh_rows(), std::slice::from_ref(&h), &[]);
        assert_eq!(row(&rows, "c9").mdns.as_ref(), Some(&h));
    }

    #[test]
    fn d36_fallback_to_node_id_uses_info_node() {
        for sender in [None, Some(SenderId([0xee; 8]))] {
            let f = found("posix-0102030405060708", POSIX, sender);
            let rows = merge_mdns(zenoh_rows(), std::slice::from_ref(&f), &[]);
            assert_eq!(rows.len(), 4, "{sender:?}");
            // The posix-node's sys/info names its node id, not its sender id.
            assert_eq!(row(&rows, "c7").mdns.as_ref(), Some(&f), "{sender:?}");
        }
        // The reply key's id counts too.
        let f = found("rt1050-310441d265f82484", NODE, None);
        let rows = merge_mdns(zenoh_rows(), std::slice::from_ref(&f), &[]);
        assert_eq!(row(&rows, "c9").mdns.as_ref(), Some(&f));
    }

    #[test]
    fn d36_unmatched_mdns_nodes_become_their_own_rows() {
        let b = found(
            "posix-bbbbbbbbbbbbbbbb",
            SenderId([0xbb; 8]),
            Some(SenderId([0xbc; 8])),
        );
        let a = found("posix-aaaaaaaaaaaaaaaa", SenderId([0xaa; 8]), None);
        let rows = merge_mdns(zenoh_rows(), &[b.clone(), a.clone()], &[]);
        let kinds: Vec<_> = rows.iter().map(|r| r.whatami.as_str()).collect();
        assert_eq!(
            kinds,
            ["router", "client", "client", "client", "mdns", "mdns"]
        );
        assert_eq!(rows[4].mdns.as_ref(), Some(&a));
        assert_eq!(rows[5].mdns.as_ref(), Some(&b));
        for r in &rows[4..] {
            assert_eq!(
                (&r.zid, &r.node, r.addrs.len(), r.via.len()),
                (&None, &None, 0, 0)
            );
        }
        // Into an empty scan.
        let rows = merge_mdns(Vec::new(), std::slice::from_ref(&a), &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].whatami, "mdns");
    }

    #[test]
    fn d37_rival_claims_attach_neither_in_either_order() {
        let real = found("posix-0102030405060708", POSIX, Some(POSIX_SENDER));
        let spoof = MdnsNode {
            instance: "posix-evil".into(),
            chip: "spoofed".into(),
            ..real.clone()
        };
        for order in [[real.clone(), spoof.clone()], [spoof.clone(), real.clone()]] {
            let rows = merge_mdns(zenoh_rows(), &order, &[]);
            assert_eq!(rows.len(), 6);
            let c7 = row(&rows, "c7");
            assert_eq!((c7.mdns.as_ref(), c7.mdns_conflict), (None, 2));
            let lone: Vec<_> = rows[4..]
                .iter()
                .map(|r| (r.whatami.as_str(), r.mdns.as_ref(), r.mdns_conflict))
                .collect();
            assert_eq!(lone, [("mdns", Some(&real), 2), ("mdns", Some(&spoof), 2)]);
            let out = format(&rows);
            let line = out.lines().find(|l| l.starts_with("client  c7")).unwrap();
            assert!(line.contains("conflict: 2 records"), "{line}");
            assert_eq!(
                out.lines()
                    .filter(|l| l.starts_with("mdns") && l.contains("(conflict) 192.168.1.20"))
                    .count(),
                2,
                "{out}"
            );
        }
    }

    #[test]
    fn d37_a_sender_claim_and_an_id_claim_on_one_row_conflict() {
        let by_sender = found("posix-a", SenderId([0xaa; 8]), Some(POSIX_SENDER));
        let by_id = found("posix-b", POSIX, None);
        for order in [
            [by_sender.clone(), by_id.clone()],
            [by_id.clone(), by_sender.clone()],
        ] {
            let rows = merge_mdns(zenoh_rows(), &order, &[]);
            let c7 = row(&rows, "c7");
            assert_eq!((c7.mdns.as_ref(), c7.mdns_conflict), (None, 2));
            assert_eq!(rows.iter().filter(|r| r.whatami == "mdns").count(), 2);
        }
        // One claim on each of two rows is no conflict.
        let rt = found("rt1050-310441d265f82484", NODE, Some(NODE));
        let rows = merge_mdns(zenoh_rows(), &[by_id.clone(), rt.clone()], &[]);
        assert_eq!(row(&rows, "c7").mdns.as_ref(), Some(&by_id));
        assert_eq!(row(&rows, "c9").mdns.as_ref(), Some(&rt));
        assert!(rows.iter().all(|r| r.mdns_conflict == 0));
    }

    #[test]
    fn d37_mdns_never_overwrites_zenoh_cells() {
        let before = zenoh_rows();
        let f = MdnsNode {
            chip: "other".into(),
            fw: Some("9.9.9".into()),
            ..found("posix-0102030405060708", POSIX, Some(POSIX_SENDER))
        };
        let after = merge_mdns(before.clone(), std::slice::from_ref(&f), &[]);
        for (b, a) in before.iter().zip(&after) {
            assert_eq!(
                Row {
                    mdns: None,
                    ..a.clone()
                },
                *b
            );
        }
        let out = format(&after);
        let c7 = out.lines().find(|l| l.starts_with("client  c7")).unwrap();
        assert!(
            c7.contains("linux-aarch64") && c7.contains("0.6.0 / running"),
            "{c7}"
        );
        assert!(!c7.contains("other") && !c7.contains("9.9.9"), "{c7}");
        assert!(c7.contains("192.168.1.20"), "{c7}");
    }

    #[test]
    fn d38_hostile_txt_is_escaped_and_cut() {
        let long = "z".repeat(100);
        let f = MdnsNode {
            instance: "x\u{1b}[2J".into(),
            addr: Some("10.0.0.1\u{200b}".into()),
            id: SenderId([0xab; 8]),
            sender: None,
            chip: "\u{1b}[2Jchip".into(),
            board: "b\0d".into(),
            fw: Some("fw\u{202e}1".into()),
            zenoh: Some(format!("tcp/{long}\u{202e}")),
        };
        let out = format(&merge_mdns(Vec::new(), &[f], &[]));
        assert!(
            !out.chars()
                .any(|c| (c.is_control() && c != '\n') || matches!(c, '\u{200b}' | '\u{202e}')),
            "{out:?}"
        );
        for escaped in [
            "\\u{1b}[2Jchip",
            "b\\u{0}d",
            "fw\\u{202e}1",
            "10.0.0.1\\u{200b}",
        ] {
            assert!(out.contains(escaped), "{escaped} in {out:?}");
        }
        // Cut at MAX_CELL characters of the record's text.
        assert!(
            out.contains("z…") && !out.contains(&"z".repeat(MAX_CELL)),
            "{out:?}"
        );
    }

    #[test]
    fn d38_line_breaks_soft_hyphens_and_the_arabic_mark_are_escaped() {
        for (c, shown) in [
            ('\u{061c}', "\\u{61c}"),
            ('\u{2028}', "\\u{2028}"),
            ('\u{2029}', "\\u{2029}"),
            ('\u{00ad}', "\\u{ad}"),
        ] {
            let f = MdnsNode {
                chip: format!("a{c}b"),
                ..found("posix-0102030405060708", POSIX, None)
            };
            let out = format(&merge_mdns(Vec::new(), &[f], &[]));
            assert!(!out.contains(c), "{c:?} in {out:?}");
            assert!(out.contains(&format!("a{shown}b")), "{shown} in {out:?}");
        }
    }

    #[test]
    fn d38_at_most_256_are_kept() {
        let many: Vec<MdnsNode> = (0..300u32)
            .map(|i| {
                let mut id = [0u8; 8];
                id[..4].copy_from_slice(&i.to_be_bytes());
                found(&format!("n{i:03}"), SenderId(id), None)
            })
            .collect();
        assert_eq!(merge_mdns(Vec::new(), &many, &[]).len(), MAX_MDNS);
    }

    #[test]
    fn d37_the_table_golden() {
        let posix = MdnsNode {
            zenoh: Some("tcp/10.0.0.1:7447".into()),
            ..found("posix-0102030405060708", POSIX, Some(POSIX_SENDER))
        };
        let lone = MdnsNode {
            addr: Some("192.168.1.30".into()),
            fw: None,
            ..found(
                "posix-0909090909090909",
                SenderId([9; 8]),
                Some(SenderId([0x0a; 8])),
            )
        };
        let rows = merge_mdns(zenoh_rows(), &[posix, lone], &[]);
        assert_eq!(
            format(&rows),
            "\
KIND    ZENOH ID  ADDRESS             MDNS                                VIA  NODE              CHIP                                   FIRMWARE / PAYLOAD
router  r1        tcp/10.0.0.1:7447   -                                   -    -                 -                                      -
client  c5        tcp/10.0.0.5:50125  -                                   r1   -                 -                                      -
client  c7        tcp/10.0.0.7:50124  192.168.1.20 via tcp/10.0.0.1:7447  r1   280b4b25021764e2  linux-aarch64                          0.6.0 / running
client  c9        tcp/10.0.0.9:50123  -                                   r1   310441d265f82484  imxrt1052                              0.6.0 / running
mdns    -         -                   192.168.1.30                        -    0a0a0a0a0a0a0a0a  linux-aarch64 (ZynqMP KD240 revA/B/1)  ? / -
4 Zenoh sessions, 2 middleware nodes answered sys/info, 2 announced on mDNS
"
        );
    }
}
