//! The ESP-NOW gateway (EG6–EG15): ESP-NOW devices heard through the USB
//! bridge (`tools/espnow-bridge`, [`middleware::espnow::bridge`]) joined to
//! the node's Zenoh session. Enabled by `MW_POSIX_NODE_ESPNOW_BRIDGE`.
//!
//! * **Uplink.** `app/**` publications heard on the radio are put on Zenoh
//!   under the device's MAC-derived node id ([`core::sender_id`],
//!   `@mw/node/<mac>0000/...`), with a per-(device, key) sequence and a
//!   stream whose origin is the MAC, so subscribers see the device itself
//!   publishing. `sys/**` and `disc/**` never appear under `app`.
//! * **Beacons.** `disc/<kind>` beacons are announced through
//!   [`ZenohDiscovery`] with the device as node, by the E21 rules of
//!   [`core::Registry`], and withdrawn on a goodbye, when their TTL (at
//!   most [`core::MAX_BEACON_TTL_S`]) passes, when the device is no longer
//!   heard (expired or evicted from [`core::Heard`]), or when the gateway
//!   stops.
//! * **Downlink.** Samples from other sessions on the configured `app`
//!   filters are broadcast over ESP-NOW, except those published under a
//!   heard device's id ([`core::skip_downlink`]).
//! * **Requests.** Each heard device (at most [`core::MAX_SERVED`]) gets
//!   concrete queryables `@mw/node/<mac id>/<key>` for the
//!   [`core::FORWARDED`] keys; a query is sent to the device as an ESP-NOW
//!   request ([`RpcRequester`], resent with the same id) and the answer
//!   relayed. Concrete keys, not a wildcard, so queries for other nodes
//!   never reach the gateway. They are undeclared when the device is
//!   forgotten.
//! * **Scans.** A wildcard query (`mwctl scout`'s `@mw/node/*/sys/info`)
//!   is answered by each device's `sys/info` queryable alone, on the
//!   device's own key, and never forwarded on any other key
//!   ([`core::plan_query`], EG14). The answer comes from a cache of what
//!   the device last said in `sys/info` ([`core::InfoCache`], EG15),
//!   filled when the device is first served and by every `sys/info`
//!   answer relayed; a scan that misses waits for one fetch of that
//!   device, at most [`SCAN_HOLD`]. A query on the device's own
//!   `sys/info` key always goes to the device.
//! * **Heard list.** The heard devices ([`core::Heard::peers`]) are put on
//!   `gw/espnow/<bridge id>/heard` every [`GwConfig::heard_period`] and on
//!   change, and served at `@mw/node/<bridge id>/sys/peers`.
//!
//! One future runs three loops: the **pump** owns the [`BridgeRadio`] and
//! serves send commands, routing every frame heard to the bus (`MW`
//! frames) or to the requests (`MR` frames) through bounded queues (full:
//! dropped and counted); while a send waits for its `Sent` the pump goes
//! on reading and routing, so a lost `Sent` does not stall the uplink.
//! The last [`MW_RESERVED`] slots of the `MW` queue are kept for devices
//! already heard: a frame from a MAC not (yet) heard is dropped once the
//! queue is that full, so a flood of forged MACs while the bus is busy
//! cannot crowd out the devices that are talking (EG10); the queue stays
//! one FIFO, so frames are not reordered. The **bus** runs the
//! uplink, beacons, downlink and heard list; the **requests** loop serves
//! queries one at a time, so a slow device stalls only other requests;
//! the cache's own fetches run when no query waits, one attempt each.
//! Scans the cache can answer never reach that loop: the queryable
//! replies itself.

pub mod core;

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use embedded_io_async::{Read, Write};
use middleware::dedup::Sequence;
use middleware::discovery::Announce;
use middleware::discovery::beacon::BEACON_PREFIX;
use middleware::discovery::directory::ServiceRecord;
use middleware::espnow::bridge::{BridgeIoError, BridgeRadio};
use middleware::espnow::rpc::{Delay, EspNowUnicast, Retry, RpcRequester};
use middleware::espnow::{EspNowRadio, EspNowReceive, EspNowSend, EspNowTransport, MAX_FRAME, Mac, RadioHalves};
use middleware::key::{Key, KeyFilter};
use middleware::loader::keys;
use middleware::radio_bridge::BROADCAST;
use middleware::transport::{
    HandlerError, Publisher, RequestOptions, Requester, SenderId, Split, Subscribe, Subscriber,
};
use middleware::zenoh::responder::request_meta;
use middleware::zenoh::{ZenohDiscovery, ZenohTransport, wire};
use tokio::sync::{mpsc, oneshot};
use zenoh::Wait;
use zenoh::qos::CongestionControl;
use zenoh::query::{Query, Queryable};

use self::core::{
    Action, Heard, InfoCache, Plan, Registry, mac_of, origin_of, plan_query, reply_error_for, sender_id, skip_downlink,
};
use crate::config::DownlinkFilter;

/// Frames the pump queues for the bus.
pub const MW_QUEUE: usize = 96;
/// Of [`MW_QUEUE`], the slots only frames from heard devices may take.
pub const MW_RESERVED: usize = 32;
/// Frames the pump queues for the requests.
pub const MR_QUEUE: usize = 8;
/// Queries waiting to be forwarded.
pub const QUERY_QUEUE: usize = 32;
/// `sys/info` fetches waiting to be queued, and scans waiting for one.
pub const SCAN_QUEUE: usize = 64;
/// How long a scan that missed the cache is worth answering: a scanner
/// (`mwctl scout`) collects replies for about 2 s.
pub const SCAN_HOLD: Duration = Duration::from_millis(1500);
/// (device, key) uplink streams remembered; more start the map afresh.
pub const MAX_STREAMS: usize = 1024;
/// `app`, the uplink and downlink key space.
const APP: Key<'static> = Key::from_static("app");
/// The radio's receive filters: `app` is republished, `disc` announced,
/// `sys` only marks the device heard.
const UPLINK: [KeyFilter<'static>; 3] =
    [KeyFilter::Prefix(APP), KeyFilter::Prefix(BEACON_PREFIX), KeyFilter::Prefix(Key::from_static("sys"))];

/// The gateway's settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GwConfig {
    /// `app` filters broadcast to the devices.
    pub downlink: Vec<DownlinkFilter>,
    /// How long a send waits for the bridge's `Sent`.
    pub sent_timeout_ms: u32,
    /// How long `connect` waits for the bridge's `Hello`.
    pub connect_timeout_ms: u32,
    /// Retries of a forwarded request.
    pub request_retry: Retry,
    /// How often the heard list is put.
    pub heard_period: Duration,
    /// A device unheard for this long is forgotten: its records withdrawn,
    /// its queryables undeclared.
    pub heard_ttl_ms: u64,
}

impl Default for GwConfig {
    fn default() -> Self {
        GwConfig {
            downlink: Vec::new(),
            sent_timeout_ms: middleware::espnow::bridge::SENT_TIMEOUT_MS,
            connect_timeout_ms: 2000,
            request_retry: Retry { timeout_ms: 400, attempts: 4 },
            heard_period: Duration::from_secs(10),
            heard_ttl_ms: core::HEARD_TTL_MS,
        }
    }
}

/// Why the gateway stopped (other than being asked to).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GwError {
    /// The bridge failed or went away.
    Bridge(String),
    /// Zenoh refused a declaration.
    Zenoh(String),
}

impl std::fmt::Display for GwError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GwError::Bridge(e) => write!(f, "bridge: {e}"),
            GwError::Zenoh(e) => write!(f, "zenoh: {e}"),
        }
    }
}

impl std::error::Error for GwError {}

/// What the gateway did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Report {
    /// `app` samples put on Zenoh.
    pub frames_up: u32,
    /// Downlink samples broadcast.
    pub frames_down: u32,
    /// Beacon records announced.
    pub beacons: u32,
    /// Beacons refused (E21).
    pub refused: u32,
    /// Frames and samples dropped (full queues, too large, send failures).
    pub dropped: u32,
    /// Requests forwarded.
    pub requests: u32,
    /// Forwarded requests that failed.
    pub request_failures: u32,
    /// Devices heard at the end.
    pub heard: u32,
    /// New MACs refused because the heard table was full of recent devices.
    pub refused_macs: u32,
}

#[derive(Default)]
struct Counters {
    frames_up: AtomicU32,
    frames_down: AtomicU32,
    beacons: AtomicU32,
    dropped: AtomicU32,
    requests: AtomicU32,
    request_failures: AtomicU32,
}

fn bump(c: &AtomicU32) {
    c.fetch_add(1, Ordering::Relaxed);
}

/// A send failure as the bus and the requests see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkError {
    /// The bridge reported no acknowledgement.
    NotAcked,
    /// No `Sent` in time.
    SentTimeout,
    /// The frame is empty or over 250 bytes, or does not fit the buffer.
    FrameTooLarge,
    /// The pump is gone.
    Down,
}

impl<E> From<BridgeIoError<E>> for LinkError {
    fn from(e: BridgeIoError<E>) -> Self {
        match e {
            BridgeIoError::NotAcked => LinkError::NotAcked,
            BridgeIoError::SentTimeout => LinkError::SentTimeout,
            BridgeIoError::FrameTooLarge => LinkError::FrameTooLarge,
            _ => LinkError::Down,
        }
    }
}

/// [`Delay`] over tokio's timer.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioDelay;

impl Delay for TokioDelay {
    async fn delay_ms(&mut self, ms: u32) {
        tokio::time::sleep(Duration::from_millis(u64::from(ms))).await;
    }
}

/// A send for the pump.
struct Cmd {
    dst: Mac,
    frame: Vec<u8>,
    done: oneshot::Sender<Result<(), LinkError>>,
}

/// A frame heard, with its sender.
type Frame = (Mac, Vec<u8>);
/// The MACs in the bus's [`Heard`] table, as the pump sees them.
type Known = Arc<Mutex<HashSet<Mac>>>;
/// A downlink sample: key, payload, sender.
type Down = (String, Vec<u8>, Option<SenderId>);

async fn send_cmd(cmds: &mpsc::Sender<Cmd>, dst: Mac, frame: &[u8]) -> Result<(), LinkError> {
    let (done, result) = oneshot::channel();
    cmds.send(Cmd { dst, frame: frame.to_vec(), done }).await.map_err(|_| LinkError::Down)?;
    result.await.unwrap_or(Err(LinkError::Down))
}

fn copy_frame(buf: &mut [u8], (src, frame): Frame) -> Result<(&[u8], Mac), LinkError> {
    let out = buf.get_mut(..frame.len()).ok_or(LinkError::FrameTooLarge)?;
    out.copy_from_slice(&frame);
    Ok((out, src))
}

fn try_frame<'b>(rx: &mut mpsc::Receiver<Frame>, buf: &'b mut [u8]) -> Result<Option<(&'b [u8], Mac)>, LinkError> {
    match rx.try_recv() {
        Ok(f) => copy_frame(buf, f).map(Some),
        Err(mpsc::error::TryRecvError::Empty) => Ok(None),
        Err(mpsc::error::TryRecvError::Disconnected) => Err(LinkError::Down),
    }
}

/// The bus's sending half: broadcasts through the pump.
struct TxHandle {
    cmds: mpsc::Sender<Cmd>,
}

impl EspNowSend for TxHandle {
    type Error = LinkError;

    async fn broadcast(&mut self, frame: &[u8]) -> Result<(), LinkError> {
        send_cmd(&self.cmds, BROADCAST, frame).await
    }
}

/// Where the pump puts `MW` frames: the bus's queue, its last
/// [`MW_RESERVED`] slots kept for devices already heard.
struct MwTx {
    tx: mpsc::Sender<Frame>,
    known: Known,
}

impl MwTx {
    /// Queues `frame`; `false` if it was dropped.
    fn send(&self, src: Mac, frame: Vec<u8>) -> bool {
        // Only the pump sends, so the room seen here is not taken before
        // the `try_send`.
        if self.tx.capacity() <= MW_RESERVED && !self.known.lock().is_ok_and(|k| k.contains(&src)) {
            return false;
        }
        self.tx.try_send((src, frame)).is_ok()
    }
}

/// The bus's receiving half: the pump's `MW` frames. Cancel-safe (an mpsc
/// `recv`).
struct RxHandle {
    rx: mpsc::Receiver<Frame>,
}

impl EspNowReceive for RxHandle {
    type Error = LinkError;

    async fn receive<'b>(&mut self, buf: &'b mut [u8]) -> Result<(&'b [u8], Mac), LinkError> {
        let f = self.rx.recv().await.ok_or(LinkError::Down)?;
        copy_frame(buf, f)
    }

    fn try_receive<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<(&'b [u8], Mac)>, LinkError> {
        try_frame(&mut self.rx, buf)
    }
}

/// The requests' radio: unicast through the pump, the pump's `MR` frames.
struct MrHandle<'a> {
    cmds: &'a mpsc::Sender<Cmd>,
    rx: &'a mut mpsc::Receiver<Frame>,
}

impl EspNowRadio for MrHandle<'_> {
    type Error = LinkError;

    async fn broadcast(&mut self, frame: &[u8]) -> Result<(), LinkError> {
        send_cmd(self.cmds, BROADCAST, frame).await
    }

    async fn receive<'b>(&mut self, buf: &'b mut [u8]) -> Result<(&'b [u8], Mac), LinkError> {
        let f = self.rx.recv().await.ok_or(LinkError::Down)?;
        copy_frame(buf, f)
    }

    fn try_receive<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<(&'b [u8], Mac)>, LinkError> {
        try_frame(self.rx, buf)
    }
}

impl EspNowUnicast for MrHandle<'_> {
    async fn send_to(&mut self, peer: Mac, frame: &[u8]) -> Result<(), LinkError> {
        send_cmd(self.cmds, peer, frame).await
    }
}

/// The pump: serves send commands, routes heard frames. Returns why it
/// stopped.
///
/// A send is started ([`BridgeRadio::begin_send`]) and its `Sent` awaited
/// here, between reads, so frames heard meanwhile are routed as they come
/// instead of piling up in the radio's ring; at most one send is in
/// flight (the bridge queues four).
async fn pump<IO: Read + Write>(
    mut radio: BridgeRadio<IO, TokioDelay>,
    mut cmds: mpsc::Receiver<Cmd>,
    mw: MwTx,
    mr: mpsc::Sender<Frame>,
    sent_timeout: Duration,
    counters: &Counters,
) -> GwError {
    enum Ev {
        Cmd(Option<Cmd>),
        Read(Result<(), String>),
        SentTimeout,
        Tick,
    }
    let mut raw = [0u8; MAX_FRAME];
    let mut stats = tokio::time::interval(Duration::from_secs(60));
    let mut inflight: Option<(oneshot::Sender<Result<(), LinkError>>, tokio::time::Instant)> = None;
    loop {
        let deadline = inflight.as_ref().map_or_else(tokio::time::Instant::now, |(_, d)| *d);
        // `pump_one` is cancel-safe: whatever it read is handled before it
        // returns, so another branch winning loses no frame.
        let ev = tokio::select! {
            c = cmds.recv(), if inflight.is_none() => Ev::Cmd(c),
            r = radio.pump_one() => Ev::Read(r.map_err(|e| e.to_string())),
            _ = tokio::time::sleep_until(deadline), if inflight.is_some() => Ev::SentTimeout,
            _ = stats.tick() => Ev::Tick,
        };
        match ev {
            Ev::Cmd(None) => return GwError::Bridge("gateway stopped".into()),
            Ev::Cmd(Some(cmd)) => match radio.begin_send(cmd.dst, &cmd.frame).await {
                Ok(()) => inflight = Some((cmd.done, tokio::time::Instant::now() + sent_timeout)),
                Err(e @ (BridgeIoError::Io(_) | BridgeIoError::Closed)) => return GwError::Bridge(e.to_string()),
                Err(e) => {
                    let _ = cmd.done.send(Err(LinkError::from(e)));
                }
            },
            Ev::Read(Err(e)) => return GwError::Bridge(e),
            Ev::Read(Ok(())) => {
                // Bounded by the radio's ring.
                while let Ok(Some((bytes, src))) = radio.try_receive(&mut raw) {
                    let frame = bytes.to_vec();
                    let queued = if middleware::rpc::is_rpc(&frame) {
                        mr.try_send((src, frame)).is_ok()
                    } else {
                        mw.send(src, frame)
                    };
                    if !queued {
                        bump(&counters.dropped);
                    }
                }
                if let Some(ok) = radio.sent_outcome()
                    && let Some((done, _)) = inflight.take()
                {
                    let _ = done.send(if ok { Ok(()) } else { Err(LinkError::NotAcked) });
                }
            }
            Ev::SentTimeout => {
                radio.abandon_send();
                if let Some((done, _)) = inflight.take() {
                    let _ = done.send(Err(LinkError::SentTimeout));
                }
            }
            Ev::Tick => {
                if let Some(s) = radio.bridge_stats() {
                    tracing::debug!(
                        rx = s.rx,
                        rx_dropped = s.rx_dropped,
                        tx_ok = s.tx_ok,
                        tx_fail = s.tx_fail,
                        tx_dropped = s.tx_dropped,
                        serial_bad = s.serial_bad,
                        "espnow bridge counters"
                    );
                }
                if let Err(e @ (BridgeIoError::Io(_) | BridgeIoError::Closed)) = radio.request_stats().await {
                    return GwError::Bridge(e.to_string());
                }
            }
        }
    }
}

/// A query for the requests loop, from the queryable of `mac` for `key`.
struct Ask {
    arrived: Instant,
    mac: Mac,
    key: Key<'static>,
    query: Query,
}

/// A `sys/info` to fetch into the cache: for a scan the cache could not
/// answer (`scan`: its query and how long it is worth answering), or for a
/// device newly served (`None`).
struct Miss {
    mac: Mac,
    scan: Option<(Instant, Query)>,
}

/// The devices' cached `sys/info`, shared by the queryables (which answer
/// scans from it), the requests loop (which fills it) and the bus (which
/// drops a forgotten device's entry).
type Cache = Arc<Mutex<InfoCache>>;

fn elapsed_ms(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn cached(cache: &Cache, mac: Mac, start: Instant) -> Option<Vec<u8>> {
    cache.lock().ok()?.get(mac, elapsed_ms(start)).map(<[u8]>::to_vec)
}

/// The requests loop (EG9, EG14, EG15): one radio request at a time.
/// Forwarded queries come first; `sys/info` fetches for the cache run only
/// when no query waits, each a single attempt, so a fetch delays a query
/// by at most one [`Retry::timeout_ms`] (and the wait for the bridge's
/// `Sent`). Fetches a scan waits for go before those nobody waits for.
struct Requests {
    queries: mpsc::Receiver<Ask>,
    misses: mpsc::Receiver<Miss>,
    cmds: mpsc::Sender<Cmd>,
    mr: mpsc::Receiver<Frame>,
    bridge: Mac,
    retry: Retry,
    cache: Cache,
    known: Known,
    start: Instant,
    next_id: u16,
    /// Scans waiting for a fetch: device, deadline, query. Every entry has
    /// its device in `fetches`.
    waiting: Vec<(Mac, Instant, Query)>,
    /// Devices to fetch, each once; `true` for a newly served device
    /// (fetched even if no scan waits).
    fetches: std::collections::VecDeque<(Mac, bool)>,
}

impl Requests {
    /// One request to `mac`, resent by `retry`.
    async fn ask(
        &mut self,
        mac: Mac,
        key: Key<'_>,
        body: &[u8],
        options: &RequestOptions<'_>,
        retry: Retry,
    ) -> Result<Vec<u8>, HandlerError> {
        let radio = MrHandle { cmds: &self.cmds, rx: &mut self.mr };
        let mut requester =
            RpcRequester::new(radio, TokioDelay, mac, retry, self.next_id).with_own_address(self.bridge);
        self.next_id = middleware::rpc::next_id(self.next_id);
        let mut resp = [0u8; middleware::rpc::MAX_RESPONSE_BODY];
        let answer = requester.request_with(key, body, &mut resp, options).await;
        answer.map(<[u8]>::to_vec).map_err(|e| reply_error_for(&e))
    }

    /// Remembers what `mac` answered on `sys/info`, unless it was forgotten
    /// meanwhile. The cache is locked before the heard set is read and the
    /// bus drops the MAC from that set before the cache, so an entry never
    /// outlives its device being heard.
    fn remember(&self, mac: Mac, body: &[u8]) {
        if let Ok(mut c) = self.cache.lock()
            && self.known.lock().is_ok_and(|k| k.contains(&mac))
        {
            c.insert(mac, body, elapsed_ms(self.start));
        }
    }

    /// A query on a device's own key: sent to the device, the answer
    /// relayed on that key.
    async fn forward(&mut self, ask: Ask, counters: &Counters) {
        let Ask { arrived, mac, key, query } = ask;
        let id = sender_id(mac);
        bump(&counters.requests);
        let body = query.payload().map(|p| p.to_bytes().into_owned()).unwrap_or_default();
        let attachment = query.attachment().map(|a| a.to_bytes().into_owned());
        let waited = u64::try_from(arrived.elapsed().as_millis()).unwrap_or(u64::MAX);
        let meta = request_meta(attachment.as_deref(), waited);
        let mut options = RequestOptions::default();
        if let Some(ms) = meta.deadline_ms {
            options = options.with_timeout(Duration::from_millis(u64::from(ms)));
        }
        let answer = self.ask(mac, key, &body, &options, self.retry).await;
        // Whatever the outcome, the device's payload state may have
        // changed: the next scan asks it again.
        if core::changes_info(key)
            && let Ok(mut c) = self.cache.lock()
        {
            c.forget(mac);
        }
        let sent = match answer {
            Ok(r) => {
                if key == keys::INFO {
                    self.remember(mac, &r);
                }
                query.reply(wire::node_key(id, key), r).attachment(wire::encode_reply(id).to_vec()).await
            }
            Err(e) => {
                bump(&counters.request_failures);
                query.reply_err(wire::encode_reply_error(e, id).to_vec()).await
            }
        };
        // A failed reply only means this querier misses the answer.
        let _ = sent;
    }

    /// Queues the fetch a [`Miss`] asks for, once per device, and keeps
    /// its scan until the fetch returns.
    async fn park(&mut self, miss: Miss) {
        let Miss { mac, scan } = miss;
        let warm = scan.is_none();
        if let Some((deadline, query)) = scan {
            // Fetched since the queryable looked.
            if let Some(body) = cached(&self.cache, mac, self.start) {
                reply_info(&query, mac, body).await;
                return;
            }
            if self.waiting.len() >= SCAN_QUEUE {
                return;
            }
            self.waiting.push((mac, deadline, query));
        }
        let queued = self.fetches.iter().position(|(m, _)| *m == mac);
        if warm {
            if queued.is_none() {
                self.fetches.push_back((mac, true));
            }
        } else {
            // A scan is waiting: ahead of the fetches nobody waits for.
            let was_warm = queued.and_then(|i| self.fetches.remove(i)).is_some_and(|(_, w)| w);
            let at = self.fetches.iter().position(|(_, w)| *w).unwrap_or(self.fetches.len());
            self.fetches.insert(at, (mac, was_warm));
        }
    }

    /// Fetches `mac`'s `sys/info` once (one attempt) and answers the scans
    /// waiting for it. Skipped when the device was forgotten, when the
    /// cache was filled meanwhile, or when nobody waits any more.
    async fn fetch(&mut self, mac: Mac, warm: bool) {
        let mut scans = Vec::new();
        let mut i = 0;
        while i < self.waiting.len() {
            if self.waiting.get(i).is_some_and(|w| w.0 == mac) {
                scans.push(self.waiting.swap_remove(i));
            } else {
                i += 1;
            }
        }
        let now = Instant::now();
        let wanted = warm || scans.iter().any(|(_, deadline, _)| *deadline > now);
        if !wanted || !self.known.lock().is_ok_and(|k| k.contains(&mac)) {
            return;
        }
        let answer = match cached(&self.cache, mac, self.start) {
            Some(body) => Ok(body),
            None => {
                let once = Retry { attempts: 1, ..self.retry };
                let r = self.ask(mac, keys::INFO, &[], &RequestOptions::default(), once).await;
                if let Ok(body) = &r {
                    self.remember(mac, body);
                }
                r
            }
        };
        for (_, _, query) in scans {
            match &answer {
                Ok(body) => reply_info(&query, mac, body.clone()).await,
                Err(e) => {
                    let _ = query.reply_err(wire::encode_reply_error(*e, sender_id(mac)).to_vec()).await;
                }
            }
        }
    }

    async fn run(mut self, counters: &Counters) {
        // Ends when the queryables are all gone.
        loop {
            while let Ok(miss) = self.misses.try_recv() {
                self.park(miss).await;
            }
            match self.queries.try_recv() {
                Ok(ask) => {
                    self.forward(ask, counters).await;
                    continue;
                }
                Err(mpsc::error::TryRecvError::Disconnected) => return,
                Err(mpsc::error::TryRecvError::Empty) => {}
            }
            if let Some((mac, warm)) = self.fetches.pop_front() {
                self.fetch(mac, warm).await;
                continue;
            }
            tokio::select! {
                biased;
                ask = self.queries.recv() => match ask {
                    Some(ask) => self.forward(ask, counters).await,
                    None => return,
                },
                miss = self.misses.recv() => match miss {
                    Some(miss) => self.park(miss).await,
                    None => return,
                },
            }
        }
    }
}

/// Answers a scan with `mac`'s `sys/info`, on the device's own key (never
/// the query's expression: a scanner reads the node id from the key).
async fn reply_info(query: &Query, mac: Mac, body: Vec<u8>) {
    let id = sender_id(mac);
    // A failed reply only means this querier misses the answer.
    let _ = query.reply(wire::node_key(id, keys::INFO), body).attachment(wire::encode_reply(id).to_vec()).await;
}

/// The per-device queryables, reconciled with [`Heard::served`].
struct Served {
    session: zenoh::Session,
    queries: mpsc::Sender<Ask>,
    misses: mpsc::Sender<Miss>,
    cache: Cache,
    start: Instant,
    declared: HashMap<Mac, Vec<Queryable<()>>>,
}

impl Served {
    async fn reconcile(&mut self, heard: &Heard) -> Result<(), GwError> {
        let want: HashSet<Mac> = heard.served().into_iter().collect();
        let gone: Vec<Mac> = self.declared.keys().filter(|m| !want.contains(*m)).copied().collect();
        for mac in gone {
            for q in self.declared.remove(&mac).unwrap_or_default() {
                let _ = q.undeclare().await;
            }
        }
        for mac in want {
            if self.declared.contains_key(&mac) {
                continue;
            }
            // Its sys/info, for the scans to come (EG15). Full: the first
            // scan fetches it.
            let _ = self.misses.try_send(Miss { mac, scan: None });
            let id = sender_id(mac);
            let mut qs = Vec::new();
            for key in core::FORWARDED {
                let tx = self.queries.clone();
                let misses = self.misses.clone();
                let cache = self.cache.clone();
                let start = self.start;
                let q = self
                    .session
                    .declare_queryable(wire::node_key(id, key))
                    .callback(move |q| match plan_query(q.key_expr().as_str(), mac, key) {
                        Plan::Ignore => {}
                        Plan::Forward => {
                            // Full: the query goes unanswered and times out.
                            let _ = tx.try_send(Ask { arrived: Instant::now(), mac, key, query: q });
                        }
                        Plan::Info => match cached(&cache, mac, start) {
                            Some(body) => {
                                let _ = q
                                    .reply(wire::node_key(id, keys::INFO), body)
                                    .attachment(wire::encode_reply(id).to_vec())
                                    .wait();
                            }
                            None => {
                                let attachment = q.attachment().map(|a| a.to_bytes().into_owned());
                                let hold = request_meta(attachment.as_deref(), 0)
                                    .deadline_ms
                                    .map_or(SCAN_HOLD, |ms| SCAN_HOLD.min(Duration::from_millis(u64::from(ms))));
                                // Full: this scan misses the device.
                                let _ = misses.try_send(Miss { mac, scan: Some((Instant::now() + hold, q)) });
                            }
                        },
                    })
                    .await
                    .map_err(|e| GwError::Zenoh(e.to_string()))?;
                qs.push(q);
            }
            self.declared.insert(mac, qs);
        }
        Ok(())
    }

    async fn clear(&mut self) {
        for (_, qs) in self.declared.drain() {
            for q in qs {
                let _ = q.undeclare().await;
            }
        }
    }
}

/// The bus's state.
struct Bus {
    session: ZenohTransport,
    discovery: ZenohDiscovery,
    registry: Registry,
    heard: Heard,
    /// [`Self::heard`]'s MACs, shared with the pump.
    known: Known,
    served: Served,
    streams: HashMap<(Mac, String), Sequence>,
    peers: Arc<Mutex<Vec<u8>>>,
    heard_key: String,
    start: Instant,
    /// The heard devices changed since the last tick.
    changed: bool,
}

impl Bus {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    async fn announce(&mut self, rec: &ServiceRecord, counters: &Counters) {
        if self.discovery.announce(&rec.service()).await.is_ok() {
            bump(&counters.beacons);
        }
    }

    async fn withdraw(&mut self, recs: Vec<ServiceRecord>) {
        for rec in recs {
            let s = rec.service();
            let _ = self.discovery.withdraw(s.kind, s.instance).await;
        }
    }

    /// Forgets `macs` (no longer heard): their records are withdrawn.
    async fn forget(&mut self, macs: &[Mac]) {
        if let Ok(mut k) = self.known.lock() {
            for mac in macs {
                k.remove(mac);
            }
        }
        // After `known`: see `Requests::remember`.
        if let Ok(mut c) = self.served.cache.lock() {
            for mac in macs {
                c.forget(*mac);
            }
        }
        for mac in macs {
            let recs = self.registry.forget(*mac);
            self.withdraw(recs).await;
        }
    }

    async fn put_heard(&mut self) {
        let mut out = [0u8; middleware::loader::peers::PEERS_MAX];
        let n = self.heard.peers().encode(&mut out).unwrap_or(0);
        let bytes = out.get(..n).unwrap_or_default().to_vec();
        if let Ok(mut p) = self.peers.lock() {
            p.clone_from(&bytes);
        }
        let _ = self.session.session().put(self.heard_key.as_str(), bytes).await;
    }

    /// One uplink sample from a device.
    async fn uplink(&mut self, key: &str, payload: &[u8], mac: Mac, counters: &Counters) -> Result<(), GwError> {
        let now = self.now_ms();
        let hearing = self.heard.hear(mac, now);
        if hearing.refused {
            // The table is full of devices heard recently: a new MAC is not
            // heard, so nothing it sends is republished or announced.
            bump(&counters.dropped);
            return Ok(());
        }
        if let Some(evicted) = hearing.evicted {
            self.forget(&[evicted]).await;
        }
        if hearing.new
            && let Ok(mut k) = self.known.lock()
        {
            k.insert(mac);
        }
        // Queryables and the heard list follow on the next tick, coalesced.
        self.changed |= hearing.new || hearing.evicted.is_some();
        let Ok(k) = Key::new(key) else { return Ok(()) };
        if KeyFilter::Prefix(BEACON_PREFIX).matches(k) {
            match self.registry.observe(mac, k, payload, now) {
                Action::Announce(rec) => self.announce(&rec, counters).await,
                Action::Withdraw(rec) => self.withdraw(vec![rec]).await,
                Action::Nothing | Action::Refused(_) => {}
            }
        } else if KeyFilter::Prefix(APP).matches(k) {
            if self.streams.len() >= MAX_STREAMS {
                self.streams.clear();
            }
            let seq = self.streams.entry((mac, key.to_owned())).or_default();
            let this = *seq;
            *seq = this.next();
            let attachment = wire::encode_pub(sender_id(mac), this, wire::stream_id(origin_of(mac), k));
            let put = self
                .session
                .session()
                .put(key, payload.to_vec())
                .attachment(attachment.to_vec())
                .congestion_control(CongestionControl::Block)
                .await;
            match put {
                Ok(()) => bump(&counters.frames_up),
                Err(_) => bump(&counters.dropped),
            }
        }
        Ok(())
    }

    /// The 1 s housekeeping: expired records and devices, then, if the
    /// heard devices changed since the last tick, the queryables and the
    /// heard list (coalesced: at most once a tick, however many frames).
    async fn tick(&mut self) -> Result<(), GwError> {
        let now = self.now_ms();
        let expired = self.registry.expire(now);
        self.withdraw(expired).await;
        let gone = self.heard.expire(now);
        if !gone.is_empty() {
            self.forget(&gone).await;
            self.changed = true;
        }
        if std::mem::take(&mut self.changed) {
            self.served.reconcile(&self.heard).await?;
            self.put_heard().await;
        }
        Ok(())
    }

    async fn shutdown(&mut self) {
        let recs = self.registry.records();
        self.withdraw(recs).await;
        self.served.clear().await;
    }
}

/// Runs the gateway over the bridge on `io` (`mw_gateway::io::open_serial`
/// at [`middleware::radio_bridge::BAUD`] on a host, a `ChannelIo` pair in
/// tests) and `session` until `stop` completes (`Ok`) or the bridge fails.
/// On the way out beacons are withdrawn and queryables undeclared.
///
/// Cancel safety of the radio relies on `io`'s `read` being cancel-safe
/// (`ChannelIo`'s is; see [`middleware::espnow::bridge`]).
pub async fn run<IO>(
    io: IO,
    session: ZenohTransport,
    cfg: GwConfig,
    stop: impl Future<Output = ()>,
) -> Result<Report, GwError>
where
    IO: Read + Write,
{
    let mut radio = BridgeRadio::<IO, TokioDelay>::new(io, TokioDelay).with_sent_timeout_ms(cfg.sent_timeout_ms);
    let hello = radio.connect(cfg.connect_timeout_ms).await.map_err(|e| GwError::Bridge(e.to_string()))?;
    let bridge = hello.mac;
    let bridge_id = sender_id(bridge);
    let hex = String::from_utf8_lossy(&wire::zid16_hex(bridge_id)).into_owned();
    tracing::info!(bridge = %hex, channel = hello.channel, "espnow bridge connected");

    let counters = Counters::default();
    let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>(4);
    let (mw_tx, mw_rx) = mpsc::channel::<Frame>(MW_QUEUE);
    let known = Known::default();
    let mw_tx = MwTx { tx: mw_tx, known: known.clone() };
    let (mr_tx, mr_rx) = mpsc::channel::<Frame>(MR_QUEUE);
    let (query_tx, query_rx) = mpsc::channel::<Ask>(QUERY_QUEUE);
    let (miss_tx, miss_rx) = mpsc::channel::<Miss>(SCAN_QUEUE);
    let cache = Cache::default();
    let start = Instant::now();
    let (failed_tx, failed_rx) = oneshot::channel::<GwError>();

    let peers = Arc::new(Mutex::new(vec![0u8]));
    let peers_q = peers.clone();
    let peers_key = wire::node_key(bridge_id, keys::PEERS);
    let peers_queryable = session
        .session()
        .declare_queryable(peers_key.clone())
        .callback(move |q| {
            let bytes = peers_q.lock().map(|p| p.clone()).unwrap_or_default();
            // On the gateway's own key, also for a wildcard query (`mwctl
            // scout` asks `@mw/node/*/sys/peers` and reads the gateway's
            // id from the reply's key).
            let _ = q.reply(peers_key.clone(), bytes).attachment(wire::encode_reply(bridge_id).to_vec()).wait();
        })
        .await
        .map_err(|e| GwError::Zenoh(e.to_string()))?;

    // Downlink: one subscriber per filter (remote publications only),
    // forwarded to the bus.
    let (down_tx, mut down_rx) = mpsc::channel::<Down>(MW_QUEUE);
    let mut forwarders = Vec::new();
    for f in &cfg.downlink {
        let mut z = session.clone();
        let mut sub = z.subscribe(f.as_filter()).await.map_err(|e| GwError::Zenoh(e.to_string()))?;
        let tx = down_tx.clone();
        forwarders.push(tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            // Ends when the subscriber or the bus goes.
            loop {
                let item = match sub.recv(&mut buf).await {
                    Ok(s) => (s.key.as_str().to_owned(), s.payload.to_vec(), s.meta.sender),
                    Err(middleware::zenoh::ZenohError::Closed) => break,
                    Err(_) => continue,
                };
                if tx.send(item).await.is_err() {
                    break;
                }
            }
        }));
    }
    drop(down_tx);

    let mut bus = Bus {
        discovery: session.discovery(),
        served: Served {
            session: session.session().clone(),
            queries: query_tx,
            misses: miss_tx,
            cache: cache.clone(),
            start,
            declared: HashMap::new(),
        },
        session,
        registry: Registry::new(),
        heard: Heard::with_ttl_ms(cfg.heard_ttl_ms),
        known: known.clone(),
        streams: HashMap::new(),
        peers,
        heard_key: format!("gw/espnow/{hex}/heard"),
        start,
        changed: false,
    };

    let pump_loop = async {
        let sent_timeout = Duration::from_millis(u64::from(cfg.sent_timeout_ms));
        let e = pump(radio, cmd_rx, mw_tx, mr_tx, sent_timeout, &counters).await;
        let _ = failed_tx.send(e);
        std::future::pending::<()>().await;
    };
    let requests = Requests {
        queries: query_rx,
        misses: miss_rx,
        cmds: cmd_tx.clone(),
        mr: mr_rx,
        bridge,
        retry: cfg.request_retry,
        cache,
        known,
        start,
        // A clock-derived first id, so a restarted gateway does not reuse
        // ids the devices have cached responses for.
        next_id: std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map_or(1, |d| (d.as_millis() & 0xFFFF) as u16),
        waiting: Vec::new(),
        fetches: std::collections::VecDeque::new(),
    };
    let requests_loop = requests.run(&counters);
    let bus_loop = async {
        enum Ev {
            Up(Option<Down>),
            Down(Option<Down>),
            Tick,
            Heard,
            Stop(Option<GwError>),
        }
        let mut transport: EspNowTransport<'static, RadioHalves<TxHandle, RxHandle>, 64, 4> =
            EspNowTransport::new(RadioHalves::new(TxHandle { cmds: cmd_tx }, RxHandle { rx: mw_rx }))
                .with_filters(&UPLINK)
                .map_err(|_| GwError::Bridge("too many filters".into()))?
                .with_own_address(bridge);
        let (mut tx, mut rx) = transport.split();
        let mut buf = [0u8; MAX_FRAME];
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut heard_tick = tokio::time::interval(cfg.heard_period);
        let mut failed = failed_rx;
        let mut stop = std::pin::pin!(stop);
        let result = loop {
            let ev = tokio::select! {
                _ = &mut stop => Ev::Stop(None),
                e = &mut failed => Ev::Stop(Some(e.unwrap_or(GwError::Bridge("pump ended".into())))),
                s = rx.recv(&mut buf) => Ev::Up(s.ok().map(|s| (s.key.as_str().to_owned(), s.payload.to_vec(), s.meta.sender))),
                d = down_rx.recv(), if !cfg.downlink.is_empty() => Ev::Down(d),
                _ = tick.tick() => Ev::Tick,
                _ = heard_tick.tick() => Ev::Heard,
            };
            match ev {
                Ev::Stop(None) => break Ok(()),
                Ev::Stop(Some(e)) => break Err(e),
                // A malformed `MW` frame, or a flood of foreign ones.
                Ev::Up(None) => bump(&counters.dropped),
                Ev::Up(Some((key, payload, sender))) => {
                    let Some(mac) = sender.and_then(mac_of) else { continue };
                    if let Err(e) = bus.uplink(&key, &payload, mac, &counters).await {
                        break Err(e);
                    }
                }
                Ev::Down(None) => {}
                Ev::Down(Some((key, payload, sender))) => {
                    if skip_downlink(sender, bridge, &bus.heard) {
                        continue;
                    }
                    let sent = match Key::new(&key) {
                        Ok(k) => tx.publish(k, &payload).await.is_ok(),
                        Err(_) => false,
                    };
                    bump(if sent { &counters.frames_down } else { &counters.dropped });
                }
                Ev::Tick => {
                    if let Err(e) = bus.tick().await {
                        break Err(e);
                    }
                }
                Ev::Heard => bus.put_heard().await,
            }
        };
        bus.shutdown().await;
        result
    };
    let result = tokio::select! {
        r = bus_loop => r,
        () = pump_loop => Ok(()),
        () = requests_loop => Ok(()),
    };
    for f in forwarders {
        f.abort();
    }
    let _ = peers_queryable.undeclare().await;
    result?;
    let load = |c: &AtomicU32| c.load(Ordering::Relaxed);
    Ok(Report {
        frames_up: load(&counters.frames_up),
        frames_down: load(&counters.frames_down),
        beacons: load(&counters.beacons),
        refused: bus.registry.refused(),
        dropped: load(&counters.dropped),
        requests: load(&counters.requests),
        request_failures: load(&counters.request_failures),
        heard: u32::try_from(bus.heard.len()).unwrap_or(u32::MAX),
        refused_macs: bus.heard.refused(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::radio_bridge::{MAX_WIRE, Msg, VERSION};
    use mw_gateway::io::pair;

    const BRIDGE: Mac = [0x5c, 0xcf, 0x7f, 0x72, 0xf6, 0x54];
    const DEV: Mac = [0xe4, 0xb0, 0x63, 0x81, 0xd3, 0x24];

    fn recv_wire(src: Mac, seq: u8) -> Vec<u8> {
        let mut frame = [0u8; MAX_FRAME];
        let f = middleware::espnow::Frame { seq, key: Key::from_static("app/x"), payload: b"x" };
        let n = f.encode(&mut frame).unwrap();
        let mut out = [0u8; MAX_WIRE];
        let n = Msg::Recv { src, data: &frame[..n] }.encode_wire(&mut out).unwrap();
        out[..n].to_vec()
    }

    /// EG10: while the bus is stalled (a Zenoh put blocked, a reconcile
    /// declaring hundreds of queryables), a flood of forged MACs fills the
    /// `MW` queue; a frame from a device already heard still reaches the
    /// bus, it is not dropped behind the flood.
    #[tokio::test]
    async fn eg10_a_flood_does_not_crowd_out_a_heard_device() {
        let (gw_io, mut bridge_io) = pair();
        let mut hello = [0u8; MAX_WIRE];
        let n = Msg::Hello { mac: BRIDGE, channel: 1, version: VERSION }.encode_wire(&mut hello).unwrap();
        bridge_io.write_all(&hello[..n]).await.unwrap();
        let mut radio = BridgeRadio::<_, TokioDelay>::new(gw_io, TokioDelay);
        radio.connect(2000).await.unwrap();
        let flood = 2 * MW_QUEUE;
        let mut air = Vec::new();
        for i in 0..flood {
            let b = u32::try_from(i).unwrap().to_be_bytes();
            air.extend(recv_wire([0x02, 0xee, b[0], b[1], b[2], b[3]], 0));
        }
        air.extend(recv_wire(DEV, 1));
        bridge_io.write_all(&air).await.unwrap();

        let (_cmd_tx, cmd_rx) = mpsc::channel::<Cmd>(4);
        let (mw_tx, mw_rx) = mpsc::channel::<Frame>(MW_QUEUE);
        let known = Known::default();
        known.lock().unwrap().insert(DEV);
        let mw = MwTx { tx: mw_tx, known };
        let (mr_tx, _mr_rx) = mpsc::channel::<Frame>(MR_QUEUE);
        let counters = Counters::default();
        let mut rx = RxHandle { rx: mw_rx };
        // The bus reads nothing until the pump has routed (or dropped)
        // every frame.
        let all_routed = async {
            let total = u32::try_from(flood + 1).unwrap();
            while u32::try_from(rx.rx.len()).unwrap() + counters.dropped.load(Ordering::Relaxed) < total {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::select! {
            _ = pump(radio, cmd_rx, mw, mr_tx, Duration::from_secs(1), &counters) => panic!("pump stopped"),
            r = tokio::time::timeout(Duration::from_secs(10), all_routed) => r.expect("every frame routed"),
        }
        // Only the flood was dropped, and only beyond the unreserved
        // slots; the device's frame is queued behind what was kept, in
        // order.
        let kept = MW_QUEUE - MW_RESERVED;
        assert_eq!(counters.dropped.load(Ordering::Relaxed), u32::try_from(flood - kept).unwrap());
        // The pump is gone: `Down` once the queue is drained.
        let mut from = Vec::new();
        let mut buf = [0u8; MAX_FRAME];
        while let Ok(Some((_, src))) = rx.try_receive(&mut buf) {
            from.push(src);
        }
        assert_eq!(from.len(), kept + 1);
        assert_eq!(from.last(), Some(&DEV), "the heard device's frame was dropped behind the flood");
    }
}
