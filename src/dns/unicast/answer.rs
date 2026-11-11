//! Replies from a unicast resolver, pure: which datagram answers the
//! outstanding query ([`check_reply`]), and what an SRV ([`srv_answer`]) or
//! A/AAAA ([`address_answer`]) reply says.
//!
//! # Matching a reply to its query
//!
//! A datagram is accepted only if, in this order, it comes from the
//! nameserver address the query went to, parses as a DNS message, is a
//! response, carries the query's id, and has exactly the query's one
//! question (same name ignoring ASCII case, same type, class IN). Anything
//! else is [`Verdict::Ignore`]d with the reason, so a stray, late or
//! spoofed datagram never decides an answer (RFC 5452 §4.3, §9.1). A matching
//! reply with the TC bit is [`Verdict::Truncated`]: the full answer needs
//! DNS over TCP, which this resolver does not speak.
//!
//! # SRV answers
//!
//! The response code decides first: NXDOMAIN is [`Lookup::NoSuchName`] (a
//! Service that does not exist, or a headless Service without ready pods),
//! any code other than NOERROR is [`AnswerError::Rcode`]. Then every SRV
//! record owned by the query name, in any section, is one [`Found`]
//! endpoint, in record order, regardless of priority and weight (every pod of
//! a headless Service is reported):
//!
//! * the instance is the target's first label (`web-0` of
//!   `web-0.web.media.svc.cluster.local`, or the dashed IP Kubernetes uses
//!   for a pod without a hostname); a target of `.` (RFC 2782: no
//!   service) is skipped, and so is a later SRV with an instance already
//!   seen;
//! * address glue: the first A and the first AAAA record owned by the
//!   target anywhere in the message (CoreDNS adds them to the additional
//!   section for headless Services);
//! * attributes: a TXT record owned by the target, if the server adds one
//!   (an extension: Kubernetes never does, so attributes are normally
//!   empty);
//! * the TTL is the smallest of the SRV's and the glue records' used.
//!
//! More SRV records than `N` are counted in [`SrvSet::overflow`], not
//! kept. No SRV record for the name in a NOERROR reply is
//! [`Lookup::NoData`]. CNAMEs are not followed.
//!
//! # Address answers
//!
//! [`address_answer`] takes the first A and first AAAA owned by the target
//! and the smallest of their TTLs, with the same response-code rules.

use core::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use heapless::{String, Vec};

use crate::discovery::directory::{MAX_INSTANCE_LEN, MAX_TXT_LEN};
use crate::discovery::{Endpoint, Service, ServiceKind};
use crate::dns::wire::{
    CLASS_IN, FLAG_TC, MAX_MESSAGE_LEN, MAX_NAME_LEN, Message, Name, Rdata, WireError,
};

/// Longest SRV target kept, as dotted text (labels escaped as
/// [`Name::write_text`] does).
pub const MAX_TARGET_LEN: usize = 253;

/// NOERROR.
const RCODE_OK: u8 = 0;
/// NXDOMAIN.
const RCODE_NXDOMAIN: u8 = 3;

/// The query a reply must answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Query<'a> {
    /// Where it was sent.
    pub server: SocketAddr,
    /// Its id.
    pub id: u16,
    /// The name asked about, dotted, without escapes.
    pub name: &'a str,
    /// The record type asked for.
    pub qtype: u16,
}

/// Why a datagram was not taken as the reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ignored {
    /// It came from another address than the query's server.
    WrongSource,
    /// It does not parse, or is over [`MAX_MESSAGE_LEN`] bytes.
    Malformed,
    /// It is a query.
    NotAResponse,
    /// Its id is not the query's.
    WrongId,
    /// Its question is not the query's.
    WrongQuestion,
}

/// What a received datagram is to the outstanding query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict<'p> {
    /// The reply.
    Accept(Message<'p>),
    /// The reply, truncated (TC): its answer is incomplete.
    Truncated,
    /// Not the reply.
    Ignore(Ignored),
}

/// True if `name` is the dotted text `text` (no escapes), ignoring ASCII
/// case and a trailing dot.
pub fn name_is(name: &Name<'_>, text: &str) -> bool {
    let text = text.strip_suffix('.').unwrap_or(text);
    let mut buf = [0u8; 2 * MAX_NAME_LEN];
    name.write_text(&mut buf)
        .is_ok_and(|t| t.eq_ignore_ascii_case(text))
}

/// Classifies a datagram of `packet` received from `from` against
/// `query`; see the module docs.
pub fn check_reply<'p>(query: &Query<'_>, from: SocketAddr, packet: &'p [u8]) -> Verdict<'p> {
    if from != query.server {
        return Verdict::Ignore(Ignored::WrongSource);
    }
    if packet.len() > MAX_MESSAGE_LEN {
        return Verdict::Ignore(Ignored::Malformed);
    }
    let Ok(message) = Message::parse(packet) else {
        return Verdict::Ignore(Ignored::Malformed);
    };
    if !message.is_response() {
        return Verdict::Ignore(Ignored::NotAResponse);
    }
    if message.id() != query.id {
        return Verdict::Ignore(Ignored::WrongId);
    }
    let mut questions = message.questions();
    let same = questions.next().is_some_and(|q| {
        q.qtype == query.qtype && q.class == CLASS_IN && name_is(&q.name, query.name)
    });
    if !same || questions.next().is_some() {
        return Verdict::Ignore(Ignored::WrongQuestion);
    }
    if message.flags() & FLAG_TC != 0 {
        return Verdict::Truncated;
    }
    Verdict::Accept(message)
}

/// Why an answer could not be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AnswerError {
    /// The response code is neither NOERROR nor NXDOMAIN (2 SERVFAIL,
    /// 5 REFUSED, ...).
    Rcode(u8),
    /// A name could not be rendered.
    Wire(WireError),
}

impl From<WireError> for AnswerError {
    fn from(e: WireError) -> Self {
        AnswerError::Wire(e)
    }
}

/// The outcome of a lookup that did not fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup<T> {
    /// Records were found.
    Found(T),
    /// NXDOMAIN: the name does not exist.
    NoSuchName,
    /// NOERROR without records of the type asked for.
    NoData,
}

/// Addresses of one target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Addresses {
    /// The first IPv4 address.
    pub ipv4: Option<Ipv4Addr>,
    /// The first IPv6 address.
    pub ipv6: Option<Ipv6Addr>,
    /// The smallest TTL of the records used.
    pub ttl_s: u32,
}

impl Addresses {
    /// True if no address is known.
    pub const fn is_empty(&self) -> bool {
        self.ipv4.is_none() && self.ipv6.is_none()
    }

    /// The preferred address: IPv4, else IPv6.
    pub fn ip(&self) -> Option<IpAddr> {
        self.ipv4.map(IpAddr::V4).or(self.ipv6.map(IpAddr::V6))
    }
}

/// One endpoint of an SRV answer; see the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    instance: String<MAX_INSTANCE_LEN>,
    target: String<MAX_TARGET_LEN>,
    port: u16,
    ttl_s: u32,
    addresses: Addresses,
    txt: Vec<u8, MAX_TXT_LEN>,
}

impl Found {
    /// The instance name: the target's first label.
    pub fn instance(&self) -> &str {
        &self.instance
    }

    /// The SRV target, dotted.
    pub fn target(&self) -> &str {
        &self.target
    }

    /// The port.
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// The smallest TTL of the records it was built from.
    pub const fn ttl_s(&self) -> u32 {
        self.ttl_s
    }

    /// The addresses known for the target.
    pub const fn addresses(&self) -> Addresses {
        self.addresses
    }

    /// True if an address is known.
    pub const fn is_addressed(&self) -> bool {
        !self.addresses.is_empty()
    }

    /// The endpoint with `addresses` (from [`address_answer`]) filled in
    /// where none were known, its TTL lowered to theirs.
    pub fn with_addresses(self, addresses: Addresses) -> Self {
        if addresses.is_empty() {
            return self;
        }
        Found {
            ttl_s: self.ttl_s.min(addresses.ttl_s),
            addresses,
            ..self
        }
    }

    /// The service it is, as `kind`: [`Endpoint::Socket`] at the preferred
    /// address when one is known, else [`Endpoint::Host`] with the target;
    /// no node id; TTL as found.
    pub fn service<'a>(&'a self, kind: ServiceKind<'a>) -> Service<'a> {
        let endpoint = match self.addresses.ip() {
            Some(ip) => Endpoint::Socket(SocketAddr::new(ip, self.port)),
            None => Endpoint::Host {
                name: &self.target,
                port: self.port,
            },
        };
        Service {
            kind,
            instance: &self.instance,
            node: None,
            endpoint,
            txt: &self.txt,
            ttl_s: self.ttl_s,
        }
    }
}

/// The endpoints of an SRV answer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SrvSet<const N: usize> {
    /// The endpoints, in record order.
    pub found: Vec<Found, N>,
    /// SRV records beyond the `N` kept.
    pub overflow: usize,
}

/// NXDOMAIN, NOERROR, or the error for another code.
fn rcode(message: &Message<'_>) -> Result<Option<Lookup<()>>, AnswerError> {
    match message.rcode() {
        RCODE_OK => Ok(None),
        RCODE_NXDOMAIN => Ok(Some(Lookup::NoSuchName)),
        code => Err(AnswerError::Rcode(code)),
    }
}

fn min_ttl(a: u32, b: Option<u32>) -> u32 {
    b.map_or(a, |b| a.min(b))
}

/// Address records owned by `owner` (compared with `is_owner`) in
/// `message`, or `None` when there are none.
fn addresses_of<'p>(
    message: &Message<'p>,
    is_owner: impl Fn(&Name<'p>) -> bool,
) -> Option<Addresses> {
    let mut found = Addresses::default();
    let mut ttl: Option<u32> = None;
    // Bounded: at most MAX_RECORDS records.
    for r in message.records() {
        match r.data {
            Rdata::A(a) if found.ipv4.is_none() && is_owner(&r.name) => {
                found.ipv4 = Some(a);
                ttl = Some(min_ttl(r.ttl, ttl));
            }
            Rdata::Aaaa(a) if found.ipv6.is_none() && is_owner(&r.name) => {
                found.ipv6 = Some(a);
                ttl = Some(min_ttl(r.ttl, ttl));
            }
            _ => {}
        }
    }
    let ttl_s = ttl?;
    Some(Addresses { ttl_s, ..found })
}

/// The endpoints the SRV reply `message` gives for `qname`; see the module
/// docs.
pub fn srv_answer<const N: usize>(
    message: &Message<'_>,
    qname: &str,
) -> Result<Lookup<SrvSet<N>>, AnswerError> {
    if let Some(Lookup::NoSuchName) = rcode(message)? {
        return Ok(Lookup::NoSuchName);
    }
    let mut set = SrvSet::<N>::default();
    let mut any = false;
    // Bounded: at most MAX_RECORDS records, each joined against as many.
    for r in message.records() {
        let Rdata::Srv(srv) = r.data else { continue };
        if !name_is(&r.name, qname) {
            continue;
        }
        any = true;
        let Some(label) = srv.target.first_label() else {
            continue;
        };
        let instance = core::str::from_utf8(label).map_err(|_| WireError::NotUtf8)?;
        if set.found.iter().any(|f| f.instance.as_str() == instance) {
            continue;
        }
        if set.found.is_full() {
            set.overflow += 1;
            continue;
        }
        let mut text = [0u8; 2 * MAX_NAME_LEN];
        let target = srv.target.write_text(&mut text)?;
        let (Ok(instance), Ok(target)) = (String::try_from(instance), String::try_from(target))
        else {
            // A label over 63 bytes cannot occur in a parsed name; a target
            // whose escaped text is over 253 bytes is skipped.
            continue;
        };
        let addresses = addresses_of(message, |n| *n == srv.target).unwrap_or_default();
        let txt = message.records().find_map(|t| match t.data {
            Rdata::Txt(txt) if t.name == srv.target => Some((txt, t.ttl)),
            _ => None,
        });
        let mut ttl_s = r.ttl;
        if !addresses.is_empty() {
            ttl_s = ttl_s.min(addresses.ttl_s);
        }
        let txt = match txt {
            Some((txt, ttl)) => {
                ttl_s = ttl_s.min(ttl);
                Vec::from_slice(txt.as_bytes()).unwrap_or_default()
            }
            None => Vec::new(),
        };
        // Cannot fail: checked `is_full` above.
        let _ = set.found.push(Found {
            instance,
            target,
            port: srv.port,
            ttl_s,
            addresses,
            txt,
        });
    }
    Ok(if any {
        Lookup::Found(set)
    } else {
        Lookup::NoData
    })
}

/// The addresses an A or AAAA reply `message` gives for `target`; see the
/// module docs.
pub fn address_answer(
    message: &Message<'_>,
    target: &str,
) -> Result<Lookup<Addresses>, AnswerError> {
    if let Some(Lookup::NoSuchName) = rcode(message)? {
        return Ok(Lookup::NoSuchName);
    }
    Ok(match addresses_of(message, |n| name_is(n, target)) {
        Some(a) => Lookup::Found(a),
        None => Lookup::NoData,
    })
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::dns::wire::{
        FLAG_QR, FLAG_RA, FLAG_RD, MessageWriter, RecordData, RecordOut, Section, TYPE_A, TYPE_SRV,
        TextName, Txt,
    };

    const SERVER: &str = "10.96.0.10:53";
    const QNAME: &str = "_http._tcp.web.media.svc.cluster.local";
    const CAM: ServiceKind<'static> = ServiceKind::from_static("cam/jpeg");

    fn server() -> SocketAddr {
        SERVER.parse().unwrap()
    }

    fn rec(name: &'static str, ttl: u32, data: RecordData<'static>) -> RecordOut<'static> {
        RecordOut {
            name: TextName::new(name).unwrap(),
            ttl,
            cache_flush: false,
            data,
        }
    }

    fn srv(target: &'static str, port: u16, ttl: u32) -> RecordOut<'static> {
        rec(
            QNAME,
            ttl,
            RecordData::Srv {
                priority: 0,
                weight: 50,
                port,
                target: TextName::new(target).unwrap(),
            },
        )
    }

    /// A reply to (id, qname, qtype) with `flags` and the records, each with
    /// its section.
    fn reply(
        out: &mut [u8],
        id: u16,
        qname: &str,
        qtype: u16,
        flags: u16,
        records: &[(Section, RecordOut<'_>)],
    ) -> usize {
        let mut w = MessageWriter::new(out, id, FLAG_QR | FLAG_RD | FLAG_RA | flags)
            .unwrap()
            .question(TextName::new(qname).unwrap(), qtype, false)
            .unwrap();
        for (section, r) in records {
            w = w.record(*section, r).unwrap();
        }
        w.finish()
    }

    fn query(id: u16) -> Query<'static> {
        Query {
            server: server(),
            id,
            name: QNAME,
            qtype: TYPE_SRV,
        }
    }

    #[test]
    fn only_the_matching_reply_is_accepted() {
        let mut out = [0u8; 512];
        let q = query(0x4242);
        let n = reply(&mut out, 0x4242, QNAME, TYPE_SRV, 0, &[]);
        assert!(matches!(
            check_reply(&q, server(), &out[..n]),
            Verdict::Accept(_)
        ));
        // Case and a trailing dot do not matter.
        let n = reply(
            &mut out,
            0x4242,
            &QNAME.to_ascii_uppercase(),
            TYPE_SRV,
            0,
            &[],
        );
        let dotted = Query {
            name: "_http._tcp.web.media.svc.cluster.local.",
            ..q
        };
        assert!(matches!(
            check_reply(&dotted, server(), &out[..n]),
            Verdict::Accept(_)
        ));

        let n = reply(&mut out, 0x4242, QNAME, TYPE_SRV, 0, &[]);
        let spoofed: SocketAddr = "10.96.0.10:5353".parse().unwrap();
        assert_eq!(
            check_reply(&q, spoofed, &out[..n]),
            Verdict::Ignore(Ignored::WrongSource)
        );
        assert_eq!(
            check_reply(&query(0x4243), server(), &out[..n]),
            Verdict::Ignore(Ignored::WrongId)
        );
        assert_eq!(
            check_reply(&q, server(), &out[..n - 1]),
            Verdict::Ignore(Ignored::Malformed)
        );
        let n = reply(&mut out, 0x4242, QNAME, TYPE_A, 0, &[]);
        assert_eq!(
            check_reply(&q, server(), &out[..n]),
            Verdict::Ignore(Ignored::WrongQuestion)
        );
        let n = reply(
            &mut out,
            0x4242,
            "_http._tcp.other.media.svc.cluster.local",
            TYPE_SRV,
            0,
            &[],
        );
        assert_eq!(
            check_reply(&q, server(), &out[..n]),
            Verdict::Ignore(Ignored::WrongQuestion)
        );
        let n = crate::dns::wire::encode_query_for(
            0x4242,
            TextName::new(QNAME).unwrap(),
            TYPE_SRV,
            true,
            &mut out,
        )
        .unwrap();
        assert_eq!(
            check_reply(&q, server(), &out[..n]),
            Verdict::Ignore(Ignored::NotAResponse)
        );
        let n = reply(&mut out, 0x4242, QNAME, TYPE_SRV, FLAG_TC, &[]);
        assert_eq!(check_reply(&q, server(), &out[..n]), Verdict::Truncated);
        // A spoofed truncation is ignored like any spoofed reply.
        assert_eq!(
            check_reply(&query(1), server(), &out[..n]),
            Verdict::Ignore(Ignored::WrongId)
        );
    }

    #[test]
    fn headless_srv_answer_with_glue() {
        let mut out = [0u8; 1024];
        let a = "web-0.web.media.svc.cluster.local";
        let b = "web-1.web.media.svc.cluster.local";
        let c = "10-244-1-7.web.media.svc.cluster.local";
        let n = reply(
            &mut out,
            1,
            QNAME,
            TYPE_SRV,
            0,
            &[
                (Section::Answer, srv(a, 8080, 30)),
                (Section::Answer, srv(b, 8080, 30)),
                (Section::Answer, srv(c, 8081, 30)),
                // A second port of web-0 is the same instance: skipped.
                (Section::Answer, srv(a, 9090, 30)),
                (
                    Section::Additional,
                    rec(a, 10, RecordData::A(Ipv4Addr::new(10, 244, 1, 5))),
                ),
                (
                    Section::Additional,
                    rec(b, 30, RecordData::Aaaa("fd00::6".parse().unwrap())),
                ),
                (
                    Section::Additional,
                    rec(a, 30, RecordData::Txt(Txt::new(b"\x05ver=5").unwrap())),
                ),
            ],
        );
        let m = Message::parse(&out[..n]).unwrap();
        let Lookup::Found(set) = srv_answer::<4>(&m, QNAME).unwrap() else {
            panic!("expected endpoints")
        };
        assert_eq!(set.overflow, 0);
        let s: std::vec::Vec<_> = set.found.iter().map(|f| f.service(CAM)).collect();
        assert_eq!(s.len(), 3);
        assert_eq!(
            (
                s[0].instance,
                s[0].endpoint,
                s[0].txt,
                s[0].ttl_s,
                s[0].node
            ),
            (
                "web-0",
                Endpoint::Socket("10.244.1.5:8080".parse().unwrap()),
                &b"\x05ver=5"[..],
                10,
                None
            )
        );
        assert_eq!(
            (s[1].instance, s[1].endpoint, s[1].txt),
            (
                "web-1",
                Endpoint::Socket("[fd00::6]:8080".parse().unwrap()),
                &b""[..]
            )
        );
        assert_eq!(
            (s[2].instance, s[2].endpoint, s[2].ttl_s),
            (
                "10-244-1-7",
                Endpoint::Host {
                    name: c,
                    port: 8081
                },
                30
            )
        );
        assert!(!set.found[2].is_addressed());
        // Capacity: the rest is counted.
        let Lookup::Found(set) = srv_answer::<2>(&m, QNAME).unwrap() else {
            panic!()
        };
        assert_eq!((set.found.len(), set.overflow), (2, 1));
        // Address glue fills in later, lowering the TTL.
        let filled = set.found[1].clone().with_addresses(Addresses {
            ipv4: Some(Ipv4Addr::new(10, 0, 0, 1)),
            ipv6: None,
            ttl_s: 5,
        });
        assert_eq!(
            (filled.service(CAM).endpoint, filled.ttl_s()),
            (Endpoint::Socket("10.0.0.1:8080".parse().unwrap()), 5)
        );
        assert_eq!(filled.clone().with_addresses(Addresses::default()), filled);
    }

    #[test]
    fn response_codes_and_empty_answers() {
        let mut out = [0u8; 512];
        let n = reply(&mut out, 1, QNAME, TYPE_SRV, 3, &[]);
        let m = Message::parse(&out[..n]).unwrap();
        assert_eq!(srv_answer::<4>(&m, QNAME), Ok(Lookup::NoSuchName));
        assert_eq!(address_answer(&m, QNAME), Ok(Lookup::NoSuchName));
        let n = reply(&mut out, 1, QNAME, TYPE_SRV, 2, &[]);
        let m = Message::parse(&out[..n]).unwrap();
        assert_eq!(srv_answer::<4>(&m, QNAME), Err(AnswerError::Rcode(2)));
        // NOERROR, but the SRV belongs to another name: no data.
        let other = rec(
            "_x._tcp.other.media.svc.cluster.local",
            30,
            RecordData::Srv {
                priority: 0,
                weight: 0,
                port: 1,
                target: TextName::new("a.b").unwrap(),
            },
        );
        let n = reply(&mut out, 1, QNAME, TYPE_SRV, 0, &[(Section::Answer, other)]);
        let m = Message::parse(&out[..n]).unwrap();
        assert_eq!(srv_answer::<4>(&m, QNAME), Ok(Lookup::NoData));
        // A "." target means no service: an empty set, not NoData.
        let n = reply(
            &mut out,
            1,
            QNAME,
            TYPE_SRV,
            0,
            &[(Section::Answer, srv(".", 0, 30))],
        );
        let m = Message::parse(&out[..n]).unwrap();
        assert_eq!(
            srv_answer::<4>(&m, QNAME),
            Ok(Lookup::Found(SrvSet::default()))
        );
    }

    #[test]
    fn address_answers_take_the_first_of_each_family() {
        let mut out = [0u8; 512];
        let t = "web-0.web.media.svc.cluster.local";
        let n = reply(
            &mut out,
            1,
            t,
            TYPE_A,
            0,
            &[
                (
                    Section::Answer,
                    rec(t, 20, RecordData::A(Ipv4Addr::new(10, 0, 0, 1))),
                ),
                (
                    Section::Answer,
                    rec(t, 5, RecordData::A(Ipv4Addr::new(10, 0, 0, 2))),
                ),
                (
                    Section::Answer,
                    rec("x.y", 1, RecordData::A(Ipv4Addr::new(9, 9, 9, 9))),
                ),
            ],
        );
        let m = Message::parse(&out[..n]).unwrap();
        assert_eq!(
            address_answer(&m, t),
            Ok(Lookup::Found(Addresses {
                ipv4: Some(Ipv4Addr::new(10, 0, 0, 1)),
                ipv6: None,
                ttl_s: 20
            }))
        );
        assert_eq!(address_answer(&m, "nothing.here"), Ok(Lookup::NoData));
    }
}
