//! Message plumbing between the serial link tasks and the controller.
//!
//! * `inbound` — one channel from every link task to the controller,
//!   carrying requests and publications copied out of the link's frame
//!   buffer.
//! * `replies` — one signal per port; the controller answers a request on
//!   the port it came from. A link task has at most one request in flight,
//!   so a signal (latest value wins) is enough.
//! * `sent` — one signal per port, raised by the link task with the
//!   correlation id of each response once it has been written and flushed
//!   (or abandoned after its send timeout). The controller waits on it only
//!   for `sys/reboot`, whose reply must leave the device before the reboot.
//! * `outbound` — a publish/subscribe channel with one subscriber per port,
//!   so device publications (log lines, payload status, payload publishes)
//!   reach every port. Publishing never blocks: a port that cannot keep up
//!   (for example USB with no host reading) loses its oldest messages rather
//!   than stalling the controller.
//!
//! The port set is chosen by the shell: a type implementing [`Port`] (a
//! closed set of ports, each with a distinct index below [`Port::COUNT`])
//! and the const generic `N`, which must equal `P::COUNT` (checked at
//! compile time by [`Bus::new`]). The ESP shell uses two ports (UART0 and
//! USB-Serial-JTAG); a board with one port can use [`Single`].
//!
//! The bus is created once at boot and shared as `&'static Bus` with the
//! tasks; embassy tasks require `'static` arguments.

use core::fmt::{self, Debug, Write as _};

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::pubsub::{PubSubChannel, Subscriber};
use embassy_sync::signal::Signal;
use heapless::{String, Vec};
use middleware::Key;
use middleware::key::MAX_KEY_LEN;
use middleware::loader::{CHUNK_MAX, PayloadStatus, keys};
use middleware::transport::HandlerError;
use middleware::wasm_abi::{MAX_LOG, MAX_MESSAGE};

/// Largest inbound body: a loader chunk with its offset, or a publication.
pub const MAX_BODY: usize = if 4 + CHUNK_MAX > MAX_MESSAGE { 4 + CHUNK_MAX } else { MAX_MESSAGE };

/// Largest reply body (`sys/info` is at most 73 bytes).
pub const MAX_REPLY: usize = 128;

/// Outbound messages buffered per port before the oldest is dropped.
const OUTBOUND_DEPTH: usize = 8;

/// Inbound messages buffered for the controller.
const INBOUND_DEPTH: usize = 2;

type Mutex = CriticalSectionRawMutex;

/// The set of serial ports a shell runs the protocol on.
///
/// Implemented by a small `Copy` type (usually a fieldless enum) whose
/// values map one-to-one onto the indices `0..COUNT`.
pub trait Port: Copy + Debug + 'static {
    /// Number of ports in the set.
    const COUNT: usize;
    /// This port's index, below [`Port::COUNT`].
    fn index(self) -> usize;
}

/// The port set of a board with exactly one protocol port (for example
/// the RP2040's native USB CDC-ACM).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Single;

impl Port for Single {
    const COUNT: usize = 1;
    fn index(self) -> usize {
        0
    }
}

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

/// A request or publication from a port, owned.
pub struct Inbound<P> {
    /// Port it arrived on.
    pub port: P,
    /// Request or publication.
    pub kind: Kind,
    /// Key (already validated by frame decoding).
    pub key: String<MAX_KEY_LEN>,
    /// Body.
    pub body: Vec<u8, MAX_BODY>,
}

impl<P> Inbound<P> {
    /// Copies a frame's key and body; `None` if the body is too large.
    pub fn new(port: P, kind: Kind, key: Key<'_>, body: &[u8]) -> Option<Self> {
        let mut k = String::new();
        k.push_str(key.as_str()).ok()?;
        let body = Vec::from_slice(body).ok()?;
        Some(Inbound { port, kind, key: k, body })
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

/// Shared channels for the port set `P` of `N` ports.
pub struct Bus<P, const N: usize> {
    inbound: Channel<Mutex, Inbound<P>, INBOUND_DEPTH>,
    replies: [Signal<Mutex, Reply>; N],
    sent: [Signal<Mutex, u8>; N],
    outbound: PubSubChannel<Mutex, Outbound, OUTBOUND_DEPTH, N, 0>,
}

/// A port's view of outbound publications.
pub type OutboundSubscriber<const N: usize> =
    Subscriber<'static, Mutex, Outbound, OUTBOUND_DEPTH, N, 0>;

impl<P: Port, const N: usize> Default for Bus<P, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<P: Port, const N: usize> Bus<P, N> {
    /// Empty channels. Fails to compile unless `N == P::COUNT`.
    pub const fn new() -> Self {
        const { assert!(N == P::COUNT, "Bus<P, N> needs N == P::COUNT") };
        Bus {
            inbound: Channel::new(),
            replies: [const { Signal::new() }; N],
            sent: [const { Signal::new() }; N],
            outbound: PubSubChannel::new(),
        }
    }

    /// Queues a message for the controller, waiting while it is busy.
    pub async fn send_inbound(&self, msg: Inbound<P>) {
        self.inbound.send(msg).await;
    }

    /// Waits for the next inbound message.
    pub async fn receive_inbound(&self) -> Inbound<P> {
        self.inbound.receive().await
    }

    /// Delivers a reply to the port that sent the request.
    pub fn reply(&self, port: P, reply: Reply) {
        if let Some(signal) = self.replies.get(port.index()) {
            signal.signal(reply);
        }
    }

    /// Waits for the reply to this port's request with `id`; replies to
    /// other ids (none are expected) are discarded.
    pub async fn wait_reply(&self, port: P, id: u8) -> Reply {
        let Some(signal) = self.replies.get(port.index()) else {
            return Reply { id, result: Err(HandlerError::Internal) };
        };
        loop {
            let reply = signal.wait().await;
            if reply.id == id {
                return reply;
            }
        }
    }

    /// Forgets any earlier "response sent" notice on `port`; call before
    /// [`Bus::reply`] when the controller will wait in [`Bus::wait_sent`].
    pub fn clear_sent(&self, port: P) {
        if let Some(signal) = self.sent.get(port.index()) {
            signal.reset();
        }
    }

    /// Called by the link task once the response to `id` has been written
    /// and flushed, or abandoned after its send timeout.
    pub fn mark_sent(&self, port: P, id: u8) {
        if let Some(signal) = self.sent.get(port.index()) {
            signal.signal(id);
        }
    }

    /// Waits until the link task on `port` reports the response to `id`
    /// sent. Not bounded by itself: the caller wraps it in a timeout.
    pub async fn wait_sent(&self, port: P, id: u8) {
        let Some(signal) = self.sent.get(port.index()) else {
            return;
        };
        while signal.wait().await != id {}
    }

    /// A subscriber for one port's outbound publications.
    pub fn subscribe(&'static self) -> Option<OutboundSubscriber<N>> {
        self.outbound.subscriber().ok()
    }

    /// Publishes to every port without blocking.
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
