//! The tokio instantiation of [`MdnsAnnouncer`] (feature `mdns`):
//! [`MdnsAnnouncerBuilder`] binds a tokio UDP socket with `socket2` and
//! pairs it with a [`TokioClock`].

use core::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;

use super::{AnnounceError, AnnounceTable, AnnouncerParams, DEFAULT_HOST, MdnsAnnouncer};
use crate::mdns::host::{TokioClock, TokioUdp};
use crate::mdns::origin::{self, OnLink, OriginPolicy};
use crate::mdns::socket::MdnsError;
use crate::mdns::{MDNS_GROUP_V4, MDNS_PORT};

/// Configures an [`MdnsAnnouncer`]; see the module docs for the defaults.
#[derive(Debug, Clone)]
pub struct MdnsAnnouncerBuilder {
    bind: SocketAddr,
    destination: SocketAddr,
    join: Option<Ipv4Addr>,
    multicast_if: Option<Ipv4Addr>,
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
            multicast_if: None,
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

    /// Sends multicast out of the interface with address `interface`
    /// (`IP_MULTICAST_IF`) instead of the one the routing table picks for
    /// `224.0.0.251`; for a host with several LANs, or none with a default
    /// route. Default: unset.
    pub fn multicast_interface(self, interface: Ipv4Addr) -> Self {
        MdnsAnnouncerBuilder {
            multicast_if: Some(interface),
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
        if let Some(interface) = self.multicast_if {
            socket2::SockRef::from(&socket).set_multicast_if_v4(&interface)?;
        }
        if let Some(interface) = self.join {
            socket.join_multicast_v4(MDNS_GROUP_V4, interface)?;
        }
        let on_link = match (self.policy, self.on_link) {
            (_, Some(o)) => o,
            (OriginPolicy::OnLink, None) => origin::system_on_link(),
            (OriginPolicy::Any, None) => OnLink::new(),
        };
        let params = AnnouncerParams {
            destination: self.destination,
            policy: self.policy,
            on_link,
        };
        Ok(MdnsAnnouncer::new(
            TokioUdp(socket),
            TokioClock::new(),
            table,
            params,
        ))
    }
}

impl MdnsAnnouncer {
    /// A builder with the default responder settings.
    pub fn builder() -> MdnsAnnouncerBuilder {
        MdnsAnnouncerBuilder::default()
    }
}

impl<C, const N: usize> MdnsAnnouncer<TokioUdp, C, N> {
    /// The bound local address.
    pub fn local_addr(&self) -> Result<SocketAddr, MdnsError> {
        Ok(self.socket.local_addr()?)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::super::InstanceState;
    use super::*;
    use crate::discovery::Announce;
    use crate::discovery::{Browse, Endpoint, Event, Service, ServiceKind};
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

        within(b.browse(KeyFilter::Exact(KIND.key())))
            .await
            .unwrap();
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
    async fn multicast_interface_picks_the_egress_interface() {
        let build = |b: MdnsAnnouncerBuilder| {
            b.bind("127.0.0.1:0".parse().unwrap())
                .destination("127.0.0.1:9".parse().unwrap())
                .host("unit.local")
                .build()
        };
        let egress = |a: &MdnsAnnouncer| {
            socket2::SockRef::from(a.socket.get_ref())
                .multicast_if_v4()
                .unwrap()
        };
        // Unset: the routing table chooses.
        let a = build(MdnsAnnouncer::builder()).await.unwrap();
        assert_eq!(egress(&a), Ipv4Addr::UNSPECIFIED);
        let a = build(MdnsAnnouncer::builder().multicast_interface(Ipv4Addr::LOCALHOST))
            .await
            .unwrap();
        assert_eq!(egress(&a), Ipv4Addr::LOCALHOST);
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
        within(b.browse(KeyFilter::Exact(KIND.key())))
            .await
            .unwrap();
        within(a.step()).await.unwrap();
        let mut buf = [0u8; 512];
        // The announcement first, then the answer to the browse.
        for _ in 0..2 {
            assert!(matches!(
                within(b.next(&mut buf)).await.unwrap(),
                Event::Up(_)
            ));
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
