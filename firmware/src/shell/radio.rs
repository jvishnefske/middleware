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
//!   header, then the pattern bytes generated in place or the JPEG bytes
//!   copied from the flash-resident fixture) and broadcast with the radio
//!   directly.
//!
//! Every broadcast is awaited until esp-radio's send callback, bounded by
//! `SEND_TIMEOUT_MS`, so at most one frame waits in the Wi-Fi driver: that
//! is the pacing of a source route's chunks (one frame of 174 chunks takes
//! roughly 170–250 ms at the 1 Mbit/s ESP-NOW rate). Between two chunks the
//! task first drains the frames already received (at most [`RX_DEPTH`])
//! and one queued publish, so a streaming source never starves either.
//!
//! The counters (`esp_base::radio::Counters`) are logged on `sys/log` when
//! losses rise, at most every `REPORT_INTERVAL_MS`.
//!
//! [`SOURCES`] is the one piece of state the controller and this task
//! share: a critical-section mutex around the pure
//! [`SourceTable`], never held across an await. Results of a chunk whose
//! route was stopped meanwhile are ignored by epoch.

use core::cell::RefCell;

use embassy_futures::select::{Either4, select4};
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Timer, with_timeout};
use esp_base::radio::{
    APP_FILTER, CHANNEL, Counters, KEY_CAPACITY, RX_DEPTH, RX_PAYLOAD, SEND_TIMEOUT_MS, TX_DEPTH,
    TX_PAYLOAD,
};
use esp_base::route::sources::{Due, Job, SourceTable};
use esp_base::route::{Report, source::write_frame};
use esp_radio::esp_now::{BROADCAST_ADDRESS, EspNowError, EspNowReceiver, EspNowSender};
use esp_radio::wifi::{ControllerConfig, SecondaryChannel, WifiController, WifiError};
use heapless::{String, Vec};
use middleware::espnow::{EspNowError as MwError, EspNowRadio, EspNowTransport, HEADER_LEN, MAX_FRAME, Mac};
use middleware::{AsKey, Key, Publisher, Sample, Subscriber};
use static_cell::StaticCell;

use super::bus::Bus;
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

/// Publishes the controller dropped before they reached [`TX`].
static TX_DROPPED: Mutex<CriticalSectionRawMutex, RefCell<u32>> = Mutex::new(RefCell::new(0));

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

/// esp-radio's ESP-NOW halves as the middleware transport's radio.
pub struct EspNowLink {
    sender: EspNowSender,
    receiver: EspNowReceiver,
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
        let slot = buf.get_mut(..data.len()).ok_or(LinkError::TooLarge)?;
        slot.copy_from_slice(data);
        Ok((slot, rx.info.src_address))
    }

    fn try_receive<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<(&'b [u8], Mac)>, LinkError> {
        let Some(rx) = self.receiver.receive() else { return Ok(None) };
        let data = rx.data();
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
    let (_manager, sender, receiver) = controller.esp_now().split();
    let transport = EspNowTransport::new(EspNowLink { sender, receiver }).with_own_address(own_mac());
    transport.with_filter(APP_FILTER).map_err(|_| StartError::Again)
}

/// Our station MAC (the eFuse base MAC).
fn own_mac() -> Mac {
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
    bus: &'static Bus,
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
        for _ in 0..RX_DEPTH {
            match self.transport.try_recv(buf) {
                Ok(Some(s)) => self.received(Ok(s)),
                Ok(None) => break,
                Err(e) => self.received(Err(e)),
            }
        }
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

    /// Writes and broadcasts one route chunk; folds the result into the
    /// route and raises its events.
    async fn chunk(&mut self, job: Job, frame: &mut [u8; MAX_FRAME]) {
        let seq = self.route_seq;
        self.route_seq = seq.wrapping_add(1);
        let ok = match write_frame(frame, seq, job.key.as_key(), job.frame, &job.source, &job.plan) {
            Ok(n) => match frame.get(..n) {
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
            Err(_) => false,
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

    /// Logs the counters when losses rose.
    fn report(&mut self) {
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

/// Runs the transport and the source routes forever (see the module docs).
#[embassy_executor::task]
pub async fn radio_task(transport: Transport, bus: &'static Bus) {
    bus.log(format_args!("espnow: channel {CHANNEL}, keys app/..."));
    let mut radio = Radio { transport, counters: Counters::new(), logged: (0, 0), route_seq: 0, bus };
    let mut sample = [0u8; SAMPLE_BUF];
    let mut frame = [0u8; MAX_FRAME];
    loop {
        radio.report();
        let due = update(|t| t.due(now_ms()));
        let wait = match due {
            Due::Send(job) => {
                radio.drain(&mut sample);
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
        match select4(radio.transport.recv(&mut sample), TX.receive(), timer, SOURCES_CHANGED.wait()).await {
            Either4::First(rx) => radio.received(rx),
            Either4::Second(msg) => radio.publish(msg).await,
            Either4::Third(()) | Either4::Fourth(()) => {}
        }
    }
}
