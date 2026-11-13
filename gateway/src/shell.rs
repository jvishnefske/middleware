//! The gateway's imperative shell: the tasks that perform what
//! [`Bridge`] decides.
//!
//! # Tasks and queues
//!
//! ```text
//!  device ──serial── link task ──uplink (32)──▶ zenoh task ──▶ Zenoh put
//!                     │  ▲  └──control (16)──▶   (publisher, discovery)
//!                     │  └──commands (16)── downlink tasks ◀── Zenoh subscribers
//!                     │                  └── gw/sys handler (SysForward)
//! ```
//!
//! * The **link task** owns the `SerialLink` and the `Bridge`. It waits
//!   for whichever comes first: a frame from the device
//!   (`SerialLink::next_frame`, cancel safe), a command, or a tick
//!   ([`TICK`]). On ticks it expires records, times out device requests
//!   and sends `sys/info` to learn (and every [`REBIND_EVERY`] to recheck)
//!   the device's node id. Sends to the device are bounded by
//!   [`SEND_TIMEOUT`]; a cut frame is completed by the next send.
//! * The **zenoh task** owns the session's publisher and its
//!   `ZenohDiscovery`; it publishes uplink samples and announces or
//!   withdraws services. Discovery has its own small queue so a burst of
//!   camera chunks cannot crowd out an announcement.
//! * One **downlink task** per downlink filter forwards matching samples
//!   to the link task, which maps them ([`Bridge::downlink`]).
//! * [`SysForward`], the `gw/sys` [`ServiceHandler`], hands each request
//!   with its caller (principal and `x-request-id`) to the link task, which
//!   applies the allowlist ([`Bridge::sys_route`]) or, for update keys, the
//!   update gate ([`Bridge::sys_update`], [`crate::update`]) and forwards
//!   or answers it; the handler waits at most its timeout and the caller's
//!   deadline.
//! * [`serve_sys`] answers `gw/sys` gRPC calls from a
//!   [`GrpcResponder`](middleware::grpc::GrpcResponder) with a
//!   `SysForward`, for a process that serves the kind itself rather than
//!   through the `mw-service` runner.
//!
//! # Update requests and timing
//!
//! A forwarded update request waits at the device up to the update
//! timeout ([`UPDATE_TIMEOUT`], `MW_GATEWAY_UPDATE_TIMEOUT_MS`), longer
//! than a plain request ([`SYS_TIMEOUT`]): a `begin` erases flash and a
//! firmware `commit` hashes the image. The caller may give up sooner (its
//! deadline); the device's late answer is still recorded by the update
//! gate, so the caller's resend with the same request id is answered from
//! it instead of acting twice. An answer that never comes within the
//! update timeout is abandoned ([`Bridge::update_abandoned`]). After
//! binding, and with every recheck, the link task also asks
//! `sys/trust/info`, which tells the gate whether the device is claimed.
//!
//! Every queue is a bounded tokio channel written with `try_send`: when it
//! is full the **newest** item is dropped and counted ([`Drops`]); nothing
//! queued is ever displaced. Ownership is a tree: each task owns its
//! state, the channels are its only links, nothing is shared.

use middleware::discovery::{Announce, ServiceKind, ServiceRecord};
use middleware::key::{AsKey, Key, KeyBuf};
use middleware::loader::keys;
use middleware::serial::frame::Frame;
use middleware::serial::link::{LinkError, RECV_BUF_LEN, SerialLink};
use middleware::transport::{HandlerError, Publisher, Request, RequestMeta, Subscribe, Subscriber};
use middleware::zenoh::{ZenohDiscovery, ZenohError, ZenohSubscriber, ZenohTransport};
use mw_service::{Ctx, ServiceHandler};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::bridge::{Bound, Bridge, Counters, OwnedKey, Pending, SysRoute, Uplink};
use crate::io::ChannelIo;
use crate::update::{Admission, Caller, SysClass, Ticket, UpdateCounters, classify};

/// The link task's timer period.
pub const TICK: Duration = Duration::from_millis(250);
/// Longest one frame write to the device may take.
pub const SEND_TIMEOUT: Duration = Duration::from_millis(200);
/// How long a `gw/sys` request waits for the device by default.
pub const SYS_TIMEOUT: Duration = Duration::from_secs(2);
/// How long an update request waits for the device by default.
pub const UPDATE_TIMEOUT: Duration = Duration::from_secs(20);
/// Largest `gw/sys` answer [`serve_sys`] returns: one serial frame.
pub const SYS_ANSWER_MAX: usize = RECV_BUF_LEN;
/// How often `sys/info` is sent while the device is unbound.
pub const BIND_RETRY: Duration = Duration::from_secs(2);
/// How often `sys/info` is sent again once bound (a swapped device).
pub const REBIND_EVERY: Duration = Duration::from_secs(30);
/// Uplink (data) queue depth.
pub const UPLINK_QUEUE: usize = 32;
/// Discovery queue depth.
pub const CONTROL_QUEUE: usize = 16;
/// Command queue depth (downlink samples and `gw/sys` requests).
pub const COMMAND_QUEUE: usize = 16;
/// Largest downlink sample: one serial frame's worth.
pub const MAX_DOWNLINK_SAMPLE: usize = RECV_BUF_LEN;

type Reply = oneshot::Sender<Result<Vec<u8>, HandlerError>>;

/// What the link task is asked to do.
#[derive(Debug)]
enum Command {
    /// Relay a cluster sample (cluster key) to the device, if it maps.
    Downlink { key: OwnedKey, payload: Vec<u8> },
    /// A `gw/sys` request, decided by the link task.
    Sys {
        key: OwnedKey,
        body: Vec<u8>,
        caller: OwnedCaller,
        reply: Reply,
    },
}

/// A [`Caller`] that can wait in a queue.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct OwnedCaller {
    principal: Option<String>,
    request_id: Option<String>,
}

impl OwnedCaller {
    fn as_caller(&self) -> Caller<'_> {
        Caller {
            principal: self.principal.as_deref(),
            request_id: self.request_id.as_deref(),
        }
    }
}

/// What the link task asks of the zenoh task.
#[derive(Debug)]
enum ToCluster {
    Publish { key: OwnedKey, payload: Vec<u8> },
    Announce(Box<ServiceRecord>),
    Withdraw { kind: OwnedKey, instance: String },
}

/// Who waits for a device response.
#[derive(Debug)]
enum Waiter {
    /// The gateway's own `sys/info`.
    Bind,
    /// The gateway's own `sys/trust/info`.
    Trust,
    /// A `gw/sys` caller.
    Caller(Reply),
    /// A `gw/sys` update request the gate admitted; the caller may have
    /// stopped waiting, the answer is recorded either way.
    Update { reply: Reply, ticket: Ticket },
}

/// Items dropped by the shell (saturating), by queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Drops {
    /// Uplink samples dropped: the uplink queue was full.
    pub uplink: u32,
    /// Announcements or withdrawals dropped: the control queue was full.
    pub control: u32,
    /// Downlink samples dropped: the command queue was full, or the
    /// sample did not fit [`MAX_DOWNLINK_SAMPLE`].
    pub downlink: u32,
    /// `gw/sys` requests turned away: queue or [`MAX_PENDING`](crate::bridge::MAX_PENDING)
    /// full, or the send failed.
    pub sys: u32,
    /// Frames whose send to the device failed or timed out.
    pub send_failed: u32,
    /// Zenoh publish, announce or withdraw calls that failed.
    pub zenoh_failed: u32,
}

/// What a stopped gateway did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The bridge's decision counters.
    pub counters: Counters,
    /// The shell's drops.
    pub drops: Drops,
    /// The node the device was bound to.
    pub device: Option<middleware::SenderId>,
    /// The update gate's decisions.
    pub updates: UpdateCounters,
}

/// The shell's timeouts ([`start_with`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShellOptions {
    /// How long a plain `gw/sys` request (and the gateway's own
    /// `sys/info`) waits for the device.
    pub sys_timeout: Duration,
    /// How long an update request waits for the device.
    pub update_timeout: Duration,
}

impl Default for ShellOptions {
    fn default() -> Self {
        ShellOptions {
            sys_timeout: SYS_TIMEOUT,
            update_timeout: UPDATE_TIMEOUT,
        }
    }
}

fn bump(n: &mut u32) {
    *n = n.saturating_add(1);
}

/// The `gw/sys` handler (module docs). Holds only a queue sender.
#[derive(Debug)]
pub struct SysForward {
    commands: mpsc::Sender<Command>,
    timeout: Duration,
    update_timeout: Duration,
}

/// How long the handler waits: its timeout for the key's class, cut to the
/// caller's deadline. Pure.
pub fn sys_wait(
    key: Key<'_>,
    timeout: Duration,
    update_timeout: Duration,
    deadline_ms: Option<u32>,
) -> Duration {
    let own = match classify(key) {
        SysClass::Update(_) => update_timeout,
        SysClass::Never | SysClass::Plain => timeout,
    };
    deadline_ms.map_or(own, |d| own.min(Duration::from_millis(u64::from(d))))
}

impl SysForward {
    /// A handler attached to no gateway: every request is
    /// [`HandlerError::Unavailable`]. For `--print-spec` and
    /// `--print-manifests`, which need the service but run nothing.
    pub fn detached() -> Self {
        let (commands, _) = mpsc::channel(1);
        SysForward {
            commands,
            timeout: SYS_TIMEOUT,
            update_timeout: UPDATE_TIMEOUT,
        }
    }

    /// The same handler waiting at most `timeout` for the device.
    pub fn with_timeout(self, timeout: Duration) -> Self {
        SysForward { timeout, ..self }
    }

    /// The same handler waiting at most `timeout` for an update request.
    pub fn with_update_timeout(self, timeout: Duration) -> Self {
        SysForward {
            update_timeout: timeout,
            ..self
        }
    }

    /// Answers one `gw/sys` request body (`[key_len][key][body]`) into
    /// `out`, for an anonymous caller (no principal: update requests are
    /// `Denied`). See [`SysForward::forward_as`].
    pub async fn forward(&mut self, body: &[u8], out: &mut [u8]) -> Result<usize, HandlerError> {
        self.forward_as(&RequestMeta::NONE, body, out).await
    }

    /// Answers one `gw/sys` request body (`[key_len][key][body]`) into
    /// `out` for the caller `meta` describes (its principal, request id
    /// and deadline). The link task decides with the allowlist
    /// ([`Bridge::sys_route`]) or the update gate ([`Bridge::sys_update`]),
    /// both counted; a refused request never reaches the device. A full
    /// queue is [`HandlerError::Unavailable`], no answer within
    /// [`sys_wait`] [`HandlerError::DeadlineExceeded`].
    pub async fn forward_as(
        &mut self,
        meta: &RequestMeta<'_>,
        body: &[u8],
        out: &mut [u8],
    ) -> Result<usize, HandlerError> {
        let (key, inner) = crate::bridge::parse_sys_request(body)?;
        let wait = sys_wait(key, self.timeout, self.update_timeout, meta.deadline_ms);
        let key = KeyBuf::from_key(key).map_err(|_| HandlerError::InvalidRequest)?;
        let (reply, answer) = oneshot::channel();
        let caller = OwnedCaller {
            principal: meta.principal.map(str::to_owned),
            request_id: meta.request_id.map(|r| r.as_str().to_owned()),
        };
        self.commands
            .try_send(Command::Sys {
                key,
                body: inner.to_vec(),
                caller,
                reply,
            })
            .map_err(|_| HandlerError::Unavailable)?;
        let bytes = tokio::time::timeout(wait, answer)
            .await
            .map_err(|_| HandlerError::DeadlineExceeded)?
            .map_err(|_| HandlerError::Unavailable)??;
        let dst = out
            .get_mut(..bytes.len())
            .ok_or(HandlerError::BufferTooSmall)?;
        dst.copy_from_slice(&bytes);
        Ok(bytes.len())
    }
}

impl ServiceHandler for SysForward {
    async fn handle(
        &mut self,
        ctx: &mut Ctx<'_>,
        req: &Request<'_>,
        out: &mut [u8],
    ) -> Result<usize, HandlerError> {
        let mut meta = *ctx.meta();
        if let Some(at) = ctx.deadline() {
            let left = mw_service::ctx::remaining(at, std::time::Instant::now());
            meta.deadline_ms = Some(u32::try_from(left.as_millis()).unwrap_or(u32::MAX));
        }
        self.forward_as(&meta, req.body, out).await
    }
}

/// Answers `gw/sys` calls taken from `responder` with `sys`, one at a
/// time, until the responder's service is gone. Each call's principal
/// comes from the responder's [`PrincipalSource`](middleware::grpc::PrincipalSource)
/// (`SidecarXfcc` behind an Istio or Envoy sidecar; with `None` every
/// caller is anonymous and update requests are `Denied`); any key other
/// than [`SYS_KIND`] is `NotFound`. For a process that serves `gw/sys`
/// itself (an embedding, a test) instead of through the `mw-service`
/// runner.
pub async fn serve_sys(responder: middleware::grpc::GrpcResponder, sys: SysForward) {
    let mut responder = responder;
    let mut sys = sys;
    let mut out = vec![0u8; SYS_ANSWER_MAX];
    while let Ok(call) = responder.next_call().await {
        let result = match call.key() {
            Some(k) if k.as_str() == SYS_KIND => {
                let meta = call.meta();
                sys.forward_as(&meta, call.body(), &mut out).await
            }
            Some(_) => Err(HandlerError::NotFound),
            None => Err(HandlerError::InvalidRequest),
        };
        match result {
            Ok(n) => call.respond(Ok(out.get(..n).unwrap_or_default())),
            Err(e) => call.respond(Err(e)),
        }
    }
}

/// A running gateway; [`Gateway::stop`] ends it.
#[derive(Debug)]
pub struct Gateway {
    stop: watch::Sender<bool>,
    closed: oneshot::Receiver<()>,
    link: JoinHandle<(Bridge, Drops)>,
    zenoh: JoinHandle<Drops>,
    downlinks: Vec<JoinHandle<u32>>,
}

/// The `gw/sys` kind the gateway serves.
pub const SYS_KIND: &str = "gw/sys";

/// Starts the gateway on the device stream `io` and the Zenoh session
/// `session`, deciding with `bridge`. Declares one Zenoh subscriber per
/// downlink filter first (so a failure is reported here), then spawns the
/// tasks. Returns the running gateway and its `gw/sys` handler. The
/// default timeouts ([`ShellOptions::default`]); see [`start_with`].
pub async fn start(
    io: ChannelIo,
    session: ZenohTransport,
    bridge: Bridge,
) -> Result<(Gateway, SysForward), ZenohError> {
    start_with(io, session, bridge, ShellOptions::default()).await
}

/// [`start`] with the timeouts `options` (the returned handler waits as
/// long as the link task does).
pub async fn start_with(
    io: ChannelIo,
    session: ZenohTransport,
    bridge: Bridge,
    options: ShellOptions,
) -> Result<(Gateway, SysForward), ZenohError> {
    let mut session = session;
    let mut subscribers = Vec::new();
    for f in bridge.downlink_filters() {
        subscribers.push(session.subscribe(f.as_filter()).await?);
    }
    let discovery = session.discovery();
    let (stop, stopped) = watch::channel(false);
    let (commands, command_rx) = mpsc::channel(COMMAND_QUEUE);
    let (uplink, uplink_rx) = mpsc::channel(UPLINK_QUEUE);
    let (control, control_rx) = mpsc::channel(CONTROL_QUEUE);
    let sys = SysForward {
        commands: commands.clone(),
        timeout: options.sys_timeout,
        update_timeout: options.update_timeout,
    };
    let downlinks = subscribers
        .into_iter()
        .map(|s| tokio::spawn(downlink_task(s, commands.clone(), stopped.clone())))
        .collect();
    drop(commands);
    let link = SerialLink::<ChannelIo, 0>::with_inbox(io);
    let (done, closed) = oneshot::channel();
    let link = tokio::spawn(
        LinkTask {
            _done: done,
            bridge,
            pending: Pending::new(),
            drops: Drops::default(),
            uplink,
            control,
            started: Instant::now(),
            next_bind_ms: 0,
            sys_timeout: options.sys_timeout,
            update_timeout: options.update_timeout,
            trust_due: false,
        }
        .run(link, command_rx, stopped.clone()),
    );
    let zenoh = tokio::spawn(zenoh_task(
        session, discovery, uplink_rx, control_rx, stopped,
    ));
    Ok((
        Gateway {
            stop,
            closed,
            link,
            zenoh,
            downlinks,
        },
        sys,
    ))
}

impl Gateway {
    /// Completes when the link task has ended (the device stream closed).
    /// Await it at most once.
    pub async fn link_closed(&mut self) {
        let _ = (&mut self.closed).await;
    }

    /// Stops every task and reports. The zenoh task withdraws what it
    /// announced before it ends.
    pub async fn stop(self) -> Report {
        let _ = self.stop.send(true);
        self.wait().await
    }

    /// Waits until the tasks end by themselves (the device stream closed)
    /// and reports.
    pub async fn wait(self) -> Report {
        let (bridge, mut drops) = match self.link.await {
            Ok(r) => r,
            Err(_) => (
                Bridge::new(
                    crate::bridge::KeyMap::identity(),
                    Vec::new(),
                    crate::bridge::SysAllow::default(),
                    None,
                ),
                Drops::default(),
            ),
        };
        // The link task is gone, so the queues are closed: the others end.
        let _ = self.stop.send(true);
        if let Ok(z) = self.zenoh.await {
            drops.zenoh_failed = drops.zenoh_failed.saturating_add(z.zenoh_failed);
        }
        for d in self.downlinks {
            if let Ok(n) = d.await {
                drops.downlink = drops.downlink.saturating_add(n);
            }
        }
        Report {
            counters: bridge.counters(),
            drops,
            device: bridge.device(),
            updates: bridge.updates().counters(),
        }
    }
}

struct LinkTask {
    /// Dropped when the task ends ([`Gateway::link_closed`]).
    _done: oneshot::Sender<()>,
    bridge: Bridge,
    pending: Pending<Waiter>,
    drops: Drops,
    uplink: mpsc::Sender<ToCluster>,
    control: mpsc::Sender<ToCluster>,
    started: Instant,
    next_bind_ms: u64,
    sys_timeout: Duration,
    update_timeout: Duration,
    /// Ask `sys/trust/info` on the next tick (after a bind).
    trust_due: bool,
}

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

impl LinkTask {
    fn now_ms(&self) -> u64 {
        ms(self.started.elapsed())
    }

    async fn run<IO>(
        self,
        mut link: SerialLink<IO, 0>,
        mut commands: mpsc::Receiver<Command>,
        mut stopped: watch::Receiver<bool>,
    ) -> (Bridge, Drops)
    where
        IO: embedded_io_async::Read + embedded_io_async::Write,
    {
        let mut task = self;
        let mut buf = vec![0u8; RECV_BUF_LEN];
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = stopped.changed() => break,
                _ = tick.tick() => task = task.on_tick(&mut link).await,
                Some(c) = commands.recv() => task = task.on_command(&mut link, c).await,
                f = link.next_frame(&mut buf) => match f {
                    Ok(frame) => task = task.on_frame(frame),
                    Err(LinkError::Io(_)) => break,
                    Err(_) => {}
                },
            }
        }
        // Fail every waiter now rather than at its deadline.
        let (_, waiters) = task.pending.expire(u64::MAX);
        for w in waiters {
            match w {
                Waiter::Caller(r) => {
                    let _ = r.send(Err(HandlerError::Unavailable));
                }
                Waiter::Update { reply, ticket } => {
                    task.bridge = task.bridge.update_abandoned(&ticket);
                    let _ = reply.send(Err(HandlerError::Unavailable));
                }
                Waiter::Bind | Waiter::Trust => {}
            }
        }
        (task.bridge, task.drops)
    }

    fn enqueue(mut self, item: ToCluster, control: bool) -> Self {
        let queue = if control { &self.control } else { &self.uplink };
        if queue.try_send(item).is_err() {
            if control {
                bump(&mut self.drops.control);
            } else {
                bump(&mut self.drops.uplink);
            }
        }
        self
    }

    fn withdraw(self, gone: Vec<(OwnedKey, String)>) -> Self {
        gone.into_iter().fold(self, |t, (kind, instance)| {
            t.enqueue(ToCluster::Withdraw { kind, instance }, true)
        })
    }

    fn on_frame(mut self, frame: Frame<'_>) -> Self {
        let now = self.now_ms();
        match frame {
            Frame::Publish { key, payload } => {
                let (bridge, up) = self.bridge.uplink(key, payload, now);
                self.bridge = bridge;
                match up {
                    Uplink::Publish(key) => self.enqueue(
                        ToCluster::Publish {
                            key,
                            payload: payload.to_vec(),
                        },
                        false,
                    ),
                    Uplink::Announce(r) => self.enqueue(ToCluster::Announce(r), true),
                    Uplink::Withdraw { kind, instance } => {
                        self.enqueue(ToCluster::Withdraw { kind, instance }, true)
                    }
                    Uplink::Refuse(why) => {
                        tracing::debug!(key = key.as_str(), ?why, "not bridged");
                        self
                    }
                    Uplink::Log | Uplink::Nothing => self,
                }
            }
            Frame::Response { id, result } => {
                let (pending, waiter) = self.pending.take(id);
                self.pending = pending;
                match (waiter, result) {
                    (Some(Waiter::Caller(reply)), result) => {
                        let _ = reply.send(result.map(<[u8]>::to_vec));
                        self
                    }
                    (Some(Waiter::Update { reply, ticket }), result) => {
                        self.bridge = self.bridge.update_done(&ticket, result, now);
                        let _ = reply.send(result.map(<[u8]>::to_vec));
                        self
                    }
                    (Some(Waiter::Trust), result) => {
                        self.bridge = self.bridge.observe_trust(result);
                        self
                    }
                    (Some(Waiter::Bind), Ok(info)) => self.on_info(info),
                    _ => self,
                }
            }
            // The device sends no requests.
            Frame::Request { .. } => self,
        }
    }

    fn on_info(mut self, info: &[u8]) -> Self {
        let (bridge, bound) = self.bridge.bind(info);
        self.bridge = bridge;
        match bound {
            Bound::Node { node, withdraw } => {
                tracing::info!(node = ?node.0, "device bound");
                self.trust_due = true;
                self.withdraw(withdraw)
            }
            Bound::NoNode => {
                tracing::warn!("sys/info names no node id; beacons stay refused");
                self
            }
            Bound::Mismatch(node) => {
                tracing::warn!(node = ?node.0, "device is not the pinned MAC; beacons stay refused");
                self
            }
        }
    }

    /// Sends `frame` within [`SEND_TIMEOUT`]; counts a failure.
    async fn send<IO>(mut self, link: &mut SerialLink<IO, 0>, frame: &Frame<'_>) -> (Self, bool)
    where
        IO: embedded_io_async::Read + embedded_io_async::Write,
    {
        let ok = matches!(
            tokio::time::timeout(SEND_TIMEOUT, link.send(frame)).await,
            Ok(Ok(()))
        );
        if !ok {
            bump(&mut self.drops.send_failed);
        }
        (self, ok)
    }

    async fn on_tick<IO>(mut self, link: &mut SerialLink<IO, 0>) -> Self
    where
        IO: embedded_io_async::Read + embedded_io_async::Write,
    {
        let now = self.now_ms();
        let (bridge, gone) = self.bridge.expire(now);
        self.bridge = bridge;
        let mut task = self.withdraw(gone);
        let (pending, late) = task.pending.expire(now);
        task.pending = pending;
        for w in late {
            match w {
                Waiter::Caller(r) => {
                    let _ = r.send(Err(HandlerError::DeadlineExceeded));
                }
                Waiter::Update { reply, ticket } => {
                    task.bridge = task.bridge.update_abandoned(&ticket);
                    let _ = reply.send(Err(HandlerError::DeadlineExceeded));
                }
                Waiter::Bind | Waiter::Trust => {}
            }
        }
        if task.trust_due && !task.pending.any(|w| matches!(w, Waiter::Trust)) {
            task.trust_due = false;
            let deadline = now.saturating_add(ms(task.sys_timeout));
            match task.pending.insert(Waiter::Trust, deadline) {
                Err((pending, _)) => {
                    task.pending = pending;
                    task.trust_due = true;
                }
                Ok((pending, id)) => {
                    task.pending = pending;
                    let frame = Frame::Request {
                        id,
                        key: keys::TRUST_INFO,
                        payload: &[],
                    };
                    task = task.send(link, &frame).await.0;
                }
            }
        }
        let binding = task.pending.any(|w| matches!(w, Waiter::Bind));
        if binding || now < task.next_bind_ms {
            return task;
        }
        let every = if task.bridge.device().is_some() {
            REBIND_EVERY
        } else {
            BIND_RETRY
        };
        task.next_bind_ms = now.saturating_add(ms(every));
        let deadline = now.saturating_add(ms(task.sys_timeout));
        match task.pending.insert(Waiter::Bind, deadline) {
            Err((pending, _)) => {
                task.pending = pending;
                task
            }
            Ok((pending, id)) => {
                task.pending = pending;
                let frame = Frame::Request {
                    id,
                    key: keys::INFO,
                    payload: &[],
                };
                task.send(link, &frame).await.0
            }
        }
    }

    async fn on_command<IO>(mut self, link: &mut SerialLink<IO, 0>, command: Command) -> Self
    where
        IO: embedded_io_async::Read + embedded_io_async::Write,
    {
        match command {
            Command::Downlink { key, payload } => {
                let Some(device_key) = self.bridge.downlink(key.as_key()) else {
                    return self;
                };
                let frame = Frame::Publish {
                    key: device_key.as_key(),
                    payload: &payload,
                };
                let (mut task, ok) = self.send(link, &frame).await;
                if ok {
                    task.bridge = task.bridge.downlinked();
                }
                task
            }
            Command::Sys {
                key,
                body,
                caller,
                reply,
            } => {
                if let SysClass::Update(op) = classify(key.as_key()) {
                    let now = self.now_ms();
                    let (bridge, decision) =
                        self.bridge
                            .sys_update(caller.as_caller(), op, key.as_key(), &body, now);
                    self.bridge = bridge;
                    return match decision {
                        Err(e) => {
                            tracing::debug!(key = key.as_str(), ?e, "update refused");
                            let _ = reply.send(Err(e));
                            self
                        }
                        Ok(Admission::Replay(r)) => {
                            let _ = reply.send(r);
                            self
                        }
                        Ok(Admission::Forward { body, ticket }) => {
                            let deadline = now.saturating_add(ms(self.update_timeout));
                            self.request_device(
                                link,
                                key.as_key(),
                                &body,
                                Waiter::Update { reply, ticket },
                                deadline,
                            )
                            .await
                        }
                    };
                }
                let (bridge, route) = self.bridge.sys_route(key.as_key());
                self.bridge = bridge;
                match route {
                    Err(e) => {
                        let _ = reply.send(Err(e));
                        self
                    }
                    Ok(SysRoute::LogTail) => {
                        let mut out = vec![0u8; crate::bridge::LOG_TAIL_BYTES];
                        let n = self.bridge.log_tail().read_into(&mut out);
                        out.truncate(n);
                        let _ = reply.send(Ok(out));
                        self
                    }
                    Ok(SysRoute::Device) => {
                        self.sys_to_device(link, key.as_key(), &body, reply).await
                    }
                }
            }
        }
    }

    async fn sys_to_device<IO>(
        self,
        link: &mut SerialLink<IO, 0>,
        key: Key<'_>,
        body: &[u8],
        reply: Reply,
    ) -> Self
    where
        IO: embedded_io_async::Read + embedded_io_async::Write,
    {
        let deadline = self.now_ms().saturating_add(ms(self.sys_timeout));
        self.request_device(link, key, body, Waiter::Caller(reply), deadline)
            .await
    }

    /// Fails `waiter` with `Unavailable` (an update one is abandoned).
    fn fail(mut self, waiter: Waiter) -> Self {
        match waiter {
            Waiter::Caller(r) => {
                let _ = r.send(Err(HandlerError::Unavailable));
            }
            Waiter::Update { reply, ticket } => {
                self.bridge = self.bridge.update_abandoned(&ticket);
                let _ = reply.send(Err(HandlerError::Unavailable));
            }
            Waiter::Bind | Waiter::Trust => {}
        }
        self
    }

    /// Sends a request for `waiter`, answered by `deadline_ms`.
    async fn request_device<IO>(
        mut self,
        link: &mut SerialLink<IO, 0>,
        key: Key<'_>,
        body: &[u8],
        waiter: Waiter,
        deadline_ms: u64,
    ) -> Self
    where
        IO: embedded_io_async::Read + embedded_io_async::Write,
    {
        match self.pending.insert(waiter, deadline_ms) {
            Err((pending, waiter)) => {
                self.pending = pending;
                bump(&mut self.drops.sys);
                self.fail(waiter)
            }
            Ok((pending, id)) => {
                self.pending = pending;
                let frame = Frame::Request {
                    id,
                    key,
                    payload: body,
                };
                let (mut task, ok) = self.send(link, &frame).await;
                if !ok {
                    bump(&mut task.drops.sys);
                    let (pending, waiter) = task.pending.take(id);
                    task.pending = pending;
                    if let Some(w) = waiter {
                        task = task.fail(w);
                    }
                }
                task
            }
        }
    }
}

async fn zenoh_task(
    mut session: ZenohTransport,
    mut discovery: ZenohDiscovery,
    mut uplink: mpsc::Receiver<ToCluster>,
    mut control: mpsc::Receiver<ToCluster>,
    mut stopped: watch::Receiver<bool>,
) -> Drops {
    let mut drops = Drops::default();
    let mut announced: Vec<(OwnedKey, String)> = Vec::new();
    loop {
        let item = tokio::select! {
            biased;
            _ = stopped.changed() => break,
            Some(c) = control.recv() => c,
            Some(u) = uplink.recv() => u,
            else => break,
        };
        let ok = match item {
            ToCluster::Publish { key, payload } => {
                session.publish(key.as_key(), &payload).await.is_ok()
            }
            ToCluster::Announce(record) => {
                let r = discovery.announce(&record.service()).await;
                if r.is_ok() {
                    let s = record.service();
                    if let Ok(kind) = KeyBuf::from_key(s.kind.key()) {
                        announced.retain(|(k, i)| !(k == &kind && i == s.instance));
                        announced.push((kind, s.instance.to_owned()));
                    }
                } else {
                    tracing::warn!(error = ?r, "announce failed");
                }
                r.is_ok()
            }
            ToCluster::Withdraw { kind, instance } => {
                announced.retain(|(k, i)| !(k == &kind && i == &instance));
                discovery
                    .withdraw(ServiceKind::new(kind.as_key()), &instance)
                    .await
                    .is_ok()
            }
        };
        if !ok {
            bump(&mut drops.zenoh_failed);
        }
    }
    for (kind, instance) in announced {
        let _ = discovery
            .withdraw(ServiceKind::new(kind.as_key()), &instance)
            .await;
    }
    drops
}

async fn downlink_task(
    mut subscriber: ZenohSubscriber,
    commands: mpsc::Sender<Command>,
    mut stopped: watch::Receiver<bool>,
) -> u32 {
    let mut dropped = 0u32;
    let mut buf = vec![0u8; MAX_DOWNLINK_SAMPLE];
    loop {
        tokio::select! {
            biased;
            _ = stopped.changed() => break,
            r = subscriber.recv(&mut buf) => match r {
                Ok(sample) => {
                    let sent = KeyBuf::from_key(sample.key).ok().is_some_and(|key| {
                        commands
                            .try_send(Command::Downlink { key, payload: sample.payload.to_vec() })
                            .is_ok()
                    });
                    if !sent {
                        bump(&mut dropped);
                    }
                }
                Err(ZenohError::Closed) => break,
                Err(_) => bump(&mut dropped),
            },
        }
    }
    dropped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{KeyMap, SysAllow};

    fn task(uplink: mpsc::Sender<ToCluster>, control: mpsc::Sender<ToCluster>) -> LinkTask {
        LinkTask {
            _done: oneshot::channel().0,
            bridge: Bridge::new(KeyMap::identity(), Vec::new(), SysAllow::default(), None),
            pending: Pending::new(),
            drops: Drops::default(),
            uplink,
            control,
            started: Instant::now(),
            next_bind_ms: 0,
            sys_timeout: SYS_TIMEOUT,
            update_timeout: UPDATE_TIMEOUT,
            trust_due: false,
        }
    }

    fn publish(n: u8) -> ToCluster {
        ToCluster::Publish {
            key: OwnedKey::new("app/x").expect("key"),
            payload: vec![n],
        }
    }

    #[tokio::test]
    async fn rg13_full_queues_drop_the_newest_and_count() {
        let (up, mut up_rx) = mpsc::channel(2);
        let (ctl, mut ctl_rx) = mpsc::channel(1);
        let t = task(up, ctl)
            .enqueue(publish(1), false)
            .enqueue(publish(2), false)
            .enqueue(publish(3), false)
            .withdraw(vec![
                (OwnedKey::new("cam/jpeg").expect("key"), "a".into()),
                (OwnedKey::new("cam/jpeg").expect("key"), "b".into()),
            ]);
        assert_eq!((t.drops.uplink, t.drops.control), (1, 1));
        // The queued items are the oldest; the newest went.
        for want in [1u8, 2] {
            match up_rx.recv().await {
                Some(ToCluster::Publish { payload, .. }) => assert_eq!(payload, [want]),
                other => panic!("{other:?}"),
            }
        }
        assert!(up_rx.try_recv().is_err());
        assert!(matches!(
            ctl_rx.recv().await,
            Some(ToCluster::Withdraw { ref instance, .. }) if instance == "a"
        ));
    }

    #[tokio::test]
    async fn rg13_detached_sys_forward_is_unavailable() {
        let mut sys = SysForward::detached().with_timeout(Duration::from_millis(10));
        let mut body = [0u8; 16];
        let n = crate::bridge::encode_sys_request(keys::INFO, b"", &mut body).expect("body");
        let mut out = [0u8; 16];
        assert_eq!(
            sys.forward(&body[..n], &mut out).await,
            Err(HandlerError::Unavailable)
        );
        assert_eq!(
            sys.forward(b"", &mut out).await,
            Err(HandlerError::InvalidRequest)
        );
    }

    #[test]
    fn rg21_the_wait_follows_the_key_and_the_callers_deadline() {
        let (t, u) = (Duration::from_secs(2), Duration::from_secs(20));
        assert_eq!(sys_wait(keys::INFO, t, u, None), t);
        assert_eq!(sys_wait(keys::BEGIN, t, u, None), u);
        assert_eq!(
            sys_wait(keys::BEGIN, t, u, Some(5_000)),
            Duration::from_secs(5)
        );
        assert_eq!(sys_wait(keys::INFO, t, u, Some(60_000)), t);
        assert_eq!(sys_wait(keys::COMMIT, t, u, Some(0)), Duration::ZERO);
    }
}
