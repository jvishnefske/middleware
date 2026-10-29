//! Server side of unary calls: [`GrpcResponder`] implements
//! [`Responder`] by running a pure [`Handler`].
//!
//! tonic serves requests from its own tasks, while [`Responder::serve_one`]
//! borrows a handler and a scratch buffer from the caller. The two meet over
//! a bounded tokio `mpsc` channel created by [`responder`]:
//!
//! * [`ResponderService`] (the tonic half, cloneable, `'static`) decodes a
//!   unary call on `/Service/Method`, turns the path into the key
//!   `Service/Method` and queues the request with a `oneshot` reply slot.
//!   A full queue answers `UNAVAILABLE` (retriable) instead of buffering
//!   without bound.
//! * [`GrpcResponder`] (the caller's half) takes one queued call per
//!   [`serve_one`](Responder::serve_one), copies the request into the front
//!   of `scratch`, lets the handler write the response into the rest, and
//!   sends the reply. Handler errors become gRPC statuses through
//!   [`status::to_status`](super::status::to_status).
//!
//! Which keys exist is the handler's decision: every non-bus path reaches
//! it, and it answers `HandlerError::NotFound` (sent as `UNIMPLEMENTED`) for
//! keys it does not serve.
//!
//! # Request metadata
//!
//! The handler's [`RequestMeta`] is read from the call's metadata when it
//! arrives ([`call_meta`]):
//!
//! * `sender` — the caller's [`CLIENT_HEADER`] (`mw-client`, sent by every
//!   [`GrpcTransport`](super::GrpcTransport)); `None` when absent or
//!   malformed. It is the caller's claim, not authenticated.
//! * `principal` — with [`PrincipalSource::SidecarXfcc`] (feature
//!   `istio`), the SPIFFE ID of the last element of Envoy's
//!   `x-forwarded-client-cert` ([`peer_principal`](super::istio::peer_principal));
//!   with [`PrincipalSource::None`], the default, always `None`. Choose
//!   `SidecarXfcc` only for a server that is reachable solely through its
//!   Istio sidecar, which sets or sanitizes that header; anywhere else a
//!   client can forge it.
//! * `deadline_ms` — the `grpc-timeout` header
//!   ([`GrpcTimeout`]), turned into a deadline
//!   on arrival and handed to the handler as the milliseconds left when
//!   [`serve_one`](Responder::serve_one) runs it (`Some(0)` once passed,
//!   rounded down). `None` without the header or with a malformed one.

use super::GrpcError;
use super::bus::{CLIENT_HEADER, parse_client_id};
use super::codec::RawCodec;
use super::status::to_status;
use super::timeout::{GRPC_TIMEOUT_HEADER, GrpcTimeout};
use crate::key::Key;
use crate::transport::{
    Handler, HandlerError, Request as MwRequest, RequestMeta, Responder, SenderId,
};
use bytes::Bytes;
use core::convert::Infallible;
use core::task::{Context, Poll};
use core::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tonic::body::Body;
use tonic::codegen::{BoxFuture, Service};
use tonic::metadata::MetadataMap;
use tonic::server::{Grpc, UnaryService};
use tonic::{Request, Response, Status};

/// Where the responder takes `RequestMeta::principal` from (see the
/// module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum PrincipalSource {
    /// Nothing authenticates the caller: `principal` is always `None`.
    #[default]
    None,
    /// The SPIFFE ID in the last element of `x-forwarded-client-cert`, as
    /// set by the Istio sidecar in front of this server. Only sound when
    /// every request reaches the server through that sidecar.
    #[cfg(feature = "istio")]
    SidecarXfcc,
}

/// What a call's metadata says about its caller, owned so it can wait in
/// the responder's queue; the input of a handler's [`RequestMeta`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallMeta {
    /// From [`CLIENT_HEADER`].
    pub sender: Option<SenderId>,
    /// From the configured [`PrincipalSource`].
    pub principal: Option<String>,
    /// The `grpc-timeout` budget, measured from arrival.
    pub timeout: Option<Duration>,
}

/// Reads [`CallMeta`] from a call's metadata; pure.
pub fn call_meta(metadata: &MetadataMap, source: PrincipalSource) -> CallMeta {
    let sender = metadata
        .get(CLIENT_HEADER)
        .and_then(|v| parse_client_id(v.as_bytes()));
    let timeout = metadata
        .get(GRPC_TIMEOUT_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(GrpcTimeout::parse)
        .map(GrpcTimeout::as_duration);
    let principal = match source {
        PrincipalSource::None => None,
        #[cfg(feature = "istio")]
        PrincipalSource::SidecarXfcc => metadata
            .get(super::istio::XFCC_HEADER)
            .and_then(|v| v.to_str().ok())
            .and_then(super::istio::peer_principal)
            .map(str::to_owned),
    };
    CallMeta {
        sender,
        principal,
        timeout,
    }
}

/// Milliseconds left until `deadline` at `now`, rounded down and
/// saturating at `u32::MAX`; 0 once it has passed. Pure.
pub fn remaining_ms(deadline: Instant, now: Instant) -> u32 {
    let left = deadline.saturating_duration_since(now).as_millis();
    u32::try_from(left).unwrap_or(u32::MAX)
}

/// One queued unary call.
#[derive(Debug)]
struct Call {
    key: String,
    request: Bytes,
    sender: Option<SenderId>,
    principal: Option<String>,
    deadline: Option<Instant>,
    reply: oneshot::Sender<Result<Bytes, Status>>,
}

/// The key for an HTTP/2 path `/Service/Method`, or `None` when the path
/// is not exactly two valid key segments.
pub fn key_from_path(path: &str) -> Option<Key<'_>> {
    let key = Key::new(path.strip_prefix('/')?).ok()?;
    key.service_method().map(|_| key)
}

/// Creates a connected pair: the tonic-facing service and the
/// [`Responder`] that answers its calls. `capacity` (minimum 1) bounds the
/// number of calls waiting for [`Responder::serve_one`].
/// The service takes no principal ([`PrincipalSource::None`]); see
/// [`ResponderService::with_principal_source`].
pub fn responder(capacity: usize) -> (ResponderService, GrpcResponder) {
    let (tx, rx) = mpsc::channel(capacity.max(1));
    (
        ResponderService {
            tx,
            principals: PrincipalSource::None,
        },
        GrpcResponder { rx },
    )
}

/// tonic half of [`responder`]: forwards unary calls to the
/// [`GrpcResponder`]. Clones feed the same responder.
#[derive(Debug, Clone)]
pub struct ResponderService {
    tx: mpsc::Sender<Call>,
    principals: PrincipalSource,
}

impl ResponderService {
    /// Returns this service taking `RequestMeta::principal` from `source`
    /// (see the module docs for when `SidecarXfcc` is sound).
    pub fn with_principal_source(self, source: PrincipalSource) -> Self {
        Self {
            principals: source,
            ..self
        }
    }

    /// Where `RequestMeta::principal` comes from.
    pub fn principal_source(&self) -> PrincipalSource {
        self.principals
    }

    /// Handles one HTTP/2 request: a unary call on `/Service/Method`.
    pub async fn handle(self, req: http::Request<Body>) -> http::Response<Body> {
        match key_from_path(req.uri().path()) {
            Some(key) => {
                let method = UnaryMethod {
                    key: key.as_str().to_owned(),
                    tx: self.tx,
                    principals: self.principals,
                };
                Grpc::new(RawCodec).unary(method, req).await
            }
            None => Status::unimplemented("path is not /Service/Method").into_http(),
        }
    }
}

impl Service<http::Request<Body>> for ResponderService {
    type Response = http::Response<Body>;
    type Error = Infallible;
    type Future = BoxFuture<Self::Response, Infallible>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<Body>) -> Self::Future {
        let this = self.clone();
        Box::pin(async move { Ok(this.handle(req).await) })
    }
}

struct UnaryMethod {
    key: String,
    tx: mpsc::Sender<Call>,
    principals: PrincipalSource,
}

impl UnaryService<Bytes> for UnaryMethod {
    type Response = Bytes;
    type Future = BoxFuture<Response<Bytes>, Status>;

    fn call(&mut self, request: Request<Bytes>) -> Self::Future {
        let (reply, answer) = oneshot::channel();
        let meta = call_meta(request.metadata(), self.principals);
        let deadline = meta.timeout.and_then(|t| Instant::now().checked_add(t));
        let call = Call {
            key: core::mem::take(&mut self.key),
            request: request.into_inner(),
            sender: meta.sender,
            principal: meta.principal,
            deadline,
            reply,
        };
        let queued = self.tx.try_send(call);
        Box::pin(async move {
            queued.map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => Status::unavailable("responder busy"),
                mpsc::error::TrySendError::Closed(_) => Status::unavailable("responder stopped"),
            })?;
            let body = answer
                .await
                .map_err(|_| Status::internal("responder dropped the call"))??;
            Ok(Response::new(body))
        })
    }
}

/// Runs `handler` for `key` with `request` copied into the front of
/// `scratch` and the response written into the remainder; the handler sees
/// `meta`. Pure apart from writing `scratch`.
pub fn run_handler<H: Handler>(
    handler: &mut H,
    key: Key<'_>,
    request: &[u8],
    meta: RequestMeta<'_>,
    scratch: &mut [u8],
) -> Result<Bytes, HandlerError> {
    let (req_buf, resp_buf) = scratch
        .split_at_mut_checked(request.len())
        .ok_or(HandlerError::BufferTooSmall)?;
    req_buf.copy_from_slice(request);
    let n = handler.handle(&MwRequest::new(key, req_buf).with_meta(meta), resp_buf)?;
    let out = resp_buf.get(..n).ok_or(HandlerError::BufferTooSmall)?;
    Ok(Bytes::copy_from_slice(out))
}

/// Caller half of [`responder`]; implements [`Responder`].
#[derive(Debug)]
pub struct GrpcResponder {
    rx: mpsc::Receiver<Call>,
}

impl Responder for GrpcResponder {
    type Error = GrpcError;

    /// Answers one queued call. Handler failures are sent to the client as a
    /// status and still return `Ok(())`; only a closed queue is an error. A
    /// client that disconnected before the reply is ignored. The handler
    /// sees the [`RequestMeta`] described in the module docs, with
    /// `deadline_ms` measured now.
    async fn serve_one<H: Handler>(
        &mut self,
        handler: &mut H,
        scratch: &mut [u8],
    ) -> Result<(), GrpcError> {
        let call = self.rx.recv().await.ok_or(GrpcError::Closed)?;
        let meta = RequestMeta {
            sender: call.sender,
            principal: call.principal.as_deref(),
            deadline_ms: call.deadline.map(|d| remaining_ms(d, Instant::now())),
        };
        let reply = match Key::new(&call.key) {
            Ok(key) => run_handler(handler, key, &call.request, meta, scratch).map_err(to_status),
            Err(_) => Err(Status::invalid_argument("invalid key")),
        };
        let _ = call.reply.send(reply);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_from_path_requires_service_method() {
        assert_eq!(
            key_from_path("/pkg.Svc/Call").map(|k| k.as_str()),
            Some("pkg.Svc/Call")
        );
        assert!(key_from_path("pkg.Svc/Call").is_none());
        assert!(key_from_path("/a").is_none());
        assert!(key_from_path("/a/b/c").is_none());
        assert!(key_from_path("/a/b c").is_none());
    }

    #[test]
    fn run_handler_splits_scratch() {
        let mut echo = crate::transport::handler_fn(|req, resp| {
            let out = resp
                .get_mut(..req.body.len())
                .ok_or(HandlerError::BufferTooSmall)?;
            out.copy_from_slice(req.body);
            Ok(req.body.len())
        });
        let k = Key::from_static("s/m");
        let mut scratch = [0u8; 6];
        assert_eq!(
            run_handler(&mut echo, k, b"abc", RequestMeta::NONE, &mut scratch).unwrap(),
            Bytes::from_static(b"abc")
        );
        assert_eq!(
            run_handler(&mut echo, k, b"abcd", RequestMeta::NONE, &mut scratch),
            Err(HandlerError::BufferTooSmall)
        );
        assert_eq!(
            run_handler(&mut echo, k, b"abcdefg", RequestMeta::NONE, &mut scratch),
            Err(HandlerError::BufferTooSmall)
        );
    }

    #[test]
    fn g10_call_meta_reads_client_timeout_and_only_trusted_principals() {
        let mut m = MetadataMap::new();
        assert_eq!(call_meta(&m, PrincipalSource::None), CallMeta::default());
        m.insert(CLIENT_HEADER, "0102030405060708".parse().unwrap());
        m.insert(GRPC_TIMEOUT_HEADER, "250m".parse().unwrap());
        m.insert(
            "x-forwarded-client-cert",
            "By=spiffe://td/s;URI=spiffe://td/ns/a/sa/b"
                .parse()
                .unwrap(),
        );
        let meta = call_meta(&m, PrincipalSource::None);
        assert_eq!(meta.sender, Some(SenderId([1, 2, 3, 4, 5, 6, 7, 8])));
        assert_eq!(meta.timeout, Some(Duration::from_millis(250)));
        assert_eq!(meta.principal, None, "XFCC is ignored unless trusted");
        #[cfg(feature = "istio")]
        assert_eq!(
            call_meta(&m, PrincipalSource::SidecarXfcc)
                .principal
                .as_deref(),
            Some("spiffe://td/ns/a/sa/b")
        );
        m.insert(CLIENT_HEADER, "zz".parse().unwrap());
        m.insert(GRPC_TIMEOUT_HEADER, "soon".parse().unwrap());
        let meta = call_meta(&m, PrincipalSource::None);
        assert_eq!((meta.sender, meta.timeout), (None, None));
    }

    #[test]
    fn g10_remaining_ms_rounds_down_and_saturates() {
        let now = Instant::now();
        assert_eq!(remaining_ms(now + Duration::from_micros(2_999), now), 2);
        assert_eq!(remaining_ms(now, now + Duration::from_secs(1)), 0);
        assert_eq!(
            remaining_ms(now + Duration::from_secs(u64::from(u32::MAX)), now),
            u32::MAX
        );
    }
}
