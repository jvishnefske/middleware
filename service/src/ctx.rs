//! What a handler gets: the [`ServiceHandler`] trait, its per-request
//! [`Ctx`], and the downstream clients and publisher reached through it.
//!
//! # Why an async trait next to `Handler`
//!
//! `middleware`'s [`Handler`] is synchronous and pure on purpose: it runs
//! on microcontrollers and inside embassy-usb callbacks. A mesh service,
//! though, often has to call another service before it can answer, and a
//! downstream call is async I/O. Blocking inside a sync handler would stall
//! the serve loop and could deadlock a single-threaded runtime, so the
//! runner takes an async trait instead:
//!
//! ```text
//! fn handle(&mut self, ctx: &mut Ctx<'_>, req: &Request<'_>, out: &mut [u8])
//!     -> impl Future<Output = Result<usize, HandlerError>> + Send
//! ```
//!
//! which implementors write as `async fn handle(..)`. Every plain
//! [`Handler`] (closures included) is a `ServiceHandler` through a blanket
//! implementation that ignores the context and answers at once, so pure
//! handlers stay pure and unit-testable without a runtime. The future must
//! be `Send` so the runner can serve each kind on its own task.
//!
//! # Trace continuation
//!
//! The runner builds one [`Ctx`] per request with the caller's
//! [`RequestMeta`] and this handler's span as [`Ctx::trace`] (a child of
//! the caller's [`RequestMeta::trace`], or a new root, see
//! [`server_trace`]). Requests made through [`Ctx::client`] carry that
//! trace context, the caller's `x-request-id` and the time left of the
//! caller's deadline unless the call's own options set them, so a trace
//! spans every hop without handler code.
//!
//! # Ownership
//!
//! The runner owns the long-lived connections in a [`Links`] value (one
//! [`MeshClient`] per called kind, the Zenoh session and the declared
//! publish filters) and lends it to each [`Ctx`] for one request: no shared
//! ownership, no locks. Calls through a `Ctx` therefore serialize per serve
//! loop.

use middleware::Handler;
use middleware::discovery::ServiceKind;
use middleware::grpc::GrpcError;
use middleware::grpc::mesh::MeshClient;
use middleware::key::{Key, KeyFilterBuf, MAX_KEY_LEN};
use middleware::transport::{
    Flow, Gather, HandlerError, Limits, Publisher, Reply, Request, RequestId, RequestMeta,
    RequestOptions, Requester, TraceParent,
};
use middleware::zenoh::{ZenohError, ZenohTransport};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

/// Directory slots of each downstream [`MeshClient`]: the most endpoints
/// (pods) of one called kind a service tracks.
pub const DIRECTORY_SLOTS: usize = 32;

/// The client type behind [`Ctx::client`].
pub type DownstreamClient = MeshClient<DIRECTORY_SLOTS>;

/// An async request handler run by the service (module docs).
pub trait ServiceHandler: Send + 'static {
    /// Produces the response to `req` in `out` and returns its length. May
    /// call other services and publish through `ctx`.
    fn handle(
        &mut self,
        ctx: &mut Ctx<'_>,
        req: &Request<'_>,
        out: &mut [u8],
    ) -> impl Future<Output = Result<usize, HandlerError>> + Send;
}

/// Every plain [`Handler`] is a [`ServiceHandler`] that ignores the
/// context.
impl<H: Handler + Send + 'static> ServiceHandler for H {
    fn handle(
        &mut self,
        _ctx: &mut Ctx<'_>,
        req: &Request<'_>,
        out: &mut [u8],
    ) -> impl Future<Output = Result<usize, HandlerError>> + Send {
        std::future::ready(Handler::handle(self, req, out))
    }
}

/// The future of a [`BoxedHandler`] call.
pub type HandleFuture<'s> = Pin<Box<dyn Future<Output = Result<usize, HandlerError>> + Send + 's>>;

/// Object-safe form of [`ServiceHandler`], for storing handlers of
/// different types side by side.
trait DynHandler: Send {
    fn handle_dyn<'s>(
        &'s mut self,
        ctx: &'s mut Ctx<'_>,
        req: &'s Request<'_>,
        out: &'s mut [u8],
    ) -> HandleFuture<'s>;
}

impl<H: ServiceHandler> DynHandler for H {
    fn handle_dyn<'s>(
        &'s mut self,
        ctx: &'s mut Ctx<'_>,
        req: &'s Request<'_>,
        out: &'s mut [u8],
    ) -> HandleFuture<'s> {
        Box::pin(ServiceHandler::handle(self, ctx, req, out))
    }
}

/// A type-erased [`ServiceHandler`], as the builder stores them.
pub struct BoxedHandler(Box<dyn DynHandler>);

impl BoxedHandler {
    /// Erases `handler`'s type.
    pub fn new<H: ServiceHandler>(handler: H) -> Self {
        BoxedHandler(Box::new(handler))
    }

    /// Runs the handler on one request.
    pub fn call<'s>(
        &'s mut self,
        ctx: &'s mut Ctx<'_>,
        req: &'s Request<'_>,
        out: &'s mut [u8],
    ) -> HandleFuture<'s> {
        self.0.handle_dyn(ctx, req, out)
    }
}

impl fmt::Debug for BoxedHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BoxedHandler")
    }
}

/// The long-lived connections a runner lends to each [`Ctx`]: one
/// [`DownstreamClient`] per called kind and, when the service publishes,
/// the Zenoh session with the declared publish filters. Built by value.
#[derive(Debug, Default)]
pub struct Links {
    clients: Vec<(String, DownstreamClient)>,
    zenoh: Option<ZenohTransport>,
    publishes: Vec<KeyFilterBuf<MAX_KEY_LEN>>,
}

impl Links {
    /// No clients, no session.
    pub fn new() -> Self {
        Self::default()
    }

    /// The links with `client` serving calls to `kind` (replacing an
    /// earlier client for it).
    pub fn with_client(self, kind: ServiceKind<'_>, client: DownstreamClient) -> Self {
        let mut clients = self.clients;
        clients.retain(|(k, _)| k != kind.as_str());
        clients.push((kind.as_str().to_owned(), client));
        Links { clients, ..self }
    }

    /// The links publishing through `session` on keys matching `filters`
    /// (the spec's `publishes`); other keys are refused
    /// ([`PublishError::Undeclared`]).
    pub fn with_zenoh<'f>(
        self,
        session: ZenohTransport,
        filters: impl IntoIterator<Item = middleware::KeyFilter<'f>>,
    ) -> Self {
        let publishes = filters
            .into_iter()
            .filter_map(|f| f.to_buf().ok())
            .collect();
        Links {
            zenoh: Some(session),
            publishes,
            ..self
        }
    }

    /// The client for `kind`, if one was added.
    pub fn client_mut(&mut self, kind: &str) -> Option<&mut DownstreamClient> {
        self.clients
            .iter_mut()
            .find(|(k, _)| k == kind)
            .map(|(_, c)| c)
    }

    /// The kinds that have a client, in insertion order.
    pub fn kinds(&self) -> impl Iterator<Item = &str> {
        self.clients.iter().map(|(k, _)| k.as_str())
    }
}

/// The trace context of a server span `span_id` handling a request whose
/// caller sent `inbound`: a child of it, or (no or invalid inbound) a new
/// root trace `fresh_trace_id`, sampled. `None` only if `span_id` or
/// `fresh_trace_id` is all zero. Pure; the runner draws the random ids.
pub fn server_trace(
    inbound: Option<TraceParent>,
    span_id: [u8; 8],
    fresh_trace_id: [u8; 16],
) -> Option<TraceParent> {
    match inbound {
        Some(parent) => parent.child(span_id),
        None => TraceParent::new(
            fresh_trace_id,
            span_id,
            middleware::transport::trace::FLAG_SAMPLED,
        ),
    }
}

/// What a [`ServiceHandler`] sees of one request beyond its bytes (module
/// docs).
#[derive(Debug)]
pub struct Ctx<'a> {
    meta: RequestMeta<'a>,
    trace: Option<TraceParent>,
    deadline: Option<Instant>,
    links: &'a mut Links,
}

impl<'a> Ctx<'a> {
    /// The context of a request with `meta`, handled in the span `trace`
    /// (normally [`server_trace`] of `meta.trace`), lending `links`. No
    /// deadline for downstream calls unless set with
    /// [`Ctx::with_deadline`].
    pub fn new(meta: RequestMeta<'a>, trace: Option<TraceParent>, links: &'a mut Links) -> Self {
        Ctx {
            meta,
            trace,
            deadline: None,
            links,
        }
    }

    /// The same context bounding downstream calls by `at` (the runner
    /// passes the arrival time plus `meta.deadline_ms`).
    pub fn with_deadline(self, at: Instant) -> Self {
        Ctx {
            deadline: Some(at),
            ..self
        }
    }

    /// The caller's metadata, as the transport delivered it.
    pub fn meta(&self) -> &RequestMeta<'a> {
        &self.meta
    }

    /// This handler's span: what downstream calls send as their parent.
    pub fn trace(&self) -> Option<TraceParent> {
        self.trace
    }

    /// The caller's `x-request-id`.
    pub fn request_id(&self) -> Option<RequestId<'a>> {
        self.meta.request_id
    }

    /// When downstream calls must be done by, if the caller set a deadline.
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// A client for the declared dependency `kind` that continues this
    /// request's trace ([`Downstream`]); `None` if the service does not
    /// call `kind`.
    pub fn client(&mut self, kind: &str) -> Option<Downstream<'_>> {
        let defaults = Defaults {
            trace: self.trace,
            request_id: self.meta.request_id,
            deadline: self.deadline,
        };
        self.links
            .client_mut(kind)
            .map(|client| Downstream { client, defaults })
    }

    /// The service's publisher ([`ServicePublisher`]); `None` if it
    /// declared nothing to publish.
    pub fn publisher(&mut self) -> Option<ServicePublisher<'_>> {
        let links = &mut *self.links;
        links.zenoh.as_mut().map(|session| ServicePublisher {
            session,
            allowed: &links.publishes,
        })
    }
}

/// What a [`Downstream`] call uses when its own options leave it unset.
#[derive(Debug, Clone, Copy)]
struct Defaults<'a> {
    trace: Option<TraceParent>,
    request_id: Option<RequestId<'a>>,
    deadline: Option<Instant>,
}

impl Defaults<'_> {
    /// `options` with every unset field filled from the defaults: the
    /// handler's span, the caller's request id and the time left until the
    /// deadline at `now`.
    fn fill<'o>(&self, options: &RequestOptions<'o>, now: Instant) -> RequestOptions<'o>
    where
        Self: 'o,
    {
        RequestOptions {
            timeout: options
                .timeout
                .or_else(|| self.deadline.map(|d| d.saturating_duration_since(now))),
            target: options.target,
            trace: options.trace.or(self.trace),
            request_id: options.request_id.or(self.request_id),
        }
    }
}

/// A downstream client borrowed from a [`Ctx`]: [`Requester`] and
/// [`Gather`] over the kind's [`DownstreamClient`] that fill unset
/// `RequestOptions` fields from the request being handled (module docs).
#[derive(Debug)]
pub struct Downstream<'c> {
    client: &'c mut DownstreamClient,
    defaults: Defaults<'c>,
}

impl Downstream<'_> {
    /// The underlying mesh client (its directory, client id).
    pub fn mesh_client(&self) -> &DownstreamClient {
        self.client
    }

    /// The options a call with `options` actually uses now.
    pub fn effective_options<'o>(&self, options: &RequestOptions<'o>) -> RequestOptions<'o>
    where
        Self: 'o,
    {
        self.defaults.fill(options, Instant::now())
    }
}

impl Requester for Downstream<'_> {
    type Error = GrpcError;

    async fn request_with<'b>(
        &mut self,
        key: Key<'_>,
        request: &[u8],
        response: &'b mut [u8],
        options: &RequestOptions<'_>,
    ) -> Result<&'b [u8], GrpcError> {
        let options = self.defaults.fill(options, Instant::now());
        self.client
            .request_with(key, request, response, &options)
            .await
    }
}

impl Gather for Downstream<'_> {
    type Error = GrpcError;

    async fn request_each<F>(
        &mut self,
        key: Key<'_>,
        request: &[u8],
        buf: &mut [u8],
        options: &RequestOptions<'_>,
        max_replies: usize,
        on_reply: F,
    ) -> Result<usize, GrpcError>
    where
        F: FnMut(Reply<'_>) -> Flow,
    {
        let options = self.defaults.fill(options, Instant::now());
        self.client
            .request_each(key, request, buf, &options, max_replies, on_reply)
            .await
    }
}

/// The service's publisher borrowed from a [`Ctx`]: a [`Publisher`] over
/// its Zenoh session that refuses keys outside the declared `publishes`
/// filters, so a service cannot publish what its manifests (and the
/// router's ACL) do not grant.
#[derive(Debug)]
pub struct ServicePublisher<'c> {
    session: &'c mut ZenohTransport,
    allowed: &'c [KeyFilterBuf<MAX_KEY_LEN>],
}

impl ServicePublisher<'_> {
    /// Whether `key` matches a declared filter.
    pub fn allows(&self, key: Key<'_>) -> bool {
        self.allowed.iter().any(|f| f.as_filter().matches(key))
    }
}

/// Failure of a [`ServicePublisher`] publish.
#[derive(Debug)]
#[non_exhaustive]
pub enum PublishError {
    /// The key matches no declared `publishes` filter; nothing was sent.
    Undeclared,
    /// Zenoh failed.
    Zenoh(ZenohError),
}

impl fmt::Display for PublishError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PublishError::Undeclared => f.write_str("key not declared in publishes"),
            PublishError::Zenoh(e) => write!(f, "zenoh: {e}"),
        }
    }
}

impl std::error::Error for PublishError {}

impl Publisher for ServicePublisher<'_> {
    type Error = PublishError;

    const LIMITS: Limits = <ZenohTransport as Publisher>::LIMITS;

    async fn publish(&mut self, key: Key<'_>, payload: &[u8]) -> Result<(), PublishError> {
        if !self.allows(key) {
            return Err(PublishError::Undeclared);
        }
        self.session
            .publish(key, payload)
            .await
            .map_err(PublishError::Zenoh)
    }
}

/// How long until `at`, from `now`; zero once passed. Pure.
pub fn remaining(at: Instant, now: Instant) -> Duration {
    at.saturating_duration_since(now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::grpc::mesh::MeshDialer;
    use middleware::transport::handler_fn;

    const TP: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    const KIND: ServiceKind<'static> = ServiceKind::from_static("demo/echo");

    fn assert_send<T: Send>(_: &T) {}

    #[test]
    fn m4_server_trace_continues_or_starts_a_trace() {
        let inbound = TraceParent::parse(TP).unwrap();
        let t = server_trace(Some(inbound), [7; 8], [9; 16]).unwrap();
        assert_eq!(
            (t.trace_id(), t.span_id(), t.flags()),
            (inbound.trace_id(), [7; 8], 1)
        );
        let root = server_trace(None, [7; 8], [9; 16]).unwrap();
        assert_eq!((root.trace_id(), root.span_id()), ([9; 16], [7; 8]));
        assert!(root.is_sampled());
        assert_eq!(server_trace(Some(inbound), [0; 8], [9; 16]), None);
        assert_eq!(server_trace(None, [7; 8], [0; 16]), None);
    }

    #[test]
    fn m4_downstream_fills_unset_options_from_the_request() {
        let span = TraceParent::parse(TP).unwrap().child([7; 8]).unwrap();
        let caller_id = RequestId::new("req-1").unwrap();
        let now = Instant::now();
        let d = Defaults {
            trace: Some(span),
            request_id: Some(caller_id),
            deadline: Some(now + Duration::from_millis(800)),
        };
        let filled = d.fill(&RequestOptions::DEFAULT, now);
        assert_eq!(filled.trace, Some(span));
        assert_eq!(filled.request_id, Some(caller_id));
        assert_eq!(filled.timeout, Some(Duration::from_millis(800)));
        // The call's own options win.
        let own_trace = TraceParent::new([1; 16], [2; 8], 0).unwrap();
        let own_id = RequestId::new("own").unwrap();
        let own = RequestOptions::DEFAULT
            .with_timeout(Duration::from_millis(5))
            .with_trace(own_trace)
            .with_request_id(own_id);
        assert_eq!(d.fill(&own, now), own);
        // A passed deadline leaves no time.
        let late = d.fill(&RequestOptions::DEFAULT, now + Duration::from_secs(1));
        assert_eq!(late.timeout, Some(Duration::ZERO));
        let none = Defaults {
            trace: None,
            request_id: None,
            deadline: None,
        };
        assert_eq!(
            none.fill(&RequestOptions::DEFAULT, now),
            RequestOptions::DEFAULT
        );
        assert_eq!(remaining(now, now + Duration::from_secs(1)), Duration::ZERO);
    }

    #[test]
    fn m4_ctx_exposes_meta_trace_and_declared_clients_only() {
        let meta = RequestMeta::NONE
            .with_trace(TraceParent::parse(TP).unwrap())
            .with_request_id_str("req-2");
        let span = server_trace(meta.trace, [3; 8], [4; 16]);
        let mut links = Links::new().with_client(KIND, MeshClient::new(KIND, MeshDialer::direct()));
        assert_eq!(links.kinds().collect::<Vec<_>>(), ["demo/echo"]);
        let at = Instant::now() + Duration::from_secs(1);
        let mut ctx = Ctx::new(meta, span, &mut links).with_deadline(at);
        assert_eq!(ctx.meta(), &meta);
        assert_eq!(ctx.trace(), span);
        assert_eq!(ctx.request_id().map(|r| r.as_str()), Some("req-2"));
        assert_eq!(ctx.deadline(), Some(at));
        assert!(ctx.client("cam/jpeg").is_none(), "undeclared kind");
        {
            let down = ctx.client("demo/echo").expect("declared kind");
            let eff = down.effective_options(&RequestOptions::DEFAULT);
            assert_eq!(eff.trace, span);
            assert_eq!(eff.request_id.map(|r| r.as_str()), Some("req-2"));
            assert!(eff.timeout.is_some_and(|t| t <= Duration::from_secs(1)));
            assert_send(&down);
        }
        assert!(ctx.publisher().is_none(), "no zenoh session");
        assert_send(&ctx);
    }

    /// A handler calling a dependency: its future is `Send`, so it boxes.
    struct Caller;

    impl ServiceHandler for Caller {
        async fn handle(
            &mut self,
            ctx: &mut Ctx<'_>,
            req: &Request<'_>,
            out: &mut [u8],
        ) -> Result<usize, HandlerError> {
            let mut down = ctx.client("demo/echo").ok_or(HandlerError::Unavailable)?;
            let reply = down
                .request(req.key, req.body, out)
                .await
                .map_err(|e| e.handler_error().unwrap_or(HandlerError::Internal))?;
            Ok(reply.len())
        }
    }

    #[tokio::test]
    async fn m4_plain_handlers_and_async_handlers_box_alike() {
        let mut links = Links::new();
        let mut echo = BoxedHandler::new(handler_fn(|r, out| {
            let dst = out
                .get_mut(..r.body.len())
                .ok_or(HandlerError::BufferTooSmall)?;
            dst.copy_from_slice(r.body);
            Ok(r.body.len())
        }));
        let req = Request::new(Key::from_static("demo/echo"), b"hi");
        let mut out = [0u8; 8];
        let mut ctx = Ctx::new(RequestMeta::NONE, None, &mut links);
        assert_eq!(echo.call(&mut ctx, &req, &mut out).await, Ok(2));
        assert_eq!(&out[..2], b"hi");
        // The async handler without its dependency reports Unavailable.
        let mut caller = BoxedHandler::new(Caller);
        let fut = caller.call(&mut ctx, &req, &mut out);
        assert_send(&fut);
        assert_eq!(fut.await, Err(HandlerError::Unavailable));
        assert_eq!(format!("{caller:?}"), "BoxedHandler");
    }
}
