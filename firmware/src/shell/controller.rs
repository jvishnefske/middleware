//! The controller: owns the dispatcher state, the payload store and (with
//! the `wasm` feature) the running payload.
//!
//! It runs on the main task and alternates between two sessions:
//!
//! * **idle** — no payload runs; requests are dispatched and storage actions
//!   performed. A `sys/payload/start` ends the session.
//! * **running** (`wasm` only) — the stored image is read, CRC checked and
//!   loaded into a [`Payload`]; the session then loops:
//!   1. drain the interrupt event queue ([`irq`]), at most `MAX_EVENTS`
//!      events per iteration, into `Payload::dispatch` (`on_irq`), and log
//!      on `sys/log` when the queue's overflow count has risen;
//!   2. wait for whichever comes first: an inbound message, the payload's
//!      next tick, a newly queued event ([`irq::WAKE`]), or SPI/UART work
//!      completing ([`EspDevice::io`], which queues `DmaDone` / `UartRx`).
//!
//!   Requests are dispatched as when idle; publications on subscribed keys
//!   are delivered. A trap, `sys/payload/stop` or a new transfer
//!   (`sys/loader/begin`) ends the session and drops the payload, whose
//!   device stops timers, aborts DMA, disables interrupts, releases the
//!   pins and clears the queue.
//!
//! All decisions come from `esp_base::dispatch`; this module performs them.
//! Status changes are published on `sys/payload/status`, notable events as
//! `sys/log` lines.

use esp_base::dispatch::{Action, Context, Outcome, State, handle_request};
use esp_base::store::{PayloadStore, StoreError};
use esp_storage::FlashStorage;
use heapless::Vec;
use middleware::Key;
use middleware::transport::HandlerError;

use super::bus::{Bus, Inbound, Kind, MAX_REPLY, Port, Reply};

#[cfg(feature = "wasm")]
use {
    super::device::{EspDevice, now_ms},
    super::irq,
    alloc::vec::Vec as AllocVec,
    embassy_futures::select::{Either4, select4},
    middleware::wasm_abi::events::MAX_EVENTS,
    embassy_time::Timer,
    esp_base::chip::Profile,
    esp_base::dispatch::{route_publication, tick_delay},
    wasm_host::{Limits, LoadError, Payload, Trap},
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
    bus: &'static Bus,
    #[cfg(feature = "wasm")]
    profile: &'static Profile,
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

impl Controller {
    /// A controller starting from `state`.
    pub fn new(
        state: State,
        ctx: Context<'static>,
        store: Option<Store>,
        bus: &'static Bus,
        #[cfg(feature = "wasm")] profile: &'static Profile,
    ) -> Self {
        Controller {
            state,
            ctx,
            store,
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
            Action::Activate { len, crc } => self.store()?.activate(len, crc),
            Action::None | Action::Start | Action::Stop => Ok(()),
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
        let outcome = handle_request(self.state, &self.ctx, key, body, &mut out);
        if outcome.action == Action::Start {
            return Handled { reply: None, stop: false };
        }
        let stop = outcome.action.stops_payload();
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

    #[cfg(feature = "wasm")]
    fn load(&mut self) -> Result<Payload<EspDevice>, StartError> {
        let store = self.store().map_err(StartError::Store)?;
        let header = store.header().ok_or(StartError::Store(StoreError::NoImage))?;
        let len = header.len as usize;
        let mut image = AllocVec::new();
        image.try_reserve_exact(len).map_err(|_| StartError::NoMemory)?;
        image.resize(len, 0u8);
        store.load(header, &mut image).map_err(StartError::Store)?;
        let device = EspDevice::new(self.profile, self.bus).ok_or(StartError::NoPins)?;
        let limits = Limits::bytes(self.profile.max_memory_bytes)
            .with_fuel(middleware::wasm_abi::FUEL_PER_CALL);
        Payload::load(&image, device, limits).map_err(StartError::Load)
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
        loop {
            for _ in 0..MAX_EVENTS {
                let Some(event) = irq::pop() else { break };
                if let Err(trap) = payload.dispatch(event) {
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
            match select4(self.bus.receive_inbound(), tick, irq::WAKE.wait(), payload.device().io()).await {
                Either4::Third(()) | Either4::Fourth(()) => {}
                Either4::Second(()) => {
                    if let Err(trap) = payload.tick(now_ms()) {
                        self.trapped(&trap);
                        return;
                    }
                }
                Either4::First(msg) => match msg.kind {
                    Kind::Publish => {
                        let Ok(key) = Key::new(&msg.key) else { continue };
                        if route_publication(&self.state, key, payload.subscriptions())
                            && let Err(trap) = payload.deliver(key, &msg.body)
                        {
                            self.trapped(&trap);
                            return;
                        }
                    }
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
