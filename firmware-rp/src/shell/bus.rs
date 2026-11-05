//! Message plumbing between the USB link task and the controller.
//!
//! * `inbound` — the link task's requests and publications, copied out of
//!   its frame buffer, for the controller.
//! * `reply` — the controller's answer to the link's one request in flight
//!   (a signal: the latest value wins, and the link waits for its id).
//! * `outbound` — device publications (log lines, payload status, payload
//!   publishes) for the link. Publishing never blocks: when the host is
//!   not reading, the oldest messages are dropped rather than stalling the
//!   controller.
//!
//! * `usb_configured` — whether the host has configured the USB device
//!   (set by the USB stack's handler; an input of the firmware's
//!   self-test).
//!
//! This is esp-base's bus (`firmware/src/shell/bus.rs`) cut down to one
//! port and no radio. The bus is created once at boot and shared as
//! `&'static Bus` with the tasks, as embassy tasks require.

use core::fmt::{self, Write as _};
use core::sync::atomic::{AtomicBool, Ordering};

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::pubsub::{PubSubChannel, Subscriber};
use embassy_sync::signal::Signal;
use heapless::{String, Vec};
use middleware::Key;
use middleware::key::MAX_KEY_LEN;
use middleware::loader::{CHUNK_MAX, PayloadStatus, keys};
#[cfg(feature = "wasm")]
use middleware::transport::RequestMeta;
use middleware::transport::{HandlerError, SenderId};
use middleware::wasm_abi::{MAX_LOG, MAX_MESSAGE};

/// Largest inbound body: a loader chunk with its offset, or a publication.
pub const MAX_BODY: usize = if 4 + CHUNK_MAX > MAX_MESSAGE { 4 + CHUNK_MAX } else { MAX_MESSAGE };

/// Largest reply body: a payload's reply to a request on a key it serves
/// (`esp_base::serve::REPLY_CAPACITY`, 512); `sys/info` is at most 85
/// bytes, `sys/trust/info` 41.
pub const MAX_REPLY: usize = esp_base::serve::REPLY_CAPACITY;

/// Outbound messages buffered before the oldest is dropped.
const OUTBOUND_DEPTH: usize = 8;

/// Inbound messages buffered for the controller.
const INBOUND_DEPTH: usize = 2;

/// Subscribers of outbound publications: the USB link.
const SUBSCRIBERS: usize = 1;

type Mutex = CriticalSectionRawMutex;

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

/// A request or publication from the host, owned.
pub struct Inbound {
    /// Request or publication.
    pub kind: Kind,
    /// Key (already validated by frame decoding).
    pub key: String<MAX_KEY_LEN>,
    /// Body.
    pub body: Vec<u8, MAX_BODY>,
    /// The link's peer, if it was told one: the sender a payload sees in a
    /// request's meta. The serial frames carry no deadline.
    #[cfg_attr(not(feature = "wasm"), allow(dead_code))]
    pub peer: Option<SenderId>,
}

impl Inbound {
    /// Copies a frame's key and body; `None` if the body is too large.
    pub fn new(kind: Kind, key: Key<'_>, body: &[u8], peer: Option<SenderId>) -> Option<Self> {
        let mut k = String::new();
        k.push_str(key.as_str()).ok()?;
        let body = Vec::from_slice(body).ok()?;
        Some(Inbound { kind, key: k, body, peer })
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
    /// Correlation id of the request.
    pub id: u8,
    /// Body or error.
    pub result: Result<Vec<u8, MAX_REPLY>, HandlerError>,
}

/// A device publication, owned.
#[derive(Clone)]
pub struct Outbound {
    /// Key.
    pub key: String<MAX_KEY_LEN>,
    /// Payload.
    pub payload: Vec<u8, MAX_MESSAGE>,
}

/// Why a publication was not queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooLarge;

/// Shared channels.
pub struct Bus {
    inbound: Channel<Mutex, Inbound, INBOUND_DEPTH>,
    reply: Signal<Mutex, Reply>,
    outbound: PubSubChannel<Mutex, Outbound, OUTBOUND_DEPTH, SUBSCRIBERS, 0>,
    usb_configured: AtomicBool,
}

/// The link's view of outbound publications.
pub type OutboundSubscriber = Subscriber<'static, Mutex, Outbound, OUTBOUND_DEPTH, SUBSCRIBERS, 0>;

impl Bus {
    /// Empty channels.
    pub const fn new() -> Self {
        Bus {
            inbound: Channel::new(),
            reply: Signal::new(),
            outbound: PubSubChannel::new(),
            usb_configured: AtomicBool::new(false),
        }
    }

    /// Records whether the host has configured the USB device.
    pub fn set_usb_configured(&self, configured: bool) {
        self.usb_configured.store(configured, Ordering::Relaxed);
    }

    /// Whether the host has configured the USB device.
    pub fn usb_configured(&self) -> bool {
        self.usb_configured.load(Ordering::Relaxed)
    }

    /// Queues a message for the controller, waiting while it is busy.
    pub async fn send_inbound(&self, msg: Inbound) {
        self.inbound.send(msg).await;
    }

    /// Waits for the next inbound message.
    pub async fn receive_inbound(&self) -> Inbound {
        self.inbound.receive().await
    }

    /// Delivers a reply to the link.
    pub fn reply(&self, reply: Reply) {
        self.reply.signal(reply);
    }

    /// Waits for the reply to the request with `id`; replies to other ids
    /// (none are expected) are discarded.
    pub async fn wait_reply(&self, id: u8) -> Reply {
        loop {
            let reply = self.reply.wait().await;
            if reply.id == id {
                return reply;
            }
        }
    }

    /// A subscriber for the link's outbound publications.
    pub fn subscribe(&'static self) -> Option<OutboundSubscriber> {
        self.outbound.subscriber().ok()
    }

    /// Publishes without blocking.
    pub fn publish(&self, key: Key<'_>, data: &[u8]) -> Result<(), TooLarge> {
        let mut k = String::new();
        k.push_str(key.as_str()).map_err(|_| TooLarge)?;
        let payload = Vec::from_slice(data).map_err(|_| TooLarge)?;
        self.outbound.immediate_publisher().publish_immediate(Outbound { key: k, payload });
        Ok(())
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
