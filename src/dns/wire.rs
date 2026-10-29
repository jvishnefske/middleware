//! DNS message wire format for unicast DNS, mDNS and DNS-SD (RFC 1035 §4,
//! RFC 6762, RFC 6763), `no_std` and allocation free. Moved here from
//! `mdns::wire` in 0.5; `mdns::wire` re-exports this module.
//!
//! # Decoding
//!
//! [`Message::parse`] validates a whole message once: header, every
//! question, every resource record, every name (labels ≤ 63 bytes, names
//! ≤ 255 bytes, at most [`MAX_POINTER_HOPS`] compression pointers per name,
//! so a pointer loop is an error rather than a hang) and the RDATA of the
//! record types it knows. A message with more than [`MAX_RECORDS`] resource
//! records is refused ([`WireError::TooManyRecords`]) before any is parsed,
//! which bounds the work of every accessor, including the
//! O(PTR × records) join of [`Response::instances`] that a hostile peer
//! could otherwise inflate with a 9000-byte message of PTRs. After that the
//! iterators and accessors borrow the message bytes and cannot fail.
//!
//! [`Name`] is a view into the message, so compressed names are never
//! copied; [`Name::write_text`] renders one as dotted text when a string is
//! needed. Names compare label by label, ignoring ASCII case.
//!
//! Records are reported from the answer, authority and additional sections
//! alike ([`Message::records`]); mDNS responders spread one instance's
//! records over the answer and additional sections.
//!
//! # Encoding
//!
//! [`MessageWriter`] builds a message into a caller buffer: questions, then
//! PTR, SRV, TXT, A and AAAA records ([`RecordData`]) in the answer,
//! authority and additional sections, in that order. Owner names and the
//! names inside PTR and SRV data are compressed against every name written
//! before (RFC 1035 §4.1.4; RFC 6762 §18.14 allows it for SRV targets, and
//! unicast resolvers accept it). Names to encode are [`TextName`]s: dotted
//! text with `\.` and `\\` escapes, the form [`Name::write_text`] produces,
//! or an instance label joined to a [`ServiceType`]. [`encode_query`] (one
//! PTR question, mDNS browse) and [`encode_query_for`] (any type, optional
//! recursion desired, for a unicast resolver) are shorthands. Nothing is
//! truncated: a full buffer is [`WireError::BufferTooSmall`].
//!
//! [`TxtWriter`] builds TXT RDATA (`key=value` strings, RFC 6763 §6) and
//! [`Txt::new`] validates it.

use core::fmt;
use core::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4};

/// Largest message this codec is meant for: mDNS's 9000 bytes (RFC 6762
/// §17), above unicast DNS over UDP's 512 (or EDNS's typical 1232), so a
/// sufficient receive buffer for both.
pub const MAX_MESSAGE_LEN: usize = 9000;
/// Most resource records (answer + authority + additional) a message may
/// carry; [`Message::parse`] refuses more with
/// [`WireError::TooManyRecords`]. A responder answering one service with a
/// few dozen instances stays well below it.
pub const MAX_RECORDS: usize = 64;
/// Distinct name offsets a [`MessageWriter`] remembers as compression
/// targets; later names compress only against these.
pub const MAX_COMPRESSION_TARGETS: usize = 32;
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
pub const FLAG_QR: u16 = 0x8000;
/// Header flag: authoritative answer (every mDNS response sets it).
pub const FLAG_AA: u16 = 0x0400;
/// Header flag: recursion desired (unicast queries to a resolver).
pub const FLAG_RD: u16 = 0x0100;
/// Header flag: recursion available (set by resolvers in responses).
pub const FLAG_RA: u16 = 0x0080;
/// Mask of the response code in the header flags (0 = no error,
/// 3 = NXDOMAIN).
pub const RCODE_MASK: u16 = 0x000F;
/// Top bit of a question's class (unicast response requested) or a record's
/// class (cache flush).
const CLASS_TOP_BIT: u16 = 0x8000;

/// Why a message could not be decoded or encoded. Non-exhaustive: match
/// with a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
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
    /// The message declares more than [`MAX_RECORDS`] resource records.
    TooManyRecords,
    /// A name given as text is not a valid name: an empty label, a label
    /// over 63 bytes, a dangling `\\` escape, or over 255 bytes in wire form.
    InvalidName,
    /// A [`MessageWriter`] was asked to write into a section before one it
    /// has already written (sections go question, answer, authority,
    /// additional).
    SectionOrder,
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
/// query sent from a port other than the mDNS port (RFC 6762 §6.7).
pub fn encode_query(id: u16, service: ServiceType<'_>, out: &mut [u8]) -> Result<usize, WireError> {
    encode_query_for(id, TextName::service(service), TYPE_PTR, false, out)
}

/// Encodes a one-question query for `qtype` records of `name` (class IN),
/// with the recursion-desired bit when `recursion_desired` (a query to a
/// unicast resolver), and returns its length.
pub fn encode_query_for(
    id: u16,
    name: TextName<'_>,
    qtype: u16,
    recursion_desired: bool,
    out: &mut [u8],
) -> Result<usize, WireError> {
    let flags = if recursion_desired { FLAG_RD } else { 0 };
    Ok(MessageWriter::new(out, id, flags)?
        .question(name, qtype, false)?
        .finish())
}

/// A domain name given as text, to encode.
///
/// Either dotted text with `\.` and `\\` escapes for a literal dot or
/// backslash inside a label (what [`Name::write_text`] produces; a trailing
/// dot is ignored; `""` is the root), or a raw first label joined to a
/// [`ServiceType`] ([`TextName::instance`]), the form of a DNS-SD instance
/// name like `My.Printer._http._tcp.local` whose first label contains a dot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextName<'a> {
    /// A raw, unescaped first label.
    first: Option<&'a [u8]>,
    /// The rest as escaped dotted text without a trailing dot.
    rest: &'a str,
}

/// One label of a [`TextName`], possibly escaped.
#[derive(Debug, Clone, Copy)]
struct TextLabel<'a> {
    raw: &'a [u8],
    escaped: bool,
}

impl<'a> TextLabel<'a> {
    /// The label's bytes with escapes removed.
    fn bytes(self) -> impl Iterator<Item = u8> + 'a {
        let mut escape = false;
        self.raw.iter().filter_map(move |&b| {
            if self.escaped && !escape && b == b'\\' {
                escape = true;
                None
            } else {
                escape = false;
                Some(b)
            }
        })
    }

    fn len(self) -> usize {
        self.bytes().count()
    }

    fn eq_ignore_case(self, other: &[u8]) -> bool {
        self.len() == other.len()
            && self
                .bytes()
                .zip(other.iter())
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
    }
}

/// Splits escaped dotted text at unescaped dots.
fn split_escaped(text: &str) -> impl Iterator<Item = TextLabel<'_>> + Clone {
    let mut rest = text.as_bytes();
    core::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let mut i = 0;
        // Bounded by the text length.
        while i < rest.len() {
            match rest[i] {
                b'\\' => i += 2,
                b'.' => break,
                _ => i += 1,
            }
        }
        let i = i.min(rest.len());
        let (label, tail) = rest.split_at(i);
        rest = tail.split_first().map_or(tail, |(_, t)| t);
        Some(TextLabel {
            raw: label,
            escaped: true,
        })
    })
}

impl<'a> TextName<'a> {
    /// The root name.
    pub const ROOT: TextName<'static> = TextName {
        first: None,
        rest: "",
    };

    /// Validates escaped dotted text (see the type docs).
    pub fn new(text: &'a str) -> Result<Self, WireError> {
        let odd = |t: &str| t.bytes().rev().take_while(|&b| b == b'\\').count() % 2 == 1;
        // A trailing dot is the root label unless it is escaped.
        let rest = match text.strip_suffix('.') {
            Some(r) if !odd(r) => r,
            _ => text,
        };
        if odd(rest) {
            // A dangling escape.
            return Err(WireError::InvalidName);
        }
        TextName { first: None, rest }.validated()
    }

    /// The name of a DNS-SD service type, e.g. `_http._tcp.local`.
    pub fn service(service: ServiceType<'a>) -> Self {
        // ServiceType labels are graphic ASCII without `\\`, ≤ 63 bytes,
        // ≤ 255 bytes in all: already a valid name.
        TextName {
            first: None,
            rest: service.as_str(),
        }
    }

    /// The instance name `label.<service>`: `label` is taken literally
    /// (it may contain dots and spaces), 1..=63 bytes.
    pub fn instance(label: &'a str, service: ServiceType<'a>) -> Result<Self, WireError> {
        TextName {
            first: Some(label.as_bytes()),
            rest: service.as_str(),
        }
        .validated()
    }

    fn validated(self) -> Result<Self, WireError> {
        let mut wire = 1usize;
        // Bounded: ≤ 255 bytes of text.
        for label in self.labels() {
            let n = label.len();
            if n == 0 || n > MAX_LABEL_LEN {
                return Err(WireError::InvalidName);
            }
            wire += n + 1;
        }
        if wire > MAX_NAME_LEN {
            return Err(WireError::InvalidName);
        }
        Ok(self)
    }

    fn labels(&self) -> impl Iterator<Item = TextLabel<'a>> + Clone + use<'a> {
        let first = self.first.map(|raw| TextLabel {
            raw,
            escaped: false,
        });
        first.into_iter().chain(split_escaped(self.rest))
    }
}

/// Section of a message a record is written to, in wire order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Section {
    /// Answers to the questions.
    Answer,
    /// Authority records (mDNS probes put proposed records here).
    Authority,
    /// Additional records (DNS-SD puts SRV, TXT and addresses here).
    Additional,
}

impl Section {
    const fn index(self) -> usize {
        match self {
            Section::Answer => 1,
            Section::Authority => 2,
            Section::Additional => 3,
        }
    }
}

/// RDATA to encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordData<'a> {
    /// IPv4 address.
    A(Ipv4Addr),
    /// IPv6 address.
    Aaaa(Ipv6Addr),
    /// Pointer to another name (DNS-SD: service type → instance).
    Ptr(TextName<'a>),
    /// Service location.
    Srv {
        /// Lower is preferred.
        priority: u16,
        /// Relative weight among equal priorities.
        weight: u16,
        /// The port.
        port: u16,
        /// The host.
        target: TextName<'a>,
    },
    /// Attributes: TXT RDATA as validated by [`Txt::new`] (e.g. built with
    /// [`TxtWriter`]). Empty data is written as one empty string, the
    /// smallest valid TXT record (RFC 6763 §6.1).
    Txt(Txt<'a>),
}

impl RecordData<'_> {
    /// The record type written.
    pub const fn rtype(&self) -> u16 {
        match self {
            RecordData::A(_) => TYPE_A,
            RecordData::Aaaa(_) => TYPE_AAAA,
            RecordData::Ptr(_) => TYPE_PTR,
            RecordData::Srv { .. } => TYPE_SRV,
            RecordData::Txt(_) => TYPE_TXT,
        }
    }
}

/// One resource record to encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordOut<'a> {
    /// The owner name.
    pub name: TextName<'a>,
    /// Seconds the record stays valid; 0 announces its removal (an mDNS
    /// goodbye).
    pub ttl: u32,
    /// Set the mDNS cache-flush bit (RFC 6762 §10.2): for unique records
    /// (SRV, TXT, A, AAAA), never for shared PTRs.
    pub cache_flush: bool,
    /// The data. The class is always IN.
    pub data: RecordData<'a>,
}

/// Builds a DNS message into a caller buffer, without allocating.
///
/// A consuming builder: each call returns the writer or the error, so
/// `MessageWriter::new(buf, 0, FLAG_QR | FLAG_AA)?.record(..)?.finish()`
/// either yields the whole message's length or fails; there is no partial
/// success to forget about. The header counts are kept current, so the
/// bytes up to [`MessageWriter::len`] are always a valid message.
#[derive(Debug)]
pub struct MessageWriter<'o> {
    out: &'o mut [u8],
    pos: usize,
    /// Highest section written so far: 0 question, 1..=3 as [`Section`].
    section: usize,
    counts: [u16; 4],
    /// Offsets of labels written in place, compression targets.
    targets: [u16; MAX_COMPRESSION_TARGETS],
    n_targets: usize,
}

impl<'o> MessageWriter<'o> {
    /// Starts a message with `id` and header `flags` ([`FLAG_QR`],
    /// [`FLAG_AA`], [`FLAG_RD`], ...).
    pub fn new(out: &'o mut [u8], id: u16, flags: u16) -> Result<Self, WireError> {
        let header = out.get_mut(..HEADER_LEN).ok_or(WireError::BufferTooSmall)?;
        header.fill(0);
        header[..2].copy_from_slice(&id.to_be_bytes());
        header[2..4].copy_from_slice(&flags.to_be_bytes());
        Ok(MessageWriter {
            out,
            pos: HEADER_LEN,
            section: 0,
            counts: [0; 4],
            targets: [0; MAX_COMPRESSION_TARGETS],
            n_targets: 0,
        })
    }

    /// Bytes written so far: a complete message.
    pub const fn len(&self) -> usize {
        self.pos
    }

    /// Always false: a message has at least its header.
    pub const fn is_empty(&self) -> bool {
        false
    }

    /// Finishes the message and returns its length.
    pub fn finish(self) -> usize {
        self.pos
    }

    /// Adds a question for `qtype` records of `name`, class IN, with the
    /// mDNS unicast-response bit when `unicast_response`. Questions come
    /// before every record.
    pub fn question(
        mut self,
        name: TextName<'_>,
        qtype: u16,
        unicast_response: bool,
    ) -> Result<Self, WireError> {
        if self.section > 0 {
            return Err(WireError::SectionOrder);
        }
        self.name(name)?;
        let class = CLASS_IN | if unicast_response { CLASS_TOP_BIT } else { 0 };
        self.put(&qtype.to_be_bytes())?;
        self.put(&class.to_be_bytes())?;
        self.count(0)?;
        Ok(self)
    }

    /// Adds `record` to `section`. Sections are written in order: a record
    /// for an earlier section than the last one written is
    /// [`WireError::SectionOrder`].
    pub fn record(mut self, section: Section, record: &RecordOut<'_>) -> Result<Self, WireError> {
        let index = section.index();
        if index < self.section {
            return Err(WireError::SectionOrder);
        }
        if usize::from(self.counts[1]) + usize::from(self.counts[2]) + usize::from(self.counts[3])
            >= MAX_RECORDS
        {
            return Err(WireError::TooManyRecords);
        }
        self.section = index;
        self.name(record.name)?;
        let class = CLASS_IN | if record.cache_flush { CLASS_TOP_BIT } else { 0 };
        self.put(&record.data.rtype().to_be_bytes())?;
        self.put(&class.to_be_bytes())?;
        self.put(&record.ttl.to_be_bytes())?;
        let len_at = self.pos;
        self.put(&[0, 0])?;
        match record.data {
            RecordData::A(a) => self.put(&a.octets())?,
            RecordData::Aaaa(a) => self.put(&a.octets())?,
            RecordData::Ptr(name) => self.name(name)?,
            RecordData::Srv {
                priority,
                weight,
                port,
                target,
            } => {
                self.put(&priority.to_be_bytes())?;
                self.put(&weight.to_be_bytes())?;
                self.put(&port.to_be_bytes())?;
                self.name(target)?;
            }
            RecordData::Txt(txt) if txt.as_bytes().is_empty() => self.put(&[0])?,
            RecordData::Txt(txt) => self.put(txt.as_bytes())?,
        }
        let rdlen = u16::try_from(self.pos - len_at - 2).map_err(|_| WireError::BadRdata)?;
        self.out
            .get_mut(len_at..len_at + 2)
            .ok_or(WireError::BufferTooSmall)?
            .copy_from_slice(&rdlen.to_be_bytes());
        self.count(index)?;
        Ok(self)
    }

    fn count(&mut self, index: usize) -> Result<(), WireError> {
        let n = self.counts[index]
            .checked_add(1)
            .ok_or(WireError::TooManyRecords)?;
        self.counts[index] = n;
        let at = 4 + 2 * index;
        self.out
            .get_mut(at..at + 2)
            .ok_or(WireError::BufferTooSmall)?
            .copy_from_slice(&n.to_be_bytes());
        Ok(())
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

    /// The offset of an earlier name equal to `suffix`, if one is a
    /// compression target.
    fn find<'a>(&self, suffix: impl Iterator<Item = TextLabel<'a>> + Clone) -> Option<u16> {
        let written = self.out.get(..self.pos)?;
        self.targets
            .get(..self.n_targets)?
            .iter()
            .copied()
            .find(|&at| {
                let mut walk = Walk::new(written, usize::from(at));
                let mut want = suffix.clone();
                // Bounded: a name has at most 128 labels.
                for _ in 0..=MAX_NAME_LEN / 2 {
                    match (walk.next_label(), want.next()) {
                        (Ok(None), None) => return true,
                        (Ok(Some(have)), Some(want)) if want.eq_ignore_case(have) => {}
                        _ => return false,
                    }
                }
                false
            })
    }

    /// Writes `name`, compressing its longest suffix already written.
    fn name(&mut self, name: TextName<'_>) -> Result<(), WireError> {
        let count = name.labels().count();
        for i in 0..count {
            if let Some(at) = self.find(name.labels().skip(i)) {
                return self.put(&(0xC000 | at).to_be_bytes());
            }
            let label = name.labels().nth(i).ok_or(WireError::InvalidName)?;
            if self.n_targets < MAX_COMPRESSION_TARGETS
                && let Ok(at) = u16::try_from(self.pos)
                && at < 0x4000
            {
                self.targets[self.n_targets] = at;
                self.n_targets += 1;
            }
            let len = u8::try_from(label.len()).map_err(|_| WireError::InvalidName)?;
            self.put(&[len])?;
            for b in label.bytes() {
                self.put(&[b])?;
            }
        }
        self.put(&[0])
    }
}

/// Builds TXT RDATA (RFC 6763 §6): one length-prefixed `key=value` string
/// per [`TxtWriter::entry`], into a caller buffer.
#[derive(Debug)]
pub struct TxtWriter<'o> {
    out: &'o mut [u8],
    pos: usize,
}

impl<'o> TxtWriter<'o> {
    /// An empty TXT record in `out`.
    pub fn new(out: &'o mut [u8]) -> Self {
        TxtWriter { out, pos: 0 }
    }

    /// Appends `key=value`; an empty `value` writes `key=` (use
    /// [`TxtWriter::flag`] for a key without `=`). The key must be non-empty
    /// printable ASCII without `=` (RFC 6763 §6.4); key, `=` and value
    /// together at most 255 bytes.
    pub fn entry(self, key: &[u8], value: &[u8]) -> Result<Self, WireError> {
        self.push(key, Some(value))
    }

    /// Appends a boolean attribute: `key` alone, no `=`.
    pub fn flag(self, key: &[u8]) -> Result<Self, WireError> {
        self.push(key, None)
    }

    fn push(mut self, key: &[u8], value: Option<&[u8]>) -> Result<Self, WireError> {
        let key_ok =
            !key.is_empty() && key.iter().all(|&b| (0x20..=0x7E).contains(&b) && b != b'=');
        if !key_ok {
            return Err(WireError::BadRdata);
        }
        let len = key.len() + value.map_or(0, |v| v.len() + 1);
        let len = u8::try_from(len).map_err(|_| WireError::BadRdata)?;
        let end = self.pos + 1 + usize::from(len);
        let slot = self
            .out
            .get_mut(self.pos..end)
            .ok_or(WireError::BufferTooSmall)?;
        slot[0] = len;
        slot[1..=key.len()].copy_from_slice(key);
        if let Some(v) = value {
            slot[1 + key.len()] = b'=';
            slot[2 + key.len()..].copy_from_slice(v);
        }
        self.pos = end;
        Ok(self)
    }

    /// The finished RDATA.
    pub fn finish(self) -> Txt<'o> {
        let out: &'o [u8] = self.out;
        Txt(out.get(..self.pos).unwrap_or_default())
    }
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
    /// Validates `data` as TXT RDATA: a sequence of length-prefixed strings
    /// that ends exactly at the end. Empty data is accepted (no entries).
    pub fn new(data: &'p [u8]) -> Result<Self, WireError> {
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

    /// The raw RDATA bytes.
    pub const fn as_bytes(&self) -> &'p [u8] {
        self.0
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
        TYPE_TXT => Rdata::Txt(Txt::new(raw)?),
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
        if records > MAX_RECORDS as u32 {
            return Err(WireError::TooManyRecords);
        }
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

    /// The header flags (see [`FLAG_QR`], [`FLAG_RD`], [`RCODE_MASK`]).
    pub const fn flags(&self) -> u16 {
        self.flags
    }

    /// The response code (0 = no error, 3 = NXDOMAIN).
    pub const fn rcode(&self) -> u8 {
        (self.flags & RCODE_MASK) as u8
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

    fn service() -> ServiceType<'static> {
        http()
    }

    /// The printer of `fixture::printer`, encoded with `MessageWriter`.
    fn encoded_printer(out: &mut [u8], ttl: u32) -> usize {
        let instance = TextName::instance("My.Printer", service()).unwrap();
        let host = TextName::new("printer.local").unwrap();
        let mut txt = [0u8; 32];
        let txt = TxtWriter::new(&mut txt)
            .entry(b"path", b"/")
            .unwrap()
            .flag(b"Color")
            .unwrap()
            .finish();
        let rec = |name, data, cache_flush| RecordOut {
            name,
            ttl,
            cache_flush,
            data,
        };
        MessageWriter::new(out, 7, FLAG_QR | FLAG_AA)
            .unwrap()
            .record(
                Section::Answer,
                &rec(
                    TextName::service(service()),
                    RecordData::Ptr(instance),
                    false,
                ),
            )
            .unwrap()
            .record(
                Section::Additional,
                &rec(
                    instance,
                    RecordData::Srv {
                        priority: 1,
                        weight: 2,
                        port: 8080,
                        target: host,
                    },
                    true,
                ),
            )
            .unwrap()
            .record(
                Section::Additional,
                &rec(instance, RecordData::Txt(txt), true),
            )
            .unwrap()
            .record(
                Section::Additional,
                &rec(host, RecordData::A(Ipv4Addr::new(192, 168, 1, 20)), true),
            )
            .unwrap()
            .record(
                Section::Additional,
                &rec(host, RecordData::Aaaa(Ipv6Addr::LOCALHOST), true),
            )
            .unwrap()
            .finish()
    }

    #[test]
    fn every_record_type_round_trips_with_compression() {
        let mut out = [0u8; 512];
        let n = encoded_printer(&mut out, 120);
        let r = Response::parse(&out[..n]).unwrap().unwrap();
        assert_eq!(
            (r.message().id(), r.message().flags()),
            (7, FLAG_QR | FLAG_AA)
        );
        let recs: Vec<_> = r.records().collect();
        assert_eq!(recs.len(), 5);
        let mut text = [0u8; 64];
        match recs[0].data {
            Rdata::Ptr(p) => {
                assert_eq!(p.write_text(&mut text), Ok("My\\.Printer._http._tcp.local"))
            }
            other => panic!("{other:?}"),
        }
        assert!(!recs[0].cache_flush && recs[1..].iter().all(|r| r.cache_flush));
        match recs[1].data {
            Rdata::Srv(srv) => {
                assert_eq!((srv.priority, srv.weight, srv.port), (1, 2, 8080));
                assert_eq!(srv.target.write_text(&mut text), Ok("printer.local"));
            }
            other => panic!("{other:?}"),
        }
        let Rdata::Txt(txt) = recs[2].data else {
            panic!()
        };
        assert_eq!(
            (txt.get(b"PATH"), txt.get(b"color")),
            (Some(&b"/"[..]), Some(&b""[..]))
        );
        assert_eq!(recs[3].data, Rdata::A(Ipv4Addr::new(192, 168, 1, 20)));
        assert_eq!(recs[4].data, Rdata::Aaaa(Ipv6Addr::LOCALHOST));
        assert!(recs.iter().all(|r| r.ttl == 120 && r.class == CLASS_IN));
        // The DNS-SD join resolves what was written.
        let Some(Instance::Resolved(i)) = r.instances(http()).next() else {
            panic!()
        };
        assert_eq!(i.label(), Ok("My.Printer"));
        assert_eq!(
            i.socket_addr_v4(),
            Some("192.168.1.20:8080".parse().unwrap())
        );
        assert_eq!(i.ipv6, Some(Ipv6Addr::LOCALHOST));
        // Compressed: the same records spelled out take far more room.
        let spelled = 12
            + (18 + 10 + 30)
            + (30 + 10 + 6 + 15)
            + (30 + 10 + 13)
            + (15 + 10 + 4)
            + (15 + 10 + 16);
        assert!(n < spelled - 60, "{n} bytes, uncompressed {spelled}");
        // Goodbye: TTL 0 on the PTR.
        let n = encoded_printer(&mut out, 0);
        let r = Response::parse(&out[..n]).unwrap().unwrap();
        assert!(matches!(
            r.instances(http()).next(),
            Some(Instance::Gone(_))
        ));
    }

    #[test]
    fn writer_refuses_to_truncate_and_keeps_section_order() {
        let mut big = [0u8; 512];
        let n = encoded_printer(&mut big, 120);
        for cut in [0, HEADER_LEN - 1, HEADER_LEN + 3, n - 1] {
            let mut small = std::vec![0u8; cut];
            let r = std::panic::catch_unwind(move || {
                let instance = TextName::instance("x", http()).unwrap();
                MessageWriter::new(&mut small, 0, FLAG_QR)
                    .and_then(|w| {
                        w.record(
                            Section::Answer,
                            &RecordOut {
                                name: TextName::service(http()),
                                ttl: 1,
                                cache_flush: false,
                                data: RecordData::Ptr(instance),
                            },
                        )
                    })
                    .map(|w| w.finish())
            });
            if cut < 40 {
                assert_eq!(r.unwrap(), Err(WireError::BufferTooSmall), "cut {cut}");
            }
        }
        let a = RecordOut {
            name: TextName::ROOT,
            ttl: 1,
            cache_flush: false,
            data: RecordData::A(Ipv4Addr::LOCALHOST),
        };
        let w = MessageWriter::new(&mut big, 0, FLAG_QR)
            .unwrap()
            .record(Section::Additional, &a)
            .unwrap();
        assert_eq!(
            w.record(Section::Answer, &a).unwrap_err(),
            WireError::SectionOrder
        );
        let w = MessageWriter::new(&mut big, 0, 0)
            .unwrap()
            .record(Section::Answer, &a)
            .unwrap();
        assert_eq!(
            w.question(TextName::ROOT, TYPE_A, false).unwrap_err(),
            WireError::SectionOrder
        );
    }

    #[test]
    fn unicast_queries_set_rd_and_any_type() {
        let mut out = [0u8; 64];
        let name = TextName::new("_mw-bus._tcp.example.com.").unwrap();
        for qtype in [TYPE_SRV, TYPE_A, TYPE_AAAA, TYPE_TXT, TYPE_PTR] {
            let n = encode_query_for(0xBEEF, name, qtype, true, &mut out).unwrap();
            let m = Message::parse(&out[..n]).unwrap();
            assert_eq!(
                (m.id(), m.flags() & FLAG_RD, m.is_response(), m.rcode()),
                (0xBEEF, FLAG_RD, false, 0)
            );
            let q = m.questions().next().unwrap();
            assert_eq!(
                (q.qtype, q.class, q.unicast_response),
                (qtype, CLASS_IN, false)
            );
            let mut text = [0u8; 64];
            assert_eq!(q.name.write_text(&mut text), Ok("_mw-bus._tcp.example.com"));
        }
        let n = encode_query_for(1, name, TYPE_SRV, false, &mut out).unwrap();
        assert_eq!(Message::parse(&out[..n]).unwrap().flags(), 0);
        // The mDNS browse shorthand is unchanged on the wire.
        let n = encode_query(0x1234, http(), &mut out).unwrap();
        assert_eq!(
            &out[..n],
            b"\x12\x34\0\0\0\x01\0\0\0\0\0\0\x05_http\x04_tcp\x05local\0\0\x0c\0\x01"
        );
    }

    #[test]
    fn text_names_are_validated_and_unescaped() {
        let mut out = [0u8; 600];
        for (text, wire) in [
            ("", &b"\0"[..]),
            (".", b"\0"),
            ("a.b", b"\x01a\x01b\0"),
            ("a\\.b.c.", b"\x03a.b\x01c\0"),
            ("a\\\\.c", b"\x02a\\\x01c\0"),
            ("a\\.", b"\x02a.\0"),
        ] {
            let name = TextName::new(text).unwrap();
            let n = encode_query_for(0, name, TYPE_A, false, &mut out).unwrap();
            assert_eq!(&out[HEADER_LEN..n - 4], wire, "{text}");
        }
        for bad in [
            "a..b",
            ".a",
            "a\\",
            "a\\\\\\",
            &"x".repeat(64),
            &std::vec!["a".repeat(63); 4].join("."),
        ] {
            assert_eq!(TextName::new(bad), Err(WireError::InvalidName), "{bad}");
        }
        assert_eq!(TextName::instance("", http()), Err(WireError::InvalidName));
        assert_eq!(
            TextName::instance(&"x".repeat(64), http()),
            Err(WireError::InvalidName)
        );
    }

    #[test]
    fn txt_writer_builds_valid_rdata() {
        let mut buf = [0u8; 16];
        let txt = TxtWriter::new(&mut buf)
            .entry(b"k", b"v")
            .unwrap()
            .flag(b"f")
            .unwrap()
            .finish();
        assert_eq!(txt.as_bytes(), b"\x03k=v\x01f");
        assert_eq!(Txt::new(txt.as_bytes()), Ok(txt));
        assert_eq!(txt.entries().count(), 2);
        let mut small = [0u8; 3];
        assert_eq!(
            TxtWriter::new(&mut small).entry(b"k", b"v").unwrap_err(),
            WireError::BufferTooSmall
        );
        for bad in [&b""[..], b"a=b", b"\x01"] {
            assert_eq!(
                TxtWriter::new(&mut buf).flag(bad).unwrap_err(),
                WireError::BadRdata
            );
        }
        assert_eq!(Txt::new(b"\x05ab"), Err(WireError::BadRdata));
        assert_eq!(Txt::new(b"").map(|t| t.entries().count()), Ok(0));
    }

    #[test]
    fn records_over_the_cap_are_refused_before_parsing() {
        // A response declaring MAX_RECORDS + 1 PTR answers, every one present.
        let ptrs = |count: usize| {
            let mut p = std::vec![0, 0, 0x84, 0, 0, 0];
            p.extend_from_slice(&(count as u16).to_be_bytes());
            p.extend_from_slice(&[0, 0, 0, 0]);
            let service = name(&mut p, "_http._tcp.local");
            for i in 0..count {
                if i > 0 {
                    pointer(&mut p, service);
                }
                rr_head(&mut p, TYPE_PTR, CLASS_IN, 120);
                let s = begin_rdata(&mut p);
                name_ptr(&mut p, &["x"], service);
                end_rdata(&mut p, s);
            }
            p
        };
        let ok = ptrs(MAX_RECORDS);
        let r = Response::parse(&ok).unwrap().unwrap();
        assert_eq!(r.instances(http()).count(), MAX_RECORDS);
        assert_eq!(
            Message::parse(&ptrs(MAX_RECORDS + 1)),
            Err(WireError::TooManyRecords)
        );
        // A header claiming 3 × 65535 records fails on the count alone.
        let mut huge = std::vec![0, 0, 0x84, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        huge.resize(MAX_MESSAGE_LEN, 0);
        assert_eq!(Message::parse(&huge), Err(WireError::TooManyRecords));
        // The writer refuses to build what the parser would refuse.
        let mut out = std::vec![0u8; MAX_MESSAGE_LEN];
        let a = RecordOut {
            name: TextName::ROOT,
            ttl: 1,
            cache_flush: false,
            data: RecordData::A(Ipv4Addr::LOCALHOST),
        };
        let mut w = MessageWriter::new(&mut out, 0, FLAG_QR).unwrap();
        for _ in 0..MAX_RECORDS {
            w = w.record(Section::Answer, &a).unwrap();
        }
        assert_eq!(
            w.record(Section::Answer, &a).unwrap_err(),
            WireError::TooManyRecords
        );
    }
}
