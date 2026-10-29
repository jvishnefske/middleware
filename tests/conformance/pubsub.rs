//! Pub/sub cases: the declared `Subscriber::DELIVERY` holds under overload,
//! and gaps are reported.

use std::time::Duration;

use middleware::key::{Key, KeyFilter};
use middleware::{Overflow, Publisher, Subscriber};

use super::{PubSubFixture, prompt};

const KEY: Key<'static> = Key::from_static("conformance/load");
const EXTRA: u32 = 3;

/// Receives everything queued, as `(payload number, gap)`.
async fn drain<F: PubSubFixture>(f: &mut F, sub: &mut F::Subscriber) -> Vec<(u32, Option<u16>)> {
    f.settle().await;
    let mut buf = vec![0u8; 512];
    let mut got = Vec::new();
    // Bounded: a correct backend holds at most the queue depth.
    for _ in 0..10_000 {
        match sub.try_recv(&mut buf).expect("try_recv") {
            Some(s) => got.push((number(s.payload), s.meta.gap)),
            None => break,
        }
    }
    got
}

fn number(payload: &[u8]) -> u32 {
    u32::from_le_bytes(payload.try_into().expect("4-byte payload"))
}

async fn publish<F: PubSubFixture>(p: &mut F::Publisher, n: u32) {
    p.publish(KEY, &n.to_le_bytes()).await.expect("publish");
}

/// Publishes one sample and receives it, then publishes the subscriber's
/// queue depth plus [`EXTRA`] samples without receiving, and checks what
/// arrives against `DELIVERY`: `DropOldest` keeps the newest `depth`,
/// `DropNewest` the oldest `depth`, `Block` loses nothing (the publisher
/// waits until the subscriber drains). With `per_sender_order`, what arrives
/// is in publish order.
pub async fn overload_matches_declared_delivery<F: PubSubFixture>(f: &mut F) {
    let (mut p, mut s) = f.connect(KeyFilter::Exact(KEY)).await;
    let depth = u32::try_from(f.queue_depth()).expect("depth fits u32");
    let delivery = <F::Subscriber as Subscriber>::DELIVERY;
    publish::<F>(&mut p, 0).await;
    let mut buf = vec![0u8; 512];
    let first = prompt("first sample", s.recv(&mut buf))
        .await
        .expect("recv");
    assert_eq!(number(first.payload), 0);

    let total = depth + EXTRA;
    let got: Vec<u32> = match delivery.overflow {
        Overflow::Block => {
            // The publisher must stall once the queue is full and resume
            // as the subscriber drains; run both concurrently.
            let publishing = async {
                for n in 1..=total {
                    publish::<F>(&mut p, n).await;
                }
            };
            let receiving = async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let mut got = Vec::new();
                for _ in 0..total {
                    let s = prompt("blocked sample", s.recv(&mut buf))
                        .await
                        .expect("recv");
                    got.push(number(s.payload));
                }
                got
            };
            tokio::join!(publishing, receiving).1
        }
        Overflow::DropOldest | Overflow::DropNewest => {
            for n in 1..=total {
                publish::<F>(&mut p, n).await;
            }
            drain(f, &mut s).await.into_iter().map(|(n, _)| n).collect()
        }
    };
    let expected: Vec<u32> = match delivery.overflow {
        Overflow::Block => (1..=total).collect(),
        Overflow::DropOldest => (EXTRA + 1..=total).collect(),
        Overflow::DropNewest => (1..=depth).collect(),
    };
    if delivery.per_sender_order {
        assert_eq!(got, expected, "{delivery:?}");
    } else {
        let mut sorted = got.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, expected, "{delivery:?}");
    }
}

/// After a loss caused by overload, the next delivered sample says how many
/// were lost (`gap: Some(EXTRA)`), and in-order samples say `Some(0)`; a
/// backend that does not report gaps says `None` throughout. A `Block`
/// backend loses nothing, so every gap it reports is 0.
pub async fn gap_is_reported<F: PubSubFixture>(f: &mut F) {
    let (mut p, mut s) = f.connect(KeyFilter::Exact(KEY)).await;
    let depth = u32::try_from(f.queue_depth()).expect("depth fits u32");
    let delivery = <F::Subscriber as Subscriber>::DELIVERY;
    publish::<F>(&mut p, 0).await;
    let mut buf = vec![0u8; 512];
    prompt("first sample", s.recv(&mut buf))
        .await
        .expect("recv");
    publish::<F>(&mut p, 1).await;
    let second = prompt("second sample", s.recv(&mut buf))
        .await
        .expect("recv");
    assert_eq!(second.meta.gap, F::REPORTS_GAPS.then_some(0), "in order");
    if delivery.overflow == Overflow::Block {
        return;
    }
    let total = depth + EXTRA;
    for n in 2..2 + total {
        publish::<F>(&mut p, n).await;
    }
    let mut got = drain(f, &mut s).await;
    if delivery.overflow == Overflow::DropNewest {
        // The loss is at the end: the next sample published carries it.
        publish::<F>(&mut p, 2 + total).await;
        got.extend(drain(f, &mut s).await);
    }
    let gaps: Vec<Option<u16>> = got.iter().map(|&(_, g)| g).collect();
    if !F::REPORTS_GAPS {
        assert!(gaps.iter().all(Option::is_none), "{gaps:?}");
        return;
    }
    let lost = u16::try_from(EXTRA).expect("small");
    let expected: Vec<Option<u16>> = match delivery.overflow {
        Overflow::DropOldest => std::iter::once(Some(lost))
            .chain((1..depth).map(|_| Some(0)))
            .collect(),
        _ => (0..depth)
            .map(|_| Some(0))
            .chain(std::iter::once(Some(lost)))
            .collect(),
    };
    assert_eq!(gaps, expected, "{got:?}");
}
