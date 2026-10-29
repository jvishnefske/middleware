//! Which datagrams an mDNS socket accepts, by where they came from
//! (RFC 6762 §11): a pure policy, applied by [`MdnsSocket`] and
//! [`MdnsAnnouncer`] to every datagram they receive.
//!
//! mDNS has no authentication; what keeps an off-link host from injecting
//! answers is that mDNS traffic is link-local. A spoofed response routed in
//! from elsewhere, or a unicast reply from a host that never saw the query,
//! must not become a discovery event or force an announcer to rename.
//!
//! # Rules
//!
//! [`OriginPolicy::OnLink`] (the default when no destination was configured
//! explicitly) accepts a datagram only if its source address is on-link:
//!
//! * IPv4 loopback (127/8), IPv4 link-local (169.254/16), or inside one of
//!   the [`OnLink`] subnets: the directly connected routes of the host's
//!   interfaces (on Linux read from `/proc/net/route` when the socket is
//!   built, [`OnLink::parse_proc_net_route`]) plus any configured subnet;
//! * IPv6 loopback or link-local unicast (fe80::/10). Other IPv6 sources are
//!   refused: the host's IPv6 prefixes are not read.
//!
//! A *response* must further come from source port 5353 (RFC 6762 §11, also
//! true of unicast replies to one-shot queries, §6.7), and a one-shot
//! querier (bound to a port other than 5353) accepts only responses whose id
//! is that of one of its last [`MAX_OUTSTANDING`] queries.
//!
//! The RFC 6762 §11 check of the received IP TTL (255) is not done: reading
//! it needs `IP_RECVTTL` and ancillary data, which tokio's socket does not
//! expose. The on-link source check covers the same threat for IPv4 and
//! link-local IPv6.
//!
//! [`OriginPolicy::Any`] (the default once a destination is set explicitly,
//! as tests on loopback do) accepts every source.
//!
//! [`MdnsSocket`]: super::socket::MdnsSocket
//! [`MdnsAnnouncer`]: super::announce::MdnsAnnouncer

use core::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use heapless::Vec;

use super::MDNS_PORT;

/// Most subnets an [`OnLink`] holds; further routes are ignored.
pub const MAX_SUBNETS: usize = 16;
/// Query ids a one-shot querier remembers as outstanding.
pub const MAX_OUTSTANDING: usize = 8;

/// Whether received datagrams are checked against their source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginPolicy {
    /// Accept every source (explicit destinations, tests on loopback).
    Any,
    /// Accept on-link sources only; see the module docs.
    OnLink,
}

/// Why a datagram was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    /// The source address is not on-link.
    OffLink,
    /// A response from a port other than 5353.
    SourcePort,
    /// A unicast reply whose id matches no outstanding query.
    UnknownId,
}

/// One IPv4 subnet: network address and prefix length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Subnet {
    net: u32,
    mask: u32,
}

impl Subnet {
    /// The subnet `net/prefix`; `None` for a prefix over 32.
    pub fn new(net: Ipv4Addr, prefix: u8) -> Option<Self> {
        let mask = match prefix {
            0 => 0,
            1..=32 => u32::MAX << (32 - u32::from(prefix)),
            _ => return None,
        };
        Some(Subnet {
            net: u32::from(net) & mask,
            mask,
        })
    }

    /// True if `ip` is inside.
    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        u32::from(ip) & self.mask == self.net
    }
}

/// The IPv4 subnets considered on-link besides loopback and link-local.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OnLink {
    subnets: Vec<Subnet, MAX_SUBNETS>,
}

/// `RTF_UP` in `/proc/net/route` flags.
const RTF_UP: u32 = 0x0001;
/// `RTF_GATEWAY`: the route goes through a router, not on-link.
const RTF_GATEWAY: u32 = 0x0002;
/// `RTF_REJECT`: an unreachable route.
const RTF_REJECT: u32 = 0x0200;
/// Lines of `/proc/net/route` read.
const MAX_ROUTE_LINES: usize = 256;

impl OnLink {
    /// Loopback and link-local only.
    pub const fn new() -> Self {
        OnLink {
            subnets: Vec::new(),
        }
    }

    /// Adds `subnet` (ignored once [`MAX_SUBNETS`] are held).
    pub fn with(mut self, subnet: Subnet) -> Self {
        if !self.subnets.contains(&subnet) {
            // A full table keeps the subnets it has: documented as ignored.
            let _ = self.subnets.push(subnet);
        }
        self
    }

    /// Adds the directly connected routes of a Linux `/proc/net/route`
    /// table: up, no gateway, not a reject route, a real interface (not
    /// `*`), a non-zero mask. Malformed lines are skipped; at most 256
    /// lines are read.
    pub fn parse_proc_net_route(self, text: &str) -> Self {
        let route = |line: &str| -> Option<Subnet> {
            let mut f = line.split_whitespace();
            let iface = f.next()?;
            let dest = u32::from_str_radix(f.next()?, 16).ok()?;
            let gateway = u32::from_str_radix(f.next()?, 16).ok()?;
            let flags = u32::from_str_radix(f.next()?, 16).ok()?;
            let mask = u32::from_str_radix(f.nth(3)?, 16).ok()?;
            let on_link = iface != "*"
                && flags & RTF_UP != 0
                && flags & (RTF_GATEWAY | RTF_REJECT) == 0
                && gateway == 0
                && mask != 0;
            // The kernel prints the network-order address as a native u32.
            let mask = u32::from_be_bytes(mask.to_ne_bytes());
            on_link.then(|| Subnet {
                net: u32::from_be_bytes(dest.to_ne_bytes()) & mask,
                mask,
            })
        };
        text.lines()
            .skip(1)
            .take(MAX_ROUTE_LINES)
            .filter_map(route)
            .fold(self, OnLink::with)
    }

    /// The subnets held.
    pub fn subnets(&self) -> &[Subnet] {
        &self.subnets
    }

    /// True if `ip` is on-link: see the module docs.
    pub fn contains(&self, ip: IpAddr) -> bool {
        match ip.to_canonical() {
            IpAddr::V4(v4) => {
                v4.is_loopback()
                    || v4.is_link_local()
                    || self.subnets.iter().any(|s| s.contains(v4))
            }
            IpAddr::V6(v6) => v6.is_loopback() || is_unicast_link_local(v6),
        }
    }
}

fn is_unicast_link_local(ip: Ipv6Addr) -> bool {
    ip.segments()[0] & 0xFFC0 == 0xFE80
}

/// What a received datagram is, for the origin check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind<'a> {
    /// A query.
    Query,
    /// A response with this id; `outstanding` holds the ids of the
    /// receiver's outstanding one-shot queries, `None` if it is not a
    /// one-shot querier (bound to port 5353).
    Response {
        /// The message id.
        id: u16,
        /// Outstanding query ids of a one-shot querier.
        outstanding: Option<&'a [u16]>,
    },
}

/// Applies `policy` to a datagram from `from`; see the module docs.
pub fn check(
    policy: OriginPolicy,
    on_link: &OnLink,
    from: SocketAddr,
    kind: Kind<'_>,
) -> Result<(), Reject> {
    if policy == OriginPolicy::Any {
        return Ok(());
    }
    if !on_link.contains(from.ip()) {
        return Err(Reject::OffLink);
    }
    match kind {
        Kind::Query => Ok(()),
        Kind::Response { .. } if from.port() != MDNS_PORT => Err(Reject::SourcePort),
        Kind::Response {
            id,
            outstanding: Some(ids),
        } if !ids.contains(&id) => Err(Reject::UnknownId),
        Kind::Response { .. } => Ok(()),
    }
}

/// The last [`MAX_OUTSTANDING`] query ids sent, as a pure value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Outstanding {
    ids: [u16; MAX_OUTSTANDING],
    len: usize,
    next: usize,
}

impl Outstanding {
    /// Records `id` as sent, forgetting the oldest beyond
    /// [`MAX_OUTSTANDING`].
    pub fn sent(self, id: u16) -> Self {
        let mut ids = self.ids;
        let at = self.next % MAX_OUTSTANDING;
        ids[at] = id;
        Outstanding {
            ids,
            len: (self.len + 1).min(MAX_OUTSTANDING),
            next: (at + 1) % MAX_OUTSTANDING,
        }
    }

    /// The remembered ids.
    pub fn ids(&self) -> &[u16] {
        self.ids.get(..self.len).unwrap_or_default()
    }
}

/// The on-link subnets of this host: on Linux the connected routes of
/// `/proc/net/route`, elsewhere none (loopback and link-local only).
pub fn system_on_link() -> OnLink {
    std::fs::read_to_string("/proc/net/route")
        .map(|t| OnLink::new().parse_proc_net_route(&t))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::format;
    use std::string::String;

    fn hex(ip: [u8; 4]) -> String {
        format!("{:08X}", u32::from_ne_bytes(ip))
    }

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn proc_net_route_yields_connected_subnets_only() {
        let header = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT";
        let line = |iface: &str, dest: [u8; 4], gw: [u8; 4], flags: &str, mask: [u8; 4]| {
            format!(
                "{iface}\t{}\t{}\t{flags}\t0\t0\t0\t{}\t0\t0\t0",
                hex(dest),
                hex(gw),
                hex(mask)
            )
        };
        let text = [
            header.into(),
            // Default route via a gateway: not on-link.
            line("eth0", [0, 0, 0, 0], [192, 168, 1, 254], "0003", [0, 0, 0, 0]),
            // The LAN.
            line("eth0", [192, 168, 1, 0], [0; 4], "0001", [255, 255, 255, 0]),
            // A reject route and a device-less route.
            line("eth0", [10, 9, 0, 0], [0; 4], "0201", [255, 255, 0, 0]),
            line("*", [10, 1, 179, 192], [0; 4], "0001", [255, 255, 255, 192]),
            "garbage".into(),
        ]
        .join("\n");
        let on_link = OnLink::new().parse_proc_net_route(&text);
        assert_eq!(
            on_link.subnets(),
            &[Subnet::new(Ipv4Addr::new(192, 168, 1, 0), 24).unwrap()]
        );
        assert!(on_link.contains("192.168.1.77".parse().unwrap()));
        assert!(!on_link.contains("192.168.2.1".parse().unwrap()));
        assert!(!on_link.contains("10.9.0.1".parse().unwrap()));
        assert!(!on_link.contains("10.1.179.193".parse().unwrap()));
    }

    #[test]
    fn on_link_sources_are_loopback_link_local_or_connected() {
        let on_link = OnLink::new().with(Subnet::new(Ipv4Addr::new(10, 0, 0, 0), 8).unwrap());
        for ok in ["127.0.0.1", "169.254.3.4", "10.20.30.40", "::1", "fe80::1", "::ffff:10.1.1.1"] {
            assert!(on_link.contains(ok.parse().unwrap()), "{ok}");
        }
        for off in ["8.8.8.8", "192.168.0.1", "2001:db8::1", "fec0::1"] {
            assert!(!on_link.contains(off.parse().unwrap()), "{off}");
        }
        assert_eq!(Subnet::new(Ipv4Addr::UNSPECIFIED, 33), None);
    }

    #[test]
    fn responses_need_port_5353_and_an_outstanding_id_when_on_link() {
        let on_link = OnLink::new();
        let pending = Outstanding::default().sent(7).sent(8);
        let one_shot = |id| Kind::Response {
            id,
            outstanding: Some(pending.ids()),
        };
        let p = OriginPolicy::OnLink;
        assert_eq!(check(p, &on_link, sa("8.8.8.8:5353"), Kind::Query), Err(Reject::OffLink));
        assert_eq!(check(p, &on_link, sa("169.254.1.1:40000"), Kind::Query), Ok(()));
        assert_eq!(
            check(p, &on_link, sa("169.254.1.1:40000"), one_shot(7)),
            Err(Reject::SourcePort)
        );
        assert_eq!(check(p, &on_link, sa("169.254.1.1:5353"), one_shot(8)), Ok(()));
        assert_eq!(
            check(p, &on_link, sa("169.254.1.1:5353"), one_shot(9)),
            Err(Reject::UnknownId)
        );
        // A multicast listener (port 5353) takes any id.
        let listener = Kind::Response {
            id: 0,
            outstanding: None,
        };
        assert_eq!(check(p, &on_link, sa("169.254.1.1:5353"), listener), Ok(()));
        // Any accepts everything.
        assert_eq!(
            check(OriginPolicy::Any, &on_link, sa("8.8.8.8:1"), one_shot(1)),
            Ok(())
        );
    }

    #[test]
    fn outstanding_keeps_the_last_ids() {
        let o = (1..=10u16).fold(Outstanding::default(), Outstanding::sent);
        let mut ids: std::vec::Vec<u16> = o.ids().into();
        ids.sort_unstable();
        assert_eq!(ids, (3..=10).collect::<std::vec::Vec<_>>());
    }
}
