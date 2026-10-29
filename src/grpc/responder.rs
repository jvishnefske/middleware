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

use super::GrpcError;
use super::codec::RawCodec;
use super::status::to_status;
use crate::key::Key;
use crate::transport::{Handler, HandlerError, Request as MwRequest, RequestMeta, Responder};
use bytes::Bytes;
use core::convert::Infallible;
use core::task::{Context, Poll};
use tokio::sync::{mpsc, oneshot};
use tonic::body::Body;
use tonic::codegen::{BoxFuture, Service};
use tonic::server::{Grpc, UnaryService};
use tonic::{Request, Response, Status};

/// One queued unary call.
#[derive(Debug)]
struct Call {
    key: String,
    request: Bytes,
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
pub fn responder(capacity: usize) -> (ResponderService, GrpcResponder) {
    let (tx, rx) = mpsc::channel(capacity.max(1));
    (ResponderService { tx }, GrpcResponder { rx })
}

/// tonic half of [`responder`]: forwards unary calls to the
/// [`GrpcResponder`]. Clones feed the same responder.
#[derive(Debug, Clone)]
pub struct ResponderService {
    tx: mpsc::Sender<Call>,
}

impl ResponderService {
    /// Handles one HTTP/2 request: a unary call on `/Service/Method`.
    pub async fn handle(self, req: http::Request<Body>) -> http::Response<Body> {
        match key_from_path(req.uri().path()) {
            Some(key) => {
                let method = UnaryMethod {
                    key: key.as_str().to_owned(),
                    tx: self.tx,
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
}

impl UnaryService<Bytes> for UnaryMethod {
    type Response = Bytes;
    type Future = BoxFuture<Response<Bytes>, Status>;

    fn call(&mut self, request: Request<Bytes>) -> Self::Future {
        let (reply, answer) = oneshot::channel();
        let call = Call {
            key: core::mem::take(&mut self.key),
            request: request.into_inner(),
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
    /// sees no [`RequestMeta`] yet (caller identity and deadline are not
    /// carried through this responder).
    async fn serve_one<H: Handler>(
        &mut self,
        handler: &mut H,
        scratch: &mut [u8],
    ) -> Result<(), GrpcError> {
        let call = self.rx.recv().await.ok_or(GrpcError::Closed)?;
        let reply = match Key::new(&call.key) {
            Ok(key) => run_handler(handler, key, &call.request, RequestMeta::NONE, scratch).map_err(to_status),
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
}
