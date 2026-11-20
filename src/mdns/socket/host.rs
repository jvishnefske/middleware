//! The tokio instantiation of [`MdnsSocket`] (feature `mdns`):
//! [`MdnsSocketBuilder`] binds a tokio UDP socket and wraps it in
//! [`TokioUdp`].

use core::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use tokio::net::UdpSocket;

use super::{MdnsError, MdnsSocket, SocketParams};
use crate::mdns::host::TokioUdp;
use crate::mdns::origin::{self, OnLink, OriginPolicy};
use crate::mdns::{MDNS_GROUP_V4, MDNS_PORT};

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
        let params = SocketParams {
            destination: self.destination,
            policy: self.policy,
            on_link,
            one_shot,
        };
        Ok(MdnsSocket::new(TokioUdp(socket), params))
    }
}

impl MdnsSocket {
    /// A builder with the default one-shot querier settings.
    pub fn builder() -> MdnsSocketBuilder {
        MdnsSocketBuilder::default()
    }
}

impl<const N: usize> MdnsSocket<TokioUdp, N> {
    /// The bound local address.
    pub fn local_addr(&self) -> Result<SocketAddr, MdnsError> {
        Ok(self.socket.local_addr()?)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::super::{MAX_IGNORED_DATAGRAMS, MAX_SHORT_BUFFER_RETRIES};
    use super::*;
    use crate::discovery::{Browse, Endpoint, Event, RecordError};
    use crate::key::{Key, KeyFilter};
    use crate::mdns::wire::{
        Instance, MAX_MESSAGE_LEN, Message, ServiceType, TYPE_PTR, WireError, encode_query, fixture,
    };
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
        within(async { while q.socket.0.peek_from(&mut peek).await.is_err() {} }).await;
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
        for packet in [
            &bad[..],
            truncated,
            &huge,
            &http_response(4, "good", b"", 60),
        ] {
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
