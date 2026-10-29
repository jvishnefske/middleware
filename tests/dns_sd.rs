//! Unicast DNS-SD (`dns::unicast`, feature `dns-sd`) against a stub DNS
//! server on 127.0.0.1 (`tests/dns_sd/stub.rs`): headless Services whose
//! pods come and go across TTL expiries, targets without glue, NXDOMAIN,
//! mismatched ids and spoofed sources, truncated and failed replies, prefix
//! browsing, and the discovery conformance cases.
//!
//! Network conduct: every query goes to the stub on loopback; nothing here
//! reads `/etc/resolv.conf` or reaches a real DNS server.
#![cfg(feature = "dns-sd")]

mod conformance;
#[path = "dns_sd/stub.rs"]
mod stub;

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use conformance::{DiscoveryFixture, discovery, prompt};
use middleware::discovery::{Announce, Browse, Endpoint, Event, Service, ServiceKind};
use middleware::dns::unicast::{
    AnswerError, MapError, ResolvConf, Schedule, ServiceMap, ServiceRef, UnicastDnsSd,
    UnicastError, query_id,
};
use middleware::dns::wire::{FLAG_RD, TYPE_A, TYPE_AAAA, TYPE_SRV};
use middleware::key::{Key, KeyFilter};
use stub::{Mode, Pod, Stub};

const CAM: ServiceKind<'static> = ServiceKind::from_static("cam/jpeg");
const RAW: ServiceKind<'static> = ServiceKind::from_static("cam/raw");
const BUS: ServiceKind<'static> = ServiceKind::from_static("mw/bus");

const CAM_SRV: &str = "_http._tcp.cam.media.svc.cluster.local";
const CAM_PODS: &str = "cam.media.svc.cluster.local";
const RAW_SRV: &str = "_cam-raw._tcp.raw.media.svc.cluster.local";
const RAW_PODS: &str = "raw.media.svc.cluster.local";

/// Polls every 20..=50 ms, whatever the TTL.
const FAST: Schedule = Schedule::new(20, 50).empty_interval(20).error_interval(20);

fn map() -> ServiceMap {
    ServiceMap::new()
        .with(
            CAM,
            ServiceRef {
                service: "cam",
                namespace: "media",
                port_name: "http",
            },
        )
        .unwrap()
        .with_kind_port(RAW, "raw", "media")
        .unwrap()
        .with_kind_port(BUS, "bus", "infra")
        .unwrap()
}

async fn browser(stub: &Stub, schedule: Schedule) -> UnicastDnsSd {
    UnicastDnsSd::builder(map())
        .nameserver(stub.addr)
        .bind("127.0.0.1".parse().unwrap())
        .timeout(Duration::from_millis(300))
        .attempts(2)
        .schedule(schedule)
        .build()
        .await
        .expect("build")
}

/// An event, owned: ("up"/"down", kind, instance, endpoint text, txt, ttl).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Owned {
    up: bool,
    kind: String,
    instance: String,
    endpoint: String,
    ttl_s: u32,
}

fn owned(e: &Event<'_>) -> Owned {
    match e {
        Event::Up(s) => Owned {
            up: true,
            kind: s.kind.as_str().into(),
            instance: s.instance.into(),
            endpoint: match s.endpoint {
                Endpoint::Socket(a) => a.to_string(),
                Endpoint::Host { name, port } => format!("{name}:{port}"),
                other => format!("{other:?}"),
            },
            ttl_s: s.ttl_s,
        },
        Event::Down { kind, instance } => Owned {
            up: false,
            kind: kind.as_str().into(),
            instance: (*instance).into(),
            endpoint: String::new(),
            ttl_s: 0,
        },
    }
}

async fn next(b: &mut UnicastDnsSd) -> Owned {
    let mut buf = [0u8; 512];
    let e = prompt("event", b.next(&mut buf)).await.expect("next");
    owned(&e)
}

async fn events(b: &mut UnicastDnsSd, n: usize) -> Vec<Owned> {
    let mut out = Vec::new();
    for _ in 0..n {
        out.push(next(b).await);
    }
    out
}

fn up(instance: &str, endpoint: &str, ttl_s: u32) -> Owned {
    Owned {
        up: true,
        kind: CAM.as_str().into(),
        instance: instance.into(),
        endpoint: endpoint.into(),
        ttl_s,
    }
}

fn down(instance: &str) -> Owned {
    Owned {
        up: false,
        kind: CAM.as_str().into(),
        instance: instance.into(),
        endpoint: String::new(),
        ttl_s: 0,
    }
}

fn pod(i: u8, ttl: u32) -> Pod {
    Pod::v4(&format!("cam-{i}"), CAM_PODS, [10, 0, 0, i], 8080, ttl)
}

fn sorted(v: Vec<Owned>) -> BTreeSet<Owned> {
    v.into_iter().collect()
}

/// A headless Service with a 1 s TTL: 2 pods, then 3, then 1. Each change
/// is seen at the next re-resolution, due at 80 % of the TTL; at that time
/// the unchanged pods are reported again (a refresh), so a Directory fed
/// with the events keeps them.
#[tokio::test]
async fn headless_service_pods_come_and_go_across_ttl_expiries() {
    let stub = Stub::start().await;
    stub.set_pods(CAM_SRV, vec![pod(0, 1), pod(1, 1)]).await;
    let mut b = browser(&stub, Schedule::new(100, 60_000)).await;
    b.browse(KeyFilter::Exact(CAM.key())).await.expect("browse");
    let resolved = Instant::now();
    assert_eq!(
        sorted(events(&mut b, 2).await),
        sorted(vec![
            up("cam-0", "10.0.0.0:8080", 1),
            up("cam-1", "10.0.0.1:8080", 1)
        ])
    );
    let mut buf = [0u8; 512];
    assert_eq!(b.try_next(&mut buf).ok().flatten(), None);

    stub.set_pods(CAM_SRV, vec![pod(0, 1), pod(1, 1), pod(2, 1)])
        .await;
    assert_eq!(
        sorted(events(&mut b, 3).await),
        sorted(vec![
            up("cam-0", "10.0.0.0:8080", 1),
            up("cam-1", "10.0.0.1:8080", 1),
            up("cam-2", "10.0.0.2:8080", 1),
        ])
    );
    let waited = resolved.elapsed();
    assert!(
        waited >= Duration::from_millis(750),
        "re-resolved after {waited:?}, before 80 % of the 1 s TTL"
    );
    let srv_queries = |seen: &[stub::Seen]| seen.iter().filter(|s| s.qtype == TYPE_SRV).count();
    assert_eq!(srv_queries(&stub.seen().await), 2);

    stub.set_pods(CAM_SRV, vec![pod(1, 1)]).await;
    let e = events(&mut b, 3).await;
    assert_eq!(
        sorted(e[..2].to_vec()),
        sorted(vec![down("cam-0"), down("cam-2")])
    );
    assert_eq!(e[2], up("cam-1", "10.0.0.1:8080", 1), "downs come first");
    assert_eq!(srv_queries(&stub.seen().await), 3);
    let known: Vec<_> = b
        .endpoints(CAM)
        .unwrap()
        .map(|s| s.instance.to_owned())
        .collect();
    assert_eq!(known, ["cam-1"]);
}

/// Without glue every target is resolved: A first, AAAA when A has no
/// data, and a target without addresses is reported by host name. Every
/// query has RD, an EDNS(0) OPT for 1232 bytes and its own id.
#[tokio::test]
async fn targets_without_glue_are_resolved_with_a_then_aaaa() {
    let stub = Stub::start().await;
    stub.set_glue(false).await;
    let v6 = Pod {
        v4: None,
        v6: Some("fd00::7".parse().unwrap()),
        ..pod(7, 30)
    };
    let bare = Pod {
        v4: None,
        ..pod(9, 30)
    };
    stub.set_pods(CAM_SRV, vec![pod(1, 30), v6, bare]).await;
    let mut b = browser(&stub, FAST).await;
    b.browse(KeyFilter::Exact(CAM.key())).await.expect("browse");
    assert_eq!(
        events(&mut b, 3).await,
        [
            up("cam-1", "10.0.0.1:8080", 30),
            up("cam-7", "[fd00::7]:8080", 30),
            up("cam-9", &format!("cam-9.{CAM_PODS}:8080"), 30),
        ]
    );
    let seen = stub.seen().await;
    let asked: Vec<_> = seen.iter().map(|s| (s.name.as_str(), s.qtype)).collect();
    let t = |i: u8| format!("cam-{i}.{CAM_PODS}");
    let (t1, t7, t9) = (t(1), t(7), t(9));
    assert_eq!(
        asked,
        [
            (CAM_SRV, TYPE_SRV),
            (t1.as_str(), TYPE_A),
            (t7.as_str(), TYPE_A),
            (t7.as_str(), TYPE_AAAA),
            (t9.as_str(), TYPE_A),
            (t9.as_str(), TYPE_AAAA),
        ]
    );
    assert!(
        seen.iter()
            .all(|s| s.flags & FLAG_RD != 0 && s.edns == Some(1232))
    );
    let ids: BTreeSet<_> = seen.iter().map(|s| s.id).collect();
    assert_eq!(ids.len(), seen.len(), "fresh id per query");
    let ports: BTreeSet<_> = seen.iter().map(|s| s.port).collect();
    assert_eq!(ports.len(), seen.len(), "fresh source port per query");
}

/// A flood of bogus datagrams (wrong ids from the nameserver's address,
/// right ids with a bogus pod from another port) neither ends the attempt
/// early nor poisons the answer: with one attempt, the real reply after
/// the flood is still taken.
#[tokio::test]
async fn a_flood_of_bogus_replies_does_not_suppress_or_poison_resolution() {
    let stub = Stub::start().await;
    stub.set_pods(CAM_SRV, vec![pod(1, 30)]).await;
    stub.set_mode(Mode::Flood(200)).await;
    let mut b = UnicastDnsSd::builder(map())
        .nameserver(stub.addr)
        .bind("127.0.0.1".parse().unwrap())
        .timeout(Duration::from_secs(2))
        .attempts(1)
        .schedule(FAST)
        .build()
        .await
        .expect("build");
    b.browse(KeyFilter::Exact(CAM.key())).await.expect("browse");
    assert_eq!(next(&mut b).await, up("cam-1", "10.0.0.1:8080", 30));
    let mut buf = [0u8; 512];
    assert_eq!(b.try_next(&mut buf).ok().flatten(), None, "no bogus pod");
    assert_eq!(stub.seen().await.len(), 1, "answered on the first attempt");
}

/// Query ids come from the OS CSPRNG: many draws are not a short cycle
/// and use both bytes.
#[test]
fn query_ids_come_from_the_os_csprng() {
    let ids: Vec<u16> = (0..256).map(|_| query_id().expect("getrandom")).collect();
    let distinct: BTreeSet<_> = ids.iter().collect();
    // 256 draws from 65536 values: about 0.5 collisions expected.
    assert!(distinct.len() > 240, "{} distinct", distinct.len());
    assert!(ids.iter().any(|&i| i > 0xFF) && ids.iter().any(|&i| i & 0xFF != 0));
}

/// NXDOMAIN (a headless Service without ready pods) is an empty set, not
/// an error: browsing succeeds, pods that appear are Up, and NXDOMAIN again
/// is Down for each.
#[tokio::test]
async fn nxdomain_is_an_empty_service() {
    let stub = Stub::start().await;
    let mut b = browser(&stub, FAST).await;
    b.browse(KeyFilter::Exact(CAM.key())).await.expect("browse");
    let mut buf = [0u8; 512];
    assert_eq!(b.try_next(&mut buf).ok().flatten(), None);
    assert_eq!(b.endpoints(CAM).unwrap().count(), 0);
    stub.set_pods(CAM_SRV, vec![pod(3, 30)]).await;
    assert_eq!(next(&mut b).await, up("cam-3", "10.0.0.3:8080", 30));
    stub.set_pods(CAM_SRV, vec![]).await;
    assert_eq!(next(&mut b).await, down("cam-3"));
}

/// Replies with another id, or from another address than the nameserver,
/// are skipped: the spoofed pod is never reported. With only mismatched
/// replies the query times out after its attempts, each with a fresh id.
#[tokio::test]
async fn mismatched_ids_and_spoofed_sources_are_ignored() {
    let stub = Stub::start().await;
    stub.set_pods(CAM_SRV, vec![pod(1, 30)]).await;
    let mut b = browser(&stub, FAST).await;
    for mode in [Mode::WrongIdFirst, Mode::SpoofFirst] {
        stub.set_mode(mode).await;
        b.browse(KeyFilter::Exact(CAM.key())).await.expect("browse");
        assert_eq!(
            next(&mut b).await,
            up("cam-1", "10.0.0.1:8080", 30),
            "{mode:?}"
        );
        let mut buf = [0u8; 512];
        assert_eq!(b.try_next(&mut buf).ok().flatten(), None, "{mode:?}");
    }
    stub.set_mode(Mode::OnlyWrongId).await;
    let before = stub.seen().await.len();
    let start = Instant::now();
    let err = b.browse(KeyFilter::Exact(CAM.key())).await.unwrap_err();
    assert!(matches!(err, UnicastError::Timeout), "{err:?}");
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "bounded by 2 × 300 ms"
    );
    let seen = stub.seen().await;
    let attempts = &seen[before..];
    assert_eq!(attempts.len(), 2);
    assert_ne!(attempts[0].id, attempts[1].id);
}

/// A truncated reply (TC) and an error code fail the resolution; the
/// endpoints already reported stay, and the next good answer is diffed
/// against them.
#[tokio::test]
async fn truncated_and_failed_replies_are_errors_that_keep_endpoints() {
    let stub = Stub::start().await;
    stub.set_pods(CAM_SRV, vec![pod(1, 120)]).await;
    let mut b = browser(&stub, FAST).await;
    b.browse(KeyFilter::Exact(CAM.key())).await.expect("browse");
    assert_eq!(next(&mut b).await, up("cam-1", "10.0.0.1:8080", 120));
    let mut buf = [0u8; 512];
    stub.set_mode(Mode::Truncate).await;
    let err = prompt("truncated", b.next(&mut buf)).await.unwrap_err();
    assert!(matches!(err, UnicastError::Truncated), "{err:?}");
    stub.set_mode(Mode::ServFail).await;
    let err = prompt("servfail", b.next(&mut buf)).await.unwrap_err();
    assert!(
        matches!(err, UnicastError::Answer(AnswerError::Rcode(2))),
        "{err:?}"
    );
    assert_eq!(b.endpoints(CAM).unwrap().count(), 1, "kept");
    stub.set_mode(Mode::Normal).await;
    stub.set_pods(CAM_SRV, vec![]).await;
    assert_eq!(next(&mut b).await, down("cam-1"), "no repeated Up first");
}

/// A prefix browses every mapped kind under it; an unmapped kind is
/// refused; `next` needs a browse first.
#[tokio::test]
async fn browse_selects_mapped_kinds_by_prefix() {
    let stub = Stub::start().await;
    stub.set_pods(CAM_SRV, vec![pod(1, 30)]).await;
    stub.set_pods(
        RAW_SRV,
        vec![Pod::v4("raw-0", RAW_PODS, [10, 1, 0, 1], 9000, 30)],
    )
    .await;
    let mut b = browser(&stub, FAST).await;
    let mut buf = [0u8; 512];
    let err = b.next(&mut buf).await.unwrap_err();
    assert!(matches!(err, UnicastError::NotBrowsing), "{err:?}");
    let err = b
        .browse(KeyFilter::Exact(Key::from_static("x/y")))
        .await
        .unwrap_err();
    assert!(matches!(err, UnicastError::Unmapped), "{err:?}");
    b.browse(KeyFilter::Prefix(Key::from_static("cam")))
        .await
        .expect("browse");
    let got = sorted(events(&mut b, 2).await);
    let raw = Owned {
        kind: RAW.as_str().into(),
        ..up("raw-0", "10.1.0.1:9000", 30)
    };
    assert_eq!(got, sorted(vec![up("cam-1", "10.0.0.1:8080", 30), raw]));
    let srv: BTreeSet<_> = stub
        .seen()
        .await
        .into_iter()
        .filter(|s| s.qtype == TYPE_SRV)
        .map(|s| s.name)
        .collect();
    assert_eq!(srv, [CAM_SRV.to_owned(), RAW_SRV.to_owned()].into());
    assert!(b.endpoints(BUS).is_none(), "not under the prefix");
}

/// The nameserver comes from the builder or the resolver configuration
/// (no file is read and nothing is sent here).
#[tokio::test]
async fn nameserver_from_the_resolver_configuration() {
    let conf = ResolvConf::parse("search media.svc.cluster.local\nnameserver 127.0.0.1\n");
    let b = UnicastDnsSd::builder(map())
        .resolv_conf(conf.clone())
        .build()
        .await
        .expect("build");
    assert_eq!(b.nameserver(), "127.0.0.1:53".parse().unwrap());
    assert_eq!(conf.namespace(), Some("media"));
    let err = UnicastDnsSd::builder(map())
        .resolv_conf(ResolvConf::parse("search x"))
        .build()
        .await
        .unwrap_err();
    assert!(matches!(err, UnicastError::NoNameserver), "{err:?}");
    // The map refuses what Kubernetes would.
    assert_eq!(
        ServiceMap::<1>::new()
            .with_kind_port(CAM, "Cam", "media")
            .unwrap_err(),
        MapError::InvalidLabel
    );
}

/// The discovery conformance fixture: the announcer edits the stub's
/// fixture table (as Kubernetes would publish a pod's records), the browser
/// polls the stub every 50 ms.
struct DnsSd {
    stub: Stub,
}

impl DnsSd {
    async fn new() -> Self {
        DnsSd {
            stub: Stub::start().await,
        }
    }
}

const CONF_SRV: &str = "_conf-svc._tcp.conf.test.svc.cluster.local";
const CONF_PODS: &str = "conf.test.svc.cluster.local";

/// Announces into the stub's table: the instance becomes the pod name.
struct StubAnnouncer {
    stub: Stub,
}

#[derive(Debug)]
enum AnnounceError {
    /// Only socket endpoints have DNS records.
    NotASocket,
}

impl Announce for StubAnnouncer {
    type Error = AnnounceError;

    async fn announce(&mut self, s: &Service<'_>) -> Result<(), AnnounceError> {
        let Endpoint::Socket(addr) = s.endpoint else {
            return Err(AnnounceError::NotASocket);
        };
        let (v4, v6) = match addr.ip() {
            std::net::IpAddr::V4(a) => (Some(a), None),
            std::net::IpAddr::V6(a) => (None, Some(a)),
        };
        let pod = Pod {
            target: format!("{}.{CONF_PODS}", s.instance),
            port: addr.port(),
            v4,
            v6,
            txt: s.txt.to_vec(),
            ttl: s.ttl_s,
        };
        self.stub.upsert(CONF_SRV, pod).await;
        Ok(())
    }

    async fn withdraw(
        &mut self,
        _kind: ServiceKind<'_>,
        instance: &str,
    ) -> Result<(), AnnounceError> {
        self.stub
            .remove(CONF_SRV, &format!("{instance}.{CONF_PODS}"))
            .await;
        Ok(())
    }
}

impl DiscoveryFixture for DnsSd {
    type Announcer = StubAnnouncer;
    type Browser = UnicastDnsSd;

    async fn connect(&mut self) -> (StubAnnouncer, UnicastDnsSd) {
        let map = ServiceMap::new()
            .with_kind_port(ServiceKind::from_static("conf/svc"), "conf", "test")
            .unwrap();
        let b = UnicastDnsSd::builder(map)
            .nameserver(self.stub.addr)
            .bind("127.0.0.1".parse().unwrap())
            .timeout(Duration::from_millis(300))
            .schedule(FAST)
            .build()
            .await
            .expect("build");
        (
            StubAnnouncer {
                stub: self.stub.clone(),
            },
            b,
        )
    }
}

#[tokio::test]
async fn l_announce_then_withdraw() {
    discovery::announce_then_withdraw(&mut DnsSd::new().await).await;
}

#[tokio::test]
async fn l_goodbye_removes_record_at_once() {
    discovery::goodbye_removes_record_at_once(&mut DnsSd::new().await).await;
}

#[test]
fn l_directory_expiry() {
    discovery::directory_expiry();
}
