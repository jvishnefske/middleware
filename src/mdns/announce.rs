//! mDNS announcing over a tokio UDP socket (feature `mdns`):
//! [`MdnsAnnouncer`] implements [`Announce`].
//!
//! The decisions — probing, conflict renames, announcements, answers,
//! goodbyes — are the pure [`table::AnnounceTable`]; see its docs for the
//! RFC 6762 behaviour. This shell owns the socket and the clock (tokio's
//! monotonic `Instant`, in milliseconds since the announcer was built).
//!
//! # Driving
//!
//! * [`Announce::announce`] registers the service and runs the table until
//!   its first announcement is sent: three probes 250 ms apart, then the
//!   announcement 250 ms after the last, so about 750 ms on a quiet link.
//!   Meanwhile it reads the socket, answering queries for instances already
//!   announced and watching responses for conflicts. A conflict renames the
//!   instance (`name (2)` …, see [`instance_label`](MdnsAnnouncer::instance_label))
//!   and probes again; when every rename conflicts it is
//!   [`AnnounceError::Conflict`] and the instance is dropped. It reads at
//!   most [`MAX_DATAGRAMS_PER_CALL`] datagrams, else
//!   [`MdnsError::IgnoredLimit`] (the instance stays registered).
//! * The second announcement, 1 s later, and every answer to a query are
//!   sent by [`step`](MdnsAnnouncer::step), which the owner calls in a loop
//!   (`loop { announcer.step().await? }`, or in a `select!` with its other
//!   work). Without it the instance is announced once and queries are
//!   answered only during `announce` calls.
//! * [`Announce::withdraw`] removes the instance and multicasts its
//!   goodbye (PTR, SRV and TXT with TTL 0) if it had been announced;
//!   withdrawing an unknown instance does nothing.
//!
//! # Sockets
//!
//! By default the announcer binds `0.0.0.0:5353` with `SO_REUSEADDR` and
//! `SO_REUSEPORT` (so it can share the port with a system mDNS daemon),
//! joins `224.0.0.251` on the default interface, multicasts to
//! `224.0.0.251:5353` with IP TTL 255, and accepts on-link sources only
//! ([`OriginPolicy::OnLink`], see [`super::origin`]). Setting a
//! [`destination`](MdnsAnnouncerBuilder::destination) explicitly (tests on
//! loopback) also stops the group join and selects [`OriginPolicy::Any`];
//! both can be set again afterwards. Replies to legacy and QU queries go to
//! the query's source, unless that source is port 0 or an unspecified,
//! multicast or broadcast address ([`origin::reply_target`]).
//!
//! A received datagram cannot stop the announcer: a reply to it that cannot
//! be encoded or sent, or has no valid unicast target, is dropped and
//! counted ([`dropped_replies`](MdnsAnnouncer::dropped_replies)); `step`
//! and `announce` fail only on errors of the announcer's own socket and
//! messages (receiving, its probes, announcements and goodbyes).
//!
//! # Cancel safety
//!
//! A dropped `announce` leaves the instance registered: later `step` calls
//! finish probing and announce it, and `withdraw` removes it. A dropped
//! `step` or `withdraw` may or may not have sent its message, never twice:
//! the table moves on before the send is awaited.

pub mod table;

use core::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::time::Instant;

use super::origin::{self, Kind, OnLink, OriginPolicy};
use super::socket::MdnsError;
use super::wire::{MAX_MESSAGE_LEN, Message};
use super::{MDNS_GROUP_V4, MDNS_PORT};
use crate::discovery::{Announce, Service, ServiceKind};
pub use table::{AnnounceError, AnnounceTable, Destination, InstanceState, MAX_INSTANCES, Step};

/// Datagrams one [`Announce::announce`] call reads before giving up.
pub const MAX_DATAGRAMS_PER_CALL: usize = 1024;
/// Default SRV target host name.
pub const DEFAULT_HOST: &str = "middleware.local";

impl From<AnnounceError> for MdnsError {
    fn from(e: AnnounceError) -> Self {
        MdnsError::Announce(e)
    }
}

/// Configures an [`MdnsAnnouncer`]; see the module docs for the defaults.
#[derive(Debug, Clone)]
pub struct MdnsAnnouncerBuilder {
    bind: SocketAddr,
    destination: SocketAddr,
    join: Option<Ipv4Addr>,
    table: Result<AnnounceTable, AnnounceError>,
    policy: OriginPolicy,
    on_link: Option<OnLink>,
}

impl Default for MdnsAnnouncerBuilder {
    fn default() -> Self {
        MdnsAnnouncerBuilder {
            bind: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, MDNS_PORT)),
            destination: SocketAddr::V4(SocketAddrV4::new(MDNS_GROUP_V4, MDNS_PORT)),
            join: Some(Ipv4Addr::UNSPECIFIED),
            table: AnnounceTable::new(DEFAULT_HOST),
            policy: OriginPolicy::OnLink,
            on_link: None,
        }
    }
}

impl MdnsAnnouncerBuilder {
    /// The local address to bind (ignored by [`build_on`](Self::build_on)).
    pub fn bind(self, bind: SocketAddr) -> Self {
        MdnsAnnouncerBuilder { bind, ..self }
    }

    /// Where probes, announcements, multicast answers and goodbyes go.
    /// Also stops the group join and selects [`OriginPolicy::Any`].
    pub fn destination(self, destination: SocketAddr) -> Self {
        MdnsAnnouncerBuilder {
            destination,
            join: None,
            policy: OriginPolicy::Any,
            ..self
        }
    }

    /// Joins the IPv4 mDNS group on `interface` (`UNSPECIFIED`: the OS
    /// chooses) to hear queries.
    pub fn join_multicast(self, interface: Ipv4Addr) -> Self {
        MdnsAnnouncerBuilder {
            join: Some(interface),
            ..self
        }
    }

    /// The SRV target of [`Endpoint::Socket`](crate::discovery::Endpoint)
    /// services, which also gets their A/AAAA records. Default
    /// [`DEFAULT_HOST`]; give each machine its own (the host name is not
    /// probed). An invalid name (see [`AnnounceTable::new`]) fails
    /// [`build`](Self::build).
    pub fn host(self, host: &str) -> Self {
        MdnsAnnouncerBuilder {
            table: AnnounceTable::new(host),
            ..self
        }
    }

    /// Which sources are accepted; see [`super::origin`].
    pub fn origin_policy(self, policy: OriginPolicy) -> Self {
        MdnsAnnouncerBuilder { policy, ..self }
    }

    /// The on-link subnets for [`OriginPolicy::OnLink`] (default: read from
    /// the system when built, [`origin::system_on_link`]).
    pub fn on_link(self, on_link: OnLink) -> Self {
        MdnsAnnouncerBuilder {
            on_link: Some(on_link),
            ..self
        }
    }

    /// Binds the socket (with address and port reuse) and configures it.
    pub async fn build(self) -> Result<MdnsAnnouncer, MdnsError> {
        let socket = Socket::new(
            Domain::for_address(self.bind),
            Type::DGRAM,
            Some(Protocol::UDP),
        )?;
        socket.set_reuse_address(true)?;
        #[cfg(unix)]
        socket.set_reuse_port(true)?;
        socket.bind(&self.bind.into())?;
        self.build_on(socket.into())
    }

    /// Uses the already-bound `socket` instead of binding one.
    pub fn build_on(self, socket: std::net::UdpSocket) -> Result<MdnsAnnouncer, MdnsError> {
        let table = self.table?;
        socket.set_nonblocking(true)?;
        let socket = UdpSocket::from_std(socket)?;
        if self.destination.ip().is_multicast() {
            socket.set_multicast_ttl_v4(255)?;
        }
        if let Some(interface) = self.join {
            socket.join_multicast_v4(MDNS_GROUP_V4, interface)?;
        }
        let on_link = match (self.policy, self.on_link) {
            (_, Some(o)) => o,
            (OriginPolicy::OnLink, None) => origin::system_on_link(),
            (OriginPolicy::Any, None) => OnLink::new(),
        };
        Ok(MdnsAnnouncer {
            socket,
            destination: self.destination,
            policy: self.policy,
            on_link,
            table,
            epoch: Instant::now(),
            rx: [0; MAX_MESSAGE_LEN],
            tx: [0; MAX_MESSAGE_LEN],
            dropped_replies: 0,
        })
    }
}

/// An mDNS responder announcing DNS-SD services; see the module docs.
#[derive(Debug)]
pub struct MdnsAnnouncer {
    socket: UdpSocket,
    destination: SocketAddr,
    policy: OriginPolicy,
    on_link: OnLink,
    table: AnnounceTable,
    epoch: Instant,
    rx: [u8; MAX_MESSAGE_LEN],
    tx: [u8; MAX_MESSAGE_LEN],
    dropped_replies: u32,
}

impl MdnsAnnouncer {
    /// Replies to received queries that were dropped: they could not be
    /// encoded or sent, or their source was not a valid unicast target
    /// ([`origin::reply_target`]). Saturating.
    pub fn dropped_replies(&self) -> u32 {
        self.dropped_replies
    }

    /// A builder with the default responder settings.
    pub fn builder() -> MdnsAnnouncerBuilder {
        MdnsAnnouncerBuilder::default()
    }

    /// The bound local address.
    pub fn local_addr(&self) -> Result<SocketAddr, MdnsError> {
        Ok(self.socket.local_addr()?)
    }

    /// The table of instances (read only).
    pub fn table(&self) -> &AnnounceTable {
        &self.table
    }

    /// The label in use for the instance registered as `instance` of
    /// `kind`: `instance` itself, or its rename after a conflict.
    pub fn instance_label(&self, kind: ServiceKind<'_>, instance: &str) -> Option<&str> {
        self.table.label(kind, instance)
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn at(&self, ms: u64) -> Instant {
        self.epoch + core::time::Duration::from_millis(ms)
    }

    /// Applies a table transition synchronously (never across an await).
    fn transition<R>(&mut self, f: impl FnOnce(AnnounceTable, &mut [u8]) -> (AnnounceTable, R)) -> R {
        let table = core::mem::take(&mut self.table);
        let (table, r) = f(table, &mut self.tx);
        self.table = table;
        r
    }

    /// Sends every due probe and announcement; returns when the next is
    /// due. Bounded: each instance has at most one message due at a time.
    async fn send_due(&mut self) -> Result<Option<u64>, MdnsError> {
        for _ in 0..=MAX_INSTANCES {
            let now = self.now_ms();
            match self.transition(|t, out| t.poll(now, out))? {
                Step::Send(len) => self.send(len, self.destination).await?,
                Step::Idle(next) => return Ok(next),
            }
        }
        Ok(Some(self.now_ms()))
    }

    async fn send(&mut self, len: usize, to: SocketAddr) -> Result<(), MdnsError> {
        let packet = self.tx.get(..len).ok_or(MdnsError::BufferTooSmall {
            needed: len,
            capacity: MAX_MESSAGE_LEN,
        })?;
        self.socket.send_to(packet, to).await?;
        Ok(())
    }

    /// Waits for one datagram until `deadline_ms` (forever if none) and
    /// handles it. Returns whether a datagram was read.
    async fn receive_until(&mut self, deadline_ms: Option<u64>) -> Result<bool, MdnsError> {
        let received = match deadline_ms {
            Some(ms) => {
                let at = self.at(ms);
                match tokio::time::timeout_at(at, self.socket.recv_from(&mut self.rx)).await {
                    Ok(r) => r?,
                    Err(_) => return Ok(false),
                }
            }
            None => self.socket.recv_from(&mut self.rx).await?,
        };
        let (len, from) = received;
        self.handle_datagram(len, from).await;
        Ok(true)
    }

    /// Handles the datagram of `len` bytes from `from` in the receive
    /// buffer. Infallible: whatever goes wrong with it or its reply is per
    /// datagram (see the module docs).
    async fn handle_datagram(&mut self, len: usize, from: SocketAddr) {
        let Some(packet) = self.rx.get(..len) else {
            return;
        };
        let kind = match Message::parse(packet) {
            Ok(m) if m.is_response() => Kind::Response {
                id: m.id(),
                outstanding: None,
            },
            Ok(_) => Kind::Query,
            Err(_) => return,
        };
        if origin::check(self.policy, &self.on_link, from, kind).is_err() {
            return;
        }
        let now = self.now_ms();
        let table = core::mem::take(&mut self.table);
        let packet = self.rx.get(..len).unwrap_or_default();
        let (table, reply) = table.receive(packet, from.port(), now, &mut self.tx);
        self.table = table;
        // Failures caused by the received datagram are per datagram: the
        // reply is dropped and counted, the announcer carries on.
        let Ok(Some(reply)) = reply else {
            if reply.is_err() {
                self.dropped_replies = self.dropped_replies.saturating_add(1);
            }
            return;
        };
        let to = match reply.to {
            Destination::Multicast => Some(self.destination),
            Destination::Source => origin::reply_target(&self.on_link, from),
        };
        let sent = match to {
            Some(to) => self.send(reply.len, to).await.is_ok(),
            None => false,
        };
        if !sent {
            self.dropped_replies = self.dropped_replies.saturating_add(1);
        }
    }

    /// Sends what is due, then waits for the next datagram or timer and
    /// handles it; call in a loop to keep announcing and answering. Cancel
    /// safe (see the module docs).
    pub async fn step(&mut self) -> Result<(), MdnsError> {
        let next = self.send_due().await?;
        self.receive_until(next).await?;
        Ok(())
    }
}

impl Announce for MdnsAnnouncer {
    type Error = MdnsError;

    /// Probes and announces `service`; see the module docs.
    async fn announce(&mut self, service: &Service<'_>) -> Result<(), MdnsError> {
        let now = self.now_ms();
        self.transition(|t, _| t.register(service, now))?;
        for _ in 0..MAX_DATAGRAMS_PER_CALL {
            let next = self.send_due().await?;
            match self.table.state(service.kind, service.instance) {
                Some(InstanceState::Announced) => return Ok(()),
                Some(InstanceState::Pending) => {}
                Some(InstanceState::Conflicted) | None => {
                    self.transition(|t, out| t.withdraw(service.kind, service.instance, out))?;
                    return Err(AnnounceError::Conflict.into());
                }
            }
            self.receive_until(next).await?;
        }
        Err(MdnsError::IgnoredLimit)
    }

    /// Withdraws the instance with a goodbye; see the module docs.
    async fn withdraw(&mut self, kind: ServiceKind<'_>, instance: &str) -> Result<(), MdnsError> {
        if let Some(len) = self.transition(|t, out| t.withdraw(kind, instance, out))? {
            self.send(len, self.destination).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::discovery::{Browse, Endpoint, Event};
    use crate::dns::wire::{
        FLAG_AA, FLAG_QR, MessageWriter, RecordData, RecordOut, Section, ServiceType, TextName,
    };
    use crate::key::KeyFilter;
    use crate::mdns::socket::MdnsSocket;
    use std::time::Duration;

    const KIND: ServiceKind<'static> = ServiceKind::from_static("conf/svc");

    fn service() -> Service<'static> {
        Service {
            kind: KIND,
            instance: "unit-1",
            node: None,
            endpoint: Endpoint::Socket("127.0.0.1:7447".parse().unwrap()),
            txt: b"\x05ver=5",
            ttl_s: 120,
        }
    }

    async fn within<T>(f: impl Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(5), f)
            .await
            .unwrap()
    }

    /// An announcer and a browser aimed at each other on loopback.
    async fn pair() -> (MdnsAnnouncer, MdnsSocket) {
        let std_socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let browser = MdnsSocket::builder()
            .bind("127.0.0.1:0".parse().unwrap())
            .destination(std_socket.local_addr().unwrap())
            .build()
            .await
            .unwrap();
        let announcer = MdnsAnnouncer::builder()
            .destination(browser.local_addr().unwrap())
            .host("unit.local")
            .build_on(std_socket)
            .unwrap();
        (announcer, browser)
    }

    #[tokio::test]
    async fn announces_after_probing_and_answers_a_browse() {
        let (mut a, mut b) = pair().await;
        let start = tokio::time::Instant::now();
        within(a.announce(&service())).await.unwrap();
        let took = start.elapsed();
        assert!(took >= Duration::from_millis(700), "{took:?}");
        assert_eq!(a.instance_label(KIND, "unit-1"), Some("unit-1"));

        within(b.browse(KeyFilter::Exact(KIND.key()))).await.unwrap();
        let mut buf = [0u8; 512];
        // The announcement (after three probes the browser skips).
        let Event::Up(s) = within(b.next(&mut buf)).await.unwrap() else {
            panic!()
        };
        assert_eq!((s.instance, s.ttl_s), ("unit-1", 120));
        assert_eq!(s.endpoint, service().endpoint);
        // The browse query is answered by step, as a legacy unicast reply.
        within(a.step()).await.unwrap();
        let mut buf = [0u8; 512];
        let Event::Up(s) = within(b.next(&mut buf)).await.unwrap() else {
            panic!()
        };
        assert_eq!((s.instance, s.ttl_s), ("unit-1", 10));
        assert_eq!(s.txt, service().txt);

        // Withdraw: a goodbye.
        within(a.withdraw(KIND, "unit-1")).await.unwrap();
        let mut buf = [0u8; 512];
        assert_eq!(
            within(b.next(&mut buf)).await.unwrap(),
            Event::Down {
                kind: KIND,
                instance: "unit-1"
            }
        );
        assert!(a.table().is_empty());
    }

    #[tokio::test]
    async fn replies_that_cannot_go_out_are_dropped_not_fatal() {
        let (mut a, mut b) = pair().await;
        within(a.announce(&service())).await.unwrap();
        // A legacy PTR query, as if from spoofed sources: broadcast, port
        // 0, multicast, and an IPv6 source the IPv4 socket cannot reach
        // (the send fails).
        let mut q = [0u8; 128];
        let ty = ServiceType::new("_conf-svc._tcp.local").unwrap();
        let n = crate::dns::wire::encode_query(9, ty, &mut q).unwrap();
        for (i, from) in [
            "255.255.255.255:40000",
            "127.0.0.1:0",
            "224.0.0.251:40000",
            "[::1]:40000",
        ]
        .into_iter()
        .enumerate()
        {
            a.rx[..n].copy_from_slice(&q[..n]);
            a.handle_datagram(n, from.parse().unwrap()).await;
            assert_eq!(a.dropped_replies(), i as u32 + 1, "{from}");
        }
        // The announcer still works: a real browse is answered.
        within(b.browse(KeyFilter::Exact(KIND.key()))).await.unwrap();
        within(a.step()).await.unwrap();
        let mut buf = [0u8; 512];
        // The announcement first, then the answer to the browse.
        for _ in 0..2 {
            assert!(matches!(within(b.next(&mut buf)).await.unwrap(), Event::Up(_)));
        }
        assert_eq!(a.dropped_replies(), 4);
    }

    #[tokio::test]
    async fn a_rival_answer_while_probing_renames_the_instance() {
        let std_socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let rival = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut a = MdnsAnnouncer::builder()
            .destination(rival.local_addr().unwrap())
            .host("unit.local")
            .build_on(std_socket)
            .unwrap();
        let ty = ServiceType::new("_conf-svc._tcp.local").unwrap();
        let rival_task = async {
            let mut rx = [0u8; 512];
            let (_, from) = rival.recv_from(&mut rx).await.unwrap();
            let mut out = [0u8; 512];
            let srv = RecordOut {
                name: TextName::instance("unit-1", ty).unwrap(),
                ttl: 120,
                cache_flush: true,
                data: RecordData::Srv {
                    priority: 0,
                    weight: 0,
                    port: 1,
                    target: TextName::new("rival.local").unwrap(),
                },
            };
            let len = MessageWriter::new(&mut out, 0, FLAG_QR | FLAG_AA)
                .unwrap()
                .record(Section::Answer, &srv)
                .unwrap()
                .finish();
            rival.send_to(&out[..len], from).await.unwrap();
        };
        let s = service();
        let (announced, ()) = within(async { tokio::join!(a.announce(&s), rival_task) }).await;
        announced.unwrap();
        assert_eq!(a.instance_label(KIND, "unit-1"), Some("unit-1 (2)"));
        assert_eq!(
            a.table().state(KIND, "unit-1"),
            Some(InstanceState::Announced)
        );
    }
}
