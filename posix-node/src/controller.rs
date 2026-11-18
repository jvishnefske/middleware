//! The controller: the request service and the running payload, as one
//! synchronous state machine the shell drives. It does what the base
//! firmwares' controller loops do (rp-base's `shell/controller.rs`,
//! esp-base's), minus the waiting: the shell ([`crate::shell`]) owns the
//! links and the timer and calls in.
//!
//! * [`Controller::boot`] logs what the flash holds, publishes the status
//!   and starts an installed payload, as a board does at power-up.
//! * [`Controller::request`] routes one request with `esp_base::serve`
//!   (rp-base's `requests::handle`): `sys/...` and keys nothing serves to
//!   the [`Service`]; a key the running payload serves to
//!   `Payload::request` with a 512-byte reply
//!   (`esp_base::serve::REPLY_CAPACITY`) and the request's meta; the last
//!   run's keys while none runs `Unavailable`. A start the service
//!   answers [`Reply::Start`] loads the image (read from the store,
//!   compiled, instantiated against a [`PiDevice`] under the configured
//!   [`Limits`]) and answers `Ok` once it runs, `Internal` if it failed. A
//!   trap (in a request too, answered `Internal`), `sys/payload/stop`, a
//!   `begin` or an activated commit ends the run; the service keeps the
//!   keys it served.
//! * [`Controller::publication`] delivers a publication when the payload
//!   subscribed to the key (`esp_base::dispatch::route_publication`).
//! * [`Controller::poll`] fires due timers, hands at most `MAX_EVENTS`
//!   queued events to `on_irq` and runs `tick` when it is due;
//!   [`Controller::next_wake_us`] says when to poll next.
//! * [`Controller::drain`] takes what to send: the payload's publications
//!   and log lines, the service's notes on `sys/log` and every status
//!   change on `sys/payload/status`, oldest first, in a bounded outbox.
//!
//! * ABI 8 drivers ([`crate::driver`]): what the payload publishes on a
//!   driver point's `drv/<point>` key goes to [`Driver::write`], never to
//!   the outbox; what [`Driver::poll`] reads is delivered on `drv/<point>`
//!   when the payload subscribed (its binding put it there), with a
//!   snapshot of every input right after a start; `sys/manifest/cm` lists
//!   the points; a start whose driver targets name a missing point, or
//!   one of another kind or direction, fails ([`crate::driver::check_binding`]).
//!
//! There is no heap check before a start (`esp_base::start`): the node
//! runs on an operating system with virtual memory, and the payload's
//! linear memory is capped by [`Limits::max_memory_bytes`].

use embedded_storage::nor_flash::NorFlash;
use esp_base::dispatch::{route_publication, tick_delay};
use esp_base::periph::overflow_report;
use esp_base::serve::{REPLY_CAPACITY, Route, ServedKeys, payload_reply, route_request};
use esp_base::store::StoreError;
use middleware::Key;
use middleware::loader::{PayloadStatus, keys};
use middleware::serial::frame::MAX_RAW_FRAME;
use middleware::transport::{HandlerError, RequestMeta};
use middleware::wasm_abi::events::MAX_EVENTS;
use wasm_host::{Compiled, Limits, LoadError, Payload, Trap};

use crate::device::{Clock, OUTBOX, Out, Outbox, PiDevice};
use crate::driver::{Driver, NoDriver, check_binding, index_of};
use crate::flash::Durable;
use crate::service::{Boot, Facts, Handled, Reply, Service, StartFailure};

/// Room for the controller's own lines beside a payload call's outbox.
pub const CONTROL_OUTBOX: usize = OUTBOX + 16;

/// Why a start failed.
#[derive(Debug)]
enum StartError {
    Store(StoreError),
    Load(LoadError),
    Driver(String),
}

impl core::fmt::Display for StartError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            StartError::Store(e) => write!(f, "{e}"),
            StartError::Load(e) => write!(f, "{e}"),
            StartError::Driver(e) => write!(f, "driver: {e}"),
        }
    }
}

/// The node's state (module docs).
pub struct Controller<F, C: Clock + 'static> {
    svc: Service<F>,
    payload: Option<Payload<PiDevice<C>>>,
    clock: C,
    limits: Limits,
    outbox: Outbox,
    published: Option<PayloadStatus>,
    events_dropped: u32,
    driver: Box<dyn Driver>,
}

impl<F: NorFlash + Durable, C: Clock + Clone + 'static> Controller<F, C> {
    /// Opens the service on `flash`; payloads run under `limits` with
    /// `clock`.
    pub fn open(flash: F, facts: Facts, limits: Limits, clock: C) -> (Self, Boot) {
        Self::open_with_driver(flash, facts, limits, clock, Box::new(NoDriver))
    }

    /// [`Controller::open`] with `driver`'s control-module points (the
    /// manifest names them when `facts` does: [`crate::facts_with_driver`]).
    pub fn open_with_driver(flash: F, facts: Facts, limits: Limits, clock: C, driver: Box<dyn Driver>) -> (Self, Boot) {
        let (svc, boot) = Service::open(flash, facts);
        let ctl = Controller {
            svc,
            payload: None,
            clock,
            limits,
            outbox: Outbox::with_capacity(CONTROL_OUTBOX),
            published: None,
            events_dropped: 0,
            driver,
        };
        (ctl, boot)
    }

    /// Logs the start, publishes the status and starts an installed
    /// payload.
    pub fn boot(&mut self, boot: &Boot) {
        let node = crate::node_id::hex(self.svc.node());
        self.outbox.log(&format!(
            "mw-posix-node {} up: node {node}, capacity {} bytes",
            env!("CARGO_PKG_VERSION"),
            self.svc.capacity()
        ));
        let installed = match boot.installed {
            Some(Ok(h)) => format!("payload: {} bytes installed", h.len),
            Some(Err(e)) => format!("payload: none ({e})"),
            None => "payload: store unavailable".to_owned(),
        };
        self.outbox.log(&installed);
        match boot.trust {
            None => self.outbox.log("trust: record unreadable; installs refused"),
            Some(p) => self.outbox.log(&format!("trust: {:?}", p.trust)),
        }
        if self.svc.state().startable() {
            let _ = self.start();
        }
        self.settle();
    }

    /// The payload status.
    pub fn status(&self) -> PayloadStatus {
        self.svc.status()
    }

    /// Whether a payload runs.
    pub fn running(&self) -> bool {
        self.payload.is_some()
    }

    /// The clock payloads run on.
    pub fn clock(&self) -> &C {
        &self.clock
    }

    /// The service (its state and store).
    pub fn service(&self) -> &Service<F> {
        &self.svc
    }

    /// The driver.
    pub fn driver(&mut self) -> &mut dyn Driver {
        self.driver.as_mut()
    }

    /// Handles one request: the reply body or the error.
    pub fn request(&mut self, key: Key<'_>, body: &[u8], meta: &RequestMeta<'_>) -> Result<Vec<u8>, HandlerError> {
        let mut out = vec![0u8; MAX_RAW_FRAME];
        if key == keys::CONTROL_MODULES {
            let n = crate::driver::serve_page(self.driver.points(), body, &mut out)?;
            out.truncate(n);
            return Ok(out);
        }
        let running = self.payload.is_some() && self.svc.status() == PayloadStatus::Running;
        let route = match self.payload.as_ref() {
            Some(p) if running => route_request(key, PayloadStatus::Running, p.serves()),
            _ => self.svc.route_idle(key),
        };
        let result = match route {
            Route::Payload if running => self.ask_payload(key, body, meta, &mut out),
            Route::Refuse(e) => Err(e),
            Route::System | Route::Payload => self.ask_service(key, body, &mut out),
        };
        self.settle();
        result.and_then(|n| out.get(..n).map(<[u8]>::to_vec).ok_or(HandlerError::BufferTooSmall))
    }

    /// A request on a key the running payload serves.
    fn ask_payload(
        &mut self,
        key: Key<'_>,
        body: &[u8],
        meta: &RequestMeta<'_>,
        out: &mut [u8],
    ) -> Result<usize, HandlerError> {
        let Some(p) = self.payload.as_mut() else { return Err(HandlerError::NotFound) };
        let cap = out.len().min(REPLY_CAPACITY);
        let result = p.request(key, body, meta, out.get_mut(..cap).unwrap_or_default());
        let trap = result.as_ref().err().cloned();
        let answered = payload_reply(result);
        if answered.trapped {
            let what = trap.map_or_else(|| "poisoned".to_owned(), |t| t.to_string());
            self.outbox.log(&format!("payload trapped on request {}: {what}", key.as_str()));
            self.svc.trapped();
            self.end_run();
        }
        answered.reply
    }

    /// A request for the service (`sys/...`, or a key nothing serves).
    fn ask_service(&mut self, key: Key<'_>, body: &[u8], out: &mut [u8]) -> Result<usize, HandlerError> {
        let Handled { reply, stop, notes } = self.svc.handle(key, body, out);
        for note in &notes {
            self.outbox.log(&note.to_string());
        }
        if stop && self.payload.is_some() {
            self.end_run();
            self.outbox.log("payload stopped");
        }
        match reply {
            Reply::Start => self.start().map(|()| 0),
            Reply::Now(r) => r,
        }
    }

    /// A publication from a link: delivered when the running payload
    /// subscribed to `key`.
    pub fn publication(&mut self, key: Key<'_>, body: &[u8]) {
        let state = self.svc.state();
        let delivered = match self.payload.as_mut() {
            Some(p) if route_publication(&state, key, p.subscriptions()) => p.deliver(key, body).map(|_| ()),
            _ => Ok(()),
        };
        if let Err(trap) = delivered {
            self.trapped(&trap);
        }
        self.settle();
    }

    /// Fires due timers, dispatches queued events and runs a due tick;
    /// first, what the driver read.
    pub fn poll(&mut self) {
        let readings = self.driver.poll(self.clock.now_us());
        self.deliver_driver(readings);
        let result = match self.payload.as_mut() {
            Some(p) => Self::run_due(p, &self.clock, &mut self.events_dropped, &mut self.outbox),
            None => Ok(()),
        };
        if let Err(trap) = result {
            self.trapped(&trap);
        }
        self.settle();
    }

    fn run_due(p: &mut Payload<PiDevice<C>>, clock: &C, dropped: &mut u32, outbox: &mut Outbox) -> Result<(), Trap> {
        p.device().fire_timers();
        for _ in 0..MAX_EVENTS {
            let Some(event) = p.device().pop_event() else { break };
            p.dispatch(event)?;
        }
        let overflow = p.device().overflow();
        if let Some(n) = overflow_report(*dropped, overflow) {
            outbox.log(&format!("event queue full: {n} events dropped"));
        }
        *dropped = overflow;
        p.tick(clock.now_ms()).map(|_| ())
    }

    /// Delivers driver readings on their points' keys, to a payload bound
    /// to them.
    fn deliver_driver(&mut self, readings: Vec<(usize, Vec<u8>)>) {
        let Some(p) = self.payload.as_mut() else { return };
        let mut trap = None;
        for (i, msg) in readings {
            let Some(point) = self.driver.points().get(i) else { continue };
            let key = wasm_host::channels::driver_key(&point.name);
            let Ok(k) = Key::new(&key) else { continue };
            if !p.subscriptions().any(|s| s == key) {
                continue;
            }
            if let Err(t) = p.deliver(k, &msg) {
                trap = Some(t);
                break;
            }
        }
        if let Some(t) = trap {
            self.trapped(&t);
        }
    }

    /// A payload publication: to the driver on a `drv/<point>` key, to the
    /// outbox otherwise.
    fn forward(driver: &mut dyn Driver, outbox: &mut Outbox, now_us: u64, o: Out) {
        if !o.key.starts_with("drv/") {
            outbox.push_out(o);
            return;
        }
        // Never to a link, even for a point this driver does not have.
        let point = Key::new(&o.key).ok().and_then(|k| middleware::loader::binding::driver_point(k).map(str::to_owned));
        let index = point.as_deref().and_then(|p| index_of(driver.points(), p));
        let result = match index {
            Some(i) => driver.write(i, &o.payload, now_us),
            None => Err(crate::driver::DriverError::NotWritable),
        };
        if let Err(e) = result {
            outbox.log(&format!("driver {}: {}: {e:?}", driver.name(), o.key));
        }
    }

    /// When to poll next, in µs on the clock: the next tick, timer or
    /// driver step.
    pub fn next_wake_us(&mut self) -> Option<u64> {
        let now_us = self.clock.now_us();
        let driver = self.driver.next_wake_us(now_us);
        // Without a payload nobody reads the driver: it catches up (it
        // integrates from the clock) when one starts.
        let p = self.payload.as_mut()?;
        let tick = p
            .next_tick()
            .map(|due| now_us.saturating_add(u64::from(tick_delay(self.clock.now_ms(), due)) * 1000));
        let alarm = p.device().next_alarm_us();
        [tick, alarm, driver].into_iter().flatten().min()
    }

    /// Takes what to send, oldest first.
    pub fn drain(&mut self) -> Vec<Out> {
        self.outbox.drain().collect()
    }

    /// Publications dropped because the outboxes were full.
    pub fn dropped(&mut self) -> u32 {
        let device = self.payload.as_mut().map_or(0, |p| p.device().outbox().dropped());
        self.outbox.dropped().saturating_add(device)
    }

    /// Stops a running payload and makes the flash durable.
    pub fn shutdown(&mut self) -> Result<(), StoreError> {
        if self.payload.is_some() {
            self.end_run();
            self.outbox.log("payload stopped (node shutting down)");
        }
        self.settle();
        self.svc.sync()
    }

    /// Loads and starts the stored payload.
    fn start(&mut self) -> Result<(), HandlerError> {
        match self.load() {
            Ok((payload, len)) => {
                self.payload = Some(payload);
                self.svc.started();
                self.outbox.log(&format!("payload started ({len} bytes)"));
                let snapshot = self.driver.snapshot(self.clock.now_us());
                self.deliver_driver(snapshot);
                Ok(())
            }
            Err(e) => {
                self.outbox.log(&format!("payload start failed: {e}"));
                self.svc.start_failed(match e {
                    StartError::Store(_) => StartFailure::Store,
                    StartError::Load(_) | StartError::Driver(_) => StartFailure::Load,
                });
                Err(HandlerError::Internal)
            }
        }
    }

    fn load(&mut self) -> Result<(Payload<PiDevice<C>>, u32), StartError> {
        let node = self.svc.node();
        let store = self.svc.store_mut().ok_or(StartError::Store(StoreError::NoImage))?;
        let header = store.header().ok_or(StartError::Store(StoreError::NoImage))?;
        let mut image = vec![0u8; header.len as usize];
        store.load(header, &mut image).map_err(StartError::Store)?;
        // The stored binding, if it is for this image (BD4).
        let mut record = [0u8; middleware::loader::binding::BINDING_MAX];
        let binding = match self.svc.binding_record(&mut record) {
            Some(n) => match esp_base::binding_store::for_image(&record[..n], &image) {
                Ok(b) => Some(b),
                Err(why) => {
                    self.outbox.log(&format!("payload: stored binding ignored ({why:?})"));
                    None
                }
            },
            None => None,
        };
        let compiled = Compiled::new(&image, self.limits).map_err(StartError::Load)?;
        drop(image);
        check_binding(compiled.channels(), binding.as_ref(), self.driver.points()).map_err(StartError::Driver)?;
        let device = PiDevice::new(self.clock.clone(), node);
        let payload = Payload::instantiate_bound(compiled, device, binding.as_ref()).map_err(StartError::Load)?;
        Ok((payload, header.len))
    }

    fn trapped(&mut self, trap: &Trap) {
        self.outbox.log(&format!("payload trapped: {trap}"));
        self.svc.trapped();
        self.end_run();
    }

    /// Ends the run: the service keeps the served keys; what the payload
    /// queued is still sent.
    fn end_run(&mut self) {
        if let Some(mut p) = self.payload.take() {
            self.svc.run_ended(ServedKeys::from_run(p.serves()));
            let now = self.clock.now_us();
            for o in p.device().outbox().drain() {
                Self::forward(self.driver.as_mut(), &mut self.outbox, now, o);
            }
        }
    }

    /// Moves the payload's outbox into the controller's and publishes a
    /// changed status.
    fn settle(&mut self) {
        if let Some(p) = self.payload.as_mut() {
            let now = self.clock.now_us();
            for o in p.device().outbox().drain() {
                Self::forward(self.driver.as_mut(), &mut self.outbox, now, o);
            }
        }
        let status = self.svc.status();
        if self.published != Some(status) {
            self.published = Some(status);
            self.outbox.push(keys::STATUS, &[status as u8]);
        }
    }
}

#[cfg(test)]
mod tests;
