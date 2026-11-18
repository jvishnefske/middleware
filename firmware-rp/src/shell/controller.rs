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
//!   comes first: an inbound message, the payload's next tick, the next
//!   timer deadline, or the end of a running SPI transfer, which the
//!   device pumps meanwhile ([`RpDevice::io`]). Publications go to the payload when it subscribed to
//!   the key (`esp_base::dispatch::route_publication`); requests go
//!   through [`rp_base::requests::handle`]: a key the payload serves
//!   (ABI 5) to its `on_request` with a 512-byte reply and the link's
//!   meta, everything else to the service as when idle. A trap (in a
//!   request too, answered `Internal`), `sys/payload/stop`, a `begin` or
//!   an activated commit ends the session and drops the payload, whose
//!   device returns the pins; the service keeps the keys it served
//!   ([`rp_base::requests::ended`]), so their requests answer
//!   `Unavailable` until the next run or an upload.
//!
//! Without `wasm` (the RP2040) the service refuses `sys/payload/start`
//! (`NotFound`), so the controller only ever runs the idle session.
//!
//! The service's notes are logged on `sys/log`; every status change is
//! published on `sys/payload/status`.
//!
//! Discovery beacons ([`rp_base::beacons`]): the controller owns the
//! device's [`Identity`] and an [`Announcer`] over the bus's [`Uplink`]
//! publisher. Each pass of either session brings the announced records to
//! [`wanted`]: the node record, plus the running payload's served keys
//! (none while idle, so the first idle pass after a run, whether it ended
//! by stop, trap, `begin` or an activated commit, sends their goodbyes),
//! then sends the refreshes that are due; the next due refresh is one of
//! the wake-ups both sessions wait for. Served keys left out (over the
//! four-record cap, or not usable as a kind) are logged once per run.
//! After answering `sys/info` (the gateway's bind) every record is sent
//! again, at most every 5 s, behind the response on the link.

use core::task::Poll;

use embassy_futures::poll_once;
use embassy_futures::select::{Either, select};
use embassy_time::{Instant, Timer};
use heapless::Vec;
use middleware::Key;
use middleware::loader::keys;
use middleware::transport::HandlerError;
use rp_base::beacons::{Announcer, Identity, wanted};
use rp_base::service::{Handled, Reply as Answer, Service};
#[cfg(not(feature = "wasm"))]
use esp_base::serve::Route;

use super::board::{Fw, StoreFlash};
use super::bus::{Bus, Inbound, Kind, MAX_REPLY, Reply, Uplink};

#[cfg(feature = "wasm")]
use {
    super::board::PROFILE_REF,
    super::device::{RpDevice, now_ms, now_us},
    super::heap,
    alloc::vec::Vec as AllocVec,
    embassy_futures::select::{Either3, select3},
    embassy_futures::yield_now,
    embassy_time::Duration,
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
    id: Identity,
    beacons: Announcer<Uplink>,
}

/// Milliseconds since boot: the announcer's clock.
fn now_millis() -> u64 {
    Instant::now().as_millis()
}

/// Waits until `at` (milliseconds since boot), or forever for `None`.
async fn until(at: Option<u64>) {
    match at {
        Some(ms) => Timer::at(Instant::from_millis(ms)).await,
        None => core::future::pending::<()>().await,
    }
}

/// Logs an announcer call that failed. The calls are polled once
/// (`poll_once`) rather than awaited: [`Uplink`]'s publish never waits, so
/// they are ready on the first poll, and their futures (the beacon
/// encoder's 640 bytes of buffers among them) live on the stack for the
/// call instead of in the main task's static future. A call that was not
/// ready (never expected) is dropped, as a cancelled `PubSubDiscovery` call
/// may be, and logged.
fn report<E: core::fmt::Debug>(bus: &Bus, what: &str, polled: Poll<Result<usize, E>>) {
    match polled {
        Poll::Ready(Ok(_)) => {}
        Poll::Ready(Err(e)) => bus.log(format_args!("disc: {what} not sent: {e:?}")),
        Poll::Pending => bus.log(format_args!("disc: {what} not sent: publisher busy")),
    }
}

/// Whether `msg` was a `sys/info` request that `reply` answers.
fn answers_info(msg: &Inbound, reply: &Reply) -> bool {
    msg.key.as_str() == keys::INFO.as_str() && reply.result.is_ok()
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
    /// A controller over `svc`, announcing `id`.
    pub fn new(svc: Svc, bus: &'static Bus, id: Identity) -> Self {
        Controller { svc, bus, id, beacons: Announcer::new(Uplink(bus)) }
    }

    /// Brings the announced records to the node record plus `served`,
    /// then sends the refreshes due; returns how many served keys were
    /// left out. A failed send is logged (see [`Announcer::sync`] for what
    /// is sent again).
    fn announce<'s>(&mut self, served: impl Iterator<Item = &'s str>) -> usize {
        let wanted = wanted(&self.id, served);
        report(self.bus, "beacon", poll_once(self.beacons.sync(&wanted)));
        report(self.bus, "refresh", poll_once(self.beacons.tick(now_millis())));
        wanted.omitted
    }

    /// After a `sys/info` reply: every record again (rate limited).
    fn after_info<'s>(&mut self, served: impl Iterator<Item = &'s str>) {
        let wanted = wanted(&self.id, served);
        report(self.bus, "beacon", poll_once(self.beacons.after_info(&wanted, now_millis())));
    }

    /// Runs forever: starts a stored payload (with `wasm`), then serves
    /// requests. Takes the controller by reference so the caller's future
    /// holds its one copy (by value, the controller, announcer included,
    /// would be stored twice in the main task's static future).
    pub async fn run(&mut self) -> ! {
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

    /// Handles one request (`payload` is the running payload): the reply
    /// to send now (`None` for a start), and whether a running payload
    /// must stop.
    fn handle(
        &mut self,
        id: u8,
        msg: &Inbound,
        #[cfg(feature = "wasm")] payload: Option<&mut Payload<RpDevice>>,
    ) -> (Option<Reply>, bool) {
        let Ok(key) = Key::new(&msg.key) else {
            return (Some(Reply { id, result: Err(HandlerError::InvalidRequest) }), false);
        };
        let before = self.svc.status();
        let mut out = [0u8; MAX_REPLY];
        self.svc.set_usb_configured(self.bus.usb_configured());
        #[cfg(feature = "wasm")]
        let Handled { reply, stop, notes } = {
            let answer = rp_base::requests::handle(&mut self.svc, payload, key, &msg.body, &msg.meta(), &mut out);
            if let Some(trap) = &answer.trapped {
                self.bus.log(format_args!("payload trapped on request {}: {trap}", key.as_str()));
            }
            answer.handled
        };
        // Without the runtime nothing is ever served (start is NotFound).
        #[cfg(not(feature = "wasm"))]
        let Handled { reply, stop, notes } = match self.svc.route_idle(key) {
            Route::Refuse(e) => Handled { reply: Answer::Now(Err(e)), stop: false, notes: Vec::new() },
            Route::System | Route::Payload => self.svc.handle(key, &msg.body, &mut out),
        };
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
    /// Announces the node record alone (withdrawing a past run's keys)
    /// and wakes for its refreshes.
    async fn idle(&mut self) -> u8 {
        loop {
            self.announce(core::iter::empty());
            let due = self.beacons.due(now_millis());
            let msg: Inbound = match select(self.bus.receive_inbound(), until(due)).await {
                Either::First(msg) => msg,
                Either::Second(()) => continue,
            };
            if let Kind::Request { id } = msg.kind {
                #[cfg(feature = "wasm")]
                let handled = self.handle(id, &msg, None);
                #[cfg(not(feature = "wasm"))]
                let handled = self.handle(id, &msg);
                match handled.0 {
                    Some(reply) => {
                        let info = answers_info(&msg, &reply);
                        self.bus.reply(reply);
                        if info {
                            self.after_info(core::iter::empty());
                        }
                    }
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
        // The stored binding, if it is for this image (BD4).
        let mut record = [0u8; middleware::loader::binding::BINDING_MAX];
        let binding = match self.svc.binding_record(&mut record) {
            Some(n) => match esp_base::binding_store::for_image(&record[..n], &image) {
                Ok(b) => Some(b),
                Err(why) => {
                    self.bus.log(format_args!("payload: stored binding ignored ({why:?})"));
                    None
                }
            },
            None => None,
        };
        let compiled = Compiled::new(&image, limits).map_err(StartError::Load)?;
        drop(image);
        let device = RpDevice::new(self.bus, &PROFILE_REF, self.svc.node()).ok_or(StartError::NoHardware)?;
        let payload = Payload::instantiate_bound(compiled, device, binding.as_ref()).map_err(StartError::Load)?;
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
        self.session(&mut payload).await;
        rp_base::requests::ended(&mut self.svc, &payload);
    }

    /// The running session's loop, until the payload traps or must stop.
    #[cfg(feature = "wasm")]
    async fn session(&mut self, payload: &mut Payload<RpDevice>) {
        let mut dropped = 0u32;
        let mut omitted_logged = false;
        loop {
            yield_now().await;
            let omitted = self.announce(payload.serves());
            if omitted > 0 && !omitted_logged {
                self.bus.log(format_args!("disc: {omitted} served keys not announced (4 records at most)"));
                omitted_logged = true;
            }
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
            let beacon = self.beacons.due(now_millis()).map(Instant::from_millis);
            let wake = [tick, alarm, beacon].into_iter().flatten().min();
            let timer = async move {
                match wake {
                    Some(at) => Timer::at(at).await,
                    None => core::future::pending::<()>().await,
                }
            };
            match select3(self.bus.receive_inbound(), timer, payload.device().io()).await {
                // An SPI transfer ended; its event is dispatched at the top.
                Either3::Third(()) => {}
                Either3::Second(()) => {
                    // Timers fire at the top of the loop; the tick runs
                    // only when it is due.
                    if let Err(trap) = payload.tick(now_ms()) {
                        self.trapped(&trap);
                        return;
                    }
                }
                Either3::First(msg) => match msg.kind {
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
                        let (reply, stop) = self.handle(id, &msg, Some(&mut *payload));
                        // A start while running is answered by dispatch.
                        let reply = reply.unwrap_or(Reply { id, result: Ok(Vec::new()) });
                        let info = answers_info(&msg, &reply);
                        self.bus.reply(reply);
                        if stop {
                            self.bus.log(format_args!("payload stopped"));
                            return;
                        }
                        if info {
                            self.after_info(payload.serves());
                        }
                    }
                },
            }
        }
    }
}
