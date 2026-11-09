//! The controller: owns the dispatcher state, the payload store and (with
//! the `wasm` feature) the running payload.
//!
//! It runs on the shell's main task and alternates between two sessions:
//!
//! * **idle** — no payload runs; requests are dispatched and storage actions
//!   performed. A `sys/payload/start` ends the session.
//! * **running** (`wasm` only) — the stored image is read, CRC checked and
//!   loaded into a [`Payload`]; the session then loops:
//!   1. drain the board's interrupt event queue ([`EventSource::pop`]), at
//!      most `MAX_EVENTS` events per iteration, into `Payload::dispatch`
//!      (`on_irq`), and log on `sys/log` when the queue's overflow count has
//!      risen ([`EventSource::overflow_report`]);
//!   2. wait for whichever comes first: an inbound message, the payload's
//!      next tick, a newly queued event ([`EventSource::wake`]), or
//!      peripheral work completing ([`PayloadDevice::io`]).
//!
//!   Requests are dispatched as when idle; publications on subscribed keys
//!   are delivered. A trap, `sys/payload/stop` or a new transfer
//!   (`sys/loader/begin`) ends the session and drops the payload; the
//!   board's device releases its peripherals on drop.
//!
//! `sys/reboot` ends both sessions: dispatch asks for
//! [`Action::Reboot`] only for a mode the shell's [`Context::reboot`]
//! allows, the controller sends the empty `Ok` reply, waits until that
//! port's link task reports it flushed ([`Bus::wait_sent`], bounded by
//! [`FLUSH_TIMEOUT`]), drops any running payload and returns the mode from
//! [`Controller::run`]; the shell then performs the reboot.
//!
//! All decisions come from [`crate::dispatch`]; this module performs them.
//! Status changes are published on `sys/payload/status`, notable events as
//! `sys/log` lines.
//!
//! Generic over the payload flash `F` (any `embedded-storage`
//! [`NorFlash`], which includes `ReadNorFlash`), the port set `P` of `N`
//! ports, and with `wasm` the board's payload device `D`
//! ([`PayloadDevice`]) and interrupt event source `E` ([`EventSource`]).
//! The type has those two extra parameters only with `wasm`.

use embassy_time::{Duration, with_timeout};
use embedded_storage::nor_flash::NorFlash;
use heapless::Vec;
use middleware::Key;
use middleware::transport::HandlerError;

use super::bus::{Bus, Inbound, Kind, MAX_REPLY, Port, Reply};
use crate::dispatch::{Action, Context, Outcome, RebootMode, State, handle_request};
use crate::store::{PayloadStore, StoreError};

#[cfg(feature = "wasm")]
use {
    super::now_ms,
    crate::chip::Profile,
    crate::dispatch::{route_publication, tick_delay},
    alloc::vec::Vec as AllocVec,
    core::future::Future,
    core::marker::PhantomData,
    embassy_futures::select::{Either4, select4},
    embassy_time::Timer,
    middleware::wasm_abi::events::{Event, MAX_EVENTS},
    wasm_host::{Device, Limits, LoadError, Payload, Trap},
};

/// Longest the controller waits for a `sys/reboot` reply to be flushed
/// before rebooting anyway (twice the link's send timeout).
pub const FLUSH_TIMEOUT: Duration = Duration::from_millis(400);

/// The board's payload-facing device for one payload run.
///
/// The controller opens one per run with [`PayloadDevice::open`] and drops
/// it when the run ends; dropping must stop timers, abort transfers,
/// disable interrupts, release pins and clear the event queue.
#[cfg(feature = "wasm")]
pub trait PayloadDevice<P: Port, const N: usize>: Device + Sized + 'static {
    /// A device for one run; `None` while another device holds the pins.
    fn open(profile: &'static Profile, bus: &'static Bus<P, N>) -> Option<Self>;
    /// Completes peripheral work (SPI DMA, UART), queueing its events;
    /// pending forever when nothing is in flight. Must be cancel-safe: the
    /// controller drops it whenever another wake-up wins.
    fn io(&mut self) -> impl Future<Output = ()>;
}

/// The board's interrupt event queue, as the controller drains it.
///
/// ISRs push into it; the methods take `&self` because the queue is shared
/// with them (in practice a static behind a critical section).
#[cfg(feature = "wasm")]
pub trait EventSource {
    /// Removes the oldest event.
    fn pop(&self) -> Option<Event>;
    /// Events dropped since the last report, if any rose.
    fn overflow_report(&self) -> Option<u32>;
    /// Resolves when an event has been pushed since the last wake.
    fn wake(&self) -> impl Future<Output = ()>;
}

/// What handling one request asks of the session.
struct Handled {
    /// Reply to send now (`None` when the reply waits for a payload start).
    reply: Option<Reply>,
    /// The payload must stop.
    #[cfg_attr(not(feature = "wasm"), allow(dead_code))]
    stop: bool,
    /// Reboot into this mode once the reply is flushed.
    reboot: Option<RebootMode>,
}

/// Why the idle session ended.
enum Exit<P> {
    /// `sys/payload/start` from this port and request id.
    #[cfg_attr(not(feature = "wasm"), allow(dead_code))]
    Start(P, u8),
    /// `sys/reboot` answered and flushed.
    Reboot(RebootMode),
}

/// State shared by every build: dispatcher state, store and bus.
struct Core<F, P: 'static, const N: usize> {
    state: State,
    ctx: Context<'static>,
    store: Option<PayloadStore<F>>,
    bus: &'static Bus<P, N>,
}

/// Owner of the device's state (build without the WASM runtime).
#[cfg(not(feature = "wasm"))]
pub struct Controller<F, P: 'static, const N: usize> {
    core: Core<F, P, N>,
}

/// Owner of the device's state (build with the WASM runtime).
#[cfg(feature = "wasm")]
pub struct Controller<F, P: 'static, const N: usize, D, E> {
    core: Core<F, P, N>,
    profile: &'static Profile,
    events: E,
    device: PhantomData<fn() -> D>,
}

#[cfg(feature = "wasm")]
enum StartError {
    Store(StoreError),
    NoMemory,
    NoPins,
    Load(LoadError),
}

#[cfg(feature = "wasm")]
impl core::fmt::Display for StartError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            StartError::Store(e) => write!(f, "{e}"),
            StartError::NoMemory => f.write_str("not enough heap for the image"),
            StartError::NoPins => f.write_str("pins unavailable"),
            StartError::Load(e) => write!(f, "{e}"),
        }
    }
}

impl<F: NorFlash, P: Port, const N: usize> Core<F, P, N> {
    fn set_state(&mut self, next: State) {
        if next.status() != self.state.status() {
            self.bus.status(next.status());
        }
        self.state = next;
    }

    fn store(&mut self) -> Result<&mut PayloadStore<F>, StoreError> {
        self.store.as_mut().ok_or(StoreError::NoImage)
    }

    fn storage(&mut self, outcome: &Outcome<'_>) -> Result<(), StoreError> {
        match outcome.action {
            Action::Erase { len } => self.store()?.erase(len),
            Action::Write { offset, data } => self.store()?.write(offset, data),
            Action::Activate { len, crc } => self.store()?.activate(len, crc),
            Action::None | Action::Start | Action::Stop | Action::Reboot(_) => Ok(()),
        }
    }

    /// Dispatches one request and performs its storage action. `None` reply
    /// means a start was requested.
    fn handle(&mut self, id: u8, key: &str, body: &[u8]) -> Handled {
        let Ok(key) = Key::new(key) else {
            let reply = Reply { id, result: Err(HandlerError::InvalidRequest) };
            return Handled { reply: Some(reply), stop: false, reboot: None };
        };
        let mut out = [0u8; MAX_REPLY];
        let outcome = handle_request(self.state, &self.ctx, key, body, &mut out);
        if outcome.action == Action::Start {
            return Handled { reply: None, stop: false, reboot: None };
        }
        let stop = outcome.action.stops_payload();
        let reboot = match outcome.action {
            Action::Reboot(mode) => Some(mode),
            _ => None,
        };
        let result = match self.storage(&outcome) {
            Ok(()) => {
                if let Action::Activate { len, .. } = outcome.action {
                    self.bus.log(format_args!("payload installed ({len} bytes)"));
                }
                self.set_state(outcome.state);
                outcome.reply
            }
            Err(e) => {
                self.bus.log(format_args!("storage: {e}"));
                self.set_state(self.state.storage_failed());
                Err(HandlerError::Internal)
            }
        };
        let result = result.and_then(|n| {
            out.get(..n).and_then(|b| Vec::from_slice(b).ok()).ok_or(HandlerError::BufferTooSmall)
        });
        Handled { reply: Some(Reply { id, result }), stop, reboot }
    }

    /// Sends `reply` to `port`. For a reboot, first clears the port's sent
    /// notice, then waits (at most [`FLUSH_TIMEOUT`]) until the link task
    /// has flushed the reply, and returns the mode to reboot into.
    async fn respond(&self, port: P, reply: Reply, reboot: Option<RebootMode>) -> Option<RebootMode> {
        let id = reply.id;
        let Some(mode) = reboot else {
            self.bus.reply(port, reply);
            return None;
        };
        self.bus.log(format_args!("rebooting ({mode:?})"));
        self.bus.clear_sent(port);
        self.bus.reply(port, reply);
        let _ = with_timeout(FLUSH_TIMEOUT, self.bus.wait_sent(port, id)).await;
        Some(mode)
    }

    /// Serves requests until a payload start or a reboot is requested.
    async fn idle(&mut self) -> Exit<P> {
        loop {
            let msg: Inbound<P> = self.bus.receive_inbound().await;
            if let Kind::Request { id } = msg.kind {
                let handled = self.handle(id, &msg.key, &msg.body);
                match handled.reply {
                    Some(reply) => {
                        if let Some(mode) = self.respond(msg.port, reply, handled.reboot).await {
                            return Exit::Reboot(mode);
                        }
                    }
                    None => return Exit::Start(msg.port, id),
                }
            }
        }
    }
}

#[cfg(not(feature = "wasm"))]
impl<F: NorFlash, P: Port, const N: usize> Controller<F, P, N> {
    /// A controller starting from `state`, serving `ctx` in `sys/info`, on
    /// the payload `store` (`None`: loader disabled, storage actions fail).
    pub fn new(
        state: State,
        ctx: Context<'static>,
        store: Option<PayloadStore<F>>,
        bus: &'static Bus<P, N>,
    ) -> Self {
        Controller { core: Core { state, ctx, store, bus } }
    }

    /// Serves requests until `sys/reboot` has been answered and flushed;
    /// returns the requested mode for the shell to carry out. Without the
    /// runtime a start request (which dispatch already refuses when
    /// `ctx.runtime` is `false`) is answered `NotFound`.
    pub async fn run(mut self) -> RebootMode {
        self.core.bus.status(self.core.state.status());
        loop {
            match self.core.idle().await {
                Exit::Reboot(mode) => return mode,
                Exit::Start(port, id) => {
                    self.core.bus.reply(port, Reply { id, result: Err(HandlerError::NotFound) });
                }
            }
        }
    }
}

#[cfg(feature = "wasm")]
impl<F, P, const N: usize, D, E> Controller<F, P, N, D, E>
where
    F: NorFlash,
    P: Port,
    D: PayloadDevice<P, N>,
    E: EventSource,
{
    /// A controller starting from `state`, serving `ctx` in `sys/info`, on
    /// the payload `store` (`None`: loader disabled), sizing payloads by
    /// `profile` and draining interrupt events from `events`.
    pub fn new(
        state: State,
        ctx: Context<'static>,
        store: Option<PayloadStore<F>>,
        bus: &'static Bus<P, N>,
        profile: &'static Profile,
        events: E,
    ) -> Self {
        Controller { core: Core { state, ctx, store, bus }, profile, events, device: PhantomData }
    }

    /// Starts a stored payload, then serves requests until `sys/reboot` has
    /// been answered and flushed; returns the requested mode for the shell
    /// to carry out (any payload has been dropped by then).
    pub async fn run(mut self) -> RebootMode {
        self.core.bus.status(self.core.state.status());
        let mut start = self.core.state.startable().then_some(None);
        loop {
            if let Some(pending) = start.take()
                && let Some(mode) = self.running(pending).await
            {
                return mode;
            }
            match self.core.idle().await {
                Exit::Reboot(mode) => return mode,
                Exit::Start(port, id) => start = Some(Some((port, id))),
            }
        }
    }

    fn load(&mut self) -> Result<Payload<D>, StartError> {
        let profile = self.profile;
        let bus = self.core.bus;
        let store = self.core.store().map_err(StartError::Store)?;
        let header = store.header().ok_or(StartError::Store(StoreError::NoImage))?;
        let len = header.len as usize;
        let mut image = AllocVec::new();
        image.try_reserve_exact(len).map_err(|_| StartError::NoMemory)?;
        image.resize(len, 0u8);
        store.load(header, &mut image).map_err(StartError::Store)?;
        let device = D::open(profile, bus).ok_or(StartError::NoPins)?;
        let limits =
            Limits::bytes(profile.max_memory_bytes).with_fuel(middleware::wasm_abi::FUEL_PER_CALL);
        Payload::load(&image, device, limits).map_err(StartError::Load)
    }

    fn trapped(&mut self, trap: &Trap) {
        self.core.bus.log(format_args!("payload trapped: {trap}"));
        self.core.set_state(self.core.state.trapped());
    }

    /// Loads and runs the payload until it traps, must stop, or a reboot
    /// was answered (returned). `pending` is the start request to answer
    /// once the load has succeeded or failed.
    async fn running(&mut self, pending: Option<(P, u8)>) -> Option<RebootMode> {
        let bus = self.core.bus;
        let answer = |result: Result<(), HandlerError>| {
            if let Some((port, id)) = pending {
                bus.reply(port, Reply { id, result: result.map(|()| Vec::new()) });
            }
        };
        let mut payload = match self.load() {
            Ok(p) => p,
            Err(e) => {
                bus.log(format_args!("payload start failed: {e}"));
                let next = match e {
                    StartError::Store(_) => self.core.state.storage_failed(),
                    _ => self.core.state.trapped(),
                };
                self.core.set_state(next);
                answer(Err(HandlerError::Internal));
                return None;
            }
        };
        self.core.set_state(self.core.state.started());
        answer(Ok(()));
        loop {
            for _ in 0..MAX_EVENTS {
                let Some(event) = self.events.pop() else { break };
                if let Err(trap) = payload.dispatch(event) {
                    self.trapped(&trap);
                    return None;
                }
            }
            if let Some(n) = self.events.overflow_report() {
                bus.log(format_args!("irq queue full: {n} events dropped"));
            }
            let delay = payload.next_tick().map(|due| tick_delay(now_ms(), due));
            let tick = async move {
                match delay {
                    Some(ms) => Timer::after_millis(u64::from(ms)).await,
                    None => core::future::pending::<()>().await,
                }
            };
            match select4(bus.receive_inbound(), tick, self.events.wake(), payload.device().io()).await {
                Either4::Third(()) | Either4::Fourth(()) => {}
                Either4::Second(()) => {
                    if let Err(trap) = payload.tick(now_ms()) {
                        self.trapped(&trap);
                        return None;
                    }
                }
                Either4::First(msg) => match msg.kind {
                    Kind::Publish => {
                        let Ok(key) = Key::new(&msg.key) else { continue };
                        if route_publication(&self.core.state, key, payload.subscriptions())
                            && let Err(trap) = payload.deliver(key, &msg.body)
                        {
                            self.trapped(&trap);
                            return None;
                        }
                    }
                    Kind::Request { id } => {
                        let handled = self.core.handle(id, &msg.key, &msg.body);
                        // A start while running is answered by dispatch, so
                        // a reply is always present here.
                        let reply = handled.reply.unwrap_or(Reply { id, result: Ok(Vec::new()) });
                        if let Some(mode) = self.core.respond(msg.port, reply, handled.reboot).await {
                            return Some(mode);
                        }
                        if handled.stop {
                            bus.log(format_args!("payload stopped"));
                            return None;
                        }
                    }
                },
            }
        }
    }
}
