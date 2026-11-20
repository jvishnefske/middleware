//! mDNS announcing over any [`Datagram`] socket and [`Clock`]:
//! [`MdnsAnnouncer`] implements [`Announce`].
//!
//! The decisions — probing, conflict renames, announcements, answers,
//! goodbyes — are the pure [`table::AnnounceTable`]; see its docs for the
//! RFC 6762 behaviour. This shell owns the socket and the clock; the table
//! sees milliseconds since the announcer was made.
//!
//! The shell is generic, `MdnsAnnouncer<S, C, N>`: `S` is the socket seam,
//! `C` the clock and `N` the receive and transmit buffer (see
//! [`STD_BUF`] and [`MCU_BUF`]). The bare name `MdnsAnnouncer` is the
//! tokio instantiation (feature `mdns`), which [`MdnsAnnouncer::builder`]
//! binds; any other socket and clock are wrapped with
//! [`MdnsAnnouncer::new`] and [`AnnouncerParams`].
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
//! By default the tokio announcer binds `0.0.0.0:5353` with `SO_REUSEADDR` and
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
//! A datagram of `N` bytes or more is oversized (the seam truncates to the
//! buffer) and ignored; a message of the announcer's own that does not fit
//! `N` bytes is [`MdnsError::BufferTooSmall`] (or the table's encoding
//! error), never sent truncated.
//!
//! # Cancel safety
//!
//! The wait for the next datagram or timer polls the socket first, then
//! the clock, and drops whichever loses; both are cancel safe, so a
//! dropped `step` loses no datagram. A dropped `announce` leaves the
//! instance registered: later `step` calls finish probing and announce it,
//! and `withdraw` removes it. A dropped `step` or `withdraw` may or may not
//! have sent its message, never twice: the table moves on before the send
//! is awaited.

pub mod table;

#[cfg(feature = "mdns")]
mod host;
#[cfg(feature = "mdns")]
pub use host::MdnsAnnouncerBuilder;

use core::net::SocketAddr;

#[cfg(doc)]
use super::MCU_BUF;
use super::error::MdnsError;
use super::origin::{self, Kind, OnLink, OriginPolicy};
use super::race::{Raced, race};
use super::wire::Message;
use super::{DefaultClock, DefaultSocket, STD_BUF};
use crate::clock::Clock;
use crate::datagram::Datagram;
use crate::discovery::{Announce, Service, ServiceKind};
pub use table::{AnnounceError, AnnounceTable, Destination, InstanceState, MAX_INSTANCES, Step};

/// Datagrams one [`Announce::announce`] call reads before giving up.
pub const MAX_DATAGRAMS_PER_CALL: usize = 1024;
/// Default SRV target host name.
pub const DEFAULT_HOST: &str = "middleware.local";

/// Where an [`MdnsAnnouncer`] made with [`MdnsAnnouncer::new`] sends and
/// what it accepts.
///
/// On a microcontroller, `on_link` must list the LAN's subnet
/// ([`OnLink::with`]) when `policy` is [`OriginPolicy::OnLink`]: without
/// `std` the system's routes are not read, so [`OnLink::new`] alone
/// accepts loopback and link-local sources only and every query from a
/// peer on a DHCP LAN would be refused.
#[derive(Debug, Clone)]
pub struct AnnouncerParams {
    /// Where probes, announcements, multicast answers and goodbyes go
    /// (`224.0.0.251:5353` on a LAN).
    pub destination: SocketAddr,
    /// Which sources are accepted; see [`super::origin`].
    pub policy: OriginPolicy,
    /// The on-link subnets for [`OriginPolicy::OnLink`], also used to
    /// refuse unicast replies to subnet broadcast addresses.
    pub on_link: OnLink,
}

/// An mDNS responder announcing DNS-SD services; see the module docs.
#[derive(Debug)]
pub struct MdnsAnnouncer<S = DefaultSocket, C = DefaultClock, const N: usize = STD_BUF> {
    socket: S,
    clock: C,
    /// The clock's reading when the announcer was made.
    epoch: u64,
    destination: SocketAddr,
    policy: OriginPolicy,
    on_link: OnLink,
    table: AnnounceTable,
    rx: [u8; N],
    tx: [u8; N],
    dropped_replies: u32,
}

impl<S: Datagram, C: Clock, const N: usize> MdnsAnnouncer<S, C, N> {
    /// An announcer over `socket`, which is already bound (to port 5353
    /// and a member of the mDNS group, to hear queries), with `table`'s
    /// host name. Time starts now on `clock`. `N` must be over 1.
    pub fn new(socket: S, mut clock: C, table: AnnounceTable, params: AnnouncerParams) -> Self {
        const { assert!(N > 1, "an mDNS buffer needs room for a datagram") };
        let epoch = clock.now_ms();
        MdnsAnnouncer {
            socket,
            clock,
            epoch,
            destination: params.destination,
            policy: params.policy,
            on_link: params.on_link,
            table,
            rx: [0; N],
            tx: [0; N],
            dropped_replies: 0,
        }
    }

    /// Replies to received queries that were dropped: they could not be
    /// encoded or sent, or their source was not a valid unicast target
    /// ([`origin::reply_target`]). Saturating.
    pub fn dropped_replies(&self) -> u32 {
        self.dropped_replies
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

    /// Milliseconds since the announcer was made.
    fn now_ms(&mut self) -> u64 {
        self.clock.now_ms().saturating_sub(self.epoch)
    }

    /// Applies a table transition synchronously (never across an await).
    fn transition<R>(
        &mut self,
        f: impl FnOnce(AnnounceTable, &mut [u8]) -> (AnnounceTable, R),
    ) -> R {
        let table = core::mem::take(&mut self.table);
        let (table, r) = f(table, &mut self.tx);
        self.table = table;
        r
    }

    /// Sends every due probe and announcement; returns when the next is
    /// due. Bounded: each instance has at most one message due at a time.
    async fn send_due(&mut self) -> Result<Option<u64>, MdnsError<S::Error>> {
        for _ in 0..=MAX_INSTANCES {
            let now = self.now_ms();
            match self.transition(|t, out| t.poll(now, out))? {
                Step::Send(len) => self.send(len, self.destination).await?,
                Step::Idle(next) => return Ok(next),
            }
        }
        let now = self.now_ms();
        Ok(Some(now))
    }

    async fn send(&mut self, len: usize, to: SocketAddr) -> Result<(), MdnsError<S::Error>> {
        let packet = self.tx.get(..len).ok_or(MdnsError::BufferTooSmall {
            needed: len,
            capacity: N,
        })?;
        self.socket.send_to(packet, to).await.map_err(MdnsError::Io)
    }

    /// Waits for one datagram until `deadline_ms` (forever if none) and
    /// handles it. Returns whether a datagram was read. The datagram is
    /// polled before the timer, so one that is ready is never lost to it.
    async fn receive_until(
        &mut self,
        deadline_ms: Option<u64>,
    ) -> Result<bool, MdnsError<S::Error>> {
        let received = match deadline_ms {
            Some(ms) => {
                let deadline = self.epoch.saturating_add(ms);
                let recv = self.socket.recv_from(&mut self.rx);
                match race(recv, self.clock.sleep_until_ms(deadline)).await {
                    Raced::First(r) => r.map_err(MdnsError::Io)?,
                    Raced::Second(()) => return Ok(false),
                }
            }
            None => self
                .socket
                .recv_from(&mut self.rx)
                .await
                .map_err(MdnsError::Io)?,
        };
        let (len, from) = received;
        self.handle_datagram(len, from).await;
        Ok(true)
    }

    /// Handles the datagram of `len` bytes from `from` in the receive
    /// buffer. Infallible: whatever goes wrong with it or its reply is per
    /// datagram (see the module docs).
    async fn handle_datagram(&mut self, len: usize, from: SocketAddr) {
        // N bytes or more: the seam may have cut it.
        if len >= N {
            return;
        }
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
    pub async fn step(&mut self) -> Result<(), MdnsError<S::Error>> {
        let next = self.send_due().await?;
        self.receive_until(next).await?;
        Ok(())
    }
}

impl<S: Datagram, C: Clock, const N: usize> Announce for MdnsAnnouncer<S, C, N> {
    type Error = MdnsError<S::Error>;

    /// Probes and announces `service`; see the module docs.
    async fn announce(&mut self, service: &Service<'_>) -> Result<(), MdnsError<S::Error>> {
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
    async fn withdraw(
        &mut self,
        kind: ServiceKind<'_>,
        instance: &str,
    ) -> Result<(), MdnsError<S::Error>> {
        if let Some(len) = self.transition(|t, out| t.withdraw(kind, instance, out))? {
            self.send(len, self.destination).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! The shell over the in-memory seam and a virtual clock.
    extern crate std;
    use super::*;
    use crate::datagram::fake::{FakeClock, FakeErr, FakeNet, FakeUdp, Sent, poll_once, run};
    use crate::discovery::Endpoint;
    use crate::dns::wire::{
        FLAG_AA, FLAG_QR, MessageWriter, RecordData, RecordOut, Section, ServiceType, TextName,
        encode_query,
    };
    use crate::mdns::MCU_BUF;
    use std::vec::Vec;

    type Fake = MdnsAnnouncer<FakeUdp, FakeClock, MCU_BUF>;

    const KIND: ServiceKind<'static> = ServiceKind::from_static("conf/svc");

    fn group() -> SocketAddr {
        "224.0.0.251:5353".parse().unwrap()
    }

    fn peer() -> SocketAddr {
        "127.0.0.1:40000".parse().unwrap()
    }

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

    fn announcer(net: &FakeNet) -> Fake {
        MdnsAnnouncer::new(
            net.udp(),
            net.clock(),
            AnnounceTable::new("unit.local").unwrap(),
            AnnouncerParams {
                destination: group(),
                policy: OriginPolicy::Any,
                on_link: OnLink::new(),
            },
        )
    }

    /// An announcer whose instance is announced, with what it sent so far
    /// taken.
    fn announced(net: &FakeNet) -> Fake {
        let mut a = announcer(net);
        run(a.announce(&service())).unwrap();
        net.take_sent();
        a
    }

    /// As [`announced`], with the second announcement sent too: nothing is
    /// due any more, so `step` waits on the socket alone.
    fn settled(net: &FakeNet) -> Fake {
        let mut a = announced(net);
        // The timer to 1750 ms, then the second announcement (the step is
        // dropped while it waits for a datagram).
        run(a.step()).unwrap();
        assert!(poll_once(a.step()).is_pending());
        assert_eq!(net.take_sent().len(), 1);
        a
    }

    /// A legacy (one-shot) PTR query for the service's type.
    fn query() -> Vec<u8> {
        let mut q = [0u8; 128];
        let ty = ServiceType::new("_conf-svc._tcp.local").unwrap();
        let n = encode_query(9, ty, &mut q).unwrap();
        q[..n].into()
    }

    #[test]
    fn fake_seam_probes_then_announces_on_the_clock() {
        let net = FakeNet::new();
        let mut a = announcer(&net);
        run(a.announce(&service())).unwrap();
        let sent = net.take_sent();
        let times: Vec<u64> = sent.iter().map(|s| s.at_ms).collect();
        assert_eq!(times, [0, 250, 500, 750]);
        assert!(sent.iter().all(|s| s.to == group()));
        let parsed: Vec<_> = sent
            .iter()
            .map(|s| Message::parse(&s.bytes).unwrap())
            .collect();
        assert!(
            parsed[..3].iter().all(|m| !m.is_response()),
            "probes are queries"
        );
        assert!(parsed[3].is_response(), "then the announcement");
        assert_eq!(
            a.table().state(KIND, "unit-1"),
            Some(InstanceState::Announced)
        );
        // The second announcement, 1 s later, is sent by step: the first
        // waits out the timer, the second sends it (then waits for a
        // datagram that never comes, so it is polled once).
        run(a.step()).unwrap();
        assert!(net.take_sent().is_empty());
        assert!(poll_once(a.step()).is_pending());
        let sent = net.take_sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].at_ms, 1750);
    }

    #[test]
    fn fake_seam_answers_a_legacy_query_by_unicast() {
        let net = FakeNet::new();
        let mut a = announced(&net);
        net.deliver(&query(), peer());
        run(a.step()).unwrap();
        let sent = net.take_sent();
        let reply = sent
            .iter()
            .find(|s| s.to == peer())
            .expect("a unicast reply");
        let m = Message::parse(&reply.bytes).unwrap();
        assert!(m.is_response());
        assert_eq!(m.id(), 9, "a legacy reply echoes the query id");
        assert_eq!(a.dropped_replies(), 0);
    }

    #[test]
    fn fake_seam_drops_and_counts_a_reply_to_port_zero() {
        let net = FakeNet::new();
        let mut a = announced(&net);
        net.deliver(&query(), "127.0.0.1:0".parse().unwrap());
        run(a.step()).unwrap();
        assert!(net.take_sent().iter().all(|s| s.to.port() != 0));
        assert_eq!(a.dropped_replies(), 1);
    }

    #[test]
    fn fake_seam_ignores_oversized_datagrams() {
        let net = FakeNet::new();
        let mut a = announced(&net);
        // A valid query padded to N bytes: the seam may have cut it.
        let mut big = query();
        big.resize(MCU_BUF, 0);
        net.deliver(&big, peer());
        run(a.step()).unwrap();
        assert!(net.take_sent().iter().all(|s| s.to != peer()));
        assert_eq!((a.dropped_replies(), net.queued()), (0, 0));
        // One byte under N is a datagram like any other.
        let mut fits = query();
        fits.resize(MCU_BUF - 1, 0);
        net.deliver(&fits, peer());
        run(a.step()).unwrap();
        assert!(net.take_sent().iter().any(|s| s.to == peer()));
    }

    #[test]
    fn fake_seam_send_errors_of_own_messages_are_io() {
        let net = FakeNet::new();
        let mut a = announcer(&net);
        net.fail_sends(true);
        assert!(matches!(
            run(a.announce(&service())),
            Err(MdnsError::Io(FakeErr))
        ));
        // The instance stays registered (as for a dropped announce).
        assert_eq!(
            a.table().state(KIND, "unit-1"),
            Some(InstanceState::Pending)
        );
        net.fail_sends(false);
        let mut a = announced(&net);
        net.fail_sends(true);
        assert!(matches!(
            run(a.withdraw(KIND, "unit-1")),
            Err(MdnsError::Io(FakeErr))
        ));
    }

    #[test]
    fn fake_seam_send_errors_of_replies_are_counted_not_fatal() {
        let net = FakeNet::new();
        let mut a = settled(&net);
        net.fail_sends(true);
        net.deliver(&query(), peer());
        run(a.step()).unwrap();
        assert_eq!(a.dropped_replies(), 1);
    }

    #[test]
    fn fake_seam_cancelled_step_loses_no_datagram() {
        let net = FakeNet::new();
        let mut a = settled(&net);
        // Nothing is due: step waits on the socket alone. Drop it there.
        assert!(poll_once(a.step()).is_pending());
        net.deliver(&query(), peer());
        run(a.step()).unwrap();
        assert!(net.take_sent().iter().any(|s| s.to == peer()));
    }

    #[test]
    fn fake_seam_takes_a_ready_datagram_before_a_due_timer() {
        let net = FakeNet::new();
        let mut a = announced(&net);
        // The second announcement is due at 1750; the query is ready now.
        net.deliver(&query(), peer());
        run(a.step()).unwrap();
        let sent = net.take_sent();
        assert!(sent.iter().any(|s| s.to == peer()));
        assert!(net.now_ms() < 1750, "the timer did not fire");
    }

    #[test]
    fn fake_seam_a_datagram_queued_when_the_timer_is_due_is_handled_once() {
        let net = FakeNet::new();
        let mut a = announced(&net);
        // The second announcement is due at 1750: the query arrives then.
        net.advance_to(1750);
        net.deliver(&query(), peer());
        run(a.step()).unwrap();
        // Nothing more is due or queued: the next step waits.
        assert!(poll_once(a.step()).is_pending());
        let sent = net.take_sent();
        assert_eq!(sent.iter().filter(|s| s.to == peer()).count(), 1);
        assert_eq!(sent.iter().filter(|s| s.to == group()).count(), 1);
        assert_eq!(net.queued(), 0);
    }

    #[test]
    fn fake_seam_a_rival_answer_while_probing_renames_the_instance() {
        let net = FakeNet::new();
        let mut a = announcer(&net);
        let ty = ServiceType::new("_conf-svc._tcp.local").unwrap();
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
        let mut out = [0u8; 512];
        let len = MessageWriter::new(&mut out, 0, FLAG_QR | FLAG_AA)
            .unwrap()
            .record(Section::Answer, &srv)
            .unwrap()
            .finish();
        net.deliver(&out[..len], "127.0.0.1:5353".parse().unwrap());
        run(a.announce(&service())).unwrap();
        assert_eq!(a.instance_label(KIND, "unit-1"), Some("unit-1 (2)"));
        let last: Option<Sent> = net.take_sent().pop();
        assert!(Message::parse(&last.unwrap().bytes).unwrap().is_response());
    }
}
