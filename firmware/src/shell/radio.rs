//! ESP-NOW for payloads: the Wi-Fi station on a fixed channel, the
//! middleware transport over it, and the source routes it streams.
//!
//! Ported from the conagotchi badge firmware (`firmware/src/radio/wifi.rs`):
//! [`EspNowLink`] binds esp-radio's ESP-NOW sender and receiver halves to
//! the middleware [`EspNowRadio`] trait, and the
//! [`EspNowTransport`] on top of it ignores our own MAC and repeated
//! `(sender, key, seq)` frames. Unlike the badge there is no access point,
//! scan or BLE: [`start`] brings Wi-Fi up in station mode, parks it on
//! [`CHANNEL`] (`esp_base::radio::CHANNEL`, 1) and spawns [`radio_task`].
//!
//! The task owns the transport and waits for whichever comes first:
//!
//! * a received sample (the transport's only filter is
//!   [`APP_FILTER`], `Prefix("app")`): copied into [`RX`] with `try_send`,
//!   so a slow controller loses samples (counted) instead of stalling the
//!   radio;
//! * a payload publish from [`TX`] (the controller queues `app/...` keys
//!   with [`send`], which drops and counts when the queue is full):
//!   broadcast through the transport;
//! * a source-route chunk due in [`SOURCES`]: written into the frame buffer
//!   by `esp_base::route::source::write_frame` (radio header, key, chunk
//!   header, then the pattern bytes generated in place, the JPEG bytes
//!   copied from the flash-resident fixture, or the camera frame's bytes
//!   copied from the camera's frame buffer) and broadcast with the radio
//!   directly;
//! * a camera frame due (ESP32, R15): captured by `super::camera::capture`
//!   in this task (bounded, yielding while the DMA fills), then sent chunk
//!   by chunk like any other frame. The first captures and every 16th are
//!   logged on `sys/log` with their length, time and DMA byte count, and
//!   the first captured frame and every 64th are also published on the
//!   serial links as `dbg/cam/jpeg` chunks (a debug mirror, paced at one
//!   chunk per 25 ms), so a host can decode what the camera sees.
//!
//! Every broadcast is awaited until esp-radio's send callback, bounded by
//! `SEND_TIMEOUT_MS`, so at most one frame waits in the Wi-Fi driver: that
//! is the pacing of a source route's chunks (one frame of 174 chunks takes
//! roughly 170–250 ms at the 1 Mbit/s ESP-NOW rate). Between two chunks the
//! task first drains the frames already received (at most [`RX_DEPTH`])
//! and one queued publish, so a streaming source never starves either.
//! After every received frame it yields to the executor once: under a
//! flood (a camera streaming, every frame dropped while no payload
//! subscribes) the receive is ready on every poll, and the task would
//! otherwise keep the controller and the serial links from running.
//!
//! # Requests over the air
//!
//! [`EspNowLink`] taps every `middleware::rpc` frame (`MR`) it receives
//! into [`RPC_RX`] (bounded; drops counted) before the transport, which
//! skips them as foreign. The task decodes each one: a request addressed
//! to our MAC goes through the responder cache
//! (`esp_base::radio::RpcReplies`): a repeat is answered from the cache, a
//! repeat of the request in flight (or any request while one is in flight)
//! is dropped, and a new one is handed to the controller on the bus as a
//! [`Port::Radio`] request from that MAC, without waiting (a full inbound
//! queue drops it; the requester resends). The controller's reply comes
//! back on the radio port's signal, is cached and sent unicast to the
//! requester ([`EspNowUnicast::send_to`], which registers the requester as
//! a peer with the driver, at most `RPC_PEERS` of them, least recently used
//! replaced). Whether the controller accepts the request at all is decided
//! there (`esp_base::ota`). While a source route streams, queued requests
//! and replies are served between two chunks. The request counters
//! (`esp_base::radio::RpcCounters`) are logged like the others.
//!
//! The counters (`esp_base::radio::Counters`) are logged on `sys/log` when
//! losses rise, at most every `REPORT_INTERVAL_MS`. The time spent handling
//! received frames (transport decode and the queue to the controller) is
//! added up in [`TIME`] for the controller's `perf:` report
//! ([`take_time`]).
//!
//! [`SOURCES`] is the one piece of state the controller and this task
//! share: a critical-section mutex around the pure
//! [`SourceTable`], never held across an await. Results of a chunk whose
//! route was stopped meanwhile are ignored by epoch.

use core::cell::RefCell;

use embassy_futures::select::{Either, Either4, select, select4};
use embassy_futures::yield_now;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Timer, with_timeout};
use esp_base::radio::{
    APP_FILTER, CHANNEL, Counters, KEY_CAPACITY, PeerChange, Peers, RPC_BODY, RPC_DEPTH, RPC_PEERS,
    RPC_SEND_TIMEOUT_MS, RX_DEPTH, RX_PAYLOAD, RpcCounters, RpcReplies, SEND_TIMEOUT_MS, TX_DEPTH,
    TX_PAYLOAD,
};
use esp_base::perf::RadioTime;
use esp_base::route::source::{Feed, write_frame};
use esp_base::route::sources::{Capture, Due, Job, SourceTable};
use esp_base::route::Report;
use esp_radio::esp_now::{
    BROADCAST_ADDRESS, EspNowError, EspNowManager, EspNowReceiver, EspNowSender, EspNowWifiInterface, PeerInfo,
};
use esp_radio::wifi::{ControllerConfig, SecondaryChannel, WifiController, WifiError};
use heapless::{String, Vec};
use middleware::espnow::rpc::EspNowUnicast;
use middleware::espnow::{EspNowError as MwError, EspNowRadio, EspNowTransport, HEADER_LEN, MAX_FRAME, Mac};
use middleware::rpc::{self, Admit, Message};
use middleware::transport::HandlerError;
#[cfg(feature = "esp32")]
use middleware::stream::{FrameSet, FrameSource};
use middleware::{AsKey, Key, Publisher, Sample, Subscriber};
use static_cell::StaticCell;

use super::bus::{Bus, Inbound, Reply};
use super::device::now_ms;
use super::irq;

/// A sample received over ESP-NOW, for the controller.
pub struct Received {
    /// Its key (an `app` key).
    pub key: String<KEY_CAPACITY>,
    /// Its payload.
    pub payload: Vec<u8, RX_PAYLOAD>,
}

/// A payload publish to broadcast.
struct Outgoing {
    key: String<KEY_CAPACITY>,
    payload: Vec<u8, TX_PAYLOAD>,
}

/// Samples for the controller (`try_send`: dropped and counted when full).
static RX: Channel<CriticalSectionRawMutex, Received, RX_DEPTH> = Channel::new();

/// Publishes for the radio task (`try_send`: dropped and counted when full).
static TX: Channel<CriticalSectionRawMutex, Outgoing, TX_DEPTH> = Channel::new();

/// A request/response frame tapped off the radio, with its sender.
struct RpcFrame {
    src: Mac,
    data: Vec<u8, MAX_FRAME>,
}

/// Request/response frames for the radio task (`try_send`: dropped and
/// counted when full).
static RPC_RX: Channel<CriticalSectionRawMutex, RpcFrame, RPC_DEPTH> = Channel::new();

/// Request/response frames dropped before they reached [`RPC_RX`].
static RPC_DROPPED: Mutex<CriticalSectionRawMutex, RefCell<u32>> = Mutex::new(RefCell::new(0));

/// Queues `data` from `src` for the radio task if it is a request/response
/// frame.
fn tap(data: &[u8], src: Mac) {
    if !rpc::is_rpc(data) {
        return;
    }
    let queued = Vec::from_slice(data).is_ok_and(|data| RPC_RX.try_send(RpcFrame { src, data }).is_ok());
    if !queued {
        RPC_DROPPED.lock(|n| {
            let mut n = n.borrow_mut();
            *n = n.saturating_add(1);
        });
    }
}

/// Publishes the controller dropped before they reached [`TX`].
static TX_DROPPED: Mutex<CriticalSectionRawMutex, RefCell<u32>> = Mutex::new(RefCell::new(0));

/// Time spent on received frames since the controller last took it.
static TIME: Mutex<CriticalSectionRawMutex, RefCell<RadioTime>> = Mutex::new(RefCell::new(RadioTime::ZERO));

/// Adds `frames` frames handled in the time since `start_us`.
fn add_time(start_us: u64, frames: u32) {
    let us = irq::now_us().saturating_sub(start_us);
    TIME.lock(|t| {
        let mut t = t.borrow_mut();
        *t = t.add(us, frames);
    });
}

/// The radio task's time on received frames since the last call (for the
/// controller's `perf:` report).
pub fn take_time() -> RadioTime {
    TIME.lock(|t| core::mem::take(&mut *t.borrow_mut()))
}

/// Serial key of the camera frame mirror (ESP32): the first captured frame
/// of a boot and every [`MIRROR_EVERY`]th are also published on the serial
/// links, so a host can see what the camera sees (`mwctl monitor`).
#[cfg(feature = "esp32")]
const MIRROR_KEY: Key<'static> = Key::from_static("dbg/cam/jpeg");
/// One captured frame in this many is mirrored.
#[cfg(feature = "esp32")]
const MIRROR_EVERY: u32 = 64;
/// Pause after each mirrored chunk: one ~180-byte frame takes about 16 ms
/// at 115 200 baud.
#[cfg(feature = "esp32")]
const MIRROR_PACE_MS: u64 = 25;

/// The running source routes, shared with the controller.
pub static SOURCES: Mutex<CriticalSectionRawMutex, RefCell<SourceTable>> =
    Mutex::new(RefCell::new(SourceTable::new()));

/// Signalled when a source route starts, so a waiting task looks again.
static SOURCES_CHANGED: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Updates the source table; returns `f`'s value.
fn update<R>(f: impl FnOnce(SourceTable) -> (SourceTable, R)) -> R {
    SOURCES.lock(|cell| {
        let table = *cell.borrow();
        let (next, r) = f(table);
        *cell.borrow_mut() = next;
        r
    })
}

/// Updates the source table (controller side) and wakes the radio task,
/// which may be waiting for a route that is now due.
pub fn with_sources(f: impl FnOnce(SourceTable) -> SourceTable) {
    update(|t| (f(t), ()));
    SOURCES_CHANGED.signal(());
}

/// Reads the source table.
pub fn read_sources<R>(f: impl FnOnce(&SourceTable) -> R) -> R {
    SOURCES.lock(|cell| f(&cell.borrow()))
}

/// Queues a payload publish for broadcast; drops (and counts) it when the
/// queue is full or it does not fit one frame.
pub fn send(key: Key<'_>, data: &[u8]) {
    let mut msg = Outgoing { key: String::new(), payload: Vec::new() };
    let fits = msg.key.push_str(key.as_str()).is_ok() && msg.payload.extend_from_slice(data).is_ok();
    if !fits || TX.try_send(msg).is_err() {
        TX_DROPPED.lock(|n| {
            let mut n = n.borrow_mut();
            *n = n.saturating_add(1);
        });
    }
}

/// Waits for the next received sample.
pub async fn receive() -> Received {
    RX.receive().await
}

/// Discards samples received while no payload ran.
pub fn clear() {
    RX.clear();
}

/// Why the ESP-NOW binding failed.
#[derive(Debug, Clone, Copy)]
pub enum LinkError {
    /// esp-radio refused or failed the send.
    #[allow(dead_code, reason = "carried for Debug")]
    Send(EspNowError),
    /// A received frame did not fit the transport's buffer.
    TooLarge,
}

/// esp-radio's ESP-NOW halves as the middleware transport's radio, with
/// the peer list for unicast responses.
pub struct EspNowLink {
    manager: EspNowManager,
    sender: EspNowSender,
    receiver: EspNowReceiver,
    peers: Peers<RPC_PEERS>,
}

impl EspNowLink {
    /// Registers `peer` with the driver if it is not yet.
    fn add_peer(&self, peer: Mac) -> Result<(), LinkError> {
        if self.manager.peer_exists(&peer) {
            return Ok(());
        }
        let info = PeerInfo {
            interface: EspNowWifiInterface::Station,
            peer_address: peer,
            lmk: None,
            channel: None,
            encrypt: false,
        };
        self.manager.add_peer(info).map_err(LinkError::Send)
    }
}

impl EspNowUnicast for EspNowLink {
    async fn send_to(&mut self, peer: Mac, frame: &[u8]) -> Result<(), LinkError> {
        let (peers, change) = self.peers.send_to(peer);
        self.peers = peers;
        if let PeerChange::Replace(old) = change {
            let _ = self.manager.remove_peer(&old);
        }
        self.add_peer(peer)?;
        self.sender.send_async(&peer, frame).await.map_err(LinkError::Send)
    }
}

impl EspNowRadio for EspNowLink {
    type Error = LinkError;

    async fn broadcast(&mut self, frame: &[u8]) -> Result<(), LinkError> {
        self.sender.send_async(&BROADCAST_ADDRESS, frame).await.map_err(LinkError::Send)
    }

    async fn receive<'b>(&mut self, buf: &'b mut [u8]) -> Result<(&'b [u8], Mac), LinkError> {
        // Cancel-safe: the frame is popped from esp-radio's queue only when
        // the future completes.
        let rx = self.receiver.receive_async().await;
        let data = rx.data();
        tap(data, rx.info.src_address);
        let slot = buf.get_mut(..data.len()).ok_or(LinkError::TooLarge)?;
        slot.copy_from_slice(data);
        Ok((slot, rx.info.src_address))
    }

    fn try_receive<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<(&'b [u8], Mac)>, LinkError> {
        let Some(rx) = self.receiver.receive() else { return Ok(None) };
        let data = rx.data();
        tap(data, rx.info.src_address);
        let slot = buf.get_mut(..data.len()).ok_or(LinkError::TooLarge)?;
        slot.copy_from_slice(data);
        Ok(Some((slot, rx.info.src_address)))
    }
}

/// The transport: one filter slot, 16 remembered senders, and no kept
/// frame (`P = 0`): every receive passes a buffer that holds any sample, so
/// `BufferTooSmall` cannot happen.
type Transport = EspNowTransport<'static, EspNowLink, 16, 1, 0>;

/// Room for any sample the transport delivers (key plus payload).
const SAMPLE_BUF: usize = MAX_FRAME - HEADER_LEN;

/// Wi-Fi buffers sized for broadcast ESP-NOW only (no AP, no TCP/IP): fewer
/// RX/TX buffers than the driver default, no aggregation. See
/// `Profile::radio_heap` for the heap this leaves.
fn controller_config() -> ControllerConfig {
    ControllerConfig::default()
        .with_static_rx_buf_num(6)
        .with_dynamic_rx_buf_num(12)
        .with_dynamic_tx_buf_num(8)
        .with_rx_queue_size(3)
        .with_tx_queue_size(3)
        .with_ampdu_rx_enable(false)
        .with_ampdu_tx_enable(false)
        .with_rx_ba_win(4)
}

static CONTROLLER: StaticCell<WifiController<'static>> = StaticCell::new();

/// Why [`start`] failed.
#[derive(Debug, Clone, Copy)]
pub enum StartError {
    /// The Wi-Fi driver refused to initialise or to change channel.
    #[allow(dead_code, reason = "carried for Debug")]
    Wifi(WifiError),
    /// [`start`] ran twice.
    Again,
}

/// Brings Wi-Fi up (station, not associated) on [`CHANNEL`] and returns
/// the transport. Call once, after `esp_rtos::start` and the heap.
pub fn start(wifi: esp_hal::peripherals::WIFI<'static>) -> Result<Transport, StartError> {
    let controller = WifiController::new(wifi, controller_config()).map_err(StartError::Wifi)?;
    let controller = CONTROLLER.try_init(controller).ok_or(StartError::Again)?;
    controller.set_channel(CHANNEL, SecondaryChannel::None).map_err(StartError::Wifi)?;
    // The only ESP-NOW instance: `esp_now` panics only if one exists.
    let (manager, sender, receiver) = controller.esp_now().split();
    let link = EspNowLink { manager, sender, receiver, peers: Peers::new() };
    let transport = EspNowTransport::new(link).with_own_address(own_mac());
    transport.with_filter(APP_FILTER).map_err(|_| StartError::Again)
}

/// Our station MAC (the eFuse base MAC): requests over the air are
/// addressed to it, and signed control requests must name it.
pub fn own_mac() -> Mac {
    let mut mac = [0u8; 6];
    for (dst, src) in mac.iter_mut().zip(esp_hal::efuse::base_mac_address().as_bytes()) {
        *dst = *src;
    }
    mac
}

/// The radio task's state.
struct Radio {
    transport: Transport,
    counters: Counters,
    /// Losses at the last report, and when it was.
    logged: (u32, u32),
    /// Radio sequence number of route chunks (the transport keeps its own
    /// for publishes; receivers deduplicate per sender and key).
    route_seq: u8,
    /// Camera captures so far (for the log).
    #[cfg(feature = "esp32")]
    captures: u32,
    bus: &'static Bus,
    /// Our station MAC: requests must be addressed to it.
    own: Mac,
    /// The responder: the request in flight and the cached responses.
    replies: RpcReplies,
    /// Bus id of the request handed to the controller, if any.
    pending: Option<u8>,
    /// Bus id for the next request.
    next_id: u8,
    rpc: RpcCounters,
    /// Request counters at the last report, and when it was.
    rpc_logged: (RpcCounters, u32),
}

impl Radio {
    /// Hands one receive outcome to the controller.
    fn received(&mut self, rx: Result<Sample<'_>, MwError<LinkError>>) {
        self.counters = match rx {
            Ok(s) => {
                let mut msg = Received { key: String::new(), payload: Vec::new() };
                let fits = msg.key.push_str(s.key.as_str()).is_ok()
                    && msg.payload.extend_from_slice(s.payload).is_ok();
                self.counters.received(fits && RX.try_send(msg).is_ok())
            }
            Err(MwError::IgnoredLimit) => self.counters,
            Err(_) => self.counters.bad(),
        };
    }

    /// Drains the frames already received, at most [`RX_DEPTH`].
    fn drain(&mut self, buf: &mut [u8; SAMPLE_BUF]) {
        let start = irq::now_us();
        let mut frames = 0;
        for _ in 0..RX_DEPTH {
            match self.transport.try_recv(buf) {
                Ok(Some(s)) => self.received(Ok(s)),
                Ok(None) => break,
                Err(e) => self.received(Err(e)),
            }
            frames += 1;
        }
        add_time(start, frames);
    }

    /// Broadcasts a payload publish.
    async fn publish(&mut self, msg: Outgoing) {
        let ok = match Key::new(&msg.key) {
            Ok(key) => matches!(
                with_timeout(Duration::from_millis(SEND_TIMEOUT_MS), self.transport.publish(key, &msg.payload)).await,
                Ok(Ok(()))
            ),
            Err(_) => false,
        };
        self.counters = self.counters.sent(ok);
    }

    /// Captures one camera frame for `capture`'s route; folds the result
    /// into the route and raises its events (ESP32 only; elsewhere no route
    /// can ask).
    async fn capture(&mut self, capture: Capture) {
        #[cfg(feature = "esp32")]
        let result = {
            let (result, seen) = super::camera::capture().await;
            self.captures = self.captures.wrapping_add(1);
            // The first captures and then one in 16 are logged.
            if self.captures <= 4 || self.captures.is_multiple_of(16) {
                self.bus.log(format_args!(
                    "camera: capture {} {:?} in {} ms (DMA {} B, skipped {} B, head {:02x?})",
                    self.captures, result, seen.ms, seen.raw, seen.skipped, seen.head
                ));
            }
            if let Ok(len) = result
                && (self.captures == 1 || self.captures.is_multiple_of(MIRROR_EVERY))
            {
                self.mirror(len).await;
            }
            result
        };
        #[cfg(not(feature = "esp32"))]
        let result = Err(esp_base::route::source::CaptureFault::Hardware);
        let now = now_ms();
        let time_us = irq::now_us() as u32;
        update(|table| {
            let (next, report) = table.captured(capture, result, now);
            for event in report.events(capture.handle, time_us).into_iter().flatten() {
                irq::push(event);
            }
            (next, ())
        });
    }

    /// Publishes the captured frame of `len` bytes on the serial links as
    /// [`MIRROR_KEY`] chunks (the stream wire format, 166 data bytes each),
    /// paced so the links' outbound queue never overflows.
    #[cfg(feature = "esp32")]
    async fn mirror(&mut self, len: u32) {
        let Ok(chunker) = middleware::stream::Chunker::new(0, len, middleware::stream::ESPNOW_CHUNK_DATA) else {
            return;
        };
        for plan in chunker.chunks() {
            let mut msg = [0u8; middleware::stream::HEADER_LEN + middleware::stream::ESPNOW_CHUNK_DATA];
            let n = plan.range.len();
            let filled = super::camera::with_frame(len, |frame| {
                let (head, data) = msg.split_at_mut(middleware::stream::HEADER_LEN);
                let header = plan.header.encode(n).ok()?;
                head.copy_from_slice(&header);
                data.get_mut(..n)?.copy_from_slice(frame.get(plan.range.clone())?);
                Some(())
            })
            .flatten();
            if filled.is_none() {
                return;
            }
            if let Some(bytes) = msg.get(..middleware::stream::HEADER_LEN + n) {
                let _ = self.bus.publish(MIRROR_KEY, bytes);
            }
            Timer::after_millis(MIRROR_PACE_MS).await;
        }
    }

    /// Writes and broadcasts one route chunk; folds the result into the
    /// route and raises its events.
    async fn chunk(&mut self, job: Job, frame: &mut [u8; MAX_FRAME]) {
        let seq = self.route_seq;
        self.route_seq = seq.wrapping_add(1);
        let written = match job.feed {
            Feed::Stored(source) => write_frame(frame, seq, job.key.as_key(), job.frame, &source, &job.plan).ok(),
            Feed::Camera { len } => camera_chunk(frame, seq, &job, len),
        };
        let ok = match written {
            Some(n) => match frame.get(..n) {
                Some(bytes) => matches!(
                    with_timeout(
                        Duration::from_millis(SEND_TIMEOUT_MS),
                        self.transport.radio_mut().broadcast(bytes)
                    )
                    .await,
                    Ok(Ok(()))
                ),
                None => false,
            },
            None => false,
        };
        self.counters = self.counters.sent(ok);
        let now = now_ms();
        let time_us = irq::now_us() as u32;
        // The events are pushed under the table lock, so a concurrent
        // `route_stop` (which purges them) cannot be overtaken.
        update(|table| {
            let (next, report): (SourceTable, Report) = table.finish(job.handle, job.epoch, ok, now);
            for event in report.events(job.handle, time_us).into_iter().flatten() {
                irq::push(event);
            }
            (next, ())
        });
    }

    /// Handles one tapped request/response frame (see the module docs).
    async fn request(&mut self, frame: RpcFrame) {
        let src = frame.src;
        let (id, key, body) = match Message::decode(&frame.data) {
            Ok(Message::Request { id, to, key, body }) if to == self.own => (id, key, body),
            Ok(_) => return,
            Err(_) => {
                self.rpc = self.rpc.dropped(1);
                return;
            }
        };
        let (replies, admit) = self.replies.admit(src, id, now_ms());
        self.replies = replies;
        match admit {
            Admit::Busy => self.rpc = self.rpc.busy(),
            Admit::Replay => {
                self.rpc = self.rpc.replayed();
                let mut copy = [0u8; RPC_BODY];
                let result = match self.replies.cached(src, id) {
                    Some(Ok(b)) => copy
                        .get_mut(..b.len())
                        .map(|c| {
                            c.copy_from_slice(b);
                            &*c
                        })
                        .ok_or(HandlerError::Internal),
                    Some(Err(e)) => Err(e),
                    None => Err(HandlerError::Internal),
                };
                self.respond(src, id, result).await;
            }
            Admit::Execute => {
                let bus_id = self.next_id;
                self.next_id = bus_id.wrapping_add(1);
                match Inbound::from_air(src, bus_id, key, body) {
                    Some(msg) => {
                        if self.bus.try_send_inbound(msg) {
                            self.rpc = self.rpc.executed();
                            self.pending = Some(bus_id);
                        } else {
                            self.rpc = self.rpc.dropped(1);
                            self.replies = self.replies.abandon();
                        }
                    }
                    None => {
                        let result = Err(HandlerError::BufferTooSmall);
                        self.replies = self.replies.complete(src, id, result);
                        self.respond(src, id, result).await;
                    }
                }
            }
        }
    }

    /// The controller answered the request handed over with bus id
    /// `reply.id`: caches and sends the response.
    async fn replied(&mut self, reply: Reply) {
        let Some(f) = self.replies.in_flight() else { return };
        if self.pending != Some(reply.id) {
            return;
        }
        self.pending = None;
        let result = reply.result.as_ref().map(|v| v.as_slice()).map_err(|e| *e);
        self.replies = self.replies.complete(f.peer, f.id, result);
        self.respond(f.peer, f.id, result).await;
    }

    /// Sends a response unicast to `peer`.
    async fn respond(&mut self, peer: Mac, id: u16, result: Result<&[u8], HandlerError>) {
        let mut frame = [0u8; MAX_FRAME];
        let msg = Message::Response { id, to: peer, result };
        let sent = match msg.encode(&mut frame) {
            Ok(n) => match frame.get(..n) {
                Some(bytes) => matches!(
                    with_timeout(
                        Duration::from_millis(RPC_SEND_TIMEOUT_MS),
                        self.transport.radio_mut().send_to(peer, bytes)
                    )
                    .await,
                    Ok(Ok(()))
                ),
                None => false,
            },
            Err(_) => false,
        };
        if !sent {
            self.rpc = self.rpc.tx_failed();
        }
    }

    /// Serves the requests and the controller reply already queued (at most
    /// [`RPC_DEPTH`] requests), between two route chunks.
    async fn serve_queued(&mut self) {
        if let Some(reply) = self.bus.try_radio_reply() {
            self.replied(reply).await;
        }
        for _ in 0..RPC_DEPTH {
            let Ok(frame) = RPC_RX.try_receive() else { break };
            self.request(frame).await;
        }
    }

    /// Logs the counters when losses rose.
    fn report(&mut self) {
        let rpc_dropped = RPC_DROPPED.lock(|n| core::mem::take(&mut *n.borrow_mut()));
        self.rpc = self.rpc.dropped(rpc_dropped);
        let now = now_ms();
        let (logged, at) = self.rpc_logged;
        if self.rpc.report(&logged, now.wrapping_sub(at)) {
            let r = self.rpc;
            self.bus.log(format_args!(
                "espnow rpc: {} executed, {} replayed, {} busy, {} dropped, {} responses failed",
                r.executed, r.replayed, r.busy, r.dropped, r.tx_failed
            ));
            self.rpc_logged = (r, now);
        }
        let dropped = TX_DROPPED.lock(|n| core::mem::take(&mut *n.borrow_mut()));
        self.counters = self.counters.dropped(dropped);
        let now = now_ms();
        let (logged, at) = self.logged;
        if self.counters.report(logged, now.wrapping_sub(at)) {
            let c = self.counters;
            self.bus.log(format_args!(
                "espnow: rx {} (dropped {}, bad {}), tx {} (dropped {}, failed {})",
                c.rx, c.rx_dropped, c.rx_bad, c.tx, c.tx_dropped, c.tx_failed
            ));
            self.logged = (c.losses(), now);
        }
    }
}

/// Writes the ESP-NOW frame of a camera chunk, its bytes copied from the
/// camera's frame buffer (ESP32 only).
fn camera_chunk(out: &mut [u8; MAX_FRAME], seq: u8, job: &Job, len: u32) -> Option<usize> {
    #[cfg(feature = "esp32")]
    return super::camera::with_frame(len, |bytes| {
        let frames = [bytes];
        let set = FrameSet::new(&frames).ok()?;
        write_frame(out, seq, job.key.as_key(), 0, &FrameSource::Frames(set), &job.plan).ok()
    })
    .flatten();
    #[cfg(not(feature = "esp32"))]
    {
        let _ = (out, seq, job, len);
        None
    }
}

/// Runs the transport and the source routes forever (see the module docs).
#[embassy_executor::task]
pub async fn radio_task(transport: Transport, bus: &'static Bus) {
    bus.log(format_args!("espnow: channel {CHANNEL}, keys app/..."));
    let mut radio = Radio {
        transport,
        counters: Counters::new(),
        logged: (0, 0),
        route_seq: 0,
        #[cfg(feature = "esp32")]
        captures: 0,
        bus,
        own: own_mac(),
        replies: RpcReplies::new(),
        pending: None,
        next_id: 0,
        rpc: RpcCounters::new(),
        rpc_logged: (RpcCounters::new(), 0),
    };
    let mut sample = [0u8; SAMPLE_BUF];
    let mut frame = [0u8; MAX_FRAME];
    loop {
        radio.report();
        let due = update(|t| t.due(now_ms()));
        let wait = match due {
            Due::Capture(capture) => {
                radio.drain(&mut sample);
                radio.serve_queued().await;
                radio.capture(capture).await;
                continue;
            }
            Due::Send(job) => {
                radio.drain(&mut sample);
                radio.serve_queued().await;
                if let Ok(msg) = TX.try_receive() {
                    radio.publish(msg).await;
                }
                radio.chunk(job, &mut frame).await;
                continue;
            }
            Due::Wait(ms) => Some(ms),
            Due::Idle => None,
        };
        let timer = async {
            match wait {
                Some(ms) => Timer::after_millis(u64::from(ms)).await,
                None => core::future::pending().await,
            }
        };
        let rpc = select(RPC_RX.receive(), bus.radio_reply());
        let woke = select(
            select4(radio.transport.recv(&mut sample), TX.receive(), timer, SOURCES_CHANGED.wait()),
            rpc,
        )
        .await;
        match woke {
            Either::First(Either4::First(rx)) => {
                let start = irq::now_us();
                radio.received(rx);
                add_time(start, 1);
                // A flood keeps esp-radio's queue non-empty, so `recv` is
                // ready on every poll; yield so the controller and the
                // serial links still run between two frames.
                yield_now().await;
            }
            Either::First(Either4::Second(msg)) => radio.publish(msg).await,
            Either::First(Either4::Third(()) | Either4::Fourth(())) => {}
            Either::Second(Either::First(frame)) => radio.request(frame).await,
            Either::Second(Either::Second(reply)) => radio.replied(reply).await,
        }
    }
}
