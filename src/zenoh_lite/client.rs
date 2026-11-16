//! [`ZenohLite`]: the async shell of the board's Zenoh client over one
//! `embedded-io-async` byte stream, implementing the middleware traits.
//!
//! All decisions are in the pure [`codec`](super::codec),
//! [`session`](super::session) and [`tables`](super::tables); this module
//! reads batches, feeds them through the session, writes what it asks for
//! and copies data into the caller's buffers. Time comes from a [`Clock`]
//! (the board's embassy time, tokio in tests); the client is driven by its
//! calls, so a caller that is idle for longer than a keep-alive interval
//! must still call [`ZenohLite::next_event`] (or another receive) to keep
//! the lease. Every wait races the stream against the session's next
//! deadline (keep-alive, lease, handshake timeout) and the call's own.
//!
//! # Opening and reconnecting
//!
//! [`ZenohLite::open`] runs the handshake on the stream it holds and then
//! (re)sends every declaration. A lost session ([`LiteError::Down`],
//! [`LiteError::Io`], [`LiteError::Eof`]) is reconnected by the caller:
//! [`ZenohLite::wait_backoff`] waits out the session's bounded backoff,
//! the caller reconnects the stream ([`ZenohLite::stream_mut`]; on the
//! board, the TCP socket), then calls `open` again, or
//! [`ZenohLite::link_failed`] if the connect failed.
//!
//! # Traits
//!
//! * [`Publisher::publish`]: a Push (Put) with congestion control Block on
//!   a reliable frame, carrying the 16-byte publication attachment (sender
//!   id, the key's sequence, stream id), as the host backend writes it.
//!   Sequences count per key from 0 in a table of [`PUB_STREAMS`] keys (the
//!   oldest key is forgotten when it is full, and restarts at 0); the
//!   sequence advances before anything is checked or sent, so a refused or
//!   failed publication shows as a gap. The stream id hashes the key with
//!   an origin ([`ZenohLite::with_origin`], 0 by default).
//! * [`Subscriber`] on the client yields the samples of every declared
//!   subscription; [`Subscribe::subscribe`] declares one (re-sent after
//!   every open) and borrows the client as a [`LiteSubscription`] yielding
//!   only matching samples (others are dropped and counted). Sender,
//!   sequence and exact gaps come from the attachment through a
//!   [`SeenTable`] of [`SEEN_STREAMS`] streams, as on the host.
//! * [`Requester`] and [`Gather`]: a Request (no consolidation; target best
//!   matching or all) with the query attachment (our sender id, the
//!   timeout) and a timeout extension. [`Target::Node`] queries the
//!   per-node key and accepts only replies naming that node. The first
//!   accepted reply wins (`request_with`); an error reply is
//!   [`LiteError::Handler`] if no success follows; ResponseFinal without a
//!   reply, or the timeout ([`DEFAULT_TIMEOUT_MS`] unless set), is
//!   [`LiteError::NoReply`].
//! * [`Responder::serve_one`] waits for a request to any declared
//!   queryable ([`ZenohLite::declare_queryable`] declares the key and its
//!   per-node key), runs the handler, and answers with a Reply carrying the
//!   reply attachment or a reply error whose payload is the code byte and
//!   our sender id, then ResponseFinal. A request whose key and body do not
//!   fit `scratch` is answered with [`HandlerError::BufferTooSmall`].
//!
//! One stream carries everything, so a call that waits for one kind of
//! message handles the others it reads meanwhile: samples read while a
//! request or `serve_one` waits are dropped and counted
//! ([`Stats::dropped_samples`]); requests read while a receive or a
//! request waits are answered with [`HandlerError::Unavailable`]
//! ([`Stats::busy_queries`]); stale responses are discarded. Firmware that
//! mixes traffic uses [`ZenohLite::next_event`], which returns both
//! samples and queries ([`ZenohLite::reply`] answers the latter).
//!
//! # Buffers and limits
//!
//! `RX` bytes hold one received batch (announced as our batch size, so
//! the router never sends a larger one; a larger one is skipped and
//! counted), `TX` bytes one batch to send including its 2-byte length.
//! Nothing is fragmented: a message that does not fit the agreed batch is
//! [`LiteError::TooLarge`]. [`Publisher::LIMITS`] allows any key with
//! payloads up to `TX - PUB_OVERHEAD`.
//!
//! # Cancel safety
//!
//! Partial reads (length prefix, batch body), the walk through a batch
//! and partial writes live in `self`; a dropped call resumes on the next
//! one (a pending write is finished before anything new is sent), provided
//! the stream's `read` and `write` are cancel safe. [`Subscriber::try_recv`]
//! polls a receive once with a no-op waker and relies on the same.

use core::future::{Future, poll_fn};
use core::pin::pin;
use core::task::{Context, Poll, Waker};

use embedded_io_async::{ErrorType, Read, Write};

use crate::dedup::{SeenTable, Sequence, fnv1a64};
use crate::key::{Key, KeyFilter, KeyFilterBuf, MAX_KEY_LEN};
use crate::transport::{
    Delivery, Flow, Gather, Handler, HandlerError, Limits, Overflow, Publisher, Reply as Gathered,
    Request as MwRequest, RequestMeta, RequestOptions, Requester, Responder, Sample, SampleMeta,
    SenderId, Subscribe, Subscriber, Target, copy_into,
};
use crate::zenoh_wire as wire;

use super::codec::network::{
    Declare, DeclareBody, NetExts, NetworkMessage, Push, PushBody, Put, QOS_BLOCK, QOS_DEFAULT,
    Query, QueryTarget, Reply, ReplyErr, Request, Response, ResponseBody, ResponseFinal, Value,
};
use super::codec::transport::{
    Batch, Close, Cursor, DEFAULT_BATCH, LEN_PREFIX, TransportMessage, write_batch,
};
use super::codec::{CodecError, Encoding, WireExpr, Zid};
use super::session::{Config, DownReason, Reaction, Session, SessionError};
use super::tables::{Declarations, Expr, Mappings, TableError, resolve};

/// Declarations the client can hold (a queryable takes two).
pub const DECLARATIONS: usize = 16;
/// Key expression ids from the router the client remembers.
pub const KEY_MAPPINGS: usize = 8;
/// (sender, stream) pairs remembered for gaps, as on the host.
pub const SEEN_STREAMS: usize = 32;
/// Keys whose publication sequence the client keeps.
pub const PUB_STREAMS: usize = 16;
/// Request timeout unless set with [`ZenohLite::with_timeout`].
pub const DEFAULT_TIMEOUT_MS: u32 = 5_000;
/// Bytes of a publication batch besides the payload, with the longest key:
/// length prefix 2, frame header 6, push header with key 4 + key, QoS 2,
/// put header 1, attachment 18, payload length 5.
pub const PUB_OVERHEAD: usize = 2 + 6 + 4 + MAX_KEY_LEN + 2 + 1 + 18 + 5;

/// Milliseconds and waiting, from the caller's runtime.
pub trait Clock {
    /// Milliseconds since an arbitrary, fixed start; never decreases.
    fn now_ms(&mut self) -> u64;
    /// Completes at or after `deadline_ms`. Must be cancel safe.
    async fn sleep_until_ms(&mut self, deadline_ms: u64);
}

/// Failure of a client operation; `E` is the stream's error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiteError<E> {
    /// The stream failed; the session is down.
    Io(E),
    /// The stream ended (or wrote nothing); the session is down.
    Eof,
    /// The session is not open.
    NotOpen,
    /// `open` before the reconnect backoff has passed.
    Backoff {
        /// Earliest time to open.
        until_ms: u64,
    },
    /// The session went down.
    Down(DownReason),
    /// The message does not fit one batch (no fragmentation).
    TooLarge,
    /// The data needs `needed` bytes but the buffer holds `capacity`.
    BufferTooSmall {
        /// Bytes required.
        needed: usize,
        /// Bytes available.
        capacity: usize,
    },
    /// A declaration table is full or a key expression too long.
    Table(TableError),
    /// A key is not a valid middleware key.
    InvalidKey,
    /// No reply before the final response or the timeout.
    NoReply,
    /// The responder answered with this error.
    Handler(HandlerError),
    /// A message could not be encoded.
    Codec(CodecError),
}

/// Counters of what the client dropped or refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    /// Samples read while waiting for something else, for no declared
    /// subscription, or outside a subscription view's filter.
    pub dropped_samples: u32,
    /// Requests answered with `Unavailable` (or with no reply, for no
    /// declared queryable) because nothing was serving.
    pub busy_queries: u32,
    /// Items the session refused: malformed batches, out-of-order frames,
    /// fragments, unexpected messages.
    pub refused: u32,
    /// Batches larger than our receive buffer, skipped.
    pub oversized_batches: u32,
    /// Responses to requests no longer waited for.
    pub stale_replies: u32,
}

/// A query received by [`ZenohLite::next_event`]; answer it with
/// [`ZenohLite::reply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryEvent<'b> {
    /// The queryable's key (also for per-node queries).
    pub key: Key<'b>,
    /// The request payload.
    pub body: &'b [u8],
    /// Requester and deadline from the query attachment.
    pub meta: RequestMeta<'static>,
    rid: u32,
    reply_to: Expr,
}

/// What [`ZenohLite::next_event`] received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event<'b> {
    /// A sample for a declared subscription.
    Sample(Sample<'b>),
    /// A query for a declared queryable.
    Query(QueryEvent<'b>),
}

/// Partial read state of the next batch; `Copy`, kept in the client so a
/// dropped read loses nothing.
#[derive(Debug, Clone, Copy)]
struct RxState {
    head: [u8; LEN_PREFIX],
    head_have: usize,
    len: usize,
    have: usize,
    discard: bool,
    ready: bool,
    cursor: Cursor,
}

impl RxState {
    const EMPTY: RxState = RxState {
        head: [0; LEN_PREFIX],
        head_have: 0,
        len: 0,
        have: 0,
        discard: false,
        ready: false,
        cursor: Cursor::START,
    };
}

/// Why a read or write ended the link.
enum LinkFail<E> {
    Io(E),
    Eof,
}

/// One read into the batch state. `Ok(true)` when an oversized batch was
/// fully skipped.
async fn read_some<S: Read>(
    stream: &mut S,
    buf: &mut [u8],
    st: &mut RxState,
) -> Result<bool, LinkFail<S::Error>> {
    if st.head_have < LEN_PREFIX {
        let slot = st.head.get_mut(st.head_have..).unwrap_or(&mut []);
        let n = stream.read(slot).await.map_err(LinkFail::Io)?;
        if n == 0 {
            return Err(LinkFail::Eof);
        }
        st.head_have += n;
        if st.head_have >= LEN_PREFIX {
            st.len = usize::from(u16::from_le_bytes(st.head));
            st.have = 0;
            st.discard = st.len > buf.len();
            if st.len == 0 {
                *st = RxState::EMPTY;
            }
        }
        return Ok(false);
    }
    let slot = if st.discard {
        let n = (st.len - st.have).min(buf.len());
        buf.get_mut(..n)
    } else {
        buf.get_mut(st.have..st.len)
    };
    let slot = slot.unwrap_or(&mut []);
    let n = stream.read(slot).await.map_err(LinkFail::Io)?;
    if n == 0 {
        return Err(LinkFail::Eof);
    }
    st.have += n;
    if st.have >= st.len {
        if st.discard {
            *st = RxState::EMPTY;
            return Ok(true);
        }
        st.ready = true;
        st.cursor = Cursor::START;
    }
    Ok(false)
}

/// One batch to send and how much of it was written.
#[derive(Debug)]
struct Tx<const N: usize> {
    buf: [u8; N],
    len: usize,
    sent: usize,
}

impl<const N: usize> Tx<N> {
    const fn new() -> Self {
        Tx {
            buf: [0; N],
            len: 0,
            sent: 0,
        }
    }

    const fn pending(&self) -> bool {
        self.sent < self.len
    }

    fn clear(&mut self) {
        self.len = 0;
        self.sent = 0;
    }

    fn encode<F>(&mut self, max_batch: u16, body: F) -> Result<(), CodecError>
    where
        F: FnOnce(&mut super::codec::Writer<'_>) -> Result<(), CodecError>,
    {
        if self.pending() {
            return Err(CodecError::NoSpace);
        }
        let n = write_batch(&mut self.buf, max_batch, body)?;
        self.len = n;
        self.sent = 0;
        Ok(())
    }

    async fn flush<S: Write>(&mut self, stream: &mut S) -> Result<(), LinkFail<S::Error>> {
        while self.sent < self.len {
            let chunk = self.buf.get(self.sent..self.len).unwrap_or(&[]);
            let n = stream.write(chunk).await.map_err(LinkFail::Io)?;
            if n == 0 {
                return Err(LinkFail::Eof);
            }
            self.sent += n;
        }
        stream.flush().await.map_err(LinkFail::Io)?;
        self.clear();
        Ok(())
    }
}

enum Either<A, B> {
    Left(A),
    Right(B),
}

/// Polls both futures; the first to finish wins and the other is dropped.
async fn race<A: Future, B: Future>(a: A, b: B) -> Either<A::Output, B::Output> {
    let mut a = pin!(a);
    let mut b = pin!(b);
    poll_fn(move |cx| {
        if let Poll::Ready(x) = a.as_mut().poll(cx) {
            return Poll::Ready(Either::Left(x));
        }
        if let Poll::Ready(y) = b.as_mut().poll(cx) {
            return Poll::Ready(Either::Right(y));
        }
        Poll::Pending
    })
    .await
}

/// What a step through the input produced.
enum Step {
    /// The session opened (declarations sent).
    Opened,
    /// An application network message at `start..end` of the batch;
    /// `prev` rewinds to it.
    Message {
        start: usize,
        end: usize,
        prev: Cursor,
    },
    /// The caller's deadline passed.
    Deadline,
}

/// What a receive is after.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Want {
    /// Samples; queries are answered busy.
    Samples,
    /// Queries; samples are dropped.
    Queries,
    /// Both.
    Both,
}

/// What [`ZenohLite::next_inner`] copied to the front of the buffer.
enum Got {
    Sample {
        key_len: usize,
        len: usize,
        meta: SampleMeta,
    },
    Query {
        key_len: usize,
        len: usize,
        meta: RequestMeta<'static>,
        rid: u32,
        reply_to: Expr,
    },
}

/// The sender id of a Zenoh id, as the host backend computes it: FNV-1a
/// 64 of the 16 little-endian id bytes.
pub fn sender_id_of(zid: Zid) -> SenderId {
    SenderId(fnv1a64(&zid.to_le_bytes()).to_le_bytes())
}

/// The metadata of a sample with `attachment`, and the table after
/// recording it (the host's rule). Pure.
pub fn sample_meta(
    seen: SeenTable<SEEN_STREAMS>,
    attachment: Option<&[u8]>,
) -> (SeenTable<SEEN_STREAMS>, SampleMeta) {
    let Some(header) = attachment.and_then(wire::decode_pub) else {
        return (seen, SampleMeta::NONE);
    };
    match header.stream {
        Some(stream) => {
            let (seen, check) = seen.check_stream(header.sender, stream, header.seq);
            (seen, header.meta().with_gap(check.gap()))
        }
        None => (seen, header.meta()),
    }
}

/// The request metadata of a query attachment, the 0.5 form or one with
/// the trace extension: sender, deadline and trace context. The request
/// id is left `None` (K19): it borrows the receive buffer, which the
/// client reuses before the query is answered, so this link does not
/// carry it to the handler. Pure.
pub fn request_meta(attachment: Option<&[u8]>) -> RequestMeta<'static> {
    match attachment.and_then(wire::decode_query) {
        Some(h) => RequestMeta {
            trace: h.trace,
            ..RequestMeta::NONE
                .with_sender(h.sender)
                .with_deadline_ms(h.deadline_ms)
        },
        None => RequestMeta::NONE,
    }
}

/// The Zenoh client. `RX` and `TX` are the receive and send buffers in
/// bytes (see the module docs).
pub struct ZenohLite<S, C, const RX: usize = 2048, const TX: usize = 2048> {
    stream: S,
    clock: C,
    session: Session,
    decls: Declarations<DECLARATIONS>,
    maps: Mappings<KEY_MAPPINGS>,
    sender: SenderId,
    origin: u32,
    pubs: heapless::Vec<(u32, Sequence), PUB_STREAMS>,
    evict: usize,
    seen: SeenTable<SEEN_STREAMS>,
    rx_buf: [u8; RX],
    rx: RxState,
    tx: Tx<TX>,
    stats: Stats,
    timeout_ms: u32,
}

type Res<T, S> = Result<T, LiteError<<S as ErrorType>::Error>>;

impl<S, C, const RX: usize, const TX: usize> ZenohLite<S, C, RX, TX>
where
    S: Read + Write,
    C: Clock,
{
    const BUFFERS_OK: () = assert!(
        RX >= 64 && RX <= u16::MAX as usize && TX >= 64,
        "ZenohLite buffers: RX in 64..=65535, TX >= 64"
    );

    /// A client over `stream` (connected, not yet opened). The configured
    /// batch size is replaced by `RX`.
    pub fn new(stream: S, clock: C, config: Config) -> Self {
        let () = Self::BUFFERS_OK;
        let config = Config {
            batch_size: RX as u16,
            ..config
        };
        ZenohLite {
            stream,
            clock,
            session: Session::new(config),
            decls: Declarations::new(),
            maps: Mappings::new(),
            sender: sender_id_of(config.zid),
            origin: 0,
            pubs: heapless::Vec::new(),
            evict: 0,
            seen: SeenTable::new(),
            rx_buf: [0; RX],
            rx: RxState::EMPTY,
            tx: Tx::new(),
            stats: Stats::default(),
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }

    /// The same client with requests timing out after `timeout_ms` unless
    /// they set their own.
    pub fn with_timeout(self, timeout_ms: u32) -> Self {
        ZenohLite { timeout_ms, ..self }
    }

    /// The same client numbering publication streams under `origin` (for
    /// example a value fresh at every boot, so subscribers see a restart
    /// as new streams).
    pub fn with_origin(self, origin: u32) -> Self {
        ZenohLite { origin, ..self }
    }

    /// Our sender id (the host's `sender_id_of` our Zenoh id).
    pub fn sender_id(&self) -> SenderId {
        self.sender
    }

    /// The session state.
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// The counters.
    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// The stream, to reconnect it.
    pub fn stream_mut(&mut self) -> &mut S {
        &mut self.stream
    }

    /// The clock.
    pub fn clock_mut(&mut self) -> &mut C {
        &mut self.clock
    }

    fn reset_link(&mut self) {
        self.rx = RxState::EMPTY;
        self.tx.clear();
    }

    fn link_fail(&mut self, e: LinkFail<S::Error>) -> LiteError<S::Error> {
        let now = self.clock.now_ms();
        self.session = self.session.link_lost(now);
        self.reset_link();
        match e {
            LinkFail::Io(e) => LiteError::Io(e),
            LinkFail::Eof => LiteError::Eof,
        }
    }

    async fn flush(&mut self) -> Res<(), S> {
        match self.tx.flush(&mut self.stream).await {
            Ok(()) => Ok(()),
            Err(e) => Err(self.link_fail(e)),
        }
    }

    fn session_err(e: SessionError) -> LiteError<S::Error> {
        match e {
            SessionError::Backoff { until } => LiteError::Backoff { until_ms: until },
            SessionError::NotDown | SessionError::NotOpen => LiteError::NotOpen,
        }
    }

    /// Sends a transport message (handshake, keep-alive) in its own batch.
    async fn send_transport(&mut self, msg: &TransportMessage<'_>) -> Res<(), S> {
        self.flush().await?;
        self.tx
            .encode(DEFAULT_BATCH, |w| msg.write(w))
            .map_err(LiteError::Codec)?;
        self.flush().await
    }

    /// Sends network messages in one reliable frame.
    async fn send_network(&mut self, msgs: &[NetworkMessage<'_>]) -> Res<(), S> {
        self.flush().await?;
        let now = self.clock.now_ms();
        let max = self
            .session
            .link()
            .map(|l| l.batch_size)
            .ok_or(LiteError::NotOpen)?;
        let (next, header) = self
            .session
            .next_frame(now, true)
            .map_err(Self::session_err)?;
        self.tx
            .encode(max, |w| {
                header.write(w)?;
                msgs.iter().try_for_each(|m| m.write(w))
            })
            .map_err(|e| match e {
                CodecError::NoSpace => LiteError::TooLarge,
                e => LiteError::Codec(e),
            })?;
        self.session = next;
        self.flush().await
    }

    /// The session went down: sends `close` if given (best effort) and
    /// drops the link state.
    async fn went_down(&mut self, reason: DownReason, close: Option<Close>) -> LiteError<S::Error> {
        if let Some(c) = close {
            self.tx.clear();
            if self.tx.encode(DEFAULT_BATCH, |w| c.write(w)).is_ok() {
                let _ = self.tx.flush(&mut self.stream).await;
            }
        }
        self.reset_link();
        LiteError::Down(reason)
    }

    /// Runs the handshake on the current stream, then sends every
    /// declaration. [`LiteError::Backoff`] if called before the session's
    /// reconnect delay has passed.
    pub async fn open(&mut self) -> Res<(), S> {
        let now = self.clock.now_ms();
        let (next, syn) = self.session.connect(now).map_err(Self::session_err)?;
        self.session = next;
        self.reset_link();
        self.send_transport(&TransportMessage::InitSyn(syn)).await?;
        // Bounded by the handshake timeout, which `step` enforces.
        loop {
            if let Step::Opened = self.step(None).await? {
                return Ok(());
            }
        }
    }

    /// Waits until the session may reconnect.
    pub async fn wait_backoff(&mut self) {
        if let super::session::Phase::Down { retry_at } = self.session.phase() {
            self.clock.sleep_until_ms(retry_at).await;
        }
    }

    /// Records a failed connect of the stream (counts toward the backoff).
    pub fn link_failed(&mut self) {
        let now = self.clock.now_ms();
        self.session = self.session.link_lost(now);
        self.reset_link();
    }

    /// Closes the session (sending Close if a link is up). It stays
    /// closed until [`ZenohLite::restart`].
    pub async fn close(&mut self) -> Res<(), S> {
        let (next, close) = self.session.close();
        self.session = next;
        let result = match close {
            Some(c) => {
                let sent = self.send_transport(&TransportMessage::Close(c)).await;
                sent.or_else(|e| match e {
                    LiteError::Io(_) | LiteError::Eof => Ok(()),
                    e => Err(e),
                })
            }
            None => Ok(()),
        };
        self.reset_link();
        result
    }

    /// Allows [`ZenohLite::open`] again after [`ZenohLite::close`].
    pub fn restart(&mut self) {
        self.session = self.session.restart();
    }

    /// Declares the declaration at `index` and those after it, if open.
    async fn declare_from(&mut self, index: usize) -> Res<(), S> {
        if !self.session.is_open() {
            return Ok(());
        }
        let count = self.decls.entries().len();
        for i in index..count {
            let Some(d) = self.decls.entries().get(i).cloned() else {
                break;
            };
            self.send_network(&[NetworkMessage::Declare(d.declare(None))])
                .await?;
        }
        Ok(())
    }

    /// Declares a subscriber for `filter` (now if open, and after every
    /// open). Declaring an existing filter again does nothing.
    pub async fn declare_subscriber(&mut self, filter: KeyFilter<'_>) -> Res<(), S> {
        let (decls, index) = self
            .decls
            .add_subscriber(filter)
            .map_err(LiteError::Table)?;
        self.decls = decls;
        match index {
            Some(i) => self.declare_from(i).await,
            None => Ok(()),
        }
    }

    /// Undeclares the subscriber for `filter` (now if open; it is not
    /// declared again after an open), so samples for it are no longer
    /// delivered. Undeclaring a filter never declared does nothing.
    pub async fn undeclare_subscriber(&mut self, filter: KeyFilter<'_>) -> Res<(), S> {
        let (decls, id) = self
            .decls
            .remove_subscriber(filter)
            .map_err(LiteError::Table)?;
        self.decls = decls;
        let Some(id) = id else { return Ok(()) };
        if !self.session.is_open() {
            return Ok(());
        }
        self.send_network(&[NetworkMessage::Declare(Declare {
            interest_id: None,
            exts: NetExts::DEFAULT,
            body: DeclareBody::UndeclareSubscriber {
                id,
                wire_expr: None,
            },
        })])
        .await
    }

    /// Declares a queryable on `key` and on its per-node key (now if open,
    /// and after every open).
    pub async fn declare_queryable(&mut self, key: Key<'_>) -> Res<(), S> {
        let (decls, index) = self
            .decls
            .add_queryable(key, self.sender)
            .map_err(LiteError::Table)?;
        self.decls = decls;
        match index {
            Some(i) => self.declare_from(i).await,
            None => Ok(()),
        }
    }

    /// Answers the interest `id` with the declarations it asks for, then a
    /// final declare.
    async fn answer_interest(&mut self, id: u32, options: u8, expr: Option<Expr>) -> Res<(), S> {
        let count = self.decls.entries().len();
        for i in 0..count {
            let Some(d) = self.decls.entries().get(i).cloned() else {
                break;
            };
            let wanted = self
                .decls
                .answering(options, expr.as_deref())
                .any(|a| a.id == d.id);
            if wanted {
                self.send_network(&[NetworkMessage::Declare(d.declare(Some(id)))])
                    .await?;
            }
        }
        let fin = super::codec::network::Declare {
            interest_id: Some(id),
            exts: NetExts::DEFAULT,
            body: super::codec::network::DeclareBody::Final,
        };
        self.send_network(&[NetworkMessage::Declare(fin)]).await
    }

    /// Handles the next item of the current batch.
    async fn process_one(&mut self, now: u64) -> Res<Option<Step>, S> {
        let len = self.rx.len;
        let prev = self.rx.cursor;
        let bytes = self.rx_buf.get(..len).unwrap_or(&[]);
        let mut batch = Batch::resume(bytes, prev);
        let item = match batch.next() {
            None => {
                self.rx = RxState::EMPTY;
                return Ok(None);
            }
            Some(Err(e)) => {
                self.rx = RxState::EMPTY;
                let (next, r) = self.session.on_malformed(now, e);
                self.session = next;
                return match r {
                    Reaction::Lost { reason, close } => Err(self.went_down(reason, close).await),
                    _ => {
                        self.stats.refused = self.stats.refused.saturating_add(1);
                        Ok(None)
                    }
                };
            }
            Some(Ok(item)) => item,
        };
        let cursor = batch.cursor();
        self.rx.cursor = cursor;
        let (next, reaction) = self.session.on_item(now, item);
        self.session = next;
        match reaction {
            Reaction::Nothing => Ok(None),
            Reaction::Send(msg) => {
                // A handshake reply may borrow the batch (the cookie):
                // encode it now, send after.
                self.tx
                    .encode(DEFAULT_BATCH, |w| msg.write(w))
                    .map_err(LiteError::Codec)?;
                self.flush().await?;
                Ok(None)
            }
            Reaction::Opened => {
                self.declare_from(0).await?;
                Ok(Some(Step::Opened))
            }
            Reaction::Refused(_) => {
                self.stats.refused = self.stats.refused.saturating_add(1);
                Ok(None)
            }
            Reaction::Lost { reason, close } => Err(self.went_down(reason, close).await),
            Reaction::Deliver(msg) => match msg {
                NetworkMessage::Push(_)
                | NetworkMessage::Request(_)
                | NetworkMessage::Response(_)
                | NetworkMessage::ResponseFinal(_) => Ok(Some(Step::Message {
                    start: prev.pos(),
                    end: cursor.pos(),
                    prev,
                })),
                NetworkMessage::Declare(d) => {
                    use super::codec::network::DeclareBody;
                    match d.body {
                        DeclareBody::KeyExpr { id, wire_expr } => {
                            let we = WireExpr {
                                sender_mapping: true,
                                ..wire_expr
                            };
                            match self.maps.declare(id, &we) {
                                Ok(m) => self.maps = m,
                                Err(_) => self.stats.refused = self.stats.refused.saturating_add(1),
                            }
                        }
                        DeclareBody::UndeclareKeyExpr { id } => self.maps = self.maps.undeclare(id),
                        _ => {}
                    }
                    Ok(None)
                }
                NetworkMessage::Interest(i) => {
                    let (id, mode, options) = (i.id, i.mode, i.options);
                    let expr = i.wire_expr.and_then(|we| resolve(&self.maps, &we));
                    if mode.is_current() {
                        self.answer_interest(id, options, expr).await?;
                    }
                    Ok(None)
                }
                NetworkMessage::Oam => Ok(None),
            },
        }
    }

    /// Drives the session until an application message arrives, the
    /// session opens, or `until` passes. Fails when the session is not
    /// open or handshaking, or goes down.
    async fn step(&mut self, until: Option<u64>) -> Res<Step, S> {
        use super::session::Phase;
        loop {
            if matches!(self.session.phase(), Phase::Down { .. } | Phase::Closed) {
                return Err(LiteError::NotOpen);
            }
            self.flush().await?;
            let now = self.clock.now_ms();
            let (next, r) = self.session.tick(now);
            self.session = next;
            match r {
                Reaction::Send(m) => {
                    self.send_transport(&m).await?;
                    continue;
                }
                Reaction::Lost { reason, close } => {
                    return Err(self.went_down(reason, close).await);
                }
                _ => {}
            }
            if until.is_some_and(|u| now >= u) {
                return Ok(Step::Deadline);
            }
            if self.rx.ready {
                if let Some(step) = self.process_one(now).await? {
                    return Ok(step);
                }
                continue;
            }
            let wake = match (until, self.session.deadline()) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            let read = read_some(&mut self.stream, &mut self.rx_buf, &mut self.rx);
            let outcome = match wake {
                Some(at) => race(read, self.clock.sleep_until_ms(at)).await,
                None => Either::Left(read.await),
            };
            match outcome {
                Either::Left(Ok(skipped)) => {
                    if skipped {
                        self.stats.oversized_batches =
                            self.stats.oversized_batches.saturating_add(1);
                    }
                }
                Either::Left(Err(e)) => return Err(self.link_fail(e)),
                Either::Right(()) => {}
            }
        }
    }

    /// Answers request `rid` on `expr` with `result`, then ResponseFinal.
    async fn send_reply(
        &mut self,
        rid: u32,
        expr: &str,
        result: Result<&[u8], HandlerError>,
    ) -> Res<(), S> {
        let attachment = wire::encode_reply(self.sender);
        let fin = NetworkMessage::ResponseFinal(ResponseFinal {
            rid,
            exts: NetExts::DEFAULT,
        });
        let response = |body| {
            NetworkMessage::Response(Response {
                rid,
                wire_expr: WireExpr::plain(expr),
                exts: NetExts::DEFAULT,
                responder: None,
                body,
            })
        };
        let sender = self.sender;
        let error = |e: HandlerError| wire::encode_reply_error(e, sender);
        match result {
            Ok(payload) => {
                let body = ResponseBody::Reply(Reply {
                    consolidation: 0,
                    body: PushBody::Put(Put {
                        attachment: Some(&attachment),
                        payload,
                        ..Put::default()
                    }),
                });
                match self.send_network(&[response(body), fin]).await {
                    Err(LiteError::TooLarge) => {
                        let e = error(HandlerError::BufferTooSmall);
                        let body = ResponseBody::Err(ReplyErr {
                            payload: &e,
                            ..ReplyErr::default()
                        });
                        self.send_network(&[response(body), fin]).await
                    }
                    other => other,
                }
            }
            Err(err) => {
                let e = error(err);
                let body = ResponseBody::Err(ReplyErr {
                    payload: &e,
                    ..ReplyErr::default()
                });
                self.send_network(&[response(body), fin]).await
            }
        }
    }

    /// A request nobody is serving: `Unavailable` for a declared queryable,
    /// no reply otherwise.
    async fn answer_busy(&mut self, rid: u32, expr: Option<Expr>) -> Res<(), S> {
        self.stats.busy_queries = self.stats.busy_queries.saturating_add(1);
        match expr {
            Some(e) if self.decls.queryable_for(&e).is_some() => {
                self.send_reply(rid, &e, Err(HandlerError::Unavailable))
                    .await
            }
            _ => {
                let fin = ResponseFinal {
                    rid,
                    exts: NetExts::DEFAULT,
                };
                self.send_network(&[NetworkMessage::ResponseFinal(fin)])
                    .await
            }
        }
    }

    fn drop_sample(&mut self) {
        self.stats.dropped_samples = self.stats.dropped_samples.saturating_add(1);
    }

    /// Waits for a sample or query (per `want`) and copies its key then its
    /// payload or body to the front of `buf`.
    async fn next_inner(&mut self, buf: &mut [u8], want: Want) -> Res<Got, S> {
        if !self.session.is_open() {
            return Err(LiteError::NotOpen);
        }
        loop {
            let Step::Message { start, end, prev } = self.step(None).await? else {
                continue;
            };
            let bytes = self.rx_buf.get(start..end).unwrap_or(&[]);
            let Ok((msg, _)) = NetworkMessage::decode(bytes) else {
                continue;
            };
            match msg {
                NetworkMessage::Push(p) => {
                    let PushBody::Put(put) = p.body else {
                        continue;
                    };
                    let expr = resolve(&self.maps, &p.wire_expr);
                    let key = expr.as_deref().and_then(|e| Key::new(e).ok());
                    let Some(key) = key.filter(|k| want != Want::Queries && self.decls.wants(*k))
                    else {
                        self.drop_sample();
                        continue;
                    };
                    let needed = key.as_str().len() + put.payload.len();
                    if needed > buf.len() {
                        // Kept: the next receive with a larger buffer gets it.
                        self.rx.cursor = prev;
                        return Err(LiteError::BufferTooSmall {
                            needed,
                            capacity: buf.len(),
                        });
                    }
                    let (seen, meta) = sample_meta(self.seen, put.attachment);
                    let key_len = key.as_str().len();
                    // `needed <= buf.len()` was checked above.
                    let Some((k, rest)) = buf.split_at_mut_checked(key_len) else {
                        continue;
                    };
                    k.copy_from_slice(key.as_str().as_bytes());
                    if let Some(p) = rest.get_mut(..put.payload.len()) {
                        p.copy_from_slice(put.payload);
                    }
                    self.seen = seen;
                    return Ok(Got::Sample {
                        key_len,
                        len: put.payload.len(),
                        meta,
                    });
                }
                NetworkMessage::Request(q) => {
                    let rid = q.id;
                    let expr = resolve(&self.maps, &q.wire_expr);
                    let decl_key = expr
                        .as_deref()
                        .and_then(|e| self.decls.queryable_for(e))
                        .map(|d| d.key);
                    let (Some(key), Some(reply_to), true) =
                        (decl_key, expr.clone(), want != Want::Samples)
                    else {
                        self.answer_busy(rid, expr).await?;
                        continue;
                    };
                    let body = q.body.body.map_or(&[][..], |v| v.payload);
                    let meta = request_meta(q.body.attachment);
                    let key_len = key.as_str().len();
                    let needed = key_len + body.len();
                    if needed > buf.len() {
                        self.send_reply(rid, &reply_to, Err(HandlerError::BufferTooSmall))
                            .await?;
                        continue;
                    }
                    let Some((k, rest)) = buf.split_at_mut_checked(key_len) else {
                        continue;
                    };
                    k.copy_from_slice(key.as_str().as_bytes());
                    if let Some(b) = rest.get_mut(..body.len()) {
                        b.copy_from_slice(body);
                    }
                    return Ok(Got::Query {
                        key_len,
                        len: body.len(),
                        meta,
                        rid,
                        reply_to,
                    });
                }
                NetworkMessage::Response(_) | NetworkMessage::ResponseFinal(_) => {
                    self.stats.stale_replies = self.stats.stale_replies.saturating_add(1);
                }
                _ => {}
            }
        }
    }

    /// Waits for the next sample of a declared subscription or query of a
    /// declared queryable; key and payload (or body) are copied into
    /// `buf`. A sample that does not fit is kept
    /// ([`LiteError::BufferTooSmall`]); a query that does not fit is
    /// answered with [`HandlerError::BufferTooSmall`].
    pub async fn next_event<'b>(&mut self, buf: &'b mut [u8]) -> Res<Event<'b>, S> {
        let got = self.next_inner(buf, Want::Both).await?;
        let buf: &'b [u8] = buf;
        Ok(match got {
            Got::Sample { key_len, len, meta } => {
                Event::Sample(sample_view(buf, key_len, len, meta)?)
            }
            Got::Query {
                key_len,
                len,
                meta,
                rid,
                reply_to,
            } => {
                let (k, rest) = buf.split_at_checked(key_len).ok_or(LiteError::InvalidKey)?;
                let key = key_of(k)?;
                Event::Query(QueryEvent {
                    key,
                    body: rest.get(..len).unwrap_or(&[]),
                    meta,
                    rid,
                    reply_to,
                })
            }
        })
    }

    /// Answers a query from [`ZenohLite::next_event`]: a reply with the
    /// reply attachment, or a reply error, then the final response.
    pub async fn reply(
        &mut self,
        query: &QueryEvent<'_>,
        result: Result<&[u8], HandlerError>,
    ) -> Res<(), S> {
        self.send_reply(query.rid, &query.reply_to, result).await
    }

    async fn recv_any<'b>(&mut self, buf: &'b mut [u8]) -> Res<Sample<'b>, S> {
        let got = self.next_inner(buf, Want::Samples).await?;
        let buf: &'b [u8] = buf;
        match got {
            Got::Sample { key_len, len, meta } => sample_view(buf, key_len, len, meta),
            Got::Query { .. } => Err(LiteError::NoReply),
        }
    }

    /// Sends a request and hands every reply to `on_reply` until it says
    /// stop (returns `Some(result)`), the final response, or the timeout.
    async fn query<F>(
        &mut self,
        key: Key<'_>,
        request: &[u8],
        options: &RequestOptions<'_>,
        target: QueryTarget,
        mut on_reply: F,
    ) -> Res<(), S>
    where
        F: FnMut(Option<SenderId>, Result<&[u8], HandlerError>) -> Flow,
    {
        if !self.session.is_open() {
            return Err(LiteError::NotOpen);
        }
        let timeout_ms = options.timeout_ms().unwrap_or(self.timeout_ms);
        let (next, rid) = self.session.next_request_id().map_err(Self::session_err)?;
        self.session = next;
        let node = match options.target {
            Target::Any => None,
            Target::Node(id) => Some(wire::node_key_buf(id, key).ok_or(LiteError::InvalidKey)?),
        };
        let selector = node.as_deref().unwrap_or(key.as_str());
        let attachment =
            wire::encode_query(self.sender, timeout_ms, options.trace, options.request_id);
        let msg = Request {
            id: rid,
            wire_expr: WireExpr::plain(selector),
            exts: NetExts::DEFAULT,
            target,
            budget: None,
            timeout_ms: Some(u64::from(timeout_ms)),
            body: Query {
                consolidation: 1,
                parameters: "",
                source: None,
                body: Some(Value {
                    encoding: Encoding::EMPTY,
                    payload: request,
                }),
                attachment: Some(&attachment),
            },
        };
        self.send_network(&[NetworkMessage::Request(msg)]).await?;
        let deadline = self.clock.now_ms().saturating_add(u64::from(timeout_ms));
        loop {
            let (start, end) = match self.step(Some(deadline)).await? {
                Step::Deadline => return Ok(()),
                Step::Opened => continue,
                Step::Message { start, end, .. } => (start, end),
            };
            let bytes = self.rx_buf.get(start..end).unwrap_or(&[]);
            let Ok((msg, _)) = NetworkMessage::decode(bytes) else {
                continue;
            };
            match msg {
                NetworkMessage::Response(r) if r.rid == rid => {
                    let (replier, result) = match r.body {
                        ResponseBody::Reply(Reply {
                            body: PushBody::Put(p),
                            ..
                        }) => (p.attachment.and_then(wire::decode_reply), Ok(p.payload)),
                        ResponseBody::Reply(_) => continue,
                        ResponseBody::Err(e) if wire::is_timeout_error(e.payload) => continue,
                        ResponseBody::Err(e) => {
                            let (err, replier) = wire::decode_reply_error(e.payload);
                            (replier, Err(err))
                        }
                    };
                    if !wire::accepts(options.target, replier) {
                        continue;
                    }
                    if on_reply(replier, result) == Flow::Stop {
                        return Ok(());
                    }
                }
                NetworkMessage::ResponseFinal(f) if f.rid == rid => return Ok(()),
                NetworkMessage::Response(_) | NetworkMessage::ResponseFinal(_) => {
                    self.stats.stale_replies = self.stats.stale_replies.saturating_add(1);
                }
                NetworkMessage::Push(_) => self.drop_sample(),
                NetworkMessage::Request(q) => {
                    let rid = q.id;
                    let expr = resolve(&self.maps, &q.wire_expr);
                    self.answer_busy(rid, expr).await?;
                }
                _ => {}
            }
        }
    }

    /// The next sequence of publication stream `stream`, advancing it.
    fn next_seq(&mut self, stream: u32) -> Sequence {
        if let Some(entry) = self.pubs.iter_mut().find(|(s, _)| *s == stream) {
            let seq = entry.1;
            entry.1 = seq.next();
            return seq;
        }
        let fresh = (stream, Sequence::default().next());
        if self.pubs.push(fresh).is_err() {
            let slot = self.evict % PUB_STREAMS;
            if let Some(e) = self.pubs.get_mut(slot) {
                *e = fresh;
            }
            self.evict = slot + 1;
        }
        Sequence::default()
    }
}

fn key_of<E>(bytes: &[u8]) -> Result<Key<'_>, LiteError<E>> {
    core::str::from_utf8(bytes)
        .ok()
        .and_then(|s| Key::new(s).ok())
        .ok_or(LiteError::InvalidKey)
}

fn sample_view<E>(
    buf: &[u8],
    key_len: usize,
    len: usize,
    meta: SampleMeta,
) -> Result<Sample<'_>, LiteError<E>> {
    let (k, rest) = buf.split_at_checked(key_len).ok_or(LiteError::InvalidKey)?;
    Ok(Sample::new(key_of(k)?, rest.get(..len).unwrap_or(&[])).with_meta(meta))
}

impl<S, C, const RX: usize, const TX: usize> Publisher for ZenohLite<S, C, RX, TX>
where
    S: Read + Write,
    C: Clock,
{
    type Error = LiteError<S::Error>;

    /// Any key, payloads up to `TX - PUB_OVERHEAD` (and within the batch
    /// size the router agreed, which is at most `RX`).
    const LIMITS: Limits = Limits::new(MAX_KEY_LEN, TX.saturating_sub(PUB_OVERHEAD));

    /// A Push (Put) on `key` with congestion control Block and the
    /// publication attachment, on a reliable frame. The key's sequence
    /// advances first, even if the publication is then refused.
    async fn publish(&mut self, key: Key<'_>, payload: &[u8]) -> Result<(), Self::Error> {
        let stream = wire::stream_id(self.origin, key);
        let seq = self.next_seq(stream);
        if !self.session.is_open() {
            return Err(LiteError::NotOpen);
        }
        let attachment = wire::encode_pub(self.sender, seq, stream);
        let push = Push {
            wire_expr: WireExpr::plain(key.as_str()),
            exts: NetExts {
                qos: QOS_DEFAULT | QOS_BLOCK,
                ..NetExts::DEFAULT
            },
            body: PushBody::Put(Put {
                attachment: Some(&attachment),
                payload,
                ..Put::default()
            }),
        };
        self.send_network(&[NetworkMessage::Push(push)]).await
    }
}

impl<S, C, const RX: usize, const TX: usize> Subscriber for ZenohLite<S, C, RX, TX>
where
    S: Read + Write,
    C: Clock,
{
    type Error = LiteError<S::Error>;

    /// The router's queue may drop the newest samples under congestion
    /// (for publishers using `Drop`), and samples read while the client
    /// waits for something else are dropped; one TCP stream keeps each
    /// sender's order.
    const DELIVERY: Delivery = Delivery::new(Overflow::DropNewest, true);

    /// A sample's key and payload fit one received batch.
    const LIMITS: Limits = Limits::shared(MAX_KEY_LEN, RX);

    /// The next sample of any declared subscription.
    async fn recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Sample<'b>, Self::Error> {
        self.recv_any(buf).await
    }

    /// Polls [`Subscriber::recv`] once with a no-op waker.
    fn try_recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Sample<'b>>, Self::Error> {
        let fut = pin!(self.recv_any(buf));
        match fut.poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(r) => r.map(Some),
            Poll::Pending => Ok(None),
        }
    }
}

/// The client borrowed as a subscriber of one filter (see the module docs).
pub struct LiteSubscription<'a, S, C, const RX: usize, const TX: usize> {
    client: &'a mut ZenohLite<S, C, RX, TX>,
    filter: KeyFilterBuf<MAX_KEY_LEN>,
}

impl<S, C, const RX: usize, const TX: usize> LiteSubscription<'_, S, C, RX, TX>
where
    S: Read + Write,
    C: Clock,
{
    async fn recv_matching<'b>(&mut self, buf: &'b mut [u8]) -> Res<Sample<'b>, S> {
        loop {
            let got = self.client.next_inner(buf, Want::Samples).await?;
            let Got::Sample { key_len, len, meta } = got else {
                continue;
            };
            let matches = buf
                .get(..key_len)
                .and_then(|k| core::str::from_utf8(k).ok())
                .and_then(|k| Key::new(k).ok())
                .is_some_and(|k| self.filter.matches(k));
            if !matches {
                self.client.drop_sample();
                continue;
            }
            return sample_view(buf, key_len, len, meta);
        }
    }
}

impl<S, C, const RX: usize, const TX: usize> Subscriber for LiteSubscription<'_, S, C, RX, TX>
where
    S: Read + Write,
    C: Clock,
{
    type Error = LiteError<S::Error>;
    const DELIVERY: Delivery = <ZenohLite<S, C, RX, TX> as Subscriber>::DELIVERY;
    const LIMITS: Limits = <ZenohLite<S, C, RX, TX> as Subscriber>::LIMITS;

    /// The next sample matching this view's filter.
    async fn recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Sample<'b>, Self::Error> {
        self.recv_matching(buf).await
    }

    /// Polls [`Subscriber::recv`] once with a no-op waker.
    fn try_recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Sample<'b>>, Self::Error> {
        let fut = pin!(self.recv_matching(buf));
        match fut.poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(r) => r.map(Some),
            Poll::Pending => Ok(None),
        }
    }
}

impl<S, C, const RX: usize, const TX: usize> Subscribe for ZenohLite<S, C, RX, TX>
where
    S: Read + Write,
    C: Clock,
{
    type Subscriber<'a>
        = LiteSubscription<'a, S, C, RX, TX>
    where
        Self: 'a;
    type Error = LiteError<S::Error>;

    /// Declares the subscriber and borrows the client as its view.
    async fn subscribe<'a>(
        &'a mut self,
        filter: KeyFilter<'_>,
    ) -> Result<Self::Subscriber<'a>, Self::Error> {
        let owned = filter.to_buf().map_err(|_| LiteError::InvalidKey)?;
        self.declare_subscriber(filter).await?;
        Ok(LiteSubscription {
            client: self,
            filter: owned,
        })
    }
}

impl<S, C, const RX: usize, const TX: usize> Requester for ZenohLite<S, C, RX, TX>
where
    S: Read + Write,
    C: Clock,
{
    type Error = LiteError<S::Error>;

    /// A Zenoh get (see the module docs): the first accepted successful
    /// reply is copied into `response`.
    async fn request_with<'b>(
        &mut self,
        key: Key<'_>,
        request: &[u8],
        response: &'b mut [u8],
        options: &RequestOptions<'_>,
    ) -> Result<&'b [u8], Self::Error> {
        let capacity = response.len();
        let mut outcome: Option<Result<usize, LiteError<S::Error>>> = None;
        let mut last_error = None;
        self.query(
            key,
            request,
            options,
            QueryTarget::BestMatching,
            |_, result| match result {
                Ok(payload) => {
                    outcome = Some(match copy_into(response, payload) {
                        Some(_) => Ok(payload.len()),
                        None => Err(LiteError::BufferTooSmall {
                            needed: payload.len(),
                            capacity,
                        }),
                    });
                    Flow::Stop
                }
                Err(e) => {
                    last_error = Some(e);
                    Flow::Continue
                }
            },
        )
        .await?;
        match outcome {
            Some(Ok(n)) => {
                let response: &'b [u8] = response;
                Ok(response.get(..n).unwrap_or(&[]))
            }
            Some(Err(e)) => Err(e),
            None => Err(last_error.map_or(LiteError::NoReply, LiteError::Handler)),
        }
    }
}

impl<S, C, const RX: usize, const TX: usize> Gather for ZenohLite<S, C, RX, TX>
where
    S: Read + Write,
    C: Clock,
{
    type Error = LiteError<S::Error>;

    /// A Zenoh get with target all; every accepted reply (success or
    /// error) is passed on, until `max_replies`, [`Flow::Stop`], the final
    /// response or the timeout (not an error).
    async fn request_each<F>(
        &mut self,
        key: Key<'_>,
        request: &[u8],
        buf: &mut [u8],
        options: &RequestOptions<'_>,
        max_replies: usize,
        mut on_reply: F,
    ) -> Result<usize, Self::Error>
    where
        F: FnMut(Gathered<'_>) -> Flow,
    {
        if max_replies == 0 {
            return Ok(0);
        }
        let mut seen = 0usize;
        self.query(key, request, options, QueryTarget::All, |sender, result| {
            seen += 1;
            let result = match result {
                Ok(payload) => copy_into(buf, payload)
                    .map(|r| &*r)
                    .ok_or(HandlerError::BufferTooSmall),
                Err(e) => Err(e),
            };
            let flow = on_reply(Gathered { sender, result });
            if flow == Flow::Stop || seen >= max_replies {
                Flow::Stop
            } else {
                Flow::Continue
            }
        })
        .await?;
        Ok(seen)
    }
}

impl<S, C, const RX: usize, const TX: usize> Responder for ZenohLite<S, C, RX, TX>
where
    S: Read + Write,
    C: Clock,
{
    type Error = LiteError<S::Error>;

    /// Serves one request to any declared queryable: the request's key and
    /// body go to the front of `scratch`, the rest is the response buffer.
    async fn serve_one<H: Handler>(
        &mut self,
        handler: &mut H,
        scratch: &mut [u8],
    ) -> Result<(), Self::Error> {
        let Got::Query {
            key_len,
            len,
            meta,
            rid,
            reply_to,
        } = self.next_inner(scratch, Want::Queries).await?
        else {
            return Ok(());
        };
        let (head, response) = scratch.split_at_mut(key_len.saturating_add(len).min(scratch.len()));
        let (k, body) = head.split_at(key_len.min(head.len()));
        let result = match key_of::<S::Error>(k) {
            Ok(key) => handler
                .handle(&MwRequest::new(key, body).with_meta(meta), response)
                .and_then(|n| response.get(..n).ok_or(HandlerError::BufferTooSmall)),
            Err(_) => Err(HandlerError::InvalidRequest),
        };
        self.send_reply(rid, &reply_to, result).await
    }
}

#[cfg(test)]
mod tests;
