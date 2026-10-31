//! The conformance cases (`tests/conformance.rs`) against the serial link
//! (feature `serial`), over an in-memory fake link pair, plus the serial
//! specific checks: the inbox kept while a request waits and `Split`.
//!
//! # The fake link
//!
//! [`pair`] joins two [`SerialLink`]s over [`Duplex`] streams of a
//! [`FakeRx`] and a [`FakeTx`] sharing one byte queue per direction. A
//! direction is either unbounded or **lossy**: it holds a fixed number of
//! bytes, like the receive buffer of a UART without flow control, and a
//! write that does not fit is dropped whole (the newest data is lost).
//! Reads wait (cancel-safely) for bytes.
//!
//! # What runs, and how
//!
//! * Request/response: every `ReqRepFixture` case. The link has no clock
//!   (it documents that the caller bounds the call), so the fixture's
//!   requester is [`Bounded`], the caller-side wrapper every user of a
//!   clockless link writes: it races `request_with` against
//!   `RequestOptions::timeout` with a tokio timer. That is what
//!   `timeout_is_honoured` measures; it also shows the call is cancel safe
//!   (the next request after a timeout still works, see
//!   `s8_request_after_a_timeout_still_works`). Each side's link names its
//!   peer ([`SerialLink::with_peer`]), so the handler sees the client's id;
//!   there is no principal and no deadline on the wire.
//! * Gather: not implemented by the serial link (one peer), not run.
//! * Pub/sub: both cases over a lossy direction of 4 frames, which is
//!   where a serial link's `DropNewest` comes from in deployment (a UART
//!   behind a CH340 has no flow control). The link reports no gaps.
//! * Discovery: every case, with `PubSubDiscovery` over the two halves of
//!   a split link on each side (`directory_expiry` is pure and needs no
//!   link).

#![cfg(feature = "serial")]

mod conformance;

use std::cell::RefCell;
use std::collections::VecDeque;
use std::convert::Infallible;
use std::future::poll_fn;
use std::rc::Rc;
use std::task::{Poll, Waker};
use std::time::Duration;

use conformance::{
    DiscoveryFixture, PubSubFixture, ReqRepFixture, discovery, prompt, pubsub, reqrep,
};
use embedded_io_async::{ErrorType, Read, Write};
use middleware::discovery::beacon::PubSubDiscovery;
use middleware::key::{Key, KeyFilter};
use middleware::serial::frame::{self, Frame, MAX_RAW_FRAME, MAX_WIRE_FRAME};
use middleware::serial::link::{Duplex, LinkError, RECV_BUF_LEN, SerialLink, SerialRx, SerialTx};
use middleware::transport::handler_fn;
use middleware::{
    HandlerError, Publisher, RequestOptions, Requester, Responder, SenderId, Split, Subscriber,
};

const CLIENT: SenderId = SenderId::from_mac([0x02, 0, 0, 0, 0, 0xC1]);
const DEVICE: SenderId = SenderId::from_mac([0x02, 0, 0, 0, 0, 0xD1]);

/// One direction of the fake link.
struct Wire {
    bytes: VecDeque<u8>,
    /// `Some(n)`: holds at most `n` bytes; a write that does not fit is
    /// dropped whole.
    lossy: Option<usize>,
    dropped_writes: usize,
    reader: Option<Waker>,
}

impl Wire {
    fn new(lossy: Option<usize>) -> Rc<RefCell<Wire>> {
        Rc::new(RefCell::new(Wire {
            bytes: VecDeque::new(),
            lossy,
            dropped_writes: 0,
            reader: None,
        }))
    }
}

/// The reading end of a [`Wire`].
struct FakeRx(Rc<RefCell<Wire>>);
/// The writing end of a [`Wire`].
struct FakeTx(Rc<RefCell<Wire>>);

impl ErrorType for FakeRx {
    type Error = Infallible;
}
impl ErrorType for FakeTx {
    type Error = Infallible;
}

impl Read for FakeRx {
    /// Waits for bytes; consumes them only when it completes (cancel safe).
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Infallible> {
        poll_fn(|cx| {
            let mut w = self.0.borrow_mut();
            if w.bytes.is_empty() {
                w.reader = Some(cx.waker().clone());
                return Poll::Pending;
            }
            let n = buf.len().min(w.bytes.len());
            for (dst, src) in buf.iter_mut().zip(w.bytes.drain(..n)) {
                *dst = src;
            }
            Poll::Ready(Ok(n))
        })
        .await
    }
}

impl Write for FakeTx {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Infallible> {
        let mut w = self.0.borrow_mut();
        let fits = w.lossy.is_none_or(|cap| w.bytes.len() + buf.len() <= cap);
        if fits {
            w.bytes.extend(buf);
        } else {
            w.dropped_writes += 1;
        }
        if let Some(r) = w.reader.take() {
            r.wake();
        }
        Ok(buf.len())
    }

    async fn flush(&mut self) -> Result<(), Infallible> {
        Ok(())
    }
}

type Stream = Duplex<FakeRx, FakeTx>;
type Link = SerialLink<Stream>;

/// Two links joined by the fake: `a`'s writes reach `b` through a wire of
/// `a_to_b` bytes (lossy) or unbounded, and the other way round.
fn pair(a_to_b: Option<usize>, b_to_a: Option<usize>) -> (Link, Link, Rc<RefCell<Wire>>) {
    let ab = Wire::new(a_to_b);
    let ba = Wire::new(b_to_a);
    let a = SerialLink::new(Duplex::new(FakeRx(ba.clone()), FakeTx(ab.clone())));
    let b = SerialLink::new(Duplex::new(FakeRx(ab.clone()), FakeTx(ba)));
    (a, b, ab)
}

/// Wire bytes of one publication.
fn wire_len(key: Key<'_>, payload: &[u8]) -> usize {
    let mut scratch = [0u8; MAX_RAW_FRAME];
    let mut out = [0u8; MAX_WIRE_FRAME];
    frame::encode(&Frame::Publish { key, payload }, &mut scratch, &mut out).expect("encodes")
}

/// Why a [`Bounded`] request failed.
#[derive(Debug)]
enum BoundedError {
    /// The link failed (or the peer's handler did).
    Link(LinkError<<Stream as ErrorType>::Error>),
    /// `RequestOptions::timeout` elapsed first.
    Timeout,
}

/// A serial link whose `request_with` is bounded by the caller's timeout,
/// as the link's documentation asks callers to do.
struct Bounded(Link);

impl Requester for Bounded {
    type Error = BoundedError;

    async fn request_with<'b>(
        &mut self,
        key: Key<'_>,
        request: &[u8],
        response: &'b mut [u8],
        options: &RequestOptions,
    ) -> Result<&'b [u8], BoundedError> {
        let call = self.0.request_with(key, request, response, options);
        match options.timeout {
            None => call.await.map_err(BoundedError::Link),
            Some(t) => match tokio::time::timeout(t, call).await {
                Ok(r) => r.map_err(BoundedError::Link),
                Err(_) => Err(BoundedError::Timeout),
            },
        }
    }
}

struct Serial;

impl ReqRepFixture for Serial {
    type Requester = Bounded;
    type Responder = Link;
    const PROPAGATES_DEADLINE: bool = false;

    async fn connect(&mut self, _key: Key<'static>) -> (Bounded, Link) {
        let (client, device, _) = pair(None, None);
        (Bounded(client.with_peer(DEVICE)), device.with_peer(CLIENT))
    }

    fn handler_error(e: &BoundedError) -> Option<HandlerError> {
        match e {
            BoundedError::Link(LinkError::Handler(h)) => Some(*h),
            _ => None,
        }
    }

    fn is_timeout(e: &BoundedError) -> bool {
        matches!(e, BoundedError::Timeout)
    }

    fn expected_sender(&self) -> Option<SenderId> {
        Some(CLIENT)
    }

    fn expected_principal(&self) -> Option<&str> {
        None
    }
}

/// Frames the lossy direction of the pub/sub fixture holds.
const DEPTH: usize = 4;

impl PubSubFixture for Serial {
    type Publisher = Link;
    type Subscriber = Link;
    const REPORTS_GAPS: bool = false;

    async fn connect(&mut self, _filter: KeyFilter<'static>) -> (Link, Link) {
        // The cases publish 4-byte payloads on one key: every frame has the
        // same wire length (COBS adds one byte below 254).
        let frame = wire_len(Key::from_static("conformance/load"), &[0; 4]);
        let (publisher, subscriber, _) = pair(Some(DEPTH * frame), None);
        (publisher.with_peer(DEVICE), subscriber.with_peer(CLIENT))
    }

    fn queue_depth(&self) -> usize {
        DEPTH
    }
}

type Disc = PubSubDiscovery<SerialTx<'static, FakeTx>, SerialRx<'static, FakeRx>>;

/// A split link leaked to `'static`, as a discovery node.
fn discovery_node(link: Link) -> Disc {
    let link: &'static mut Link = Box::leak(Box::new(link));
    let (tx, rx) = link.split();
    PubSubDiscovery::new(tx, rx)
}

impl DiscoveryFixture for Serial {
    type Announcer = Disc;
    type Browser = Disc;

    async fn connect(&mut self) -> (Disc, Disc) {
        let (a, b, _) = pair(None, None);
        (discovery_node(a), discovery_node(b))
    }
}

#[tokio::test]
async fn k15_serial_round_trip() {
    reqrep::round_trip(&mut Serial).await;
}

#[tokio::test]
async fn k15_serial_timeout_is_honoured() {
    reqrep::timeout_is_honoured(&mut Serial).await;
}

#[tokio::test]
async fn k15_serial_handler_errors_round_trip() {
    reqrep::handler_errors_round_trip(&mut Serial).await;
}

#[tokio::test]
async fn k15_serial_handler_sees_caller_identity() {
    reqrep::handler_sees_caller_identity(&mut Serial).await;
}

#[tokio::test]
async fn k15_serial_overload_matches_declared_delivery() {
    pubsub::overload_matches_declared_delivery(&mut Serial).await;
}

#[tokio::test]
async fn k15_serial_gap_is_reported() {
    pubsub::gap_is_reported(&mut Serial).await;
}

#[tokio::test]
async fn k15_serial_announce_then_withdraw() {
    discovery::announce_then_withdraw(&mut Serial).await;
}

#[tokio::test]
async fn k15_serial_goodbye_removes_record_at_once() {
    discovery::goodbye_removes_record_at_once(&mut Serial).await;
}

#[test]
fn k15_serial_directory_expiry() {
    discovery::directory_expiry();
}

const ECHO: Key<'static> = Key::from_static("svc/echo");

/// A device that publishes `n` samples before it answers: the requester
/// keeps them in its inbox and `recv` returns them in order, with the
/// device as their sender.
#[tokio::test]
async fn s8_publications_during_a_request_reach_recv_in_order() {
    let (client, mut device, _) = pair(None, None);
    let mut client = client.with_peer(DEVICE);
    let mut h = handler_fn(|r, out| {
        out[..r.body.len()].copy_from_slice(r.body);
        Ok(r.body.len())
    });
    let mut resp = [0u8; 16];
    let mut scratch = [0u8; 64];
    let (got, served) = prompt("request", async {
        tokio::join!(client.request(ECHO, b"ping", &mut resp), async {
            for i in 0..3u8 {
                device.publish(Key::from_static("app/log"), &[i]).await?;
            }
            device.serve_one(&mut h, &mut scratch).await
        })
    })
    .await;
    served.expect("serve");
    assert_eq!(got.expect("request"), b"ping");
    assert_eq!((client.inbox_len(), client.inbox_dropped()), (3, 0));
    let mut buf = [0u8; RECV_BUF_LEN];
    for i in 0..3u8 {
        let s = client
            .try_recv(&mut buf)
            .expect("try_recv")
            .expect("queued");
        assert_eq!(
            (s.key.as_str(), s.payload, s.meta.sender),
            ("app/log", &[i][..], Some(DEVICE))
        );
    }
    assert!(client.try_recv(&mut buf).expect("try_recv").is_none());
}

/// More publications than the inbox holds: the newest are dropped and
/// counted, the oldest kept, and the request still completes.
#[tokio::test]
async fn s8_inbox_overflow_drops_the_newest() {
    let (client, mut device, _) = pair(None, None);
    let key = Key::from_static("app/big");
    let payload = [7u8; 600];
    // The default inbox holds one 600-byte publication, not two.
    let mut client: Link = client;
    let mut h = handler_fn(|_r, _out| Ok(0));
    let mut resp = [0u8; 16];
    let mut scratch = [0u8; 64];
    let (got, served) = prompt("request", async {
        tokio::join!(client.request(ECHO, b"", &mut resp), async {
            device.publish(key, &payload).await?;
            device.publish(key, &[8u8; 600]).await?;
            device.serve_one(&mut h, &mut scratch).await
        })
    })
    .await;
    served.expect("serve");
    got.expect("request");
    assert_eq!((client.inbox_len(), client.inbox_dropped()), (1, 1));
    let mut buf = [0u8; RECV_BUF_LEN];
    let s = client.try_recv(&mut buf).expect("try_recv").expect("kept");
    assert_eq!(s.payload, &payload[..]);
}

/// A request abandoned by its caller's timeout leaves the link usable: the
/// late answer is discarded and the next request gets its own.
#[tokio::test]
async fn s8_request_after_a_timeout_still_works() {
    let (client, mut device, _) = pair(None, None);
    let mut client = Bounded(client);
    let mut out = [0u8; 16];
    let short = RequestOptions::DEFAULT.with_timeout(Duration::from_millis(50));
    let first = client.request_with(ECHO, b"one", &mut out, &short).await;
    assert!(matches!(first, Err(BoundedError::Timeout)));
    let mut h = handler_fn(|r, out| {
        out[..r.body.len()].copy_from_slice(r.body);
        Ok(r.body.len())
    });
    let mut scratch = [0u8; 64];
    let (got, served) = prompt("second request", async {
        tokio::join!(client.request(ECHO, b"two", &mut out), async {
            // Answers the abandoned request, then the new one.
            device.serve_one(&mut h, &mut scratch).await?;
            device.serve_one(&mut h, &mut scratch).await
        })
    })
    .await;
    served.expect("serve");
    assert_eq!(got.expect("request"), b"two");
}

/// K11: one side publishes on its `SerialTx` while a receive waits on its
/// `SerialRx`, concurrently in one task.
#[tokio::test]
async fn k11_serial_split_publishes_while_subscribed() {
    let (mut a, mut b, _) = pair(None, None);
    let (mut a_tx, mut a_rx) = a.split();
    let (mut b_tx, mut b_rx) = b.split();
    let mut abuf = [0u8; RECV_BUF_LEN];
    let mut bbuf = [0u8; RECV_BUF_LEN];
    let (from_b, from_a, ()) = prompt("split", async {
        tokio::join!(
            // `a` waits for `b`'s sample...
            a_rx.recv(&mut abuf),
            // ...while `b` waits for `a`'s...
            b_rx.recv(&mut bbuf),
            // ...and both publish while their receives are pending.
            async {
                tokio::task::yield_now().await;
                a_tx.publish(Key::from_static("app/a"), b"from a")
                    .await
                    .expect("a");
                b_tx.publish(Key::from_static("app/b"), b"from b")
                    .await
                    .expect("b");
            }
        )
    })
    .await;
    assert_eq!(from_b.expect("a recv").payload, b"from b");
    assert_eq!(from_a.expect("b recv").payload, b"from a");
}

/// The lossy fake drops whole writes once full, as the pub/sub fixture
/// relies on.
#[tokio::test]
async fn fake_lossy_wire_drops_whole_writes() {
    let key = Key::from_static("conformance/load");
    let len = wire_len(key, &[0; 4]);
    let (mut a, mut b, ab) = pair(Some(2 * len), None);
    for n in 0..3u32 {
        a.publish(key, &n.to_le_bytes()).await.expect("publish");
    }
    assert_eq!(ab.borrow().dropped_writes, 1);
    let mut buf = [0u8; RECV_BUF_LEN];
    for n in 0..2u32 {
        let s = b.try_recv(&mut buf).expect("try_recv").expect("sample");
        assert_eq!(s.payload, n.to_le_bytes());
    }
    assert!(b.try_recv(&mut buf).expect("try_recv").is_none());
}
