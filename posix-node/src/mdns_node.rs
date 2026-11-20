//! The node's mDNS announcement (`MW_POSIX_NODE_MDNS`, D27–D34): the
//! shared `mw/node` record (`middleware::mdns::node`) of this node on the
//! LAN, so `avahi-browse -rt _mw-node._tcp` and `mwctl scout` find it.
//!
//! * [`Identity`] — what the record says, built purely from the node's
//!   [`Facts`], its Zenoh session's sender id and its
//!   router: TXT `txtvers=1`, `id`, `chip`, `board` (`unknown` without a
//!   device tree), `fw`, `sender` and, when LAN peers could use it,
//!   `zenoh` ([`advertised_locator`]).
//!
//! * [`run`] — the announcer beside the node: it announces `posix-<node
//!   id>` on host `posix-<node id>.local` at the node's address
//!   ([`Net::local_ip`]), answers queries, re-checks the address every
//!   [`RECHECK`] ([`next_action`]: a change is a goodbye, then the new
//!   address), retries what fails (2 s doubling to 60 s, 30 s after a name
//!   conflict; logged at the 1st, 2nd, 4th … failure in a row) and on
//!   stop multicasts its goodbye within [`GOODBYE_WAIT`]. Nothing it does
//!   fails the node.
//!
//! A Zenoh client listens on nothing, so the SRV port is
//! `mdns::node::NO_PORT` and the A record is the node's own address.
//!
//! The announcer binds UDP 5353 with `SO_REUSEADDR` and `SO_REUSEPORT`
//! beside the system's mDNS daemon (avahi) and any other node on the
//! host: each answers for its own names only. A unicast query to port
//! 5353 reaches one of them, so a peer browsing as a one-shot querier
//! (`mwctl scout`, from an ephemeral port) or by multicast sees them all.

use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::pin::pin;
use std::time::Duration;

use middleware::SenderId;
use middleware::discovery::directory::MAX_TXT_LEN;
use middleware::discovery::{Announce, Endpoint};
use middleware::dns::wire::WireError;
use middleware::mdns::announce::{AnnounceError, MdnsAnnouncer};
use middleware::mdns::node::{self, NodeTxt};
use middleware::mdns::socket::MdnsError;
use middleware::mdns::{MDNS_GROUP_V4, MDNS_PORT};
use tokio::time::Instant;

use crate::config::MdnsMode;
use crate::service::Facts;

/// How often the announced address is re-checked.
pub const RECHECK: Duration = Duration::from_secs(15);
/// The longest wait for the goodbye when the node stops.
pub const GOODBYE_WAIT: Duration = Duration::from_secs(2);

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

/// The announcer's timers; [`Timing::default`] is the node's (module
/// docs). Tests shorten them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// How often the address is re-checked.
    pub recheck: Duration,
    /// The wait after a first failure, doubled after each further one…
    pub retry_min: Duration,
    /// … up to this.
    pub retry_max: Duration,
    /// The wait after every rename of the instance conflicted.
    pub conflict: Duration,
    /// The longest wait for the goodbye on stop.
    pub goodbye: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            recheck: RECHECK,
            retry_min: Duration::from_secs(2),
            retry_max: Duration::from_secs(60),
            conflict: Duration::from_secs(30),
            goodbye: GOODBYE_WAIT,
        }
    }
}

impl Timing {
    /// The wait after the `failures`th failure in a row (from 1); `conflict`
    /// when it was a name conflict. Pure.
    pub fn backoff(&self, failures: u32, conflict: bool) -> Duration {
        if conflict {
            return self.conflict;
        }
        let doublings = failures.saturating_sub(1).min(16);
        self.retry_min.saturating_mul(1 << doublings).min(self.retry_max)
    }
}

/// Whether the `n`th failure in a row is logged: the 1st, 2nd, 4th, 8th …
/// Pure.
pub fn logged(n: u32) -> bool {
    n.is_power_of_two()
}

/// The network the announcer runs on: [`StdNet`] for the node, a loopback
/// one in tests.
pub trait Net {
    /// The address to announce under `mode` (never [`MdnsMode::Off`]).
    fn local_ip(&self, mode: MdnsMode) -> io::Result<Ipv4Addr>;
    /// An announcer for `ip`, its services' SRV target `host`.
    fn bind(&self, ip: Ipv4Addr, host: &str) -> impl Future<Output = Result<MdnsAnnouncer, MdnsError>> + Send;
}

/// The host's network: [`MdnsMode::Auto`] is the address the routing table
/// picks for `224.0.0.251` (a UDP socket connected there, which sends
/// nothing), [`MdnsMode::Pinned`] the configured one; the announcer binds
/// `0.0.0.0:5353`, joins the group on that address and multicasts from
/// its interface.
#[derive(Debug, Clone, Copy, Default)]
pub struct StdNet;

impl Net for StdNet {
    fn local_ip(&self, mode: MdnsMode) -> io::Result<Ipv4Addr> {
        match mode {
            MdnsMode::Pinned(ip) => Ok(ip),
            MdnsMode::Off => Err(io::Error::new(io::ErrorKind::Unsupported, "mdns is off")),
            MdnsMode::Auto => {
                let probe = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
                probe.connect((MDNS_GROUP_V4, MDNS_PORT))?;
                match probe.local_addr()? {
                    SocketAddr::V4(a) if !a.ip().is_unspecified() && !a.ip().is_loopback() => Ok(*a.ip()),
                    other => Err(io::Error::new(
                        io::ErrorKind::AddrNotAvailable,
                        format!("the mDNS group is routed from {other}, not a LAN address"),
                    )),
                }
            }
        }
    }

    async fn bind(&self, ip: Ipv4Addr, host: &str) -> Result<MdnsAnnouncer, MdnsError> {
        MdnsAnnouncer::builder().host(host).join_multicast(ip).multicast_interface(ip).build().await
    }
}

/// What the announcer did, when it stops.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Announcements made (one per address).
    pub announced: u32,
    /// Goodbyes sent.
    pub withdrawn: u32,
    /// Failures (binding, announcing, the socket), each retried.
    pub failures: u32,
    /// The last failure.
    pub last_error: Option<String>,
}

/// [`run_with`] on the node's [`Timing`].
pub async fn run(id: Identity, mode: MdnsMode, net: impl Net, stop: impl Future<Output = ()>) -> Report {
    run_with(id, mode, net, Timing::default(), stop).await
}

/// What woke the announcer.
enum Wake {
    Stop,
    Check,
    Stepped,
    Failed(MdnsError),
}

/// Failures in a row, counted into the report and logged sparsely.
struct Failures<'r> {
    report: &'r mut Report,
    in_a_row: u32,
    timing: Timing,
}

impl Failures<'_> {
    /// Notes a failure; returns when to try again.
    fn note(&mut self, what: &str, e: impl fmt::Display, conflict: bool) -> Instant {
        self.in_a_row = self.in_a_row.saturating_add(1);
        self.report.failures = self.report.failures.saturating_add(1);
        let wait = self.timing.backoff(self.in_a_row, conflict);
        let text = format!("{what}: {e}");
        if logged(self.in_a_row) {
            eprintln!("mw-posix-node: mdns: {text} (failure {} in a row, retrying in {wait:?})", self.in_a_row);
        }
        self.report.last_error = Some(text);
        Instant::now() + wait
    }
}

/// Withdraws the instance under the goodbye wait; true if it went out.
async fn goodbye(a: &mut MdnsAnnouncer, label: &str, wait: Duration) -> bool {
    match tokio::time::timeout(wait, a.withdraw(node::KIND, label)).await {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            eprintln!("mw-posix-node: mdns: goodbye: {e}");
            false
        }
        Err(_) => {
            eprintln!("mw-posix-node: mdns: goodbye: timed out");
            false
        }
    }
}

/// Announces `id` on the address `net` gives under `mode` until `stop`
/// completes, then says goodbye (module docs). Never fails: what goes
/// wrong is retried, logged and counted in the [`Report`].
pub async fn run_with(
    id: Identity,
    mode: MdnsMode,
    net: impl Net,
    timing: Timing,
    stop: impl Future<Output = ()>,
) -> Report {
    let mut report = Report::default();
    let (label, host) = (id.label(), id.host());
    let mut buf = [0u8; MAX_TXT_LEN];
    let txt = match id.txt(&mut buf) {
        Ok(t) => t.to_vec(),
        Err(e) => {
            eprintln!("mw-posix-node: mdns: the record does not encode: {e:?}");
            report.last_error = Some(format!("the record does not encode: {e:?}"));
            return report;
        }
    };
    let mut stop = pin!(stop);
    let mut fails = Failures { report: &mut report, in_a_row: 0, timing };
    let mut held: Option<(Ipv4Addr, MdnsAnnouncer)> = None;
    let mut waiting_for_address = false;
    let mut next_check = Instant::now();
    loop {
        let woke = {
            let step = async {
                match held.as_mut() {
                    Some((_, a)) => a.step().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                () = &mut stop => Wake::Stop,
                () = tokio::time::sleep_until(next_check) => Wake::Check,
                r = step => r.map_or_else(Wake::Failed, |()| Wake::Stepped),
            }
        };
        let ip = match woke {
            Wake::Stop => break,
            Wake::Stepped => continue,
            Wake::Failed(e) => {
                // Its socket is what failed: no goodbye through it.
                held = None;
                next_check = fails.note("announcer", e, false);
                continue;
            }
            Wake::Check => {
                next_check = Instant::now() + timing.recheck;
                let probed = net.local_ip(mode);
                match &probed {
                    Err(e) if !waiting_for_address => {
                        eprintln!("mw-posix-node: mdns: no address to announce on: {e}");
                        waiting_for_address = true;
                    }
                    Ok(_) => waiting_for_address = false,
                    Err(_) => {}
                }
                match next_action(held.as_ref().map(|h| h.0), probed) {
                    Action::Keep => continue,
                    Action::Withdraw => {
                        if let Some((old, mut a)) = held.take() {
                            eprintln!("mw-posix-node: mdns: {old} is gone; {label} withdrawn");
                            if goodbye(&mut a, &label, timing.goodbye).await {
                                fails.report.withdrawn += 1;
                            }
                        }
                        continue;
                    }
                    Action::Reannounce(ip) => ip,
                }
            }
        };
        if let Some((old, mut a)) = held.take() {
            eprintln!("mw-posix-node: mdns: the address moved from {old} to {ip}");
            if goodbye(&mut a, &label, timing.goodbye).await {
                fails.report.withdrawn += 1;
            }
        }
        let bound = tokio::select! {
            () = &mut stop => None,
            r = net.bind(ip, &host) => Some(r),
        };
        let mut a = match bound {
            None => break,
            Some(Ok(a)) => a,
            Some(Err(e)) => {
                next_check = fails.note(&format!("bind on {ip}"), e, false);
                continue;
            }
        };
        let service = node::service(
            &label,
            Endpoint::Socket(SocketAddr::V4(SocketAddrV4::new(ip, node::NO_PORT))),
            &txt,
            id.node,
        );
        let announced = tokio::select! {
            () = &mut stop => None,
            r = a.announce(&service) => Some(r),
        };
        match announced {
            None => {
                // Stopped while probing: a goodbye in case it went out.
                goodbye(&mut a, &label, timing.goodbye).await;
                break;
            }
            Some(Ok(())) => {
                fails.in_a_row = 0;
                fails.report.announced += 1;
                let shown = a.instance_label(node::KIND, &label).unwrap_or(&label);
                if shown == label {
                    eprintln!("mw-posix-node: mdns: announced {label} on {host} at {ip}");
                } else {
                    eprintln!(
                        "mw-posix-node: mdns: announced {label} as {shown:?} (a name conflict) on {host} at {ip}"
                    );
                }
                held = Some((ip, a));
            }
            Some(Err(e)) => {
                let conflict = matches!(e, MdnsError::Announce(AnnounceError::Conflict));
                next_check = fails.note(&format!("announce on {ip}"), e, conflict);
            }
        }
    }
    if let Some((_, mut a)) = held.take()
        && goodbye(&mut a, &label, timing.goodbye).await
    {
        fails.report.withdrawn += 1;
    }
    report
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
    fn d32_retries_back_off_and_are_logged_sparsely() {
        let t = Timing::default();
        let secs = |n| t.backoff(n, false).as_secs();
        assert_eq!(
            [secs(1), secs(2), secs(3), secs(4), secs(5), secs(6), secs(7), secs(100)],
            [2, 4, 8, 16, 32, 60, 60, 60]
        );
        assert_eq!(t.backoff(1, true), Duration::from_secs(30));
        assert_eq!(t.backoff(u32::MAX, false), Duration::from_secs(60));
        let logged_at: Vec<u32> = (1..=20).filter(|&n| logged(n)).collect();
        assert_eq!(logged_at, [1, 2, 4, 8, 16]);
        assert_eq!((t.recheck, t.goodbye), (RECHECK, GOODBYE_WAIT));
        assert_eq!((RECHECK.as_secs(), GOODBYE_WAIT.as_secs()), (15, 2));
    }

    #[test]
    fn d31_a_pinned_address_is_used_as_given() {
        let ip = Ipv4Addr::new(192, 168, 1, 20);
        assert_eq!(StdNet.local_ip(MdnsMode::Pinned(ip)).unwrap(), ip);
        assert!(StdNet.local_ip(MdnsMode::Off).is_err());
        // Auto: whatever the host routes the group through, never loopback.
        if let Ok(ip) = StdNet.local_ip(MdnsMode::Auto) {
            assert!(!ip.is_loopback() && !ip.is_unspecified(), "{ip}");
        }
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
