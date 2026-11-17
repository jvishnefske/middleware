//! The controller: owns the request service and (with `wasm`) the running
//! payload. It runs on the main task and alternates between two sessions,
//! as firmware-rp's controller does:
//!
//! * **idle** — no payload runs; each request goes to [`Service::handle`]
//!   (after the served-key route) and its reply back to the link. A
//!   request the service answers [`Answer::Start`] ends the session.
//! * **running** (`wasm` only) — the stored image is read, the heap check
//!   applied before and after reading it (`esp_base::start`), the image
//!   compiled, freed, and the module instantiated against an
//!   [`RtDevice`]. The session then loops: yield once to the executor,
//!   fire due software timers and hand at most `MAX_EVENTS` queued events
//!   to `on_irq`, then wait for an inbound message, the payload's next
//!   tick or the next timer deadline. Publications go to the payload when
//!   it subscribed to the key; requests go through
//!   [`imxrt_base::requests::handle`]. A trap, `sys/payload/stop`, a
//!   `begin` or an activated commit ends the session and drops the
//!   payload, whose device returns the pins.
//!
//! What a [`Handled`] asks of the shell beyond the reply is applied here:
//! `notes` are logged on `sys/log`, a status change is published on
//! `sys/payload/status`, `release_watchdog` stops the trial watchdog
//! ([`super::reset::release`]) once `sys/fw/confirm` succeeded, and
//! `reset` waits (bounded) until the link has sent the reply, then resets
//! the chip into the bootloader ([`super::reset::reset`]).

use embassy_time::{Duration, with_timeout};
use heapless::Vec;
use imxrt_base::links::LinkId;
use imxrt_base::service::{Handled, Reply as Answer, Service};
use imxrt_base::shell::flexspi::HyperFlash;
use middleware::Key;
use middleware::transport::HandlerError;

use super::bus::{Bus, Inbound, Kind, MAX_REPLY, Reply};
use super::reset;

#[cfg(not(feature = "wasm"))]
use esp_base::serve::Route;
#[cfg(feature = "wasm")]
use {
    super::device::{RtDevice, now_ms, now_us},
    super::heap,
    alloc::vec::Vec as AllocVec,
    embassy_futures::select::{Either, select},
    embassy_futures::yield_now,
    embassy_time::{Instant, Timer},
    esp_base::dispatch::{route_publication, tick_delay},
    esp_base::periph::overflow_report,
    esp_base::start::{HeapShort, check_heap, check_heap_holding},
    esp_base::store::StoreError,
    imxrt_base::chip::RT1050,
    imxrt_base::service::StartFailure,
    middleware::wasm_abi::events::MAX_EVENTS,
    wasm_host::{Compiled, LoadError, Payload, Trap},
};

/// Longest wait for the link to send a reply before a reset (a largest
/// frame takes about 100 ms at 115200 baud).
const RESET_GRACE: Duration = Duration::from_millis(1000);

/// The service the controller runs.
pub type Svc = Service<HyperFlash>;

/// Owner of the device's state.
pub struct Controller {
    svc: Svc,
    bus: &'static Bus,
    /// The board's Ethernet MAC: the 6-byte address signed control
    /// requests over Zenoh name.
    #[cfg_attr(not(feature = "eth"), allow(dead_code))]
    mac: [u8; 6],
}

#[cfg(feature = "wasm")]
enum StartError {
    NoHeap(HeapShort),
    NoMemory,
    NoHardware,
    Store(StoreError),
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
    pub fn new(svc: Svc, bus: &'static Bus, mac: [u8; 6]) -> Self {
        Controller { svc, bus, mac }
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
            self.bus.reply(Reply {
                link: id.0,
                id: id.1,
                result: Err(HandlerError::NotFound),
            });
        }
    }

    /// Handles one request (`payload` is the running payload): the reply
    /// to send now (`None` for a start), whether a running payload must
    /// stop, and the reset to perform once the reply has left.
    fn handle(
        &mut self,
        id: u8,
        msg: &Inbound,
        #[cfg(feature = "wasm")] payload: Option<&mut Payload<RtDevice>>,
    ) -> (Option<Reply>, bool, bool) {
        self.svc.set_link_ready(true);
        let link = msg.link;
        let Ok(key) = Key::new(&msg.key) else {
            return (
                Some(Reply {
                    link,
                    id,
                    result: Err(HandlerError::InvalidRequest),
                }),
                false,
                false,
            );
        };
        // The requester (`Service::set_origin`): the serial links are one,
        // every Zenoh peer is another.
        self.svc.set_origin(imxrt_base::service::Origin::Serial);
        // Over Zenoh, the management gate decides first (unclaimed: only
        // `sys/info` and `sys/trust/info`; claimed: read-only keys, staged
        // uploads with a signed commit, firmware transfers with a signed
        // commit, and start/stop/fw reboot/confirm/abort signed by the
        // pinned key, whose counter is persisted before it acts). A
        // refused request changes nothing (but the owner's unsigned commit
        // ends its own upload).
        #[cfg(feature = "eth")]
        let mut gated_body: Option<&[u8]> = None;
        #[cfg(feature = "eth")]
        if link == LinkId::ZENOH {
            use imxrt_base::zenoh_mgmt::{self, Admission};
            self.svc.set_origin(zenoh_mgmt::origin(self.mac));
            let facts = zenoh_mgmt::facts(self.svc.state(), self.mac);
            match zenoh_mgmt::admit(&facts, self.svc.loader_owner(), key, &msg.body) {
                Err(refusal) => {
                    if refusal.abandons_transfer() {
                        self.svc.abandon_transfer();
                    }
                    self.bus.log(format_args!(
                        "zenoh: {} refused ({})",
                        key.as_str(),
                        refusal.reason()
                    ));
                    return (
                        Some(Reply {
                            link,
                            id,
                            result: Err(refusal.error()),
                        }),
                        false,
                        false,
                    );
                }
                Ok(Admission::Dispatch) => {}
                Ok(Admission::Control { counter }) => {
                    if self.svc.accept_control(counter).is_err() {
                        return (
                            Some(Reply {
                                link,
                                id,
                                result: Err(HandlerError::Internal),
                            }),
                            false,
                            false,
                        );
                    }
                    gated_body = Some(&[]);
                }
            }
        }
        #[cfg(feature = "eth")]
        let body: &[u8] = gated_body.unwrap_or(&msg.body);
        #[cfg(not(feature = "eth"))]
        let body: &[u8] = &msg.body;
        // `sys/link/sub` / `unsub`: the asking link's filter, before the
        // service.
        if let Some(r) = self.bus.link_request(link, key, body) {
            return (
                Some(Reply {
                    link,
                    id,
                    result: r.map(|()| Vec::new()),
                }),
                false,
                false,
            );
        }
        let before = self.svc.status();
        let mut out = [0u8; MAX_REPLY];
        #[cfg(feature = "wasm")]
        let handled = {
            let answer = imxrt_base::requests::handle(
                &mut self.svc,
                payload,
                key,
                body,
                &msg.meta(),
                &mut out,
            );
            if let Some(trap) = &answer.trapped {
                self.bus.log(format_args!(
                    "payload trapped on request {}: {trap}",
                    key.as_str()
                ));
            }
            answer.handled
        };
        #[cfg(not(feature = "wasm"))]
        let handled = match self.svc.route_idle(key) {
            Route::Refuse(e) => Handled {
                reply: Answer::Now(Err(e)),
                stop: false,
                notes: Vec::new(),
                reset: None,
                release_watchdog: false,
            },
            Route::System | Route::Payload => self.svc.handle(key, body, &mut out),
        };
        let Handled {
            reply,
            stop,
            notes,
            reset: reset_after,
            release_watchdog,
        } = handled;
        #[cfg(feature = "eth")]
        if key == imxrt_base::config::ZENOH_ENDPOINT {
            super::zenoh::set_endpoint(self.svc.zenoh_endpoint());
        }
        for note in &notes {
            self.bus.log(format_args!("{note}"));
        }
        if self.svc.status() != before {
            self.bus.status(self.svc.status());
        }
        if release_watchdog {
            reset::release();
        }
        let reply = match reply {
            Answer::Start => None,
            Answer::Now(r) => Some(Reply {
                link,
                id,
                result: r.and_then(|n| {
                    out.get(..n)
                        .and_then(|b| Vec::from_slice(b).ok())
                        .ok_or(HandlerError::BufferTooSmall)
                }),
            }),
        };
        (reply, stop, reset_after.is_some())
    }

    /// Sends `reply`, then, if asked, waits for it to leave and resets.
    async fn answer(&self, reply: Reply, then_reset: bool) {
        let (link, id) = (reply.link, reply.id);
        self.bus.reply(reply);
        if then_reset {
            let _ = with_timeout(RESET_GRACE, self.bus.wait_sent(link, id)).await;
            reset::reset();
        }
    }

    /// Serves requests until a payload start is requested; returns where
    /// to answer it.
    async fn idle(&mut self) -> (LinkId, u8) {
        loop {
            let msg: Inbound = self.bus.receive_inbound().await;
            if let Kind::Request { id } = msg.kind {
                #[cfg(feature = "wasm")]
                let (reply, _, then_reset) = self.handle(id, &msg, None);
                #[cfg(not(feature = "wasm"))]
                let (reply, _, then_reset) = self.handle(id, &msg);
                match reply {
                    Some(reply) => self.answer(reply, then_reset).await,
                    None => return (msg.link, id),
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
    fn load(&mut self) -> Result<Payload<RtDevice>, StartError> {
        let limits = imxrt_base::budget::limits(&RT1050);
        let store = self.svc.store_mut();
        let header = store
            .header()
            .ok_or(StartError::Store(StoreError::NoImage))?;
        check_heap(heap::free(), header.len).map_err(StartError::NoHeap)?;
        let len = header.len as usize;
        let mut image = AllocVec::new();
        image
            .try_reserve_exact(len)
            .map_err(|_| StartError::NoMemory)?;
        image.resize(len, 0u8);
        store.load(header, &mut image).map_err(StartError::Store)?;
        let bound = limits.heap_needed(&image);
        let needed = check_heap_holding(heap::free(), image.capacity(), bound)
            .map_err(StartError::NoHeap)?;
        let compiled = Compiled::new(&image, limits).map_err(StartError::Load)?;
        drop(image);
        let device =
            RtDevice::new(self.bus, &RT1050, self.svc.node()).ok_or(StartError::NoHardware)?;
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
    async fn running(&mut self, pending: Option<(LinkId, u8)>) {
        let answer = |bus: &Bus, result: Result<(), HandlerError>| {
            if let Some((link, id)) = pending {
                bus.reply(Reply {
                    link,
                    id,
                    result: result.map(|()| Vec::new()),
                });
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
        self.bus.set_subscriptions(core::iter::empty());
        imxrt_base::requests::ended(&mut self.svc, &payload);
    }

    /// The running session's loop, until the payload traps or must stop.
    #[cfg(feature = "wasm")]
    async fn session(&mut self, payload: &mut Payload<RtDevice>) {
        let mut dropped = 0u32;
        loop {
            yield_now().await;
            // Init or any handler may subscribe: the Zenoh link follows.
            self.bus.set_subscriptions(payload.subscriptions());
            payload.device().poll(now_us());
            for _ in 0..MAX_EVENTS {
                let Some(event) = payload.device().pop_event() else {
                    break;
                };
                if let Err(trap) = payload.dispatch(event) {
                    self.trapped(&trap);
                    return;
                }
            }
            let overflow = payload.device().overflow();
            if let Some(n) = overflow_report(dropped, overflow) {
                self.bus
                    .log(format_args!("event queue full: {n} events dropped"));
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
                        let Ok(key) = Key::new(&msg.key) else {
                            continue;
                        };
                        if route_publication(&self.svc.state(), key, payload.subscriptions())
                            && let Err(trap) = payload.deliver(key, &msg.body)
                        {
                            self.trapped(&trap);
                            return;
                        }
                    }
                    Kind::Request { id } => {
                        let (reply, stop, then_reset) = self.handle(id, &msg, Some(&mut *payload));
                        // A start while running is answered by dispatch.
                        let reply = reply.unwrap_or(Reply {
                            link: msg.link,
                            id,
                            result: Ok(Vec::new()),
                        });
                        self.answer(reply, then_reset).await;
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
