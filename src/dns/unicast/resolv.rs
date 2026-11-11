//! `resolv.conf(5)` parsing, pure: [`ResolvConf::parse`] reads the text of
//! `/etc/resolv.conf` (the file is read by the shell,
//! [`UnicastDnsSdBuilder::build`](super::UnicastDnsSdBuilder::build)).
//!
//! # What is read
//!
//! | line                         | effect                                         |
//! |------------------------------|------------------------------------------------|
//! | `nameserver <ip>`            | appended, port 53, at most [`MAX_NAMESERVERS`] (glibc's limit) |
//! | `search <d1> <d2> ...`       | replaces the search list, at most [`MAX_SEARCH`] domains |
//! | `domain <d>`                 | replaces the search list with `<d>`            |
//! | `options ndots:n timeout:n attempts:n` | clamped to [`MAX_NDOTS`], 1..=[`MAX_TIMEOUT_S`], 1..=[`MAX_ATTEMPTS`] |
//!
//! As in glibc, the last `search` or `domain` line wins, and anything
//! malformed is ignored rather than refused: a nameserver that is not an IP
//! address (an IPv6 address may carry a numeric `%scope`), a search domain
//! with an empty label, a label over 63 bytes or over [`MAX_DOMAIN_LEN`]
//! bytes, an unknown option or keyword. Text after `#` or `;` is a comment.
//! A trailing dot on a domain is dropped.
//!
//! # Kubernetes
//!
//! A pod's resolver configuration names the cluster DNS service and
//! searches `<namespace>.svc.<cluster-domain> svc.<cluster-domain>
//! <cluster-domain>`. [`ResolvConf::cluster_domain`] and
//! [`ResolvConf::namespace`] recover the two from that list, so a caller can
//! build a [`ServiceMap`](super::ServiceMap) without hard-coding them.
//!
//! The browser queries fully qualified names it builds itself, so it never
//! applies the search list or `ndots`; they are parsed so the configuration
//! can be inspected and the Kubernetes names derived.

use core::net::{IpAddr, Ipv6Addr, SocketAddr, SocketAddrV6};
use core::time::Duration;

use heapless::{String, Vec};

/// Port of a DNS server.
pub const DNS_PORT: u16 = 53;
/// Most nameservers kept (glibc's `MAXNS`).
pub const MAX_NAMESERVERS: usize = 3;
/// Most search domains kept (glibc's `MAXDNSRCH`).
pub const MAX_SEARCH: usize = 6;
/// Longest search domain, as dotted text without the trailing dot.
pub const MAX_DOMAIN_LEN: usize = 253;
/// Largest `ndots` (glibc caps it at 15).
pub const MAX_NDOTS: u8 = 15;
/// `ndots` when not set.
pub const DEFAULT_NDOTS: u8 = 1;
/// Largest per-query timeout in seconds (glibc's `RES_MAXRETRANS`).
pub const MAX_TIMEOUT_S: u8 = 30;
/// Per-query timeout in seconds when not set (glibc's `RES_TIMEOUT`).
pub const DEFAULT_TIMEOUT_S: u8 = 5;
/// Largest number of attempts (glibc's `RES_MAXRETRY`).
pub const MAX_ATTEMPTS: u8 = 5;
/// Attempts per query when not set (glibc's `RES_DFLRETRY`).
pub const DEFAULT_ATTEMPTS: u8 = 2;

/// A parsed resolver configuration; see the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvConf {
    nameservers: Vec<SocketAddr, MAX_NAMESERVERS>,
    search: Vec<String<MAX_DOMAIN_LEN>, MAX_SEARCH>,
    ndots: u8,
    timeout_s: u8,
    attempts: u8,
}

impl Default for ResolvConf {
    fn default() -> Self {
        ResolvConf {
            nameservers: Vec::new(),
            search: Vec::new(),
            ndots: DEFAULT_NDOTS,
            timeout_s: DEFAULT_TIMEOUT_S,
            attempts: DEFAULT_ATTEMPTS,
        }
    }
}

/// Parses a nameserver address, with an optional numeric IPv6 scope.
fn nameserver(word: &str) -> Option<SocketAddr> {
    match word.split_once('%') {
        Some((ip, scope)) => {
            let ip: Ipv6Addr = ip.parse().ok()?;
            let scope: u32 = scope.parse().ok()?;
            Some(SocketAddr::V6(SocketAddrV6::new(ip, DNS_PORT, 0, scope)))
        }
        None => word
            .parse::<IpAddr>()
            .ok()
            .map(|ip| SocketAddr::new(ip, DNS_PORT)),
    }
}

/// A search domain without its trailing dot, if every label is 1..=63
/// bytes and it fits [`MAX_DOMAIN_LEN`].
fn domain(word: &str) -> Option<String<MAX_DOMAIN_LEN>> {
    let word = word.strip_suffix('.').unwrap_or(word);
    let ok = !word.is_empty() && word.split('.').all(|l| (1..=63).contains(&l.len()));
    if ok {
        String::try_from(word).ok()
    } else {
        None
    }
}

/// The number after `name:` in an option word, clamped to `lo..=hi`.
fn option_value(word: &str, name: &str, lo: u8, hi: u8) -> Option<u8> {
    let n: u32 = word.strip_prefix(name)?.strip_prefix(':')?.parse().ok()?;
    Some(u8::try_from(n).unwrap_or(u8::MAX).clamp(lo, hi))
}

impl ResolvConf {
    /// Parses resolver configuration text; see the module docs. Never
    /// fails: what is not understood is ignored, as glibc does.
    pub fn parse(text: &str) -> Self {
        // Bounded by the text length.
        text.lines().fold(Self::default(), Self::line)
    }

    /// The configuration with one more line applied.
    fn line(self, line: &str) -> Self {
        let line = line.split(['#', ';']).next().unwrap_or("");
        let mut words = line.split_ascii_whitespace();
        match words.next() {
            Some("nameserver") => match words.next().and_then(nameserver) {
                Some(a) => self.with_nameserver(a),
                None => self,
            },
            Some("search") => self.with_search(words),
            Some("domain") => self.with_search(words.take(1)),
            Some("options") => words.fold(self, Self::option),
            _ => self,
        }
    }

    fn with_nameserver(self, addr: SocketAddr) -> Self {
        let mut nameservers = self.nameservers;
        // Beyond MAX_NAMESERVERS further lines are ignored, as in glibc.
        let _ = nameservers.push(addr);
        ResolvConf {
            nameservers,
            ..self
        }
    }

    fn with_search<'a>(self, words: impl Iterator<Item = &'a str>) -> Self {
        let mut search = Vec::new();
        for d in words.filter_map(domain) {
            if search.push(d).is_err() {
                break;
            }
        }
        ResolvConf { search, ..self }
    }

    fn option(self, word: &str) -> Self {
        if let Some(ndots) = option_value(word, "ndots", 0, MAX_NDOTS) {
            ResolvConf { ndots, ..self }
        } else if let Some(timeout_s) = option_value(word, "timeout", 1, MAX_TIMEOUT_S) {
            ResolvConf { timeout_s, ..self }
        } else if let Some(attempts) = option_value(word, "attempts", 1, MAX_ATTEMPTS) {
            ResolvConf { attempts, ..self }
        } else {
            self
        }
    }

    /// The nameservers, in file order, each on port 53.
    pub fn nameservers(&self) -> &[SocketAddr] {
        &self.nameservers
    }

    /// The search domains, in order, without trailing dots.
    pub fn search(&self) -> impl Iterator<Item = &str> {
        self.search.iter().map(String::as_str)
    }

    /// `options ndots`: a name with at least this many dots is tried as
    /// is before the search list.
    pub const fn ndots(&self) -> u8 {
        self.ndots
    }

    /// `options timeout`: how long to wait for one reply.
    pub const fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_s as u64)
    }

    /// `options attempts`: how many times a query is sent before giving
    /// up.
    pub const fn attempts(&self) -> u8 {
        self.attempts
    }

    /// The Kubernetes cluster domain: the rest of the first search domain
    /// of the form `svc.<domain>` (`svc.cluster.local` → `cluster.local`).
    pub fn cluster_domain(&self) -> Option<&str> {
        self.search()
            .find_map(|d| d.strip_prefix("svc.").filter(|rest| !rest.is_empty()))
    }

    /// The pod's Kubernetes namespace: the first label of the first search
    /// domain of the form `<namespace>.svc.<domain>`.
    pub fn namespace(&self) -> Option<&str> {
        self.search().find_map(|d| {
            let (ns, rest) = d.split_once(".svc.")?;
            (!ns.is_empty() && !ns.contains('.') && !rest.is_empty()).then_some(ns)
        })
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec as StdVec;

    const POD: &str = "\
search media.svc.cluster.local svc.cluster.local cluster.local
nameserver 10.96.0.10
options ndots:5
";

    #[test]
    fn kubernetes_pod_configuration() {
        let c = ResolvConf::parse(POD);
        assert_eq!(c.nameservers(), ["10.96.0.10:53".parse().unwrap()]);
        assert_eq!(
            c.search().collect::<StdVec<_>>(),
            [
                "media.svc.cluster.local",
                "svc.cluster.local",
                "cluster.local"
            ]
        );
        assert_eq!(c.ndots(), 5);
        assert_eq!(c.cluster_domain(), Some("cluster.local"));
        assert_eq!(c.namespace(), Some("media"));
        assert_eq!(
            (c.timeout(), c.attempts()),
            (Duration::from_secs(5), DEFAULT_ATTEMPTS)
        );
    }

    #[test]
    fn variants_comments_limits_and_malformed_lines() {
        let c = ResolvConf::parse(
            "# generated\n\
             ; also a comment\n\
             nameserver 192.0.2.1 # trailing comment\n\
             nameserver fe80::1%2\n\
             nameserver fe80::1%eth0\n\
             nameserver not-an-ip\n\
             nameserver\n\
             nameserver 2001:db8::53\n\
             nameserver 192.0.2.4\n\
             search first.example\n\
             domain corp.example.\n\
             options ndots:99 timeout:0 attempts:9 rotate edns0\n\
             bogus line here\n",
        );
        assert_eq!(
            c.nameservers(),
            [
                "192.0.2.1:53".parse().unwrap(),
                SocketAddr::V6(SocketAddrV6::new("fe80::1".parse().unwrap(), 53, 0, 2)),
                "[2001:db8::53]:53".parse().unwrap(),
            ],
            "zone names skipped, at most three kept"
        );
        assert_eq!(c.search().collect::<StdVec<_>>(), ["corp.example"]);
        assert_eq!(
            (c.ndots(), c.timeout(), c.attempts()),
            (MAX_NDOTS, Duration::from_secs(1), MAX_ATTEMPTS)
        );
        assert_eq!((c.cluster_domain(), c.namespace()), (None, None));

        let long = "a".repeat(64);
        let c = ResolvConf::parse(&std::format!(
            "search a..b {long}.x ok.example . a b c d e f g\noptions timeout:3"
        ));
        assert_eq!(
            c.search().collect::<StdVec<_>>(),
            ["ok.example", "a", "b", "c", "d", "e"],
            "bad domains skipped, at most six kept"
        );
        assert_eq!(c.timeout(), Duration::from_secs(3));

        let empty = ResolvConf::parse("");
        assert_eq!(empty, ResolvConf::default());
        assert!(empty.nameservers().is_empty());
        assert_eq!(empty.ndots(), DEFAULT_NDOTS);
    }
}
