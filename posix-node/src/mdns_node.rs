//! The node's mDNS announcement (`MW_POSIX_NODE_MDNS`, D27–D34): the
//! shared `mw/node` record (`middleware::mdns::node`) of this node on the
//! LAN, so `avahi-browse -rt _mw-node._tcp` and `mwctl scout` find it.
//!
//! * [`Identity`] — what the record says, built purely from the node's
//!   [`Facts`](crate::service::Facts), its Zenoh session's sender id and its
//!   router: TXT `txtvers=1`, `id`, `chip`, `board` (`unknown` without a
//!   device tree), `fw`, `sender` and, when LAN peers could use it,
//!   `zenoh` ([`advertised_locator`]).
//!
//! A Zenoh client listens on nothing, so the SRV port is
//! `mdns::node::NO_PORT` and the A record is the node's own address.

use std::io;
use std::net::Ipv4Addr;

use middleware::SenderId;
use middleware::discovery::directory::MAX_TXT_LEN;
use middleware::dns::wire::WireError;
use middleware::mdns::node::{self, NodeTxt};

use crate::service::Facts;

/// The instance label's prefix: `posix-<node id>`.
pub const LABEL_PREFIX: &str = "posix";

/// The board named when the device tree names none.
pub const UNKNOWN_BOARD: &str = "unknown";

/// What the node's record says (module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The node id (`id`).
    pub node: SenderId,
    /// The id its Zenoh session is addressed by (`sender`).
    pub sender: SenderId,
    /// `chip`.
    pub chip: &'static str,
    /// `board`: the device tree's, or [`UNKNOWN_BOARD`].
    pub board: String,
    /// `fw`: the node's version.
    pub firmware: &'static str,
    /// `zenoh`: [`advertised_locator`] of the router.
    pub zenoh: Option<String>,
}

impl Identity {
    /// The identity of the node with `facts`, whose session has `sender`
    /// and connects to `router`. Pure.
    pub fn new(facts: &Facts, sender: SenderId, router: &str) -> Self {
        Identity {
            node: facts.node,
            sender,
            chip: facts.chip,
            board: facts.board.as_ref().and_then(|b| b.as_str()).unwrap_or(UNKNOWN_BOARD).to_owned(),
            firmware: facts.firmware,
            zenoh: advertised_locator(router),
        }
    }

    /// The instance label, `posix-<node id as 16 hex digits>`: per node,
    /// so the unit and process nodes of one host never clash, and never
    /// the host's name (the system's mDNS daemon owns that).
    pub fn label(&self) -> String {
        node::label(LABEL_PREFIX, self.node).expect("a short lower-case prefix").as_str().to_owned()
    }

    /// The SRV target and A record name, `<label>.local`.
    pub fn host(&self) -> String {
        node::host(&self.label()).expect("a 22-byte label").as_str().to_owned()
    }

    /// The TXT attributes.
    pub fn record(&self) -> NodeTxt<'_> {
        NodeTxt {
            id: self.node,
            chip: self.chip,
            board: &self.board,
            fw: Some(self.firmware),
            sender: Some(self.sender),
            zenoh: self.zenoh.as_deref(),
        }
    }

    /// The TXT RDATA in `out`; without `zenoh` when the record would not
    /// fit (the locator is the one optional key that can be long), so the
    /// announcement never fails on it.
    pub fn txt<'b>(&self, out: &'b mut [u8; MAX_TXT_LEN]) -> Result<&'b [u8], WireError> {
        // Measured first: a failed encode leaves `out` borrowed.
        let mut scratch = [0u8; MAX_TXT_LEN];
        let record = match self.record().encode(&mut scratch) {
            Err(WireError::BufferTooSmall) => NodeTxt { zenoh: None, ..self.record() },
            _ => self.record(),
        };
        record.encode(out)
    }
}

/// What to do after probing the address: keep the announcement, announce
/// on a new address (a goodbye first if one is held), or withdraw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Nothing changes.
    Keep,
    /// Announce on this address.
    Reannounce(Ipv4Addr),
    /// Say goodbye and announce nothing until an address is back.
    Withdraw,
}

/// The step after probing the address (`probed`) while `current` is
/// announced (`None`: nothing is). Pure.
pub fn next_action(current: Option<Ipv4Addr>, probed: io::Result<Ipv4Addr>) -> Action {
    match (current, probed) {
        (Some(held), Ok(ip)) if held == ip => Action::Keep,
        (_, Ok(ip)) => Action::Reannounce(ip),
        (Some(_), Err(_)) => Action::Withdraw,
        (None, Err(_)) => Action::Keep,
    }
}

/// The router locator worth announcing in `zenoh=`: a `tcp` or `tls`
/// locator whose host is neither loopback nor unspecified (nor
/// `localhost`), cut at its metadata (`?`) and configuration (`#`, which
/// may name key files); `None` for anything else, so a node behind a
/// loopback router (the KD240's) announces no locator. Pure.
pub fn advertised_locator(router: &str) -> Option<String> {
    let locator = router.trim();
    let locator = locator.split(['?', '#']).next().unwrap_or_default();
    if locator.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return None;
    }
    let (scheme, addr) = locator.split_once('/')?;
    if !matches!(scheme, "tcp" | "tls") {
        return None;
    }
    let local = match addr.parse::<std::net::SocketAddr>() {
        Ok(a) => a.ip().is_loopback() || a.ip().is_unspecified(),
        Err(_) => {
            let (host, port) = addr.rsplit_once(':')?;
            port.parse::<u16>().ok()?;
            let host = host.trim_end_matches('.');
            host.is_empty()
                || host.starts_with('[')
                || host.eq_ignore_ascii_case("localhost")
                || host.to_ascii_lowercase().ends_with(".localhost")
        }
    };
    (!local).then(|| locator.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::dns::wire::Txt;
    use middleware::loader::manifest::{BOARD_NAME_MAX, BoardName};

    const NODE: SenderId = SenderId([1, 2, 3, 4, 5, 6, 7, 8]);
    const SENDER: SenderId = SenderId([0xa0, 0xb1, 0xc2, 0xd3, 0xe4, 0xf5, 0x06, 0x17]);

    fn facts(board: Option<&str>) -> Facts {
        Facts { board: board.and_then(BoardName::new), ..crate::facts(NODE) }
    }

    #[test]
    fn d28_txt_is_built_from_facts() {
        let f = facts(Some("ZynqMP KD240 revA/B/1"));
        let id = Identity::new(&f, SENDER, "tcp/192.168.1.10:7447");
        assert_eq!(id.node, NODE);
        assert_eq!(id.sender, SENDER);
        assert_eq!(id.chip, crate::service::CHIP);
        assert_eq!(id.board, "ZynqMP KD240 revA/B/1");
        assert_eq!(id.firmware, env!("CARGO_PKG_VERSION"));
        assert_eq!(id.zenoh.as_deref(), Some("tcp/192.168.1.10:7447"));
        let mut out = [0u8; MAX_TXT_LEN];
        let txt = id.txt(&mut out).unwrap();
        let parsed = NodeTxt::parse(Txt::new(txt).unwrap()).unwrap();
        assert_eq!(
            parsed,
            NodeTxt {
                id: NODE,
                chip: crate::service::CHIP,
                board: "ZynqMP KD240 revA/B/1",
                fw: Some(env!("CARGO_PKG_VERSION")),
                sender: Some(SENDER),
                zenoh: Some("tcp/192.168.1.10:7447"),
            }
        );
        // A loopback router: no locator.
        let id = Identity::new(&f, SENDER, "tcp/127.0.0.1:7447");
        let mut out = [0u8; MAX_TXT_LEN];
        let parsed = NodeTxt::parse(Txt::new(id.txt(&mut out).unwrap()).unwrap()).unwrap();
        assert_eq!(parsed.zenoh, None);
    }

    #[test]
    fn d28_no_device_tree_board_is_unknown() {
        let id = Identity::new(&facts(None), SENDER, "tcp/127.0.0.1:7447");
        assert_eq!(id.board, UNKNOWN_BOARD);
        let mut out = [0u8; MAX_TXT_LEN];
        let parsed = NodeTxt::parse(Txt::new(id.txt(&mut out).unwrap()).unwrap()).unwrap();
        assert_eq!(parsed.board, "unknown");
    }

    #[test]
    fn d28_zenoh_is_dropped_when_it_does_not_fit_192() {
        let board = "b".repeat(BOARD_NAME_MAX);
        let long = format!("tls/{}.example:443", "h".repeat(100));
        let id = Identity::new(&facts(Some(&board)), SENDER, &long);
        assert_eq!(id.zenoh.as_deref(), Some(long.as_str()));
        let mut out = [0u8; MAX_TXT_LEN];
        let parsed = NodeTxt::parse(Txt::new(id.txt(&mut out).unwrap()).unwrap()).unwrap();
        assert_eq!(parsed.zenoh, None);
        assert_eq!((parsed.id, parsed.sender, parsed.board), (NODE, Some(SENDER), board.as_str()));
    }

    #[test]
    fn d28_longest_board_name_fits() {
        let board = "é".repeat(BOARD_NAME_MAX);
        let f = facts(Some(&board));
        // Cut to BOARD_NAME_MAX bytes at a character boundary.
        let worst = "tcp/255.255.255.255:65535";
        let id = Identity::new(&f, SENDER, worst);
        assert_eq!(id.board.len(), BOARD_NAME_MAX);
        let mut out = [0u8; MAX_TXT_LEN];
        let parsed = NodeTxt::parse(Txt::new(id.txt(&mut out).unwrap()).unwrap()).unwrap();
        assert_eq!(parsed.zenoh, Some(worst));
        assert_eq!(parsed.board, id.board);
        // And an IPv6 router's locator, at its longest.
        let worst6 = "tcp/[ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff]:65535";
        let id = Identity::new(&f, SENDER, worst6);
        let mut out = [0u8; MAX_TXT_LEN];
        let parsed = NodeTxt::parse(Txt::new(id.txt(&mut out).unwrap()).unwrap()).unwrap();
        assert_eq!(parsed.zenoh, Some(worst6));
    }

    #[test]
    fn d29_label_and_host_are_per_node() {
        let id = Identity::new(&facts(None), SENDER, "tcp/127.0.0.1:7447");
        assert_eq!(id.label(), "posix-0102030405060708");
        assert_eq!(id.host(), "posix-0102030405060708.local");
        let other = Identity::new(&Facts { node: SenderId([9; 8]), ..facts(None) }, SENDER, "");
        assert_eq!(other.label(), "posix-0909090909090909");
        assert_ne!(other.host(), id.host());
        // Never the system's own name.
        if let Ok(name) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
            assert_ne!(id.host(), format!("{}.local", name.trim()));
        }
    }

    #[test]
    fn d31_next_action_table() {
        let a = Ipv4Addr::new(192, 168, 1, 20);
        let b = Ipv4Addr::new(192, 168, 1, 21);
        let gone = || Err(io::Error::from(io::ErrorKind::NetworkUnreachable));
        assert_eq!(next_action(None, Ok(a)), Action::Reannounce(a));
        assert_eq!(next_action(Some(a), Ok(a)), Action::Keep);
        assert_eq!(next_action(Some(a), Ok(b)), Action::Reannounce(b));
        assert_eq!(next_action(Some(a), gone()), Action::Withdraw);
        assert_eq!(next_action(None, gone()), Action::Keep);
    }

    #[test]
    fn d30_locator_table() {
        for (router, want) in [
            ("tcp/127.0.0.1:7447", None),
            ("tcp/127.9.9.9:7447", None),
            ("tcp/localhost:7447", None),
            ("tcp/LocalHost:7447", None),
            ("tcp/0.0.0.0:7447", None),
            ("tcp/[::1]:7447", None),
            ("tcp/[::]:7447", None),
            ("unixsock-stream//run/zenoh.sock", None),
            ("udp/192.168.1.10:7447", None),
            ("quic/192.168.1.10:7447", None),
            ("tcp/192.168.1.10", None),
            ("tcp/:7447", None),
            ("tcp/192.168.1.10:port", None),
            ("tcp192.168.1.10:7447", None),
            ("", None),
            ("tcp/192.168.1.10:7447", Some("tcp/192.168.1.10:7447")),
            (" tcp/192.168.1.10:7447 ", Some("tcp/192.168.1.10:7447")),
            ("tcp/[fd00::2]:7447", Some("tcp/[fd00::2]:7447")),
            ("tls/mesh.updates.x:443", Some("tls/mesh.updates.x:443")),
            ("tls/mesh.updates.x:443#root_ca_certificate=/p", Some("tls/mesh.updates.x:443")),
            ("tcp/10.0.0.1:7447?iface=eth0#x=y", Some("tcp/10.0.0.1:7447")),
            ("tcp/10.0.0.1:7447\u{1b}[2J", None),
            ("tcp/ho st:7447", None),
        ] {
            assert_eq!(advertised_locator(router).as_deref(), want, "{router:?}");
        }
    }
}
