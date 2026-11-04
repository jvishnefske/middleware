//! Serving requests (ABI 5): [`serve`] a key, answer in
//! [`Payload::on_request`](crate::Payload::on_request), and read the
//! device's [`node_id`].
//!
//! Build the payload with `payload!(App, serve)` so it exports
//! `on_request` and its receive buffer holds a whole request
//! ([`MAX_REQUEST_BUFFER`] bytes: the meta block, at most [`MAX_REQUEST`]
//! body bytes and at most [`MAX_REPLY`] reply bytes). The handler gets the
//! body and [`RequestMeta`] (sender and deadline; never a principal) and
//! writes its reply into the buffer it is given, returning the length or a
//! [`HandlerError`]: the same contract as a middleware `Handler`. Firmware
//! without ABI 5 refuses to load a payload that imports `serve` or
//! `node_id`; firmware that cannot route requests answers `serve` with
//! [`AbiError::Unsupported`].
//!
//! ```ignore
//! struct App { slot: Option<mw_payload::serve::ServeSlot> }
//! impl mw_payload::Payload for App {
//!     fn init() -> Self {
//!         App { slot: mw_payload::serve::serve("app/echo").ok() }
//!     }
//!     fn on_request(&mut self, _: ServeSlot, req: &Request<'_>, reply: &mut [u8])
//!         -> Result<usize, HandlerError> {
//!         let out = reply.get_mut(..req.body.len()).ok_or(HandlerError::BufferTooSmall)?;
//!         out.copy_from_slice(req.body);
//!         Ok(out.len())
//!     }
//! }
//! mw_payload::payload!(App, serve);
//! ```

pub use middleware::wasm_abi::serve::{
    MAX_REPLY, MAX_REQUEST, MAX_REQUEST_BUFFER, MAX_SERVES, NODE_ID_LEN, REQUEST_META_LEN,
    decode_meta, encode_meta, reply_code,
};
pub use middleware::{HandlerError, RequestMeta, SenderId};

use crate::{AbiError, check, len_i32, ptr_i32, sys};

/// A served key's handle, passed back to
/// [`Payload::on_request`](crate::Payload::on_request).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ServeSlot(u8);

impl ServeSlot {
    /// Parses a host-provided slot; `None` unless `0 <= raw < MAX_SERVES`.
    pub fn from_raw(raw: i32) -> Option<Self> {
        let i = usize::try_from(raw).ok().filter(|&i| i < MAX_SERVES)?;
        u8::try_from(i).ok().map(ServeSlot)
    }

    /// The slot index, `0..MAX_SERVES`.
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// One request as the payload sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request<'a> {
    /// The request body.
    pub body: &'a [u8],
    /// Sender and remaining deadline, as far as the transport knows them.
    pub meta: RequestMeta<'static>,
}

/// Serves requests on `key`; serving it again returns the same slot.
///
/// Fails with [`AbiError::InvalidKey`] for a malformed or `sys/` key,
/// [`AbiError::NoResources`] once [`MAX_SERVES`] keys are served, and
/// [`AbiError::Unsupported`] when the payload was built without
/// `payload!(App, serve)` or the firmware cannot route requests.
pub fn serve(key: &str) -> Result<ServeSlot, AbiError> {
    let raw = check(sys::serve(ptr_i32(key.as_bytes()), len_i32(key.len())?))?;
    ServeSlot::from_raw(raw).ok_or(AbiError::Unsupported)
}

/// The device's node id, the [`SenderId`] its transports and `sys/info`
/// report; [`AbiError::Unsupported`] when the device has none.
pub fn node_id() -> Result<SenderId, AbiError> {
    let mut id = [0u8; NODE_ID_LEN];
    check(sys::node_id(&mut id))?;
    Ok(SenderId(id))
}

/// Writes `id` as 16 lowercase hex digits into `out`, the form keys use
/// (`unit/<node>/...`).
pub fn node_hex(id: SenderId, out: &mut [u8; 16]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for (pair, b) in out.chunks_exact_mut(2).zip(id.0) {
        pair[0] = HEX[usize::from(b >> 4)];
        pair[1] = HEX[usize::from(b & 15)];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_hex_and_host_stubs() {
        assert_eq!(ServeSlot::from_raw(7).map(ServeSlot::index), Some(7));
        assert_eq!(ServeSlot::from_raw(8), None);
        assert_eq!(ServeSlot::from_raw(-1), None);
        let mut hex = [0u8; 16];
        node_hex(SenderId([0x24, 0x6f, 0, 0, 0, 0, 0, 0xfe]), &mut hex);
        assert_eq!(&hex, b"246f0000000000fe");
        assert_eq!(serve("app/x"), Err(AbiError::Unsupported));
        assert_eq!(node_id(), Err(AbiError::Unsupported));
    }
}
