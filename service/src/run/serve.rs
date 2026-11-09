//! The serving shell: what [`Service::run_with`](crate::Service::run_with)
//! does between start and exit.
//!
//! # Start
//!
//! 1. Telemetry ([`Telemetry::for_config`]), the Zenoh session if the spec
//!    uses one ([`super::zenoh::open`]) and discovery of every called kind
//!    ([`super::calls::start`]). Any failure ends the run before it binds.
//! 2. Bind h2c on `0.0.0.0:<Config::port_for>` (or take
//!    [`RunOptions::listener`](super::RunOptions::listener)) and spawn the
//!    `GrpcServer` with the health service (node id in `mw-node`) and the
//!    responder (`PrincipalSource::SidecarXfcc`, or none
//!    [`without_sidecar`](super::RunOptions::without_sidecar)). Health
//!    starts `NOT_SERVING` for `""` and every served kind.
//! 3. Spawn one dispatcher and one serve task per served kind, and one
//!    task per subscription hook. Then [`Event::Ready`]: health `SERVING`.
//!
//! # Requests
//!
//! The dispatcher takes each call off the responder and decides
//! ([`route`], pure): a caller whose principal is not in `allow_callers`
//! is answered `PERMISSION_DENIED` (also when no sidecar identified it); a
//! key no handler serves `UNIMPLEMENTED`; anything else goes to its kind's
//! queue ([`KIND_QUEUE`] calls; full is `UNAVAILABLE`, retriable).
//!
//! A serve task owns its kind's handler, its own
//! [`Links`] and a [`RESPONSE_CAPACITY`] response buffer,
//! and handles one call at a time: newest directory snapshots into the
//! links, a server span ([`Spans::start`]), a fresh
//! [`Ctx`] (the call's metadata, the span's trace context,
//! the caller's deadline), the handler bounded by that deadline
//! (`DEADLINE_EXCEEDED` when it passes, the handler's future is dropped),
//! the reply, one log line.
//!
//! **Concurrency:** kinds run concurrently, each kind serially.
//! [`ServiceHandler::handle`](crate::ServiceHandler::handle) takes
//! `&mut self`, so one handler value can only be in one call at a time,
//! and a handler's state is its own without locks; a kind's throughput is
//! one call per handler latency, with [`KIND_QUEUE`] waiting. A service
//! needing more parallelism scales out with `replicas` (each pod is
//! another endpoint of the kind). The `Ctx` borrows the serve task's own
//! `Links`, so handlers of different kinds never share a client.
//!
//! # Shutdown
//!
//! [`Lifecycle`] decides; this shell feeds it events and performs its
//! actions:
//!
//! * the shutdown future (SIGTERM/SIGINT for [`Service::run`]) is
//!   [`Event::Terminate`]: health `NOT_SERVING` for `""` and every kind,
//!   requests still answered;
//! * a [`Event::Tick`] at the end of the drain grace stops: the server
//!   stops accepting (HTTP/2 GOAWAY), the health announcer is dropped so
//!   open `Watch` streams end, and in-flight calls finish; all of it, and
//!   the serve tasks draining their queues, bounded by the stop timeout
//!   ([`DEFAULT_STOP_TIMEOUT`](super::options::DEFAULT_STOP_TIMEOUT)),
//!   after which what is left is aborted;
//! * a second termination (a second signal for `Service::run`) stops at
//!   once; the server ending on its own is [`Event::Failed`];
//! * then discovery and hook tasks are aborted, the Zenoh session closed
//!   and the spans flushed ([`FLUSH_TIMEOUT`]).
//!
//! [`Service::run`]: crate::Service::run

use std::future::Future;
use std::pin::pin;
use std::time::Duration;

use middleware::grpc::health::health;
use middleware::grpc::server::serve as serve_grpc;
use middleware::grpc::{
    GrpcAnnouncer, GrpcServer, PendingCall, PrincipalSource, ServingStatus, responder,
};
use middleware::key::Key;
use middleware::transport::{HandlerError, Request};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::calls::{self, Called, Snapshot};
use super::node::node_id;
use super::options::{FLUSH_TIMEOUT, RunOptions};
use super::telemetry::{Spans, Telemetry};
use crate::builder::{Served, Service};
use crate::config::Config;
use crate::ctx::{BoxedHandler, Ctx, Links};
use crate::error::ServiceError;
use crate::lifecycle::{Action, Event, Lifecycle, Monotonic, Outcome};

/// Calls the responder queues before the dispatcher takes them.
pub const RESPONDER_QUEUE: usize = 64;

/// Calls waiting for one kind's handler; more are `UNAVAILABLE`.
pub const KIND_QUEUE: usize = 32;

/// The largest response a handler can write (1 MiB).
pub const RESPONSE_CAPACITY: usize = 1 << 20;

/// Where the dispatcher sends a call. Pure decision of [`route`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// To the serve task of `serves[i]`.
    Kind(usize),
    /// The caller is not admitted: `PERMISSION_DENIED`.
    Denied,
    /// No handler serves the key: `UNIMPLEMENTED`.
    Unknown,
}

/// Routes a call on `key` from `principal`: with `allow` (the in-process
/// `allow_callers` check on) only listed principals are admitted, an
/// unidentified caller never; admitted calls go to the kind serving `key`.
/// The check comes first, so an unadmitted caller learns nothing about
/// which kinds exist. Pure.
pub fn route(
    serves: &[String],
    allow: Option<&[String]>,
    key: &str,
    principal: Option<&str>,
) -> Route {
    if let Some(allow) = allow
        && !principal.is_some_and(|p| allow.iter().any(|a| a == p))
    {
        return Route::Denied;
    }
    serves
        .iter()
        .position(|k| k == key)
        .map_or(Route::Unknown, Route::Kind)
}

/// Runs `service` (module docs). `again` is a second termination, polled
/// only while draining.
pub async fn run<S, A>(
    service: Service,
    config: Config,
    mut options: RunOptions,
    shutdown: S,
    again: A,
) -> Result<(), ServiceError>
where
    S: Future<Output = ()> + Send,
    A: Future<Output = ()> + Send,
{
    let (spec, served, hooks) = service.into_all_parts();
    let node = node_id(&config);
    let drain_grace = config.drain_grace_for(&spec);
    let stop_timeout = options.stop_timeout_or_default();

    let telemetry = Telemetry::for_config(&spec.name, &config)?;
    let zenoh = super::zenoh::open(&spec, &config).await?;
    let called = calls::start(&spec.calls, &config, &options).await?;
    let listener = match options.listener.take() {
        Some(l) => l,
        None => TcpListener::bind(("0.0.0.0", config.port_for(&spec))).await?,
    };
    let local = listener.local_addr()?;

    // Health: NOT_SERVING until every loop is up.
    let (health_service, mut announcer) = health();
    set_health(&mut announcer, &spec.serves, ServingStatus::NotServing);
    let principals = if options.without_sidecar {
        PrincipalSource::None
    } else {
        PrincipalSource::SidecarXfcc
    };
    let (responder_service, mut grpc_responder) = responder(RESPONDER_QUEUE);
    let server = GrpcServer::new()
        .with_health(health_service.with_node(node))
        .with_responder(responder_service.with_principal_source(principals));
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let mut server_task = tokio::spawn(serve_grpc(server, listener, async {
        let _ = stop_rx.await;
    }));

    // One serve task per kind, then the dispatcher feeding them.
    let spans = telemetry.spans();
    let dialer = options.dialer_or_default();
    let mut queues = Vec::with_capacity(served.len());
    let mut serve_tasks = Vec::with_capacity(served.len());
    for Served { kind, handler } in served {
        let (tx, rx) = mpsc::channel(KIND_QUEUE);
        let links = calls::links_for_task(
            &called,
            node,
            &dialer,
            zenoh.as_ref(),
            spec.publish_filters(),
        );
        let directories = called
            .iter()
            .map(|c| (c.kind.clone(), c.directory.clone()))
            .collect();
        serve_tasks.push(tokio::spawn(serve_kind(
            handler,
            kind,
            rx,
            links,
            directories,
            spans.clone(),
        )));
        queues.push(tx);
    }
    let allow = (!options.without_sidecar).then(|| spec.allow_callers.clone());
    let serves = spec.serves.clone();
    let dispatcher = tokio::spawn(async move {
        // Each iteration routes one call; it ends when the server is gone.
        while let Ok(call) = grpc_responder.next_call().await {
            dispatch(call, &serves, allow.as_deref(), &queues);
        }
    });
    let hook_tasks: Vec<JoinHandle<()>> = match &zenoh {
        Some(z) => hooks
            .into_iter()
            .map(|hook| {
                let z = z.clone();
                tokio::spawn(async move {
                    let filter = hook.filter.clone();
                    if let Err(e) = super::zenoh::run_hook(z, hook).await {
                        tracing::warn!(filter, error = %e, "subscription ended");
                    }
                })
            })
            .collect(),
        None => Vec::new(),
    };

    // The lifecycle.
    let start = Instant::now();
    let clock = move || Monotonic::from_origin(start.elapsed());
    let mut shell = Shell {
        announcer: Some(announcer),
        serves: &spec.serves,
        stop: Some(stop_tx),
    };
    let (mut state, actions) = Lifecycle::default().on(Event::Ready, clock());
    let mut outcome = shell.perform(&actions);
    tracing::info!(
        service = spec.name.as_str(),
        address = %local,
        node = middleware::grpc::health::format_node(node),
        serves = ?spec.serves,
        calls = ?spec.calls,
        zenoh = zenoh.is_some(),
        "serving"
    );
    let mut shutdown = pin!(shutdown);
    let mut again = pin!(again);
    let mut terminated = false;
    let mut server_ended = false;
    let mut failure = None;
    // Each iteration feeds one event; the lifecycle reaches Exit after at
    // most a termination and a tick (or a second termination or a
    // failure).
    while outcome.is_none() {
        let deadline = state.next_deadline().map(|m| start + m.since_origin());
        let event = tokio::select! {
            () = &mut shutdown, if !terminated => {
                terminated = true;
                tracing::info!(drain_grace_ms = drain_grace.as_millis() as u64, "terminating");
                Event::Terminate { drain_grace }
            }
            () = &mut again, if terminated => Event::Terminate { drain_grace },
            () = sleep_until(deadline), if deadline.is_some() => Event::Tick,
            ended = &mut server_task, if !server_ended => {
                server_ended = true;
                let why = match ended {
                    Ok(Ok(())) => "server stopped on its own".to_owned(),
                    Ok(Err(e)) => format!("server failed: {e}"),
                    Err(e) => format!("server task: {e}"),
                };
                tracing::error!(reason = why.as_str(), "failed");
                failure = Some(why);
                Event::Failed
            }
        };
        let (next, actions) = state.on(event, clock());
        state = next;
        outcome = shell.perform(&actions);
    }

    // Stop: in-flight calls finish within the stop timeout.
    let stop_by = Instant::now() + stop_timeout;
    if !server_ended {
        match tokio::time::timeout_at(stop_by, &mut server_task).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(e))) => tracing::warn!(error = %e, "server stopped with an error"),
            Ok(Err(e)) => tracing::warn!(error = %e, "server task"),
            Err(_) => {
                tracing::warn!("in-flight requests outlived the stop timeout");
                server_task.abort();
            }
        }
    }
    for mut task in serve_tasks.into_iter().chain([dispatcher]) {
        if tokio::time::timeout_at(stop_by, &mut task).await.is_err() {
            task.abort();
        }
    }
    for Called { task, .. } in called {
        task.abort();
    }
    for task in hook_tasks {
        task.abort();
    }
    if let Some(z) = zenoh {
        let _ = tokio::time::timeout(Duration::from_secs(1), z.session().close()).await;
    }
    telemetry.shutdown(FLUSH_TIMEOUT).await;
    // The loop above only ends with an outcome.
    let outcome = outcome.unwrap_or(Outcome::Clean);
    tracing::info!(outcome = ?outcome, "stopped");
    match outcome {
        Outcome::Failed => Err(ServiceError::Failed(
            failure.unwrap_or_else(|| "failed".to_owned()),
        )),
        Outcome::Clean => Ok(()),
    }
}

/// Sleeps until `deadline`; never completes for `None`.
async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

/// Sets `""` and every served kind to `status`.
fn set_health(announcer: &mut GrpcAnnouncer, serves: &[String], status: ServingStatus) {
    for name in std::iter::once("").chain(serves.iter().map(String::as_str)) {
        if let Err(e) = announcer.set_status(name, status) {
            tracing::warn!(name, error = %e, "health status not set");
        }
    }
}

/// What performs the lifecycle's actions.
struct Shell<'s> {
    announcer: Option<GrpcAnnouncer>,
    serves: &'s [String],
    stop: Option<oneshot::Sender<()>>,
}

impl Shell<'_> {
    /// Performs `actions` in order; the outcome once one is `Exit`.
    fn perform(&mut self, actions: &[Action]) -> Option<Outcome> {
        for action in actions {
            match *action {
                Action::SetHealth(status) => {
                    if let Some(a) = self.announcer.as_mut() {
                        set_health(a, self.serves, status);
                    }
                    tracing::info!(status = ?status, "health");
                }
                Action::StopAccepting => {
                    if let Some(stop) = self.stop.take() {
                        let _ = stop.send(());
                    }
                    // Ends every open Watch stream, so graceful shutdown
                    // does not wait on them.
                    self.announcer = None;
                    tracing::info!("stopped accepting");
                }
                Action::Exit(outcome) => return Some(outcome),
            }
        }
        None
    }
}

/// Answers or queues one call ([`route`]).
fn dispatch(
    call: PendingCall,
    serves: &[String],
    allow: Option<&[String]>,
    queues: &[mpsc::Sender<PendingCall>],
) {
    let principal = call.meta(Instant::now()).principal.map(str::to_owned);
    match route(serves, allow, call.key(), principal.as_deref()) {
        Route::Kind(i) => match queues.get(i) {
            Some(q) => {
                if let Err(e) = q.try_send(call) {
                    let call = match e {
                        mpsc::error::TrySendError::Full(c)
                        | mpsc::error::TrySendError::Closed(c) => c,
                    };
                    tracing::warn!(kind = call.key(), "queue full");
                    call.reply(Err(HandlerError::Unavailable));
                }
            }
            None => call.reply(Err(HandlerError::Internal)),
        },
        Route::Denied => {
            tracing::warn!(
                kind = call.key(),
                principal = principal.as_deref().unwrap_or("-"),
                "caller not admitted"
            );
            call.reply(Err(HandlerError::Denied));
        }
        Route::Unknown => call.reply(Err(HandlerError::NotFound)),
    }
}

/// The serve loop of one kind (module docs). Ends when its queue closes
/// and is empty.
async fn serve_kind(
    mut handler: BoxedHandler,
    kind: String,
    mut queue: mpsc::Receiver<PendingCall>,
    mut links: Links,
    mut directories: Vec<(String, watch::Receiver<Snapshot>)>,
    spans: Spans,
) {
    let mut out = vec![0u8; RESPONSE_CAPACITY];
    // Each iteration answers one call.
    while let Some(call) = queue.recv().await {
        if call.is_abandoned() {
            continue;
        }
        links = calls::refresh(links, &mut directories);
        let started = Instant::now();
        let (result, trace) =
            handle_one(&mut handler, &kind, &call, &mut links, &spans, &mut out).await;
        let reply = result.and_then(|n| out.get(..n).ok_or(HandlerError::BufferTooSmall));
        let meta = call.meta(started);
        tracing::info!(
            target: "mw_service::request",
            kind = kind.as_str(),
            outcome = match &reply { Ok(_) => "ok".to_owned(), Err(e) => format!("{e:?}") },
            duration_ms = started.elapsed().as_millis() as u64,
            trace_id = trace.map(|t| hex(&t.trace_id())),
            span_id = trace.map(|t| hex(&t.span_id())),
            request_id = meta.request_id.map(|r| r.as_str()),
            principal = meta.principal,
            "request"
        );
        call.reply(reply);
    }
}

/// Lowercase hex of `bytes`.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Runs the handler on `call` within its deadline, in a server span.
async fn handle_one(
    handler: &mut BoxedHandler,
    kind: &str,
    call: &PendingCall,
    links: &mut Links,
    spans: &Spans,
    out: &mut [u8],
) -> (Result<usize, HandlerError>, Option<middleware::TraceParent>) {
    let now = Instant::now();
    let meta = call.meta(now);
    let span = spans.start(kind, &meta);
    let trace = span.trace();
    let result = match (Key::new(call.key()), call.deadline()) {
        (Err(_), _) => Err(HandlerError::InvalidRequest),
        (Ok(_), Some(d)) if d <= now => Err(HandlerError::DeadlineExceeded),
        (Ok(key), deadline) => {
            let req = Request::new(key, call.body()).with_meta(meta);
            let ctx = Ctx::new(meta, trace, links);
            let mut ctx = match deadline {
                Some(d) => ctx.with_deadline(d.into_std()),
                None => ctx,
            };
            let handled = handler.call(&mut ctx, &req, out);
            match deadline {
                Some(d) => tokio::time::timeout_at(d, handled)
                    .await
                    .unwrap_or(Err(HandlerError::DeadlineExceeded)),
                None => handled.await,
            }
        }
    };
    span.end(result.map(|_| ()));
    (result, trace)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m6_route_checks_callers_then_kinds() {
        let serves = vec!["demo/echo".to_owned(), "demo/ping".to_owned()];
        let caller = "spiffe://infra.local/ns/apps/sa/demo-caller";
        let allow = vec![caller.to_owned()];
        let allow = Some(allow.as_slice());
        assert_eq!(
            route(&serves, allow, "demo/ping", Some(caller)),
            Route::Kind(1)
        );
        assert_eq!(
            route(&serves, allow, "demo/nope", Some(caller)),
            Route::Unknown
        );
        assert_eq!(
            route(
                &serves,
                allow,
                "demo/echo",
                Some("spiffe://infra.local/ns/apps/sa/other")
            ),
            Route::Denied
        );
        assert_eq!(route(&serves, allow, "demo/echo", None), Route::Denied);
        assert_eq!(
            route(&serves, allow, "demo/nope", None),
            Route::Denied,
            "no probing"
        );
        assert_eq!(
            route(&serves, Some(&[]), "demo/echo", Some(caller)),
            Route::Denied
        );
        // Check off (no sidecar).
        assert_eq!(route(&serves, None, "demo/echo", None), Route::Kind(0));
        assert_eq!(route(&serves, None, "x/y", None), Route::Unknown);
    }

    #[test]
    fn m6_shell_performs_health_stop_and_exit_in_order() {
        let (svc, announcer) = health();
        let serves = vec!["demo/echo".to_owned()];
        let (stop, mut stopped) = oneshot::channel();
        let mut shell = Shell {
            announcer: Some(announcer),
            serves: &serves,
            stop: Some(stop),
        };
        assert_eq!(
            shell.perform(&[Action::SetHealth(ServingStatus::Serving)]),
            None
        );
        let a = shell.announcer.as_ref().unwrap();
        assert_eq!(a.status(""), Some(ServingStatus::Serving));
        assert_eq!(a.status("demo/echo"), Some(ServingStatus::Serving));
        assert!(stopped.try_recv().is_err());
        assert_eq!(
            shell.perform(&[
                Action::SetHealth(ServingStatus::NotServing),
                Action::StopAccepting,
                Action::Exit(Outcome::Clean),
            ]),
            Some(Outcome::Clean)
        );
        assert!(stopped.try_recv().is_ok(), "server told to stop");
        assert!(shell.announcer.is_none(), "watch streams end");
        drop(svc);
    }
}
