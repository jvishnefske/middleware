//! Connection-oriented BLE transport over GATT.
//!
//! [`GattLink`] is the only contact with the radio: write a characteristic
//! value (client role), notify a characteristic (server role), receive the
//! next inbound event, and report the negotiated ATT MTU. Bindings for
//! trouble-host, nrf-softdevice or BlueZ implement it; tests use an
//! in-memory fake.
//!
//! [`BleTransport`] layers the middleware traits on a link. Every message is
//! split into [`frame`](super::frame) fragments of at most `ATT_MTU - 3`
//! bytes and reassembled into an `N`-byte buffer.
//!
//! * [`Publisher`]: notifies the fragments on [`characteristic_uuid`]`(key)`.
//! * [`Subscriber`]: accepts notifications whose UUID belongs to one of the
//!   keys given to [`BleTransport::with_subscriptions`] and maps the UUID
//!   back to that key. Only one message is reassembled at a time; a fragment
//!   on another subscribed characteristic restarts reassembly with it.
//! * [`Requester`]: writes a request frame (key in the body) to
//!   [`RPC_CHARACTERISTIC_UUID`] with a fresh correlation id, then returns
//!   the response notified on that characteristic with the same id.
//!   Responses with other ids (stale) are discarded. A handler error in the
//!   response surfaces as [`BleError::Handler`].
//! * [`Responder`]: reassembles one request written to the RPC
//!   characteristic, runs the [`Handler`] when the key was given to
//!   [`BleTransport::with_services`] (otherwise answers
//!   [`HandlerError::NotFound`]; an undecodable request is answered
//!   [`HandlerError::InvalidRequest`]) and notifies the fragmented response.
//!   A response too large to frame is replaced by
//!   [`HandlerError::BufferTooSmall`]. When a request fragment is rejected
//!   the responder answers that request id with an error status and returns
//!   the [`FrameError`].
//!
//! Inbound events that a call does not want (other characteristics, wrong
//! direction, stale ids) are dropped. Each call handles at most
//! [`MAX_EVENTS_PER_CALL`] events before failing with
//! [`BleError::EventLimit`], so every loop is bounded.

use super::frame::{
    FragmentHeader, Fragmenter, FrameError, MAX_FRAGMENTS, Progress, Reassembler, parse_request,
    parse_response,
};
use super::uuid::{RPC_CHARACTERISTIC_UUID, Uuid128, characteristic_uuid};
use crate::key::Key;
use crate::transport::{
    Handler, HandlerError, Publisher, Requester, Responder, Sample, Subscriber, copy_into,
    sample_into,
};

/// Largest ATT payload (`517 - 3`), the size of the per-call fragment buffers.
pub const MAX_ATT_PAYLOAD: usize = 514;

/// Upper bound on inbound events processed by one call.
pub const MAX_EVENTS_PER_CALL: usize = MAX_FRAGMENTS + 256;

/// A negotiated ATT MTU, guaranteed to lie in `23..=517`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttMtu(u16);

impl AttMtu {
    /// The minimum (and default) ATT MTU.
    pub const DEFAULT: AttMtu = AttMtu(23);
    /// The largest MTU the ATT protocol allows.
    pub const MAX: AttMtu = AttMtu(517);

    /// Wraps `mtu` if it is a legal ATT MTU.
    pub const fn new(mtu: u16) -> Option<Self> {
        if mtu >= Self::DEFAULT.0 && mtu <= Self::MAX.0 {
            Some(AttMtu(mtu))
        } else {
            None
        }
    }

    /// The MTU value.
    pub const fn get(self) -> u16 {
        self.0
    }

    /// Bytes available to one write or notification value (`MTU - 3`).
    pub const fn payload(self) -> usize {
        self.0 as usize - 3
    }
}

/// How an inbound value arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboundKind {
    /// A peer wrote one of our characteristics (we are the GATT server).
    Written,
    /// A peer notified a characteristic we subscribed to (we are the client).
    Notified,
}

/// One inbound characteristic value; `data` borrows the receive buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inbound<'b> {
    /// Write or notification.
    pub kind: InboundKind,
    /// The characteristic the value belongs to.
    pub uuid: Uuid128,
    /// The value.
    pub data: &'b [u8],
}

/// The radio as seen by [`BleTransport`]: one connection to one peer.
pub trait GattLink {
    /// Binding-specific failure.
    type Error: core::fmt::Debug;

    /// The ATT MTU negotiated for the connection.
    fn att_mtu(&self) -> AttMtu;

    /// Writes `data` to the peer's characteristic `uuid` (client role).
    async fn write(&mut self, uuid: Uuid128, data: &[u8]) -> Result<(), Self::Error>;

    /// Notifies subscribers of characteristic `uuid` with `data` (server role).
    async fn notify(&mut self, uuid: Uuid128, data: &[u8]) -> Result<(), Self::Error>;

    /// Waits for the next write or notification and copies its value into
    /// `buf` (at most [`MAX_ATT_PAYLOAD`] bytes are offered).
    async fn receive<'b>(&mut self, buf: &'b mut [u8]) -> Result<Inbound<'b>, Self::Error>;
}

/// Failure of a [`BleTransport`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BleError<E> {
    /// The link failed.
    Link(E),
    /// A fragment or message could not be framed or was rejected.
    Frame(FrameError),
    /// The remote handler answered with an error.
    Handler(HandlerError),
    /// The caller's buffer cannot hold the received message.
    BufferTooSmall,
    /// [`MAX_EVENTS_PER_CALL`] inbound events passed without completing the call.
    EventLimit,
}

impl<E> From<FrameError> for BleError<E> {
    fn from(e: FrameError) -> Self {
        BleError::Frame(e)
    }
}

/// A message id that advances by wrapping increment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CorrelationId(u8);

impl CorrelationId {
    /// The raw id.
    pub const fn get(self) -> u8 {
        self.0
    }

    /// The id that follows this one.
    pub const fn next(self) -> Self {
        CorrelationId(self.0.wrapping_add(1))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outbound {
    Write,
    Notify,
}

/// Middleware transport over one GATT connection, reassembling messages of
/// up to `N` bytes.
pub struct BleTransport<'a, L, const N: usize> {
    link: L,
    subscriptions: &'a [Key<'a>],
    services: &'a [Key<'a>],
    next_id: CorrelationId,
}

impl<'a, L: GattLink, const N: usize> BleTransport<'a, L, N> {
    /// Wraps `link` with no subscriptions and no served keys.
    pub const fn new(link: L) -> Self {
        BleTransport {
            link,
            subscriptions: &[],
            services: &[],
            next_id: CorrelationId(0),
        }
    }

    /// Sets the keys [`Subscriber::recv`] listens to.
    pub fn with_subscriptions(self, keys: &'a [Key<'a>]) -> Self {
        BleTransport {
            subscriptions: keys,
            ..self
        }
    }

    /// Sets the keys [`Responder::serve_one`] answers.
    pub fn with_services(self, keys: &'a [Key<'a>]) -> Self {
        BleTransport {
            services: keys,
            ..self
        }
    }

    /// Borrows the link.
    pub fn link_mut(&mut self) -> &mut L {
        &mut self.link
    }

    /// Returns the link.
    pub fn into_link(self) -> L {
        self.link
    }

    fn take_id(&mut self) -> u8 {
        let id = self.next_id;
        self.next_id = id.next();
        id.get()
    }

    fn att_payload(&self) -> usize {
        self.link.att_mtu().payload().min(MAX_ATT_PAYLOAD)
    }

    async fn send(
        &mut self,
        how: Outbound,
        uuid: Uuid128,
        frames: &Fragmenter<'_>,
    ) -> Result<(), BleError<L::Error>> {
        let mut out = [0u8; MAX_ATT_PAYLOAD];
        for i in 0..frames.count() {
            let fragment = frames.fragment(i, &mut out)?;
            match how {
                Outbound::Write => self.link.write(uuid, fragment).await,
                Outbound::Notify => self.link.notify(uuid, fragment).await,
            }
            .map_err(BleError::Link)?;
        }
        Ok(())
    }

    async fn respond(
        &mut self,
        id: u8,
        outcome: Result<&[u8], HandlerError>,
    ) -> Result<(), BleError<L::Error>> {
        let att = self.att_payload();
        let frames = match Fragmenter::response(id, outcome, att) {
            Err(FrameError::TooLarge) => {
                Fragmenter::response(id, Err(HandlerError::BufferTooSmall), att)?
            }
            other => other?,
        };
        self.send(Outbound::Notify, RPC_CHARACTERISTIC_UUID, &frames)
            .await
    }
}

impl<L: GattLink, const N: usize> Publisher for BleTransport<'_, L, N> {
    type Error = BleError<L::Error>;

    async fn publish(&mut self, key: Key<'_>, payload: &[u8]) -> Result<(), Self::Error> {
        let id = self.take_id();
        let frames = Fragmenter::message(id, payload, self.att_payload())?;
        self.send(Outbound::Notify, characteristic_uuid(key), &frames)
            .await
    }
}

impl<L: GattLink, const N: usize> Subscriber for BleTransport<'_, L, N> {
    type Error = BleError<L::Error>;

    async fn recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Sample<'b>, Self::Error> {
        let mut rx = [0u8; MAX_ATT_PAYLOAD];
        let mut pending: Option<(usize, Reassembler<N>)> = None;
        for _ in 0..MAX_EVENTS_PER_CALL {
            let ev = self.link.receive(&mut rx).await.map_err(BleError::Link)?;
            if ev.kind != InboundKind::Notified {
                continue;
            }
            let Some(slot) = self
                .subscriptions
                .iter()
                .position(|k| characteristic_uuid(*k) == ev.uuid)
            else {
                continue;
            };
            let state = match pending.take() {
                Some((s, r)) if s == slot => r,
                _ => Reassembler::new(),
            };
            match state.accept(ev.data)? {
                Progress::Partial(r) => pending = Some((slot, r)),
                Progress::Complete(m) => {
                    let key = self.subscriptions.get(slot).ok_or(BleError::EventLimit)?;
                    return sample_into(buf, key.as_str(), m.as_bytes())
                        .ok_or(BleError::BufferTooSmall);
                }
            }
        }
        Err(BleError::EventLimit)
    }
}

impl<L: GattLink, const N: usize> Requester for BleTransport<'_, L, N> {
    type Error = BleError<L::Error>;

    async fn request<'b>(
        &mut self,
        key: Key<'_>,
        request: &[u8],
        response: &'b mut [u8],
    ) -> Result<&'b [u8], Self::Error> {
        let id = self.take_id();
        let frames = Fragmenter::request(id, key, request, self.att_payload())?;
        self.send(Outbound::Write, RPC_CHARACTERISTIC_UUID, &frames)
            .await?;

        let mut rx = [0u8; MAX_ATT_PAYLOAD];
        let mut state = Reassembler::<N>::new();
        for _ in 0..MAX_EVENTS_PER_CALL {
            let ev = self.link.receive(&mut rx).await.map_err(BleError::Link)?;
            if ev.kind != InboundKind::Notified || ev.uuid != RPC_CHARACTERISTIC_UUID {
                continue;
            }
            match FragmentHeader::parse(ev.data) {
                Ok((h, _)) if h.id == id => {}
                _ => continue, // stale or unrelated response
            }
            match state.accept(ev.data)? {
                Progress::Partial(r) => state = r,
                Progress::Complete(m) => {
                    return match parse_response(m.as_bytes())? {
                        Ok(payload) => copy_into(response, payload)
                            .map(|s| &*s)
                            .ok_or(BleError::BufferTooSmall),
                        Err(e) => Err(BleError::Handler(e)),
                    };
                }
            }
        }
        Err(BleError::EventLimit)
    }
}

impl<L: GattLink, const N: usize> Responder for BleTransport<'_, L, N> {
    type Error = BleError<L::Error>;

    async fn serve_one<H: Handler>(
        &mut self,
        handler: &mut H,
        scratch: &mut [u8],
    ) -> Result<(), Self::Error> {
        let mut rx = [0u8; MAX_ATT_PAYLOAD];
        let mut state = Reassembler::<N>::new();
        for _ in 0..MAX_EVENTS_PER_CALL {
            let ev = self.link.receive(&mut rx).await.map_err(BleError::Link)?;
            if ev.kind != InboundKind::Written || ev.uuid != RPC_CHARACTERISTIC_UUID {
                continue;
            }
            let header = FragmentHeader::parse(ev.data);
            match state.accept(ev.data) {
                Ok(Progress::Partial(r)) => state = r,
                Ok(Progress::Complete(m)) => {
                    let outcome = match parse_request(m.as_bytes()) {
                        Err(_) => Err(HandlerError::InvalidRequest),
                        Ok((key, _)) if !self.services.contains(&key) => {
                            Err(HandlerError::NotFound)
                        }
                        Ok((key, payload)) => handler
                            .handle(key, payload, scratch)
                            .and_then(|n| scratch.get(..n).ok_or(HandlerError::BufferTooSmall)),
                    };
                    return self.respond(m.id(), outcome).await;
                }
                Err(e) => {
                    if let Ok((h, _)) = header {
                        let status = match e {
                            FrameError::Oversized => HandlerError::BufferTooSmall,
                            _ => HandlerError::InvalidRequest,
                        };
                        self.respond(h.id, Err(status)).await?;
                    }
                    return Err(BleError::Frame(e));
                }
            }
        }
        Err(BleError::EventLimit)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::ble::frame::{Message, STATUS_OK};
    use core::cell::RefCell;
    use core::task::{Poll, Waker};
    use futures::executor::block_on;
    use std::collections::VecDeque;
    use std::rc::Rc;
    use std::vec::Vec;

    const TEMP: Key<'static> = Key::from_static("sensor/temperature");
    const HUM: Key<'static> = Key::from_static("sensor/humidity");
    const ECHO: Key<'static> = Key::from_static("svc/Echo");

    type Event = (InboundKind, Uuid128, Vec<u8>);
    /// Inbound events plus the waker of a receiver blocked on them.
    type Queue = Rc<RefCell<(VecDeque<Event>, Option<Waker>)>>;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum FakeError {
        /// The scripted inbound queue is empty.
        Exhausted,
    }

    /// One end of an in-memory link. Outbound values are logged and, when a
    /// peer queue is attached, delivered to it. An empty inbound queue either
    /// fails (scripted tests) or waits for the peer (loopback tests).
    struct FakeLink {
        mtu: AttMtu,
        inbound: Queue,
        peer: Option<Queue>,
        sent: Vec<(Outbound, Uuid128, Vec<u8>)>,
    }

    impl FakeLink {
        fn scripted(mtu: u16, events: Vec<Event>) -> Self {
            FakeLink {
                mtu: AttMtu::new(mtu).unwrap(),
                inbound: Rc::new(RefCell::new((events.into(), None))),
                peer: None,
                sent: Vec::new(),
            }
        }

        fn pair(mtu: u16) -> (Self, Self) {
            let a = FakeLink::scripted(mtu, Vec::new());
            let b = FakeLink::scripted(mtu, Vec::new());
            let a2 = FakeLink {
                peer: Some(b.inbound.clone()),
                ..a
            };
            let b2 = FakeLink {
                peer: Some(a2.inbound.clone()),
                ..b
            };
            (a2, b2)
        }

        fn deliver(&mut self, how: Outbound, uuid: Uuid128, data: &[u8]) {
            self.sent.push((how, uuid, data.to_vec()));
            if let Some(peer) = &self.peer {
                let kind = match how {
                    Outbound::Write => InboundKind::Written,
                    Outbound::Notify => InboundKind::Notified,
                };
                let mut q = peer.borrow_mut();
                q.0.push_back((kind, uuid, data.to_vec()));
                if let Some(w) = q.1.take() {
                    w.wake();
                }
            }
        }
    }

    impl GattLink for FakeLink {
        type Error = FakeError;

        fn att_mtu(&self) -> AttMtu {
            self.mtu
        }

        async fn write(&mut self, uuid: Uuid128, data: &[u8]) -> Result<(), FakeError> {
            assert!(data.len() <= self.mtu.payload());
            self.deliver(Outbound::Write, uuid, data);
            Ok(())
        }

        async fn notify(&mut self, uuid: Uuid128, data: &[u8]) -> Result<(), FakeError> {
            assert!(data.len() <= self.mtu.payload());
            self.deliver(Outbound::Notify, uuid, data);
            Ok(())
        }

        async fn receive<'b>(&mut self, buf: &'b mut [u8]) -> Result<Inbound<'b>, FakeError> {
            let waits = self.peer.is_some();
            let inbound = self.inbound.clone();
            let (kind, uuid, data) = core::future::poll_fn(|cx| {
                let mut q = inbound.borrow_mut();
                match q.0.pop_front() {
                    Some(ev) => Poll::Ready(Ok(ev)),
                    None if waits => {
                        q.1 = Some(cx.waker().clone());
                        Poll::Pending
                    }
                    None => Poll::Ready(Err(FakeError::Exhausted)),
                }
            })
            .await?;
            let out = buf.get_mut(..data.len()).unwrap();
            out.copy_from_slice(&data);
            Ok(Inbound {
                kind,
                uuid,
                data: out,
            })
        }
    }

    fn frames(f: &Fragmenter<'_>) -> Vec<Vec<u8>> {
        let mut out = [0u8; MAX_ATT_PAYLOAD];
        (0..f.count())
            .map(|i| f.fragment(i, &mut out).unwrap().to_vec())
            .collect()
    }

    fn reassemble<const N: usize>(frags: &[Vec<u8>]) -> Message<N> {
        let mut r = Reassembler::<N>::new();
        for f in frags {
            match r.accept(f).unwrap() {
                Progress::Partial(n) => r = n,
                Progress::Complete(m) => return m,
            }
        }
        panic!("incomplete");
    }

    fn notified(uuid: Uuid128, f: &Fragmenter<'_>) -> Vec<Event> {
        frames(f)
            .into_iter()
            .map(|d| (InboundKind::Notified, uuid, d))
            .collect()
    }

    fn written(uuid: Uuid128, f: &Fragmenter<'_>) -> Vec<Event> {
        frames(f)
            .into_iter()
            .map(|d| (InboundKind::Written, uuid, d))
            .collect()
    }

    fn echo(_k: Key<'_>, req: &[u8], resp: &mut [u8]) -> Result<usize, HandlerError> {
        let out = resp
            .get_mut(..req.len())
            .ok_or(HandlerError::BufferTooSmall)?;
        out.copy_from_slice(req);
        Ok(req.len())
    }

    #[test]
    fn att_mtu_is_range_checked() {
        assert_eq!(AttMtu::new(22), None);
        assert_eq!(AttMtu::new(518), None);
        assert_eq!(AttMtu::new(23).unwrap().payload(), 20);
        assert_eq!(AttMtu::MAX.payload(), MAX_ATT_PAYLOAD);
        assert_eq!(CorrelationId(255).next().get(), 0);
    }

    #[test]
    fn publish_notifies_fragments_on_key_characteristic() {
        let payload: Vec<u8> = (0..50u8).collect();
        let mut t: BleTransport<_, 64> = BleTransport::new(FakeLink::scripted(23, Vec::new()));
        block_on(t.publish(TEMP, &payload)).unwrap();
        let sent = &t.link_mut().sent;
        assert!(sent.len() > 1, "50 bytes must be fragmented at MTU 23");
        assert!(sent.iter().all(|(how, uuid, d)| *how == Outbound::Notify
            && *uuid == characteristic_uuid(TEMP)
            && d.len() <= 20));
        let frags: Vec<Vec<u8>> = sent.iter().map(|s| s.2.clone()).collect();
        assert_eq!(reassemble::<64>(&frags).as_bytes(), &payload[..]);
    }

    #[test]
    fn subscribe_yields_notification_for_subscribed_key() {
        let keys = [TEMP, HUM];
        let payload: Vec<u8> = (0..40u8).collect();
        let mut events = Vec::new();
        // Ignored: not subscribed, wrong direction, and the RPC characteristic.
        events.extend(notified(
            characteristic_uuid(Key::from_static("other")),
            &Fragmenter::message(0, b"no", 20).unwrap(),
        ));
        events.extend(written(
            characteristic_uuid(HUM),
            &Fragmenter::message(0, b"no", 20).unwrap(),
        ));
        events.extend(notified(
            RPC_CHARACTERISTIC_UUID,
            &Fragmenter::message(0, b"no", 20).unwrap(),
        ));
        events.extend(notified(
            characteristic_uuid(HUM),
            &Fragmenter::message(5, &payload, 20).unwrap(),
        ));
        let mut t: BleTransport<_, 64> =
            BleTransport::new(FakeLink::scripted(23, events)).with_subscriptions(&keys);
        let mut buf = [0u8; 128];
        let s = block_on(t.recv(&mut buf)).unwrap();
        assert_eq!(s.key, HUM);
        assert_eq!(s.payload, &payload[..]);
    }

    #[test]
    fn subscribe_reports_small_buffer_and_bad_fragments() {
        let keys = [TEMP];
        let f = Fragmenter::message(0, b"0123456789", 20).unwrap();
        let mut t: BleTransport<_, 64> = BleTransport::new(FakeLink::scripted(
            23,
            notified(characteristic_uuid(TEMP), &f),
        ))
        .with_subscriptions(&keys);
        let mut small = [0u8; 8];
        assert_eq!(block_on(t.recv(&mut small)), Err(BleError::BufferTooSmall));

        let big = [7u8; 80];
        let f = Fragmenter::message(0, &big, 20).unwrap();
        let events = notified(characteristic_uuid(TEMP), &f);
        let mut t: BleTransport<_, 64> =
            BleTransport::new(FakeLink::scripted(23, events)).with_subscriptions(&keys);
        let mut buf = [0u8; 128];
        assert_eq!(
            block_on(t.recv(&mut buf)),
            Err(BleError::Frame(FrameError::Oversized))
        );
    }

    #[test]
    fn subscriber_drops_unrelated_events_up_to_a_bound() {
        let keys = [TEMP];
        let junk = Fragmenter::message(0, b"x", 20).unwrap();
        let events: Vec<Event> = (0..MAX_EVENTS_PER_CALL)
            .flat_map(|_| notified(characteristic_uuid(HUM), &junk))
            .collect();
        let mut t: BleTransport<_, 16> =
            BleTransport::new(FakeLink::scripted(23, events)).with_subscriptions(&keys);
        let mut buf = [0u8; 32];
        assert_eq!(block_on(t.recv(&mut buf)), Err(BleError::EventLimit));
    }

    #[test]
    fn request_carries_correlation_id_and_discards_stale_responses() {
        // The transport's first correlation id is 0; script a stale response
        // (id 9) that must be discarded before the matching one.
        let mut events = notified(
            RPC_CHARACTERISTIC_UUID,
            &Fragmenter::response(9, Ok(b"stale"), 20).unwrap(),
        );
        events.extend(notified(
            characteristic_uuid(TEMP),
            &Fragmenter::message(0, b"pub", 20).unwrap(),
        ));
        let answer: Vec<u8> = (100..140u8).collect();
        events.extend(notified(
            RPC_CHARACTERISTIC_UUID,
            &Fragmenter::response(0, Ok(&answer), 20).unwrap(),
        ));
        let mut t: BleTransport<_, 64> = BleTransport::new(FakeLink::scripted(23, events));
        let request: Vec<u8> = (0..30u8).collect();
        let mut resp = [0u8; 64];
        let got = block_on(t.request(ECHO, &request, &mut resp)).unwrap();
        assert_eq!(got, &answer[..]);

        let sent = &t.link_mut().sent;
        assert!(
            sent.iter()
                .all(|(how, uuid, _)| *how == Outbound::Write && *uuid == RPC_CHARACTERISTIC_UUID)
        );
        let frags: Vec<Vec<u8>> = sent.iter().map(|s| s.2.clone()).collect();
        let m = reassemble::<64>(&frags);
        assert_eq!(m.id(), 0);
        assert_eq!(parse_request(m.as_bytes()).unwrap(), (ECHO, &request[..]));
    }

    #[test]
    fn successive_requests_use_new_correlation_ids() {
        let mut events = notified(
            RPC_CHARACTERISTIC_UUID,
            &Fragmenter::response(0, Ok(b"a"), 20).unwrap(),
        );
        // A late duplicate of response 0 arrives before response 1.
        events.extend(notified(
            RPC_CHARACTERISTIC_UUID,
            &Fragmenter::response(0, Ok(b"a"), 20).unwrap(),
        ));
        events.extend(notified(
            RPC_CHARACTERISTIC_UUID,
            &Fragmenter::response(1, Ok(b"b"), 20).unwrap(),
        ));
        let mut t: BleTransport<_, 16> = BleTransport::new(FakeLink::scripted(23, events));
        let mut resp = [0u8; 16];
        assert_eq!(block_on(t.request(ECHO, b"1", &mut resp)).unwrap(), b"a");
        assert_eq!(block_on(t.request(ECHO, b"2", &mut resp)).unwrap(), b"b");
    }

    #[test]
    fn handler_error_reaches_requester() {
        let events = notified(
            RPC_CHARACTERISTIC_UUID,
            &Fragmenter::response(0, Err(HandlerError::Internal), 20).unwrap(),
        );
        let mut t: BleTransport<_, 16> = BleTransport::new(FakeLink::scripted(23, events));
        let mut resp = [0u8; 16];
        assert_eq!(
            block_on(t.request(ECHO, b"x", &mut resp)),
            Err(BleError::Handler(HandlerError::Internal))
        );
    }

    #[test]
    fn requester_reports_small_buffer_and_link_errors() {
        let events = notified(
            RPC_CHARACTERISTIC_UUID,
            &Fragmenter::response(0, Ok(b"too long"), 20).unwrap(),
        );
        let mut t: BleTransport<_, 16> = BleTransport::new(FakeLink::scripted(23, events));
        let mut resp = [0u8; 4];
        assert_eq!(
            block_on(t.request(ECHO, b"x", &mut resp)),
            Err(BleError::BufferTooSmall)
        );
        // No more scripted events: the link error surfaces.
        assert_eq!(
            block_on(t.request(ECHO, b"x", &mut resp)),
            Err(BleError::Link(FakeError::Exhausted))
        );
    }

    #[test]
    fn responder_answers_with_handler_output() {
        let request: Vec<u8> = (0..33u8).collect();
        let events = written(
            RPC_CHARACTERISTIC_UUID,
            &Fragmenter::request(77, ECHO, &request, 20).unwrap(),
        );
        let services = [ECHO];
        let mut t: BleTransport<_, 64> =
            BleTransport::new(FakeLink::scripted(23, events)).with_services(&services);
        let mut scratch = [0u8; 64];
        block_on(t.serve_one(&mut echo, &mut scratch)).unwrap();
        let sent = &t.link_mut().sent;
        assert!(
            sent.iter()
                .all(|(how, uuid, _)| *how == Outbound::Notify && *uuid == RPC_CHARACTERISTIC_UUID)
        );
        let frags: Vec<Vec<u8>> = sent.iter().map(|s| s.2.clone()).collect();
        let m = reassemble::<64>(&frags);
        assert_eq!(m.id(), 77);
        assert_eq!(m.as_bytes()[0], STATUS_OK);
        assert_eq!(parse_response(m.as_bytes()).unwrap(), Ok(&request[..]));
    }

    fn served_status(
        services: &[Key<'_>],
        events: Vec<Event>,
        scratch: usize,
    ) -> (Result<(), BleError<FakeError>>, Message<16>) {
        let mut t: BleTransport<_, 32> =
            BleTransport::new(FakeLink::scripted(23, events)).with_services(services);
        let mut scratch_buf = [0u8; 64];
        let r = block_on(t.serve_one(&mut echo, &mut scratch_buf[..scratch]));
        let frags: Vec<Vec<u8>> = t.link_mut().sent.iter().map(|s| s.2.clone()).collect();
        (r, reassemble::<16>(&frags))
    }

    #[test]
    fn responder_maps_failures_to_status_codes() {
        let req = |k| {
            written(
                RPC_CHARACTERISTIC_UUID,
                &Fragmenter::request(4, k, b"abc", 20).unwrap(),
            )
        };
        // Unknown key → NotFound.
        let (r, m) = served_status(&[ECHO], req(TEMP), 64);
        assert_eq!(r, Ok(()));
        assert_eq!(
            parse_response(m.as_bytes()).unwrap(),
            Err(HandlerError::NotFound)
        );
        // Handler error (response does not fit scratch) → its code.
        let (r, m) = served_status(&[ECHO], req(ECHO), 2);
        assert_eq!(r, Ok(()));
        assert_eq!(m.id(), 4);
        assert_eq!(
            parse_response(m.as_bytes()).unwrap(),
            Err(HandlerError::BufferTooSmall)
        );
        // Undecodable request body → InvalidRequest.
        let bad = written(
            RPC_CHARACTERISTIC_UUID,
            &Fragmenter::message(5, &[9, b'a'], 20).unwrap(),
        );
        let (r, m) = served_status(&[ECHO], bad, 64);
        assert_eq!(r, Ok(()));
        assert_eq!(
            parse_response(m.as_bytes()).unwrap(),
            Err(HandlerError::InvalidRequest)
        );
    }

    #[test]
    fn responder_rejects_oversized_request_and_tells_requester() {
        let big = [0u8; 40];
        let events = written(
            RPC_CHARACTERISTIC_UUID,
            &Fragmenter::request(6, ECHO, &big, 20).unwrap(),
        );
        let (r, m) = served_status(&[ECHO], events, 64);
        assert_eq!(r, Err(BleError::Frame(FrameError::Oversized)));
        assert_eq!(m.id(), 6);
        assert_eq!(
            parse_response(m.as_bytes()).unwrap(),
            Err(HandlerError::BufferTooSmall)
        );
    }

    #[test]
    fn responder_rejects_duplicate_fragment() {
        let mut events = written(
            RPC_CHARACTERISTIC_UUID,
            &Fragmenter::request(8, ECHO, &[1u8; 30], 20).unwrap(),
        );
        let first = events[0].clone();
        events.insert(1, first);
        let mut t: BleTransport<_, 64> =
            BleTransport::new(FakeLink::scripted(23, events)).with_services(&[ECHO]);
        let mut scratch = [0u8; 64];
        assert_eq!(
            block_on(t.serve_one(&mut echo, &mut scratch)),
            Err(BleError::Frame(FrameError::Duplicate))
        );
    }

    #[test]
    fn end_to_end_request_over_in_memory_link() {
        let (client_link, server_link) = FakeLink::pair(23);
        let services = [ECHO];
        let mut client: BleTransport<_, 128> = BleTransport::new(client_link);
        let mut server: BleTransport<_, 128> =
            BleTransport::new(server_link).with_services(&services);
        let request: Vec<u8> = (0..90u8).collect();
        let mut resp = [0u8; 128];
        let mut scratch = [0u8; 128];
        let (got, served) = block_on(futures::future::join(
            client.request(ECHO, &request, &mut resp),
            server.serve_one(&mut echo, &mut scratch),
        ));
        served.unwrap();
        assert_eq!(got.unwrap(), &request[..]);
    }

    #[test]
    fn end_to_end_publish_subscribe_over_in_memory_link() {
        let (pub_link, sub_link) = FakeLink::pair(64);
        let keys = [TEMP];
        let mut publisher: BleTransport<_, 128> = BleTransport::new(pub_link);
        let mut subscriber: BleTransport<_, 128> =
            BleTransport::new(sub_link).with_subscriptions(&keys);
        let payload = [0x5Au8; 100];
        let mut buf = [0u8; 160];
        let (p, s) = block_on(futures::future::join(
            publisher.publish(TEMP, &payload),
            subscriber.recv(&mut buf),
        ));
        p.unwrap();
        let s = s.unwrap();
        assert_eq!(s.key, TEMP);
        assert_eq!(s.payload, &payload[..]);
    }
}
