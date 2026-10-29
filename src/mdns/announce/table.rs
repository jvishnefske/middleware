//! The pure core of [`MdnsAnnouncer`](super::MdnsAnnouncer): a fixed-size
//! table of announced instances and the RFC 6762 decisions about them.
//!
//! [`AnnounceTable`] holds at most [`MAX_INSTANCES`] instances. Every
//! transition takes the table by value with the current time (milliseconds
//! on any monotonic clock) and returns the new table with what to send,
//! encoded into a caller buffer; nothing here does I/O, reads a clock or
//! allocates.
//!
//! # Life of an instance
//!
//! 1. [`register`](AnnounceTable::register): the service is validated
//!    (DNS-SD type of its kind, instance label, endpoint, TXT) and probes
//!    are due now. Registering a kind and instance that is already announced
//!    replaces its data and announces again without probing (RFC 6762
//!    §8.4); one still probing keeps probing with the new data.
//! 2. Probing (RFC 6762 §8.1): [`PROBE_COUNT`] probes, [`PROBE_INTERVAL_MS`]
//!    apart, each a query for the instance name (type ANY, unicast-response
//!    bit set) with the proposed SRV and TXT in the authority section. The
//!    first probe is sent at once (no random 0–250 ms delay).
//! 3. A *response* received while probing that carries a record owned by
//!    the instance name with data other than ours (another SRV port or
//!    target, another TXT, any other type) is a conflict: the instance is
//!    renamed `name (2)`, then `name (3)` …, up to `name (`
//!    [`MAX_RENAMES`]` + 1)`, and probing restarts at once. After that the
//!    instance is [`InstanceState::Conflicted`]. Records identical to ours
//!    (our own probe looped back) are not conflicts. Probe queries from
//!    other hosts for the same name are ignored: there is no simultaneous
//!    probe tie-break (§8.2), and conflicts are not detected once probing
//!    has finished (§9).
//! 4. [`PROBE_INTERVAL_MS`] after the last probe, the instance is announced
//!    (§8.3): an unsolicited response with its PTR, SRV, TXT and address
//!    records in the answer section, the unique ones with the cache-flush
//!    bit. [`ANNOUNCE_COUNT`] announcements go out,
//!    [`ANNOUNCE_INTERVAL_MS`] apart; there is no periodic re-announcement
//!    (queriers refresh by querying at 80 % of the TTL).
//! 5. [`withdraw`](AnnounceTable::withdraw) removes the instance and, if it
//!    was announced, encodes a goodbye: its PTR, SRV and TXT with TTL 0.
//!
//! [`poll`](AnnounceTable::poll) encodes whatever probe or announcement is
//! due, one message per call, and otherwise says when the next one is.
//!
//! # Answering queries
//!
//! [`receive`](AnnounceTable::receive) answers queries for instances whose
//! first announcement has gone out:
//!
//! * PTR (or ANY) for the service type: the PTR, with SRV, TXT and address
//!   records as additional records (RFC 6763 §12). Known-answer suppression
//!   (RFC 6762 §7.1) is done for PTRs: a PTR the query already lists with
//!   at least half our TTL is not repeated.
//! * SRV, TXT (or ANY) for the instance name; A, AAAA (or ANY) for the SRV
//!   target host.
//!
//! The first [`MAX_QUESTIONS`] questions of a query are read. A query from a
//! port other than 5353 gets a legacy unicast reply (§6.7) to its source:
//! the query id and questions echoed, TTLs capped at
//! [`LEGACY_UNICAST_TTL`] seconds, no cache-flush bits. A query with the
//! unicast-response (QU) bit on any question is answered by unicast to its
//! source; otherwise the answer is multicast. Answers are sent at once (no
//! 20–120 ms random delay). The `_services._dns-sd._udp` meta-query is not
//! answered.
//!
//! # Endpoints
//!
//! An [`Endpoint::Socket`] is announced as an SRV record targeting the
//! table's host name with that port, and an A or AAAA record of the host
//! name with that address (an unspecified address is refused). An
//! [`Endpoint::Host`] is an SRV record targeting that name, without an
//! address record. [`Endpoint::Locator`] and [`Endpoint::Local`] have no
//! DNS-SD form and are refused. Field bounds are those of
//! [`ServiceRecord`] (TXT at most 192 bytes); every record's TTL is the
//! service's `ttl_s`.

use core::net::IpAddr;

use heapless::{String, Vec};

use crate::discovery::directory::{MAX_HOST_LEN, ServiceRecord};
use crate::discovery::record::dns_sd_type_in;
use crate::discovery::{Endpoint, RecordError, Service, ServiceKind};
use crate::dns::wire::{
    CLASS_IN, FLAG_AA, FLAG_QR, MAX_LABEL_LEN, MAX_NAME_LEN, Message, MessageWriter, Name, Rdata,
    RecordData, RecordOut, Section, ServiceType, TYPE_A, TYPE_AAAA, TYPE_PTR, TYPE_SRV, TYPE_TXT,
    TextName, Txt, WireError,
};
use crate::mdns::{DOMAIN, MAX_TYPE_TEXT, MDNS_PORT};

/// Instances an [`AnnounceTable`] holds.
pub const MAX_INSTANCES: usize = 8;
/// Probes sent before announcing (RFC 6762 §8.1).
pub const PROBE_COUNT: u8 = 3;
/// Milliseconds between probes, and from the last probe to the first
/// announcement.
pub const PROBE_INTERVAL_MS: u64 = 250;
/// Unsolicited announcements sent (RFC 6762 §8.3: at least two).
pub const ANNOUNCE_COUNT: u8 = 2;
/// Milliseconds between announcements.
pub const ANNOUNCE_INTERVAL_MS: u64 = 1000;
/// Renames tried after conflicts before giving up.
pub const MAX_RENAMES: u8 = 8;
/// Questions of one query that are read.
pub const MAX_QUESTIONS: usize = 8;
/// TTL cap of a legacy unicast reply (RFC 6762 §6.7).
pub const LEGACY_UNICAST_TTL: u32 = 10;

/// Query type and class ANY.
const ANY: u16 = 255;
/// Records one reply plans: every record of every instance.
const MAX_PLANNED: usize = MAX_INSTANCES * 4;

/// Why an instance could not be announced or an operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AnnounceError {
    /// All [`MAX_INSTANCES`] slots are taken.
    Full,
    /// The endpoint has no DNS-SD form (a locator, `Local`, or an
    /// unspecified address).
    Endpoint,
    /// The kind has no DNS-SD type, or a field is over its bound.
    Record(RecordError),
    /// A name, label or TXT record is invalid, or a message did not fit.
    Wire(WireError),
    /// Every rename after a conflict conflicted too.
    Conflict,
}

impl From<WireError> for AnnounceError {
    fn from(e: WireError) -> Self {
        AnnounceError::Wire(e)
    }
}

impl From<RecordError> for AnnounceError {
    fn from(e: RecordError) -> Self {
        AnnounceError::Record(e)
    }
}

/// Where an instance is in its life; see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// `sent` probes went out; the next step is due at `due_ms`.
    Probing { sent: u8, due_ms: u64 },
    /// `sent` announcements went out; the next is due at `due_ms`. `sent`
    /// is 0 only for a re-registered announced instance, which keeps
    /// answering queries.
    Announcing { sent: u8, due_ms: u64 },
    /// Announced; answers queries.
    Live,
    /// Renames exhausted.
    Conflicted,
}

/// What [`AnnounceTable::state`] reports about an instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceState {
    /// Probing, or re-registered and not yet announced again.
    Pending,
    /// The first announcement of the current data has been encoded.
    Announced,
    /// Every rename conflicted ([`AnnounceError::Conflict`]).
    Conflicted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Slot {
    /// The service as registered (its instance is the requested label).
    record: ServiceRecord,
    /// The label in use: the requested one, or a rename.
    label: String<MAX_LABEL_LEN>,
    renames: u8,
    phase: Phase,
}

impl Slot {
    fn announced(&self) -> bool {
        matches!(self.phase, Phase::Live | Phase::Announcing { .. })
    }

    fn due_ms(&self) -> Option<u64> {
        match self.phase {
            Phase::Probing { due_ms, .. } | Phase::Announcing { due_ms, .. } => Some(due_ms),
            Phase::Live | Phase::Conflicted => None,
        }
    }
}

/// The names and data of one instance, borrowing its slot.
struct Names<'a> {
    service: ServiceType<'a>,
    instance: TextName<'a>,
    label: &'a str,
    target: TextName<'a>,
    target_text: &'a str,
    port: u16,
    addr: Option<IpAddr>,
    txt: Txt<'a>,
    ttl: u32,
}

fn names<'a>(
    slot: &'a Slot,
    host: &'a str,
    ty: &'a mut [u8; MAX_TYPE_TEXT],
) -> Result<Names<'a>, AnnounceError> {
    let service = slot.record.service();
    let ty = dns_sd_type_in(service.kind, DOMAIN, ty)?;
    let (target_text, port, addr) = match service.endpoint {
        Endpoint::Socket(a) if a.ip().is_unspecified() => return Err(AnnounceError::Endpoint),
        Endpoint::Socket(a) => (host, a.port(), Some(a.ip())),
        Endpoint::Host { name, port } => (name, port, None),
        Endpoint::Locator(_) | Endpoint::Local => return Err(AnnounceError::Endpoint),
    };
    Ok(Names {
        service: ty,
        instance: TextName::instance(&slot.label, ty)?,
        label: &slot.label,
        target: TextName::new(target_text)?,
        target_text,
        port,
        addr,
        txt: Txt::new(service.txt)?,
        ttl: service.ttl_s,
    })
}

/// One record of an instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rec {
    Ptr,
    Srv,
    Txt,
    Addr,
}

fn record_out<'a>(n: &Names<'a>, rec: Rec, ttl: u32, flush: bool) -> Option<RecordOut<'a>> {
    let (name, data) = match rec {
        Rec::Ptr => (TextName::service(n.service), RecordData::Ptr(n.instance)),
        Rec::Srv => (
            n.instance,
            RecordData::Srv {
                priority: 0,
                weight: 0,
                port: n.port,
                target: n.target,
            },
        ),
        Rec::Txt => (n.instance, RecordData::Txt(n.txt)),
        Rec::Addr => (
            n.target,
            match n.addr? {
                IpAddr::V4(a) => RecordData::A(a),
                IpAddr::V6(a) => RecordData::Aaaa(a),
            },
        ),
    };
    Some(RecordOut {
        name,
        ttl,
        // PTRs are shared records; the rest are unique (RFC 6762 §10.2).
        cache_flush: flush && rec != Rec::Ptr,
        data,
    })
}

/// True if `a` and `b` are the same labels, ASCII case ignored.
fn labels_eq<'x, 'y>(
    mut a: impl Iterator<Item = &'x [u8]>,
    mut b: impl Iterator<Item = &'y [u8]>,
) -> bool {
    // Bounded: a name has at most 128 labels.
    for _ in 0..=MAX_NAME_LEN / 2 {
        match (a.next(), b.next()) {
            (None, None) => return true,
            (Some(x), Some(y)) if x.eq_ignore_ascii_case(y) => {}
            _ => return false,
        }
    }
    false
}

fn is_instance(name: &Name<'_>, n: &Names<'_>) -> bool {
    let mut labels = name.labels();
    labels
        .next()
        .is_some_and(|l| l.eq_ignore_ascii_case(n.label.as_bytes()))
        && labels_eq(labels, n.service.labels())
}

fn is_host(name: &Name<'_>, host: &str) -> bool {
    labels_eq(name.labels(), host.split('.').map(str::as_bytes))
}

/// `base (n)`, the base cut at a character boundary to fit a label.
fn renamed(base: &str, n: u8) -> String<MAX_LABEL_LEN> {
    let mut suffix: String<8> = String::new();
    // " (" + at most 3 digits + ")" fits 8 bytes.
    let _ = core::fmt::write(&mut suffix, format_args!(" ({n})"));
    let room = MAX_LABEL_LEN - suffix.len();
    let cut = (0..=room.min(base.len()))
        .rev()
        .find(|&i| base.is_char_boundary(i))
        .unwrap_or(0);
    let mut out = String::new();
    // Both fit: cut + suffix ≤ MAX_LABEL_LEN.
    let _ = out.push_str(base.get(..cut).unwrap_or_default());
    let _ = out.push_str(&suffix);
    out
}

/// What [`AnnounceTable::poll`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Send the first `len` bytes of the buffer to the multicast
    /// destination now, then poll again.
    Send(usize),
    /// Nothing is due; poll again at this time (none: nothing pending).
    Idle(Option<u64>),
}

/// Where a reply to a received query goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Destination {
    /// The multicast destination.
    Multicast,
    /// The query's source address.
    Source,
}

/// A reply [`AnnounceTable::receive`] encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reply {
    /// Bytes of the buffer to send.
    pub len: usize,
    /// Where to send them.
    pub to: Destination,
}

/// The announced instances; see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnnounceTable {
    host: String<MAX_HOST_LEN>,
    slots: Vec<Slot, MAX_INSTANCES>,
}

impl AnnounceTable {
    /// An empty table whose [`Endpoint::Socket`] services are served by the
    /// host `host` (dotted, e.g. `cam-1.local`; at most 64 bytes of
    /// letters, digits and hyphens).
    pub fn new(host: &str) -> Result<Self, AnnounceError> {
        let ok = host.split('.').all(|l| {
            !l.is_empty() && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        });
        if !ok {
            return Err(WireError::InvalidName.into());
        }
        TextName::new(host)?;
        Ok(AnnounceTable {
            host: String::try_from(host).map_err(|_| RecordError::BufferTooSmall)?,
            slots: Vec::new(),
        })
    }

    /// The host name.
    pub fn host(&self) -> &str {
        &self.host
    }

    fn position(&self, kind: ServiceKind<'_>, instance: &str) -> Option<usize> {
        self.slots.iter().position(|s| s.record.is(kind, instance))
    }

    /// Registers `service`; see the module docs.
    pub fn register(
        mut self,
        service: &Service<'_>,
        now_ms: u64,
    ) -> (Self, Result<(), AnnounceError>) {
        let checked = (|| {
            let record = ServiceRecord::new(service).ok_or(RecordError::BufferTooSmall)?;
            let label = String::try_from(service.instance).map_err(|_| RecordError::BufferTooSmall)?;
            let slot = Slot {
                record,
                label,
                renames: 0,
                phase: Phase::Probing {
                    sent: 0,
                    due_ms: now_ms,
                },
            };
            let mut ty = [0u8; MAX_TYPE_TEXT];
            names(&slot, &self.host, &mut ty)?;
            Ok::<_, AnnounceError>(slot)
        })();
        let new = match checked {
            Ok(s) => s,
            Err(e) => return (self, Err(e)),
        };
        let at = self.position(service.kind, service.instance);
        match at.and_then(|i| self.slots.get_mut(i)) {
            Some(old) => {
                let phase = match old.phase {
                    Phase::Live | Phase::Announcing { .. } => Phase::Announcing {
                        sent: 0,
                        due_ms: now_ms,
                    },
                    Phase::Probing { .. } | Phase::Conflicted => new.phase,
                };
                let label = if old.phase == Phase::Conflicted {
                    new.label
                } else {
                    old.label.clone()
                };
                *old = Slot {
                    label,
                    phase,
                    renames: if old.phase == Phase::Conflicted { 0 } else { old.renames },
                    ..new
                };
                (self, Ok(()))
            }
            None => match self.slots.push(new) {
                Ok(()) => (self, Ok(())),
                Err(_) => (self, Err(AnnounceError::Full)),
            },
        }
    }

    /// The state of the instance registered as `kind` / `instance`.
    pub fn state(&self, kind: ServiceKind<'_>, instance: &str) -> Option<InstanceState> {
        let slot = self.slots.get(self.position(kind, instance)?)?;
        Some(match slot.phase {
            Phase::Probing { .. } | Phase::Announcing { sent: 0, .. } => InstanceState::Pending,
            Phase::Announcing { .. } | Phase::Live => InstanceState::Announced,
            Phase::Conflicted => InstanceState::Conflicted,
        })
    }

    /// The label in use for the instance registered as `instance` (a
    /// rename after a conflict, or `instance` itself).
    pub fn label(&self, kind: ServiceKind<'_>, instance: &str) -> Option<&str> {
        Some(&self.slots.get(self.position(kind, instance)?)?.label)
    }

    /// Instances registered.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// True if none is registered.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Removes the instance; if it was announced, encodes its goodbye into
    /// `out` and returns the length to multicast. An unknown instance is
    /// `Ok(None)`.
    pub fn withdraw(
        mut self,
        kind: ServiceKind<'_>,
        instance: &str,
        out: &mut [u8],
    ) -> (Self, Result<Option<usize>, AnnounceError>) {
        let Some(i) = self.position(kind, instance) else {
            return (self, Ok(None));
        };
        let slot = self.slots.swap_remove(i);
        if !slot.announced() {
            return (self, Ok(None));
        }
        let encoded = (|| {
            let mut ty = [0u8; MAX_TYPE_TEXT];
            let n = names(&slot, &self.host, &mut ty)?;
            let mut w = MessageWriter::new(out, 0, FLAG_QR | FLAG_AA)?;
            for rec in [Rec::Ptr, Rec::Srv, Rec::Txt] {
                if let Some(r) = record_out(&n, rec, 0, false) {
                    w = w.record(Section::Answer, &r)?;
                }
            }
            Ok::<_, AnnounceError>(w.finish())
        })();
        (self, encoded.map(Some))
    }

    /// Encodes the probe or announcement due at `now_ms`, if any; see
    /// [`Step`]. A slot whose message cannot be encoded is removed and its
    /// error returned.
    pub fn poll(mut self, now_ms: u64, out: &mut [u8]) -> (Self, Result<Step, AnnounceError>) {
        let due = self
            .slots
            .iter()
            .position(|s| s.due_ms().is_some_and(|d| d <= now_ms));
        let Some(i) = due else {
            let next = self.slots.iter().filter_map(Slot::due_ms).min();
            return (self, Ok(Step::Idle(next)));
        };
        let host = self.host.clone();
        let Some(slot) = self.slots.get_mut(i) else {
            return (self, Ok(Step::Idle(None)));
        };
        let (msg, phase) = match slot.phase {
            Phase::Probing { sent, .. } if sent < PROBE_COUNT => (
                probe(slot, &host, out),
                Phase::Probing {
                    sent: sent + 1,
                    due_ms: now_ms + PROBE_INTERVAL_MS,
                },
            ),
            Phase::Probing { .. } => (announcement(slot, &host, out), after_announcement(0, now_ms)),
            Phase::Announcing { sent, .. } => {
                (announcement(slot, &host, out), after_announcement(sent, now_ms))
            }
            Phase::Live | Phase::Conflicted => return (self, Ok(Step::Idle(None))),
        };
        match msg {
            Ok(len) => {
                slot.phase = phase;
                (self, Ok(Step::Send(len)))
            }
            Err(e) => {
                self.slots.swap_remove(i);
                (self, Err(e))
            }
        }
    }

    /// Handles a received datagram from a source whose port is
    /// `source_port`: a response may conflict with a probing instance, a
    /// query may be answered (encoded into `out`). Malformed datagrams are
    /// ignored (`Ok(None)`).
    pub fn receive(
        mut self,
        packet: &[u8],
        source_port: u16,
        now_ms: u64,
        out: &mut [u8],
    ) -> (Self, Result<Option<Reply>, AnnounceError>) {
        let Ok(msg) = Message::parse(packet) else {
            return (self, Ok(None));
        };
        if msg.is_response() {
            self.conflicts(&msg, now_ms);
            return (self, Ok(None));
        }
        let reply = self.answer(&msg, source_port, out);
        (self, reply)
    }

    /// Renames every probing instance `msg` conflicts with.
    fn conflicts(&mut self, msg: &Message<'_>, now_ms: u64) {
        let host = self.host.clone();
        for slot in self.slots.iter_mut() {
            if !matches!(slot.phase, Phase::Probing { .. }) {
                continue;
            }
            let mut ty = [0u8; MAX_TYPE_TEXT];
            let Ok(n) = names(slot, &host, &mut ty) else {
                continue;
            };
            let conflict = msg.records().any(|r| {
                is_instance(&r.name, &n)
                    && match r.data {
                        Rdata::Srv(s) => s.port != n.port || !is_host(&s.target, n.target_text),
                        Rdata::Txt(t) => !same_txt(t, n.txt),
                        _ => true,
                    }
            });
            if !conflict {
                continue;
            }
            slot.renames = slot.renames.saturating_add(1);
            if slot.renames > MAX_RENAMES {
                slot.phase = Phase::Conflicted;
            } else {
                slot.label = renamed(slot.record.service().instance, slot.renames + 1);
                slot.phase = Phase::Probing {
                    sent: 0,
                    due_ms: now_ms,
                };
            }
        }
    }

    /// Encodes the reply to query `msg`, if it asks about an announced
    /// instance.
    fn answer(
        &self,
        msg: &Message<'_>,
        source_port: u16,
        out: &mut [u8],
    ) -> Result<Option<Reply>, AnnounceError> {
        let legacy = source_port != MDNS_PORT;
        let mut answers: Vec<(usize, Rec), MAX_PLANNED> = Vec::new();
        let mut additional: Vec<(usize, Rec), MAX_PLANNED> = Vec::new();
        let mut unicast = legacy;
        for q in msg.questions().take(MAX_QUESTIONS) {
            if q.class != CLASS_IN && q.class != ANY {
                continue;
            }
            let before = answers.len();
            for (i, slot) in self.slots.iter().enumerate() {
                if !slot.announced() {
                    continue;
                }
                let mut ty = [0u8; MAX_TYPE_TEXT];
                let Ok(n) = names(slot, &self.host, &mut ty) else {
                    continue;
                };
                let asks = |t: u16| q.qtype == t || q.qtype == ANY;
                if asks(TYPE_PTR) && q.name.is_service(&n.service) {
                    if !known_answer(msg, &n) {
                        plan(&mut answers, i, Rec::Ptr);
                        for r in [Rec::Srv, Rec::Txt, Rec::Addr] {
                            plan(&mut additional, i, r);
                        }
                    }
                } else if is_instance(&q.name, &n) {
                    if asks(TYPE_SRV) {
                        plan(&mut answers, i, Rec::Srv);
                        plan(&mut additional, i, Rec::Addr);
                    }
                    if asks(TYPE_TXT) {
                        plan(&mut answers, i, Rec::Txt);
                    }
                } else if is_host(&q.name, n.target_text) {
                    let family = match n.addr {
                        Some(IpAddr::V4(_)) => TYPE_A,
                        Some(IpAddr::V6(_)) => TYPE_AAAA,
                        None => continue,
                    };
                    if asks(family) {
                        plan(&mut answers, i, Rec::Addr);
                    }
                }
            }
            unicast |= q.unicast_response && answers.len() > before;
        }
        if answers.is_empty() {
            return Ok(None);
        }
        let (id, cap) = if legacy {
            (msg.id(), LEGACY_UNICAST_TTL)
        } else {
            (0, u32::MAX)
        };
        let mut w = MessageWriter::new(out, id, FLAG_QR | FLAG_AA)?;
        if legacy {
            for q in msg.questions().take(MAX_QUESTIONS) {
                let mut text = [0u8; MAX_NAME_LEN * 2];
                let name = TextName::new(q.name.write_text(&mut text)?)?;
                w = w.question(name, q.qtype, false)?;
            }
        }
        for (section, list) in [(Section::Answer, &answers), (Section::Additional, &additional)] {
            for &(i, rec) in list.iter() {
                if section == Section::Additional && answers.contains(&(i, rec)) {
                    continue;
                }
                if rec == Rec::Addr && self.duplicate_addr(list, i) {
                    continue;
                }
                let Some(slot) = self.slots.get(i) else {
                    continue;
                };
                let mut ty = [0u8; MAX_TYPE_TEXT];
                let n = names(slot, &self.host, &mut ty)?;
                if let Some(r) = record_out(&n, rec, n.ttl.min(cap), !legacy) {
                    w = w.record(section, &r)?;
                }
            }
        }
        let to = if unicast {
            Destination::Source
        } else {
            Destination::Multicast
        };
        Ok(Some(Reply { len: w.finish(), to }))
    }

    /// True if an earlier slot in `list` has an address record for the same
    /// host and address as slot `i`.
    fn duplicate_addr(&self, list: &[(usize, Rec)], i: usize) -> bool {
        let key = |j: usize| {
            let s = self.slots.get(j)?.record.service();
            match s.endpoint {
                Endpoint::Socket(a) => Some(a.ip()),
                _ => None,
            }
        };
        let mine = key(i);
        list.iter()
            .take_while(|&&(j, _)| j != i)
            .any(|&(j, r)| r == Rec::Addr && key(j) == mine)
    }
}

fn plan(list: &mut Vec<(usize, Rec), MAX_PLANNED>, i: usize, rec: Rec) {
    if !list.contains(&(i, rec)) {
        // Capacity covers every record of every slot.
        let _ = list.push((i, rec));
    }
}

/// True if query `msg` lists our PTR with at least half its TTL.
fn known_answer(msg: &Message<'_>, n: &Names<'_>) -> bool {
    msg.records().any(|r| {
        r.name.is_service(&n.service)
            && r.ttl >= n.ttl / 2
            && matches!(r.data, Rdata::Ptr(target) if is_instance(&target, n))
    })
}

/// TXT data compared as sent: empty data goes out as one empty string.
fn same_txt(a: Txt<'_>, b: Txt<'_>) -> bool {
    fn norm<'t>(t: &Txt<'t>) -> &'t [u8] {
        match t.as_bytes() {
            [] => &[0u8],
            bytes => bytes,
        }
    }
    norm(&a) == norm(&b)
}

fn after_announcement(sent_before: u8, now_ms: u64) -> Phase {
    let sent = sent_before + 1;
    if sent >= ANNOUNCE_COUNT {
        Phase::Live
    } else {
        Phase::Announcing {
            sent,
            due_ms: now_ms + ANNOUNCE_INTERVAL_MS,
        }
    }
}

fn probe(slot: &Slot, host: &str, out: &mut [u8]) -> Result<usize, AnnounceError> {
    let mut ty = [0u8; MAX_TYPE_TEXT];
    let n = names(slot, host, &mut ty)?;
    let mut w = MessageWriter::new(out, 0, 0)?.question(n.instance, ANY, true)?;
    for rec in [Rec::Srv, Rec::Txt] {
        if let Some(r) = record_out(&n, rec, n.ttl, false) {
            w = w.record(Section::Authority, &r)?;
        }
    }
    Ok(w.finish())
}

fn announcement(slot: &Slot, host: &str, out: &mut [u8]) -> Result<usize, AnnounceError> {
    let mut ty = [0u8; MAX_TYPE_TEXT];
    let n = names(slot, host, &mut ty)?;
    let mut w = MessageWriter::new(out, 0, FLAG_QR | FLAG_AA)?;
    for rec in [Rec::Ptr, Rec::Srv, Rec::Txt, Rec::Addr] {
        if let Some(r) = record_out(&n, rec, n.ttl, true) {
            w = w.record(Section::Answer, &r)?;
        }
    }
    Ok(w.finish())
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::transport::SenderId;
    use crate::dns::wire::{Instance, Response, encode_query, encode_query_for};
    use std::vec::Vec as StdVec;

    const KIND: ServiceKind<'static> = ServiceKind::from_static("conf/svc");

    fn ty() -> ServiceType<'static> {
        ServiceType::new("_conf-svc._tcp.local").unwrap()
    }

    fn service(instance: &str) -> Service<'_> {
        Service {
            kind: KIND,
            instance,
            node: Some(SenderId([1; 8])),
            endpoint: Endpoint::Socket("10.0.0.7:7447".parse().unwrap()),
            txt: b"\x05ver=5",
            ttl_s: 120,
        }
    }

    fn table() -> AnnounceTable {
        AnnounceTable::new("unit.local").unwrap()
    }

    /// Polls at `now`, expecting a message, and returns it.
    fn send(t: AnnounceTable, now: u64) -> (AnnounceTable, StdVec<u8>) {
        let mut out = [0u8; 2048];
        let (t, step) = t.poll(now, &mut out);
        let Ok(Step::Send(len)) = step else {
            panic!("expected a send at {now}, got {step:?}")
        };
        (t, out[..len].to_vec())
    }

    fn idle(t: AnnounceTable, now: u64) -> (AnnounceTable, Option<u64>) {
        let mut out = [0u8; 2048];
        let (t, step) = t.poll(now, &mut out);
        let Ok(Step::Idle(next)) = step else {
            panic!("expected idle at {now}, got {step:?}")
        };
        (t, next)
    }

    /// A table with `unit-1` announced twice (live) at t = 0..1750.
    fn live() -> AnnounceTable {
        let (mut t, r) = table().register(&service("unit-1"), 0);
        r.unwrap();
        for now in [0, 250, 500, 750, 1750] {
            t = send(t, now).0;
        }
        t
    }

    fn reply(t: &AnnounceTable, query: &[u8], port: u16) -> Option<(Reply, StdVec<u8>)> {
        let mut out = [0u8; 4096];
        let (_, r) = t.clone().receive(query, port, 5_000, &mut out);
        r.unwrap().map(|r| (r, out[..r.len].to_vec()))
    }

    #[test]
    fn probes_three_times_then_announces_twice() {
        let (t, r) = table().register(&service("unit-1"), 0);
        r.unwrap();
        assert_eq!(t.state(KIND, "unit-1"), Some(InstanceState::Pending));
        let (t, probe) = send(t, 0);
        let m = Message::parse(&probe).unwrap();
        assert!(!m.is_response());
        let q = m.questions().next().unwrap();
        assert_eq!((q.qtype, q.unicast_response), (ANY, true));
        let mut text = [0u8; 255];
        assert_eq!(q.name.write_text(&mut text), Ok("unit-1._conf-svc._tcp.local"));
        // The proposed SRV and TXT ride in the authority section.
        assert_eq!(m.records().count(), 2);
        let (t, next) = idle(t, 0);
        assert_eq!(next, Some(250));
        let (t, _) = send(t, 250);
        let (t, _) = send(t, 500);
        let (t, next) = idle(t, 749);
        assert_eq!(next, Some(750));
        assert_eq!(t.state(KIND, "unit-1"), Some(InstanceState::Pending));
        let (t, ann) = send(t, 750);
        assert_eq!(t.state(KIND, "unit-1"), Some(InstanceState::Announced));
        let r = Response::parse(&ann).unwrap().unwrap();
        let Some(Instance::Resolved(i)) = r.instances(ty()).next() else {
            panic!()
        };
        assert_eq!(i.label(), Ok("unit-1"));
        assert_eq!(i.socket_addr_v4(), Some("10.0.0.7:7447".parse().unwrap()));
        assert_eq!(i.txt.unwrap().get(b"ver"), Some(&b"5"[..]));
        assert_eq!(i.ttl, 120);
        // Unique records carry the cache-flush bit, the shared PTR not.
        assert!(r.records().all(|rec| rec.cache_flush != matches!(rec.data, Rdata::Ptr(_))));
        let (t, next) = idle(t, 750);
        assert_eq!(next, Some(1750));
        let (t, again) = send(t, 1750);
        assert_eq!(again, ann);
        let (_, next) = idle(t, 100_000);
        assert_eq!(next, None);
    }

    fn conflicting(label: &str, port: u16) -> StdVec<u8> {
        let mut out = [0u8; 512];
        let srv = RecordOut {
            name: TextName::instance(label, ty()).unwrap(),
            ttl: 120,
            cache_flush: true,
            data: RecordData::Srv {
                priority: 0,
                weight: 0,
                port,
                target: TextName::new("unit.local").unwrap(),
            },
        };
        let len = MessageWriter::new(&mut out, 0, FLAG_QR | FLAG_AA)
            .unwrap()
            .record(Section::Answer, &srv)
            .unwrap()
            .finish();
        out[..len].to_vec()
    }

    #[test]
    fn a_conflict_while_probing_renames_and_probes_again() {
        let (t, _) = table().register(&service("unit-1"), 0);
        let (t, _) = send(t, 0);
        let mut out = [0u8; 2048];
        // Our own records looped back are not a conflict.
        let (t, r) = t.receive(&conflicting("unit-1", 7447), MDNS_PORT, 10, &mut out);
        assert_eq!(r, Ok(None));
        assert_eq!(t.label(KIND, "unit-1"), Some("unit-1"));
        // Another port is.
        let (t, _) = t.receive(&conflicting("unit-1", 9999), MDNS_PORT, 10, &mut out);
        assert_eq!(t.label(KIND, "unit-1"), Some("unit-1 (2)"));
        assert_eq!(t.state(KIND, "unit-1"), Some(InstanceState::Pending));
        let (t, probe) = send(t, 10);
        let m = Message::parse(&probe).unwrap();
        let mut text = [0u8; 255];
        assert_eq!(
            m.questions().next().unwrap().name.write_text(&mut text),
            Ok("unit-1 (2)._conf-svc._tcp.local")
        );
        // Every rename conflicts: the instance gives up.
        let mut t = t;
        for n in 2..=MAX_RENAMES + 1 {
            let label = t.label(KIND, "unit-1").unwrap();
            assert_eq!(label, std::format!("unit-1 ({n})"));
            let msg = conflicting(label, 1);
            t = t.receive(&msg, MDNS_PORT, 20, &mut out).0;
        }
        assert_eq!(t.state(KIND, "unit-1"), Some(InstanceState::Conflicted));
        assert_eq!(idle(t, 10_000).1, None);
    }

    #[test]
    fn renames_fit_a_label() {
        let long = "é".repeat(31); // 62 bytes
        let r = renamed(&long, 2);
        assert!(r.len() <= MAX_LABEL_LEN && r.ends_with(" (2)"));
        assert_eq!(renamed("a", 10), "a (10)");
    }

    fn ptr_query(id: u16) -> StdVec<u8> {
        let mut q = [0u8; 128];
        let len = encode_query(id, ty(), &mut q).unwrap();
        q[..len].to_vec()
    }

    #[test]
    fn announced_instances_answer_queries() {
        // Nothing is answered while probing.
        let (probing, _) = table().register(&service("unit-1"), 0);
        assert!(reply(&probing, &ptr_query(0), MDNS_PORT).is_none());

        let t = live();
        // Multicast PTR query: multicast answer, mDNS id 0, SRV/TXT/A added.
        let (r, msg) = reply(&t, &ptr_query(0), MDNS_PORT).unwrap();
        assert_eq!(r.to, Destination::Multicast);
        let resp = Response::parse(&msg).unwrap().unwrap();
        assert_eq!(resp.message().id(), 0);
        assert_eq!(resp.message().questions().count(), 0);
        let Some(Instance::Resolved(i)) = resp.instances(ty()).next() else {
            panic!()
        };
        assert_eq!((i.label(), i.ttl), (Ok("unit-1"), 120));

        // Legacy unicast: id and question echoed, TTL capped, to the source.
        let (r, msg) = reply(&t, &ptr_query(77), 40_000).unwrap();
        assert_eq!(r.to, Destination::Source);
        let resp = Response::parse(&msg).unwrap().unwrap();
        assert_eq!(resp.message().id(), 77);
        assert_eq!(resp.message().questions().count(), 1);
        assert!(resp.records().all(|r| r.ttl == LEGACY_UNICAST_TTL && !r.cache_flush));

        // QU bit from port 5353: unicast to the source.
        let mut q = [0u8; 128];
        let len = MessageWriter::new(&mut q, 0, 0)
            .unwrap()
            .question(TextName::service(ty()), TYPE_PTR, true)
            .unwrap()
            .finish();
        assert_eq!(reply(&t, &q[..len], MDNS_PORT).unwrap().0.to, Destination::Source);

        // SRV of the instance; A of the host; nothing for another name.
        let srv_q = |name: TextName<'_>, qtype| {
            let mut q = [0u8; 128];
            let len = encode_query_for(0, name, qtype, false, &mut q).unwrap();
            q[..len].to_vec()
        };
        let instance = TextName::instance("unit-1", ty()).unwrap();
        let (_, msg) = reply(&t, &srv_q(instance, TYPE_SRV), MDNS_PORT).unwrap();
        let m = Message::parse(&msg).unwrap();
        let first = m.records().next().unwrap();
        assert!(matches!(first.data, Rdata::Srv(s) if s.port == 7447));
        let (_, msg) = reply(&t, &srv_q(TextName::new("unit.local").unwrap(), TYPE_A), MDNS_PORT)
            .unwrap();
        let m = Message::parse(&msg).unwrap();
        assert_eq!(m.records().count(), 1);
        assert!(matches!(m.records().next().unwrap().data, Rdata::A(a) if a.octets() == [10, 0, 0, 7]));
        let other = TextName::new("other.local").unwrap();
        assert!(reply(&t, &srv_q(other, TYPE_A), MDNS_PORT).is_none());
        // Malformed datagrams are ignored.
        assert!(reply(&t, &[0xFF; 5], MDNS_PORT).is_none());
    }

    #[test]
    fn known_answers_are_suppressed() {
        let t = live();
        let known = |ttl| {
            let mut q = [0u8; 256];
            let ptr = RecordOut {
                name: TextName::service(ty()),
                ttl,
                cache_flush: false,
                data: RecordData::Ptr(TextName::instance("unit-1", ty()).unwrap()),
            };
            let len = MessageWriter::new(&mut q, 0, 0)
                .unwrap()
                .question(TextName::service(ty()), TYPE_PTR, false)
                .unwrap()
                .record(Section::Answer, &ptr)
                .unwrap()
                .finish();
            q[..len].to_vec()
        };
        assert!(reply(&t, &known(60), MDNS_PORT).is_none());
        // Under half our TTL: answered again.
        assert!(reply(&t, &known(59), MDNS_PORT).is_some());
    }

    #[test]
    fn withdraw_sends_a_goodbye_only_once_announced() {
        let mut out = [0u8; 1024];
        let (t, r) = live().withdraw(KIND, "unit-1", &mut out);
        let len = r.unwrap().unwrap();
        assert!(t.is_empty());
        let resp = Response::parse(&out[..len]).unwrap().unwrap();
        assert!(matches!(resp.instances(ty()).next(), Some(Instance::Gone(_))));
        assert!(resp.records().all(|r| r.ttl == 0));
        // Probing only, or unknown: nothing to say.
        let (t, _) = table().register(&service("unit-1"), 0);
        let (t, r) = t.withdraw(KIND, "unit-1", &mut out);
        assert_eq!((r, t.len()), (Ok(None), 0));
        assert_eq!(t.withdraw(KIND, "nope", &mut out).1, Ok(None));
    }

    #[test]
    fn registration_is_validated_and_bounded() {
        let t = table();
        let bad = |endpoint| Service {
            endpoint,
            ..service("x")
        };
        for e in [
            Endpoint::Locator("tcp/1.2.3.4:5"),
            Endpoint::Local,
            Endpoint::Socket("0.0.0.0:80".parse().unwrap()),
        ] {
            assert_eq!(t.clone().register(&bad(e), 0).1, Err(AnnounceError::Endpoint));
        }
        let unmappable = Service {
            kind: ServiceKind::from_static("Bad/Kind"),
            ..service("x")
        };
        assert!(matches!(
            t.clone().register(&unmappable, 0).1,
            Err(AnnounceError::Record(_))
        ));
        assert!(t.clone().register(&service(""), 0).1.is_err());
        assert!(AnnounceTable::new("bad host").is_err());
        assert!(AnnounceTable::new("").is_err());
        let names: StdVec<std::string::String> =
            (0..=MAX_INSTANCES).map(|i| std::format!("u{i}")).collect();
        let mut t = t;
        for name in &names[..MAX_INSTANCES] {
            let (next, r) = t.register(&service(name), 0);
            r.unwrap();
            t = next;
        }
        assert_eq!(
            t.register(&service(&names[MAX_INSTANCES]), 0).1,
            Err(AnnounceError::Full)
        );
    }

    #[test]
    fn re_registering_an_announced_instance_announces_without_probing() {
        let t = live();
        let changed = Service {
            txt: b"\x05ver=6",
            ..service("unit-1")
        };
        let (t, r) = t.register(&changed, 9_000);
        r.unwrap();
        assert_eq!(t.state(KIND, "unit-1"), Some(InstanceState::Pending));
        // Still answers meanwhile.
        assert!(reply(&t, &ptr_query(0), MDNS_PORT).is_some());
        let (t, msg) = send(t, 9_000);
        assert!(Message::parse(&msg).unwrap().is_response());
        assert_eq!(t.state(KIND, "unit-1"), Some(InstanceState::Announced));
        let resp = Response::parse(&msg).unwrap().unwrap();
        let Some(Instance::Resolved(i)) = resp.instances(ty()).next() else {
            panic!()
        };
        assert_eq!(i.txt.unwrap().get(b"ver"), Some(&b"6"[..]));
    }
}
