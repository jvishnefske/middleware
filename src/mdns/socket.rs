//! mDNS browsing over any [`Datagram`] socket: [`MdnsSocket`] implements
//! [`Browse`].
//!
//! The shell is generic, `MdnsSocket<S, N>`: `S` is the socket seam and
//! `N` the receive buffer (see [`STD_BUF`] and [`MCU_BUF`]). The bare name
//! `MdnsSocket` is the tokio instantiation (feature `mdns`), which
//! [`MdnsSocket::builder`] binds; any other socket is wrapped with
//! [`MdnsSocket::new`] and [`SocketParams`].
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
//!   keeps it in the internal `N`-byte buffer, and copies it into the
//!   caller's buffer. Queries, refused sources and oversized datagrams
//!   (`N` bytes or more: the seam truncates to the buffer, so a datagram
//!   that fills it may have been cut) are skipped; a datagram that does not parse is
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
//! * Cancel safety: the seam's `recv_from` is cancel-safe and a received
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

use core::net::SocketAddr;

use super::origin::{self, Kind, OnLink, OriginPolicy, Outstanding};
use super::wire::{
    HEADER_LEN, Instance, MAX_NAME_LEN, Message, Response, ServiceType, WireError, encode_query,
};
use super::{DOMAIN, DefaultSocket, MAX_TYPE_TEXT, STD_BUF};
#[cfg(doc)]
use super::{MCU_BUF, MDNS_PORT};
use crate::datagram::Datagram;
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

pub use super::error::MdnsError;
#[cfg(feature = "mdns")]
pub use host::MdnsSocketBuilder;

#[cfg(feature = "mdns")]
mod host;

/// How an [`MdnsSocket`] made with [`MdnsSocket::new`] sends and accepts.
///
/// On a microcontroller, `on_link` must list the LAN's subnet
/// ([`OnLink::with`]) when `policy` is [`OriginPolicy::OnLink`]: without
/// `std` the system's routes are not read, so [`OnLink::new`] alone
/// accepts loopback and link-local sources only and every peer on a
/// DHCP LAN would be refused.
#[derive(Debug, Clone)]
pub struct SocketParams {
    /// Where queries are sent (`224.0.0.251:5353` for a normal querier).
    pub destination: SocketAddr,
    /// Which sources are accepted; see [`super::origin`].
    pub policy: OriginPolicy,
    /// The on-link subnets for [`OriginPolicy::OnLink`].
    pub on_link: OnLink,
    /// The socket is bound to a port other than [`MDNS_PORT`]: responses
    /// must carry the id of an outstanding query.
    pub one_shot: bool,
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
pub struct MdnsSocket<S = DefaultSocket, const N: usize = STD_BUF> {
    socket: S,
    destination: SocketAddr,
    policy: OriginPolicy,
    on_link: OnLink,
    /// Bound to a port other than 5353: replies must carry an outstanding id.
    one_shot: bool,
    outstanding: Outstanding,
    next_id: u16,
    /// The receive buffer; a datagram that fills it is oversized.
    rx: [u8; N],
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
    fn service(&self) -> Result<ServiceType<'_>, WireError> {
        let text = self
            .service
            .get(..self.service_len)
            .ok_or(WireError::InvalidServiceType)?;
        let text = core::str::from_utf8(text).map_err(|_| WireError::InvalidServiceType)?;
        ServiceType::new(text)
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

impl<S: Datagram, const N: usize> MdnsSocket<S, N> {
    /// A socket over `socket`, which is already bound (and, to hear
    /// announcements, a member of the mDNS group). `N` must be over 1.
    pub fn new(socket: S, params: SocketParams) -> Self {
        const { assert!(N > 1, "an mDNS receive buffer needs room for a datagram") };
        MdnsSocket {
            socket,
            destination: params.destination,
            policy: params.policy,
            on_link: params.on_link,
            one_shot: params.one_shot,
            outstanding: Outstanding::default(),
            next_id: 1,
            rx: [0; N],
            kept: None,
            kept_from: None,
            last_source: None,
            short: 0,
            browsing: None,
            cursor: 0,
        }
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
    fn classify(&mut self) -> Result<Kept, MdnsError<S::Error>> {
        let Some(len) = self.kept else {
            return Ok(Kept::Empty);
        };
        let packet = self.rx.get(..len).filter(|_| len < N);
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
    fn deliver<'b>(
        &mut self,
        len: usize,
        buf: &'b mut [u8],
    ) -> Result<Response<'b>, MdnsError<S::Error>> {
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

impl<S: Datagram, const N: usize> MdnsSocket<S, N> {
    /// Sends one PTR query for instances of `service`; answers arrive
    /// through [`recv_response`](Self::recv_response) or, when browsing a
    /// kind, [`Browse::next`]. Dropping may or may not have sent the query;
    /// it is never sent twice.
    pub async fn query(&mut self, service: ServiceType<'_>) -> Result<(), MdnsError<S::Error>> {
        let id = self.next_id;
        self.next_id = match id.wrapping_add(1) {
            0 => 1,
            n => n,
        };
        self.outstanding = self.outstanding.sent(id);
        let mut query = [0u8; MAX_QUERY_LEN];
        let len = encode_query(id, service, &mut query)?;
        let packet = query.get(..len).ok_or(WireError::BufferTooSmall)?;
        self.socket
            .send_to(packet, self.destination)
            .await
            .map_err(MdnsError::Io)
    }

    /// Waits for the next mDNS response and copies it into `buf`; see the
    /// module docs. Cancel safe.
    pub async fn recv_response<'b>(
        &mut self,
        buf: &'b mut [u8],
    ) -> Result<Response<'b>, MdnsError<S::Error>> {
        for _ in 0..MAX_IGNORED_DATAGRAMS {
            if self.kept.is_none() {
                let (len, from) = self
                    .socket
                    .recv_from(&mut self.rx)
                    .await
                    .map_err(MdnsError::Io)?;
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
    ) -> Result<Option<Response<'b>>, MdnsError<S::Error>> {
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
    fn try_fill(&mut self) -> Result<bool, MdnsError<S::Error>> {
        if self.kept.is_some() {
            return Ok(true);
        }
        match self.socket.try_recv_from(&mut self.rx) {
            Ok(Some((len, from))) => {
                self.keep(len, from);
                Ok(true)
            }
            Ok(None) => Ok(false),
            Err(e) => Err(MdnsError::Io(e)),
        }
    }

    /// The next event from the kept datagram, if it has one; otherwise
    /// drops it. Datagram faults never surface here (see the module docs):
    /// the only error is the caller's buffer being too small.
    fn event_from_kept<'b>(
        &mut self,
        buf: &'b mut [u8],
    ) -> Result<Option<Event<'b>>, MdnsError<S::Error>> {
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

impl<S: Datagram, const N: usize> Browse for MdnsSocket<S, N> {
    type Error = MdnsError<S::Error>;

    /// Queries the kind's DNS-SD type; see the module docs.
    async fn browse(&mut self, filter: KeyFilter<'_>) -> Result<(), MdnsError<S::Error>> {
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
    async fn next<'b>(&mut self, buf: &'b mut [u8]) -> Result<Event<'b>, MdnsError<S::Error>> {
        for _ in 0..MAX_IGNORED_DATAGRAMS {
            if self.kept.is_none() {
                let (len, from) = self
                    .socket
                    .recv_from(&mut self.rx)
                    .await
                    .map_err(MdnsError::Io)?;
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

    fn try_next<'b>(
        &mut self,
        buf: &'b mut [u8],
    ) -> Result<Option<Event<'b>>, MdnsError<S::Error>> {
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

impl<S: Datagram, const N: usize> MdnsSocket<S, N> {
    /// True if the kept datagram has an event to report from the cursor
    /// on (or one too large for any buffer the caller may pass, which
    /// `event_from_kept` then reports); otherwise drops it.
    ///
    /// Separate from `event_from_kept` so the loops in `next` and
    /// `try_next` hand the caller's buffer out only on return.
    fn peek_event(&mut self) -> Result<bool, MdnsError<S::Error>> {
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
    //! The shell over the in-memory seam (no sockets, no runtime).
    extern crate std;
    use super::*;
    use crate::datagram::fake::{FakeErr, FakeNet, FakeUdp, poll_once, run};
    use crate::key::Key;
    use crate::mdns::wire::fixture;

    /// Small enough that a test can build an oversized datagram.
    const SMALL: usize = 512;

    fn responder() -> SocketAddr {
        "127.0.0.1:5353".parse().unwrap()
    }

    fn querier(net: &FakeNet) -> MdnsSocket<FakeUdp, SMALL> {
        MdnsSocket::new(
            net.udp(),
            SocketParams {
                destination: responder(),
                policy: OriginPolicy::Any,
                on_link: OnLink::new(),
                one_shot: false,
            },
        )
    }

    fn http() -> ServiceType<'static> {
        ServiceType::new("_http._tcp.local").unwrap()
    }

    #[test]
    fn fake_seam_delivers_a_response_and_sends_the_query() {
        let net = FakeNet::new();
        let mut q = querier(&net);
        run(q.query(http())).unwrap();
        let sent = net.take_sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].to, responder());
        assert!(!Message::parse(&sent[0].bytes).unwrap().is_response());

        net.deliver(&fixture::printer(7, 120), responder());
        let mut buf = [0u8; SMALL];
        let r = run(q.recv_response(&mut buf)).unwrap();
        assert_eq!(r.message().id(), 7);
        assert_eq!(q.last_source(), Some(responder()));
    }

    #[test]
    fn fake_seam_skips_oversized_datagrams() {
        let net = FakeNet::new();
        let mut q = querier(&net);
        // Exactly N bytes: the seam may have cut it, so it is skipped
        // even though its first bytes are a valid response.
        let mut big = fixture::printer(1, 120);
        big.resize(SMALL, 0);
        net.deliver(&big, responder());
        net.deliver(&fixture::printer(2, 120), responder());
        let mut buf = [0u8; SMALL];
        let r = run(q.recv_response(&mut buf)).unwrap();
        assert_eq!(r.message().id(), 2);
        // One byte under N is not oversized.
        let mut fits = fixture::printer(3, 120);
        fits.resize(SMALL - 1, 0);
        net.deliver(&fits, responder());
        assert_eq!(
            q.try_recv_response(&mut buf)
                .unwrap()
                .unwrap()
                .message()
                .id(),
            3
        );
    }

    #[test]
    fn fake_seam_keeps_a_response_for_one_retry() {
        let net = FakeNet::new();
        let mut q = querier(&net);
        let packet = fixture::printer(4, 120);
        net.deliver(&packet, responder());
        let mut small = [0u8; 16];
        let err = run(q.recv_response(&mut small)).unwrap_err();
        assert!(
            matches!(err, MdnsError::BufferTooSmall { needed, capacity: 16 } if needed == packet.len())
        );
        let mut buf = [0u8; SMALL];
        assert_eq!(
            q.try_recv_response(&mut buf)
                .unwrap()
                .unwrap()
                .message()
                .id(),
            4
        );
        assert!(q.try_recv_response(&mut buf).unwrap().is_none());
    }

    #[test]
    fn fake_seam_cancelled_recv_loses_no_response() {
        let net = FakeNet::new();
        let mut q = querier(&net);
        let mut buf = [0u8; SMALL];
        assert!(poll_once(q.recv_response(&mut buf)).is_pending());
        net.deliver(&fixture::printer(5, 120), responder());
        assert_eq!(run(q.recv_response(&mut buf)).unwrap().message().id(), 5);
    }

    #[test]
    fn fake_seam_browses_a_kind_to_an_event() {
        let net = FakeNet::new();
        let mut q = querier(&net);
        run(q.browse(KeyFilter::Exact(Key::from_static("http")))).unwrap();
        assert_eq!(net.take_sent().len(), 1);
        let mut buf = [0u8; 256];
        assert!(q.try_next(&mut buf).unwrap().is_none());
        net.deliver(&fixture::printer(1, 120), responder());
        let Event::Up(s) = run(q.next(&mut buf)).unwrap() else {
            panic!()
        };
        assert_eq!(s.instance, "My.Printer");
    }

    #[test]
    fn fake_seam_carries_the_socket_error() {
        let net = FakeNet::new();
        let mut q = querier(&net);
        net.fail_sends(true);
        assert!(matches!(run(q.query(http())), Err(MdnsError::Io(FakeErr))));
    }
}
