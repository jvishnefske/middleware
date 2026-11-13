//! The client's transport session as a pure state machine: a `Copy` state
//! in, one input (an item of a received batch, a clock tick, a request to
//! send) in, the new state and **one** [`Reaction`] out. No I/O and no
//! clock of its own: every time is a caller-supplied millisecond count
//! (`now`), so the timing rules are unit tested without waiting.
//!
//! # Phases
//!
//! ```text
//!  Down ──connect──▶ InitSent ──InitAck──▶ OpenSent ──OpenAck──▶ Open
//!   ▲                   │                     │                   │
//!   └──── timeout, Close, malformed or unexpected input, lease ───┘
//!                                                     close() ──▶ Closed
//! ```
//!
//! * [`Session::connect`] (allowed in `Down` once the backoff has passed)
//!   gives the [`InitSyn`] to send: version [`VERSION`], role client, our
//!   [`Zid`], the default resolution and our receive batch size.
//! * An [`InitAck`] with our version gives the [`OpenSyn`] to send: our
//!   lease, our initial sequence number (FNV-1a of both ids, masked to the
//!   agreed resolution, so the same pair always starts at the same
//!   number, as Zenoh's own `compute_sn` intends) and the cookie echoed.
//!   The agreed batch size is the smaller of ours and the router's.
//! * An [`OpenAck`] opens the session ([`Reaction::Opened`]): the caller
//!   now sends its declarations.
//! * Anything else during the handshake, a different version, a zero
//!   lease, a malformed batch or no answer within
//!   [`Config::open_timeout_ms`] ends the attempt ([`Reaction::Lost`]).
//!
//! # Open
//!
//! * **Lease.** Any received item refreshes the router's lease; nothing
//!   for the router's lease (from its OpenAck) is
//!   [`DownReason::LeaseExpired`], with a Close to send.
//! * **Keep-alive.** When nothing was sent for
//!   `lease_ms / keep_alive_divisor` (Zenoh's default: a quarter of the
//!   lease we announced), [`Session::tick`] asks for a KeepAlive.
//! * **Sequence numbers.** [`Session::next_frame`] numbers outgoing frames
//!   per channel (reliable, best effort) from the initial number, wrapping
//!   at the agreed resolution's mask ([`Resolution::sn_mask`]). An incoming
//!   frame is accepted if its number follows the last one accepted on its
//!   channel by 1 up to half the range (Zenoh's `SeqNum::roll` rule, so a
//!   router that skipped numbers is still followed); a repeat or an older
//!   number is refused ([`Refusal::OutOfOrder`]) and so are the network
//!   messages of that frame. Network messages are delivered only from an
//!   accepted frame ([`Reaction::Deliver`]).
//! * Fragments ([`Refusal::Fragment`]), malformed batches and handshake
//!   messages are refused and the session stays open; a Close from the
//!   router ends it.
//!
//! # Reconnect
//!
//! Every loss counts an attempt and sets the earliest retry to
//! `now + min(backoff_min_ms << (attempts - 1), backoff_max_ms)`: bounded
//! exponential backoff. Opening resets the count. [`Session::close`] moves
//! to `Closed`, which never reconnects until [`Session::restart`].

use super::codec::network::NetworkMessage;
use super::codec::transport::{
    Close, FrameHeader, InitAck, InitSyn, Item, OpenAck, OpenSyn, Resolution, TransportMessage,
    reason,
};
use super::codec::{CodecError, VERSION, WhatAmI, Zid};

/// Session parameters. Construct with [`Config::new`] and adjust fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Our Zenoh id.
    pub zid: Zid,
    /// The lease we announce, in milliseconds (Zenoh's default 10 s).
    pub lease_ms: u32,
    /// Keep-alives go out every `lease_ms / keep_alive_divisor` of silence
    /// (Zenoh's default 4).
    pub keep_alive_divisor: u32,
    /// How long the handshake may take.
    pub open_timeout_ms: u32,
    /// The largest batch we accept (our receive buffer).
    pub batch_size: u16,
    /// First reconnect delay.
    pub backoff_min_ms: u32,
    /// Largest reconnect delay.
    pub backoff_max_ms: u32,
}

impl Config {
    /// Zenoh's defaults for `zid` with a 2 KiB batch, reconnecting after
    /// 100 ms, doubling up to 10 s.
    pub const fn new(zid: Zid) -> Config {
        Config {
            zid,
            lease_ms: 10_000,
            keep_alive_divisor: 4,
            open_timeout_ms: 10_000,
            batch_size: 2048,
            backoff_min_ms: 100,
            backoff_max_ms: 10_000,
        }
    }

    /// Milliseconds of silence after which a KeepAlive is due.
    pub const fn keep_alive_ms(&self) -> u64 {
        let div = if self.keep_alive_divisor == 0 {
            1
        } else {
            self.keep_alive_divisor
        };
        let ms = self.lease_ms / div;
        if ms == 0 { 1 } else { ms as u64 }
    }

    /// The reconnect delay after `attempts` failed attempts (at least 1).
    pub const fn backoff_ms(&self, attempts: u8) -> u64 {
        let shift = if attempts == 0 { 0 } else { attempts - 1 };
        let shift = if shift > 20 { 20 } else { shift };
        let d = (self.backoff_min_ms as u64) << shift;
        if d > self.backoff_max_ms as u64 {
            self.backoff_max_ms as u64
        } else {
            d
        }
    }
}

/// What the router agreed to, and the per-channel sequence numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Link {
    /// The router's (or peer's) id.
    pub peer: Zid,
    /// Its role.
    pub peer_whatami: WhatAmI,
    /// Agreed resolutions.
    pub resolution: Resolution,
    /// Largest batch we may send.
    pub batch_size: u16,
    /// The router's lease.
    pub peer_lease_ms: u64,
    tx_reliable: u32,
    tx_best_effort: u32,
    rx_reliable: u32,
    rx_best_effort: u32,
    last_rx: u64,
    last_tx: u64,
    frame_ok: Option<bool>,
    next_rid: u32,
}

/// Where the session stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// No link; may connect from `retry_at`.
    Down {
        /// Earliest time to connect.
        retry_at: u64,
    },
    /// InitSyn sent at `since`.
    InitSent {
        /// When it was sent.
        since: u64,
    },
    /// OpenSyn sent at `since`.
    OpenSent {
        /// When the handshake started.
        since: u64,
        /// What the InitAck agreed.
        link: Link,
    },
    /// Open.
    Open(Link),
    /// Closed by us; no reconnect until [`Session::restart`].
    Closed,
}

/// Why the session went down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownReason {
    /// The router sent Close with this reason.
    Remote(u8),
    /// Nothing heard for the router's lease.
    LeaseExpired,
    /// The handshake took longer than [`Config::open_timeout_ms`].
    HandshakeTimeout,
    /// The router answered with something unexpected, another version or
    /// unusable parameters.
    HandshakeRefused,
    /// A malformed batch during the handshake.
    Malformed(CodecError),
    /// The link failed (reported by the caller).
    LinkError,
}

/// Why an input was refused (the session state is otherwise unchanged).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Not valid in this phase.
    Unexpected,
    /// A frame number that does not follow, or a message of such a frame.
    OutOfOrder,
    /// A fragment: messages larger than a batch are not reassembled.
    Fragment,
    /// The batch did not decode.
    Malformed(CodecError),
}

/// The one thing the caller does after a transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reaction<'a> {
    /// Nothing.
    Nothing,
    /// Send this transport message.
    Send(TransportMessage<'a>),
    /// The session is open: send the declarations.
    Opened,
    /// A network message from an accepted frame.
    Deliver(NetworkMessage<'a>),
    /// The input was refused (count it); the session goes on.
    Refused(Refusal),
    /// The session went down; send `close` first if given.
    Lost {
        /// Why.
        reason: DownReason,
        /// A Close to send before dropping the link.
        close: Option<Close>,
    },
}

/// Why a request to the session was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionError {
    /// `connect` before the backoff has passed.
    Backoff {
        /// Earliest time to connect.
        until: u64,
    },
    /// `connect` while connecting, open or closed.
    NotDown,
    /// Sending needs an open session.
    NotOpen,
}

/// The session state. `Copy`: transitions take it by value and return the
/// next one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Session {
    config: Config,
    phase: Phase,
    attempts: u8,
}

/// Does `sn` follow `last` (mod `mask + 1`) by 1 up to half the range?
const fn follows(last: u32, sn: u32, mask: u32) -> bool {
    if sn & !mask != 0 {
        return false;
    }
    let gap = sn.wrapping_sub(last) & mask;
    gap != 0 && (gap & !(mask >> 1)) == 0
}

fn initial_sn(ours: Zid, theirs: Zid, mask: u32) -> u32 {
    // FNV-1a over our id then theirs (the bytes on the wire).
    let mut hash: u32 = 0x811c_9dc5;
    for &b in ours.as_wire().iter().chain(theirs.as_wire()) {
        hash ^= u32::from(b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash & mask
}

impl Session {
    /// A session that may connect at once.
    pub const fn new(config: Config) -> Session {
        Session {
            config,
            phase: Phase::Down { retry_at: 0 },
            attempts: 0,
        }
    }

    /// The configuration.
    pub const fn config(&self) -> &Config {
        &self.config
    }

    /// The phase.
    pub const fn phase(&self) -> Phase {
        self.phase
    }

    /// The open link, if any.
    pub const fn link(&self) -> Option<&Link> {
        match &self.phase {
            Phase::Open(l) => Some(l),
            _ => None,
        }
    }

    /// True when open.
    pub const fn is_open(&self) -> bool {
        matches!(self.phase, Phase::Open(_))
    }

    /// Failed attempts since the last open.
    pub const fn attempts(&self) -> u8 {
        self.attempts
    }

    fn down(
        self,
        now: u64,
        reason: DownReason,
        close: Option<Close>,
    ) -> (Session, Reaction<'static>) {
        let attempts = self.attempts.saturating_add(1);
        let retry_at = now.saturating_add(self.config.backoff_ms(attempts));
        (
            Session {
                phase: Phase::Down { retry_at },
                attempts,
                ..self
            },
            Reaction::Lost { reason, close },
        )
    }

    /// Starts a handshake: the InitSyn to send.
    pub fn connect(self, now: u64) -> Result<(Session, InitSyn), SessionError> {
        match self.phase {
            Phase::Down { retry_at } if now >= retry_at => {
                let syn = InitSyn {
                    version: VERSION,
                    whatami: WhatAmI::Client,
                    zid: self.config.zid,
                    resolution: Resolution::DEFAULT,
                    batch_size: self.config.batch_size,
                    qos: false,
                    patch: 0,
                };
                Ok((
                    Session {
                        phase: Phase::InitSent { since: now },
                        ..self
                    },
                    syn,
                ))
            }
            Phase::Down { retry_at } => Err(SessionError::Backoff { until: retry_at }),
            _ => Err(SessionError::NotDown),
        }
    }

    /// The link failed (I/O error, end of stream) or, when down, a
    /// connect attempt failed: down, one more attempt counted for the
    /// backoff. No effect when closed.
    pub fn link_lost(self, now: u64) -> Session {
        match self.phase {
            Phase::Closed => self,
            _ => self.down(now, DownReason::LinkError, None).0,
        }
    }

    /// Closes the session: the Close to send if a link is up. No
    /// reconnect until [`Session::restart`].
    pub fn close(self) -> (Session, Option<Close>) {
        let close = match self.phase {
            Phase::Down { .. } | Phase::Closed => None,
            _ => Some(Close {
                reason: reason::GENERIC,
                session: true,
            }),
        };
        (
            Session {
                phase: Phase::Closed,
                ..self
            },
            close,
        )
    }

    /// Leaves `Closed`: may connect at once, attempts reset.
    pub fn restart(self) -> Session {
        match self.phase {
            Phase::Closed => Session {
                phase: Phase::Down { retry_at: 0 },
                attempts: 0,
                ..self
            },
            _ => self,
        }
    }

    /// A batch failed to decode at `now`.
    pub fn on_malformed(self, now: u64, err: CodecError) -> (Session, Reaction<'static>) {
        match self.phase {
            Phase::InitSent { .. } | Phase::OpenSent { .. } => self.down(
                now,
                DownReason::Malformed(err),
                Some(Close {
                    reason: reason::INVALID,
                    session: true,
                }),
            ),
            Phase::Open(link) => (
                Session {
                    phase: Phase::Open(Link {
                        last_rx: now,
                        frame_ok: None,
                        ..link
                    }),
                    ..self
                },
                Reaction::Refused(Refusal::Malformed(err)),
            ),
            _ => (self, Reaction::Refused(Refusal::Malformed(err))),
        }
    }

    /// Handles one item of a received batch.
    pub fn on_item<'a>(self, now: u64, item: Item<'a>) -> (Session, Reaction<'a>) {
        match self.phase {
            Phase::InitSent { since } => self.on_init_sent(now, since, item),
            Phase::OpenSent { link, .. } => self.on_open_sent(now, link, item),
            Phase::Open(link) => self.on_open(now, link, item),
            Phase::Down { .. } | Phase::Closed => (self, Reaction::Refused(Refusal::Unexpected)),
        }
    }

    fn refused_handshake(self, now: u64) -> (Session, Reaction<'static>) {
        self.down(
            now,
            DownReason::HandshakeRefused,
            Some(Close {
                reason: reason::INVALID,
                session: true,
            }),
        )
    }

    fn on_init_sent<'a>(self, now: u64, since: u64, item: Item<'a>) -> (Session, Reaction<'a>) {
        match item {
            Item::Transport(TransportMessage::InitAck(ack)) => self.on_init_ack(now, since, ack),
            Item::Transport(TransportMessage::Close(c)) => {
                self.down(now, DownReason::Remote(c.reason), None)
            }
            _ => self.refused_handshake(now),
        }
    }

    fn on_init_ack<'a>(self, now: u64, since: u64, ack: InitAck<'a>) -> (Session, Reaction<'a>) {
        let usable = ack.version == VERSION
            && ack.whatami != WhatAmI::Client
            && ack.batch_size != 0
            && ack.cookie.len() <= usize::from(u16::MAX);
        if !usable {
            return self.refused_handshake(now);
        }
        let mask = ack.resolution.sn_mask();
        let tx = initial_sn(self.config.zid, ack.zid, mask);
        let link = Link {
            peer: ack.zid,
            peer_whatami: ack.whatami,
            resolution: ack.resolution,
            batch_size: ack.batch_size.min(self.config.batch_size),
            peer_lease_ms: 0,
            tx_reliable: tx,
            tx_best_effort: tx,
            rx_reliable: 0,
            rx_best_effort: 0,
            last_rx: now,
            last_tx: now,
            frame_ok: None,
            next_rid: 0,
        };
        let syn = OpenSyn {
            lease_ms: u64::from(self.config.lease_ms),
            initial_sn: tx,
            cookie: ack.cookie,
            qos: false,
        };
        (
            Session {
                phase: Phase::OpenSent { since, link },
                ..self
            },
            Reaction::Send(TransportMessage::OpenSyn(syn)),
        )
    }

    fn on_open_sent<'a>(self, now: u64, link: Link, item: Item<'a>) -> (Session, Reaction<'a>) {
        match item {
            Item::Transport(TransportMessage::OpenAck(ack)) => self.on_open_ack(now, link, ack),
            Item::Transport(TransportMessage::Close(c)) => {
                self.down(now, DownReason::Remote(c.reason), None)
            }
            _ => self.refused_handshake(now),
        }
    }

    fn on_open_ack(self, now: u64, link: Link, ack: OpenAck) -> (Session, Reaction<'static>) {
        let mask = link.resolution.sn_mask();
        if ack.lease_ms == 0 || ack.initial_sn & !mask != 0 {
            return self.refused_handshake(now);
        }
        // As if the frame before the initial one had been received.
        let rx = ack.initial_sn.wrapping_sub(1) & mask;
        let link = Link {
            peer_lease_ms: ack.lease_ms,
            rx_reliable: rx,
            rx_best_effort: rx,
            last_rx: now,
            ..link
        };
        (
            Session {
                phase: Phase::Open(link),
                attempts: 0,
                ..self
            },
            Reaction::Opened,
        )
    }

    fn on_open<'a>(self, now: u64, link: Link, item: Item<'a>) -> (Session, Reaction<'a>) {
        let link = Link {
            last_rx: now,
            ..link
        };
        let with = |link: Link| Session {
            phase: Phase::Open(link),
            ..self
        };
        match item {
            Item::Transport(TransportMessage::KeepAlive) => (with(link), Reaction::Nothing),
            Item::Transport(TransportMessage::Close(c)) => {
                self.down(now, DownReason::Remote(c.reason), None)
            }
            Item::Transport(TransportMessage::Frame(f)) => {
                let mask = link.resolution.sn_mask();
                let last = if f.reliable {
                    link.rx_reliable
                } else {
                    link.rx_best_effort
                };
                if follows(last, f.sn, mask) {
                    let link = if f.reliable {
                        Link {
                            rx_reliable: f.sn,
                            frame_ok: Some(true),
                            ..link
                        }
                    } else {
                        Link {
                            rx_best_effort: f.sn,
                            frame_ok: Some(true),
                            ..link
                        }
                    };
                    (with(link), Reaction::Nothing)
                } else {
                    (
                        with(Link {
                            frame_ok: Some(false),
                            ..link
                        }),
                        Reaction::Refused(Refusal::OutOfOrder),
                    )
                }
            }
            Item::Network(msg) => match link.frame_ok {
                Some(true) => (with(link), Reaction::Deliver(msg)),
                Some(false) => (with(link), Reaction::Refused(Refusal::OutOfOrder)),
                None => (with(link), Reaction::Refused(Refusal::Unexpected)),
            },
            Item::Fragment(_) => (
                with(Link {
                    frame_ok: None,
                    ..link
                }),
                Reaction::Refused(Refusal::Fragment),
            ),
            Item::Oam => (
                with(Link {
                    frame_ok: None,
                    ..link
                }),
                Reaction::Nothing,
            ),
            Item::Transport(_) => (
                with(Link {
                    frame_ok: None,
                    ..link
                }),
                Reaction::Refused(Refusal::Unexpected),
            ),
        }
    }

    /// Applies the clock: handshake timeout, lease expiry, keep-alive.
    pub fn tick(self, now: u64) -> (Session, Reaction<'static>) {
        let timeout = u64::from(self.config.open_timeout_ms);
        match self.phase {
            Phase::InitSent { since } | Phase::OpenSent { since, .. }
                if now.saturating_sub(since) >= timeout =>
            {
                self.down(now, DownReason::HandshakeTimeout, None)
            }
            Phase::Open(link) if now.saturating_sub(link.last_rx) >= link.peer_lease_ms => self
                .down(
                    now,
                    DownReason::LeaseExpired,
                    Some(Close {
                        reason: reason::EXPIRED,
                        session: true,
                    }),
                ),
            Phase::Open(link)
                if now.saturating_sub(link.last_tx) >= self.config.keep_alive_ms() =>
            {
                (
                    Session {
                        phase: Phase::Open(Link {
                            last_tx: now,
                            ..link
                        }),
                        ..self
                    },
                    Reaction::Send(TransportMessage::KeepAlive),
                )
            }
            _ => (self, Reaction::Nothing),
        }
    }

    /// The next time [`Session::tick`] (or, when down, [`Session::connect`])
    /// has something to do; `None` when closed.
    pub fn deadline(&self) -> Option<u64> {
        let timeout = u64::from(self.config.open_timeout_ms);
        match self.phase {
            Phase::Down { retry_at } => Some(retry_at),
            Phase::InitSent { since } | Phase::OpenSent { since, .. } => {
                Some(since.saturating_add(timeout))
            }
            Phase::Open(link) => {
                let lease = link.last_rx.saturating_add(link.peer_lease_ms);
                let ka = link.last_tx.saturating_add(self.config.keep_alive_ms());
                Some(lease.min(ka))
            }
            Phase::Closed => None,
        }
    }

    /// Numbers the next outgoing frame on a channel and records the send.
    pub fn next_frame(
        self,
        now: u64,
        reliable: bool,
    ) -> Result<(Session, FrameHeader), SessionError> {
        let Phase::Open(link) = self.phase else {
            return Err(SessionError::NotOpen);
        };
        let mask = link.resolution.sn_mask();
        let (sn, link) = if reliable {
            let sn = link.tx_reliable;
            (
                sn,
                Link {
                    tx_reliable: sn.wrapping_add(1) & mask,
                    ..link
                },
            )
        } else {
            let sn = link.tx_best_effort;
            (
                sn,
                Link {
                    tx_best_effort: sn.wrapping_add(1) & mask,
                    ..link
                },
            )
        };
        let header = FrameHeader {
            reliable,
            sn,
            qos: FrameHeader::DEFAULT_QOS,
        };
        Ok((
            Session {
                phase: Phase::Open(Link {
                    last_tx: now,
                    ..link
                }),
                ..self
            },
            header,
        ))
    }

    /// A fresh request id (wrapping at the agreed resolution).
    pub fn next_request_id(self) -> Result<(Session, u32), SessionError> {
        let Phase::Open(link) = self.phase else {
            return Err(SessionError::NotOpen);
        };
        let rid = link.next_rid;
        let link = Link {
            next_rid: rid.wrapping_add(1) & link.resolution.rid_mask(),
            ..link
        };
        Ok((
            Session {
                phase: Phase::Open(link),
                ..self
            },
            rid,
        ))
    }
}

#[cfg(test)]
mod tests;
