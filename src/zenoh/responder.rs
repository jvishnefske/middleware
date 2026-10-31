//! [`ZenohResponder`]: answers Zenoh queries on one key with a pure
//! [`Handler`].
//!
//! A responder declares two queryables that feed **one** FIFO, so queries
//! are served in arrival order whichever way they were addressed:
//!
//! * the key itself, reached by [`Target::Any`](crate::transport::Target)
//!   requests and by [`Gather`](crate::transport::Gather);
//! * the per-node key `@mw/node/<zid16>/<key>`
//!   ([`node_key`](super::node_key)), reached by
//!   [`Target::Node`](crate::transport::Target) requests for this
//!   responder's session.
//!
//! The FIFO entry is stamped with the time the query arrived.
//! [`Responder::serve_one`] receives one query, copies its payload to the
//! front of `scratch`, and runs the handler with the responder's key, the
//! remainder of `scratch` as the response buffer and a [`RequestMeta`]:
//!
//! * `sender`: the requester's sender id from the query attachment
//!   ([`wire`]), as the requester claims it (self-asserted; see the trust
//!   model in [`super`]); `None` for a query without it (another Zenoh
//!   client).
//! * `deadline_ms`: the requester's timeout carried in the attachment, minus
//!   the time the query waited in the FIFO. Time on the network is not
//!   subtracted (the nodes share no clock), so the true budget is a little
//!   smaller.
//! * `principal`: always `None`; Zenoh 1.x authenticates no caller name
//!   this backend can see.
//!
//! The reply goes to the key expression the query was sent to, with a reply
//! attachment naming this responder; a handler error is a Zenoh reply error
//! whose payload is the [`HandlerError::code`] byte and this responder's
//! sender id. A request that does not fit `scratch` is answered with
//! [`HandlerError::BufferTooSmall`] without calling the handler. In every one
//! of these cases the query has been answered, so `serve_one` returns
//! `Ok(())`; it fails only when Zenoh itself fails.

use std::time::Instant;

use ::zenoh::Session;
use ::zenoh::handlers::{Callback, FifoChannel, FifoChannelHandler, IntoHandler};
use ::zenoh::query::{Query, Queryable};

use crate::key::Key;
use crate::transport::{
    Handler, HandlerError, Request, RequestMeta, Responder, SenderId, copy_into,
};

use super::{ZenohError, wire};

type Stamped = (Instant, Query);

/// Answers queries on the one key it was declared for (and its per-node
/// key) by running a pure [`Handler`].
#[derive(Debug)]
pub struct ZenohResponder<'k> {
    key: Key<'k>,
    sender: SenderId,
    queries: FifoChannelHandler<Stamped>,
    _plain: Queryable<()>,
    _node: Queryable<()>,
}

/// A queryable callback pushing each query, stamped, into `fifo`.
fn stamping(fifo: Callback<Stamped>) -> impl Fn(Query) + Send + Sync + 'static {
    move |q| fifo.call((Instant::now(), q))
}

impl<'k> ZenohResponder<'k> {
    pub(super) async fn declare(
        session: &Session,
        sender: SenderId,
        key: Key<'k>,
        depth: usize,
    ) -> Result<Self, ZenohError> {
        let (fifo, queries) = FifoChannel::new(depth).into_handler();
        // The plain queryable is declared first: a peer that sees the
        // per-node one has seen both.
        let plain = session
            .declare_queryable(key.as_str())
            .callback(stamping(fifo.clone()))
            .await?;
        let node = session
            .declare_queryable(wire::node_key(sender, key))
            .callback(stamping(fifo))
            .await?;
        Ok(Self {
            key,
            sender,
            queries,
            _plain: plain,
            _node: node,
        })
    }

    /// The key this responder serves.
    pub fn key(&self) -> Key<'_> {
        self.key
    }

    /// The node id requests address with `Target::Node`.
    pub fn sender_id(&self) -> SenderId {
        self.sender
    }
}

/// The [`RequestMeta`] of a query with `attachment` that waited
/// `waited_ms` in the FIFO. Pure.
pub fn request_meta(attachment: Option<&[u8]>, waited_ms: u64) -> RequestMeta<'static> {
    match attachment.and_then(wire::decode_query) {
        Some((sender, deadline)) => {
            let waited = u32::try_from(waited_ms).unwrap_or(u32::MAX);
            RequestMeta::NONE
                .with_sender(sender)
                .with_deadline_ms(deadline.saturating_sub(waited))
        }
        None => RequestMeta::NONE,
    }
}

/// Runs `handler` with the request at the front of `scratch` and the rest as
/// the response buffer. Returns the response slice or the handler error.
fn run_handler<'s, H: Handler>(
    handler: &mut H,
    key: Key<'_>,
    request: &[u8],
    meta: RequestMeta<'_>,
    scratch: &'s mut [u8],
) -> Result<&'s [u8], HandlerError> {
    let request_len = request.len();
    copy_into(scratch, request).ok_or(HandlerError::BufferTooSmall)?;
    let (req, resp) = scratch.split_at_mut(request_len);
    let n = handler.handle(&Request::new(key, req).with_meta(meta), resp)?;
    resp.get(..n).ok_or(HandlerError::BufferTooSmall)
}

impl Responder for ZenohResponder<'_> {
    type Error = ZenohError;

    /// Serves the next query from either queryable (see the module docs).
    /// Cancel safe: a dropped wait leaves the query queued.
    async fn serve_one<H: Handler>(
        &mut self,
        handler: &mut H,
        scratch: &mut [u8],
    ) -> Result<(), ZenohError> {
        let (arrived, query) = self
            .queries
            .recv_async()
            .await
            .map_err(|_| ZenohError::Closed)?;
        let waited_ms = u64::try_from(arrived.elapsed().as_millis()).unwrap_or(u64::MAX);
        let attachment = query.attachment().map(|a| a.to_bytes());
        let meta = request_meta(attachment.as_deref(), waited_ms);
        let request = query.payload().map(|p| p.to_bytes()).unwrap_or_default();
        match run_handler(handler, self.key, &request, meta, scratch) {
            Ok(response) => {
                query
                    .reply(query.key_expr().clone(), response)
                    .attachment(wire::encode_reply(self.sender).to_vec())
                    .await?
            }
            Err(e) => {
                query
                    .reply_err(wire::encode_reply_error(e, self.sender).to_vec())
                    .await?
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_handler_splits_scratch_and_reports_errors() {
        let mut echo = crate::transport::handler_fn(|req, resp| {
            copy_into(resp, req.body)
                .map(|s| s.len())
                .ok_or(HandlerError::BufferTooSmall)
        });
        let key = Key::from_static("a/b");
        let mut scratch = [0u8; 8];
        let none = RequestMeta::NONE;
        assert_eq!(
            run_handler(&mut echo, key, b"abc", none, &mut scratch),
            Ok(&b"abc"[..])
        );
        assert_eq!(
            run_handler(&mut echo, key, b"abcde", none, &mut scratch),
            Err(HandlerError::BufferTooSmall)
        );
        assert_eq!(
            run_handler(&mut echo, key, b"too long for scratch", none, &mut scratch),
            Err(HandlerError::BufferTooSmall)
        );
    }

    #[test]
    fn request_meta_carries_sender_and_remaining_deadline() {
        let id = SenderId([3; 8]);
        let a = wire::encode_query(id, 3000);
        assert_eq!(
            request_meta(Some(&a), 250),
            RequestMeta::NONE.with_sender(id).with_deadline_ms(2750)
        );
        assert_eq!(request_meta(Some(&a), 5000).deadline_ms, Some(0));
        assert_eq!(request_meta(Some(&a), u64::MAX).deadline_ms, Some(0));
        assert_eq!(request_meta(None, 0), RequestMeta::NONE);
        assert_eq!(request_meta(Some(b"junk"), 0), RequestMeta::NONE);
    }
}
