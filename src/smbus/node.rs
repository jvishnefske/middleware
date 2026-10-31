//! [`SmbusNode`]: the middleware over one SMBus multi-master bus.
//!
//! A node is a master when it sends (an `embedded-hal-async`
//! [`I2c`]) and a target at its own address
//! when it receives (an [`I2cTarget`]); a [`Clock`] times its retries,
//! timeouts and reassembly. Every message is cut into SMBus Block Writes
//! with PEC ([`wire`]):
//!
//! * [`Publisher::publish`] writes to the broadcast address
//!   ([`Config::broadcast`], the general call `0x00` by default);
//! * [`Requester::request_with`] writes the request to the responder's
//!   address and the responder's [`Responder::serve_one`] writes the reply
//!   (or the handler's error) back to the requester's address.
//!
//! # Type parameters
//!
//! `B` is the block size (data bytes per block, 32 for SMBus 2.0, up to 255
//! for SMBus 3.x), `M` the largest message body a node sends or
//! reassembles, `R` the reassembly slots and `Q` the inbox entries. Every
//! node on a bus must use the same `B` and the same `M` (a node drops
//! messages over its own `M`). With the defaults (32, 512, 4, 4) a node
//! holds about 5.6 KB besides its master, target and clock: `M × (R + Q)`
//! for the reassembly slots and the inbox, a 258-byte transaction buffer
//! per direction, a 255-byte fragment buffer and the 16-sender sequence
//! table.
//!
//! # Receiving and the inbox
//!
//! Every receive (`recv`, `try_recv`, `serve_one`, and the wait for a
//! reply inside `request_with`) first drains every transaction the target
//! adapter already holds (at most [`MAX_PUMP`] per call) without waiting:
//! each is checked (PEC, framing), reassembled, and its message handled:
//!
//! * a complete publication or request goes to the **inbox** of `Q`
//!   entries; when the inbox is full the arriving message is dropped and
//!   counted ([`RxStats::inbox_dropped`]); queued ones are kept;
//! * a reply or error goes to the `request_with` waiting for it (same
//!   responder, same correlation id); any other reply is stale and counted.
//!
//! `recv` and `try_recv` return the oldest queued publication; `serve_one`
//! takes the oldest queued request. A queued request whose deadline (or,
//! without one, [`Config::default_timeout_ms`]) has passed is dropped: its
//! requester has given up. Publications and requests share the inbox; a
//! node that never serves should not be sent requests.
//!
//! # Delivery and gaps
//!
//! [`DELIVERY`] is [`Overflow::DropNewest`] with per-sender order: one
//! master's blocks reach a target in the order written, its messages are
//! sent one after another, and the inbox drops the arriving publication.
//! Every publication carries the sender's 8-bit publication sequence
//! ([`SampleMeta::seq`]); the node keeps the last one accepted from each of
//! up to [`SEEN_SENDERS`] senders (a [`SeenTable`] over one stream per
//! sender) and reports [`SampleMeta::gap`] exactly: publications of that
//! sender lost on the bus (a PEC failure, a missing fragment, an expired
//! reassembly) or dropped by a full inbox are counted in the gap of the
//! next one accepted. The first publication seen from a sender, and one
//! whose sequence went backwards (a restarted sender), report `None`. The
//! sequence counts every key the sender publishes, so a [`Filtered`] view
//! sees the gaps of the whole stream, not of its keys. A publication with
//! the same sequence as the last one accepted from its sender is a
//! duplicate and is dropped ([`RxStats::duplicates`]); a sender that
//! restarts and happens to begin at that number loses its first message.
//!
//! # Requests
//!
//! [`Target::Node`] must name an SMBus node ([`Address::sender_id`]); any
//! other id is [`SmbusError::UnsupportedTarget`]. [`Target::Any`] goes to
//! the node's default peer ([`Config::with_default_peer`]) and is
//! [`SmbusError::NoTarget`] without one: the bus has no service routing,
//! and broadcasting a request would collect an answer from every node.
//! `request_with` honours `options.timeout` on the node's clock, and
//! without one waits [`Config::default_timeout_ms`]; the timeout covers
//! sending as well as waiting. The caller's timeout travels with the
//! request, and the handler sees what is left of it as
//! [`RequestMeta::deadline_ms`] (measured from when the responder finished
//! reassembling it). The handler sees the requester's address as
//! [`RequestMeta::sender`]; the address is claimed by the sender, not
//! authenticated, so the principal is always `None`. [`Gather`](crate::Gather)
//! is not implemented.
//!
//! # Split
//!
//! [`Split::split`] borrows the node as an [`SmbusTx`] (the master, the
//! transmit state) and an [`SmbusRx`] (the target, the receive state and
//! inbox), both sharing the clock and the configuration by shared
//! reference: one task can publish while another waits in `recv`, without
//! `RefCell` or shared ownership.
//!
//! # Cancel safety
//!
//! All receive state (the reassembly table, the inbox, the sequence table)
//! lives in the node, so dropping a receive loses no completed message,
//! provided the adapter's `listen` is cancel safe (its contract). Dropping
//! `publish` (or a `serve_one` writing its reply, or a `request_with`
//! sending) part way may have sent some of the message's blocks; the
//! message is then abandoned and never resent. Its receivers discard the
//! partial message when the next one from the node starts or its
//! reassembly times out, and never deliver it. Sequence numbers and
//! correlation ids advance before the first write. A dropped
//! `request_with` abandons the request; its late reply is discarded as
//! stale. The master's `write` must itself tolerate being dropped (the HAL
//! aborts the transfer).

use core::convert::Infallible;
use core::future::{Future, poll_fn};
use core::pin::pin;
use core::task::Poll;

use embedded_hal::i2c::Error as _;
use embedded_hal_async::i2c::I2c;

use super::address::Address;
use super::reassembly::{Discards, Head, Push, Reassembler};
use super::retry::{Decision, Fault, GiveUp, RetryPolicy, Tries};
use super::target::{Clock, I2cTarget, Transaction};
use super::wire::{
    self, Body, Fragment, Kind, MAX_BLOCK, MAX_TRANSACTION, MIN_BLOCK, Message, PUBLISH_HEAD,
    REQUEST_HEAD, WireError,
};
use crate::dedup::{SeenTable, SeqCheck};
use crate::key::{Key, KeyFilter, KeyFilterBuf, MAX_KEY_LEN};
use crate::transport::{
    Delivery, Handler, HandlerError, Limits, Overflow, Publisher, Request, RequestMeta,
    RequestOptions, Requester, Responder, Sample, SampleMeta, SenderId, Split, Subscribe,
    Subscriber, Target, sample_into,
};

/// The default block size: SMBus 2.0's 32 data bytes.
pub const DEFAULT_BLOCK: usize = wire::SMBUS2_BLOCK;

/// The default largest message body.
pub const DEFAULT_MESSAGE: usize = 512;

/// The default number of reassembly slots.
pub const DEFAULT_SLOTS: usize = 4;

/// The default number of inbox entries.
pub const DEFAULT_INBOX: usize = 4;

/// Transactions one receive call drains from the adapter without waiting.
pub const MAX_PUMP: usize = 64;

/// Transactions a waiting call (`recv`, `serve_one`, the reply wait of
/// `request_with`) reads without finding what it waits for before it gives
/// up with [`SmbusError::SkipLimit`].
pub const MAX_IDLE_TRANSACTIONS: usize = 1024;

/// Senders whose last publication sequence a node remembers for gaps.
pub const SEEN_SENDERS: usize = 16;

/// The default request timeout when the caller sets none.
pub const DEFAULT_TIMEOUT_MS: u32 = 1_000;

/// The default reassembly timeout: a message's fragments must all arrive
/// within this time of its first.
pub const DEFAULT_REASSEMBLY_TIMEOUT_MS: u32 = 1_000;

/// What a node's subscribers deliver (see the module docs).
pub const DELIVERY: Delivery = Delivery::new(Overflow::DropNewest, true);

/// What every send and receive needs besides its own state: the clock,
/// the configuration and the node's `B` and `M`.
struct Ctx<'a, C> {
    clock: &'a C,
    cfg: &'a Config,
    block: usize,
    max: usize,
}

impl<C> Clone for Ctx<'_, C> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<C> Copy for Ctx<'_, C> {}

impl<'a, C> Ctx<'a, C> {
    const fn new(clock: &'a C, cfg: &'a Config, block: usize, max: usize) -> Self {
        Ctx {
            clock,
            cfg,
            block,
            max,
        }
    }
}

/// The one stream id every sender's publications are numbered in.
const PUBLISH_STREAM: u32 = 0;

/// Failure of an SMBus node operation; `ME` is the master's error, `TE`
/// the target adapter's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmbusError<ME, TE> {
    /// The master reported a bus error, overrun or other failure.
    Bus(ME),
    /// The target adapter failed.
    Target(TE),
    /// Every allowed attempt of a block lost arbitration.
    ArbitrationLost,
    /// The destination did not acknowledge on every allowed attempt.
    NoAcknowledge,
    /// No reply within the request's timeout.
    Timeout,
    /// The responder's handler failed.
    Handler(HandlerError),
    /// The caller's buffer is smaller than the message.
    BufferTooSmall,
    /// The message exceeds the node's `M` bytes.
    TooLarge,
    /// `Target::Any` and no default peer is configured.
    NoTarget,
    /// The target is not an SMBus node's id, or is this node or the
    /// broadcast address.
    UnsupportedTarget(SenderId),
    /// [`MAX_IDLE_TRANSACTIONS`] transactions passed without what the call
    /// waits for.
    SkipLimit,
    /// A queued message failed to decode (not expected: messages are
    /// checked before they are queued).
    Malformed,
}

/// Why a [`Config`] was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigError {
    /// The broadcast address is the node's own address.
    BroadcastIsOwnAddress,
    /// The default peer is the node's own address or the broadcast address.
    InvalidPeer,
}

/// A node's addresses and policies. Build with [`Config::new`] and the
/// `with_` methods.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    address: Address,
    broadcast: Address,
    default_peer: Option<Address>,
    retry: RetryPolicy,
    reassembly_timeout_ms: u32,
    default_timeout_ms: u32,
}

impl Config {
    /// A node at `address`, broadcasting to the general call address, with
    /// no default peer, [`RetryPolicy::DEFAULT`] and the default timeouts.
    pub const fn new(address: Address) -> Config {
        Config {
            address,
            broadcast: Address::GENERAL_CALL,
            default_peer: None,
            retry: RetryPolicy::DEFAULT,
            reassembly_timeout_ms: DEFAULT_REASSEMBLY_TIMEOUT_MS,
            default_timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }

    /// The same configuration broadcasting to `broadcast`, which every
    /// node's target must also answer (see [`I2cTarget`]).
    pub const fn with_broadcast(self, broadcast: Address) -> Result<Config, ConfigError> {
        if broadcast.get() == self.address.get() {
            return Err(ConfigError::BroadcastIsOwnAddress);
        }
        if let Some(p) = self.default_peer
            && p.get() == broadcast.get()
        {
            return Err(ConfigError::InvalidPeer);
        }
        Ok(Config { broadcast, ..self })
    }

    /// The same configuration sending `Target::Any` requests to `peer`.
    pub const fn with_default_peer(self, peer: Address) -> Result<Config, ConfigError> {
        if peer.get() == self.address.get() || peer.get() == self.broadcast.get() {
            return Err(ConfigError::InvalidPeer);
        }
        Ok(Config {
            default_peer: Some(peer),
            ..self
        })
    }

    /// The same configuration with `retry`.
    pub const fn with_retry(self, retry: RetryPolicy) -> Config {
        Config { retry, ..self }
    }

    /// The same configuration with a reassembly timeout of `ms`.
    pub const fn with_reassembly_timeout_ms(self, ms: u32) -> Config {
        Config {
            reassembly_timeout_ms: ms,
            ..self
        }
    }

    /// The same configuration waiting `ms` for a reply when the caller sets
    /// no timeout.
    pub const fn with_default_timeout_ms(self, ms: u32) -> Config {
        Config {
            default_timeout_ms: ms,
            ..self
        }
    }

    /// The node's own address.
    pub const fn address(&self) -> Address {
        self.address
    }

    /// The broadcast address.
    pub const fn broadcast(&self) -> Address {
        self.broadcast
    }

    /// Where `Target::Any` requests go.
    pub const fn default_peer(&self) -> Option<Address> {
        self.default_peer
    }

    /// The retry policy.
    pub const fn retry(&self) -> RetryPolicy {
        self.retry
    }

    /// The reassembly timeout.
    pub const fn reassembly_timeout_ms(&self) -> u32 {
        self.reassembly_timeout_ms
    }

    /// The request timeout used when the caller sets none.
    pub const fn default_timeout_ms(&self) -> u32 {
        self.default_timeout_ms
    }
}

/// Transmit counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TxStats {
    /// Blocks written again after a lost arbitration.
    pub arbitration_retries: u32,
    /// Blocks written again after a NACK.
    pub nack_retries: u32,
    /// Broadcast messages nobody acknowledged (dropped).
    pub unheard: u32,
}

/// Receive counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RxStats {
    /// Blocks dropped for a bad Packet Error Code.
    pub pec_errors: u32,
    /// Blocks or messages with bad framing.
    pub malformed: u32,
    /// Transactions not of this protocol: another command, another address,
    /// a request or reply broadcast, or the node's own address as sender.
    pub foreign: u32,
    /// Transactions longer than the largest block.
    pub oversized: u32,
    /// Fragments and partial messages the reassembler discarded.
    pub reassembly: Discards,
    /// Publications with the sequence of the last one accepted.
    pub duplicates: u32,
    /// Messages dropped because the inbox was full.
    pub inbox_dropped: u32,
    /// Queued requests dropped because their deadline passed.
    pub expired_requests: u32,
    /// Replies nobody was waiting for.
    pub stale_replies: u32,
}

/// Both directions' counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    /// Receive counters.
    pub rx: RxStats,
    /// Transmit counters.
    pub tx: TxStats,
}

/// Transmit state: the block being written, the fragment being cut, the
/// next publication sequence and correlation id, and counters.
struct TxState {
    block: [u8; MAX_TRANSACTION],
    chunk: [u8; MAX_BLOCK],
    publish_seq: u8,
    rpc_id: u8,
    stats: TxStats,
}

impl TxState {
    const fn new() -> Self {
        TxState {
            block: [0; MAX_TRANSACTION],
            chunk: [0; MAX_BLOCK],
            publish_seq: 0,
            rpc_id: 0,
            stats: TxStats {
                arbitration_retries: 0,
                nack_retries: 0,
                unheard: 0,
            },
        }
    }

    fn take_publish_seq(&mut self) -> u8 {
        let s = self.publish_seq;
        self.publish_seq = s.wrapping_add(1);
        s
    }

    fn take_rpc_id(&mut self) -> u8 {
        let s = self.rpc_id;
        self.rpc_id = s.wrapping_add(1);
        s
    }
}

/// One queued publication or request: its reassembled body.
#[derive(Clone, Copy)]
struct Entry<const M: usize> {
    kind: Kind,
    source: Address,
    id: u8,
    gap: Option<u16>,
    arrived_ms: u64,
    order: u32,
    len: usize,
    bytes: [u8; M],
}

impl<const M: usize> Entry<M> {
    fn body(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or_default()
    }
}

/// `Q` queued messages, oldest first by arrival order.
struct Inbox<const Q: usize, const M: usize> {
    entries: [Option<Entry<M>>; Q],
    next_order: u32,
}

impl<const Q: usize, const M: usize> Inbox<Q, M> {
    const fn new() -> Self {
        Inbox {
            entries: [None; Q],
            next_order: 0,
        }
    }

    /// Queues a message; false (nothing changed) when full or too long.
    fn push(&mut self, head: Head, gap: Option<u16>, now_ms: u64, body: &[u8]) -> bool {
        let Some(slot) = self.entries.iter_mut().find(|e| e.is_none()) else {
            return false;
        };
        let mut bytes = [0u8; M];
        let Some(dst) = bytes.get_mut(..body.len()) else {
            return false;
        };
        dst.copy_from_slice(body);
        *slot = Some(Entry {
            kind: head.kind,
            source: head.source,
            id: head.id,
            gap,
            arrived_ms: now_ms,
            order: self.next_order,
            len: body.len(),
            bytes,
        });
        self.next_order = self.next_order.wrapping_add(1);
        true
    }

    /// The index of the oldest queued message of `kind`.
    fn oldest(&self, kind: Kind) -> Option<usize> {
        let next = self.next_order;
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| e.as_ref().filter(|e| e.kind == kind).map(|e| (i, e)))
            .max_by_key(|(_, e)| next.wrapping_sub(e.order))
            .map(|(i, _)| i)
    }

    fn get(&self, i: usize) -> Option<&Entry<M>> {
        self.entries.get(i).and_then(Option::as_ref)
    }

    fn remove(&mut self, i: usize) {
        if let Some(e) = self.entries.get_mut(i) {
            *e = None;
        }
    }

    fn len(&self) -> usize {
        self.entries.iter().flatten().count()
    }

    /// Drops queued requests whose deadline (or `default_ms`) has passed at
    /// `now_ms`; returns how many.
    fn expire_requests(&mut self, now_ms: u64, default_ms: u32) -> u32 {
        let mut n = 0;
        for e in self.entries.iter_mut() {
            let expired = e.as_ref().is_some_and(|e| {
                e.kind == Kind::Request && {
                    let budget = match wire::decode_message(Kind::Request, e.body()) {
                        Ok(Message::Request {
                            deadline_ms: Some(d),
                            ..
                        }) => d,
                        _ => default_ms,
                    };
                    now_ms.saturating_sub(e.arrived_ms) > u64::from(budget)
                }
            });
            if expired {
                *e = None;
                n += 1;
            }
        }
        n
    }
}

/// What processing one transaction produced for a waiting requester.
enum Got<'a> {
    /// Nothing for a requester (queued, discarded or partial).
    Nothing,
    /// A reply or error.
    Reply {
        head: Head,
        result: Result<&'a [u8], HandlerError>,
    },
}

/// Receive state: the transaction buffer, reassembly, inbox and the
/// per-sender publication sequences.
struct RxState<const M: usize, const R: usize, const Q: usize> {
    buf: [u8; MAX_TRANSACTION],
    reasm: Reassembler<R, M>,
    inbox: Inbox<Q, M>,
    seen: SeenTable<SEEN_SENDERS>,
    stats: RxStats,
}

impl<const M: usize, const R: usize, const Q: usize> RxState<M, R, Q> {
    const fn new() -> Self {
        RxState {
            buf: [0; MAX_TRANSACTION],
            reasm: Reassembler::new(),
            inbox: Inbox::new(),
            seen: SeenTable::new(),
            stats: RxStats {
                pec_errors: 0,
                malformed: 0,
                foreign: 0,
                oversized: 0,
                reassembly: Discards {
                    duplicates: 0,
                    missing: 0,
                    oversized: 0,
                    orphans: 0,
                    expired: 0,
                    evicted: 0,
                },
                duplicates: 0,
                inbox_dropped: 0,
                expired_requests: 0,
                stale_replies: 0,
            },
        }
    }

    fn stats(&self) -> RxStats {
        RxStats {
            reassembly: self.reasm.discards(),
            ..self.stats
        }
    }

    /// Checks, reassembles and handles one transaction now in `buf`.
    fn process(&mut self, cfg: &Config, block: usize, t: Transaction, now_ms: u64) -> Got<'_> {
        let RxState {
            buf,
            reasm,
            inbox,
            seen,
            stats,
        } = self;
        let Some(bytes) = buf.get(..t.len) else {
            stats.oversized = stats.oversized.saturating_add(1);
            return Got::Nothing;
        };
        let to = if t.address == cfg.address.get() {
            cfg.address
        } else if t.address == cfg.broadcast.get() {
            cfg.broadcast
        } else {
            stats.foreign = stats.foreign.saturating_add(1);
            return Got::Nothing;
        };
        let frag = match wire::decode_block(to, bytes, block) {
            Ok(f) => f,
            Err(WireError::Pec) => {
                stats.pec_errors = stats.pec_errors.saturating_add(1);
                return Got::Nothing;
            }
            Err(WireError::Foreign(_)) => {
                stats.foreign = stats.foreign.saturating_add(1);
                return Got::Nothing;
            }
            Err(_) => {
                stats.malformed = stats.malformed.saturating_add(1);
                return Got::Nothing;
            }
        };
        let broadcast = to == cfg.broadcast;
        if frag.source == cfg.address || (broadcast && frag.kind != Kind::Publish) {
            stats.foreign = stats.foreign.saturating_add(1);
            return Got::Nothing;
        }
        let Push::Complete { head, body } = reasm.push(&frag, now_ms, cfg.reassembly_timeout_ms)
        else {
            return Got::Nothing;
        };
        let Ok(message) = wire::decode_message(head.kind, body) else {
            stats.malformed = stats.malformed.saturating_add(1);
            return Got::Nothing;
        };
        match message {
            Message::Publish { .. } => {
                let sender = head.source.sender_id();
                let (next, check) = seen.check_stream(sender, PUBLISH_STREAM, head.id);
                if check == SeqCheck::Repeat {
                    stats.duplicates = stats.duplicates.saturating_add(1);
                } else if inbox.push(head, check.gap(), now_ms, body) {
                    *seen = next;
                } else {
                    stats.inbox_dropped = stats.inbox_dropped.saturating_add(1);
                }
                Got::Nothing
            }
            Message::Request { .. } => {
                if !inbox.push(head, None, now_ms, body) {
                    stats.inbox_dropped = stats.inbox_dropped.saturating_add(1);
                }
                Got::Nothing
            }
            Message::Reply { body } => Got::Reply {
                head,
                result: Ok(body),
            },
            Message::Error { error } => Got::Reply {
                head,
                result: Err(error),
            },
        }
    }

    /// Processes a transaction for a caller not waiting for a reply.
    fn absorb(&mut self, cfg: &Config, block: usize, t: Transaction, now_ms: u64) {
        let stale = matches!(self.process(cfg, block, t, now_ms), Got::Reply { .. });
        if stale {
            self.stats.stale_replies = self.stats.stale_replies.saturating_add(1);
        }
    }

    /// Drains what the adapter already holds (at most [`MAX_PUMP`]) and
    /// expires queued requests.
    fn pump<T: I2cTarget, C: Clock>(
        &mut self,
        target: &mut T,
        ctx: Ctx<'_, C>,
    ) -> Result<(), T::Error> {
        for _ in 0..MAX_PUMP {
            match target.try_listen(&mut self.buf)? {
                Some(t) => self.absorb(ctx.cfg, ctx.block, t, ctx.clock.now_ms()),
                None => break,
            }
        }
        let n = self
            .inbox
            .expire_requests(ctx.clock.now_ms(), ctx.cfg.default_timeout_ms);
        self.stats.expired_requests = self.stats.expired_requests.saturating_add(n);
        Ok(())
    }

    /// Discards queued publications `filter` rejects up to the oldest one
    /// it selects, and returns that one's index (bounded by `Q`).
    fn selected(&mut self, filter: Option<KeyFilter<'_>>) -> Option<usize> {
        for _ in 0..Q {
            let i = self.inbox.oldest(Kind::Publish)?;
            let keep = match self
                .inbox
                .get(i)
                .map(|e| wire::decode_message(Kind::Publish, e.body()))
            {
                Some(Ok(Message::Publish { key, .. })) => filter.is_none_or(|f| f.matches(key)),
                _ => false,
            };
            if keep {
                return Some(i);
            }
            self.inbox.remove(i);
        }
        None
    }

    /// Copies the queued publication `i` into `buf` as a sample and removes
    /// it; a `buf` too small keeps it.
    fn take<'b, ME, TE>(
        &mut self,
        i: usize,
        buf: &'b mut [u8],
    ) -> Result<Sample<'b>, SmbusError<ME, TE>> {
        let entry = self.inbox.get(i).ok_or(SmbusError::Malformed)?;
        let Ok(Message::Publish { key, payload }) =
            wire::decode_message(Kind::Publish, entry.body())
        else {
            return Err(SmbusError::Malformed);
        };
        let meta = SampleMeta::from_sender(entry.source.sender_id())
            .with_seq(entry.id)
            .with_gap(entry.gap);
        let sample = sample_into(buf, key.as_str(), payload)
            .ok_or(SmbusError::BufferTooSmall)?
            .with_meta(meta);
        self.inbox.remove(i);
        Ok(sample)
    }

    /// Waits for the next publication `filter` selects, inbox first.
    async fn recv_matching<'b, T: I2cTarget, C: Clock, ME>(
        &mut self,
        target: &mut T,
        ctx: Ctx<'_, C>,
        filter: Option<KeyFilter<'_>>,
        buf: &'b mut [u8],
    ) -> Result<Sample<'b>, SmbusError<ME, T::Error>> {
        for _ in 0..MAX_IDLE_TRANSACTIONS {
            self.pump(target, ctx).map_err(SmbusError::Target)?;
            if let Some(i) = self.selected(filter) {
                return self.take(i, buf);
            }
            let t = target
                .listen(&mut self.buf)
                .await
                .map_err(SmbusError::Target)?;
            self.absorb(ctx.cfg, ctx.block, t, ctx.clock.now_ms());
        }
        Err(SmbusError::SkipLimit)
    }

    /// Returns a publication `filter` selects if one has already arrived.
    fn try_recv_matching<'b, T: I2cTarget, C: Clock, ME>(
        &mut self,
        target: &mut T,
        ctx: Ctx<'_, C>,
        filter: Option<KeyFilter<'_>>,
        buf: &'b mut [u8],
    ) -> Result<Option<Sample<'b>>, SmbusError<ME, T::Error>> {
        self.pump(target, ctx).map_err(SmbusError::Target)?;
        match self.selected(filter) {
            Some(i) => self.take(i, buf).map(Some),
            None => Ok(None),
        }
    }

    /// Waits for the reply (or error) with correlation `id` from `from`,
    /// copying a reply into `response`.
    async fn wait_reply<'b, T: I2cTarget, C: Clock, ME>(
        &mut self,
        target: &mut T,
        ctx: Ctx<'_, C>,
        from: Address,
        id: u8,
        response: &'b mut [u8],
    ) -> Result<&'b [u8], SmbusError<ME, T::Error>> {
        for _ in 0..MAX_IDLE_TRANSACTIONS {
            let t = match target
                .try_listen(&mut self.buf)
                .map_err(SmbusError::Target)?
            {
                Some(t) => t,
                None => target
                    .listen(&mut self.buf)
                    .await
                    .map_err(SmbusError::Target)?,
            };
            let stale = match self.process(ctx.cfg, ctx.block, t, ctx.clock.now_ms()) {
                Got::Reply { head, result } if head.source == from && head.id == id => {
                    let body = result.map_err(SmbusError::Handler)?;
                    let dst = response
                        .get_mut(..body.len())
                        .ok_or(SmbusError::BufferTooSmall)?;
                    dst.copy_from_slice(body);
                    return Ok(dst);
                }
                Got::Reply { .. } => true,
                Got::Nothing => false,
            };
            if stale {
                self.stats.stale_replies = self.stats.stale_replies.saturating_add(1);
            }
        }
        Err(SmbusError::SkipLimit)
    }

    /// Waits for a request, inbox first, and returns its inbox index.
    async fn next_request<T: I2cTarget, C: Clock, ME>(
        &mut self,
        target: &mut T,
        ctx: Ctx<'_, C>,
    ) -> Result<usize, SmbusError<ME, T::Error>> {
        for _ in 0..MAX_IDLE_TRANSACTIONS {
            self.pump(target, ctx).map_err(SmbusError::Target)?;
            if let Some(i) = self.inbox.oldest(Kind::Request) {
                return Ok(i);
            }
            let t = target
                .listen(&mut self.buf)
                .await
                .map_err(SmbusError::Target)?;
            self.absorb(ctx.cfg, ctx.block, t, ctx.clock.now_ms());
        }
        Err(SmbusError::SkipLimit)
    }
}

/// How a block write ended without an error.
enum Written {
    /// The destination acknowledged the block.
    Sent,
    /// Nobody acknowledged the broadcast address.
    Unheard,
}

/// Writes one block with the bounded retries of `cfg.retry`.
async fn write_block<I2C: I2c, C: Clock, TE>(
    i2c: &mut I2C,
    stats: &mut TxStats,
    ctx: Ctx<'_, C>,
    to: Address,
    bytes: &[u8],
) -> Result<Written, SmbusError<I2C::Error, TE>> {
    let Ctx { clock, cfg, .. } = ctx;
    let broadcast = to == cfg.broadcast;
    let mut tries = Tries::default();
    for _ in 0..cfg.retry.max_writes() {
        let err = match i2c.write(to.get(), bytes).await {
            Ok(()) => return Ok(Written::Sent),
            Err(e) => e,
        };
        let fault = Fault::from_kind(err.kind());
        match cfg.retry.decide(cfg.address, fault, broadcast, tries) {
            Decision::Retry { delay_us, tries: t } => {
                if t.arbitration > tries.arbitration {
                    stats.arbitration_retries = stats.arbitration_retries.saturating_add(1);
                } else {
                    stats.nack_retries = stats.nack_retries.saturating_add(1);
                }
                tries = t;
                clock.delay_us(delay_us).await;
            }
            Decision::Unheard => return Ok(Written::Unheard),
            Decision::GiveUp(GiveUp::ArbitrationLost) => return Err(SmbusError::ArbitrationLost),
            Decision::GiveUp(GiveUp::NoAcknowledge) => return Err(SmbusError::NoAcknowledge),
            Decision::Fatal => return Err(SmbusError::Bus(err)),
        }
    }
    // `decide` gives up within `max_writes`; this is the same verdict.
    Err(if tries.nack > 0 {
        SmbusError::NoAcknowledge
    } else {
        SmbusError::ArbitrationLost
    })
}

impl TxState {
    /// Cuts `msg` into blocks of `block` bytes and writes them to `to`.
    async fn send<I2C: I2c, C: Clock, TE>(
        &mut self,
        i2c: &mut I2C,
        ctx: Ctx<'_, C>,
        to: Address,
        id: u8,
        msg: &Message<'_>,
    ) -> Result<(), SmbusError<I2C::Error, TE>> {
        let Ctx {
            cfg, block, max, ..
        } = ctx;
        let body = Body::of(msg);
        if body.len() > max {
            return Err(SmbusError::TooLarge);
        }
        let count = wire::fragment_count(body.len(), block).map_err(|_| SmbusError::TooLarge)?;
        let cap = wire::fragment_capacity(block);
        let TxState {
            block: out,
            chunk,
            stats,
            ..
        } = self;
        for index in 0..count {
            let piece = chunk.get_mut(..cap).ok_or(SmbusError::TooLarge)?;
            let n = body.copy_from(index * cap, piece);
            let frag = Fragment {
                kind: msg.kind(),
                source: cfg.address,
                id,
                // `count <= MAX_FRAGMENTS` (128).
                index: u8::try_from(index).map_err(|_| SmbusError::TooLarge)?,
                last: index + 1 == count,
                body: piece.get(..n).unwrap_or_default(),
            };
            let len =
                wire::encode_block(to, &frag, block, out).map_err(|_| SmbusError::TooLarge)?;
            let bytes = out.get(..len).ok_or(SmbusError::TooLarge)?;
            match write_block(i2c, stats, ctx, to, bytes).await? {
                Written::Sent => {}
                Written::Unheard => {
                    stats.unheard = stats.unheard.saturating_add(1);
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Publishes `key`/`payload` to the broadcast address.
    async fn publish<I2C: I2c, C: Clock, TE>(
        &mut self,
        i2c: &mut I2C,
        ctx: Ctx<'_, C>,
        key: Key<'_>,
        payload: &[u8],
    ) -> Result<(), SmbusError<I2C::Error, TE>> {
        let id = self.take_publish_seq();
        let msg = Message::Publish { key, payload };
        self.send(i2c, ctx, ctx.cfg.broadcast, id, &msg).await
    }
}

/// Which of two futures finished first.
enum Either<A, B> {
    Left(A),
    Right(B),
}

/// Polls `a` and `b` until one completes; the other is dropped.
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

/// Waits `ms` milliseconds on `clock`, in waits of at most one second
/// (bounded: `ms / 1000 + 1` waits).
async fn sleep_ms<C: Clock>(clock: &C, ms: u32) {
    for _ in 0..ms / 1000 {
        clock.delay_us(1_000_000).await;
    }
    clock.delay_us((ms % 1000) * 1000).await;
}

/// The address a request for `target` goes to.
fn destination<ME, TE>(cfg: &Config, target: &Target) -> Result<Address, SmbusError<ME, TE>> {
    match *target {
        Target::Any => cfg.default_peer.ok_or(SmbusError::NoTarget),
        Target::Node(id) => Address::from_sender_id(id)
            .filter(|&a| a != cfg.address && a != cfg.broadcast)
            .ok_or(SmbusError::UnsupportedTarget(id)),
    }
}

/// An SMBus multi-master node (see the module docs).
pub struct SmbusNode<
    I2C,
    T,
    C,
    const B: usize = DEFAULT_BLOCK,
    const M: usize = DEFAULT_MESSAGE,
    const R: usize = DEFAULT_SLOTS,
    const Q: usize = DEFAULT_INBOX,
> {
    i2c: I2C,
    target: T,
    clock: C,
    config: Config,
    tx: TxState,
    rx: RxState<M, R, Q>,
}

impl<I2C, T, C, const B: usize, const M: usize, const R: usize, const Q: usize>
    SmbusNode<I2C, T, C, B, M, R, Q>
{
    /// Compile-time bounds on the type parameters.
    const VALID: () = assert!(
        B >= MIN_BLOCK
            && B <= MAX_BLOCK
            && M <= wire::max_body(B)
            && M > MAX_KEY_LEN + REQUEST_HEAD
            && Q >= 1,
        "SmbusNode: need 4 <= B <= 255, MAX_KEY_LEN + 5 < M <= 128 * (B - 3), Q >= 1"
    );

    /// A node mastering `i2c`, receiving through `target`, timed by
    /// `clock`, configured by `config`.
    pub fn new(i2c: I2C, target: T, clock: C, config: Config) -> Self {
        let () = Self::VALID;
        SmbusNode {
            i2c,
            target,
            clock,
            config,
            tx: TxState::new(),
            rx: RxState::new(),
        }
    }

    /// The configuration.
    pub const fn config(&self) -> &Config {
        &self.config
    }

    /// This node's [`SenderId`], as other nodes see it.
    pub const fn sender_id(&self) -> SenderId {
        self.config.address.sender_id()
    }

    /// Counters of both directions since creation.
    pub fn stats(&self) -> Stats {
        Stats {
            rx: self.rx.stats(),
            tx: self.tx.stats,
        }
    }

    /// Messages (publications and requests) queued in the inbox.
    pub fn inbox_len(&self) -> usize {
        self.rx.inbox.len()
    }

    /// Returns the master, the target adapter and the clock. Queued
    /// messages and partial reassemblies are discarded.
    pub fn into_parts(self) -> (I2C, T, C) {
        (self.i2c, self.target, self.clock)
    }
}

/// What every node and half can carry, as their `LIMITS`: any key with a
/// payload sharing `M` bytes less the key length byte.
pub const fn limits(m: usize) -> Limits {
    Limits::shared(MAX_KEY_LEN, m.saturating_sub(PUBLISH_HEAD))
}

impl<
    I2C: I2c,
    T: I2cTarget,
    C: Clock,
    const B: usize,
    const M: usize,
    const R: usize,
    const Q: usize,
> Publisher for SmbusNode<I2C, T, C, B, M, R, Q>
{
    type Error = SmbusError<I2C::Error, T::Error>;

    /// See [`limits`]: with the default `M` = 512, a 128-byte key leaves
    /// room for 383 payload bytes.
    const LIMITS: Limits = limits(M);

    /// Broadcasts `payload` on `key` in as many blocks as it takes. A
    /// broadcast nobody acknowledges is dropped and counted
    /// ([`TxStats::unheard`]), not an error.
    async fn publish(&mut self, key: Key<'_>, payload: &[u8]) -> Result<(), Self::Error> {
        let ctx = Ctx::new(&self.clock, &self.config, B, M);
        self.tx.publish(&mut self.i2c, ctx, key, payload).await
    }
}

impl<
    I2C: I2c,
    T: I2cTarget,
    C: Clock,
    const B: usize,
    const M: usize,
    const R: usize,
    const Q: usize,
> Subscriber for SmbusNode<I2C, T, C, B, M, R, Q>
{
    type Error = SmbusError<I2C::Error, T::Error>;

    /// See [`DELIVERY`].
    const DELIVERY: Delivery = DELIVERY;
    const LIMITS: Limits = limits(M);

    /// The oldest queued publication, whatever its key, after draining the
    /// adapter; waits for traffic when none is queued. `buf` must hold the
    /// key and the payload; a smaller one is
    /// [`SmbusError::BufferTooSmall`] and keeps the publication.
    async fn recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Sample<'b>, Self::Error> {
        let ctx = Ctx::new(&self.clock, &self.config, B, M);
        self.rx
            .recv_matching(&mut self.target, ctx, None, buf)
            .await
    }

    /// As `recv`, without waiting: drains the adapter with `try_listen`.
    fn try_recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Sample<'b>>, Self::Error> {
        let ctx = Ctx::new(&self.clock, &self.config, B, M);
        self.rx.try_recv_matching(&mut self.target, ctx, None, buf)
    }
}

impl<
    I2C: I2c,
    T: I2cTarget,
    C: Clock,
    const B: usize,
    const M: usize,
    const R: usize,
    const Q: usize,
> Subscribe for SmbusNode<I2C, T, C, B, M, R, Q>
{
    type Subscriber<'a>
        = Filtered<'a, I2C, T, C, B, M, R, Q>
    where
        Self: 'a;
    type Error = SmbusError<I2C::Error, T::Error>;

    /// Borrows the node as a [`Filtered`] subscriber holding a copy of
    /// `filter`. Never waits; never fails for a valid filter.
    async fn subscribe<'a>(
        &'a mut self,
        filter: KeyFilter<'_>,
    ) -> Result<Self::Subscriber<'a>, Self::Error> {
        let filter = filter.to_buf().map_err(|_| SmbusError::TooLarge)?;
        Ok(Filtered { node: self, filter })
    }
}

/// An [`SmbusNode`] borrowed as a subscriber to the keys matching a
/// [`KeyFilter`]; made by [`Subscribe::subscribe`]. Queued publications on
/// other keys are discarded when reached.
pub struct Filtered<
    'n,
    I2C,
    T,
    C,
    const B: usize = DEFAULT_BLOCK,
    const M: usize = DEFAULT_MESSAGE,
    const R: usize = DEFAULT_SLOTS,
    const Q: usize = DEFAULT_INBOX,
> {
    node: &'n mut SmbusNode<I2C, T, C, B, M, R, Q>,
    filter: KeyFilterBuf<MAX_KEY_LEN>,
}

impl<I2C, T, C, const B: usize, const M: usize, const R: usize, const Q: usize>
    Filtered<'_, I2C, T, C, B, M, R, Q>
{
    /// The filter this view applies.
    pub fn filter(&self) -> KeyFilter<'_> {
        self.filter.as_filter()
    }
}

impl<
    I2C: I2c,
    T: I2cTarget,
    C: Clock,
    const B: usize,
    const M: usize,
    const R: usize,
    const Q: usize,
> Subscriber for Filtered<'_, I2C, T, C, B, M, R, Q>
{
    type Error = SmbusError<I2C::Error, T::Error>;

    /// See [`DELIVERY`].
    const DELIVERY: Delivery = DELIVERY;
    const LIMITS: Limits = limits(M);

    /// As the node's `recv`, skipping keys the filter rejects.
    async fn recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Sample<'b>, Self::Error> {
        let n = &mut *self.node;
        let ctx = Ctx::new(&n.clock, &n.config, B, M);
        n.rx.recv_matching(&mut n.target, ctx, Some(self.filter.as_filter()), buf)
            .await
    }

    /// As the node's `try_recv`, skipping keys the filter rejects.
    fn try_recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Sample<'b>>, Self::Error> {
        let n = &mut *self.node;
        let ctx = Ctx::new(&n.clock, &n.config, B, M);
        n.rx.try_recv_matching(&mut n.target, ctx, Some(self.filter.as_filter()), buf)
    }
}

impl<
    I2C: I2c,
    T: I2cTarget,
    C: Clock,
    const B: usize,
    const M: usize,
    const R: usize,
    const Q: usize,
> Requester for SmbusNode<I2C, T, C, B, M, R, Q>
{
    type Error = SmbusError<I2C::Error, T::Error>;

    /// Writes the request to the target's address and waits for its reply,
    /// within `options.timeout` (or [`Config::default_timeout_ms`]) on the
    /// node's clock, sending included; then [`SmbusError::Timeout`]. The
    /// caller's timeout is carried to the handler. Publications and
    /// requests that arrive meanwhile are queued; other replies are stale.
    /// See the module docs for targets.
    async fn request_with<'b>(
        &mut self,
        key: Key<'_>,
        request: &[u8],
        response: &'b mut [u8],
        options: &RequestOptions,
    ) -> Result<&'b [u8], Self::Error> {
        let SmbusNode {
            i2c,
            target,
            clock,
            config,
            tx,
            rx,
        } = self;
        let to = destination(config, &options.target)?;
        let id = tx.take_rpc_id();
        let timeout_ms = options.timeout_ms().unwrap_or(config.default_timeout_ms);
        let msg = Message::Request {
            deadline_ms: options.timeout_ms(),
            key,
            body: request,
        };
        let ctx = Ctx::new(&*clock, &*config, B, M);
        let work = async {
            tx.send(i2c, ctx, to, id, &msg).await?;
            rx.wait_reply(target, ctx, to, id, response).await
        };
        match race(work, sleep_ms(ctx.clock, timeout_ms)).await {
            Either::Left(r) => r,
            Either::Right(()) => Err(SmbusError::Timeout),
        }
    }
}

impl<
    I2C: I2c,
    T: I2cTarget,
    C: Clock,
    const B: usize,
    const M: usize,
    const R: usize,
    const Q: usize,
> Responder for SmbusNode<I2C, T, C, B, M, R, Q>
{
    type Error = SmbusError<I2C::Error, T::Error>;

    /// Takes the oldest queued request (waiting for one if none is queued),
    /// runs `handler` and writes its reply or error back to the requester.
    /// The request is copied into the front of `scratch` and the response
    /// built in the rest; a request that does not fit `scratch` is answered
    /// with [`HandlerError::BufferTooSmall`] and reported as
    /// [`SmbusError::BufferTooSmall`], as is a response over `M` bytes
    /// (answered likewise). The handler sees the requester's [`SenderId`]
    /// and the remaining deadline.
    async fn serve_one<H: Handler>(
        &mut self,
        handler: &mut H,
        scratch: &mut [u8],
    ) -> Result<(), Self::Error> {
        let SmbusNode {
            i2c,
            target,
            clock,
            config,
            tx,
            rx,
        } = self;
        let ctx = Ctx::new(&*clock, &*config, B, M);
        let i = rx.next_request(target, ctx).await?;
        let entry = rx.inbox.get(i).ok_or(SmbusError::Malformed)?;
        let (source, id, arrived_ms, len) = (entry.source, entry.id, entry.arrived_ms, entry.len);
        let Some((req_buf, resp_buf)) = scratch.split_at_mut_checked(len) else {
            rx.inbox.remove(i);
            let msg = Message::Error {
                error: HandlerError::BufferTooSmall,
            };
            tx.send(i2c, ctx, source, id, &msg).await?;
            return Err(SmbusError::BufferTooSmall);
        };
        req_buf.copy_from_slice(entry.body());
        rx.inbox.remove(i);
        let Ok(Message::Request {
            deadline_ms,
            key,
            body,
        }) = wire::decode_message(Kind::Request, req_buf)
        else {
            return Err(SmbusError::Malformed);
        };
        let waited = ctx.clock.now_ms().saturating_sub(arrived_ms);
        let mut meta = RequestMeta::NONE.with_sender(source.sender_id());
        if let Some(d) = deadline_ms {
            meta =
                meta.with_deadline_ms(d.saturating_sub(u32::try_from(waited).unwrap_or(u32::MAX)));
        }
        let result = handler
            .handle(&Request::new(key, body).with_meta(meta), resp_buf)
            .and_then(|n| resp_buf.get(..n).ok_or(HandlerError::BufferTooSmall));
        let (msg, too_large) = match result {
            Ok(body) if body.len() <= M => (Message::Reply { body }, false),
            Ok(_) => (
                Message::Error {
                    error: HandlerError::BufferTooSmall,
                },
                true,
            ),
            Err(error) => (Message::Error { error }, false),
        };
        tx.send(i2c, ctx, source, id, &msg).await?;
        if too_large {
            return Err(SmbusError::BufferTooSmall);
        }
        Ok(())
    }
}

/// The publishing half of a split [`SmbusNode`]: the master and the
/// transmit state, with the clock and configuration shared.
pub struct SmbusTx<'n, I2C, C, const B: usize = DEFAULT_BLOCK, const M: usize = DEFAULT_MESSAGE> {
    i2c: &'n mut I2C,
    tx: &'n mut TxState,
    clock: &'n C,
    config: &'n Config,
}

impl<I2C, C, const B: usize, const M: usize> SmbusTx<'_, I2C, C, B, M> {
    /// Transmit counters.
    pub fn stats(&self) -> TxStats {
        self.tx.stats
    }
}

impl<I2C: I2c, C: Clock, const B: usize, const M: usize> Publisher for SmbusTx<'_, I2C, C, B, M> {
    type Error = SmbusError<I2C::Error, Infallible>;

    /// See [`limits`].
    const LIMITS: Limits = limits(M);

    /// As the node's `publish`.
    async fn publish(&mut self, key: Key<'_>, payload: &[u8]) -> Result<(), Self::Error> {
        let ctx = Ctx::new(self.clock, self.config, B, M);
        self.tx.publish(self.i2c, ctx, key, payload).await
    }
}

/// The receiving half of a split [`SmbusNode`]: the target adapter, the
/// receive state and the inbox, with the clock and configuration shared.
/// Yields every publication, like the node's own [`Subscriber`]. Requests
/// it receives are queued (and expire) as on the node; replies are stale.
pub struct SmbusRx<
    'n,
    T,
    C,
    const B: usize = DEFAULT_BLOCK,
    const M: usize = DEFAULT_MESSAGE,
    const R: usize = DEFAULT_SLOTS,
    const Q: usize = DEFAULT_INBOX,
> {
    target: &'n mut T,
    rx: &'n mut RxState<M, R, Q>,
    clock: &'n C,
    config: &'n Config,
}

impl<T, C, const B: usize, const M: usize, const R: usize, const Q: usize>
    SmbusRx<'_, T, C, B, M, R, Q>
{
    /// Receive counters.
    pub fn stats(&self) -> RxStats {
        self.rx.stats()
    }
}

impl<T: I2cTarget, C: Clock, const B: usize, const M: usize, const R: usize, const Q: usize>
    Subscriber for SmbusRx<'_, T, C, B, M, R, Q>
{
    type Error = SmbusError<Infallible, T::Error>;

    /// See [`DELIVERY`].
    const DELIVERY: Delivery = DELIVERY;
    const LIMITS: Limits = limits(M);

    /// As the node's `recv`.
    async fn recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Sample<'b>, Self::Error> {
        let ctx = Ctx::new(self.clock, self.config, B, M);
        self.rx.recv_matching(self.target, ctx, None, buf).await
    }

    /// As the node's `try_recv`.
    fn try_recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Sample<'b>>, Self::Error> {
        let ctx = Ctx::new(self.clock, self.config, B, M);
        self.rx.try_recv_matching(self.target, ctx, None, buf)
    }
}

impl<
    I2C: I2c,
    T: I2cTarget,
    C: Clock,
    const B: usize,
    const M: usize,
    const R: usize,
    const Q: usize,
> Split for SmbusNode<I2C, T, C, B, M, R, Q>
{
    type Tx<'a>
        = SmbusTx<'a, I2C, C, B, M>
    where
        Self: 'a;
    type Rx<'a>
        = SmbusRx<'a, T, C, B, M, R, Q>
    where
        Self: 'a;

    /// Borrows the master with the transmit state and the target with the
    /// receive state; both share the clock and configuration.
    fn split(&mut self) -> (Self::Tx<'_>, Self::Rx<'_>) {
        (
            SmbusTx {
                i2c: &mut self.i2c,
                tx: &mut self.tx,
                clock: &self.clock,
                config: &self.config,
            },
            SmbusRx {
                target: &mut self.target,
                rx: &mut self.rx,
                clock: &self.clock,
                config: &self.config,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    fn a(raw: u8) -> Address {
        Address::node(raw).unwrap()
    }

    fn head(source: u8, kind: Kind, id: u8) -> Head {
        Head {
            source: a(source),
            kind,
            id,
        }
    }

    #[test]
    fn default_node_ram_is_as_documented() {
        let n = core::mem::size_of::<SmbusNode<(), (), ()>>();
        assert!((5_400..5_800).contains(&n), "{n}");
    }

    #[test]
    fn m7_config_refuses_self_addressing() {
        let c = Config::new(a(0x20));
        assert_eq!(c.broadcast(), Address::GENERAL_CALL);
        assert_eq!(
            c.with_broadcast(a(0x20)),
            Err(ConfigError::BroadcastIsOwnAddress)
        );
        assert_eq!(c.with_default_peer(a(0x20)), Err(ConfigError::InvalidPeer));
        let c = c.with_broadcast(a(0x70)).unwrap();
        assert_eq!(c.with_default_peer(a(0x70)), Err(ConfigError::InvalidPeer));
        let c = c.with_default_peer(a(0x21)).unwrap();
        assert_eq!(c.with_broadcast(a(0x21)), Err(ConfigError::InvalidPeer));
        assert_eq!(c.default_peer(), Some(a(0x21)));
    }

    #[test]
    fn m7_targets_resolve_to_node_addresses_only() {
        let c = Config::new(a(0x20)).with_default_peer(a(0x21)).unwrap();
        let d = |t| destination::<(), ()>(&c, &t);
        assert_eq!(d(Target::Any), Ok(a(0x21)));
        assert_eq!(d(Target::Node(a(0x33).sender_id())), Ok(a(0x33)));
        let mac = SenderId::from_mac([2, 0, 0, 0, 0, 1]);
        assert_eq!(
            d(Target::Node(mac)),
            Err(SmbusError::UnsupportedTarget(mac))
        );
        let me = a(0x20).sender_id();
        assert_eq!(d(Target::Node(me)), Err(SmbusError::UnsupportedTarget(me)));
        assert_eq!(
            destination::<(), ()>(&Config::new(a(0x20)), &Target::Any),
            Err(SmbusError::NoTarget)
        );
    }

    #[test]
    fn m8_inbox_keeps_arrival_order_per_kind_and_drops_the_newest() {
        let mut q = Inbox::<3, 8>::new();
        assert!(q.push(head(0x10, Kind::Publish, 0), None, 0, b"p0"));
        assert!(q.push(head(0x11, Kind::Request, 0), None, 0, b"r0"));
        assert!(q.push(head(0x10, Kind::Publish, 1), Some(0), 0, b"p1"));
        assert!(!q.push(head(0x10, Kind::Publish, 2), Some(0), 0, b"p2"));
        assert!(!Inbox::<1, 2>::new().push(head(0x10, Kind::Publish, 0), None, 0, b"long"));
        let i = q.oldest(Kind::Publish).unwrap();
        assert_eq!(q.get(i).unwrap().body(), b"p0");
        q.remove(i);
        let i = q.oldest(Kind::Publish).unwrap();
        assert_eq!(q.get(i).unwrap().body(), b"p1");
        // The freed slot takes a newer message, still behind p1 in order.
        assert!(q.push(head(0x10, Kind::Publish, 3), Some(1), 0, b"p3"));
        assert_eq!(q.oldest(Kind::Publish), Some(i));
        assert_eq!(
            q.get(q.oldest(Kind::Request).unwrap()).unwrap().body(),
            b"r0"
        );
        assert_eq!(q.len(), 3);
    }

    #[test]
    fn m8_queued_requests_expire_on_their_deadline() {
        let mut q = Inbox::<2, 32>::new();
        let mut body = [0u8; 32];
        let req = Message::Request {
            deadline_ms: Some(50),
            key: Key::from_static("a"),
            body: b"",
        };
        let b = Body::of(&req);
        let n = b.copy_from(0, &mut body);
        q.push(head(0x10, Kind::Request, 0), None, 1_000, &body[..n]);
        q.push(head(0x10, Kind::Publish, 0), None, 1_000, b"\x01a");
        assert_eq!(q.expire_requests(1_050, 500), 0);
        assert_eq!(q.expire_requests(1_051, 500), 1);
        assert_eq!(q.len(), 1);
    }
}
