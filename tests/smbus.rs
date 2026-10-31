//! The conformance cases (`tests/conformance.rs`) against the SMBus
//! multi-master transport (feature `smbus`), over the simulated bus of
//! `tests/smbus/bus.rs`, plus the SMBus specific checks: streaming across
//! blocks, interleaved and contending masters, arbitration and NACK retry
//! bounds, PEC corruption, targets, the inbox and `Split`.
//!
//! # What runs, and how
//!
//! * Request/response: every `ReqRepFixture` case, between two nodes on
//!   one bus. The node has a clock, so `request_with` honours the timeout
//!   itself (no caller-side wrapper) and carries it to the handler as
//!   `deadline_ms`; the handler sees the requester's SMBus sender id and no
//!   principal.
//! * Gather: not implemented (no service routing on the bus), not run.
//! * Pub/sub: both cases. The subscriber's inbox (4 entries, the default
//!   `Q`) is the queue `DropNewest` applies to; gaps are reported from the
//!   publication sequence.
//! * Discovery: every case, with `PubSubDiscovery` over the two halves of a
//!   split node on each side, beacons broadcast to the general call
//!   address (`directory_expiry` is pure).

#![cfg(feature = "smbus")]

#[path = "smbus/bus.rs"]
mod bus;
mod conformance;

use std::time::Duration;

use bus::{
    Fault, Outcome, SimMaster, SimTarget, TokioClock, attach, inject, set_attached, set_capacity,
};
use conformance::{
    DiscoveryFixture, PubSubFixture, ReqRepFixture, discovery, prompt, pubsub, reqrep,
};
use middleware::discovery::beacon::PubSubDiscovery;
use middleware::key::{Key, KeyFilter};
use middleware::smbus::{Address, Config, SmbusError, SmbusNode, SmbusRx, SmbusTx};
use middleware::transport::handler_fn;
use middleware::{
    HandlerError, Publisher, RequestOptions, Requester, Responder, SenderId, Split, Subscribe,
    Subscriber, Target,
};

type Node = SmbusNode<SimMaster, SimTarget, TokioClock>;
type Error = SmbusError<bus::SimError, std::convert::Infallible>;

const CLIENT: u8 = 0x20;
const DEVICE: u8 = 0x21;
const GC: Option<u8> = Some(0x00);

fn addr(raw: u8) -> Address {
    Address::node(raw).expect("node address")
}

/// A node at `address` on `bus`, answering the general call.
fn node(bus: &bus::Shared, address: u8) -> Node {
    node_with(bus, address, Config::new(addr(address)))
}

fn node_with(bus: &bus::Shared, address: u8, config: Config) -> Node {
    let (m, t) = attach(bus, address, GC);
    SmbusNode::new(m, t, TokioClock::new(), config)
}

/// A client with the device as its default peer, and the device.
fn pair(bus: &bus::Shared) -> (Node, Node) {
    let client = node_with(
        bus,
        CLIENT,
        Config::new(addr(CLIENT))
            .with_default_peer(addr(DEVICE))
            .expect("peer"),
    );
    (client, node(bus, DEVICE))
}

fn echo(r: &middleware::Request<'_>, out: &mut [u8]) -> Result<usize, HandlerError> {
    let out = out
        .get_mut(..r.body.len())
        .ok_or(HandlerError::BufferTooSmall)?;
    out.copy_from_slice(r.body);
    Ok(r.body.len())
}

struct Smbus;

impl ReqRepFixture for Smbus {
    type Requester = Node;
    type Responder = Node;
    const PROPAGATES_DEADLINE: bool = true;

    async fn connect(&mut self, _key: Key<'static>) -> (Node, Node) {
        pair(&bus::bus())
    }

    fn handler_error(e: &Error) -> Option<HandlerError> {
        match e {
            SmbusError::Handler(h) => Some(*h),
            _ => None,
        }
    }

    fn is_timeout(e: &Error) -> bool {
        matches!(e, SmbusError::Timeout)
    }

    fn expected_sender(&self) -> Option<SenderId> {
        Some(addr(CLIENT).sender_id())
    }

    fn expected_principal(&self) -> Option<&str> {
        None
    }
}

impl PubSubFixture for Smbus {
    type Publisher = Node;
    type Subscriber = Node;
    const REPORTS_GAPS: bool = true;

    async fn connect(&mut self, _filter: KeyFilter<'static>) -> (Node, Node) {
        let bus = bus::bus();
        (node(&bus, DEVICE), node(&bus, CLIENT))
    }

    fn queue_depth(&self) -> usize {
        middleware::smbus::node::DEFAULT_INBOX
    }
}

type Disc = PubSubDiscovery<
    SmbusTx<'static, SimMaster, TokioClock>,
    SmbusRx<'static, SimTarget, TokioClock>,
>;

/// A split node leaked to `'static`, as a discovery node.
fn discovery_node(n: Node) -> Disc {
    let n: &'static mut Node = Box::leak(Box::new(n));
    let (tx, rx) = n.split();
    PubSubDiscovery::new(tx, rx)
}

impl DiscoveryFixture for Smbus {
    type Announcer = Disc;
    type Browser = Disc;

    async fn connect(&mut self) -> (Disc, Disc) {
        let bus = bus::bus();
        (
            discovery_node(node(&bus, DEVICE)),
            discovery_node(node(&bus, CLIENT)),
        )
    }
}

#[tokio::test]
async fn m9_smbus_round_trip() {
    reqrep::round_trip(&mut Smbus).await;
}

#[tokio::test]
async fn m9_smbus_timeout_is_honoured() {
    reqrep::timeout_is_honoured(&mut Smbus).await;
}

#[tokio::test]
async fn m9_smbus_handler_errors_round_trip() {
    reqrep::handler_errors_round_trip(&mut Smbus).await;
}

#[tokio::test]
async fn m9_smbus_handler_sees_caller_identity() {
    reqrep::handler_sees_caller_identity(&mut Smbus).await;
}

#[tokio::test]
async fn m9_smbus_overload_matches_declared_delivery() {
    pubsub::overload_matches_declared_delivery(&mut Smbus).await;
}

#[tokio::test]
async fn m9_smbus_gap_is_reported() {
    pubsub::gap_is_reported(&mut Smbus).await;
}

#[tokio::test]
async fn m9_smbus_announce_then_withdraw() {
    discovery::announce_then_withdraw(&mut Smbus).await;
}

#[tokio::test]
async fn m9_smbus_goodbye_removes_record_at_once() {
    discovery::goodbye_removes_record_at_once(&mut Smbus).await;
}

#[test]
fn m9_smbus_directory_expiry() {
    discovery::directory_expiry();
}

const KA: Key<'static> = Key::from_static("app/a");
const KB: Key<'static> = Key::from_static("app/b");

fn pattern(seed: u8, n: usize) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

/// Drains `n` samples as (sender address, key, payload, gap).
fn drain(n: &mut Node) -> Vec<(u8, String, Vec<u8>, Option<u16>)> {
    let mut buf = [0u8; 1024];
    let mut got = Vec::new();
    for _ in 0..64 {
        match n.try_recv(&mut buf).expect("try_recv") {
            Some(s) => got.push((
                s.meta.sender.map_or(0, |id| id.0[3]),
                s.key.as_str().to_string(),
                s.payload.to_vec(),
                s.meta.gap,
            )),
            None => break,
        }
    }
    got
}

/// Two masters stream 300-byte publications (11 blocks each) at the same
/// time; their blocks interleave on the bus and the listener reassembles
/// both intact.
#[tokio::test]
async fn m10_interleaved_masters_stream_large_messages() {
    let bus = bus::bus();
    let mut a = node(&bus, 0x10);
    let mut b = node(&bus, 0x11);
    let mut c = node(&bus, 0x12);
    let (pa, pb) = (pattern(1, 300), pattern(2, 300));
    let (ra, rb) = tokio::join!(a.publish(KA, &pa), b.publish(KB, &pb));
    ra.expect("a");
    rb.expect("b");
    // The broadcast blocks alternate between the two masters.
    let order: Vec<u8> = bus
        .borrow()
        .log
        .iter()
        .filter(|r| r.to == 0)
        .map(|r| r.from)
        .collect();
    assert_eq!(order.len(), 22);
    let switches = order.windows(2).filter(|w| w[0] != w[1]).count();
    assert!(switches >= 10, "not interleaved: {order:?}");
    let mut got = drain(&mut c);
    got.sort();
    assert_eq!(
        got,
        [
            (0x10, "app/a".into(), pa, None),
            (0x11, "app/b".into(), pb, None)
        ]
    );
    assert_eq!(c.stats().rx.reassembly.missing, 0);
}

/// With contention, a master that starts while another's block is on the
/// bus loses arbitration, backs off and retries: both messages arrive.
#[tokio::test]
async fn m10_contending_masters_back_off_and_both_deliver() {
    let bus = bus::bus();
    bus.borrow_mut().contention = true;
    let mut a = node(&bus, 0x10);
    let mut b = node(&bus, 0x11);
    let mut c = node(&bus, 0x12);
    let (pa, pb) = (pattern(3, 120), pattern(4, 120));
    let (ra, rb) = prompt("contention", async {
        tokio::join!(a.publish(KA, &pa), b.publish(KB, &pb))
    })
    .await;
    ra.expect("a");
    rb.expect("b");
    let losses = bus
        .borrow()
        .log
        .iter()
        .filter(|r| r.outcome == Outcome::Failed(embedded_hal::i2c::ErrorKind::ArbitrationLoss))
        .count();
    assert!(losses > 0, "no contention happened");
    let retries = a.stats().tx.arbitration_retries + b.stats().tx.arbitration_retries;
    assert_eq!(retries as usize, losses);
    let mut got = drain(&mut c);
    got.sort();
    assert_eq!(got.len(), 2);
    assert_eq!(
        (got[0].2.as_slice(), got[1].2.as_slice()),
        (&pa[..], &pb[..])
    );
}

/// A block is retried after each lost arbitration up to the policy's
/// bound (8 losses by default): 7 losses still deliver, 8 give up.
#[tokio::test]
async fn m11_arbitration_retries_are_bounded() {
    let bus = bus::bus();
    let mut a = node(&bus, 0x10);
    let mut c = node(&bus, 0x12);
    inject(&bus, 0x10, &[Fault::ArbitrationLoss; 7]);
    a.publish(KA, b"seven")
        .await
        .expect("delivered on the 8th write");
    assert_eq!(bus.borrow().writes(0x10, 0), 8);
    assert_eq!(a.stats().tx.arbitration_retries, 7);
    assert_eq!(drain(&mut c).len(), 1);

    inject(&bus, 0x10, &[Fault::ArbitrationLoss; 8]);
    let e = a.publish(KA, b"eight").await.expect_err("gives up");
    assert!(matches!(e, SmbusError::ArbitrationLost), "{e:?}");
    assert_eq!(bus.borrow().writes(0x10, 0), 16);
    assert!(drain(&mut c).is_empty());
}

/// Unicast NACKs are retried 3 times then reported; a broadcast nobody
/// answers is dropped after one write; a broadcast data NACK is retried
/// then reported; a bus error propagates at once.
#[tokio::test]
async fn m11_nacks_and_bus_errors() {
    let bus = bus::bus();
    let mut a = node(&bus, 0x10);
    let mut out = [0u8; 16];
    let absent = Target::Node(addr(0x13).sender_id());
    let e = a
        .request_with(
            KA,
            b"x",
            &mut out,
            &RequestOptions::DEFAULT.with_target(absent),
        )
        .await
        .expect_err("nobody at 0x13");
    assert!(matches!(e, SmbusError::NoAcknowledge), "{e:?}");
    assert_eq!(bus.borrow().writes(0x10, 0x13), 3);

    a.publish(KA, b"nobody")
        .await
        .expect("unheard is not an error");
    assert_eq!((a.stats().tx.unheard, bus.borrow().writes(0x10, 0)), (1, 1));

    let _c = node(&bus, 0x12);
    inject(&bus, 0x10, &[Fault::NackData; 3]);
    let e = a.publish(KA, b"data").await.expect_err("data NACKs");
    assert!(matches!(e, SmbusError::NoAcknowledge), "{e:?}");

    inject(&bus, 0x10, &[Fault::Bus]);
    let e = a.publish(KA, b"bus").await.expect_err("bus error");
    assert!(
        matches!(
            e,
            SmbusError::Bus(bus::SimError(embedded_hal::i2c::ErrorKind::Bus))
        ),
        "{e:?}"
    );
}

/// What a target adapter's own limits look like from the sender: a full
/// unicast target NACKs the data (retried, then reported); a full broadcast
/// listener drops the newest, which the next sample's gap reports; a
/// detached listener, or an address NACK on the broadcast, means nobody
/// heard the publication.
#[tokio::test]
async fn m11_full_and_detached_targets() {
    let bus = bus::bus();
    let (mut client, _device) = pair(&bus);
    set_capacity(&bus, DEVICE, 0);
    let mut out = [0u8; 16];
    let e = client.request(KA, b"x", &mut out).await.expect_err("full");
    assert!(matches!(e, SmbusError::NoAcknowledge), "{e:?}");
    assert_eq!(bus.borrow().writes(CLIENT, DEVICE), 3);

    let mut c = node(&bus, 0x12);
    set_capacity(&bus, 0x12, 2);
    for n in 0..3u8 {
        client.publish(KA, &[n]).await.expect("publish");
    }
    assert_eq!(bus.borrow().dropped(0x12), 1);
    assert_eq!(drain(&mut c).len(), 2);
    client.publish(KA, &[3]).await.expect("publish");
    assert_eq!(drain(&mut c)[0].3, Some(1));

    set_attached(&bus, 0x12, false);
    set_attached(&bus, DEVICE, false);
    client.publish(KA, b"gone").await.expect("unheard");
    set_attached(&bus, 0x12, true);
    inject(&bus, CLIENT, &[Fault::NackAddress]);
    let before = bus.borrow().writes(CLIENT, 0);
    client.publish(KA, b"nack").await.expect("unheard");
    assert_eq!(bus.borrow().writes(CLIENT, 0), before + 1);
    assert_eq!(client.stats().tx.unheard, 2);
    assert!(drain(&mut c).is_empty());
}

/// A corrupted block fails its PEC and is dropped and counted; the message
/// it belonged to is never delivered, the next one is, and its gap says one
/// publication was lost.
#[tokio::test]
async fn m12_pec_corruption_drops_the_block_and_the_message() {
    let bus = bus::bus();
    let mut a = node(&bus, 0x10);
    let mut c = node(&bus, 0x12);
    a.publish(KA, b"first").await.expect("first");
    // A 4-block message: block 1 delivered twice, block 2 corrupted in its
    // first body byte (byte 5 after the address byte).
    inject(
        &bus,
        0x10,
        &[Fault::Pass, Fault::Duplicate, Fault::Corrupt(5)],
    );
    a.publish(KA, &pattern(9, 100)).await.expect("sent");
    a.publish(KA, b"after").await.expect("after");
    let got = drain(&mut c);
    let payloads: Vec<&[u8]> = got.iter().map(|g| g.2.as_slice()).collect();
    assert_eq!(payloads, [&b"first"[..], b"after"]);
    assert_eq!(got[1].3, Some(1), "one publication lost");
    let s = c.stats().rx;
    assert_eq!(s.pec_errors, 1);
    assert_eq!(s.reassembly.duplicates, 1, "the duplicated block");
    assert_eq!(s.reassembly.missing, 1);
}

/// A single-block publication delivered twice is delivered once.
#[tokio::test]
async fn m12_duplicate_single_block_is_delivered_once() {
    let bus = bus::bus();
    let mut a = node(&bus, 0x10);
    let mut c = node(&bus, 0x12);
    inject(&bus, 0x10, &[Fault::Duplicate]);
    a.publish(KA, b"once").await.expect("publish");
    assert_eq!(drain(&mut c).len(), 1);
    assert_eq!(c.stats().rx.duplicates, 1);
}

/// `Target::Node` reaches that node; ids that are not SMBus nodes are
/// refused, as is `Target::Any` without a default peer.
#[tokio::test]
async fn m13_targets() {
    let bus = bus::bus();
    let mut a = node(&bus, 0x10);
    let b = node(&bus, 0x11);
    let mut c = node(&bus, 0x12);
    let mut out = [0u8; 16];
    let mut scratch = [0u8; 256];
    let mut h = handler_fn(|r, out| {
        out[0] = r.meta.sender.map_or(0, |s| s.0[3]);
        Ok(1)
    });
    let to_c = RequestOptions::DEFAULT
        .with_target(Target::Node(c.sender_id()))
        .with_timeout(Duration::from_secs(2));
    let (got, served) = prompt("to c", async {
        tokio::join!(
            a.request_with(KA, b"", &mut out, &to_c),
            c.serve_one(&mut h, &mut scratch)
        )
    })
    .await;
    served.expect("serve");
    assert_eq!(got.expect("reply"), [0x10]);
    assert_eq!(bus.borrow().writes(0x10, 0x11), 0, "b was not asked");
    assert_eq!(b.inbox_len(), 0);

    let mac = SenderId::from_mac([2, 0, 0, 0, 0, 1]);
    let opt = RequestOptions::DEFAULT.with_target(Target::Node(mac));
    let e = a
        .request_with(KA, b"", &mut out, &opt)
        .await
        .expect_err("mac");
    assert_eq!(e, SmbusError::UnsupportedTarget(mac));
    let e = a.request(KA, b"", &mut out).await.expect_err("no peer");
    assert_eq!(e, SmbusError::NoTarget);
}

/// Requests and replies of several blocks each.
#[tokio::test]
async fn m13_large_requests_and_replies_stream() {
    let bus = bus::bus();
    let (mut client, mut device) = pair(&bus);
    let req = pattern(5, 400);
    let mut h = handler_fn(|r, out| {
        let n = r.body.len() + 50;
        for (i, b) in out[..n].iter_mut().enumerate() {
            *b = r.body.get(i).copied().unwrap_or(0xEE);
        }
        Ok(n)
    });
    let mut out = [0u8; 512];
    let mut scratch = [0u8; 1024];
    let (got, served) = prompt("large", async {
        tokio::join!(
            client.request(KA, &req, &mut out),
            device.serve_one(&mut h, &mut scratch)
        )
    })
    .await;
    served.expect("serve");
    let got = got.expect("reply");
    assert_eq!((got.len(), &got[..400], got[449]), (450, &req[..], 0xEE));
    // A reply that does not fit the requester's buffer.
    let mut small = [0u8; 8];
    let mut h = handler_fn(echo);
    let (got, served) = prompt("small", async {
        tokio::join!(
            client.request(KA, b"hello world", &mut small),
            device.serve_one(&mut h, &mut scratch)
        )
    })
    .await;
    served.expect("serve");
    assert_eq!(got.expect_err("too small"), SmbusError::BufferTooSmall);
    // A request over M is refused before anything is sent.
    let writes = bus.borrow().log.len();
    let e = client
        .request(KA, &[0u8; 600], &mut out)
        .await
        .expect_err("too large");
    assert_eq!(e, SmbusError::TooLarge);
    assert_eq!(bus.borrow().log.len(), writes);
}

/// Publications that arrive while a request waits are kept, in order, and
/// returned by `recv` afterwards with their sender.
#[tokio::test]
async fn m14_publications_during_a_request_are_queued_in_order() {
    let bus = bus::bus();
    let (mut client, mut device) = pair(&bus);
    let mut out = [0u8; 16];
    let mut scratch = [0u8; 256];
    let mut h = handler_fn(echo);
    let (got, served) = prompt("request", async {
        tokio::join!(client.request(KA, b"ping", &mut out), async {
            for i in 0..3u8 {
                device.publish(KB, &[i]).await?;
            }
            device.serve_one(&mut h, &mut scratch).await
        })
    })
    .await;
    served.expect("serve");
    assert_eq!(got.expect("reply"), b"ping");
    assert_eq!(client.inbox_len(), 3);
    let got = drain(&mut client);
    let expected: Vec<_> = (0..3u8)
        .map(|i| {
            (
                DEVICE,
                "app/b".to_string(),
                vec![i],
                if i == 0 { None } else { Some(0) },
            )
        })
        .collect();
    assert_eq!(got, expected);
}

/// A request that reaches a node while that node waits for its own reply
/// is queued and served by its next `serve_one` (multi-master: every node
/// may be requester and responder).
#[tokio::test]
async fn m14_requests_arriving_while_waiting_are_served_next() {
    let bus = bus::bus();
    let mut a = node(&bus, 0x10);
    let mut b = node(&bus, 0x11);
    let mut c = node(&bus, 0x12);
    let opts = |to: &Node| {
        RequestOptions::DEFAULT
            .with_target(Target::Node(to.sender_id()))
            .with_timeout(Duration::from_secs(3))
    };
    let (to_a, to_b) = (opts(&a), opts(&b));
    let (mut out_a, mut out_c) = ([0u8; 16], [0u8; 16]);
    let (mut sa, mut sb) = ([0u8; 256], [0u8; 256]);
    let (ra, rb, rc) = prompt("mutual", async {
        tokio::join!(
            async {
                // c's request lands while a waits for b.
                let r = a
                    .request_with(KA, b"a->b", &mut out_a, &to_b)
                    .await
                    .map(<[u8]>::to_vec);
                a.serve_one(&mut handler_fn(echo), &mut sa)
                    .await
                    .map(|()| r)
            },
            async {
                tokio::time::sleep(Duration::from_millis(20)).await;
                b.serve_one(&mut handler_fn(echo), &mut sb).await
            },
            c.request_with(KB, b"c->a", &mut out_c, &to_a)
        )
    })
    .await;
    assert_eq!(ra.expect("a served").expect("a's reply"), b"a->b");
    rb.expect("b served");
    assert_eq!(rc.expect("c's reply"), b"c->a");
}

/// One task publishes on the `SmbusTx` half while `recv` waits on the
/// `SmbusRx` half of the same node.
#[tokio::test]
async fn m15_split_publishes_while_subscribed() {
    let bus = bus::bus();
    let mut a = node(&bus, 0x10);
    let mut b = node(&bus, 0x11);
    let (mut a_tx, mut a_rx) = a.split();
    let (mut b_tx, mut b_rx) = b.split();
    let (mut abuf, mut bbuf) = ([0u8; 512], [0u8; 512]);
    let (from_b, from_a, ()) = prompt("split", async {
        tokio::join!(a_rx.recv(&mut abuf), b_rx.recv(&mut bbuf), async {
            tokio::task::yield_now().await;
            a_tx.publish(KA, b"from a").await.expect("a");
            b_tx.publish(KB, b"from b").await.expect("b");
        })
    })
    .await;
    assert_eq!(from_b.expect("a recv").payload, b"from b");
    assert_eq!(from_a.expect("b recv").payload, b"from a");
}

/// A filtered subscription skips other keys; queued requests and
/// publications share the inbox.
#[tokio::test]
async fn m15_subscribe_filters_keys() {
    let bus = bus::bus();
    let mut a = node(&bus, 0x10);
    let mut c = node(&bus, 0x12);
    for k in [KA, KB, KA] {
        a.publish(k, k.as_bytes()).await.expect("publish");
    }
    let mut sub = c.subscribe(KeyFilter::Exact(KB)).await.expect("subscribe");
    let mut buf = [0u8; 64];
    let s = sub.try_recv(&mut buf).expect("try_recv").expect("app/b");
    assert_eq!(s.key, KB);
    assert!(sub.try_recv(&mut buf).expect("try_recv").is_none());
}

/// Nodes refuse reserved addresses at construction: no such node exists.
#[test]
fn m1_reserved_addresses_build_no_node() {
    assert!(Address::node(0x08).is_err());
    assert!(Address::node(0x61).is_err());
    assert!(Address::node(0x7F).is_err());
}
