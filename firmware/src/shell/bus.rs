//! Message plumbing between the serial link tasks and the controller.
//!
//! * `inbound` — one channel from every link task to the controller,
//!   carrying requests and publications copied out of the link's frame
//!   buffer.
//! * `replies` — one signal per port; the controller answers a request on
//!   the port it came from. A link task has at most one request in flight,
//!   so a signal (latest value wins) is enough. With `espnow` the radio task
//!   is a third port ([`Port::Radio`]): it hands over requests received over
//!   ESP-NOW (one at a time, tagged with the requester's MAC as
//!   [`Origin::Air`]) without waiting, and takes the reply from its signal
//!   in its own select loop.
//! * `outbound` — a publish/subscribe channel with one subscriber per port,
//!   so device publications (log lines, payload status, payload publishes)
//!   reach every port. Publishing never blocks: a port that cannot keep up
//!   (for example USB with no host reading) loses its oldest messages rather
//!   than stalling the controller.
//!
//! The bus is created once at boot and shared as `&'static Bus` with the
//! tasks; embassy tasks require `'static` arguments.

use core::fmt::{self, Write as _};

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::pubsub::{PubSubChannel, Subscriber};
use embassy_sync::signal::Signal;
use heapless::{String, Vec};
use middleware::key::MAX_KEY_LEN;
use middleware::loader::{CHUNK_MAX, PayloadStatus, keys};
use middleware::transport::HandlerError;
use middleware::wasm_abi::{MAX_LOG, MAX_MESSAGE};
use middleware::Key;
use esp_base::ota::Origin;

/// Number of serial ports the protocol can run on (each subscribes to
/// outbound publications).
pub const PORTS: usize = 2;

/// Ports that can send requests: the serial ports and the radio task.
const REPLY_PORTS: usize = PORTS + 1;

/// Largest inbound body: a loader chunk with its offset, or a publication.
pub const MAX_BODY: usize = if 4 + CHUNK_MAX > MAX_MESSAGE { 4 + CHUNK_MAX } else { MAX_MESSAGE };

/// Largest reply body (`sys/info` is at most `MAX_INFO_LEN`, 81 bytes).
pub const MAX_REPLY: usize = 128;

/// Outbound messages buffered per port before the oldest is dropped.
const OUTBOUND_DEPTH: usize = 8;

/// Inbound messages buffered for the controller.
const INBOUND_DEPTH: usize = 2;

type Mutex = CriticalSectionRawMutex;

/// A serial port running the protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Port {
    /// UART0, behind the board's USB-serial bridge chip.
    Uart,
    /// The chip's native USB-Serial-JTAG (ESP32-S3, ESP32-C3).
    #[cfg_attr(feature = "esp32", allow(dead_code))]
    Usb,
    /// The radio task: requests received over ESP-NOW (`espnow`).
    #[cfg_attr(not(feature = "espnow"), allow(dead_code))]
    Radio,
}

impl Port {
    const fn index(self) -> usize {
        match self {
            Port::Uart => 0,
            Port::Usb => 1,
            Port::Radio => 2,
        }
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
pub struct Inbound {
    /// Port it arrived on (where the reply goes).
    pub port: Port,
    /// Who sent it: a serial port, or an ESP-NOW peer.
    pub origin: Origin,
    /// Request or publication.
    pub kind: Kind,
    /// Key (already validated by frame decoding).
    pub key: String<MAX_KEY_LEN>,
    /// Body.
    pub body: Vec<u8, MAX_BODY>,
}

impl Inbound {
    /// Copies a serial frame's key and body; `None` if the body is too
    /// large.
    pub fn new(port: Port, kind: Kind, key: Key<'_>, body: &[u8]) -> Option<Self> {
        let mut k = String::new();
        k.push_str(key.as_str()).ok()?;
        let body = Vec::from_slice(body).ok()?;
        Some(Inbound { port, origin: Origin::Serial, kind, key: k, body })
    }

    /// Copies a request received over ESP-NOW from `mac`, answered on
    /// [`Port::Radio`] with bus id `id`.
    #[cfg(feature = "espnow")]
    pub fn from_air(mac: [u8; 6], id: u8, key: Key<'_>, body: &[u8]) -> Option<Self> {
        Inbound::new(Port::Radio, Kind::Request { id }, key, body).map(|m| Inbound { origin: Origin::Air(mac), ..m })
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
    replies: [Signal<Mutex, Reply>; REPLY_PORTS],
    outbound: PubSubChannel<Mutex, Outbound, OUTBOUND_DEPTH, PORTS, 0>,
}

/// A port's view of outbound publications.
pub type OutboundSubscriber = Subscriber<'static, Mutex, Outbound, OUTBOUND_DEPTH, PORTS, 0>;

impl Bus {
    /// Empty channels.
    pub const fn new() -> Self {
        Bus {
            inbound: Channel::new(),
            replies: [Signal::new(), Signal::new(), Signal::new()],
            outbound: PubSubChannel::new(),
        }
    }

    /// Queues a message for the controller, waiting while it is busy.
    pub async fn send_inbound(&self, msg: Inbound) {
        self.inbound.send(msg).await;
    }

    /// Queues a message for the controller without waiting; `false` (the
    /// message dropped) when the queue is full.
    #[cfg(feature = "espnow")]
    pub fn try_send_inbound(&self, msg: Inbound) -> bool {
        self.inbound.try_send(msg).is_ok()
    }

    /// Waits for the next reply on the radio's port, whatever its id (the
    /// radio task matches it to the request it handed over).
    #[cfg(feature = "espnow")]
    pub async fn radio_reply(&self) -> Reply {
        match self.replies.get(Port::Radio.index()) {
            Some(signal) => signal.wait().await,
            None => core::future::pending().await,
        }
    }

    /// The radio port's reply, if one is waiting.
    #[cfg(feature = "espnow")]
    pub fn try_radio_reply(&self) -> Option<Reply> {
        self.replies.get(Port::Radio.index()).and_then(Signal::try_take)
    }

    /// Waits for the next inbound message.
    pub async fn receive_inbound(&self) -> Inbound {
        self.inbound.receive().await
    }

    /// Delivers a reply to the port that sent the request.
    pub fn reply(&self, port: Port, reply: Reply) {
        if let Some(signal) = self.replies.get(port.index()) {
            signal.signal(reply);
        }
    }

    /// Waits for the reply to this port's request with `id`; replies to
    /// other ids (none are expected) are discarded.
    pub async fn wait_reply(&self, port: Port, id: u8) -> Reply {
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

    /// A subscriber for one port's outbound publications.
    pub fn subscribe(&'static self) -> Option<OutboundSubscriber> {
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
