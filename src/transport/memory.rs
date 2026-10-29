//! An in-process backend (feature `memory`, `std`): every transport trait
//! over one shared hub, for tests, examples and the conformance harness
//! (`tests/conformance.rs`).
//!
//! ```text
//! MemoryBus ──node(id)──▶ MemoryNode: Publisher, Subscribe, Requester, Gather
//!                          ├─ subscriber(filter) ─▶ MemorySubscriber: Subscriber
//!                          ├─ responder(filter)  ─▶ MemoryResponder: Responder
//!                          └─ discovery()        ─▶ MemoryDiscovery: Announce, Browse
//! ```
//!
//! # Why a shared hub
//!
//! An in-process bus connects endpoints that are owned separately (a
//! publisher here, three subscribers there, used concurrently in one task),
//! so the routing table must be reachable from all of them: that is the
//! one place this crate uses shared ownership. It is kept minimal: one
//! `Rc<RefCell<Hub>>`, single-threaded (the types are `!Send`; run them on
//! a current-thread executor such as `#[tokio::test]`), never borrowed
//! across an `await`, and nothing in the hub points back at the endpoints.
//! Each endpoint deregisters itself when dropped.
//!
//! # Behaviour
//!
//! * **Pub/sub.** A publish is copied into the queue of every subscriber
//!   whose filter matches, including subscribers on the publishing node.
//!   Queues hold [`MemoryBus::queue_depth`] samples ([`DEFAULT_QUEUE_DEPTH`]);
//!   a full queue drops its oldest sample ([`Overflow::DropOldest`]), and
//!   each sender's samples stay in order. Samples carry the sender's
//!   [`SenderId`], a sequence number counted per (sender, key), and the
//!   gap computed with [`SeenTable::check`], so a drop shows up as
//!   `gap: Some(n)` on the next sample delivered. A sample too large for the
//!   caller's buffer is an error and stays queued.
//! * **Requests.** [`Requester::request_with`] goes to the first responder
//!   (registration order) whose filter matches the key, restricted to one
//!   node by [`Target::Node`]; with none it fails at once with
//!   [`MemoryError::NoResponder`]. [`Gather::request_each`] goes to every
//!   such responder. Handlers see [`RequestMeta`] with the requesting
//!   node's id, its principal (set with [`MemoryBus::node_with_principal`]),
//!   and the time left of the caller's timeout; a request whose deadline
//!   passed before it was served is answered
//!   [`HandlerError::DeadlineExceeded`] without running the handler.
//! * **Timeouts.** `options.timeout` is honoured with tokio's timer (call
//!   from a tokio runtime with time enabled). With no timeout a call waits
//!   until every addressed responder has answered; a responder dropped with
//!   requests queued answers them [`HandlerError::Unavailable`], so such a
//!   wait ends unless a responder is alive and never served.
//! * **Discovery.** [`MemoryDiscovery`] is
//!   [`PubSubDiscovery`] over the bus: beacons on `disc/<kind>`.
//! * **Cancel safety.** A dropped `recv` loses nothing (samples are removed
//!   only when copied out). A dropped `request_with` / `request_each`
//!   removes its reply slot, so late replies are discarded.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::string::String;
use std::task::{Poll, Waker};
use std::time::{Duration, Instant};
use std::vec::Vec;

use crate::dedup::SeenTable;
use crate::discovery::beacon::{BEACON_PREFIX, PubSubDiscovery};
use crate::key::{Key, KeyFilter, KeyFilterBuf, MAX_KEY_LEN};

use super::{
    Delivery, Flow, Gather, Handler, HandlerError, Limits, Overflow, Publisher, Reply, Request,
    RequestMeta, RequestOptions, Requester, Responder, Sample, SampleMeta, SenderId, Subscribe,
    Subscriber, Target, sample_into,
};

/// Queue depth of each subscriber unless set with
/// [`MemoryBus::with_queue_depth`].
pub const DEFAULT_QUEUE_DEPTH: usize = 16;

/// (sender, key) pairs a subscriber remembers for gap detection.
const SEEN_PAIRS: usize = 64;

/// [`PubSubDiscovery`] over the bus, keeping up to 8 announced services.
pub type MemoryDiscovery = PubSubDiscovery<MemoryNode, MemorySubscriber, 8>;

/// Failure of a memory endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryError {
    /// No responder serves the key (on the targeted node).
    NoResponder,
    /// The caller's timeout elapsed without a reply.
    Timeout,
    /// The responder's handler failed.
    Handler(HandlerError),
    /// The sample or reply needs `needed` bytes; the buffer has `capacity`.
    BufferTooSmall {
        /// Bytes required.
        needed: usize,
        /// Bytes available.
        capacity: usize,
    },
    /// A filter key does not fit the hub's key buffer (unreachable for
    /// valid keys, which are at most [`MAX_KEY_LEN`] bytes).
    InvalidKey,
}

impl core::fmt::Display for MemoryError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            MemoryError::NoResponder => f.write_str("no responder for key"),
            MemoryError::Timeout => f.write_str("request timed out"),
            MemoryError::Handler(e) => write!(f, "handler error: {e:?}"),
            MemoryError::BufferTooSmall { needed, capacity } => {
                write!(f, "buffer too small: need {needed} bytes, have {capacity}")
            }
            MemoryError::InvalidKey => f.write_str("invalid key"),
        }
    }
}

impl std::error::Error for MemoryError {}

struct Msg {
    key: String,
    payload: Vec<u8>,
    sender: SenderId,
    seq: u8,
}

struct SubSlot {
    id: u64,
    filter: KeyFilterBuf<MAX_KEY_LEN>,
    queue: VecDeque<Msg>,
    waker: Option<Waker>,
}

struct Pending {
    request: u64,
    key: String,
    body: Vec<u8>,
    sender: SenderId,
    principal: Option<String>,
    deadline: Option<Instant>,
}

struct RespSlot {
    id: u64,
    node: SenderId,
    filter: KeyFilterBuf<MAX_KEY_LEN>,
    queue: VecDeque<Pending>,
    waker: Option<Waker>,
}

type OwnedReply = (SenderId, Result<Vec<u8>, HandlerError>);

struct ReplySlot {
    request: u64,
    /// Responders addressed that have not answered yet.
    outstanding: usize,
    replies: VecDeque<OwnedReply>,
    waker: Option<Waker>,
}

struct Hub {
    depth: usize,
    next_id: u64,
    subs: Vec<SubSlot>,
    responders: Vec<RespSlot>,
    replies: Vec<ReplySlot>,
    seqs: Vec<(SenderId, String, u8)>,
}

impl Hub {
    fn fresh_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn next_seq(&mut self, sender: SenderId, key: &str) -> u8 {
        match self
            .seqs
            .iter_mut()
            .find(|(s, k, _)| *s == sender && k == key)
        {
            Some((_, _, seq)) => {
                *seq = seq.wrapping_add(1);
                *seq
            }
            None => {
                self.seqs.push((sender, key.into(), 0));
                0
            }
        }
    }

    fn reply(&mut self, request: u64, reply: OwnedReply) {
        if let Some(slot) = self.replies.iter_mut().find(|r| r.request == request) {
            slot.outstanding = slot.outstanding.saturating_sub(1);
            slot.replies.push_back(reply);
            if let Some(w) = slot.waker.take() {
                w.wake();
            }
        }
    }
}

/// The hub endpoints share; see the module docs. Cloning shares the hub.
#[derive(Clone)]
pub struct MemoryBus {
    hub: Rc<RefCell<Hub>>,
}

impl Default for MemoryBus {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryBus {
    /// A bus with [`DEFAULT_QUEUE_DEPTH`] subscriber queues.
    pub fn new() -> Self {
        Self::with_queue_depth(DEFAULT_QUEUE_DEPTH)
    }

    /// A bus whose subscriber queues hold `depth` samples (at least 1).
    pub fn with_queue_depth(depth: usize) -> Self {
        let hub = Hub {
            depth: depth.max(1),
            next_id: 0,
            subs: Vec::new(),
            responders: Vec::new(),
            replies: Vec::new(),
            seqs: Vec::new(),
        };
        MemoryBus {
            hub: Rc::new(RefCell::new(hub)),
        }
    }

    /// Samples each subscriber queue holds.
    pub fn queue_depth(&self) -> usize {
        self.hub.borrow().depth
    }

    /// An endpoint for node `id` with no principal.
    pub fn node(&self, id: SenderId) -> MemoryNode {
        MemoryNode {
            hub: self.hub.clone(),
            id,
            principal: None,
        }
    }

    /// An endpoint for node `id` whose requests carry `principal` as their
    /// authenticated caller ([`RequestMeta::principal`]).
    pub fn node_with_principal(&self, id: SenderId, principal: &str) -> MemoryNode {
        MemoryNode {
            hub: self.hub.clone(),
            id,
            principal: Some(principal.into()),
        }
    }
}

/// One node on a [`MemoryBus`]: publishes, subscribes, requests, and makes
/// responders. Cloning gives another handle to the same node.
#[derive(Clone)]
pub struct MemoryNode {
    hub: Rc<RefCell<Hub>>,
    id: SenderId,
    principal: Option<String>,
}

impl MemoryNode {
    /// This node's id.
    pub fn id(&self) -> SenderId {
        self.id
    }

    /// Subscribes to `filter` without awaiting (the async
    /// [`Subscribe::subscribe`] does the same).
    pub fn subscriber(&self, filter: KeyFilter<'_>) -> Result<MemorySubscriber, MemoryError> {
        let filter = filter.to_buf().map_err(|_| MemoryError::InvalidKey)?;
        let mut hub = self.hub.borrow_mut();
        let id = hub.fresh_id();
        hub.subs.push(SubSlot {
            id,
            filter,
            queue: VecDeque::new(),
            waker: None,
        });
        Ok(MemorySubscriber {
            hub: self.hub.clone(),
            id,
            seen: SeenTable::new(),
        })
    }

    /// A responder on this node for requests whose key matches `filter`.
    pub fn responder(&self, filter: KeyFilter<'_>) -> Result<MemoryResponder, MemoryError> {
        let filter = filter.to_buf().map_err(|_| MemoryError::InvalidKey)?;
        let mut hub = self.hub.borrow_mut();
        let id = hub.fresh_id();
        hub.responders.push(RespSlot {
            id,
            node: self.id,
            filter,
            queue: VecDeque::new(),
            waker: None,
        });
        Ok(MemoryResponder {
            hub: self.hub.clone(),
            id,
            node: self.id,
        })
    }

    /// Discovery for this node: beacons published by a clone of this node,
    /// received by a subscriber to `disc`.
    pub fn discovery(&self) -> Result<MemoryDiscovery, MemoryError> {
        Ok(PubSubDiscovery::new(
            self.clone(),
            self.subscriber(KeyFilter::Prefix(BEACON_PREFIX))?,
        ))
    }

    /// Queues the request with every addressed responder (the first only
    /// unless `all`) and returns its id with the guard owning its reply
    /// slot, or `None` if no responder matched.
    fn send(
        &self,
        key: Key<'_>,
        body: &[u8],
        options: &RequestOptions,
        all: bool,
    ) -> Option<(u64, ReplyGuard)> {
        let mut hub = self.hub.borrow_mut();
        let request = hub.fresh_id();
        // A timeout too far out to represent is no deadline at all.
        let deadline = options.timeout.and_then(|d| Instant::now().checked_add(d));
        let mut addressed = 0;
        for r in hub.responders.iter_mut() {
            let on_target = match options.target {
                Target::Any => true,
                Target::Node(n) => n == r.node,
            };
            if !on_target || !r.filter.matches(key) {
                continue;
            }
            r.queue.push_back(Pending {
                request,
                key: key.as_str().into(),
                body: body.to_vec(),
                sender: self.id,
                principal: self.principal.clone(),
                deadline,
            });
            if let Some(w) = r.waker.take() {
                w.wake();
            }
            addressed += 1;
            if !all {
                break;
            }
        }
        if addressed == 0 {
            return None;
        }
        hub.replies.push(ReplySlot {
            request,
            outstanding: addressed,
            replies: VecDeque::new(),
            waker: None,
        });
        Some((
            request,
            ReplyGuard {
                hub: self.hub.clone(),
                request,
            },
        ))
    }

    /// The next reply to `request`, `None` once every addressed responder
    /// has answered.
    async fn next_reply(&self, request: u64) -> Option<OwnedReply> {
        core::future::poll_fn(|cx| {
            let mut hub = self.hub.borrow_mut();
            let Some(slot) = hub.replies.iter_mut().find(|r| r.request == request) else {
                return Poll::Ready(None);
            };
            match slot.replies.pop_front() {
                Some(r) => Poll::Ready(Some(r)),
                None if slot.outstanding == 0 => Poll::Ready(None),
                None => {
                    slot.waker = Some(cx.waker().clone());
                    Poll::Pending
                }
            }
        })
        .await
    }
}

/// Removes a reply slot when the request or gather ends or is dropped.
struct ReplyGuard {
    hub: Rc<RefCell<Hub>>,
    request: u64,
}

impl Drop for ReplyGuard {
    fn drop(&mut self) {
        if let Ok(mut hub) = self.hub.try_borrow_mut() {
            hub.replies.retain(|r| r.request != self.request);
        }
    }
}

impl Publisher for MemoryNode {
    type Error = MemoryError;

    async fn publish(&mut self, key: Key<'_>, payload: &[u8]) -> Result<(), MemoryError> {
        let mut hub = self.hub.borrow_mut();
        let seq = hub.next_seq(self.id, key.as_str());
        let depth = hub.depth;
        for sub in hub.subs.iter_mut().filter(|s| s.filter.matches(key)) {
            if sub.queue.len() >= depth {
                sub.queue.pop_front();
            }
            sub.queue.push_back(Msg {
                key: key.as_str().into(),
                payload: payload.to_vec(),
                sender: self.id,
                seq,
            });
            if let Some(w) = sub.waker.take() {
                w.wake();
            }
        }
        Ok(())
    }
}

impl Subscribe for MemoryNode {
    type Subscriber<'a> = MemorySubscriber;
    type Error = MemoryError;

    async fn subscribe(&mut self, filter: KeyFilter<'_>) -> Result<MemorySubscriber, MemoryError> {
        self.subscriber(filter)
    }
}

impl Requester for MemoryNode {
    type Error = MemoryError;

    /// See the module docs: first matching responder, timeout honoured,
    /// without one waits for that responder's answer.
    async fn request_with<'b>(
        &mut self,
        key: Key<'_>,
        request: &[u8],
        response: &'b mut [u8],
        options: &RequestOptions,
    ) -> Result<&'b [u8], MemoryError> {
        let (id, _guard) = self
            .send(key, request, options, false)
            .ok_or(MemoryError::NoResponder)?;
        let reply = match options.timeout {
            Some(d) => tokio::time::timeout(d, self.next_reply(id))
                .await
                .map_err(|_| MemoryError::Timeout)?,
            None => self.next_reply(id).await,
        };
        let (_, result) = reply.ok_or(MemoryError::Handler(HandlerError::Unavailable))?;
        let body = result.map_err(MemoryError::Handler)?;
        let capacity = response.len();
        let out = response
            .get_mut(..body.len())
            .ok_or(MemoryError::BufferTooSmall {
                needed: body.len(),
                capacity,
            })?;
        out.copy_from_slice(&body);
        Ok(out)
    }
}

impl Gather for MemoryNode {
    type Error = MemoryError;

    /// Every matching responder (see the module docs). No responder is
    /// `Ok(0)`, not an error: nobody answered.
    async fn request_each<F>(
        &mut self,
        key: Key<'_>,
        request: &[u8],
        buf: &mut [u8],
        options: &RequestOptions,
        max_replies: usize,
        mut on_reply: F,
    ) -> Result<usize, MemoryError>
    where
        F: FnMut(Reply<'_>) -> Flow,
    {
        let Some((id, _guard)) = self.send(key, request, options, true) else {
            return Ok(0);
        };
        let deadline = options
            .timeout
            .and_then(|d| tokio::time::Instant::now().checked_add(d));
        let mut seen = 0;
        for _ in 0..max_replies {
            let next = match deadline {
                Some(t) => match tokio::time::timeout_at(t, self.next_reply(id)).await {
                    Ok(next) => next,
                    Err(_) => break,
                },
                None => self.next_reply(id).await,
            };
            let Some((sender, result)) = next else { break };
            seen += 1;
            let result = match &result {
                Ok(body) => match buf.get_mut(..body.len()) {
                    Some(out) => {
                        out.copy_from_slice(body);
                        Ok(&*out)
                    }
                    None => Err(HandlerError::BufferTooSmall),
                },
                Err(e) => Err(*e),
            };
            if on_reply(Reply {
                sender: Some(sender),
                result,
            }) == Flow::Stop
            {
                break;
            }
        }
        Ok(seen)
    }
}

/// Receives what a [`MemoryNode`] subscribed to; deregisters on drop.
pub struct MemorySubscriber {
    hub: Rc<RefCell<Hub>>,
    id: u64,
    seen: SeenTable<SEEN_PAIRS>,
}

impl MemorySubscriber {
    /// Copies `msg` into `buf` with its metadata, updating the gap table.
    fn deliver<'b>(&mut self, msg: &Msg, buf: &'b mut [u8]) -> Result<Sample<'b>, MemoryError> {
        let capacity = buf.len();
        let needed = msg.key.len() + msg.payload.len();
        let sample = sample_into(buf, &msg.key, &msg.payload)
            .ok_or(MemoryError::BufferTooSmall { needed, capacity })?;
        let (seen, check) = self.seen.check(msg.sender, sample.key, msg.seq);
        self.seen = seen;
        let meta = SampleMeta::from_sender(msg.sender)
            .with_seq(msg.seq)
            .with_gap(check.gap());
        Ok(sample.with_meta(meta))
    }

    /// Pops the next message if it fits `capacity` bytes; with a waker,
    /// registers it when the queue is empty.
    fn pop(
        &self,
        capacity: usize,
        waker: Option<&Waker>,
    ) -> Poll<Result<Option<Msg>, MemoryError>> {
        let mut hub = self.hub.borrow_mut();
        let Some(slot) = hub.subs.iter_mut().find(|s| s.id == self.id) else {
            return Poll::Ready(Ok(None));
        };
        match slot.queue.front() {
            Some(m) if m.key.len() + m.payload.len() > capacity => {
                Poll::Ready(Err(MemoryError::BufferTooSmall {
                    needed: m.key.len() + m.payload.len(),
                    capacity,
                }))
            }
            Some(_) => Poll::Ready(Ok(slot.queue.pop_front())),
            None => match waker {
                Some(w) => {
                    slot.waker = Some(w.clone());
                    Poll::Pending
                }
                None => Poll::Ready(Ok(None)),
            },
        }
    }
}

impl Drop for MemorySubscriber {
    fn drop(&mut self) {
        if let Ok(mut hub) = self.hub.try_borrow_mut() {
            hub.subs.retain(|s| s.id != self.id);
        }
    }
}

impl Subscriber for MemorySubscriber {
    type Error = MemoryError;

    /// A full queue drops its oldest sample; each sender's samples stay in
    /// order.
    const DELIVERY: Delivery = Delivery::new(Overflow::DropOldest, true);
    const LIMITS: Limits = Limits::UNBOUNDED;

    async fn recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Sample<'b>, MemoryError> {
        let capacity = buf.len();
        let msg = core::future::poll_fn(|cx| self.pop(capacity, Some(cx.waker()))).await?;
        // The slot is always registered while `self` lives; if it were not,
        // nothing could arrive: wait like a subscriber nobody publishes to.
        let Some(msg) = msg else {
            return core::future::pending().await;
        };
        self.deliver(&msg, buf)
    }

    fn try_recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Sample<'b>>, MemoryError> {
        match self.pop(buf.len(), None) {
            Poll::Ready(Ok(Some(msg))) => self.deliver(&msg, buf).map(Some),
            Poll::Ready(Ok(None)) | Poll::Pending => Ok(None),
            Poll::Ready(Err(e)) => Err(e),
        }
    }
}

/// Serves requests for a [`MemoryNode`]; deregisters on drop, answering
/// queued requests [`HandlerError::Unavailable`].
pub struct MemoryResponder {
    hub: Rc<RefCell<Hub>>,
    id: u64,
    node: SenderId,
}

impl Drop for MemoryResponder {
    fn drop(&mut self) {
        if let Ok(mut hub) = self.hub.try_borrow_mut() {
            let Some(i) = hub.responders.iter().position(|r| r.id == self.id) else {
                return;
            };
            let slot = hub.responders.remove(i);
            for p in slot.queue {
                hub.reply(p.request, (self.node, Err(HandlerError::Unavailable)));
            }
        }
    }
}

impl Responder for MemoryResponder {
    type Error = MemoryError;

    /// Runs `handler` with the whole of `scratch` as the response buffer
    /// (the request body stays owned by the hub).
    async fn serve_one<H: Handler>(
        &mut self,
        handler: &mut H,
        scratch: &mut [u8],
    ) -> Result<(), MemoryError> {
        let pending = core::future::poll_fn(|cx| {
            let mut hub = self.hub.borrow_mut();
            let Some(slot) = hub.responders.iter_mut().find(|r| r.id == self.id) else {
                return Poll::Pending;
            };
            match slot.queue.pop_front() {
                Some(p) => Poll::Ready(p),
                None => {
                    slot.waker = Some(cx.waker().clone());
                    Poll::Pending
                }
            }
        })
        .await;
        let remaining = pending
            .deadline
            .map(|t| t.saturating_duration_since(Instant::now()));
        let result = match (remaining, Key::new(&pending.key)) {
            (Some(Duration::ZERO), _) => Err(HandlerError::DeadlineExceeded),
            (_, Err(_)) => Err(HandlerError::InvalidRequest),
            (remaining, Ok(key)) => {
                let meta = RequestMeta {
                    sender: Some(pending.sender),
                    principal: pending.principal.as_deref(),
                    deadline_ms: remaining
                        .map(|d| u32::try_from(d.as_millis()).unwrap_or(u32::MAX)),
                };
                let req = Request::new(key, &pending.body).with_meta(meta);
                handler.handle(&req, scratch).and_then(|n| {
                    scratch
                        .get(..n)
                        .map(<[u8]>::to_vec)
                        .ok_or(HandlerError::BufferTooSmall)
                })
            }
        };
        self.hub
            .borrow_mut()
            .reply(pending.request, (self.node, result));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::handler_fn;

    const A: SenderId = SenderId([1; 8]);
    const B: SenderId = SenderId([2; 8]);
    const K: Key<'static> = Key::from_static("app/echo");

    #[tokio::test]
    async fn publish_reaches_matching_subscribers_with_meta() {
        let bus = MemoryBus::with_queue_depth(2);
        let mut a = bus.node(A);
        let mut s = bus
            .node(B)
            .subscriber(KeyFilter::Prefix(Key::from_static("app")))
            .unwrap();
        let mut other = bus
            .node(B)
            .subscriber(KeyFilter::Exact(Key::from_static("x")))
            .unwrap();
        a.publish(K, b"1").await.unwrap();
        let mut buf = [0u8; 32];
        let got = s.recv(&mut buf).await.unwrap();
        assert_eq!(
            (got.key, got.payload, got.meta),
            (K, &b"1"[..], SampleMeta::from_sender(A).with_seq(0))
        );
        assert_eq!(other.try_recv(&mut buf), Ok(None));
        // Depth 2: three more drop the oldest; the gap says one was lost.
        for p in [b"2", b"3", b"4"] {
            a.publish(K, p).await.unwrap();
        }
        let got = s.try_recv(&mut buf).unwrap().unwrap();
        assert_eq!((got.payload, got.meta.gap), (&b"3"[..], Some(1)));
        // Too small: an error, and the sample stays.
        assert_eq!(
            s.try_recv(&mut buf[..8]),
            Err(MemoryError::BufferTooSmall {
                needed: 9,
                capacity: 8
            })
        );
        assert_eq!(s.try_recv(&mut buf).unwrap().unwrap().meta.gap, Some(0));
        assert_eq!(s.try_recv(&mut buf), Ok(None));
    }

    #[tokio::test]
    async fn requests_reach_a_responder_with_meta_and_honour_timeouts() {
        let bus = MemoryBus::new();
        let mut a = bus.node_with_principal(A, "spiffe://test/a");
        let mut r = bus.node(B).responder(KeyFilter::Exact(K)).unwrap();
        let mut seen = None;
        let mut h = handler_fn(|req, out| {
            seen = Some((
                req.meta.sender,
                req.meta.principal.map(String::from),
                req.meta.deadline_ms.is_some(),
            ));
            let out = out
                .get_mut(..req.body.len())
                .ok_or(HandlerError::BufferTooSmall)?;
            out.copy_from_slice(req.body);
            Ok(req.body.len())
        });
        let mut scratch = [0u8; 16];
        let mut resp = [0u8; 16];
        let (got, served) = tokio::join!(
            a.request(K, b"hi", &mut resp),
            r.serve_one(&mut h, &mut scratch)
        );
        served.unwrap();
        assert_eq!(got.unwrap(), b"hi");
        assert_eq!(seen, Some((Some(A), Some("spiffe://test/a".into()), false)));
        // Nobody serves: the timeout ends the call.
        let o = RequestOptions::DEFAULT.with_timeout(Duration::from_millis(20));
        assert_eq!(
            a.request_with(K, b"x", &mut resp, &o).await,
            Err(MemoryError::Timeout)
        );
        // Wrong node, or no responder at all.
        let o = o.with_target(Target::Node(A));
        assert_eq!(
            a.request_with(K, b"x", &mut resp, &o).await,
            Err(MemoryError::NoResponder)
        );
        drop(r);
        assert_eq!(
            a.request(K, b"x", &mut resp).await,
            Err(MemoryError::NoResponder)
        );
        // An unrepresentable timeout is not a panic.
        let huge = RequestOptions::DEFAULT.with_timeout(Duration::MAX);
        let _r2 = bus.node(B).responder(KeyFilter::Exact(K)).unwrap();
        let mut buf = [0u8; 4];
        let gathered = a.request_each(K, b"x", &mut buf, &huge, 0, |_| Flow::Continue);
        assert_eq!(gathered.await, Ok(0));
    }
}
