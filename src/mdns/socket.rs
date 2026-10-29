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
//!   TTL 255, carrying a fresh id (wrapping `u16`, advanced before the
//!   send).
//! * [`recv_response`](MdnsSocket::recv_response) waits for a datagram,
//!   keeps it in an internal [`MAX_MESSAGE_LEN`] buffer, and copies it into
//!   the caller's buffer. Queries and datagrams over [`MAX_MESSAGE_LEN`]
//!   are skipped; a datagram that does not parse is [`MdnsError::Wire`] and
//!   is dropped. After [`MAX_IGNORED_DATAGRAMS`] skipped datagrams in one
//!   call, every receive call returns [`MdnsError::IgnoredLimit`], so each
//!   is bounded in datagrams.
//! * A response that does not fit the caller's buffer is
//!   [`MdnsError::BufferTooSmall`] and is kept: a retry with a larger buffer
//!   returns it.
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
//! An event that does not fit the caller's buffer is
//! [`MdnsError::Record`] and is kept for the next call. Mixing the raw
//! receive methods with `next` hands the kept response to whichever comes
//! first.
//!
//! Tests point the socket at a loopback responder with
//! [`destination`](MdnsSocketBuilder::destination), so they put no
//! multicast traffic on the network.

use core::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::io;

use tokio::net::UdpSocket;

use super::wire::{
    HEADER_LEN, Instance, MAX_MESSAGE_LEN, MAX_NAME_LEN, Message, Response, ServiceType, WireError,
    encode_query,
};
use super::{DOMAIN, MDNS_GROUP_V4, MDNS_PORT};
use crate::discovery::record::{MAX_DNS_SD_TYPE_LEN, dns_sd_type_in, event_into};
use crate::discovery::{Browse, Endpoint, Event, RecordError, Service, ServiceKind};
use crate::key::{AsKey, KeyBuf, KeyFilter, MAX_KEY_LEN};
use crate::transport::copy_into;

/// Datagrams skipped in one receive call before it gives up.
pub const MAX_IGNORED_DATAGRAMS: usize = 64;

/// Largest query [`encode_query`] produces: header, name, type and class.
const MAX_QUERY_LEN: usize = HEADER_LEN + MAX_NAME_LEN + 4;

/// Failure of an [`MdnsSocket`].
#[derive(Debug)]
#[non_exhaustive]
pub enum MdnsError {
    /// The socket failed.
    Io(io::Error),
    /// A received datagram is not a valid DNS message, or a query could not
    /// be encoded.
    Wire(WireError),
    /// The response needs `needed` bytes but the buffer holds only
    /// `capacity`. The response is kept for the next call.
    BufferTooSmall {
        /// Bytes required.
        needed: usize,
        /// Bytes available.
        capacity: usize,
    },
    /// [`MAX_IGNORED_DATAGRAMS`] datagrams passed without a response (or,
    /// for [`Browse::next`], without an event).
    IgnoredLimit,
    /// [`Browse::browse`] with a prefix filter: DNS-SD browses one type.
    UnsupportedFilter,
    /// The kind has no DNS-SD type, or an event does not fit the caller's
    /// buffer (it is kept).
    Record(RecordError),
}

impl core::fmt::Display for MdnsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            MdnsError::Io(e) => write!(f, "mdns socket error: {e}"),
            MdnsError::Wire(e) => write!(f, "malformed mdns message: {e:?}"),
            MdnsError::BufferTooSmall { needed, capacity } => {
                write!(f, "buffer too small: need {needed} bytes, have {capacity}")
            }
            MdnsError::IgnoredLimit => {
                write!(f, "{MAX_IGNORED_DATAGRAMS} datagrams without a response")
            }
            MdnsError::UnsupportedFilter => f.write_str("mDNS browses an exact kind, not a prefix"),
            MdnsError::Record(e) => write!(f, "discovery record: {e:?}"),
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
/// `224.0.0.251:5353`, join no group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MdnsSocketBuilder {
    bind: SocketAddr,
    destination: SocketAddr,
    join: Option<Ipv4Addr>,
}

impl Default for MdnsSocketBuilder {
    fn default() -> Self {
        MdnsSocketBuilder {
            bind: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)),
            destination: SocketAddr::V4(SocketAddrV4::new(MDNS_GROUP_V4, MDNS_PORT)),
            join: None,
        }
    }
}

impl MdnsSocketBuilder {
    /// The local address to bind.
    pub const fn bind(self, bind: SocketAddr) -> Self {
        MdnsSocketBuilder { bind, ..self }
    }

    /// Where queries are sent.
    pub const fn destination(self, destination: SocketAddr) -> Self {
        MdnsSocketBuilder {
            destination,
            ..self
        }
    }

    /// Joins the IPv4 mDNS group on `interface` (`UNSPECIFIED` lets the OS
    /// choose), to receive announcements sent to the group.
    pub const fn join_multicast(self, interface: Ipv4Addr) -> Self {
        MdnsSocketBuilder {
            join: Some(interface),
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
        Ok(MdnsSocket {
            socket,
            destination: self.destination,
            next_id: 1,
            rx: [0; MAX_MESSAGE_LEN + 1],
            kept: None,
            browsing: None,
            cursor: 0,
        })
    }
}

/// What the kept datagram turned out to be.
enum Kept {
    /// Nothing is kept.
    Empty,
    /// It was skipped (a query, or oversized) and dropped.
    Ignored,
    /// A response of this length, still kept.
    Response(usize),
}

/// mDNS service discovery over UDP; see the module docs.
#[derive(Debug)]
pub struct MdnsSocket {
    socket: UdpSocket,
    destination: SocketAddr,
    next_id: u16,
    /// One byte over the largest message, to detect oversized datagrams.
    rx: [u8; MAX_MESSAGE_LEN + 1],
    /// Length of a received datagram in `rx` not yet delivered.
    kept: Option<usize>,
    /// The kind being browsed and its DNS-SD type.
    browsing: Option<Browsing>,
    /// Instances of the kept response already reported by `next`.
    cursor: usize,
}

/// Longest DNS-SD type text browsed: the type, a dot, the domain.
const MAX_TYPE_TEXT: usize = MAX_DNS_SD_TYPE_LEN + 1 + DOMAIN.len();

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

impl MdnsSocket {
    /// A builder with the default one-shot querier settings.
    pub fn builder() -> MdnsSocketBuilder {
        MdnsSocketBuilder::default()
    }

    /// The bound local address.
    pub fn local_addr(&self) -> Result<SocketAddr, MdnsError> {
        Ok(self.socket.local_addr()?)
    }

    /// Classifies the kept datagram, dropping it unless it is a response.
    fn classify(&mut self) -> Result<Kept, MdnsError> {
        let Some(len) = self.kept else {
            return Ok(Kept::Empty);
        };
        let packet = self.rx.get(..len).filter(|_| len <= MAX_MESSAGE_LEN);
        match packet.map(Message::parse) {
            Some(Ok(m)) if m.is_response() => Ok(Kept::Response(len)),
            Some(Err(e)) => {
                self.kept = None;
                Err(e.into())
            }
            _ => {
                self.kept = None;
                Ok(Kept::Ignored)
            }
        }
    }

    /// Copies the kept response of `len` bytes into `buf` and releases it.
    /// A response partly reported by [`Browse::next`] is handed over whole.
    fn deliver<'b>(&mut self, len: usize, buf: &'b mut [u8]) -> Result<Response<'b>, MdnsError> {
        let capacity = buf.len();
        let packet = self.rx.get(..len).ok_or(WireError::Truncated)?;
        let copied = copy_into(buf, packet).ok_or(MdnsError::BufferTooSmall {
            needed: len,
            capacity,
        })?;
        self.kept = None;
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
        self.next_id = id.wrapping_add(1);
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
                let (len, _) = self.socket.recv_from(&mut self.rx).await?;
                self.keep(len);
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

    /// Records a received datagram of `len` bytes.
    fn keep(&mut self, len: usize) {
        self.kept = Some(len);
        self.cursor = 0;
    }

    /// Makes sure a datagram is kept, without waiting; false if none has
    /// arrived.
    fn try_fill(&mut self) -> Result<bool, MdnsError> {
        if self.kept.is_some() {
            return Ok(true);
        }
        match self.socket.try_recv_from(&mut self.rx) {
            Ok((len, _)) => {
                self.keep(len);
                Ok(true)
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// The index of the next instance of the kept response to report, if
    /// any; otherwise drops the kept datagram (a query, a response without
    /// more instances of the browsed type, or nothing browsed).
    fn find_event(&mut self) -> Result<Option<usize>, MdnsError> {
        let Kept::Response(len) = self.classify()? else {
            return Ok(None);
        };
        let found = match &self.browsing {
            None => None,
            Some(b) => {
                let service = b.service()?;
                let packet = self.rx.get(..len).ok_or(WireError::Truncated)?;
                let response = Response::parse(packet)?.ok_or(WireError::Truncated)?;
                response
                    .instances(service)
                    .enumerate()
                    .skip(self.cursor)
                    .find(|(_, i)| !matches!(i, Instance::Unresolved(_)))
                    .map(|(i, _)| i)
            }
        };
        if found.is_none() {
            self.kept = None;
            self.cursor = 0;
        }
        Ok(found)
    }

    /// The event for instance `index` of the kept response, copied into
    /// `buf`.
    fn emit<'b>(&self, index: usize, buf: &'b mut [u8]) -> Result<Event<'b>, MdnsError> {
        let b = self.browsing.as_ref().ok_or(WireError::Truncated)?;
        let len = self.kept.ok_or(WireError::Truncated)?;
        let packet = self.rx.get(..len).ok_or(WireError::Truncated)?;
        let response = Response::parse(packet)?.ok_or(WireError::Truncated)?;
        let instance = response
            .instances(b.service()?)
            .nth(index)
            .ok_or(WireError::Truncated)?;
        let mut host = [0u8; MAX_NAME_LEN];
        let event = match instance {
            Instance::Resolved(i) => {
                let endpoint = match (i.ipv4, i.ipv6) {
                    (Some(ip), _) => Endpoint::Socket(SocketAddr::new(ip.into(), i.srv.port)),
                    (None, Some(ip)) => Endpoint::Socket(SocketAddr::new(ip.into(), i.srv.port)),
                    (None, None) => Endpoint::Host {
                        name: i.srv.target.write_text(&mut host)?,
                        port: i.srv.port,
                    },
                };
                Event::Up(Service {
                    kind: b.kind(),
                    instance: i.label()?,
                    node: None,
                    endpoint,
                    txt: i.txt.map_or(&[][..], |t| t.as_bytes()),
                    ttl_s: i.ttl,
                })
            }
            Instance::Gone(name) => Event::Down {
                kind: b.kind(),
                instance: core::str::from_utf8(name.first_label().unwrap_or_default())
                    .map_err(|_| WireError::NotUtf8)?,
            },
            Instance::Unresolved(_) => return Err(WireError::Truncated.into()),
        };
        event_into(buf, &event).map_err(MdnsError::Record)
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
        // `browsing` is a copy: the query borrows it, not `self`.
        let service = browsing.service()?;
        self.query(service).await
    }

    /// The next instance event; bounded by [`MAX_IGNORED_DATAGRAMS`]
    /// datagrams without one. Cancel safe.
    async fn next<'b>(&mut self, buf: &'b mut [u8]) -> Result<Event<'b>, MdnsError> {
        for _ in 0..MAX_IGNORED_DATAGRAMS {
            if self.kept.is_none() {
                let (len, _) = self.socket.recv_from(&mut self.rx).await?;
                self.keep(len);
            }
            if let Some(index) = self.find_event()? {
                let event = self.emit(index, buf)?;
                self.cursor = index + 1;
                return Ok(event);
            }
        }
        Err(MdnsError::IgnoredLimit)
    }

    fn try_next<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Event<'b>>, MdnsError> {
        for _ in 0..MAX_IGNORED_DATAGRAMS {
            if !self.try_fill()? {
                return Ok(None);
            }
            if let Some(index) = self.find_event()? {
                let event = self.emit(index, buf)?;
                self.cursor = index + 1;
                return Ok(Some(event));
            }
        }
        Err(MdnsError::IgnoredLimit)
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
}
