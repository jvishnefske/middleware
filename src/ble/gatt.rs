//! Connection-oriented BLE transport over GATT.
//!
//! The radio is reached through one of two role-specific traits, so a binding
//! implements only what its role can do:
//!
//! * [`GattServer`] (peripheral): notify a characteristic, receive values a
//!   peer wrote, report the ATT MTU.
//! * [`GattClient`] (central): write a peer's characteristic, receive
//!   notifications (waiting or not), report the ATT MTU.
//!
//! Both report connection lifecycle as [`Inbound::Connected`] and
//! [`Inbound::Disconnected`]. The ATT MTU belongs to the current connection
//! and may change at any time (an MTU exchange, a reconnect), so
//! `att_mtu(&self)` is read again at the start of every outbound message;
//! fragments of one message all use the MTU read at its start. A binding
//! reports [`AttMtu::DEFAULT`] while no MTU was negotiated. Bindings for
//! trouble-host, nrf-softdevice or BlueZ implement the traits; tests use
//! in-memory fakes.
//!
//! Every message is split into [`frame`](super::frame) fragments of at most
//! `min(ATT_MTU - 3, A)` bytes and reassembled into an `N`-byte buffer. `A`
//! (default [`MAX_ATT_PAYLOAD`]) sizes the per-call fragment buffers, so a
//! device that never negotiates a large MTU sets it to e.g. 20 or 244 instead
//! of paying for 514-byte buffers.
//!
//! [`BleServer`] (GATT server role):
//!
//! * [`Publisher`]: notifies the fragments on [`characteristic_uuid`]`(key)`.
//!   [`Publisher::LIMITS`] gives the largest payload the peer's `N`-byte
//!   reassembly accepts (both sides are configured with the same `N`).
//! * [`Responder`]: reassembles one request written to
//!   [`RPC_CHARACTERISTIC_UUID`], runs the [`Handler`] on its key and
//!   notifies the fragmented response. The handler decides which keys exist
//!   (answering [`HandlerError::NotFound`] for the others); there is no
//!   separate service list, so dispatch happens in one place. An undecodable
//!   request is answered [`HandlerError::InvalidRequest`], and a response too
//!   large to frame is replaced by [`HandlerError::BufferTooSmall`]. When a
//!   request fragment is rejected the responder answers that request id with
//!   an error status and returns the [`FrameError`].
//!
//! [`BleClient`] (GATT client role):
//!
//! * [`Subscriber`]: accepts notifications whose UUID belongs to one of the
//!   keys given to [`BleClient::with_subscriptions`] and maps the UUID back to
//!   that key. One publication is reassembled at a time: fragment 0 of
//!   another publication (on any subscribed characteristic) restarts
//!   reassembly with it, and a fragment that continues no publication in
//!   progress is dropped.
//! * [`Requester`]: writes a request frame (key in the body) to
//!   [`RPC_CHARACTERISTIC_UUID`] with a fresh correlation id, then returns
//!   the response notified on that characteristic with the same id.
//!   Responses with other ids (stale) are discarded. A handler error in the
//!   response surfaces as [`BleError::Handler`]; a disconnect while waiting
//!   is [`BleError::Disconnected`].
//!
//! # Cancel safety
//!
//! Every await point is a link call, and all state changes after an inbound
//! event happen synchronously, so with a cancel-safe `receive`:
//!
//! * [`BleServer::serve_one`](Responder::serve_one) keeps the partial request
//!   in `self`; a dropped call resumes it on the next call. A partial request
//!   is discarded on [`Inbound::Connected`] / [`Inbound::Disconnected`], or
//!   when fragment 0 of another request arrives (the requester gave up on the
//!   old one). A fragment that continues no request in progress (the tail of
//!   a request that was abandoned, rejected or cut by a disconnect) is
//!   dropped, so one bad request never poisons the next. Dropping the call while the response is being notified
//!   abandons the rest of the response; the requester discards it as stale.
//! * [`BleClient::recv`](Subscriber::recv) keeps partial and completed
//!   publications in `self`, so a dropped call loses no sample. A publication
//!   that completes while a request is waiting for its response is kept and
//!   returned by the next `recv` / `try_recv`; only one is held, and further
//!   ones that complete before it is taken are dropped. A disconnect
//!   discards a partial publication.
//! * [`BleClient::request`](Requester::request) abandons the request when
//!   dropped; the next request has a new correlation id, so a late response
//!   is discarded.
//! * `publish` may have notified some fragments when dropped; it never sends
//!   a fragment twice.
//!
//! A sample or response that does not fit the caller's buffer is
//! [`BleError::BufferTooSmall`] and is dropped. Inbound events that a call
//! does not want (other characteristics, stale ids) are dropped. Each call
//! handles at most [`MAX_EVENTS_PER_CALL`] events before failing with
//! [`BleError::EventLimit`], so every loop is bounded.

use super::frame::{
    FragmentHeader, Fragmenter, FrameError, HEADER_LEN, MAX_FRAGMENTS, Message, Progress,
    Reassembler, parse_request, parse_response,
};
use super::uuid::{RPC_CHARACTERISTIC_UUID, Uuid128, characteristic_uuid};
use crate::key::{Key, MAX_KEY_LEN};
use crate::transport::{
    Handler, HandlerError, Limits, Publisher, Requester, Responder, Sample, Subscriber, copy_into,
    sample_into,
};

/// Largest ATT payload (`517 - 3`), the default fragment buffer size `A`.
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

/// One inbound event; `data` borrows the receive buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inbound<'b> {
    /// A characteristic value: written by the peer (server role) or notified
    /// by it (client role).
    Value {
        /// The characteristic the value belongs to.
        uuid: Uuid128,
        /// The value.
        data: &'b [u8],
    },
    /// A peer connected (or the link was re-established).
    Connected,
    /// The peer disconnected.
    Disconnected,
}

/// The radio in the GATT server (peripheral) role.
pub trait GattServer {
    /// Binding-specific failure.
    type Error: core::fmt::Debug;

    /// The ATT MTU of the current connection (see the module docs: it may
    /// change between calls).
    fn att_mtu(&self) -> AttMtu;

    /// Notifies subscribers of characteristic `uuid` with `data`. `Ok` means
    /// the stack accepted the value; a peer that has not enabled
    /// notifications does not receive it.
    async fn notify(&mut self, uuid: Uuid128, data: &[u8]) -> Result<(), Self::Error>;

    /// Waits for the next written value or lifecycle event and copies a value
    /// into `buf` (at most `A` bytes are offered). Must be cancel-safe:
    /// dropping the future before it is ready loses no event.
    async fn receive<'b>(&mut self, buf: &'b mut [u8]) -> Result<Inbound<'b>, Self::Error>;
}

/// The radio in the GATT client (central) role.
pub trait GattClient {
    /// Binding-specific failure.
    type Error: core::fmt::Debug;

    /// The ATT MTU of the current connection (see the module docs: it may
    /// change between calls).
    fn att_mtu(&self) -> AttMtu;

    /// Writes `data` to the peer's characteristic `uuid`.
    async fn write(&mut self, uuid: Uuid128, data: &[u8]) -> Result<(), Self::Error>;

    /// Waits for the next notification or lifecycle event and copies a value
    /// into `buf` (at most `A` bytes are offered). Must be cancel-safe:
    /// dropping the future before it is ready loses no event.
    async fn receive<'b>(&mut self, buf: &'b mut [u8]) -> Result<Inbound<'b>, Self::Error>;

    /// Returns the next event if one was already received, without waiting.
    fn try_receive<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Inbound<'b>>, Self::Error>;
}

/// Failure of a [`BleServer`] or [`BleClient`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BleError<E> {
    /// The link failed.
    Link(E),
    /// A fragment or message could not be framed or was rejected.
    Frame(FrameError),
    /// The remote handler answered with an error.
    Handler(HandlerError),
    /// The caller's buffer cannot hold the received message (it is dropped).
    BufferTooSmall,
    /// [`MAX_EVENTS_PER_CALL`] inbound events passed without completing the call.
    EventLimit,
    /// The peer disconnected while a request waited for its response.
    Disconnected,
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

/// Largest message a peer reassembling into `n` bytes accepts, given
/// fragment buffers of `a` bytes: limited by `n`, the 16-bit length field and
/// [`MAX_FRAGMENTS`] fragments at the smallest ATT payload.
pub const fn max_message(n: usize, a: usize) -> usize {
    let att = if a < AttMtu::DEFAULT.payload() {
        a
    } else {
        AttMtu::DEFAULT.payload()
    };
    let by_fragments = MAX_FRAGMENTS * att.saturating_sub(HEADER_LEN);
    let mut max = n;
    if max > u16::MAX as usize {
        max = u16::MAX as usize;
    }
    if max > by_fragments {
        max = by_fragments;
    }
    max
}

/// The ATT payload used for a message: the link's, capped at `A`.
const fn att_payload<const A: usize>(mtu: AttMtu) -> usize {
    let p = mtu.payload();
    if p < A { p } else { A }
}

/// Checks at compile time that `A` holds a header and one data byte.
struct AttBuffer<const A: usize>;

impl<const A: usize> AttBuffer<A> {
    const VALID: () = assert!(A > HEADER_LEN, "A must exceed the 4-byte fragment header");
}

/// A fragment that the responder rejected.
struct Rejected {
    /// The request id, when the header was readable.
    id: Option<u8>,
    error: FrameError,
}

/// Middleware transport for the GATT server role, reassembling requests of
/// up to `N` bytes with `A`-byte fragment buffers (see the module docs).
pub struct BleServer<L, const N: usize, const A: usize = MAX_ATT_PAYLOAD> {
    link: L,
    next_id: CorrelationId,
    request: Reassembler<N>,
}

impl<L: GattServer, const N: usize, const A: usize> BleServer<L, N, A> {
    /// Wraps `link`.
    pub const fn new(link: L) -> Self {
        let () = AttBuffer::<A>::VALID;
        BleServer {
            link,
            next_id: CorrelationId(0),
            request: Reassembler::new(),
        }
    }

    /// Borrows the link.
    pub const fn link(&self) -> &L {
        &self.link
    }

    /// Borrows the link mutably.
    pub fn link_mut(&mut self) -> &mut L {
        &mut self.link
    }

    /// Returns the link.
    pub fn into_link(self) -> L {
        self.link
    }

    /// True while a request is partially received.
    pub const fn request_in_progress(&self) -> bool {
        self.request.in_progress()
    }

    fn take_id(&mut self) -> u8 {
        let id = self.next_id;
        self.next_id = id.next();
        id.get()
    }

    async fn notify_all(
        &mut self,
        uuid: Uuid128,
        frames: &Fragmenter<'_>,
    ) -> Result<(), BleError<L::Error>> {
        let mut out = [0u8; A];
        for i in 0..frames.count() {
            let fragment = frames.fragment(i, &mut out)?;
            self.link
                .notify(uuid, fragment)
                .await
                .map_err(BleError::Link)?;
        }
        Ok(())
    }

    async fn respond(
        &mut self,
        id: u8,
        outcome: Result<&[u8], HandlerError>,
    ) -> Result<(), BleError<L::Error>> {
        let att = att_payload::<A>(self.link.att_mtu());
        let frames = match Fragmenter::response(id, outcome, att) {
            Err(FrameError::TooLarge) => {
                Fragmenter::response(id, Err(HandlerError::BufferTooSmall), att)?
            }
            other => other?,
        };
        self.notify_all(RPC_CHARACTERISTIC_UUID, &frames).await
    }

    /// Feeds one request fragment to the reassembly kept in `self`.
    fn absorb(&mut self, fragment: &[u8]) -> Result<Option<Message<N>>, Rejected> {
        let (h, _) =
            FragmentHeader::parse(fragment).map_err(|error| Rejected { id: None, error })?;
        let state = core::mem::take(&mut self.request);
        let state = match state.message_id() {
            Some(current) if current == h.id => state,
            _ if h.index != 0 => {
                // A stray tail of a request that was abandoned, rejected or
                // cut by a disconnect; any partial request is kept.
                self.request = state;
                return Ok(None);
            }
            // Fragment 0 starts a request, replacing any partial one.
            _ => Reassembler::new(),
        };
        match state.accept(fragment) {
            Ok(Progress::Partial(next)) => {
                self.request = next;
                Ok(None)
            }
            Ok(Progress::Complete(m)) => Ok(Some(m)),
            Err(error) => Err(Rejected {
                id: Some(h.id),
                error,
            }),
        }
    }
}

impl<L: GattServer, const N: usize, const A: usize> Publisher for BleServer<L, N, A> {
    type Error = BleError<L::Error>;

    /// Keys are hashed into the characteristic UUID, so any valid key fits;
    /// payloads are limited by [`max_message`]`(N, A)`.
    const LIMITS: Limits = Limits::new(MAX_KEY_LEN, max_message(N, A));

    async fn publish(&mut self, key: Key<'_>, payload: &[u8]) -> Result<(), Self::Error> {
        let id = self.take_id();
        let frames = Fragmenter::message(id, payload, att_payload::<A>(self.link.att_mtu()))?;
        self.notify_all(characteristic_uuid(key), &frames).await
    }
}

impl<L: GattServer, const N: usize, const A: usize> Responder for BleServer<L, N, A> {
    type Error = BleError<L::Error>;

    async fn serve_one<H: Handler>(
        &mut self,
        handler: &mut H,
        scratch: &mut [u8],
    ) -> Result<(), Self::Error> {
        let mut rx = [0u8; A];
        for _ in 0..MAX_EVENTS_PER_CALL {
            let ev = self.link.receive(&mut rx).await.map_err(BleError::Link)?;
            let data = match ev {
                Inbound::Connected | Inbound::Disconnected => {
                    self.request = Reassembler::new();
                    continue;
                }
                Inbound::Value { uuid, data } if uuid == RPC_CHARACTERISTIC_UUID => data,
                Inbound::Value { .. } => continue,
            };
            match self.absorb(data) {
                Ok(None) => {}
                Ok(Some(m)) => {
                    let outcome = match parse_request(m.as_bytes()) {
                        Err(_) => Err(HandlerError::InvalidRequest),
                        Ok((key, payload)) => handler
                            .handle(key, payload, scratch)
                            .and_then(|n| scratch.get(..n).ok_or(HandlerError::BufferTooSmall)),
                    };
                    return self.respond(m.id(), outcome).await;
                }
                Err(Rejected { id, error }) => {
                    if let Some(id) = id {
                        let status = match error {
                            FrameError::Oversized => HandlerError::BufferTooSmall,
                            _ => HandlerError::InvalidRequest,
                        };
                        self.respond(id, Err(status)).await?;
                    }
                    return Err(BleError::Frame(error));
                }
            }
        }
        Err(BleError::EventLimit)
    }
}

/// Publication reassembly kept across [`BleClient`] calls.
#[derive(Debug, Clone)]
enum Inbox<'a, const N: usize> {
    Empty,
    Partial { key: Key<'a>, state: Reassembler<N> },
    Ready { key: Key<'a>, message: Message<N> },
}

/// What an inbound event meant to a [`BleClient`].
enum Routed<'e> {
    /// Handled (or ignored); nothing for the caller.
    Absorbed,
    /// A value on the RPC characteristic.
    Rpc(&'e [u8]),
    /// The peer disconnected.
    Disconnected,
    /// A publication fragment was rejected.
    Rejected(FrameError),
}

/// Middleware transport for the GATT client role, reassembling messages of
/// up to `N` bytes with `A`-byte fragment buffers (see the module docs).
pub struct BleClient<'a, L, const N: usize, const A: usize = MAX_ATT_PAYLOAD> {
    link: L,
    subscriptions: &'a [Key<'a>],
    next_id: CorrelationId,
    inbox: Inbox<'a, N>,
}

impl<'a, L: GattClient, const N: usize, const A: usize> BleClient<'a, L, N, A> {
    /// Wraps `link` with no subscriptions.
    pub const fn new(link: L) -> Self {
        let () = AttBuffer::<A>::VALID;
        BleClient {
            link,
            subscriptions: &[],
            next_id: CorrelationId(0),
            inbox: Inbox::Empty,
        }
    }

    /// Sets the keys [`Subscriber::recv`] listens to. Notifications are
    /// matched by characteristic UUID, so only exact keys can be subscribed.
    pub fn with_subscriptions(self, keys: &'a [Key<'a>]) -> Self {
        BleClient {
            subscriptions: keys,
            ..self
        }
    }

    /// Borrows the link.
    pub const fn link(&self) -> &L {
        &self.link
    }

    /// Borrows the link mutably.
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

    async fn write_all(
        &mut self,
        uuid: Uuid128,
        frames: &Fragmenter<'_>,
    ) -> Result<(), BleError<L::Error>> {
        let mut out = [0u8; A];
        for i in 0..frames.count() {
            let fragment = frames.fragment(i, &mut out)?;
            self.link
                .write(uuid, fragment)
                .await
                .map_err(BleError::Link)?;
        }
        Ok(())
    }

    /// Updates the inbox for one event and says what is left for the caller.
    fn route<'e>(&mut self, ev: Inbound<'e>) -> Routed<'e> {
        match ev {
            Inbound::Connected => {
                self.drop_partial();
                Routed::Absorbed
            }
            Inbound::Disconnected => {
                self.drop_partial();
                Routed::Disconnected
            }
            Inbound::Value { uuid, data } if uuid == RPC_CHARACTERISTIC_UUID => Routed::Rpc(data),
            Inbound::Value { uuid, data } => {
                let key = self
                    .subscriptions
                    .iter()
                    .find(|k| characteristic_uuid(**k) == uuid);
                match key {
                    Some(key) => match self.absorb(*key, data) {
                        Ok(()) => Routed::Absorbed,
                        Err(e) => Routed::Rejected(e),
                    },
                    None => Routed::Absorbed,
                }
            }
        }
    }

    fn drop_partial(&mut self) {
        if matches!(self.inbox, Inbox::Partial { .. }) {
            self.inbox = Inbox::Empty;
        }
    }

    /// Feeds one publication fragment on `key`. On error the partial
    /// publication is discarded.
    fn absorb(&mut self, key: Key<'a>, fragment: &[u8]) -> Result<(), FrameError> {
        let (h, _) = FragmentHeader::parse(fragment)?;
        let inbox = core::mem::replace(&mut self.inbox, Inbox::Empty);
        let state = match inbox {
            Inbox::Ready { .. } => {
                // A completed sample is waiting to be taken; this one is dropped.
                self.inbox = inbox;
                return Ok(());
            }
            Inbox::Partial { key: k, state } if k == key && state.message_id() == Some(h.id) => {
                state
            }
            _ if h.index != 0 => {
                // A stray tail of a publication that was cut or rejected.
                self.inbox = inbox;
                return Ok(());
            }
            // Fragment 0 starts a publication, replacing any partial one.
            _ => Reassembler::new(),
        };
        self.inbox = match state.accept(fragment)? {
            Progress::Partial(state) => Inbox::Partial { key, state },
            Progress::Complete(message) => Inbox::Ready { key, message },
        };
        Ok(())
    }

    fn take_ready(&mut self) -> Option<(Key<'a>, Message<N>)> {
        if !matches!(self.inbox, Inbox::Ready { .. }) {
            return None;
        }
        match core::mem::replace(&mut self.inbox, Inbox::Empty) {
            Inbox::Ready { key, message } => Some((key, message)),
            _ => None,
        }
    }
}

fn deliver<'b, const N: usize, E>(
    buf: &'b mut [u8],
    (key, message): (Key<'_>, Message<N>),
) -> Result<Sample<'b>, BleError<E>> {
    sample_into(buf, key.as_str(), message.as_bytes()).ok_or(BleError::BufferTooSmall)
}

impl<L: GattClient, const N: usize, const A: usize> Subscriber for BleClient<'_, L, N, A> {
    type Error = BleError<L::Error>;

    async fn recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Sample<'b>, Self::Error> {
        if let Some(ready) = self.take_ready() {
            return deliver(buf, ready);
        }
        let mut rx = [0u8; A];
        for _ in 0..MAX_EVENTS_PER_CALL {
            let ev = self.link.receive(&mut rx).await.map_err(BleError::Link)?;
            if let Routed::Rejected(e) = self.route(ev) {
                return Err(BleError::Frame(e));
            }
            if let Some(ready) = self.take_ready() {
                return deliver(buf, ready);
            }
        }
        Err(BleError::EventLimit)
    }

    /// Processes already-received events until a publication completes.
    /// `Ok(None)` when no event is waiting, or after
    /// [`MAX_EVENTS_PER_CALL`] events (the rest are handled next call).
    fn try_recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Sample<'b>>, Self::Error> {
        if let Some(ready) = self.take_ready() {
            return deliver(buf, ready).map(Some);
        }
        let mut rx = [0u8; A];
        for _ in 0..MAX_EVENTS_PER_CALL {
            let Some(ev) = self.link.try_receive(&mut rx).map_err(BleError::Link)? else {
                return Ok(None);
            };
            if let Routed::Rejected(e) = self.route(ev) {
                return Err(BleError::Frame(e));
            }
            if let Some(ready) = self.take_ready() {
                return deliver(buf, ready).map(Some);
            }
        }
        Ok(None)
    }
}

impl<L: GattClient, const N: usize, const A: usize> Requester for BleClient<'_, L, N, A> {
    type Error = BleError<L::Error>;

    async fn request<'b>(
        &mut self,
        key: Key<'_>,
        request: &[u8],
        response: &'b mut [u8],
    ) -> Result<&'b [u8], Self::Error> {
        let id = self.take_id();
        let att = att_payload::<A>(self.link.att_mtu());
        let frames = Fragmenter::request(id, key, request, att)?;
        self.write_all(RPC_CHARACTERISTIC_UUID, &frames).await?;

        let mut rx = [0u8; A];
        let mut state = Reassembler::<N>::new();
        for _ in 0..MAX_EVENTS_PER_CALL {
            let ev = self.link.receive(&mut rx).await.map_err(BleError::Link)?;
            let data = match self.route(ev) {
                Routed::Rpc(data) => data,
                Routed::Disconnected => return Err(BleError::Disconnected),
                // Publications were kept in the inbox (or rejected there).
                Routed::Absorbed | Routed::Rejected(_) => continue,
            };
            match FragmentHeader::parse(data) {
                Ok((h, _)) if h.id == id => {}
                _ => continue, // stale or unrelated response
            }
            match state.accept(data)? {
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

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::ble::frame::STATUS_OK;
    use core::cell::{Cell, RefCell};
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};
    use futures::executor::block_on;
    use std::collections::VecDeque;
    use std::rc::Rc;
    use std::vec::Vec;

    const TEMP: Key<'static> = Key::from_static("sensor/temperature");
    const HUM: Key<'static> = Key::from_static("sensor/humidity");
    const ECHO: Key<'static> = Key::from_static("svc/Echo");

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Ev {
        Value(Uuid128, Vec<u8>),
        Connected,
        Disconnected,
    }

    /// Inbound events plus the waker of a receiver blocked on them.
    type Queue = Rc<RefCell<(VecDeque<Ev>, Option<Waker>)>>;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum FakeError {
        /// The scripted inbound queue is empty.
        Exhausted,
    }

    /// One end of an in-memory link. Outbound values are logged and, when a
    /// peer queue is attached, delivered to it. An empty inbound queue either
    /// fails (scripted) or waits (`waits`).
    struct End {
        mtu: Rc<Cell<AttMtu>>,
        inbound: Queue,
        peer: Option<Queue>,
        waits: bool,
        sent: Vec<(Uuid128, Vec<u8>)>,
    }

    fn queue(events: Vec<Ev>) -> Queue {
        Rc::new(RefCell::new((events.into(), None)))
    }

    fn push(q: &Queue, ev: Ev) {
        let mut q = q.borrow_mut();
        q.0.push_back(ev);
        if let Some(w) = q.1.take() {
            w.wake();
        }
    }

    impl End {
        fn scripted(mtu: u16, events: Vec<Ev>) -> Self {
            End {
                mtu: Rc::new(Cell::new(AttMtu::new(mtu).unwrap())),
                inbound: queue(events),
                peer: None,
                waits: false,
                sent: Vec::new(),
            }
        }

        /// An end whose empty queue waits; events are injected with `push`.
        fn waiting(mtu: u16) -> Self {
            End {
                waits: true,
                ..End::scripted(mtu, Vec::new())
            }
        }

        fn send(&mut self, uuid: Uuid128, data: &[u8]) {
            assert!(data.len() <= self.mtu.get().payload());
            self.sent.push((uuid, data.to_vec()));
            if let Some(peer) = &self.peer {
                push(peer, Ev::Value(uuid, data.to_vec()));
            }
        }

        fn fill<'b>(ev: Ev, buf: &'b mut [u8]) -> Inbound<'b> {
            match ev {
                Ev::Value(uuid, data) => {
                    let out = buf.get_mut(..data.len()).unwrap();
                    out.copy_from_slice(&data);
                    Inbound::Value { uuid, data: out }
                }
                Ev::Connected => Inbound::Connected,
                Ev::Disconnected => Inbound::Disconnected,
            }
        }

        async fn receive<'b>(&mut self, buf: &'b mut [u8]) -> Result<Inbound<'b>, FakeError> {
            let waits = self.waits;
            let inbound = self.inbound.clone();
            let ev = core::future::poll_fn(|cx| {
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
            Ok(Self::fill(ev, buf))
        }

        fn try_receive<'b>(&mut self, buf: &'b mut [u8]) -> Option<Inbound<'b>> {
            let ev = self.inbound.borrow_mut().0.pop_front()?;
            Some(Self::fill(ev, buf))
        }
    }

    struct ServerEnd(End);
    struct ClientEnd(End);

    impl GattServer for ServerEnd {
        type Error = FakeError;
        fn att_mtu(&self) -> AttMtu {
            self.0.mtu.get()
        }
        async fn notify(&mut self, uuid: Uuid128, data: &[u8]) -> Result<(), FakeError> {
            self.0.send(uuid, data);
            Ok(())
        }
        async fn receive<'b>(&mut self, buf: &'b mut [u8]) -> Result<Inbound<'b>, FakeError> {
            self.0.receive(buf).await
        }
    }

    impl GattClient for ClientEnd {
        type Error = FakeError;
        fn att_mtu(&self) -> AttMtu {
            self.0.mtu.get()
        }
        async fn write(&mut self, uuid: Uuid128, data: &[u8]) -> Result<(), FakeError> {
            self.0.send(uuid, data);
            Ok(())
        }
        async fn receive<'b>(&mut self, buf: &'b mut [u8]) -> Result<Inbound<'b>, FakeError> {
            self.0.receive(buf).await
        }
        fn try_receive<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Inbound<'b>>, FakeError> {
            Ok(self.0.try_receive(buf))
        }
    }

    /// A connected client/server pair sharing one MTU.
    fn pair(mtu: u16) -> (ClientEnd, ServerEnd) {
        let c = End::waiting(mtu);
        let s = End::waiting(mtu);
        let shared = c.mtu.clone();
        let s = End {
            mtu: shared,
            peer: Some(c.inbound.clone()),
            ..s
        };
        let c = End {
            peer: Some(s.inbound.clone()),
            ..c
        };
        (ClientEnd(c), ServerEnd(s))
    }

    fn poll_once<F: Future>(f: F) -> Poll<F::Output> {
        let mut f = pin!(f);
        f.as_mut().poll(&mut Context::from_waker(Waker::noop()))
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

    fn values(uuid: Uuid128, f: &Fragmenter<'_>) -> Vec<Ev> {
        frames(f).into_iter().map(|d| Ev::Value(uuid, d)).collect()
    }

    fn sent(end: &End) -> Vec<Vec<u8>> {
        end.sent.iter().map(|s| s.1.clone()).collect()
    }

    /// Serves only `svc/Echo`, echoing the request.
    fn echo(k: Key<'_>, req: &[u8], resp: &mut [u8]) -> Result<usize, HandlerError> {
        if k != ECHO {
            return Err(HandlerError::NotFound);
        }
        let out = resp
            .get_mut(..req.len())
            .ok_or(HandlerError::BufferTooSmall)?;
        out.copy_from_slice(req);
        Ok(req.len())
    }

    type Server<const N: usize> = BleServer<ServerEnd, N>;
    type Client<const N: usize> = BleClient<'static, ClientEnd, N>;

    fn server<const N: usize>(events: Vec<Ev>) -> Server<N> {
        BleServer::new(ServerEnd(End::scripted(23, events)))
    }

    fn client<const N: usize>(events: Vec<Ev>) -> Client<N> {
        BleClient::new(ClientEnd(End::scripted(23, events)))
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
    fn publisher_limits_follow_reassembly_size() {
        assert_eq!(<Server<128> as Publisher>::LIMITS.max_payload, 128);
        assert_eq!(<Server<128> as Publisher>::LIMITS.max_key, MAX_KEY_LEN);
        // 256 fragments of 16 data bytes at the minimum MTU.
        assert_eq!(max_message(1 << 20, MAX_ATT_PAYLOAD), 4096);
        // A smaller fragment buffer lowers it further.
        assert_eq!(max_message(1 << 20, 8), 256 * 4);
    }

    #[test]
    fn publish_notifies_fragments_on_key_characteristic() {
        let payload: Vec<u8> = (0..50u8).collect();
        let mut t: Server<64> = server(Vec::new());
        block_on(t.publish(TEMP, &payload)).unwrap();
        let end = &t.link().0;
        assert!(end.sent.len() > 1, "50 bytes must be fragmented at MTU 23");
        assert!(
            end.sent
                .iter()
                .all(|(uuid, d)| *uuid == characteristic_uuid(TEMP) && d.len() <= 20)
        );
        assert_eq!(reassemble::<64>(&sent(end)).as_bytes(), &payload[..]);
    }

    #[test]
    fn mtu_is_read_per_message_and_capped_by_buffer() {
        let payload = [3u8; 200];
        let mut t: Server<256> = server(Vec::new());
        block_on(t.publish(TEMP, &payload)).unwrap();
        let at_23 = t.link().0.sent.len();
        // The connection negotiates a larger MTU.
        t.link().0.mtu.set(AttMtu::new(247).unwrap());
        block_on(t.publish(TEMP, &payload)).unwrap();
        assert_eq!(at_23, 200usize.div_ceil(16));
        assert_eq!(t.link().0.sent.len() - at_23, 1);

        // A 64-byte fragment buffer caps fragments even at MTU 247.
        let mut small: BleServer<ServerEnd, 256, 64> =
            BleServer::new(ServerEnd(End::scripted(247, Vec::new())));
        block_on(small.publish(TEMP, &payload)).unwrap();
        let end = &small.link().0;
        assert!(end.sent.iter().all(|(_, d)| d.len() <= 64));
        assert_eq!(end.sent.len(), 200usize.div_ceil(60));
    }

    #[test]
    fn subscribe_yields_notification_for_subscribed_key() {
        let keys = [TEMP, HUM];
        let payload: Vec<u8> = (0..40u8).collect();
        let mut events = Vec::new();
        // Ignored: not subscribed, the RPC characteristic, lifecycle.
        events.extend(values(
            characteristic_uuid(Key::from_static("other")),
            &Fragmenter::message(0, b"no", 20).unwrap(),
        ));
        events.extend(values(
            RPC_CHARACTERISTIC_UUID,
            &Fragmenter::message(0, b"no", 20).unwrap(),
        ));
        events.push(Ev::Connected);
        events.extend(values(
            characteristic_uuid(HUM),
            &Fragmenter::message(5, &payload, 20).unwrap(),
        ));
        let mut t =
            BleClient::<_, 64>::new(ClientEnd(End::scripted(23, events))).with_subscriptions(&keys);
        let mut buf = [0u8; 128];
        let s = block_on(t.recv(&mut buf)).unwrap();
        assert_eq!(s.key, HUM);
        assert_eq!(s.payload, &payload[..]);
    }

    #[test]
    fn subscribe_reports_small_buffer_and_bad_fragments() {
        static KEYS: [Key<'static>; 1] = [TEMP];
        let f = Fragmenter::message(0, b"0123456789", 20).unwrap();
        let mut t = client::<64>(values(characteristic_uuid(TEMP), &f)).with_subscriptions(&KEYS);
        let mut small = [0u8; 8];
        assert_eq!(block_on(t.recv(&mut small)), Err(BleError::BufferTooSmall));

        let big = [7u8; 80];
        let f = Fragmenter::message(0, &big, 20).unwrap();
        let mut t = client::<64>(values(characteristic_uuid(TEMP), &f)).with_subscriptions(&KEYS);
        let mut buf = [0u8; 128];
        assert_eq!(
            block_on(t.recv(&mut buf)),
            Err(BleError::Frame(FrameError::Oversized))
        );
    }

    #[test]
    fn subscriber_drops_unrelated_events_up_to_a_bound() {
        static KEYS: [Key<'static>; 1] = [TEMP];
        let junk = Fragmenter::message(0, b"x", 20).unwrap();
        let events: Vec<Ev> = (0..MAX_EVENTS_PER_CALL)
            .flat_map(|_| values(characteristic_uuid(HUM), &junk))
            .collect();
        let mut t = client::<16>(events).with_subscriptions(&KEYS);
        let mut buf = [0u8; 32];
        assert_eq!(block_on(t.recv(&mut buf)), Err(BleError::EventLimit));
    }

    #[test]
    fn try_recv_returns_publications_without_waiting() {
        static KEYS: [Key<'static>; 1] = [TEMP];
        let events = values(
            characteristic_uuid(TEMP),
            &Fragmenter::message(1, &[9u8; 30], 20).unwrap(),
        );
        let (first, rest) = events.split_at(1);
        let mut t = client::<64>(first.to_vec()).with_subscriptions(&KEYS);
        let mut buf = [0u8; 64];
        // Half a publication: nothing yet, and no waiting.
        assert_eq!(t.try_recv(&mut buf), Ok(None));
        for ev in rest {
            push(&t.link().0.inbound, ev.clone());
        }
        let s = t.try_recv(&mut buf).unwrap().unwrap();
        assert_eq!((s.key, s.payload), (TEMP, &[9u8; 30][..]));
        assert_eq!(t.try_recv(&mut buf), Ok(None));
    }

    #[test]
    fn request_carries_correlation_id_and_discards_stale_responses() {
        // The first correlation id is 0; a stale response (id 9) must be
        // discarded before the matching one.
        let mut events = values(
            RPC_CHARACTERISTIC_UUID,
            &Fragmenter::response(9, Ok(b"stale"), 20).unwrap(),
        );
        events.extend(values(
            characteristic_uuid(TEMP),
            &Fragmenter::message(0, b"pub", 20).unwrap(),
        ));
        let answer: Vec<u8> = (100..140u8).collect();
        events.extend(values(
            RPC_CHARACTERISTIC_UUID,
            &Fragmenter::response(0, Ok(&answer), 20).unwrap(),
        ));
        let mut t = client::<64>(events);
        let request: Vec<u8> = (0..30u8).collect();
        let mut resp = [0u8; 64];
        let got = block_on(t.request(ECHO, &request, &mut resp)).unwrap();
        assert_eq!(got, &answer[..]);

        let end = &t.link().0;
        assert!(
            end.sent
                .iter()
                .all(|(uuid, _)| *uuid == RPC_CHARACTERISTIC_UUID)
        );
        let m = reassemble::<64>(&sent(end));
        assert_eq!(m.id(), 0);
        assert_eq!(parse_request(m.as_bytes()).unwrap(), (ECHO, &request[..]));
    }

    #[test]
    fn successive_requests_use_new_correlation_ids() {
        let mut events = values(
            RPC_CHARACTERISTIC_UUID,
            &Fragmenter::response(0, Ok(b"a"), 20).unwrap(),
        );
        // A late duplicate of response 0 arrives before response 1.
        events.extend(values(
            RPC_CHARACTERISTIC_UUID,
            &Fragmenter::response(0, Ok(b"a"), 20).unwrap(),
        ));
        events.extend(values(
            RPC_CHARACTERISTIC_UUID,
            &Fragmenter::response(1, Ok(b"b"), 20).unwrap(),
        ));
        let mut t = client::<16>(events);
        let mut resp = [0u8; 16];
        assert_eq!(block_on(t.request(ECHO, b"1", &mut resp)).unwrap(), b"a");
        assert_eq!(block_on(t.request(ECHO, b"2", &mut resp)).unwrap(), b"b");
    }

    #[test]
    fn handler_error_reaches_requester() {
        let events = values(
            RPC_CHARACTERISTIC_UUID,
            &Fragmenter::response(0, Err(HandlerError::Internal), 20).unwrap(),
        );
        let mut t = client::<16>(events);
        let mut resp = [0u8; 16];
        assert_eq!(
            block_on(t.request(ECHO, b"x", &mut resp)),
            Err(BleError::Handler(HandlerError::Internal))
        );
    }

    #[test]
    fn requester_reports_small_buffer_link_errors_and_disconnect() {
        let mut events = values(
            RPC_CHARACTERISTIC_UUID,
            &Fragmenter::response(0, Ok(b"too long"), 20).unwrap(),
        );
        events.push(Ev::Disconnected);
        let mut t = client::<16>(events);
        let mut resp = [0u8; 4];
        assert_eq!(
            block_on(t.request(ECHO, b"x", &mut resp)),
            Err(BleError::BufferTooSmall)
        );
        assert_eq!(
            block_on(t.request(ECHO, b"x", &mut resp)),
            Err(BleError::Disconnected)
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
        let events = values(
            RPC_CHARACTERISTIC_UUID,
            &Fragmenter::request(77, ECHO, &request, 20).unwrap(),
        );
        let mut t: Server<64> = server(events);
        let mut scratch = [0u8; 64];
        block_on(t.serve_one(&mut echo, &mut scratch)).unwrap();
        let end = &t.link().0;
        assert!(
            end.sent
                .iter()
                .all(|(uuid, _)| *uuid == RPC_CHARACTERISTIC_UUID)
        );
        let m = reassemble::<64>(&sent(end));
        assert_eq!(m.id(), 77);
        assert_eq!(m.as_bytes()[0], STATUS_OK);
        assert_eq!(parse_response(m.as_bytes()).unwrap(), Ok(&request[..]));
    }

    fn served_status(
        events: Vec<Ev>,
        scratch: usize,
    ) -> (Result<(), BleError<FakeError>>, Message<16>) {
        let mut t: Server<32> = server(events);
        let mut scratch_buf = [0u8; 64];
        let r = block_on(t.serve_one(&mut echo, &mut scratch_buf[..scratch]));
        (r, reassemble::<16>(&sent(&t.link().0)))
    }

    #[test]
    fn responder_maps_failures_to_status_codes() {
        let req = |k| {
            values(
                RPC_CHARACTERISTIC_UUID,
                &Fragmenter::request(4, k, b"abc", 20).unwrap(),
            )
        };
        // The handler does not serve the key → NotFound.
        let (r, m) = served_status(req(TEMP), 64);
        assert_eq!(r, Ok(()));
        assert_eq!(
            parse_response(m.as_bytes()).unwrap(),
            Err(HandlerError::NotFound)
        );
        // Handler error (response does not fit scratch) → its code.
        let (r, m) = served_status(req(ECHO), 2);
        assert_eq!(r, Ok(()));
        assert_eq!(m.id(), 4);
        assert_eq!(
            parse_response(m.as_bytes()).unwrap(),
            Err(HandlerError::BufferTooSmall)
        );
        // Undecodable request body → InvalidRequest.
        let bad = values(
            RPC_CHARACTERISTIC_UUID,
            &Fragmenter::message(5, &[9, b'a'], 20).unwrap(),
        );
        let (r, m) = served_status(bad, 64);
        assert_eq!(r, Ok(()));
        assert_eq!(
            parse_response(m.as_bytes()).unwrap(),
            Err(HandlerError::InvalidRequest)
        );
    }

    #[test]
    fn responder_rejects_oversized_request_and_tells_requester() {
        let big = [0u8; 40];
        let events = values(
            RPC_CHARACTERISTIC_UUID,
            &Fragmenter::request(6, ECHO, &big, 20).unwrap(),
        );
        let (r, m) = served_status(events, 64);
        assert_eq!(r, Err(BleError::Frame(FrameError::Oversized)));
        assert_eq!(m.id(), 6);
        assert_eq!(
            parse_response(m.as_bytes()).unwrap(),
            Err(HandlerError::BufferTooSmall)
        );
    }

    #[test]
    fn responder_rejects_duplicate_fragment_then_recovers() {
        let mut events = values(
            RPC_CHARACTERISTIC_UUID,
            &Fragmenter::request(8, ECHO, &[1u8; 30], 20).unwrap(),
        );
        let first = events[0].clone();
        events.insert(1, first);
        events.extend(values(
            RPC_CHARACTERISTIC_UUID,
            &Fragmenter::request(9, ECHO, b"ok", 20).unwrap(),
        ));
        let mut t: Server<64> = server(events);
        let mut scratch = [0u8; 64];
        assert_eq!(
            block_on(t.serve_one(&mut echo, &mut scratch)),
            Err(BleError::Frame(FrameError::Duplicate))
        );
        assert!(!t.request_in_progress());
        // The rejection does not poison the next call: the tail of request 8
        // is dropped and request 9 succeeds.
        block_on(t.serve_one(&mut echo, &mut scratch)).unwrap();
        let last = t.link().0.sent.last().unwrap().1.clone();
        let m = reassemble::<16>(&[last]);
        assert_eq!(
            (m.id(), parse_response(m.as_bytes()).unwrap()),
            (9, Ok(&b"ok"[..]))
        );
    }

    /// Polls `serve_one` once mid-request, drops it, and serves again.
    #[test]
    fn serve_one_is_cancel_safe() {
        let request: Vec<u8> = (0..50u8).collect();
        let events = values(
            RPC_CHARACTERISTIC_UUID,
            &Fragmenter::request(3, ECHO, &request, 20).unwrap(),
        );
        assert!(events.len() > 2);
        let mut t: Server<128> = BleServer::new(ServerEnd(End::waiting(23)));
        let inbound = t.link().0.inbound.clone();
        push(&inbound, events[0].clone());
        push(&inbound, events[1].clone());
        let mut scratch = [0u8; 128];
        assert!(poll_once(t.serve_one(&mut echo, &mut scratch)).is_pending());
        assert!(t.request_in_progress(), "partial request kept in self");
        for ev in &events[2..] {
            push(&inbound, ev.clone());
        }
        block_on(t.serve_one(&mut echo, &mut scratch)).unwrap();
        let m = reassemble::<128>(&sent(&t.link().0));
        assert_eq!(m.id(), 3);
        assert_eq!(parse_response(m.as_bytes()).unwrap(), Ok(&request[..]));
    }

    #[test]
    fn disconnect_discards_partial_request() {
        // Request 3 is cut by a disconnect; after reconnecting the client
        // sends a request that happens to reuse id 3.
        let abandoned = values(
            RPC_CHARACTERISTIC_UUID,
            &Fragmenter::request(3, ECHO, &[1u8; 40], 20).unwrap(),
        );
        let mut events = std::vec![abandoned[0].clone(), Ev::Disconnected, Ev::Connected];
        events.extend(values(
            RPC_CHARACTERISTIC_UUID,
            &Fragmenter::request(3, ECHO, b"fresh", 20).unwrap(),
        ));
        let mut t: Server<64> = server(events);
        let mut scratch = [0u8; 64];
        block_on(t.serve_one(&mut echo, &mut scratch)).unwrap();
        let m = reassemble::<16>(&sent(&t.link().0));
        assert_eq!(parse_response(m.as_bytes()).unwrap(), Ok(&b"fresh"[..]));
    }

    #[test]
    fn new_request_id_replaces_partial_request() {
        let abandoned = values(
            RPC_CHARACTERISTIC_UUID,
            &Fragmenter::request(3, ECHO, &[1u8; 40], 20).unwrap(),
        );
        let mut events = std::vec![abandoned[0].clone()];
        let next = values(
            RPC_CHARACTERISTIC_UUID,
            &Fragmenter::request(4, ECHO, &[2u8; 30], 20).unwrap(),
        );
        events.push(next[0].clone());
        // A stray tail fragment of the abandoned request is dropped.
        events.push(abandoned[1].clone());
        events.extend(next[1..].iter().cloned());
        let mut t: Server<64> = server(events);
        let mut scratch = [0u8; 64];
        block_on(t.serve_one(&mut echo, &mut scratch)).unwrap();
        let m = reassemble::<64>(&sent(&t.link().0));
        assert_eq!(m.id(), 4);
        assert_eq!(parse_response(m.as_bytes()).unwrap(), Ok(&[2u8; 30][..]));
    }

    #[test]
    fn end_to_end_request_at_mtu_23_and_247() {
        for mtu in [23, 247] {
            let (c, s) = pair(mtu);
            let mut client: BleClient<'_, _, 512> = BleClient::new(c);
            let mut server: BleServer<_, 512> = BleServer::new(s);
            let request: Vec<u8> = (0..=255u8).chain(0..90).collect();
            let mut resp = [0u8; 512];
            let mut scratch = [0u8; 512];
            let (got, served) = block_on(futures::future::join(
                client.request(ECHO, &request, &mut resp),
                server.serve_one(&mut echo, &mut scratch),
            ));
            served.unwrap();
            assert_eq!(got.unwrap(), &request[..], "mtu {mtu}");
            let limit = AttMtu::new(mtu).unwrap().payload();
            let end = &client.link().0;
            assert!(end.sent.iter().all(|(_, d)| d.len() <= limit));
            assert_eq!(
                end.sent.len(),
                (1 + ECHO.as_bytes().len() + request.len()).div_ceil(limit - HEADER_LEN)
            );
        }
    }

    #[test]
    fn end_to_end_publish_subscribe_at_mtu_23_and_247() {
        static KEYS: [Key<'static>; 1] = [TEMP];
        for mtu in [23, 247] {
            let (c, s) = pair(mtu);
            let mut publisher: BleServer<_, 400> = BleServer::new(s);
            let mut subscriber: BleClient<'_, _, 400> = BleClient::new(c).with_subscriptions(&KEYS);
            let payload = [0x5Au8; 300];
            let mut buf = [0u8; 400];
            let (p, s) = block_on(futures::future::join(
                publisher.publish(TEMP, &payload),
                subscriber.recv(&mut buf),
            ));
            p.unwrap();
            let s = s.unwrap();
            assert_eq!((s.key, s.payload), (TEMP, &payload[..]), "mtu {mtu}");
        }
    }

    #[test]
    fn recv_is_cancel_safe() {
        static KEYS: [Key<'static>; 1] = [TEMP];
        let events = values(
            characteristic_uuid(TEMP),
            &Fragmenter::message(2, &[4u8; 40], 20).unwrap(),
        );
        let mut t: Client<64> =
            BleClient::new(ClientEnd(End::waiting(23))).with_subscriptions(&KEYS);
        let inbound = t.link().0.inbound.clone();
        push(&inbound, events[0].clone());
        let mut buf = [0u8; 64];
        assert!(poll_once(t.recv(&mut buf)).is_pending());
        for ev in &events[1..] {
            push(&inbound, ev.clone());
        }
        let s = block_on(t.recv(&mut buf)).unwrap();
        assert_eq!(s.payload, &[4u8; 40][..]);
    }

    #[test]
    fn publication_during_request_is_kept_for_recv() {
        static KEYS: [Key<'static>; 1] = [TEMP];
        let mut events = values(
            characteristic_uuid(TEMP),
            &Fragmenter::message(0, b"kept", 20).unwrap(),
        );
        // A second publication completes before the first is taken: dropped.
        events.extend(values(
            characteristic_uuid(TEMP),
            &Fragmenter::message(1, b"dropped", 20).unwrap(),
        ));
        events.extend(values(
            RPC_CHARACTERISTIC_UUID,
            &Fragmenter::response(0, Ok(b"r"), 20).unwrap(),
        ));
        let mut t = client::<32>(events).with_subscriptions(&KEYS);
        let mut resp = [0u8; 8];
        assert_eq!(block_on(t.request(ECHO, b"q", &mut resp)).unwrap(), b"r");
        let mut buf = [0u8; 32];
        let s = t.try_recv(&mut buf).unwrap().unwrap();
        assert_eq!(s.payload, b"kept");
        assert_eq!(t.try_recv(&mut buf), Ok(None));
    }

    #[test]
    fn disconnect_discards_partial_publication() {
        static KEYS: [Key<'static>; 1] = [TEMP];
        let cut = values(
            characteristic_uuid(TEMP),
            &Fragmenter::message(0, &[1u8; 40], 20).unwrap(),
        );
        let mut events = std::vec![cut[0].clone(), Ev::Disconnected];
        // The tail of the cut publication is dropped, then a fresh one passes.
        events.extend(cut[1..].iter().cloned());
        events.extend(values(
            characteristic_uuid(TEMP),
            &Fragmenter::message(0, b"new", 20).unwrap(),
        ));
        let mut t = client::<64>(events).with_subscriptions(&KEYS);
        let mut buf = [0u8; 64];
        assert_eq!(block_on(t.recv(&mut buf)).unwrap().payload, b"new");
    }

    /// A requester dropped while waiting abandons its request; the late
    /// response is discarded by the next request.
    #[test]
    fn dropped_request_late_response_is_discarded() {
        let (c, s) = pair(23);
        let mut client: Client<64> = BleClient::new(c);
        let mut server: Server<64> = BleServer::new(s);
        let mut resp = [0u8; 64];
        let mut scratch = [0u8; 64];
        assert!(poll_once(client.request(ECHO, b"old", &mut resp)).is_pending());
        // The server answers the abandoned request (id 0) late.
        block_on(server.serve_one(&mut echo, &mut scratch)).unwrap();
        let (got, served) = block_on(futures::future::join(
            client.request(ECHO, b"new", &mut resp),
            server.serve_one(&mut echo, &mut scratch),
        ));
        served.unwrap();
        assert_eq!(got.unwrap(), b"new");
    }
}
