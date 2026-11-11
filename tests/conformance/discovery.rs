//! Discovery cases: announce/browse/withdraw through a backend, and the
//! `Directory` rules fed with what it reports.

use middleware::SenderId;
use middleware::discovery::{
    Announce, Browse, Change, Directory, Endpoint, Event, Service, ServiceKind,
};
use middleware::key::KeyFilter;

use super::{DiscoveryFixture, prompt};

const KIND: ServiceKind<'static> = ServiceKind::from_static("conf/svc");
const TTL_S: u32 = 120;

fn service() -> Service<'static> {
    Service {
        kind: KIND,
        instance: "unit-1",
        node: Some(SenderId([0x5A; 8])),
        endpoint: Endpoint::Socket("127.0.0.1:7447".parse().expect("addr")),
        txt: b"\x05ver=5",
        ttl_s: TTL_S,
    }
}

/// Browses `KIND`, announces, and returns the `Up` event's service fields
/// owned: (instance, endpoint text, txt, ttl).
async fn up<F: DiscoveryFixture>(f: &mut F, a: &mut F::Announcer, b: &mut F::Browser) {
    b.browse(KeyFilter::Exact(KIND.key()))
        .await
        .expect("browse");
    f.settle().await;
    a.announce(&service()).await.expect("announce");
    f.settle().await;
    let mut buf = [0u8; 512];
    let event = prompt("Up event", b.next(&mut buf)).await.expect("next");
    let Event::Up(s) = event else {
        panic!("expected Up, got {event:?}")
    };
    assert_eq!((s.kind, s.instance, s.endpoint, s.txt, s.ttl_s), {
        let w = service();
        (w.kind, w.instance, w.endpoint, w.txt, w.ttl_s)
    });
    assert!(
        s.node.is_none() || s.node == service().node,
        "node {:?}",
        s.node
    );
}

/// Announce → browse sees `Up` with the announced fields; withdraw →
/// browse sees `Down` promptly, far within the 120 s TTL.
pub async fn announce_then_withdraw<F: DiscoveryFixture>(f: &mut F) {
    let (mut a, mut b) = f.connect().await;
    up(f, &mut a, &mut b).await;
    let start = std::time::Instant::now();
    a.withdraw(KIND, "unit-1").await.expect("withdraw");
    f.settle().await;
    let mut buf = [0u8; 512];
    let event = prompt("Down event", b.next(&mut buf)).await.expect("next");
    assert_eq!(
        event,
        Event::Down {
            kind: KIND,
            instance: "unit-1"
        }
    );
    assert!(start.elapsed().as_secs() < u64::from(TTL_S) / 4);
}

/// The events a backend reports drive a `Directory`: `Up` adds the record,
/// the goodbye removes it at the same instant (no waiting for the TTL).
pub async fn goodbye_removes_record_at_once<F: DiscoveryFixture>(f: &mut F) {
    let (mut a, mut b) = f.connect().await;
    up(f, &mut a, &mut b).await;
    let d = Directory::<4>::new();
    let (d, c) = d.observe(&Event::Up(service()), 1_000);
    assert_eq!(c, Change::Added);
    a.withdraw(KIND, "unit-1").await.expect("withdraw");
    f.settle().await;
    let mut buf = [0u8; 512];
    let event = prompt("Down event", b.next(&mut buf)).await.expect("next");
    let (d, c) = d.observe(&event, 1_000);
    assert_eq!(c, Change::Removed);
    assert_eq!(d.lookup(KIND).count(), 0);
}

/// Pure: a record is kept until exactly its TTL and gone after; refresh is
/// due at 80 % of it.
pub fn directory_expiry() {
    let s = Service {
        ttl_s: 10,
        ..service()
    };
    let (d, _) = Directory::<2>::new().observe(&Event::Up(s), 5_000);
    assert_eq!(d.next_refresh(5_000), Some(13_000));
    let (d, n) = d.expire(14_999);
    assert_eq!((n, d.len()), (0, 1));
    let (d, n) = d.expire(15_000);
    assert_eq!((n, d.len()), (1, 0));
}
