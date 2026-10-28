//! The controller: owns the dispatcher state, the payload store and (with
//! the `wasm` feature) the running payload.
//!
//! It runs on the main task and alternates between two sessions:
//!
//! * **idle** — no payload runs; requests are dispatched and storage actions
//!   performed. A `sys/payload/start` ends the session.
//! * **running** (`wasm` only) — the stored image is read and CRC
//!   checked; if the free heap covers the payload's bound plus margin
//!   ([`esp_base::start`], the bound charging linear memory at the
//!   module's declared maximum; otherwise the start fails with the free
//!   and needed bytes on `sys/log` and the image buffer is freed), it is
//!   compiled, freed and the module instantiated into a [`Payload`]; the
//!   session then loops:
//!   1. drain the interrupt event queue ([`irq`]), at most `MAX_EVENTS`
//!      events per iteration, into `Payload::dispatch` (`on_irq`), and log
//!      on `sys/log` when the queue's overflow count has risen;
//!   2. wait for whichever comes first: an inbound message, the payload's
//!      next tick, a newly queued event ([`irq::WAKE`]), or SPI/UART work
//!      completing ([`EspDevice::io`], which queues `DmaDone` / `UartRx`).
//!
//!   Every iteration first yields to the executor once: a busy payload
//!   (say, a decoder whose 1 ms timer has always fired again by the time
//!   its call returns) keeps one of those waits ready, and without the
//!   yield the serial link tasks, which carry the start reply, and the
//!   radio task would never run.
//!
//!   With [`esp_base::perf::ENABLED`] every payload operation (delivery,
//!   `on_irq`, `tick`) is timed with the microsecond clock and its fuel
//!   read from [`Payload::fuel_stats`], as are the wait and the yield;
//!   every [`esp_base::perf::REPORT_INTERVAL_US`] the window is logged on
//!   `sys/log` as two `perf:` lines (busy time, fuel per second while busy,
//!   the split per operation kind, waiting, yielding, the radio task's
//!   time) and restarted.
//!
//!   Requests are dispatched as when idle. A publication (from a serial
//!   port, or with `espnow` a sample received over ESP-NOW) on the key of a
//!   sink route goes to that route ([`EspDevice::route_message`], which
//!   DMAs it to the display) and never to the payload; otherwise it is
//!   delivered if the payload subscribed to its key. A trap, `sys/payload/stop` or a new transfer
//!   (`sys/loader/begin`) ends the session and drops the payload, whose
//!   device stops timers, aborts DMA, disables interrupts, releases the
//!   pins and clears the queue.
//!
//! Trust: each request is dispatched with `Context::rekey_window` taken
//! from [`presence::window_open`]. A `Claim` action saves the trust record
//! ([`TrustStore::save`], on the flash the payload store lends) and closes
//! the window. An `Activate` that raises `min_version` saves the record
//! inside [`PayloadStore::activate_with`], after the image CRC check and
//! before the slot header (the ordering is explained in
//! `esp_base::trust_store`). Refused claims and commits are logged.
//!
//! All decisions come from `esp_base::dispatch`; this module performs them.
//! Status changes are published on `sys/payload/status`, notable events as
//! `sys/log` lines.

use esp_base::dispatch::{Action, Context, Outcome, State, handle_request};
use esp_base::store::{PayloadStore, StoreError};
use esp_base::trust_store::TrustStore;
use middleware::loader::keys;
use middleware::loader::trust::TrustState;
use esp_storage::FlashStorage;
use heapless::Vec;
use middleware::Key;
use middleware::transport::HandlerError;

use super::bus::{Bus, Inbound, Kind, MAX_REPLY, Port, Reply};
use super::presence;

#[cfg(feature = "wasm")]
use {
    super::device::{EspDevice, now_ms},
    super::irq,
    alloc::vec::Vec as AllocVec,
    embassy_futures::select::{Either, Either4, select, select4},
    embassy_futures::yield_now,
    middleware::wasm_abi::events::MAX_EVENTS,
    embassy_time::Timer,
    esp_base::chip::Profile,
    esp_base::dispatch::{route_publication, tick_delay},
    esp_base::perf::{self, CallKind, Cost, RadioTime, Window},
    esp_base::start::{HeapShort, check_heap, check_heap_holding},
    wasm_host::{Compiled, Limits, LoadError, Payload, Trap},
};

/// The flash-backed payload store.
pub type Store = PayloadStore<FlashStorage<'static>>;

/// What handling one request asks of the session.
struct Handled {
    /// Reply to send now (`None` when the reply waits for a payload start).
    reply: Option<Reply>,
    /// The payload must stop.
    #[cfg_attr(not(feature = "wasm"), allow(dead_code))]
    stop: bool,
}

/// Owner of the device's state.
pub struct Controller {
    state: State,
    ctx: Context<'static>,
    store: Option<Store>,
    /// The trust partition; `None` when the partition table has none.
    trust: Option<TrustStore>,
    bus: &'static Bus,
    #[cfg(feature = "wasm")]
    profile: &'static Profile,
}

#[cfg(feature = "wasm")]
enum StartError {
    Store(StoreError),
    /// The free heap is below the payload's bound plus margin.
    NoHeap(HeapShort),
    NoMemory,
    NoPins,
    Load(LoadError),
}

#[cfg(feature = "wasm")]
impl core::fmt::Display for StartError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            StartError::Store(e) => write!(f, "{e}"),
            StartError::NoHeap(short) => write!(f, "{short}"),
            StartError::NoMemory => f.write_str("not enough heap for the image"),
            StartError::NoPins => f.write_str("pins unavailable"),
            StartError::Load(e) => write!(f, "{e}"),
        }
    }
}

impl Controller {
    /// A controller starting from `state`.
    pub fn new(
        state: State,
        ctx: Context<'static>,
        store: Option<Store>,
        trust: Option<TrustStore>,
        bus: &'static Bus,
        #[cfg(feature = "wasm")] profile: &'static Profile,
    ) -> Self {
        Controller {
            state,
            ctx,
            store,
            trust,
            bus,
            #[cfg(feature = "wasm")]
            profile,
        }
    }

    /// Runs forever: starts a stored payload, then serves requests.
    pub async fn run(mut self) -> ! {
        self.bus.status(self.state.status());
        #[cfg(feature = "wasm")]
        let mut start = self.state.startable().then_some(None);
        loop {
            #[cfg(feature = "wasm")]
            if let Some(pending) = start.take() {
                self.running(pending).await;
            }
            let (port, id) = self.idle().await;
            #[cfg(feature = "wasm")]
            {
                start = Some(Some((port, id)));
            }
            #[cfg(not(feature = "wasm"))]
            self.bus.reply(port, Reply { id, result: Err(HandlerError::NotFound) });
        }
    }

    fn set_state(&mut self, next: State) {
        if next.status() != self.state.status() {
            self.bus.status(next.status());
        }
        self.state = next;
    }

    fn store(&mut self) -> Result<&mut Store, StoreError> {
        self.store.as_mut().ok_or(StoreError::NoImage)
    }

    fn storage(&mut self, outcome: &Outcome<'_>) -> Result<(), StoreError> {
        match outcome.action {
            Action::Erase { len } => self.store()?.erase(len),
            Action::Write { offset, data } => self.store()?.write(offset, data),
            Action::Activate { len, crc, trust: None } => self.store()?.activate(len, crc),
            Action::Activate { len, crc, trust: Some(next) } => {
                let (Some(store), Some(ts)) = (self.store.as_mut(), self.trust.as_mut()) else {
                    return Err(StoreError::Flash);
                };
                store.activate_with(len, crc, |flash| ts.save(flash, &next))
            }
            Action::Claim { trust } => {
                let (Some(store), Some(ts)) = (self.store.as_mut(), self.trust.as_mut()) else {
                    return Err(StoreError::Flash);
                };
                ts.save(store.flash(), &trust)
            }
            Action::None | Action::Start | Action::Stop => Ok(()),
        }
    }

    /// Logs what a trust-related request did.
    fn log_trust(&self, key: &str, outcome: &Outcome<'_>) {
        if let Action::Claim { trust: TrustState::Claimed { key, .. } } = outcome.action {
            let k = key.0;
            self.bus.log(format_args!(
                "trust: key {:02x}{:02x}{:02x}{:02x}... pinned, min_version 0; re-key window closed",
                k[0], k[1], k[2], k[3]
            ));
        }
        if let Action::Activate { trust: Some(TrustState::Claimed { min_version, .. }), .. } = outcome.action {
            self.bus.log(format_args!("trust: signed payload accepted, min_version {}", min_version.0));
        }
        if outcome.reply == Err(HandlerError::Denied) {
            if key == keys::COMMIT.as_str() {
                let min = match self.state.trust() {
                    TrustState::Claimed { min_version, .. } => min_version.0,
                    TrustState::Unclaimed => 0,
                };
                self.bus.log(format_args!(
                    "trust: commit denied (needs a valid signature and version > {min}); previous payload kept"
                ));
            } else {
                self.bus.log(format_args!(
                    "trust: claim denied; hold BOOT 3 s to open the re-key window"
                ));
            }
        }
    }

    /// Dispatches one request and performs its storage action. `None` reply
    /// means a start was requested.
    fn handle(&mut self, id: u8, key: &str, body: &[u8]) -> Handled {
        let Ok(key) = Key::new(key) else {
            let reply = Reply { id, result: Err(HandlerError::InvalidRequest) };
            return Handled { reply: Some(reply), stop: false };
        };
        let mut out = [0u8; MAX_REPLY];
        let ctx = Context { rekey_window: presence::window_open(), ..self.ctx };
        let outcome = handle_request(self.state, &ctx, key, body, &mut out);
        if outcome.action == Action::Start {
            return Handled { reply: None, stop: false };
        }
        let stop = outcome.action.stops_payload();
        let result = match self.storage(&outcome) {
            Ok(()) => {
                if let Action::Activate { len, .. } = outcome.action {
                    self.bus.log(format_args!("payload installed ({len} bytes)"));
                }
                if let Action::Claim { .. } = outcome.action {
                    presence::close();
                }
                self.log_trust(key.as_str(), &outcome);
                self.set_state(outcome.state);
                outcome.reply
            }
            Err(e) => {
                self.bus.log(format_args!("storage: {e}"));
                // The trust record may already be on flash (saved before
                // the slot header): trust what a reboot would load, never
                // an older min_version.
                let failed = self.state.storage_failed();
                let failed = match self.trust.as_ref() {
                    Some(ts) => failed.with_trust(ts.persisted()),
                    None => failed,
                };
                self.set_state(failed);
                Err(HandlerError::Internal)
            }
        };
        let result = result.and_then(|n| {
            out.get(..n).and_then(|b| Vec::from_slice(b).ok()).ok_or(HandlerError::BufferTooSmall)
        });
        Handled { reply: Some(Reply { id, result }), stop }
    }

    /// Serves requests until a payload start is requested; returns the
    /// requester.
    async fn idle(&mut self) -> (Port, u8) {
        loop {
            let msg: Inbound = self.bus.receive_inbound().await;
            if let Kind::Request { id } = msg.kind {
                match self.handle(id, &msg.key, &msg.body).reply {
                    Some(reply) => self.bus.reply(msg.port, reply),
                    None => return (msg.port, id),
                }
            }
        }
    }

    /// Loads the stored payload in two steps, compile then instantiate,
    /// freeing the image buffer between them. Refuses, with nothing left
    /// allocated, if the free heap is below the payload's bound plus
    /// [`esp_base::start::HEAP_MARGIN`]: first if it cannot even hold the
    /// image, then, with the image read (the bound charges linear memory at
    /// the module's declared maximum, [`Limits::heap_needed`]), if the heap
    /// free before the read is below the whole bound. Translation is eager,
    /// so once this succeeds the payload's later calls stay within heap
    /// that was free here.
    #[cfg(feature = "wasm")]
    fn load(&mut self) -> Result<Payload<EspDevice>, StartError> {
        let limits = Limits::bytes(self.profile.max_memory_bytes)
            .with_fuel(middleware::wasm_abi::FUEL_PER_CALL);
        let store = self.store().map_err(StartError::Store)?;
        let header = store.header().ok_or(StartError::Store(StoreError::NoImage))?;
        check_heap(esp_alloc::HEAP.free(), header.len).map_err(StartError::NoHeap)?;
        let len = header.len as usize;
        let mut image = AllocVec::new();
        image.try_reserve_exact(len).map_err(|_| StartError::NoMemory)?;
        image.resize(len, 0u8);
        store.load(header, &mut image).map_err(StartError::Store)?;
        let bound = limits.heap_needed(&image);
        let needed =
            check_heap_holding(esp_alloc::HEAP.free(), image.capacity(), bound).map_err(StartError::NoHeap)?;
        let compiled = Compiled::new(&image, limits).map_err(StartError::Load)?;
        drop(image);
        let device = EspDevice::new(self.profile, self.bus).ok_or(StartError::NoPins)?;
        let payload = Payload::instantiate(compiled, device).map_err(StartError::Load)?;
        self.bus.log(format_args!(
            "payload loaded ({} bytes): heap {} B free, needed at most {needed} B",
            header.len,
            esp_alloc::HEAP.free(),
        ));
        Ok(payload)
    }

    /// Routes one publication: to the sink route on its key, else to the
    /// payload if subscribed. Returns what the delivery cost the payload
    /// ([`Cost::ZERO`] when it was not delivered).
    #[cfg(feature = "wasm")]
    async fn publication(&mut self, payload: &mut Payload<EspDevice>, key: &str, body: &[u8]) -> Result<Cost, Trap> {
        let Ok(key) = Key::new(key) else { return Ok(Cost::ZERO) };
        if let Some(handle) = payload.device().sink_for(key) {
            payload.device().route_message(handle, body).await;
            return Ok(Cost::ZERO);
        }
        if route_publication(&self.state, key, payload.subscriptions()) {
            let (delivered, cost) = timed(payload, |p| p.deliver(key, body));
            delivered?;
            return Ok(cost);
        }
        Ok(Cost::ZERO)
    }

    /// Logs and restarts `window` when it is due ([`perf::ENABLED`] only).
    #[cfg(feature = "wasm")]
    fn report(&self, window: Window) -> Window {
        let now = irq::now_us();
        if !perf::ENABLED || !window.due(now) {
            return window;
        }
        let summary = window.summary(now, radio_time());
        self.bus.log(format_args!("{}", summary.wasm_line()));
        self.bus.log(format_args!("{}", summary.host_line()));
        Window::new(now)
    }

    #[cfg(feature = "wasm")]
    fn trapped(&mut self, trap: &Trap) {
        self.bus.log(format_args!("payload trapped: {trap}"));
        self.set_state(self.state.trapped());
    }

    /// Loads and runs the payload until it traps or must stop. `pending` is
    /// the start request to answer once the load has succeeded or failed.
    #[cfg(feature = "wasm")]
    async fn running(&mut self, pending: Option<(Port, u8)>) {
        let answer = |bus: &Bus, result: Result<(), HandlerError>| {
            if let Some((port, id)) = pending {
                bus.reply(port, Reply { id, result: result.map(|()| Vec::new()) });
            }
        };
        let mut payload = match self.load() {
            Ok(p) => p,
            Err(e) => {
                self.bus.log(format_args!("payload start failed: {e}"));
                let next = match e {
                    StartError::Store(_) => self.state.storage_failed(),
                    _ => self.state.trapped(),
                };
                self.set_state(next);
                answer(self.bus, Err(HandlerError::Internal));
                return;
            }
        };
        self.set_state(self.state.started());
        answer(self.bus, Ok(()));
        #[cfg(feature = "espnow")]
        super::radio::clear();
        // Start the radio's count with the window.
        let _ = radio_time();
        let mut window = Window::new(irq::now_us());
        loop {
            // Let every other task run once per iteration. The waits below
            // are often ready at once (a payload's timer that fired during
            // a long call, a full radio queue), and an embassy task that
            // never sees a pending future never returns to the executor:
            // the serial links (and the start reply) and the radio would
            // starve for as long as the payload stays busy.
            let t = irq::now_us();
            yield_now().await;
            window = window.yielded(irq::now_us().saturating_sub(t));
            window = self.report(window);
            for _ in 0..MAX_EVENTS {
                let Some(event) = irq::pop() else { break };
                let (result, cost) = timed(&mut payload, |p| p.dispatch(event));
                window = window.op(CallKind::Irq, cost);
                if let Err(trap) = result {
                    self.trapped(&trap);
                    return;
                }
            }
            if let Some(n) = irq::overflow_report() {
                self.bus.log(format_args!("irq queue full: {n} events dropped"));
            }
            let delay = payload.next_tick().map(|due| tick_delay(now_ms(), due));
            let tick = async move {
                match delay {
                    Some(ms) => Timer::after_millis(u64::from(ms)).await,
                    None => core::future::pending::<()>().await,
                }
            };
            let inbound = select(self.bus.receive_inbound(), radio_sample());
            let t = irq::now_us();
            let woke = select4(inbound, tick, irq::WAKE.wait(), payload.device().io()).await;
            window = window.waited(irq::now_us().saturating_sub(t));
            match woke {
                Either4::Third(()) | Either4::Fourth(()) => {}
                Either4::Second(()) => {
                    let (result, cost) = timed(&mut payload, |p| p.tick(now_ms()));
                    window = window.op(CallKind::Tick, cost);
                    if let Err(trap) = result {
                        self.trapped(&trap);
                        return;
                    }
                }
                Either4::First(Either::Second((key, body))) => {
                    match self.publication(&mut payload, &key, &body).await {
                        Ok(cost) => window = window.op(CallKind::Message, cost),
                        Err(trap) => {
                            self.trapped(&trap);
                            return;
                        }
                    }
                }
                Either4::First(Either::First(msg)) => match msg.kind {
                    Kind::Publish => match self.publication(&mut payload, &msg.key, &msg.body).await {
                        Ok(cost) => window = window.op(CallKind::Message, cost),
                        Err(trap) => {
                            self.trapped(&trap);
                            return;
                        }
                    },
                    Kind::Request { id } => {
                        let handled = self.handle(id, &msg.key, &msg.body);
                        // A start while running is answered by dispatch, so
                        // a reply is always present here.
                        let reply = handled.reply.unwrap_or(Reply { id, result: Ok(Vec::new()) });
                        self.bus.reply(msg.port, reply);
                        if handled.stop {
                            self.bus.log(format_args!("payload stopped"));
                            return;
                        }
                    }
                },
            }
        }
    }
}

/// Runs one payload operation; returns its result and what it cost (export
/// calls and fuel from [`Payload::fuel_stats`], wall time from the
/// microsecond clock).
#[cfg(feature = "wasm")]
fn timed<R>(payload: &mut Payload<EspDevice>, op: impl FnOnce(&mut Payload<EspDevice>) -> R) -> (R, Cost) {
    let before = payload.fuel_stats();
    let start = irq::now_us();
    let result = op(payload);
    let end = irq::now_us();
    let after = payload.fuel_stats();
    let cost = Cost::between((before.calls, before.total), (after.calls, after.total), start, end);
    (result, cost)
}

/// The radio task's time on received frames since the last call.
#[cfg(feature = "espnow")]
fn radio_time() -> RadioTime {
    super::radio::take_time()
}

/// Without `espnow` there is no radio task.
#[cfg(all(feature = "wasm", not(feature = "espnow")))]
fn radio_time() -> RadioTime {
    RadioTime::ZERO
}

/// The next sample received over ESP-NOW, as key and body.
#[cfg(feature = "espnow")]
async fn radio_sample() -> (heapless::String<{ esp_base::radio::KEY_CAPACITY }>, Vec<u8, { esp_base::radio::RX_PAYLOAD }>) {
    let r = super::radio::receive().await;
    (r.key, r.payload)
}

/// Without `espnow` nothing is ever received by radio.
#[cfg(all(feature = "wasm", not(feature = "espnow")))]
async fn radio_sample() -> (heapless::String<1>, Vec<u8, 1>) {
    core::future::pending().await
}
