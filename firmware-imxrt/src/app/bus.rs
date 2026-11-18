//! Message plumbing between the link tasks and the controller (FL1).
//!
//! The board has up to [`MAX_LINKS`] links ([`LinkId`]): LPUART1 and USB
//! CDC (the serial protocol) and the Zenoh session on Ethernet.
//!
//! * `inbound` — every link's requests and publications, copied out of its
//!   frame buffer and tagged with the link, for the controller (one queue:
//!   the controller serves them in arrival order).
//! * `replies` — per link, the controller's answer to the one request in
//!   flight on it (a signal: the latest value wins, and the link waits for
//!   its id). A reply goes back on the link the request came from only.
//! * outbound publications — the routing core [`Links`]
//!   (`imxrt_base::links`) behind a mutex: [`Bus::publish`] puts a copy on
//!   the queue of exactly the links whose filter selects the key, a full
//!   queue drops that link's copy (counted) and never waits, and each link
//!   task pops its own queue ([`Bus::next_outbound`]) when its `ready`
//!   signal fires. A serial link's filter is set by its host with
//!   `sys/link/sub` / `sys/link/unsub` ([`Bus::link_request`]). The Zenoh
//!   link takes every non-`sys` publication: in client mode the router
//!   does not declare its subscribers to the board, so it is the router
//!   that drops what no peer wants.
//! * `sent` — per link, the word that the last reply has left (written and
//!   flushed), which the controller waits for before a reset.
//! * `subscriptions` — the running payload's subscription keys
//!   ([`Bus::set_subscriptions`], kept current by the controller; empty
//!   while none runs) and a signal when they change, so the Zenoh link
//!   declares a subscriber for each: the router forwards a publication to
//!   the board only for a key it declared.
//!
//! The bus is created once at boot and shared as `&'static Bus` with the
//! tasks, as embassy tasks require.

use core::cell::RefCell;
use core::fmt::{self, Write as _};

use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use heapless::{String, Vec};
use imxrt_base::links::{LinkId, LinkKind, Links, MAX_LINKS, Publication};
use imxrt_base::zenoh_mgmt::{SubSet, subscription_set};
use middleware::Key;
use middleware::key::MAX_KEY_LEN;
use middleware::loader::{CHUNK_MAX, PayloadStatus, is_system_key, keys};
#[cfg(feature = "wasm")]
use middleware::transport::RequestMeta;
use middleware::transport::{HandlerError, SenderId};
use middleware::wasm_abi::{MAX_LOG, MAX_MESSAGE};

/// Largest inbound body: a loader chunk with its offset, or a publication.
pub const MAX_BODY: usize = if 4 + CHUNK_MAX > MAX_MESSAGE {
    4 + CHUNK_MAX
} else {
    MAX_MESSAGE
};

/// Largest reply body: a payload's reply to a request on a key it serves
/// (`esp_base::serve::REPLY_CAPACITY`, 512).
pub const MAX_REPLY: usize = esp_base::serve::REPLY_CAPACITY;

/// Publications queued per link before that link's copies are dropped:
/// 320 ms of a microphone PCM route (100 batches a second, SC8), so a
/// link that stalls briefly (a TCP retransmission to a Wi-Fi host) loses
/// nothing. In SDRAM with the bus (`BUS_SDRAM`), about 17 KiB per link.
const OUTBOUND_DEPTH: usize = 32;

/// Key expressions a serial host may hold (`sys/link/sub`).
const LINK_EXPRS: usize = 8;

/// Inbound messages buffered for the controller.
const INBOUND_DEPTH: usize = 2;

type Mutex = CriticalSectionRawMutex;

/// The routing core with this firmware's capacities (replies are not
/// queued in it: each link has one request in flight, answered through
/// its reply signal).
type Routing = Links<OUTBOUND_DEPTH, 1, LINK_EXPRS, MAX_MESSAGE>;

/// An outbound publication, owned.
pub type Outbound = Publication<MAX_MESSAGE>;

/// What an inbound message is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A request expecting a reply with this correlation id.
    Request {
        /// Correlation id.
        id: u8,
    },
    /// A publication from the host.
    Publish,
}

/// A request or publication from a host, owned.
pub struct Inbound {
    /// The link it came on (the reply goes back on it).
    pub link: LinkId,
    /// Request or publication.
    pub kind: Kind,
    /// Key (already validated by frame decoding).
    pub key: String<MAX_KEY_LEN>,
    /// Body.
    pub body: Vec<u8, MAX_BODY>,
    /// The link's peer, if it was told one: the sender a payload sees in a
    /// request's meta.
    #[cfg_attr(not(feature = "wasm"), allow(dead_code))]
    pub peer: Option<SenderId>,
}

impl Inbound {
    /// Copies a frame's key and body; `None` if the body is too large.
    pub fn new(
        link: LinkId,
        kind: Kind,
        key: Key<'_>,
        body: &[u8],
        peer: Option<SenderId>,
    ) -> Option<Self> {
        let mut k = String::new();
        k.push_str(key.as_str()).ok()?;
        let body = Vec::from_slice(body).ok()?;
        Some(Inbound {
            link,
            kind,
            key: k,
            body,
            peer,
        })
    }

    /// What a payload may know of the request: the link's peer as sender.
    #[cfg(feature = "wasm")]
    pub fn meta(&self) -> RequestMeta<'static> {
        match self.peer {
            Some(p) => RequestMeta::NONE.with_sender(p),
            None => RequestMeta::NONE,
        }
    }
}

/// The controller's answer to a request.
pub struct Reply {
    /// The link the request came on.
    pub link: LinkId,
    /// Correlation id of the request.
    pub id: u8,
    /// Body or error.
    pub result: Result<Vec<u8, MAX_REPLY>, HandlerError>,
}

/// Why a publication was not queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooLarge;

/// Shared channels.
pub struct Bus {
    inbound: Channel<Mutex, Inbound, INBOUND_DEPTH>,
    replies: [Signal<Mutex, Reply>; MAX_LINKS],
    sent: [Signal<Mutex, u8>; MAX_LINKS],
    ready: [Signal<Mutex, ()>; MAX_LINKS],
    routing: BlockingMutex<Mutex, RefCell<Routing>>,
    subscriptions: BlockingMutex<Mutex, RefCell<SubSet>>,
    subscriptions_changed: Signal<Mutex, ()>,
}

/// The slot of `link` in a per-link array (every [`LinkId`] is below
/// [`MAX_LINKS`]).
fn slot<T>(a: &[T; MAX_LINKS], link: LinkId) -> &T {
    let [first, ..] = a;
    a.get(link.index()).unwrap_or(first)
}

impl Bus {
    /// Empty channels; the EVKB's links (UART and USB serial, Zenoh).
    pub const fn new() -> Self {
        Bus {
            inbound: Channel::new(),
            replies: [const { Signal::new() }; MAX_LINKS],
            sent: [const { Signal::new() }; MAX_LINKS],
            ready: [const { Signal::new() }; MAX_LINKS],
            routing: BlockingMutex::new(RefCell::new(Links::evkb())),
            subscriptions: BlockingMutex::new(RefCell::new(SubSet::new())),
            subscriptions_changed: Signal::new(),
        }
    }

    /// Records the running payload's subscription keys (none when it has
    /// stopped); signals the Zenoh link when they changed.
    pub fn set_subscriptions<'a>(&self, keys: impl Iterator<Item = &'a str>) {
        let next = subscription_set(keys);
        let changed = self.subscriptions.lock(|s| {
            let mut s = s.borrow_mut();
            let changed = *s != next;
            if changed {
                *s = next;
            }
            changed
        });
        if changed {
            self.subscriptions_changed.signal(());
        }
    }

    /// The payload's subscription keys.
    pub fn subscriptions(&self) -> SubSet {
        self.subscriptions.lock(|s| s.borrow().clone())
    }

    /// Waits until the subscription keys change (cancel-safe: a change
    /// while nobody waits is kept).
    pub async fn subscriptions_changed(&self) {
        self.subscriptions_changed.wait().await;
    }

    /// Queues a message for the controller, waiting while it is busy.
    pub async fn send_inbound(&self, msg: Inbound) {
        self.inbound.send(msg).await;
    }

    /// Waits for the next inbound message.
    pub async fn receive_inbound(&self) -> Inbound {
        self.inbound.receive().await
    }

    /// Delivers a reply to the link it is addressed to.
    pub fn reply(&self, reply: Reply) {
        slot(&self.replies, reply.link).signal(reply);
    }

    /// Waits for the reply to the request with `id` on `link`; replies to
    /// other ids (none are expected) are discarded.
    pub async fn wait_reply(&self, link: LinkId, id: u8) -> Reply {
        loop {
            let reply = slot(&self.replies, link).wait().await;
            if reply.id == id {
                return reply;
            }
        }
    }

    /// The link has sent (and flushed) the response to request `id`.
    pub fn reply_sent(&self, link: LinkId, id: u8) {
        slot(&self.sent, link).signal(id);
    }

    /// Waits until the response to request `id` on `link` has been sent.
    pub async fn wait_sent(&self, link: LinkId, id: u8) {
        while slot(&self.sent, link).wait().await != id {}
    }

    /// Publishes without blocking, to every link whose filter selects
    /// `key` (`sys/...` keys never go to the Zenoh link: the board's system
    /// keys are served there as queryables, `crate::zenoh_mgmt`), or, for
    /// a node-internal channel key, to this board's own payload.
    pub fn publish(&self, key: Key<'_>, data: &[u8]) -> Result<(), TooLarge> {
        // A local source bound to a payload channel (`ch/...`, BD4) goes
        // back to this board's payload only, never on a link; dropped
        // when the inbound queue is full (the route counts it).
        if middleware::loader::binding::is_internal_key(key) {
            let msg = Inbound::new(LinkId::UART, Kind::Publish, key, data, None).ok_or(TooLarge)?;
            return self.inbound.try_send(msg).map_err(|_| TooLarge);
        }
        let fanout = self.routing.lock(|r| {
            r.borrow_mut()
                .publish(key, data, |_, k| !is_system_key(k))
                .map_err(|_| TooLarge)
        })?;
        for (id, ready) in LinkId::ALL.into_iter().zip(self.ready.iter()) {
            if fanout.queued.contains(id) {
                ready.signal(());
            }
        }
        Ok(())
    }

    /// Waits for the next publication queued for `link`.
    pub async fn next_outbound(&self, link: LinkId) -> Outbound {
        loop {
            if let Some(p) = self.routing.lock(|r| r.borrow_mut().pop(link)) {
                return p;
            }
            slot(&self.ready, link).wait().await;
        }
    }

    /// `sys/link/sub` and `sys/link/unsub` on `link`; `None` for any other
    /// key (the request goes to the service).
    pub fn link_request(
        &self,
        link: LinkId,
        key: Key<'_>,
        body: &[u8],
    ) -> Option<Result<(), HandlerError>> {
        self.routing
            .lock(|r| r.borrow_mut().link_request(link, key, body))
    }

    /// Whether `link` is a link of this board.
    #[allow(dead_code)]
    pub fn has(&self, link: LinkId) -> bool {
        self.routing.lock(|r| r.borrow().kind(link)) != LinkKind::Absent
    }

    /// Publishes a log line on `sys/log`, truncated to `MAX_LOG` bytes.
    pub fn log(&self, args: fmt::Arguments<'_>) {
        let mut line = Truncating(String::<MAX_LOG>::new());
        // Truncating never fails.
        let _ = line.write_fmt(args);
        let _ = self.publish(keys::LOG, line.0.as_bytes());
    }

    /// Publishes the payload status on `sys/payload/status`.
    pub fn status(&self, status: PayloadStatus) {
        let _ = self.publish(keys::STATUS, &[status as u8]);
    }
}

/// A string writer that drops what does not fit, on a char boundary.
struct Truncating<const N: usize>(String<N>);

impl<const N: usize> fmt::Write for Truncating<N> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for c in s.chars() {
            if self.0.push(c).is_err() {
                break;
            }
        }
        Ok(())
    }
}
