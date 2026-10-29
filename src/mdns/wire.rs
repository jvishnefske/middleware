//! DNS message wire format for mDNS / DNS-SD (RFC 1035 §4, RFC 6762,
//! RFC 6763), `no_std` and allocation free.
//!
//! [`Message::parse`] validates a whole message once: header, every
//! question, every resource record, every name (labels ≤ 63 bytes, names
//! ≤ 255 bytes, at most [`MAX_POINTER_HOPS`] compression pointers per name,
//! so a pointer loop is an error rather than a hang) and the RDATA of the
//! record types it knows. After that the iterators and accessors borrow the
//! message bytes and cannot fail.
//!
//! [`Name`] is a view into the message, so compressed names are never
//! copied; [`Name::write_text`] renders one as dotted text when a string is
//! needed. Names compare label by label, ignoring ASCII case.
//!
//! Records are reported from the answer, authority and additional sections
//! alike ([`Message::records`]); mDNS responders spread one instance's
//! records over the answer and additional sections.

use core::fmt;
use core::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4};

/// UDP port of mDNS.
pub const MDNS_PORT: u16 = 5353;
/// IPv4 mDNS multicast group.
pub const MDNS_GROUP_V4: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
/// Largest mDNS message (RFC 6762 §17), a sufficient receive buffer.
pub const MAX_MESSAGE_LEN: usize = 9000;
/// DNS header length.
pub const HEADER_LEN: usize = 12;
/// Longest label.
pub const MAX_LABEL_LEN: usize = 63;
/// Longest name in wire form, including the root label.
pub const MAX_NAME_LEN: usize = 255;
/// Compression pointers followed in one name before it is rejected.
pub const MAX_POINTER_HOPS: usize = 32;

/// Record type A (IPv4 address).
pub const TYPE_A: u16 = 1;
/// Record type PTR (DNS-SD: service type → instance).
pub const TYPE_PTR: u16 = 12;
/// Record type TXT (DNS-SD: instance attributes).
pub const TYPE_TXT: u16 = 16;
/// Record type AAAA (IPv6 address).
pub const TYPE_AAAA: u16 = 28;
/// Record type SRV (DNS-SD: instance → host and port).
pub const TYPE_SRV: u16 = 33;
/// Class IN.
pub const CLASS_IN: u16 = 1;

/// Header flag: the message is a response.
const FLAG_QR: u16 = 0x8000;
/// Top bit of a question's class (unicast response requested) or a record's
/// class (cache flush).
const CLASS_TOP_BIT: u16 = 0x8000;

/// Why a message could not be decoded or encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    /// The message ends inside a header, name or record.
    Truncated,
    /// A label length byte uses the reserved `0x40` / `0x80` forms.
    BadLabel,
    /// A name is longer than [`MAX_NAME_LEN`] bytes.
    NameTooLong,
    /// A name follows more than [`MAX_POINTER_HOPS`] compression pointers.
    PointerLoop,
    /// A record's RDATA length does not match its type.
    BadRdata,
    /// A service type is not `_service._tcp|_udp.domain` with valid labels.
    InvalidServiceType,
    /// The output buffer is too small.
    BufferTooSmall,
    /// A name rendered as text is not UTF-8.
    NotUtf8,
}

fn bytes_at(p: &[u8], at: usize, n: usize) -> Result<&[u8], WireError> {
    p.get(at..)
        .and_then(|s| s.get(..n))
        .ok_or(WireError::Truncated)
}

fn array_at<const N: usize>(p: &[u8], at: usize) -> Result<[u8; N], WireError> {
    bytes_at(p, at, N)?
        .try_into()
        .map_err(|_| WireError::Truncated)
}

fn u16_at(p: &[u8], at: usize) -> Result<u16, WireError> {
    Ok(u16::from_be_bytes(array_at(p, at)?))
}

fn u32_at(p: &[u8], at: usize) -> Result<u32, WireError> {
    Ok(u32::from_be_bytes(array_at(p, at)?))
}

/// Walks the labels of a name, following compression pointers.
#[derive(Clone)]
struct Walk<'p> {
    packet: &'p [u8],
    at: usize,
    hops: usize,
    len: usize,
    /// Offset just past the name's in-place bytes, once known.
    end: Option<usize>,
    done: bool,
}

impl<'p> Walk<'p> {
    const fn new(packet: &'p [u8], at: usize) -> Self {
        Walk {
            packet,
            at,
            hops: 0,
            len: 0,
            end: None,
            done: false,
        }
    }

    /// The next label, `None` after the root label. Each call follows at
    /// most [`MAX_POINTER_HOPS`] pointers in total over the walk.
    fn next_label(&mut self) -> Result<Option<&'p [u8]>, WireError> {
        if self.done {
            return Ok(None);
        }
        for _ in 0..=MAX_POINTER_HOPS {
            let len = *self.packet.get(self.at).ok_or(WireError::Truncated)?;
            match len & 0xC0 {
                0x00 if len == 0 => {
                    self.done = true;
                    self.end.get_or_insert(self.at + 1);
                    return Ok(None);
                }
                0x00 => {
                    let n = usize::from(len);
                    let label = bytes_at(self.packet, self.at + 1, n)?;
                    self.len += n + 1;
                    if self.len + 1 > MAX_NAME_LEN {
                        return Err(WireError::NameTooLong);
                    }
                    self.at += n + 1;
                    return Ok(Some(label));
                }
                0xC0 => {
                    if self.hops == MAX_POINTER_HOPS {
                        return Err(WireError::PointerLoop);
                    }
                    self.hops += 1;
                    let target = usize::from(u16_at(self.packet, self.at)? & 0x3FFF);
                    self.end.get_or_insert(self.at + 2);
                    self.at = target;
                }
                _ => return Err(WireError::BadLabel),
            }
        }
        Err(WireError::PointerLoop)
    }
}

/// A domain name inside a message, possibly compressed. Validated when the
/// message was parsed.
#[derive(Clone, Copy)]
pub struct Name<'p> {
    packet: &'p [u8],
    at: usize,
}

impl<'p> Name<'p> {
    /// Validates the name at `at` and returns it with the offset just past
    /// its in-place bytes.
    fn parse(packet: &'p [u8], at: usize) -> Result<(Self, usize), WireError> {
        let mut walk = Walk::new(packet, at);
        // Bounded: each label adds at least 2 bytes to a length capped at
        // MAX_NAME_LEN, so a name has at most 128 labels.
        for _ in 0..=MAX_NAME_LEN / 2 {
            if walk.next_label()?.is_none() {
                let end = walk.end.ok_or(WireError::Truncated)?;
                return Ok((Name { packet, at }, end));
            }
        }
        Err(WireError::NameTooLong)
    }

    /// The labels, outermost (leftmost) first, excluding the root label.
    pub fn labels(&self) -> Labels<'p> {
        Labels(Walk::new(self.packet, self.at))
    }

    /// The first label: for a service instance, its user-visible name.
    pub fn first_label(&self) -> Option<&'p [u8]> {
        self.labels().next()
    }

    /// True if the name is `service`, ignoring ASCII case.
    pub fn is_service(&self, service: &ServiceType<'_>) -> bool {
        labels_eq(self.labels(), service.labels())
    }

    /// Writes the name as dotted text (no trailing dot) into `out`. A `.` or
    /// `\` inside a label is escaped with `\`. The root name is empty.
    pub fn write_text<'o>(&self, out: &'o mut [u8]) -> Result<&'o str, WireError> {
        let mut w = Writer::new(out);
        for (i, label) in self.labels().enumerate() {
            if i > 0 {
                w.put(b".")?;
            }
            for &b in label {
                if b == b'.' || b == b'\\' {
                    w.put(b"\\")?;
                }
                w.put(&[b])?;
            }
        }
        let len = w.pos;
        let out = w.into_inner();
        core::str::from_utf8(out.get(..len).ok_or(WireError::BufferTooSmall)?)
            .map_err(|_| WireError::NotUtf8)
    }
}

fn labels_eq<'a, 'b>(
    mut a: impl Iterator<Item = &'a [u8]>,
    mut b: impl Iterator<Item = &'b [u8]>,
) -> bool {
    // Bounded by the shorter name's label count (≤ 128).
    loop {
        match (a.next(), b.next()) {
            (None, None) => return true,
            (Some(x), Some(y)) if x.eq_ignore_ascii_case(y) => {}
            _ => return false,
        }
    }
}

impl PartialEq for Name<'_> {
    fn eq(&self, other: &Self) -> bool {
        labels_eq(self.labels(), other.labels())
    }
}

impl Eq for Name<'_> {}

impl fmt::Debug for Name<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Name(")?;
        for (i, label) in self.labels().enumerate() {
            if i > 0 {
                f.write_str(".")?;
            }
            write!(f, "{}", label.escape_ascii())?;
        }
        f.write_str(")")
    }
}

/// Iterator over the labels of a [`Name`].
#[derive(Clone)]
pub struct Labels<'p>(Walk<'p>);

impl<'p> Iterator for Labels<'p> {
    type Item = &'p [u8];

    fn next(&mut self) -> Option<&'p [u8]> {
        // The name was validated when the message was parsed.
        self.0.next_label().ok().flatten()
    }
}

/// A DNS-SD service type such as `_http._tcp.local`: at least three labels,
/// the first `_`-prefixed, the second `_tcp` or `_udp`. One trailing dot is
/// accepted and ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceType<'a>(&'a str);

impl<'a> ServiceType<'a> {
    /// Validates `text`.
    pub fn new(text: &'a str) -> Result<Self, WireError> {
        let text = text.strip_suffix('.').unwrap_or(text);
        let mut count = 0usize;
        for label in text.split('.') {
            let ok = !label.is_empty()
                && label.len() <= MAX_LABEL_LEN
                && label.bytes().all(|b| b.is_ascii_graphic() && b != b'\\')
                && match count {
                    0 => label.len() > 1 && label.starts_with('_'),
                    1 => label.eq_ignore_ascii_case("_tcp") || label.eq_ignore_ascii_case("_udp"),
                    _ => true,
                };
            if !ok {
                return Err(WireError::InvalidServiceType);
            }
            count += 1;
        }
        // Wire length: one length byte per label, the label bytes (the dots
        // become length bytes), and the root label.
        if count < 3 || text.len() + 2 > MAX_NAME_LEN {
            return Err(WireError::InvalidServiceType);
        }
        Ok(ServiceType(text))
    }

    /// The dotted text, without a trailing dot.
    pub const fn as_str(&self) -> &'a str {
        self.0
    }

    /// The labels, outermost first.
    pub fn labels(&self) -> impl Iterator<Item = &'a [u8]> + Clone + use<'a> {
        self.0.split('.').map(str::as_bytes)
    }
}

/// Writes bytes into a slice, failing instead of truncating.
struct Writer<'o> {
    out: &'o mut [u8],
    pos: usize,
}

impl<'o> Writer<'o> {
    fn new(out: &'o mut [u8]) -> Self {
        Writer { out, pos: 0 }
    }

    fn put(&mut self, bytes: &[u8]) -> Result<(), WireError> {
        let end = self.pos + bytes.len();
        self.out
            .get_mut(self.pos..end)
            .ok_or(WireError::BufferTooSmall)?
            .copy_from_slice(bytes);
        self.pos = end;
        Ok(())
    }

    fn into_inner(self) -> &'o mut [u8] {
        self.out
    }
}

/// Encodes a one-question mDNS query for PTR records of `service` into
/// `out` and returns its length. `id` is echoed by responders answering a
/// query sent from a port other than [`MDNS_PORT`] (RFC 6762 §6.7).
pub fn encode_query(id: u16, service: ServiceType<'_>, out: &mut [u8]) -> Result<usize, WireError> {
    let mut w = Writer::new(out);
    w.put(&id.to_be_bytes())?;
    w.put(&0u16.to_be_bytes())?; // flags: standard query
    w.put(&1u16.to_be_bytes())?; // questions
    w.put(&[0; 6])?; // answers, authority, additional
    for label in service.labels() {
        // ServiceType::new bounds labels to MAX_LABEL_LEN, so this fits u8.
        w.put(&[u8::try_from(label.len()).map_err(|_| WireError::InvalidServiceType)?])?;
        w.put(label)?;
    }
    w.put(&[0])?;
    w.put(&TYPE_PTR.to_be_bytes())?;
    w.put(&CLASS_IN.to_be_bytes())?;
    Ok(w.pos)
}

/// One question of a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Question<'p> {
    /// The name asked about.
    pub name: Name<'p>,
    /// The record type asked for.
    pub qtype: u16,
    /// The class, without the unicast-response bit.
    pub class: u16,
    /// The querier asked for a unicast response (mDNS QU bit).
    pub unicast_response: bool,
}

fn parse_question(packet: &[u8], at: usize) -> Result<(Question<'_>, usize), WireError> {
    let (name, at) = Name::parse(packet, at)?;
    let qtype = u16_at(packet, at)?;
    let class = u16_at(packet, at + 2)?;
    let q = Question {
        name,
        qtype,
        class: class & !CLASS_TOP_BIT,
        unicast_response: class & CLASS_TOP_BIT != 0,
    };
    Ok((q, at + 4))
}

/// SRV record data: where an instance is served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Srv<'p> {
    /// Lower is preferred.
    pub priority: u16,
    /// Relative weight among equal priorities.
    pub weight: u16,
    /// The port the instance listens on.
    pub port: u16,
    /// The host serving the instance.
    pub target: Name<'p>,
}

/// TXT record data: a sequence of length-prefixed `key=value` strings.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Txt<'p>(&'p [u8]);

impl<'p> Txt<'p> {
    fn parse(data: &'p [u8]) -> Result<Self, WireError> {
        let mut at = 0;
        // Bounded: each entry consumes at least one byte.
        while at < data.len() {
            at += 1 + usize::from(data[at]);
        }
        if at == data.len() {
            Ok(Txt(data))
        } else {
            Err(WireError::BadRdata)
        }
    }

    /// The strings, in order. Empty strings are skipped.
    pub fn entries(&self) -> impl Iterator<Item = &'p [u8]> + use<'p> {
        let mut rest = self.0;
        core::iter::from_fn(move || {
            // Bounded by the record length; each step consumes ≥ 1 byte.
            loop {
                let (&len, tail) = rest.split_first()?;
                let (entry, tail) = tail.split_at_checked(usize::from(len))?;
                rest = tail;
                if !entry.is_empty() {
                    return Some(entry);
                }
            }
        })
    }

    /// The value of the first entry whose key is `key` (ASCII case
    /// ignored, RFC 6763 §6.4). A key with no `=` has an empty value.
    pub fn get(&self, key: &[u8]) -> Option<&'p [u8]> {
        self.entries().find_map(|e| {
            let (k, v) = match e.iter().position(|&b| b == b'=') {
                Some(i) => (&e[..i], &e[i + 1..]),
                None => (e, &e[e.len()..]),
            };
            k.eq_ignore_ascii_case(key).then_some(v)
        })
    }
}

impl fmt::Debug for Txt<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.entries().map(|e| e.escape_ascii()))
            .finish()
    }
}

/// Decoded RDATA of the record types DNS-SD uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rdata<'p> {
    /// IPv4 address.
    A(Ipv4Addr),
    /// IPv6 address.
    Aaaa(Ipv6Addr),
    /// Pointer to another name.
    Ptr(Name<'p>),
    /// Service location.
    Srv(Srv<'p>),
    /// Attributes.
    Txt(Txt<'p>),
    /// Any other type, undecoded.
    Other {
        /// The record type.
        rtype: u16,
        /// The raw RDATA.
        data: &'p [u8],
    },
}

/// One resource record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record<'p> {
    /// The owner name.
    pub name: Name<'p>,
    /// The class, without the cache-flush bit.
    pub class: u16,
    /// The mDNS cache-flush bit (RFC 6762 §10.2).
    pub cache_flush: bool,
    /// Seconds the record stays valid; 0 announces its removal.
    pub ttl: u32,
    /// The data.
    pub data: Rdata<'p>,
}

fn parse_record(packet: &[u8], at: usize) -> Result<(Record<'_>, usize), WireError> {
    let (name, at) = Name::parse(packet, at)?;
    let rtype = u16_at(packet, at)?;
    let class = u16_at(packet, at + 2)?;
    let ttl = u32_at(packet, at + 4)?;
    let len = usize::from(u16_at(packet, at + 8)?);
    let start = at + 10;
    let raw = bytes_at(packet, start, len)?;
    let end = start + len;
    // A name in RDATA must end exactly at the end of the RDATA.
    let name_at = |off: usize| match Name::parse(packet, start + off)? {
        (n, e) if e == end => Ok(n),
        _ => Err(WireError::BadRdata),
    };
    let data = match rtype {
        TYPE_A => Rdata::A(Ipv4Addr::from(
            <[u8; 4]>::try_from(raw).map_err(|_| WireError::BadRdata)?,
        )),
        TYPE_AAAA => Rdata::Aaaa(Ipv6Addr::from(
            <[u8; 16]>::try_from(raw).map_err(|_| WireError::BadRdata)?,
        )),
        TYPE_PTR => Rdata::Ptr(name_at(0)?),
        TYPE_SRV => {
            if len < 7 {
                return Err(WireError::BadRdata);
            }
            Rdata::Srv(Srv {
                priority: u16_at(packet, start)?,
                weight: u16_at(packet, start + 2)?,
                port: u16_at(packet, start + 4)?,
                target: name_at(6)?,
            })
        }
        TYPE_TXT => Rdata::Txt(Txt::parse(raw)?),
        rtype => Rdata::Other { rtype, data: raw },
    };
    let rec = Record {
        name,
        class: class & !CLASS_TOP_BIT,
        cache_flush: class & CLASS_TOP_BIT != 0,
        ttl,
        data,
    };
    Ok((rec, end))
}

/// A validated DNS message borrowing its bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Message<'p> {
    packet: &'p [u8],
    id: u16,
    flags: u16,
    questions: u16,
    records: u32,
    records_at: usize,
}

impl<'p> Message<'p> {
    /// Validates `packet` as a DNS message. Bytes after the last record are
    /// ignored.
    pub fn parse(packet: &'p [u8]) -> Result<Self, WireError> {
        let id = u16_at(packet, 0)?;
        let flags = u16_at(packet, 2)?;
        let questions = u16_at(packet, 4)?;
        let records = [6, 8, 10].into_iter().try_fold(0u32, |n, at| {
            Ok::<_, WireError>(n + u32::from(u16_at(packet, at)?))
        })?;
        let mut at = HEADER_LEN;
        // Bounded by the 16-bit counts; each item also consumes ≥ 5 bytes.
        for _ in 0..questions {
            at = parse_question(packet, at)?.1;
        }
        let records_at = at;
        for _ in 0..records {
            at = parse_record(packet, at)?.1;
        }
        Ok(Message {
            packet,
            id,
            flags,
            questions,
            records,
            records_at,
        })
    }

    /// The message id (0 in multicast mDNS).
    pub const fn id(&self) -> u16 {
        self.id
    }

    /// True for a response, false for a query.
    pub const fn is_response(&self) -> bool {
        self.flags & FLAG_QR != 0
    }

    /// The message as a [`Response`], if it is one.
    pub const fn response(self) -> Option<Response<'p>> {
        if self.is_response() {
            Some(Response(self))
        } else {
            None
        }
    }

    /// The questions.
    pub fn questions(&self) -> impl Iterator<Item = Question<'p>> + use<'p> {
        let packet = self.packet;
        let mut at = HEADER_LEN;
        (0..self.questions).map_while(move |_| {
            let (q, next) = parse_question(packet, at).ok()?;
            at = next;
            Some(q)
        })
    }

    /// Every resource record: answers, then authority, then additional.
    pub fn records(&self) -> Records<'p> {
        Records {
            packet: self.packet,
            at: self.records_at,
            remaining: self.records,
        }
    }
}

/// Iterator over the records of a [`Message`].
#[derive(Debug, Clone)]
pub struct Records<'p> {
    packet: &'p [u8],
    at: usize,
    remaining: u32,
}

impl<'p> Iterator for Records<'p> {
    type Item = Record<'p>;

    fn next(&mut self) -> Option<Record<'p>> {
        self.remaining = self.remaining.checked_sub(1)?;
        // The records were validated when the message was parsed.
        let (rec, next) = parse_record(self.packet, self.at).ok()?;
        self.at = next;
        Some(rec)
    }
}

/// A validated DNS message with the response bit set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Response<'p>(Message<'p>);

impl<'p> Response<'p> {
    /// Parses `packet`, returning `None` for a well-formed query.
    pub fn parse(packet: &'p [u8]) -> Result<Option<Self>, WireError> {
        Ok(Message::parse(packet)?.response())
    }

    /// The underlying message.
    pub const fn message(&self) -> &Message<'p> {
        &self.0
    }

    /// Every resource record.
    pub fn records(&self) -> Records<'p> {
        self.0.records()
    }

    /// The instances of `service` this response announces, one per PTR
    /// record owned by `service`, in record order.
    pub fn instances<'s>(
        &self,
        service: ServiceType<'s>,
    ) -> impl Iterator<Item = Instance<'p>> + use<'p, 's> {
        let records = self.records();
        self.records().filter_map(move |ptr| match ptr.data {
            Rdata::Ptr(name) if ptr.name.is_service(&service) => {
                Some(resolve(records.clone(), name, ptr.ttl))
            }
            _ => None,
        })
    }
}

/// Joins the SRV, TXT and address records of `name` found in `records`.
fn resolve<'p>(records: Records<'p>, name: Name<'p>, ttl: u32) -> Instance<'p> {
    if ttl == 0 {
        return Instance::Gone(name);
    }
    let Some(srv) = records.clone().find_map(|r| match r.data {
        Rdata::Srv(srv) if r.name == name => Some(srv),
        _ => None,
    }) else {
        return Instance::Unresolved(name);
    };
    let mut found = ServiceInstance {
        name,
        srv,
        txt: None,
        ipv4: None,
        ipv6: None,
        ttl,
    };
    for r in records {
        match r.data {
            Rdata::Txt(txt) if found.txt.is_none() && r.name == name => found.txt = Some(txt),
            Rdata::A(a) if found.ipv4.is_none() && r.name == srv.target => found.ipv4 = Some(a),
            Rdata::Aaaa(a) if found.ipv6.is_none() && r.name == srv.target => found.ipv6 = Some(a),
            _ => {}
        }
    }
    Instance::Resolved(found)
}

/// What a response says about one instance of a service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Instance<'p> {
    /// The PTR and the instance's SRV record were both present.
    Resolved(ServiceInstance<'p>),
    /// Only the PTR was present; the instance's SRV must be asked for.
    Unresolved(Name<'p>),
    /// A goodbye: the PTR's TTL is 0, the instance is gone.
    Gone(Name<'p>),
}

/// A resolved service instance, borrowing the response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceInstance<'p> {
    /// The full instance name, e.g. `My Printer._http._tcp.local`.
    pub name: Name<'p>,
    /// Host and port.
    pub srv: Srv<'p>,
    /// The instance's attributes, if the response carried them.
    pub txt: Option<Txt<'p>>,
    /// The host's IPv4 address, if the response carried an A record.
    pub ipv4: Option<Ipv4Addr>,
    /// The host's IPv6 address, if the response carried an AAAA record.
    pub ipv6: Option<Ipv6Addr>,
    /// The PTR's TTL in seconds.
    pub ttl: u32,
}

impl<'p> ServiceInstance<'p> {
    /// The instance's user-visible name: the first label as UTF-8.
    pub fn label(&self) -> Result<&'p str, WireError> {
        core::str::from_utf8(self.name.first_label().unwrap_or_default())
            .map_err(|_| WireError::NotUtf8)
    }

    /// The IPv4 socket address, when an A record was present.
    pub fn socket_addr_v4(&self) -> Option<SocketAddrV4> {
        self.ipv4.map(|ip| SocketAddrV4::new(ip, self.srv.port))
    }
}

/// Test fixtures: builds responses with compressed names.
#[cfg(test)]
pub(crate) mod fixture {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    /// Appends `name` as labels and returns its offset.
    pub fn name(p: &mut Vec<u8>, dotted: &str) -> usize {
        let at = p.len();
        for l in dotted.split('.') {
            p.push(l.len() as u8);
            p.extend_from_slice(l.as_bytes());
        }
        p.push(0);
        at
    }

    /// Appends labels then a pointer to `suffix`; returns the offset.
    pub fn name_ptr(p: &mut Vec<u8>, labels: &[&str], suffix: usize) -> usize {
        let at = p.len();
        for l in labels {
            p.push(l.len() as u8);
            p.extend_from_slice(l.as_bytes());
        }
        p.extend_from_slice(&(0xC000 | suffix as u16).to_be_bytes());
        at
    }

    pub fn pointer(p: &mut Vec<u8>, to: usize) {
        p.extend_from_slice(&(0xC000 | to as u16).to_be_bytes());
    }

    pub fn rr_head(p: &mut Vec<u8>, rtype: u16, class: u16, ttl: u32) {
        p.extend_from_slice(&rtype.to_be_bytes());
        p.extend_from_slice(&class.to_be_bytes());
        p.extend_from_slice(&ttl.to_be_bytes());
    }

    /// Writes an RDATA length placeholder; `end_rdata` fills it in.
    pub fn begin_rdata(p: &mut Vec<u8>) -> usize {
        p.extend_from_slice(&[0, 0]);
        p.len()
    }

    pub fn end_rdata(p: &mut [u8], start: usize) {
        let len = (p.len() - start) as u16;
        p[start - 2..start].copy_from_slice(&len.to_be_bytes());
    }

    /// A typical DNS-SD answer: PTR in the answer section, SRV, TXT and A
    /// in the additional section, names compressed against the PTR owner.
    pub fn printer(id: u16, ttl: u32) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&id.to_be_bytes());
        p.extend_from_slice(&0x8400u16.to_be_bytes()); // response, authoritative
        p.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 3]);
        // PTR _http._tcp.local -> My.Printer._http._tcp.local
        let service = name(&mut p, "_http._tcp.local");
        let local = service + 1 + 5 + 1 + 4;
        rr_head(&mut p, TYPE_PTR, CLASS_IN, ttl);
        let s = begin_rdata(&mut p);
        let instance = name_ptr(&mut p, &["My.Printer"], service);
        end_rdata(&mut p, s);
        // SRV
        pointer(&mut p, instance);
        rr_head(&mut p, TYPE_SRV, CLASS_IN | CLASS_TOP_BIT, 120);
        let s = begin_rdata(&mut p);
        p.extend_from_slice(&[0, 0, 0, 0, 0x1F, 0x90]); // priority 0, weight 0, port 8080
        let host = name_ptr(&mut p, &["printer"], local);
        end_rdata(&mut p, s);
        // TXT
        pointer(&mut p, instance);
        rr_head(&mut p, TYPE_TXT, CLASS_IN | CLASS_TOP_BIT, 4500);
        let s = begin_rdata(&mut p);
        p.extend_from_slice(b"\x06path=/\x05Color");
        end_rdata(&mut p, s);
        // A
        pointer(&mut p, host);
        rr_head(&mut p, TYPE_A, CLASS_IN | CLASS_TOP_BIT, 120);
        let s = begin_rdata(&mut p);
        p.extend_from_slice(&[192, 168, 1, 20]);
        end_rdata(&mut p, s);
        p
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::fixture::*;
    use super::*;
    use std::vec::Vec;

    fn http() -> ServiceType<'static> {
        ServiceType::new("_http._tcp.local").unwrap()
    }

    #[test]
    fn service_types_are_validated() {
        assert_eq!(
            ServiceType::new("_http._tcp.local.").unwrap().as_str(),
            "_http._tcp.local"
        );
        assert!(ServiceType::new("_mw._UDP.example.com").is_ok());
        for bad in [
            "",
            "_http._tcp",
            "http._tcp.local",
            "_http.tcp.local",
            "_._tcp.local",
            "_http._tcp..local",
            "_a b._tcp.local",
        ] {
            assert_eq!(
                ServiceType::new(bad),
                Err(WireError::InvalidServiceType),
                "{bad}"
            );
        }
        let long = std::format!("_x._tcp.{}", std::vec!["a".repeat(63); 4].join("."));
        assert_eq!(ServiceType::new(&long), Err(WireError::InvalidServiceType));
    }

    #[test]
    fn query_encodes_one_ptr_question() {
        let mut out = [0u8; 64];
        let n = encode_query(0x1234, http(), &mut out).unwrap();
        assert_eq!(
            &out[..n],
            b"\x12\x34\0\0\0\x01\0\0\0\0\0\0\x05_http\x04_tcp\x05local\0\0\x0c\0\x01"
        );
        let msg = Message::parse(&out[..n]).unwrap();
        assert!(!msg.is_response());
        let q: Vec<_> = msg.questions().collect();
        assert_eq!(q.len(), 1);
        assert!(q[0].name.is_service(&http()));
        assert_eq!(
            (q[0].qtype, q[0].class, q[0].unicast_response),
            (TYPE_PTR, CLASS_IN, false)
        );
        assert_eq!(
            encode_query(0, http(), &mut out[..n - 1]),
            Err(WireError::BufferTooSmall)
        );
    }

    #[test]
    fn response_resolves_an_instance_from_compressed_records() {
        let p = printer(7, 4500);
        let r = Response::parse(&p).unwrap().unwrap();
        assert_eq!(r.message().id(), 7);
        assert_eq!(r.records().count(), 4);
        let all: Vec<_> = r.instances(http()).collect();
        let [Instance::Resolved(i)] = all[..] else {
            panic!("{all:?}")
        };
        assert_eq!(i.label(), Ok("My.Printer"));
        let mut buf = [0u8; 64];
        assert_eq!(
            i.name.write_text(&mut buf),
            Ok("My\\.Printer._http._tcp.local")
        );
        assert_eq!(i.srv.target.write_text(&mut buf), Ok("printer.local"));
        assert_eq!(
            i.socket_addr_v4(),
            Some(SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 20), 8080))
        );
        assert_eq!(i.ipv6, None);
        assert_eq!(i.ttl, 4500);
        let txt = i.txt.unwrap();
        assert_eq!(txt.get(b"PATH"), Some(&b"/"[..]));
        assert_eq!(txt.get(b"color"), Some(&b""[..]));
        assert_eq!(txt.get(b"missing"), None);
        assert!(r.records().skip(1).all(|rec| rec.cache_flush));
    }

    #[test]
    fn other_services_and_queries_yield_no_instances() {
        let p = printer(0, 120);
        let r = Response::parse(&p).unwrap().unwrap();
        assert_eq!(
            r.instances(ServiceType::new("_ipp._tcp.local").unwrap())
                .count(),
            0
        );
        // Same service, case differs: still matches.
        assert_eq!(
            r.instances(ServiceType::new("_HTTP._tcp.LOCAL").unwrap())
                .count(),
            1
        );
        let mut q = [0u8; 64];
        let n = encode_query(0, http(), &mut q).unwrap();
        assert!(Response::parse(&q[..n]).unwrap().is_none());
    }

    #[test]
    fn goodbye_and_ptr_only_responses() {
        let p = printer(0, 0);
        let r = Response::parse(&p).unwrap().unwrap();
        let all: Vec<_> = r.instances(http()).collect();
        assert!(matches!(all[..], [Instance::Gone(n)] if n.first_label() == Some(b"My.Printer")));

        // Keep only the PTR answer.
        let mut p = printer(0, 60);
        p[11] = 0; // additional count
        let r = Response::parse(&p).unwrap().unwrap();
        let all: Vec<_> = r.instances(http()).collect();
        assert!(matches!(all[..], [Instance::Unresolved(_)]));
    }

    #[test]
    fn malformed_messages_are_errors() {
        let p = printer(0, 60);
        for cut in 0..p.len() {
            assert!(Message::parse(&p[..cut]).is_err(), "cut at {cut}");
        }
        // A pointer to itself loops.
        let mut l = std::vec![0, 0, 0x84, 0, 0, 1, 0, 0, 0, 0, 0, 0];
        pointer(&mut l, HEADER_LEN);
        l.extend_from_slice(&[0, 12, 0, 1]);
        assert_eq!(Message::parse(&l), Err(WireError::PointerLoop));
        // Reserved label type.
        let mut b = l.clone();
        b[HEADER_LEN] = 0x40;
        assert_eq!(Message::parse(&b), Err(WireError::BadLabel));
        // A record with 3 bytes of IPv4 address.
        let mut a = std::vec![0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 0];
        name(&mut a, "h.local");
        rr_head(&mut a, TYPE_A, CLASS_IN, 1);
        a.extend_from_slice(&[0, 3, 1, 2, 3]);
        assert_eq!(Message::parse(&a), Err(WireError::BadRdata));
        // Four 63-byte labels: 4 * 64 + 1 = 257 bytes in wire form.
        let mut q = std::vec![0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
        name(&mut q, &std::vec!["a".repeat(63); 4].join("."));
        q.extend_from_slice(&[0, 12, 0, 1]);
        assert_eq!(Message::parse(&q), Err(WireError::NameTooLong));
    }

    #[test]
    fn write_text_refuses_to_truncate() {
        let p = printer(0, 60);
        let r = Response::parse(&p).unwrap().unwrap();
        let Some(Instance::Resolved(i)) = r.instances(http()).next() else {
            panic!()
        };
        let mut small = [0u8; 8];
        assert_eq!(
            i.name.write_text(&mut small),
            Err(WireError::BufferTooSmall)
        );
    }
}
