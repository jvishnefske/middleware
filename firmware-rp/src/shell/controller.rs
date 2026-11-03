//! The controller: owns the request service and (with `wasm`) the running
//! payload. It runs on the main task and alternates between two sessions,
//! as esp-base's controller does:
//!
//! * **idle** — no payload runs; each request goes to
//!   [`Service::handle`] and its reply back to the link. A request the
//!   service answers [`Reply::Start`] ends the session.
//! * **running** (`wasm` only) — the stored image is read, the heap check
//!   applied before and after reading it (`esp_base::start`: the payload's
//!   `Limits::heap_needed` plus margin must be free, else the start fails
//!   with the free and needed bytes on `sys/log`), the image compiled,
//!   freed, and the module instantiated against an [`RpDevice`]. The
//!   session then loops: yield once to the executor (so the USB tasks run
//!   however busy the payload is), fire due software timers and hand at
//!   most `MAX_EVENTS` queued events to `on_irq`, then wait for whichever
//!   comes first: an inbound message, the payload's next tick, or the next
//!   timer deadline. Publications go to the payload when it subscribed to
//!   the key (`esp_base::dispatch::route_publication`); requests are
//!   handled as when idle. A trap, `sys/payload/stop`, a `begin` or an
//!   activated commit ends the session and drops the payload, whose device
//!   returns the pins.
//!
//! Without `wasm` (the RP2040) the service refuses `sys/payload/start`
//! (`NotFound`), so the controller only ever runs the idle session.
//!
//! The service's notes are logged on `sys/log`; every status change is
//! published on `sys/payload/status`.

use heapless::Vec;
use middleware::Key;
use middleware::transport::HandlerError;
use rp_base::service::{Handled, Reply as Answer, Service};

use super::board::{Fw, StoreFlash};
use super::bus::{Bus, Inbound, Kind, MAX_REPLY, Reply};

#[cfg(feature = "wasm")]
use {
    super::board::PROFILE_REF,
    super::device::{RpDevice, now_ms, now_us},
    super::heap,
    alloc::vec::Vec as AllocVec,
    embassy_futures::select::{Either, select},
    embassy_futures::yield_now,
    embassy_time::{Duration, Instant, Timer},
    esp_base::dispatch::{route_publication, tick_delay},
    esp_base::periph::overflow_report,
    esp_base::start::{HeapShort, check_heap, check_heap_holding},
    esp_base::store::StoreError,
    middleware::wasm_abi::events::MAX_EVENTS,
    rp_base::service::StartFailure,
    wasm_host::{Compiled, LoadError, Payload, Trap},
};

/// The service the controller runs.
pub type Svc = Service<StoreFlash, Fw>;

/// Owner of the device's state.
pub struct Controller {
    svc: Svc,
    bus: &'static Bus,
}

#[cfg(feature = "wasm")]
enum StartError {
    Store(StoreError),
    NoHeap(HeapShort),
    NoMemory,
    NoHardware,
    Load(LoadError),
}

#[cfg(feature = "wasm")]
impl core::fmt::Display for StartError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            StartError::Store(e) => write!(f, "{e}"),
            StartError::NoHeap(short) => write!(f, "{short}"),
            StartError::NoMemory => f.write_str("not enough heap for the image"),
            StartError::NoHardware => f.write_str("payload hardware unavailable"),
            StartError::Load(e) => write!(f, "{e}"),
        }
    }
}

impl Controller {
    /// A controller over `svc`.
    pub fn new(svc: Svc, bus: &'static Bus) -> Self {
        Controller { svc, bus }
    }

    /// Runs forever: starts a stored payload (with `wasm`), then serves
    /// requests.
    pub async fn run(mut self) -> ! {
        self.bus.status(self.svc.status());
        #[cfg(feature = "wasm")]
        let mut start = self.svc.state().startable().then_some(None);
        loop {
            #[cfg(feature = "wasm")]
            if let Some(pending) = start.take() {
                self.running(pending).await;
            }
            let id = self.idle().await;
            #[cfg(feature = "wasm")]
            {
                start = Some(Some(id));
            }
            // Never reached without the runtime (start is NotFound), but
            // answer rather than leave the link waiting.
            #[cfg(not(feature = "wasm"))]
            self.bus.reply(Reply { id, result: Err(HandlerError::NotFound) });
        }
    }

    /// Handles one request: the reply to send now (`None` for a start),
    /// and whether a running payload must stop.
    fn handle(&mut self, id: u8, key: &str, body: &[u8]) -> (Option<Reply>, bool) {
        let Ok(key) = Key::new(key) else {
            return (Some(Reply { id, result: Err(HandlerError::InvalidRequest) }), false);
        };
        let before = self.svc.status();
        let mut out = [0u8; MAX_REPLY];
        self.svc.set_usb_configured(self.bus.usb_configured());
        let Handled { reply, stop, notes } = self.svc.handle(key, body, &mut out);
        for note in &notes {
            self.bus.log(format_args!("{note}"));
        }
        if self.svc.status() != before {
            self.bus.status(self.svc.status());
        }
        let reply = match reply {
            Answer::Start => None,
            Answer::Now(r) => Some(Reply {
                id,
                result: r.and_then(|n| {
                    out.get(..n).and_then(|b| Vec::from_slice(b).ok()).ok_or(HandlerError::BufferTooSmall)
                }),
            }),
        };
        (reply, stop)
    }

    /// Serves requests until a payload start is requested; returns its id.
    async fn idle(&mut self) -> u8 {
        loop {
            let msg: Inbound = self.bus.receive_inbound().await;
            if let Kind::Request { id } = msg.kind {
                match self.handle(id, &msg.key, &msg.body).0 {
                    Some(reply) => self.bus.reply(reply),
                    None => return id,
                }
            }
        }
    }

    #[cfg(feature = "wasm")]
    fn set_status_after(&self, before: middleware::loader::PayloadStatus) {
        if self.svc.status() != before {
            self.bus.status(self.svc.status());
        }
    }

    /// Loads the stored payload: heap check on the image length, read,
    /// heap check on the image's bound, compile, free the image,
    /// instantiate.
    #[cfg(feature = "wasm")]
    fn load(&mut self) -> Result<Payload<RpDevice>, StartError> {
        let limits = rp_base::budget::limits(&PROFILE_REF);
        let store = self.svc.store_mut().ok_or(StartError::Store(StoreError::NoImage))?;
        let header = store.header().ok_or(StartError::Store(StoreError::NoImage))?;
        check_heap(heap::free(), header.len).map_err(StartError::NoHeap)?;
        let len = header.len as usize;
        let mut image = AllocVec::new();
        image.try_reserve_exact(len).map_err(|_| StartError::NoMemory)?;
        image.resize(len, 0u8);
        store.load(header, &mut image).map_err(StartError::Store)?;
        let bound = limits.heap_needed(&image);
        let needed = check_heap_holding(heap::free(), image.capacity(), bound).map_err(StartError::NoHeap)?;
        let compiled = Compiled::new(&image, limits).map_err(StartError::Load)?;
        drop(image);
        let device = RpDevice::new(self.bus, &PROFILE_REF).ok_or(StartError::NoHardware)?;
        let payload = Payload::instantiate(compiled, device).map_err(StartError::Load)?;
        self.bus.log(format_args!(
            "payload loaded ({} bytes): heap {} B free, needed at most {needed} B",
            header.len,
            heap::free()
        ));
        Ok(payload)
    }

    #[cfg(feature = "wasm")]
    fn trapped(&mut self, trap: &Trap) {
        let before = self.svc.status();
        self.bus.log(format_args!("payload trapped: {trap}"));
        self.svc.trapped();
        self.set_status_after(before);
    }

    /// Loads and runs the payload until it traps or must stop. `pending`
    /// is the start request to answer once the load succeeded or failed.
    #[cfg(feature = "wasm")]
    async fn running(&mut self, pending: Option<u8>) {
        let answer = |bus: &Bus, result: Result<(), HandlerError>| {
            if let Some(id) = pending {
                bus.reply(Reply { id, result: result.map(|()| Vec::new()) });
            }
        };
        let before = self.svc.status();
        let mut payload = match self.load() {
            Ok(p) => p,
            Err(e) => {
                self.bus.log(format_args!("payload start failed: {e}"));
                self.svc.start_failed(match e {
                    StartError::Store(_) => StartFailure::Store,
                    _ => StartFailure::Load,
                });
                self.set_status_after(before);
                answer(self.bus, Err(HandlerError::Internal));
                return;
            }
        };
        self.svc.started();
        self.set_status_after(before);
        answer(self.bus, Ok(()));
        let mut dropped = 0u32;
        loop {
            yield_now().await;
            payload.device().fire_timers(now_us());
            for _ in 0..MAX_EVENTS {
                let Some(event) = payload.device().pop_event() else { break };
                if let Err(trap) = payload.dispatch(event) {
                    self.trapped(&trap);
                    return;
                }
            }
            let overflow = payload.device().overflow();
            if let Some(n) = overflow_report(dropped, overflow) {
                self.bus.log(format_args!("event queue full: {n} events dropped"));
            }
            dropped = overflow;
            let tick = payload.next_tick().map(|due| {
                Instant::now() + Duration::from_millis(u64::from(tick_delay(now_ms(), due)))
            });
            let alarm = payload.device().next_alarm_us().map(Instant::from_micros);
            let wake = match (tick, alarm) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            let timer = async move {
                match wake {
                    Some(at) => Timer::at(at).await,
                    None => core::future::pending::<()>().await,
                }
            };
            match select(self.bus.receive_inbound(), timer).await {
                Either::Second(()) => {
                    // Timers fire at the top of the loop; the tick runs
                    // only when it is due.
                    if let Err(trap) = payload.tick(now_ms()) {
                        self.trapped(&trap);
                        return;
                    }
                }
                Either::First(msg) => match msg.kind {
                    Kind::Publish => {
                        let Ok(key) = Key::new(&msg.key) else { continue };
                        if route_publication(&self.svc.state(), key, payload.subscriptions())
                            && let Err(trap) = payload.deliver(key, &msg.body)
                        {
                            self.trapped(&trap);
                            return;
                        }
                    }
                    Kind::Request { id } => {
                        let (reply, stop) = self.handle(id, &msg.key, &msg.body);
                        // A start while running is answered by dispatch.
                        self.bus.reply(reply.unwrap_or(Reply { id, result: Ok(Vec::new()) }));
                        if stop {
                            self.bus.log(format_args!("payload stopped"));
                            return;
                        }
                    }
                },
            }
        }
    }
}
