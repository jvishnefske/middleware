//! The node's own `sys/...` keys over Zenoh (PN21, PN22, PN25), enabled by
//! `MW_POSIX_NODE_ZENOH_SYS` (on by default).
//!
//! ```text
//!   query @mw/node/<sender>/sys/<k> ── callback ── queue (32) ── worker: plan()
//!        not this node: ignored;  not in SERVED: Denied
//!        SERVED: wrap(requester) ══ mesh link ══ device thread: Controller::mesh_request
//!   reply / reply_err  ◄── response frame (or Unavailable when the deadline passes)
//! ```
//!
//! * **One queryable**, `@mw/node/<sender id>/sys/**`, where the sender id
//!   is the Zenoh session's ([`ZenohTransport::sender_id`], the id
//!   `Target::Node` requests and `mwctl scout` use). No bare `sys/...` key
//!   is declared (a router's access control drops bare declarations), so
//!   a query names this node or does not reach it. A wildcard query
//!   ([`plan`]: `mwctl scout`'s `@mw/node/*/sys/info`) gets `sys/info` and
//!   nothing else. Answers are on this node's concrete key ([`reply_key`]). `sys/info` carries the
//!   node id ([`crate::node_id`]), which is not the sender id: that maps
//!   one to the other.
//! * **Served keys**, [`SERVED`]: the description keys, the payload
//!   binding, the loader and the payload's start, stop and bind. Every
//!   other key (`sys/trust/claim`, `sys/board`, `sys/peers`, `sys/fw/*`,
//!   `sys/log`, `sys/payload/status`, `sys/link/*`, ...) is answered
//!   `Denied` here, without reaching the device. What a served key may do
//!   is then the device's [`crate::mesh_gate`] decision.
//! * **Bounded bodies.** A body that cannot fit one frame to the device
//!   ([`body_fits`]) is answered `InvalidRequest` before it is copied; the
//!   description keys ignore any body ([`takes_body`]). The session itself
//!   refuses messages much larger than a frame (`crate::session_config`).
//! * **One at a time.** The queryable's callback queues queries
//!   ([`QUERY_QUEUE`]; full: the query is dropped, counted, and times out
//!   at the querier) and one worker forwards them over the mesh link, the
//!   requester (the sender id in the query attachment, not authenticated:
//!   [`crate::mesh_gate`]) in an envelope ([`crate::mesh_gate::wrap`]).
//!   Each forward is bounded by the query's deadline less the time it
//!   queued, at most [`Options::request_cap`]; a query whose deadline has
//!   passed is dropped unanswered; a forward that runs out of time is
//!   answered `Unavailable` (the device's late answer is discarded by the
//!   next forward).
//! * The worker ends, undeclaring the queryable, when `stop` completes;
//!   the mesh link closing ends it with [`SysError::Device`].

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use middleware::Key;
use middleware::loader::keys;
use middleware::serial::link::{LinkError, SerialLink};
use middleware::transport::{HandlerError, RequestOptions, Requester, SenderId};
use middleware::zenoh::responder::request_meta;
use middleware::zenoh::{ZenohTransport, wire};
use mw_gateway::io::ChannelIo;
use tokio::sync::mpsc;
use zenoh::query::Query;

/// The keys forwarded to the device (module docs).
pub const SERVED: [Key<'static>; 11] = [
    keys::INFO,
    keys::TRUST_INFO,
    keys::MANIFEST,
    keys::CONTROL_MODULES,
    keys::BINDING,
    keys::BEGIN,
    keys::CHUNK,
    keys::COMMIT,
    keys::START,
    keys::STOP,
    keys::BIND,
];

/// Queries waiting for the worker.
pub const QUERY_QUEUE: usize = 32;

/// Longest one forward may take: mwctl's query timeout is 8 s, and a start
/// compiles the payload inside the device thread.
pub const REQUEST_CAP: Duration = Duration::from_secs(10);

/// The worker's settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// The bound on one forward ([`REQUEST_CAP`]).
    pub request_cap: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Options { request_cap: REQUEST_CAP }
    }
}

/// What to do with a query on `expr`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// Not a per-node key of this node: no answer.
    Ignore,
    /// Answer this error without asking the device.
    Refuse(HandlerError),
    /// Forward to the device.
    Forward(Key<'static>),
}

/// Decides a query on `expr` for the node whose session id is `own`. Pure.
///
/// A query naming one of this node's keys is planned by that key. A
/// wildcard query (`mwctl scout`'s `@mw/node/*/sys/info`, a sweep of
/// `@mw/node/**`) is forwarded as `sys/info` when it covers this node's
/// `sys/info`, and ignored otherwise: a wildcard never reaches a key that
/// stages or acts.
pub fn plan(expr: &str, own: SenderId) -> Plan {
    if expr.contains('*') || expr.contains('$') {
        let covers = zenoh::key_expr::KeyExpr::try_from(expr)
            .ok()
            .zip(zenoh::key_expr::KeyExpr::try_from(reply_key(own, keys::INFO)).ok())
            .is_some_and(|(query, info)| query.includes(&info));
        return if covers { Plan::Forward(keys::INFO) } else { Plan::Ignore };
    }
    let Some((id, key)) = wire::parse_node_key(expr) else { return Plan::Ignore };
    if id != own {
        return Plan::Ignore;
    }
    match SERVED.iter().find(|k| **k == key) {
        Some(k) => Plan::Forward(*k),
        None => Plan::Refuse(HandlerError::Denied),
    }
}

/// Whether a request on `key` takes a body: not the description keys
/// `sys/info`, `sys/trust/info`, `sys/manifest` nor `sys/payload/binding`
/// (a body sent with one is ignored, never copied). Pure.
pub fn takes_body(key: Key<'_>) -> bool {
    ![keys::INFO, keys::TRUST_INFO, keys::MANIFEST, keys::BINDING].contains(&key)
}

/// Whether a `len`-byte body for `key` fits one frame to the device with
/// the requester's envelope ([`crate::mesh_gate::ENVELOPE_MAX`]); checked
/// on the query's length before its body is copied. Pure.
pub fn body_fits(key: Key<'_>, len: usize) -> bool {
    len.checked_add(crate::mesh_gate::ENVELOPE_MAX).is_some_and(|n| crate::device::fits_frame(key, n))
}

/// The key a forwarded query is answered on: this node's concrete key.
pub fn reply_key(own: SenderId, key: Key<'_>) -> String {
    wire::node_key(own, key)
}

/// How long a forward may take: the query's remaining deadline (`None`:
/// none was sent), at most `cap`; `None` when the deadline has passed (the
/// query is dropped unanswered). Pure.
pub fn budget(deadline_ms: Option<u32>, cap: Duration) -> Option<Duration> {
    match deadline_ms {
        None => Some(cap),
        Some(0) => None,
        Some(ms) => Some(cap.min(Duration::from_millis(u64::from(ms)))),
    }
}

/// What the worker did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Report {
    /// Queries forwarded and answered by the device (an error included).
    pub served: u32,
    /// Queries answered `Denied` without the device.
    pub refused: u32,
    /// Forwards that ran out of time (answered `Unavailable`).
    pub timeouts: u32,
    /// Queries dropped: the queue was full, or their deadline had passed.
    pub dropped: u32,
}

/// Why the worker ended early.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SysError {
    /// The queryable could not be declared.
    Zenoh(String),
    /// The mesh link closed (the device thread ended).
    Device,
}

impl std::fmt::Display for SysError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SysError::Zenoh(e) => write!(f, "zenoh: {e}"),
            SysError::Device => f.write_str("the mesh link to the device closed"),
        }
    }
}

impl std::error::Error for SysError {}

type Link = SerialLink<ChannelIo, 0>;

/// Why one forward failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failed {
    /// Answer this error.
    Answer(HandlerError),
    /// Out of time: answer `Unavailable`.
    Timeout,
    /// The link closed.
    Closed,
}

/// Forwards one request for `sender` over `link`, bounded by `within`.
async fn forward(
    link: &mut Link,
    key: Key<'_>,
    sender: Option<SenderId>,
    body: &[u8],
    within: Duration,
) -> Result<Vec<u8>, Failed> {
    let wrapped = crate::mesh_gate::wrap(sender, body);
    if !crate::device::fits_frame(key, wrapped.len()) {
        return Err(Failed::Answer(HandlerError::InvalidRequest));
    }
    let mut out = vec![0u8; middleware::serial::frame::MAX_RAW_FRAME];
    let options = RequestOptions::default();
    let asked = link.request_with(key, &wrapped, &mut out, &options);
    match tokio::time::timeout(within, asked).await {
        Err(_) => Err(Failed::Timeout),
        Ok(Ok(r)) => Ok(r.to_vec()),
        Ok(Err(LinkError::Handler(e))) => Err(Failed::Answer(e)),
        Ok(Err(LinkError::Closed | LinkError::Io(_))) => Err(Failed::Closed),
        Ok(Err(_)) => Err(Failed::Answer(HandlerError::Internal)),
    }
}

/// Queues `item`; a full (or closed) queue drops it and counts.
fn enqueue<T>(tx: &mpsc::Sender<T>, item: T, dropped: &AtomicU32) {
    if tx.try_send(item).is_err() {
        dropped.fetch_add(1, Ordering::Relaxed);
    }
}

/// The queryable's expression for the session `own`:
/// `@mw/node/<own>/sys/**`.
pub fn expression(own: SenderId) -> String {
    format!("{}/**", wire::node_key(own, Key::from_static("sys")))
}

/// Serves the node's `sys/...` keys on `session` over the mesh link `io`
/// until `stop` completes (module docs).
pub async fn run(
    io: ChannelIo,
    session: ZenohTransport,
    opts: Options,
    stop: impl Future<Output = ()>,
) -> Result<Report, SysError> {
    let own = session.sender_id();
    let dropped = Arc::new(AtomicU32::new(0));
    let (tx, mut rx) = mpsc::channel::<(Instant, Query)>(QUERY_QUEUE);
    let counted = dropped.clone();
    let queryable = session
        .session()
        .declare_queryable(expression(own))
        .callback(move |q| enqueue(&tx, (Instant::now(), q), &counted))
        .await
        .map_err(|e| SysError::Zenoh(e.to_string()))?;
    let mut link: Link = SerialLink::with_inbox(io);
    let mut report = Report::default();
    tokio::pin!(stop);
    let result = loop {
        let (arrived, query) = tokio::select! {
            () = &mut stop => break Ok(()),
            q = rx.recv() => match q {
                Some(q) => q,
                None => break Ok(()),
            },
        };
        let expr = query.key_expr().as_str().to_owned();
        let key = match plan(&expr, own) {
            Plan::Ignore => continue,
            Plan::Refuse(e) => {
                report.refused += 1;
                let _ = query.reply_err(wire::encode_reply_error(e, own).to_vec()).await;
                continue;
            }
            Plan::Forward(key) => key,
        };
        // Sized before anything is copied: Zenoh would deliver up to its
        // message limit, and 32 queries can wait.
        let body = match (takes_body(key), query.payload()) {
            (false, _) | (true, None) => Vec::new(),
            (true, Some(p)) if !body_fits(key, p.len()) => {
                report.refused += 1;
                let error = wire::encode_reply_error(HandlerError::InvalidRequest, own);
                let _ = query.reply_err(error.to_vec()).await;
                continue;
            }
            (true, Some(p)) => p.to_bytes().into_owned(),
        };
        let attachment = query.attachment().map(|a| a.to_bytes().into_owned());
        let waited = u64::try_from(arrived.elapsed().as_millis()).unwrap_or(u64::MAX);
        let meta = request_meta(attachment.as_deref(), waited);
        let Some(within) = budget(meta.deadline_ms, opts.request_cap) else {
            dropped.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        let answer = match forward(&mut link, key, meta.sender, &body, within).await {
            Ok(r) => Ok(r),
            Err(Failed::Answer(e)) => Err(e),
            Err(Failed::Timeout) => {
                report.timeouts += 1;
                Err(HandlerError::Unavailable)
            }
            Err(Failed::Closed) => {
                let _ = query.reply_err(wire::encode_reply_error(HandlerError::Unavailable, own).to_vec()).await;
                break Err(SysError::Device);
            }
        };
        report.served += 1;
        // A failed reply only means this querier misses the answer.
        let _ = match answer {
            Ok(r) => query.reply(reply_key(own, key), r).attachment(wire::encode_reply(own).to_vec()).await,
            Err(e) => query.reply_err(wire::encode_reply_error(e, own).to_vec()).await,
        };
    };
    let _ = queryable.undeclare().await;
    report.dropped = dropped.load(Ordering::Relaxed);
    result.map(|()| report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh_gate;
    use middleware::serial::frame::Frame;
    use middleware::serial::link::RECV_BUF_LEN;
    use mw_gateway::io::pair;

    const OWN: SenderId = SenderId([0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80]);
    const OTHER: SenderId = SenderId([0x11, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80]);

    fn node_key(id: SenderId, key: &str) -> String {
        wire::node_key(id, Key::new(key).expect("key"))
    }

    #[test]
    fn pn21_one_prefix_queryable_on_the_session_id() {
        assert_eq!(expression(OWN), "@mw/node/1020304050607080/sys/**");
    }

    #[test]
    fn pn22_served_keys_are_forwarded_and_the_rest_denied() {
        for key in SERVED {
            assert_eq!(plan(&node_key(OWN, key.as_str()), OWN), Plan::Forward(key), "{key:?}");
        }
        for key in [
            "sys/trust/claim",
            "sys/board",
            "sys/peers",
            "sys/fw/info",
            "sys/fw/begin",
            "sys/log",
            "sys/payload/status",
            "sys/link/sub",
            "sys/zenoh/endpoint",
            "sys/net/addr",
            "sys",
            "sys/info/x",
        ] {
            assert_eq!(plan(&node_key(OWN, key), OWN), Plan::Refuse(HandlerError::Denied), "{key}");
        }
    }

    #[test]
    fn pn21_queries_for_another_node_or_no_node_are_ignored() {
        assert_eq!(plan(&node_key(OTHER, "sys/info"), OWN), Plan::Ignore);
        for expr in
            ["sys/info", "@mw/node/zz/sys/info", "@mw/node/1020304050607080", "@mw/svc/x/sys/info", "@mw/node/*/app/x"]
        {
            assert_eq!(plan(expr, OWN), Plan::Ignore, "{expr}");
        }
    }

    #[test]
    fn pn21_a_wildcard_query_gets_sys_info_only() {
        // `mwctl scout` asks `@mw/node/*/sys/info`; a sweep of every node
        // key finds the node by its sys/info.
        for expr in ["@mw/node/*/sys/info", "@mw/node/**", "@mw/node/1020304050607080/sys/*", "@mw/**/info"] {
            assert_eq!(plan(expr, OWN), Plan::Forward(keys::INFO), "{expr}");
        }
        // Never a wildcard to anything else: nothing that acts or stages.
        for expr in [
            "@mw/node/*/sys/loader/*",
            "@mw/node/*/sys/payload/start",
            "@mw/node/*/sys/manifest",
            "@mw/node/*/sys/trust/**",
        ] {
            assert_eq!(plan(expr, OWN), Plan::Ignore, "{expr}");
        }
        // The answer is always on this node's concrete key.
        assert_eq!(reply_key(OWN, keys::INFO), "@mw/node/1020304050607080/sys/info");
    }

    #[test]
    fn pn25_a_body_that_cannot_fit_one_frame_is_refused_before_it_is_copied() {
        // What a chunk carries fits; a byte more than a frame holds with the
        // envelope does not, nor anything near Zenoh's 1 GiB default.
        assert!(body_fits(keys::CHUNK, 512 + 8));
        let room = middleware::serial::frame::MAX_RAW_FRAME;
        assert!(!body_fits(keys::CHUNK, room));
        assert!(!body_fits(keys::COMMIT, 1 << 30));
        assert!(!body_fits(keys::BIND, usize::MAX));
        let edge = (0..room).rev().find(|n| body_fits(keys::BIND, *n)).expect("some body fits");
        assert!(crate::device::fits_frame(keys::BIND, edge + mesh_gate::ENVELOPE_MAX));
        // The keys that take no body ignore one, whatever its size.
        for key in [keys::INFO, keys::TRUST_INFO, keys::MANIFEST, keys::BINDING] {
            assert!(!takes_body(key), "{key:?}");
        }
        for key in [keys::CONTROL_MODULES, keys::BEGIN, keys::CHUNK, keys::COMMIT, keys::START, keys::STOP, keys::BIND]
        {
            assert!(takes_body(key), "{key:?}");
        }
    }

    #[test]
    fn pn25_the_query_deadline_bounds_a_forward_at_most_the_cap() {
        let cap = Duration::from_secs(10);
        assert_eq!(budget(None, cap), Some(cap));
        assert_eq!(budget(Some(8000), cap), Some(Duration::from_millis(8000)));
        assert_eq!(budget(Some(60_000), cap), Some(cap));
        assert_eq!(budget(Some(0), cap), None);
    }

    #[test]
    fn pn25_a_full_queue_drops_and_counts() {
        let (tx, mut rx) = mpsc::channel(1);
        let dropped = AtomicU32::new(0);
        enqueue(&tx, 1, &dropped);
        enqueue(&tx, 2, &dropped);
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
        assert_eq!(rx.try_recv(), Ok(1));
        assert!(rx.try_recv().is_err());
    }

    /// A device end that answers every request with its key, the body it
    /// was sent (the envelope included) after `delay`.
    async fn device(io: ChannelIo, delay: Duration) {
        let mut link: SerialLink<ChannelIo, 0> = SerialLink::with_inbox(io);
        let mut buf = vec![0u8; RECV_BUF_LEN];
        loop {
            let (id, body) = match link.next_frame(&mut buf).await {
                Ok(Frame::Request { id, payload, .. }) => (id, payload.to_vec()),
                Ok(_) => continue,
                Err(_) => return,
            };
            tokio::time::sleep(delay).await;
            let result = if body.ends_with(b"fail") { Err(HandlerError::NotFound) } else { Ok(&body[..]) };
            if link.send(&Frame::Response { id, result }).await.is_err() {
                return;
            }
        }
    }

    #[tokio::test]
    async fn pn25_a_forward_carries_the_requester_and_the_devices_answer() {
        let (ours, theirs) = pair();
        tokio::spawn(device(theirs, Duration::ZERO));
        let mut link: Link = SerialLink::with_inbox(ours);
        let r = forward(&mut link, keys::INFO, Some(OTHER), b"hi", Duration::from_secs(5)).await;
        assert_eq!(r, Ok(mesh_gate::wrap(Some(OTHER), b"hi")));
        let r = forward(&mut link, keys::INFO, None, b"fail", Duration::from_secs(5)).await;
        assert_eq!(r, Err(Failed::Answer(HandlerError::NotFound)));
        // A body that would not fit one frame never reaches the device.
        let big = vec![0u8; middleware::serial::frame::MAX_RAW_FRAME];
        let r = forward(&mut link, keys::CHUNK, Some(OTHER), &big, Duration::from_secs(5)).await;
        assert_eq!(r, Err(Failed::Answer(HandlerError::InvalidRequest)));
    }

    #[tokio::test]
    async fn pn25_a_slow_device_is_unavailable_and_its_late_answer_discarded() {
        let (ours, theirs) = pair();
        tokio::spawn(device(theirs, Duration::from_millis(300)));
        let mut link: Link = SerialLink::with_inbox(ours);
        let started = Instant::now();
        let r = forward(&mut link, keys::INFO, None, b"slow", Duration::from_millis(100)).await;
        assert_eq!(r, Err(Failed::Timeout));
        assert!(started.elapsed() < Duration::from_millis(250));
        // The next forward gets its own answer, not the late one.
        let r = forward(&mut link, keys::INFO, None, b"next", Duration::from_secs(5)).await;
        assert_eq!(r, Ok(mesh_gate::wrap(None, b"next")));
    }

    #[tokio::test]
    async fn pn25_a_closed_mesh_link_is_reported() {
        let (ours, theirs) = pair();
        drop(theirs);
        let mut link: Link = SerialLink::with_inbox(ours);
        let r = forward(&mut link, keys::INFO, None, b"", Duration::from_secs(1)).await;
        assert_eq!(r, Err(Failed::Closed));
    }
}
