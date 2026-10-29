//! mDNS browsing over a tokio UDP socket (feature `mdns`):
//! [`MdnsSocket`] implements [`Browse`].
//!
//! # Behaviour
//!
//! * By default [`MdnsSocket`] is a one-shot querier (RFC 6762 §5.1): it
//!   binds an ephemeral port on `0.0.0.0` and sends queries to
//!   `224.0.0.251:5353`. Responders answer a query from a port other than
//!   5353 with a unicast reply to that port, so the socket needs neither
//!   port 5353 (often held by Avahi or `mDNSResponder`) nor group
//!   membership. It sees answers to its own queries only, not unsolicited
//!   announcements or goodbyes; for those, bind `0.0.0.0:5353` and
//!   [`join_multicast`](MdnsSocketBuilder::join_multicast).
//! * [`query`](MdnsSocket::query) sends one PTR query, multicast with IP
//!   TTL 255, carrying a fresh id (wrapping `u16`, skipping 0, advanced
//!   before the send); the last
//!   [`MAX_OUTSTANDING`](super::origin::MAX_OUTSTANDING) ids are remembered as
//!   outstanding.
//! * Origin (RFC 6762 §11, [`super::origin`]): unless a destination was set
//!   explicitly, responses are accepted only from on-link sources and port
//!   5353, and a one-shot querier (bound to a port other than 5353) accepts
//!   only responses carrying an outstanding id. Refused datagrams are
//!   skipped like queries. The source of the last response delivered is
//!   [`last_source`](MdnsSocket::last_source).
//! * [`recv_response`](MdnsSocket::recv_response) waits for a datagram,
//!   keeps it in an internal [`MAX_MESSAGE_LEN`] buffer, and copies it into
//!   the caller's buffer. Queries, refused sources and datagrams over
//!   [`MAX_MESSAGE_LEN`] are skipped; a datagram that does not parse is
//!   [`MdnsError::Wire`] and is dropped. After [`MAX_IGNORED_DATAGRAMS`]
//!   skipped datagrams in one call, every receive call returns
//!   [`MdnsError::IgnoredLimit`], so each is bounded in datagrams.
//! * A response that does not fit the caller's buffer is
//!   [`MdnsError::BufferTooSmall`]. It is kept for
//!   [`MAX_SHORT_BUFFER_RETRIES`] (1) more call, so a retry with a larger
//!   buffer returns it; if that call's buffer is too small as well, the
//!   error is reported again and the response is dropped, so an oversized
//!   datagram cannot block the socket. [`discard_kept`](MdnsSocket::discard_kept)
//!   drops it at once.
//! * Cancel safety: tokio's `recv_from` is cancel-safe and a received
//!   datagram is stored in `self` before anything else can fail, so a
//!   dropped receive loses no response.
//!
//! # Browse
//!
//! [`Browse::browse`] takes an exact kind only (a DNS-SD type cannot
//! express a prefix; a prefix is [`MdnsError::UnsupportedFilter`]), maps it
//! with [`dns_sd_type_in`] into the [`DOMAIN`] (`cam/jpeg` →
//! `_cam-jpeg._tcp.local`) and sends the query. [`Browse::next`] /
//! [`Browse::try_next`] then report each instance of that type in the
//! responses received, one event per call, in record order:
//!
//! * [`Instance::Resolved`] → [`Event::Up`]: the instance label, the
//!   endpoint as an IPv4 (else IPv6) socket address when the response
//!   carried an address record, else [`Endpoint::Host`] with the SRV target,
//!   the TXT bytes, and the PTR's TTL; no node id.
//! * [`Instance::Gone`] → [`Event::Down`].
//! * [`Instance::Unresolved`] (PTR without SRV) is skipped; resolving it is
//!   left to a later query.
//!
//! A datagram cannot wedge the browser: a datagram that does not parse is
//! skipped (counted as ignored), and so is an instance whose event cannot
//! be built from it (a label or host that is not UTF-8 or too long) or
//! would need more than [`MAX_EVENT_LEN`] bytes. An event that does not fit
//! the caller's buffer is [`MdnsError::Record`]`(BufferTooSmall)` and is
//! kept under the same rule as an oversized response: one retry, then the
//! instance is skipped. Mixing the raw receive methods with `next` hands
//! the kept response to whichever comes first.
//!
//! Tests point the socket at a loopback responder with
//! [`destination`](MdnsSocketBuilder::destination), so they put no
//! multicast traffic on the network.

use core::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::io;

use tokio::net::UdpSocket;

use super::announce::AnnounceError;
use super::origin::{self, Kind, OnLink, OriginPolicy, Outstanding};
use super::wire::{
    HEADER_LEN, Instance, MAX_MESSAGE_LEN, MAX_NAME_LEN, Message, Response, ServiceType, WireError,
    encode_query,
};
use super::{DOMAIN, MAX_TYPE_TEXT, MDNS_GROUP_V4, MDNS_PORT};
use crate::discovery::record::{dns_sd_type_in, event_into};
use crate::discovery::{Browse, Endpoint, Event, RecordError, Service, ServiceKind};
use crate::key::{AsKey, KeyBuf, KeyFilter, MAX_KEY_LEN};
use crate::transport::copy_into;

/// Datagrams skipped in one receive call before it gives up.
pub const MAX_IGNORED_DATAGRAMS: usize = 64;
/// Further calls a response (or event) too large for the caller's buffer
/// is kept for before it is dropped.
pub const MAX_SHORT_BUFFER_RETRIES: u8 = 1;
/// Largest event [`Browse::next`] reports: kind, instance, host name and
/// TXT bytes together. Larger instances are skipped.
pub const MAX_EVENT_LEN: usize = 1024;

/// Largest query [`encode_query`] produces: header, name, type and class.
const MAX_QUERY_LEN: usize = HEADER_LEN + MAX_NAME_LEN + 4;

/// Failure of an [`MdnsSocket`] or [`MdnsAnnouncer`](super::announce::MdnsAnnouncer).
#[derive(Debug)]
#[non_exhaustive]
pub enum MdnsError {
    /// The socket failed.
    Io(io::Error),
    /// A received datagram is not a valid DNS message, or a query could not
    /// be encoded.
    Wire(WireError),
    /// The response needs `needed` bytes but the buffer holds only
    /// `capacity`. The response is kept for [`MAX_SHORT_BUFFER_RETRIES`]
    /// more call.
    BufferTooSmall {
        /// Bytes required.
        needed: usize,
        /// Bytes available.
        capacity: usize,
    },
    /// [`MAX_IGNORED_DATAGRAMS`] datagrams passed without a response (or,
    /// for [`Browse::next`], without an event; for an announcer,
    /// [`MAX_DATAGRAMS_PER_CALL`](super::announce::MAX_DATAGRAMS_PER_CALL)).
    IgnoredLimit,
    /// [`Browse::browse`] with a prefix filter: DNS-SD browses one type.
    UnsupportedFilter,
    /// The kind has no DNS-SD type, or an event does not fit the caller's
    /// buffer (kept as for `BufferTooSmall`).
    Record(RecordError),
    /// The announcer refused a service or lost every rename to conflicts.
    Announce(AnnounceError),
}

impl core::fmt::Display for MdnsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            MdnsError::Io(e) => write!(f, "mdns socket error: {e}"),
            MdnsError::Wire(e) => write!(f, "malformed mdns message: {e:?}"),
            MdnsError::BufferTooSmall { needed, capacity } => {
                write!(f, "buffer too small: need {needed} bytes, have {capacity}")
            }
            MdnsError::IgnoredLimit => f.write_str("too many datagrams without a result"),
            MdnsError::UnsupportedFilter => f.write_str("mDNS browses an exact kind, not a prefix"),
            MdnsError::Record(e) => write!(f, "discovery record: {e:?}"),
            MdnsError::Announce(e) => write!(f, "mdns announce: {e:?}"),
        }
    }
}

impl std::error::Error for MdnsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            MdnsError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for MdnsError {
    fn from(e: io::Error) -> Self {
        MdnsError::Io(e)
    }
}

impl From<WireError> for MdnsError {
    fn from(e: WireError) -> Self {
        MdnsError::Wire(e)
    }
}

/// Configures an [`MdnsSocket`]. Defaults: bind `0.0.0.0:0`, send to
/// `224.0.0.251:5353`, join no group, accept on-link sources only
/// ([`OriginPolicy::OnLink`] with the system's connected subnets).
#[derive(Debug, Clone)]
pub struct MdnsSocketBuilder {
    bind: SocketAddr,
    destination: SocketAddr,
    join: Option<Ipv4Addr>,
    policy: OriginPolicy,
    on_link: Option<OnLink>,
}

impl Default for MdnsSocketBuilder {
    fn default() -> Self {
        MdnsSocketBuilder {
            bind: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)),
            destination: SocketAddr::V4(SocketAddrV4::new(MDNS_GROUP_V4, MDNS_PORT)),
            join: None,
            policy: OriginPolicy::OnLink,
            on_link: None,
        }
    }
}

impl MdnsSocketBuilder {
    /// The local address to bind.
    pub fn bind(self, bind: SocketAddr) -> Self {
        MdnsSocketBuilder { bind, ..self }
    }

    /// Where queries are sent. Also selects [`OriginPolicy::Any`] (set
    /// [`origin_policy`](Self::origin_policy) afterwards to check anyway).
    pub fn destination(self, destination: SocketAddr) -> Self {
        MdnsSocketBuilder {
            destination,
            policy: OriginPolicy::Any,
            ..self
        }
    }

    /// Joins the IPv4 mDNS group on `interface` (`UNSPECIFIED` lets the OS
    /// choose), to receive announcements sent to the group.
    pub fn join_multicast(self, interface: Ipv4Addr) -> Self {
        MdnsSocketBuilder {
            join: Some(interface),
            ..self
        }
    }

    /// Which sources are accepted; see [`super::origin`].
    pub fn origin_policy(self, policy: OriginPolicy) -> Self {
        MdnsSocketBuilder { policy, ..self }
    }

    /// The on-link subnets for [`OriginPolicy::OnLink`] (default: read from
    /// the system when built, [`origin::system_on_link`]).
    pub fn on_link(self, on_link: OnLink) -> Self {
        MdnsSocketBuilder {
            on_link: Some(on_link),
            ..self
        }
    }

    /// Binds the socket.
    pub async fn build(self) -> Result<MdnsSocket, MdnsError> {
        let socket = UdpSocket::bind(self.bind).await?;
        if self.destination.ip().is_multicast() {
            socket.set_multicast_ttl_v4(255)?;
        }
        if let Some(interface) = self.join {
            socket.join_multicast_v4(MDNS_GROUP_V4, interface)?;
        }
        let one_shot = socket.local_addr()?.port() != MDNS_PORT;
        let on_link = match (self.policy, self.on_link) {
            (_, Some(o)) => o,
            (OriginPolicy::OnLink, None) => origin::system_on_link(),
            (OriginPolicy::Any, None) => OnLink::new(),
        };
        Ok(MdnsSocket {
            socket,
            destination: self.destination,
            policy: self.policy,
            on_link,
            one_shot,
            outstanding: Outstanding::default(),
            next_id: 1,
            rx: [0; MAX_MESSAGE_LEN + 1],
            kept: None,
            kept_from: None,
            last_source: None,
            short: 0,
            browsing: None,
            cursor: 0,
        })
    }
}

/// What the kept datagram turned out to be.
enum Kept {
    /// Nothing is kept.
    Empty,
    /// It was skipped (a query, oversized, a refused source) and dropped.
    Ignored,
    /// A response of this length, still kept.
    Response(usize),
}

/// mDNS service discovery over UDP; see the module docs.
#[derive(Debug)]
pub struct MdnsSocket {
    socket: UdpSocket,
    destination: SocketAddr,
    policy: OriginPolicy,
    on_link: OnLink,
    /// Bound to a port other than 5353: replies must carry an outstanding id.
    one_shot: bool,
    outstanding: Outstanding,
    next_id: u16,
    /// One byte over the largest message, to detect oversized datagrams.
    rx: [u8; MAX_MESSAGE_LEN + 1],
    /// Length of a received datagram in `rx` not yet delivered.
    kept: Option<usize>,
    /// Its source.
    kept_from: Option<SocketAddr>,
    /// Source of the last response delivered.
    last_source: Option<SocketAddr>,
    /// Times the kept datagram (at `cursor`) did not fit the caller's buffer.
    short: u8,
    /// The kind being browsed and its DNS-SD type.
    browsing: Option<Browsing>,
    /// Instances of the kept response already reported (or skipped) by
    /// `next`.
    cursor: usize,
}

/// What [`Browse::browse`] set up.
#[derive(Debug, Clone, Copy)]
struct Browsing {
    kind: KeyBuf<MAX_KEY_LEN>,
    service: [u8; MAX_TYPE_TEXT],
    service_len: usize,
}

impl Browsing {
    fn service(&self) -> Result<ServiceType<'_>, MdnsError> {
        let text = self
            .service
            .get(..self.service_len)
            .ok_or(WireError::InvalidServiceType)?;
        let text = core::str::from_utf8(text).map_err(|_| WireError::InvalidServiceType)?;
        Ok(ServiceType::new(text)?)
    }

    fn kind(&self) -> ServiceKind<'_> {
        ServiceKind::new(self.kind.as_key())
    }
}

/// What [`scan`] found in a kept response, from the cursor on.
enum Scan<'b> {
    /// Instance `index`'s event, copied into the caller's buffer.
    Event(usize, Event<'b>),
    /// Instance `index`'s event does not fit the caller's buffer.
    TooSmall(usize),
    /// No further reportable instance.
    Done,
}

/// Builds the event of `instance` in `scratch`; `None` if the datagram
/// cannot give one (a label or host that is not UTF-8, too long, or an
/// event over [`MAX_EVENT_LEN`]).
fn build_event<'s>(
    kind: ServiceKind<'_>,
    instance: &Instance<'_>,
    scratch: &'s mut [u8; MAX_EVENT_LEN],
) -> Option<Event<'s>> {
    let mut host = [0u8; MAX_NAME_LEN];
    let event = match instance {
        Instance::Resolved(i) => {
            let endpoint = match (i.ipv4, i.ipv6) {
                (Some(ip), _) => Endpoint::Socket(SocketAddr::new(ip.into(), i.srv.port)),
                (None, Some(ip)) => Endpoint::Socket(SocketAddr::new(ip.into(), i.srv.port)),
                (None, None) => Endpoint::Host {
                    name: i.srv.target.write_text(&mut host).ok()?,
                    port: i.srv.port,
                },
            };
            Event::Up(Service {
                kind,
                instance: i.label().ok()?,
                node: None,
                endpoint,
                txt: i.txt.map_or(&[][..], |t| t.as_bytes()),
                ttl_s: i.ttl,
            })
        }
        Instance::Gone(name) => Event::Down {
            kind,
            instance: core::str::from_utf8(name.first_label().unwrap_or_default()).ok()?,
        },
        Instance::Unresolved(_) => return None,
    };
    event_into(scratch, &event).ok()
}

/// The next reportable instance of `browsing`'s type in `packet` at or
/// after `cursor`, copied into `buf`. Pure: unparseable datagrams and
/// instances that give no event are skipped.
fn scan<'b>(packet: &[u8], browsing: &Browsing, cursor: usize, buf: &'b mut [u8]) -> Scan<'b> {
    let Ok(service) = browsing.service() else {
        return Scan::Done;
    };
    let Ok(Some(response)) = Response::parse(packet) else {
        return Scan::Done;
    };
    let mut scratch = [0u8; MAX_EVENT_LEN];
    // Bounded by MAX_RECORDS PTRs.
    for (index, instance) in response.instances(service).enumerate().skip(cursor) {
        if let Some(event) = build_event(browsing.kind(), &instance, &mut scratch) {
            return match event_into(buf, &event) {
                Ok(e) => Scan::Event(index, e),
                Err(_) => Scan::TooSmall(index),
            };
        }
    }
    Scan::Done
}

impl MdnsSocket {
    /// A builder with the default one-shot querier settings.
    pub fn builder() -> MdnsSocketBuilder {
        MdnsSocketBuilder::default()
    }

    /// The bound local address.
    pub fn local_addr(&self) -> Result<SocketAddr, MdnsError> {
        Ok(self.socket.local_addr()?)
    }

    /// The source address of the last response delivered (by
    /// `recv_response`, `try_recv_response` or a `Browse` event).
    pub fn last_source(&self) -> Option<SocketAddr> {
        self.last_source
    }

    /// Drops the kept datagram, if any (e.g. after a
    /// [`MdnsError::BufferTooSmall`] the caller will not retry).
    pub fn discard_kept(&mut self) {
        self.kept = None;
        self.kept_from = None;
        self.cursor = 0;
        self.short = 0;
    }

    /// Classifies the kept datagram, dropping it unless it is a response
    /// from an accepted source.
    fn classify(&mut self) -> Result<Kept, MdnsError> {
        let Some(len) = self.kept else {
            return Ok(Kept::Empty);
        };
        let packet = self.rx.get(..len).filter(|_| len <= MAX_MESSAGE_LEN);
        match packet.map(Message::parse) {
            Some(Ok(m)) if m.is_response() && self.accepts(m.id()) => Ok(Kept::Response(len)),
            Some(Err(e)) => {
                self.discard_kept();
                Err(e.into())
            }
            _ => {
                self.discard_kept();
                Ok(Kept::Ignored)
            }
        }
    }

    /// True if a response with `id` from the kept source passes the origin
    /// policy.
    fn accepts(&self, id: u16) -> bool {
        let Some(from) = self.kept_from else {
            return false;
        };
        let kind = Kind::Response {
            id,
            outstanding: self.one_shot.then(|| self.outstanding.ids()),
        };
        origin::check(self.policy, &self.on_link, from, kind).is_ok()
    }

    /// Counts a too-small buffer for the kept datagram; true once it has
    /// been retried [`MAX_SHORT_BUFFER_RETRIES`] times and must go.
    fn short_buffer(&mut self) -> bool {
        self.short = self.short.saturating_add(1);
        self.short > MAX_SHORT_BUFFER_RETRIES
    }

    /// Copies the kept response of `len` bytes into `buf` and releases it.
    /// A response partly reported by [`Browse::next`] is handed over whole.
    fn deliver<'b>(&mut self, len: usize, buf: &'b mut [u8]) -> Result<Response<'b>, MdnsError> {
        let capacity = buf.len();
        let packet = self.rx.get(..len).ok_or(WireError::Truncated)?;
        let Some(copied) = copy_into(buf, packet) else {
            if self.short_buffer() {
                self.discard_kept();
            }
            return Err(MdnsError::BufferTooSmall {
                needed: len,
                capacity,
            });
        };
        self.last_source = self.kept_from;
        self.discard_kept();
        Response::parse(copied)?.ok_or(MdnsError::Wire(WireError::Truncated))
    }
}

impl MdnsSocket {
    /// Sends one PTR query for instances of `service`; answers arrive
    /// through [`recv_response`](Self::recv_response) or, when browsing a
    /// kind, [`Browse::next`]. Dropping may or may not have sent the query;
    /// it is never sent twice.
    pub async fn query(&mut self, service: ServiceType<'_>) -> Result<(), MdnsError> {
        let id = self.next_id;
        self.next_id = match id.wrapping_add(1) {
            0 => 1,
            n => n,
        };
        self.outstanding = self.outstanding.sent(id);
        let mut query = [0u8; MAX_QUERY_LEN];
        let len = encode_query(id, service, &mut query)?;
        let packet = query.get(..len).ok_or(WireError::BufferTooSmall)?;
        self.socket.send_to(packet, self.destination).await?;
        Ok(())
    }

    /// Waits for the next mDNS response and copies it into `buf`; see the
    /// module docs. Cancel safe.
    pub async fn recv_response<'b>(
        &mut self,
        buf: &'b mut [u8],
    ) -> Result<Response<'b>, MdnsError> {
        for _ in 0..MAX_IGNORED_DATAGRAMS {
            if self.kept.is_none() {
                let (len, from) = self.socket.recv_from(&mut self.rx).await?;
                self.keep(len, from);
            }
            if let Kept::Response(len) = self.classify()? {
                return self.deliver(len, buf);
            }
        }
        Err(MdnsError::IgnoredLimit)
    }

    /// Returns the next response if one has already arrived, without
    /// waiting.
    pub fn try_recv_response<'b>(
        &mut self,
        buf: &'b mut [u8],
    ) -> Result<Option<Response<'b>>, MdnsError> {
        for _ in 0..MAX_IGNORED_DATAGRAMS {
            if !self.try_fill()? {
                return Ok(None);
            }
            match self.classify()? {
                Kept::Response(len) => return self.deliver(len, buf).map(Some),
                Kept::Empty | Kept::Ignored => {}
            }
        }
        Err(MdnsError::IgnoredLimit)
    }

    /// Records a received datagram of `len` bytes from `from`.
    fn keep(&mut self, len: usize, from: SocketAddr) {
        self.kept = Some(len);
        self.kept_from = Some(from);
        self.cursor = 0;
        self.short = 0;
    }

    /// Makes sure a datagram is kept, without waiting; false if none has
    /// arrived.
    fn try_fill(&mut self) -> Result<bool, MdnsError> {
        if self.kept.is_some() {
            return Ok(true);
        }
        match self.socket.try_recv_from(&mut self.rx) {
            Ok((len, from)) => {
                self.keep(len, from);
                Ok(true)
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// The next event from the kept datagram, if it has one; otherwise
    /// drops it. Datagram faults never surface here (see the module docs):
    /// the only error is the caller's buffer being too small.
    fn event_from_kept<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Event<'b>>, MdnsError> {
        let len = match self.classify() {
            Ok(Kept::Response(len)) => len,
            // A malformed datagram was dropped by classify: skip it.
            Ok(Kept::Empty | Kept::Ignored) | Err(_) => return Ok(None),
        };
        let (Some(browsing), Some(packet)) = (self.browsing, self.rx.get(..len)) else {
            self.discard_kept();
            return Ok(None);
        };
        match scan(packet, &browsing, self.cursor, buf) {
            Scan::Event(index, event) => {
                self.last_source = self.kept_from;
                self.cursor = index + 1;
                self.short = 0;
                Ok(Some(event))
            }
            Scan::TooSmall(index) => {
                if self.short_buffer() {
                    self.cursor = index + 1;
                    self.short = 0;
                }
                Err(MdnsError::Record(RecordError::BufferTooSmall))
            }
            Scan::Done => {
                self.discard_kept();
                Ok(None)
            }
        }
    }
}

impl Browse for MdnsSocket {
    type Error = MdnsError;

    /// Queries the kind's DNS-SD type; see the module docs.
    async fn browse(&mut self, filter: KeyFilter<'_>) -> Result<(), MdnsError> {
        let KeyFilter::Exact(key) = filter else {
            return Err(MdnsError::UnsupportedFilter);
        };
        let mut text = [0u8; MAX_TYPE_TEXT];
        let service =
            dns_sd_type_in(ServiceKind::new(key), DOMAIN, &mut text).map_err(MdnsError::Record)?;
        let browsing = Browsing {
            kind: KeyBuf::from_key(key).map_err(|_| MdnsError::Record(RecordError::NotAKind))?,
            service_len: service.as_str().len(),
            service: text,
        };
        self.browsing = Some(browsing);
        self.cursor = 0;
        self.short = 0;
        // `browsing` is a copy: the query borrows it, not `self`.
        let service = browsing.service()?;
        self.query(service).await
    }

    /// The next instance event; bounded by [`MAX_IGNORED_DATAGRAMS`]
    /// datagrams without one. Cancel safe.
    async fn next<'b>(&mut self, buf: &'b mut [u8]) -> Result<Event<'b>, MdnsError> {
        for _ in 0..MAX_IGNORED_DATAGRAMS {
            if self.kept.is_none() {
                let (len, from) = self.socket.recv_from(&mut self.rx).await?;
                self.keep(len, from);
            }
            if self.peek_event()? {
                return self
                    .event_from_kept(buf)?
                    .ok_or(MdnsError::Wire(WireError::Truncated));
            }
        }
        Err(MdnsError::IgnoredLimit)
    }

    fn try_next<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Event<'b>>, MdnsError> {
        for _ in 0..MAX_IGNORED_DATAGRAMS {
            if !self.try_fill()? {
                return Ok(None);
            }
            if self.peek_event()? {
                return self.event_from_kept(buf);
            }
        }
        Err(MdnsError::IgnoredLimit)
    }
}

impl MdnsSocket {
    /// True if the kept datagram has an event to report from the cursor
    /// on (or one too large for any buffer the caller may pass, which
    /// `event_from_kept` then reports); otherwise drops it.
    ///
    /// Separate from `event_from_kept` so the loops in `next` and
    /// `try_next` hand the caller's buffer out only on return.
    fn peek_event(&mut self) -> Result<bool, MdnsError> {
        let mut scratch = [0u8; MAX_EVENT_LEN];
        let len = match self.classify() {
            Ok(Kept::Response(len)) => len,
            Ok(Kept::Empty | Kept::Ignored) | Err(_) => return Ok(false),
        };
        let (Some(browsing), Some(packet)) = (self.browsing, self.rx.get(..len)) else {
            self.discard_kept();
            return Ok(false);
        };
        match scan(packet, &browsing, self.cursor, &mut scratch) {
            Scan::Event(..) | Scan::TooSmall(_) => Ok(true),
            Scan::Done => {
                self.discard_kept();
                Ok(false)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::key::Key;
    use crate::mdns::wire::{TYPE_PTR, fixture};
    use std::time::Duration;

    fn http() -> ServiceType<'static> {
        ServiceType::new("_http._tcp.local").unwrap()
    }

    /// A querier aimed at a loopback "responder" socket.
    async fn pair() -> (MdnsSocket, UdpSocket) {
        let responder = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let querier = MdnsSocket::builder()
            .bind("127.0.0.1:0".parse().unwrap())
            .destination(responder.local_addr().unwrap())
            .build()
            .await
            .unwrap();
        (querier, responder)
    }

    async fn within<T>(f: impl Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(5), f)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn browse_sends_a_ptr_query_and_recv_resolves_the_answer() {
        let (mut q, responder) = pair().await;
        within(q.query(http())).await.unwrap();
        let mut rx = [0u8; 512];
        let (n, from) = within(responder.recv_from(&mut rx)).await.unwrap();
        let query = Message::parse(&rx[..n]).unwrap();
        assert!(!query.is_response());
        let question = query.questions().next().unwrap();
        assert!(question.name.is_service(&http()));
        assert_eq!(question.qtype, TYPE_PTR);

        // Answer echoing the query id, as a legacy-unicast responder does.
        responder
            .send_to(&fixture::printer(query.id(), 120), from)
            .await
            .unwrap();
        let mut buf = [0u8; MAX_MESSAGE_LEN];
        let r = within(q.recv_response(&mut buf)).await.unwrap();
        assert_eq!(r.message().id(), query.id());
        let Some(Instance::Resolved(i)) = r.instances(http()).next() else {
            panic!()
        };
        assert_eq!(i.label(), Ok("My.Printer"));
        assert_eq!(
            i.socket_addr_v4(),
            Some("192.168.1.20:8080".parse().unwrap())
        );
    }

    #[tokio::test]
    async fn queries_are_skipped_and_ids_advance() {
        let (mut q, responder) = pair().await;
        let mut rx = [0u8; 512];
        within(q.query(http())).await.unwrap();
        within(q.query(http())).await.unwrap();
        let (n, from) = within(responder.recv_from(&mut rx)).await.unwrap();
        let first = Message::parse(&rx[..n]).unwrap().id();
        let (n, _) = within(responder.recv_from(&mut rx)).await.unwrap();
        assert_eq!(
            Message::parse(&rx[..n]).unwrap().id(),
            first.wrapping_add(1)
        );

        // Another node's query arrives first, then the answer.
        let mut query = [0u8; 64];
        let len = encode_query(0, http(), &mut query).unwrap();
        responder.send_to(&query[..len], from).await.unwrap();
        responder
            .send_to(&fixture::printer(9, 120), from)
            .await
            .unwrap();
        let mut buf = [0u8; 1024];
        assert_eq!(
            within(q.recv_response(&mut buf))
                .await
                .unwrap()
                .message()
                .id(),
            9
        );
    }

    #[tokio::test]
    async fn try_recv_is_none_when_empty_and_keeps_an_oversized_response() {
        let (mut q, responder) = pair().await;
        let mut buf = [0u8; 1024];
        assert!(q.try_recv_response(&mut buf).unwrap().is_none());

        let packet = fixture::printer(3, 120);
        let addr = q.local_addr().unwrap();
        responder.send_to(&packet, addr).await.unwrap();
        let mut small = [0u8; 16];
        let err = within(q.recv_response(&mut small)).await.unwrap_err();
        assert!(
            matches!(err, MdnsError::BufferTooSmall { needed, capacity: 16 } if needed == packet.len())
        );
        // Kept: try_recv returns it without another datagram arriving.
        assert_eq!(
            q.try_recv_response(&mut buf)
                .unwrap()
                .unwrap()
                .message()
                .id(),
            3
        );
        assert!(q.try_recv_response(&mut buf).unwrap().is_none());
    }

    #[tokio::test]
    async fn malformed_datagrams_are_errors_then_dropped() {
        let (mut q, responder) = pair().await;
        let addr = q.local_addr().unwrap();
        let packet = fixture::printer(4, 120);
        responder
            .send_to(&packet[..packet.len() - 1], addr)
            .await
            .unwrap();
        responder.send_to(&packet, addr).await.unwrap();
        let mut buf = [0u8; 1024];
        assert!(matches!(
            within(q.recv_response(&mut buf)).await,
            Err(MdnsError::Wire(WireError::Truncated))
        ));
        assert_eq!(
            within(q.recv_response(&mut buf))
                .await
                .unwrap()
                .message()
                .id(),
            4
        );
    }

    #[tokio::test]
    async fn ignored_limit_bounds_try_recv() {
        let (mut q, responder) = pair().await;
        let addr = q.local_addr().unwrap();
        let mut query = [0u8; 64];
        let len = encode_query(0, http(), &mut query).unwrap();
        for _ in 0..MAX_IGNORED_DATAGRAMS {
            responder.send_to(&query[..len], addr).await.unwrap();
        }
        responder
            .send_to(&fixture::printer(5, 120), addr)
            .await
            .unwrap();
        // Wait until the last datagram is queued.
        let mut peek = [0u8; 1];
        within(async { while q.socket.peek_from(&mut peek).await.is_err() {} }).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut buf = [0u8; 1024];
        assert!(matches!(
            q.try_recv_response(&mut buf),
            Err(MdnsError::IgnoredLimit)
        ));
        assert_eq!(
            q.try_recv_response(&mut buf)
                .unwrap()
                .unwrap()
                .message()
                .id(),
            5
        );
    }

    #[tokio::test]
    async fn cancelled_recv_loses_no_response() {
        let (mut q, responder) = pair().await;
        let addr = q.local_addr().unwrap();
        let mut buf = [0u8; 1024];
        let timed_out =
            tokio::time::timeout(Duration::from_millis(20), q.recv_response(&mut buf)).await;
        assert!(timed_out.is_err());
        responder
            .send_to(&fixture::printer(6, 120), addr)
            .await
            .unwrap();
        assert_eq!(
            within(q.recv_response(&mut buf))
                .await
                .unwrap()
                .message()
                .id(),
            6
        );
    }

    #[tokio::test]
    async fn browse_maps_the_kind_and_reports_up_then_down() {
        let (mut q, responder) = pair().await;
        let http_kind = Key::from_static("http");
        assert!(matches!(
            q.browse(KeyFilter::Prefix(http_kind)).await,
            Err(MdnsError::UnsupportedFilter)
        ));
        assert!(matches!(
            q.browse(KeyFilter::Exact(Key::from_static("Http"))).await,
            Err(MdnsError::Record(RecordError::Unmappable))
        ));
        within(q.browse(KeyFilter::Exact(http_kind))).await.unwrap();
        let mut rx = [0u8; 512];
        let (n, from) = within(responder.recv_from(&mut rx)).await.unwrap();
        let question = Message::parse(&rx[..n])
            .unwrap()
            .questions()
            .next()
            .unwrap();
        assert!(
            question.name.is_service(&http()),
            "kind `http` browses _http._tcp.local"
        );

        let mut buf = [0u8; 256];
        assert!(q.try_next(&mut buf).unwrap().is_none());
        responder
            .send_to(&fixture::printer(1, 120), from)
            .await
            .unwrap();
        let mut up_buf = [0u8; 256];
        let up = within(q.next(&mut up_buf)).await.unwrap();
        let Event::Up(s) = up else { panic!("{up:?}") };
        assert_eq!(
            (s.kind.as_str(), s.instance, s.ttl_s, s.node),
            ("http", "My.Printer", 120, None)
        );
        assert_eq!(
            s.endpoint,
            Endpoint::Socket("192.168.1.20:8080".parse().unwrap())
        );
        assert_eq!(s.txt().unwrap().get(b"path"), Some(&b"/"[..]));
        // The response had one instance: the next call waits for more.
        assert!(q.try_next(&mut buf).unwrap().is_none());

        // An event too big for the buffer is kept.
        responder
            .send_to(&fixture::printer(2, 0), from)
            .await
            .unwrap();
        let mut small = [0u8; 4];
        assert!(matches!(
            within(q.next(&mut small)).await,
            Err(MdnsError::Record(RecordError::BufferTooSmall))
        ));
        assert_eq!(
            q.try_next(&mut buf).unwrap(),
            Some(Event::Down {
                kind: s.kind,
                instance: "My.Printer"
            })
        );
    }

    #[tokio::test]
    async fn browse_reports_an_unaddressed_instance_by_host() {
        let (mut q, responder) = pair().await;
        within(q.browse(KeyFilter::Exact(Key::from_static("http"))))
            .await
            .unwrap();
        let mut rx = [0u8; 512];
        let (_, from) = within(responder.recv_from(&mut rx)).await.unwrap();
        // Drop the A record (the last additional record).
        let mut p = fixture::printer(3, 60);
        p[11] = 2;
        responder.send_to(&p, from).await.unwrap();
        let mut buf = [0u8; 256];
        let Event::Up(s) = within(q.next(&mut buf)).await.unwrap() else {
            panic!()
        };
        assert_eq!(
            s.endpoint,
            Endpoint::Host {
                name: "printer.local",
                port: 8080
            }
        );
    }

    /// A response for `_http._tcp.local` instance `label` with `txt`.
    fn http_response(id: u16, label: &str, txt: &[u8], ttl: u32) -> std::vec::Vec<u8> {
        use crate::dns::wire::{
            FLAG_AA, FLAG_QR, MessageWriter, RecordData, RecordOut, Section, TextName, Txt,
        };
        let instance = TextName::instance(label, http()).unwrap();
        let host = TextName::new("host.local").unwrap();
        let rec = |name, data| RecordOut {
            name,
            ttl,
            cache_flush: false,
            data,
        };
        let mut out = [0u8; MAX_MESSAGE_LEN];
        let len = MessageWriter::new(&mut out, id, FLAG_QR | FLAG_AA)
            .unwrap()
            .record(
                Section::Answer,
                &rec(TextName::service(http()), RecordData::Ptr(instance)),
            )
            .unwrap()
            .record(
                Section::Additional,
                &rec(
                    instance,
                    RecordData::Srv {
                        priority: 0,
                        weight: 0,
                        port: 80,
                        target: host,
                    },
                ),
            )
            .unwrap()
            .record(
                Section::Additional,
                &rec(instance, RecordData::Txt(Txt::new(txt).unwrap())),
            )
            .unwrap()
            .record(
                Section::Additional,
                &rec(host, RecordData::A("127.0.0.1".parse().unwrap())),
            )
            .unwrap()
            .finish();
        out[..len].to_vec()
    }

    #[tokio::test]
    async fn on_link_policy_refuses_wrong_port_and_unknown_ids() {
        // Loopback is on-link, but the responder's port is not 5353.
        let responder = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut q = MdnsSocket::builder()
            .bind("127.0.0.1:0".parse().unwrap())
            .destination(responder.local_addr().unwrap())
            .origin_policy(OriginPolicy::OnLink)
            .on_link(OnLink::new())
            .build()
            .await
            .unwrap();
        within(q.query(http())).await.unwrap();
        let mut rx = [0u8; 512];
        let (n, from) = within(responder.recv_from(&mut rx)).await.unwrap();
        let id = Message::parse(&rx[..n]).unwrap().id();
        responder
            .send_to(&fixture::printer(id, 120), from)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut buf = [0u8; 1024];
        assert!(q.try_recv_response(&mut buf).unwrap().is_none());
        assert!(q.last_source().is_none());

        // The default Any policy (explicit destination) accepts it and
        // records the source.
        let (mut q, responder) = pair().await;
        let addr = q.local_addr().unwrap();
        responder
            .send_to(&fixture::printer(1, 120), addr)
            .await
            .unwrap();
        within(q.recv_response(&mut buf)).await.unwrap();
        assert_eq!(q.last_source(), Some(responder.local_addr().unwrap()));
    }

    #[tokio::test]
    async fn an_oversized_response_is_dropped_after_one_retry() {
        let (mut q, responder) = pair().await;
        let addr = q.local_addr().unwrap();
        for id in [3, 4, 5] {
            responder
                .send_to(&fixture::printer(id, 120), addr)
                .await
                .unwrap();
        }
        let mut small = [0u8; 16];
        for _ in 0..=MAX_SHORT_BUFFER_RETRIES {
            assert!(matches!(
                within(q.recv_response(&mut small)).await,
                Err(MdnsError::BufferTooSmall { capacity: 16, .. })
            ));
        }
        // Response 3 was dropped; 4 is next. discard_kept drops 4 at once.
        let mut buf = [0u8; 1024];
        assert!(within(q.recv_response(&mut small)).await.is_err());
        q.discard_kept();
        assert_eq!(
            within(q.recv_response(&mut buf))
                .await
                .unwrap()
                .message()
                .id(),
            5
        );
    }

    #[tokio::test]
    async fn poison_datagrams_do_not_wedge_browse() {
        let (mut q, responder) = pair().await;
        within(q.browse(KeyFilter::Exact(Key::from_static("http"))))
            .await
            .unwrap();
        let mut rx = [0u8; 512];
        let (_, from) = within(responder.recv_from(&mut rx)).await.unwrap();

        // A label that is not UTF-8.
        let mut bad = http_response(1, "Xy", b"", 60);
        let at = bad.windows(3).position(|w| w == b"\x02Xy").unwrap();
        bad[at + 1] = 0xFF;
        // A datagram that does not parse.
        let truncated = &fixture::printer(2, 60)[..40];
        // An event over MAX_EVENT_LEN: five full TXT strings.
        let entry: std::vec::Vec<u8> = core::iter::once(255u8)
            .chain(core::iter::repeat_n(b'a', 255))
            .collect();
        let huge_txt = entry.repeat(5);
        let huge = http_response(3, "huge", &huge_txt, 60);
        for packet in [&bad[..], truncated, &huge, &http_response(4, "good", b"", 60)] {
            responder.send_to(packet, from).await.unwrap();
        }
        let mut buf = [0u8; 256];
        let event = within(q.next(&mut buf)).await.unwrap();
        assert_eq!(event.instance(), "good");

        // An event too big for the caller's buffer: kept once, then skipped.
        responder
            .send_to(&http_response(5, "next", b"", 60), from)
            .await
            .unwrap();
        responder
            .send_to(&http_response(6, "after", b"", 60), from)
            .await
            .unwrap();
        let mut small = [0u8; 4];
        for _ in 0..=MAX_SHORT_BUFFER_RETRIES {
            assert!(matches!(
                within(q.next(&mut small)).await,
                Err(MdnsError::Record(RecordError::BufferTooSmall))
            ));
        }
        assert_eq!(within(q.next(&mut buf)).await.unwrap().instance(), "after");
    }
}
