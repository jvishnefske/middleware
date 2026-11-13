//! Serve echo (ABI 5): serves [`KEY`] and answers each request with the
//! device's node id (8 bytes) followed by the request body, or
//! `BufferTooSmall` when that does not fit the caller's buffer. `init`
//! logs `serve_echo: node <16 hex digits>` and the result of `serve`.
//!
//! [`answer`] is the whole handler as a pure function, unit tested on the
//! host.
#![no_std]
#![forbid(unsafe_code)]

use mw_payload::serve::{self, HandlerError, Request, SenderId, ServeSlot};
use mw_payload::{LogLine, Payload};

/// The served key.
pub const KEY: &str = "app/serve_echo";

/// Application state: the node id and the slot, if serving succeeded.
pub struct ServeEcho {
    node: SenderId,
    slot: Option<ServeSlot>,
}

/// The reply to `body` from node `node`: written into `reply`, returning
/// its length.
pub fn answer(node: SenderId, body: &[u8], reply: &mut [u8]) -> Result<usize, HandlerError> {
    let n = body.len() + node.0.len();
    let out = reply.get_mut(..n).ok_or(HandlerError::BufferTooSmall)?;
    let (id, rest) = out.split_at_mut(node.0.len());
    id.copy_from_slice(&node.0);
    rest.copy_from_slice(body);
    Ok(n)
}

impl Payload for ServeEcho {
    fn init() -> Self {
        let node = serve::node_id().unwrap_or_default();
        let mut hex = [0u8; 16];
        serve::node_hex(node, &mut hex);
        let mut line = LogLine::new();
        line.push_str("serve_echo: node ").push_bytes_lossy(&hex);
        line.log();
        let slot = serve::serve(KEY).ok();
        mw_payload::log(if slot.is_some() {
            "serve_echo: serving"
        } else {
            "serve_echo: serve failed"
        });
        ServeEcho { node, slot }
    }

    fn on_request(
        &mut self,
        slot: ServeSlot,
        req: &Request<'_>,
        reply: &mut [u8],
    ) -> Result<usize, HandlerError> {
        if self.slot != Some(slot) {
            return Err(HandlerError::NotFound);
        }
        answer(self.node, req.body, reply)
    }
}

// The exports need a single-threaded wasm32 module; host builds only run
// the unit tests below.
#[cfg(target_arch = "wasm32")]
mw_payload::payload!(ServeEcho, serve);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_node_then_body_or_buffer_too_small() {
        let node = SenderId([1, 2, 3, 4, 5, 6, 7, 8]);
        let mut reply = [0u8; 12];
        assert_eq!(answer(node, b"hi", &mut reply), Ok(10));
        assert_eq!(&reply[..10], &[1, 2, 3, 4, 5, 6, 7, 8, b'h', b'i']);
        assert_eq!(
            answer(node, b"hello", &mut reply),
            Err(HandlerError::BufferTooSmall)
        );
    }
}
