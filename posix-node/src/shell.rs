//! The imperative shell: the device thread that drives the
//! [`Controller`] over its serial links, and the node that joins it to an
//! in-process MCU gateway.
//!
//! ```text
//!   payload ── Controller ── device thread ══ ChannelIo pair ══ mw-gateway ── Zenoh, gw/sys
//!                                 ║
//!                                 ╠═══ local link (optional: MW_POSIX_NODE_LOCAL_SERIAL, tests)
//!                                 ║
//!                                 ╚═══ mesh link (optional: crate::zenoh_sys, @mw/node/<sender>/sys/**)
//! ```
//!
//! * The **device thread** runs its own current-thread tokio runtime, so a
//!   payload call (bounded by its fuel) never holds up the gateway's
//!   tasks. It owns the controller and one `SerialLink` per link, speaking
//!   the base firmware's protocol on each exactly as a board's USB port
//!   does. Each turn it polls the controller (timers, events, a due tick),
//!   sends what the controller queued, then waits for whichever comes
//!   first: a frame on a link (`SerialLink::next_frame`, cancel safe), the
//!   payload's next tick or timer, or the stop signal. A request is
//!   answered on the link it came from; publications, `sys/log` lines and
//!   status changes go to the gateway and local links (the gateway relays
//!   `app/**` to Zenoh and keeps `sys/log` in its tail), never to the mesh
//!   link, which only carries requests and their answers. Each send is
//!   bounded by [`SEND_TIMEOUT`] and a failure counted. The gateway link
//!   closing ends the thread; a local or mesh link closing only drops that
//!   link. On stop the payload is stopped and the flash synced.
//! * The gateway and local links are serial requesters (physical access:
//!   `Controller::request`). A request on the **mesh link** came over
//!   Zenoh: its body is a `crate::mesh_gate::wrap` envelope naming the
//!   requester, and it is handled by `Controller::mesh_request`, through
//!   the Zenoh gate (a malformed envelope is `InvalidRequest`).
//! * [`start`] makes a `ChannelIo` pair (RG14), starts the gateway on one
//!   end (`mw_gateway::start_with_store`, as `mw-gateway`'s binary does:
//!   the update gate of RG20–RG27 with its persisted control counter
//!   floor) and the device thread on the other, so the node's payloads publish to Zenoh and the cluster reaches
//!   it through `gw/sys` exactly as a USB-attached board through
//!   `mw-gateway`.
//!
//! Queues are bounded: the `ChannelIo` channels (64 chunks each way), the
//! controller's outbox ([`crate::controller::CONTROL_OUTBOX`], dropping the
//! newest) and the gateway's own queues (RG13).

use std::io;
use std::thread::JoinHandle;
use std::time::Duration;

use embedded_storage::nor_flash::NorFlash;
use middleware::Key;
use middleware::loader::{PayloadStatus, keys};
use middleware::serial::frame::Frame;
use middleware::serial::link::{RECV_BUF_LEN, SerialLink};
use middleware::transport::{HandlerError, RequestMeta};
use middleware::zenoh::{ZenohError, ZenohTransport};
use mw_gateway::bridge::Bridge;
use mw_gateway::io::{ChannelIo, pair};
use mw_gateway::shell::ShellOptions;
use mw_gateway::state::ControlStore;
use mw_gateway::{Gateway, SysForward};
use tokio::sync::{oneshot, watch};
use wasm_host::Limits;

use crate::controller::Controller;
use crate::device::{Clock, Out, SystemClock};
use crate::flash::Durable;
use crate::service::Facts;

/// Longest one frame write to a link may take.
pub const SEND_TIMEOUT: Duration = Duration::from_millis(200);

type Link = SerialLink<ChannelIo, 0>;

/// Why the device thread ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// It was asked to stop.
    Stopped,
    /// The gateway's end of the link closed.
    GatewayClosed,
}

/// What the device thread did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceReport {
    /// Why it ended; `None` when its runtime could not be built or the
    /// thread panicked.
    pub ended: Option<Ended>,
    /// The payload status at the end.
    pub status: Option<PayloadStatus>,
    /// Publications dropped because an outbox was full.
    pub dropped: u32,
    /// Frames whose send failed or timed out.
    pub send_failed: u32,
    /// Whether the flash was synced at the end.
    pub synced: bool,
}

/// An owned frame from a link.
enum Inbound {
    Request { id: u8, key: String, body: Vec<u8> },
    Publish { key: String, body: Vec<u8> },
    Other,
}

/// The next frame on `link`, copied out (cancel safe).
async fn recv(link: &mut Link, buf: &mut [u8]) -> Result<Inbound, ()> {
    match link.next_frame(buf).await {
        Ok(Frame::Request { id, key, payload }) => Ok(Inbound::Request {
            id,
            key: key.as_str().to_owned(),
            body: payload.to_vec(),
        }),
        Ok(Frame::Publish { key, payload }) => Ok(Inbound::Publish { key: key.as_str().to_owned(), body: payload.to_vec() }),
        Ok(Frame::Response { .. }) => Ok(Inbound::Other),
        Err(_) => Err(()),
    }
}

/// [`recv`] on a link that may be absent (then pending forever).
async fn recv_opt(link: Option<&mut Link>, buf: &mut [u8]) -> Result<Inbound, ()> {
    match link {
        Some(l) => recv(l, buf).await,
        None => std::future::pending().await,
    }
}

/// Sends `frame`, bounded by [`SEND_TIMEOUT`]; `false` on failure.
async fn send(link: &mut Link, frame: &Frame<'_>) -> bool {
    matches!(tokio::time::timeout(SEND_TIMEOUT, link.send(frame)).await, Ok(Ok(())))
}

struct Links {
    gateway: Link,
    local: Option<Link>,
    /// Requests over Zenoh; never sent publications.
    mesh: Option<Link>,
    send_failed: u32,
}

impl Links {
    fn count(&mut self, ok: bool) {
        if !ok {
            self.send_failed = self.send_failed.saturating_add(1);
        }
    }

    /// Sends the controller's queue to the gateway and local links (not
    /// the mesh link), logging `sys/log` lines on the process's log too.
    async fn broadcast(&mut self, outs: Vec<Out>) {
        for o in outs {
            let Ok(key) = Key::new(&o.key) else { continue };
            if key == keys::LOG {
                tracing::info!(target: "mw_posix_node::device", "{}", String::from_utf8_lossy(&o.payload));
            }
            let frame = Frame::Publish { key, payload: &o.payload };
            let ok = send(&mut self.gateway, &frame).await;
            self.count(ok);
            if let Some(l) = self.local.as_mut() {
                let ok = send(l, &frame).await;
                self.count(ok);
            }
        }
    }
}

/// Which link a frame came from.
#[derive(Clone, Copy)]
enum Origin {
    Gateway,
    Local,
    Mesh,
}

/// What ended one wait.
enum Woke {
    Stop,
    Timer,
    Frame(Origin, Result<Inbound, ()>),
}

/// The device thread's loop (module docs), until stopped or the gateway
/// link closes.
async fn run<F: NorFlash + Durable, C: Clock + Clone + 'static>(
    ctl: &mut Controller<F, C>,
    links: &mut Links,
    stop: &mut watch::Receiver<bool>,
) -> Ended {
    let mut gw_buf = vec![0u8; RECV_BUF_LEN];
    let mut local_buf = vec![0u8; RECV_BUF_LEN];
    let mut mesh_buf = vec![0u8; RECV_BUF_LEN];
    loop {
        ctl.poll();
        links.broadcast(ctl.drain()).await;
        if *stop.borrow() {
            return Ended::Stopped;
        }
        let delay = ctl.next_wake_us().map(|at| Duration::from_micros(at.saturating_sub(ctl.clock().now_us())));
        let timer = async move {
            match delay {
                Some(d) => tokio::time::sleep(d).await,
                None => std::future::pending().await,
            }
        };
        let woke = tokio::select! {
            _ = stop.changed() => Woke::Stop,
            r = recv(&mut links.gateway, &mut gw_buf) => Woke::Frame(Origin::Gateway, r),
            r = recv_opt(links.local.as_mut(), &mut local_buf) => Woke::Frame(Origin::Local, r),
            r = recv_opt(links.mesh.as_mut(), &mut mesh_buf) => Woke::Frame(Origin::Mesh, r),
            () = timer => Woke::Timer,
        };
        let (origin, inbound) = match woke {
            Woke::Stop => return Ended::Stopped,
            Woke::Timer => continue,
            Woke::Frame(origin, Ok(m)) => (origin, m),
            Woke::Frame(Origin::Gateway, Err(())) => return Ended::GatewayClosed,
            Woke::Frame(Origin::Local, Err(())) => {
                links.local = None;
                continue;
            }
            Woke::Frame(Origin::Mesh, Err(())) => {
                links.mesh = None;
                continue;
            }
        };
        match inbound {
            Inbound::Request { id, key, body } => {
                let result = match (Key::new(&key), origin) {
                    (Err(_), _) => Err(HandlerError::InvalidRequest),
                    (Ok(k), Origin::Mesh) => match crate::mesh_gate::unwrap(&body) {
                        Some((sender, body)) => ctl.mesh_request(k, sender, body),
                        None => Err(HandlerError::InvalidRequest),
                    },
                    (Ok(k), Origin::Gateway | Origin::Local) => ctl.request(k, &body, &RequestMeta::NONE),
                };
                let frame = Frame::Response { id, result: result.as_deref().map_err(|e| *e) };
                let link = match origin {
                    Origin::Gateway => Some(&mut links.gateway),
                    Origin::Local => links.local.as_mut(),
                    Origin::Mesh => links.mesh.as_mut(),
                };
                if let Some(l) = link {
                    let ok = send(l, &frame).await;
                    links.count(ok);
                }
            }
            Inbound::Publish { key, body } => {
                if let Ok(k) = Key::new(&key) {
                    ctl.publication(k, &body);
                }
            }
            Inbound::Other => {}
        }
    }
}

/// The running device thread.
#[derive(Debug)]
pub struct DeviceTask {
    stop: watch::Sender<bool>,
    done: Option<oneshot::Receiver<DeviceReport>>,
    finished: Option<DeviceReport>,
    thread: Option<JoinHandle<()>>,
}

/// Starts the device thread on `flash` with the gateway link `gateway`
/// and, optionally, a local and a mesh link.
pub fn spawn_device<F: NorFlash + Durable + Send + 'static>(
    flash: F,
    facts: Facts,
    limits: Limits,
    gateway: ChannelIo,
    local: Option<ChannelIo>,
    mesh: Option<ChannelIo>,
    driver: Box<dyn crate::driver::Driver>,
) -> io::Result<DeviceTask> {
    let (stop, mut stopped) = watch::channel(false);
    let (report, done) = oneshot::channel();
    let thread = std::thread::Builder::new().name("mw-posix-node-device".into()).spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build();
        let rt = match runtime {
            Ok(rt) => rt,
            Err(e) => {
                tracing::error!("mw-posix-node: device runtime: {e}");
                let _ = report.send(DeviceReport {
                    ended: None,
                    status: None,
                    dropped: 0,
                    send_failed: 0,
                    synced: false,
                });
                return;
            }
        };
        let r = rt.block_on(async move {
            let (mut ctl, boot) = Controller::open_with_driver(flash, facts, limits, SystemClock::start(), driver);
            let mut links = Links {
                gateway: SerialLink::with_inbox(gateway),
                local: local.map(SerialLink::with_inbox),
                mesh: mesh.map(SerialLink::with_inbox),
                send_failed: 0,
            };
            ctl.boot(&boot);
            let ended = run(&mut ctl, &mut links, &mut stopped).await;
            let synced = ctl.shutdown().is_ok();
            if ended == Ended::Stopped {
                links.broadcast(ctl.drain()).await;
            }
            DeviceReport {
                ended: Some(ended),
                status: Some(ctl.status()),
                dropped: ctl.dropped(),
                send_failed: links.send_failed,
                synced,
            }
        });
        let _ = report.send(r);
    })?;
    Ok(DeviceTask { stop, done: Some(done), finished: None, thread: Some(thread) })
}

impl DeviceTask {
    /// Completes when the thread has ended by itself (the gateway link
    /// closed). Await it at most once.
    pub async fn ended(&mut self) {
        if let Some(done) = self.done.take() {
            self.finished = done.await.ok();
        }
    }

    /// Stops the payload, syncs the flash and ends the thread.
    pub async fn stop(mut self) -> DeviceReport {
        let _ = self.stop.send(true);
        let report = match (self.finished.take(), self.done.take()) {
            (Some(r), _) => Some(r),
            (None, Some(done)) => done.await.ok(),
            (None, None) => None,
        };
        if let Some(t) = self.thread.take() {
            let _ = tokio::task::spawn_blocking(move || t.join()).await;
        }
        report.unwrap_or(DeviceReport { ended: None, status: None, dropped: 0, send_failed: 0, synced: false })
    }
}

/// Why the node did not start.
#[derive(Debug)]
pub enum StartError {
    /// The gateway's Zenoh subscribers could not be declared.
    Zenoh(ZenohError),
    /// The device thread could not be spawned.
    Thread(io::Error),
}

impl core::fmt::Display for StartError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            StartError::Zenoh(e) => write!(f, "zenoh: {e}"),
            StartError::Thread(e) => write!(f, "device thread: {e}"),
        }
    }
}

impl std::error::Error for StartError {}

/// A running node: the device thread and the gateway in front of it.
#[derive(Debug)]
pub struct PosixNode {
    device: DeviceTask,
    gateway: Gateway,
}

/// What a stopped node did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The device thread's report.
    pub device: DeviceReport,
    /// The gateway's report.
    pub gateway: mw_gateway::Report,
}

/// The device half of a node.
#[derive(Debug)]
pub struct DeviceParts<F> {
    /// The flash (`crate::open_state_flash`).
    pub flash: F,
    /// What `sys/info` reports.
    pub facts: Facts,
    /// The limits payloads run under.
    pub limits: Limits,
    /// A second link beside the gateway's (`MW_POSIX_NODE_LOCAL_SERIAL`).
    pub local: Option<ChannelIo>,
    /// The mesh link: the device end of a `ChannelIo` pair whose other end
    /// `crate::zenoh_sys::run` serves (`MW_POSIX_NODE_ZENOH_SYS`).
    pub mesh: Option<ChannelIo>,
    /// The control-module driver (`MW_POSIX_NODE_DRIVER`, [`crate::driver`]).
    pub driver: Box<dyn crate::driver::Driver>,
}

/// The gateway half of a node, as `mw-gateway`'s own binary builds it:
/// the bridge with the control counter floor
/// (`GatewayConfig::bridge_with_floor` and `state::floor_at_start`, see
/// [`crate::control_floor`]), the shell's timeouts and the store the
/// accepted start/stop counters are persisted in. With no floor, or
/// without updaters, signed start/stop through `gw/sys` are refused.
#[derive(Debug)]
pub struct GatewayParts {
    /// The bridge.
    pub bridge: Bridge,
    /// The shell's timeouts.
    pub options: ShellOptions,
    /// The control counter store.
    pub store: ControlStore,
}

/// Starts the node (module docs): the gateway on `session`
/// (`mw_gateway::start_with_store`) and the device thread. Returns the
/// node and the gateway's `gw/sys` handler for the service runner.
pub async fn start<F: NorFlash + Durable + Send + 'static>(
    device: DeviceParts<F>,
    gateway: GatewayParts,
    session: ZenohTransport,
) -> Result<(PosixNode, SysForward), StartError> {
    let DeviceParts { flash, facts, limits, local, mesh, driver } = device;
    let GatewayParts { bridge, options, store } = gateway;
    let (device_io, gateway_io) = pair();
    let (gateway, sys) = mw_gateway::start_with_store(gateway_io, session, bridge, options, store)
        .await
        .map_err(StartError::Zenoh)?;
    let device = spawn_device(flash, facts, limits, device_io, local, mesh, driver).map_err(StartError::Thread)?;
    Ok((PosixNode { device, gateway }, sys))
}

impl PosixNode {
    /// Completes when the device thread has ended by itself. Await it at
    /// most once.
    pub async fn device_ended(&mut self) {
        self.device.ended().await;
    }

    /// Stops the device (payload stopped, flash synced), then the gateway
    /// (which withdraws what it announced), and reports.
    pub async fn stop(self) -> Report {
        let device = self.device.stop().await;
        let gateway = self.gateway.stop().await;
        Report { device, gateway }
    }
}
