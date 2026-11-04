//! ABI 5: payloads that serve requests.
//!
//! A payload registers a request key with the `serve(key_ptr, key_len)`
//! import and answers requests on it from its `on_request` export. The
//! `node_id(ptr)` import writes the device's 8-byte
//! [`SenderId`](crate::SenderId) into guest memory, so a payload can build
//! keys that name its own node (for example the S88 phase keys
//! `unit/<node>/phase/...`).
//!
//! # One request
//!
//! The host never allocates per request. It asks the payload for one buffer
//! with `mw_alloc(len)`, where `len` is [`RequestLayout::alloc_len`]:
//!
//! ```text
//! ptr: | meta (REQUEST_META_LEN) | request (req_len) | reply (reply_cap) |
//! ```
//!
//! It writes the [`encode_meta`] block and the request body there and calls
//!
//! ```text
//! on_request(slot, req_ptr, req_len, meta_ptr, reply_ptr, reply_cap) -> i32
//! ```
//!
//! with the slot `serve` returned and the three regions' addresses. The
//! result is the reply length (`0..=reply_cap`; the host copies that many
//! bytes from `reply_ptr`) or a negated
//! [`HandlerError::code`](crate::HandlerError::code) (`-1` = `NotFound` ..
//! `-7` = `DeadlineExceeded`). [`reply_result`] decides: any other value is
//! the payload's fault and becomes `HandlerError::Internal`. `mw_alloc`
//! returning 0 (or a range outside linear memory) means the payload cannot
//! take the request now: `HandlerError::Unavailable`. A request longer than
//! [`MAX_REQUEST`] is `InvalidRequest` without calling the payload, and a
//! key nothing serves is `NotFound`. The reply capacity is the caller's
//! buffer, at most [`MAX_REPLY`]; a payload whose answer does not fit
//! returns `BufferTooSmall`.
//!
//! # The meta block
//!
//! [`REQUEST_META_LEN`] bytes, little-endian, written before the request:
//!
//! | offset | size | field |
//! |--------|------|-------|
//! | 0      | 1    | flags: bit 0 sender present, bit 1 deadline present; other bits 0 |
//! | 1      | 3    | reserved, 0 |
//! | 4      | 4    | `deadline_ms` (`u32`; 0 when absent) |
//! | 8      | 8    | sender (`SenderId`; zeros when absent) |
//!
//! `RequestMeta::principal` does not cross into the payload: a payload has
//! no policy role, and the string would need a variable-length block.
//!
//! Every rule here is pure; `wasm-host` applies them around the
//! interpreter, and the SDKs use [`decode_meta`] and [`reply_code`].

use crate::transport::{HandlerError, RequestMeta, SenderId};

use super::{AbiError, MAX_MESSAGE};

/// Most keys one payload may serve (`serve` slots `0..MAX_SERVES`).
pub const MAX_SERVES: usize = 8;

/// Bytes of the meta block before each request.
pub const REQUEST_META_LEN: usize = 16;

/// Longest request body the host hands to a payload.
pub const MAX_REQUEST: usize = MAX_MESSAGE;

/// Largest reply capacity the host offers.
pub const MAX_REPLY: usize = MAX_MESSAGE;

/// Largest buffer `mw_alloc` is asked for on a request: the meta block,
/// the longest request and the largest reply.
pub const MAX_REQUEST_BUFFER: usize = REQUEST_META_LEN + MAX_REQUEST + MAX_REPLY;

/// Bytes `node_id` writes.
pub const NODE_ID_LEN: usize = 8;

/// Meta flag: the sender is present.
pub const META_SENDER: u8 = 1;
/// Meta flag: the deadline is present.
pub const META_DEADLINE: u8 = 2;

/// Encodes what the payload may know of a request (sender and deadline).
pub fn encode_meta(meta: &RequestMeta<'_>) -> [u8; REQUEST_META_LEN] {
    let mut out = [0u8; REQUEST_META_LEN];
    let mut flags = 0u8;
    if let Some(d) = meta.deadline_ms {
        flags |= META_DEADLINE;
        out[4..8].copy_from_slice(&d.to_le_bytes());
    }
    if let Some(SenderId(s)) = meta.sender {
        flags |= META_SENDER;
        out[8..16].copy_from_slice(&s);
    }
    out[0] = flags;
    out
}

/// Decodes a meta block. Unknown flag bits or non-zero reserved bytes are
/// [`AbiError::InvalidArgument`] (a newer host); a short block is
/// [`AbiError::OutOfBounds`].
pub fn decode_meta(block: &[u8]) -> Result<RequestMeta<'static>, AbiError> {
    let block: &[u8; REQUEST_META_LEN] = block
        .get(..REQUEST_META_LEN)
        .and_then(|b| b.try_into().ok())
        .ok_or(AbiError::OutOfBounds)?;
    let flags = block[0];
    if flags & !(META_SENDER | META_DEADLINE) != 0 || block[1..4] != [0, 0, 0] {
        return Err(AbiError::InvalidArgument);
    }
    let deadline = u32::from_le_bytes([block[4], block[5], block[6], block[7]]);
    let mut sender = [0u8; 8];
    sender.copy_from_slice(&block[8..16]);
    Ok(RequestMeta {
        sender: (flags & META_SENDER != 0).then_some(SenderId(sender)),
        principal: None,
        deadline_ms: (flags & META_DEADLINE != 0).then_some(deadline),
    })
}

/// Where one request's parts go in the buffer `mw_alloc` returns, as
/// offsets from its start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestLayout {
    /// The length passed to `mw_alloc`.
    pub alloc_len: usize,
    /// Request body length.
    pub req_len: usize,
    /// Offset of the request body (the meta block is at 0).
    pub req_at: usize,
    /// Offset of the reply buffer.
    pub reply_at: usize,
    /// Reply capacity offered to the payload.
    pub reply_cap: usize,
}

/// Lays out a request of `req_len` bytes for a caller whose reply buffer
/// holds `reply_buf` bytes (offered up to [`MAX_REPLY`]). A request over
/// [`MAX_REQUEST`] is [`HandlerError::InvalidRequest`].
pub fn layout(req_len: usize, reply_buf: usize) -> Result<RequestLayout, HandlerError> {
    if req_len > MAX_REQUEST {
        return Err(HandlerError::InvalidRequest);
    }
    let reply_cap = reply_buf.min(MAX_REPLY);
    let req_at = REQUEST_META_LEN;
    let reply_at = req_at + req_len;
    Ok(RequestLayout {
        alloc_len: reply_at + reply_cap,
        req_len,
        req_at,
        reply_at,
        reply_cap,
    })
}

/// What `on_request` returned, for a reply capacity of `reply_cap`: the
/// reply length, the payload's [`HandlerError`], or `Internal` for a value
/// that is neither (a reply longer than the buffer, an unknown code).
pub fn reply_result(code: i32, reply_cap: usize) -> Result<usize, HandlerError> {
    if code >= 0 {
        return usize::try_from(code)
            .ok()
            .filter(|&n| n <= reply_cap)
            .ok_or(HandlerError::Internal);
    }
    match u8::try_from(code.unsigned_abs()) {
        Ok(c) if (1..=7).contains(&c) => Err(HandlerError::from_code(c)),
        _ => Err(HandlerError::Internal),
    }
}

/// The `on_request` result for a handler outcome (the SDKs' side of
/// [`reply_result`]). A length that does not fit `i32` is `Internal`.
pub fn reply_code(result: Result<usize, HandlerError>) -> i32 {
    match result {
        Ok(n) => i32::try_from(n).unwrap_or(-i32::from(HandlerError::Internal.code())),
        Err(e) => -i32::from(e.code()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_round_trips_and_drops_the_principal() {
        let sender = SenderId([1, 2, 3, 4, 5, 6, 7, 8]);
        let meta = RequestMeta::NONE
            .with_sender(sender)
            .with_deadline_ms(250)
            .with_principal("spiffe://x");
        let b = encode_meta(&meta);
        assert_eq!(b, [3, 0, 0, 0, 250, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8]);
        let back = decode_meta(&b).unwrap();
        assert_eq!(back.sender, Some(sender));
        assert_eq!(back.deadline_ms, Some(250));
        assert_eq!(back.principal, None);
        assert_eq!(encode_meta(&RequestMeta::NONE), [0; REQUEST_META_LEN]);
        assert_eq!(decode_meta(&[0; REQUEST_META_LEN]), Ok(RequestMeta::NONE));
        let zero = RequestMeta::NONE.with_deadline_ms(0);
        assert_eq!(
            decode_meta(&encode_meta(&zero)).unwrap().deadline_ms,
            Some(0)
        );
    }

    #[test]
    fn meta_from_a_newer_host_or_short_is_refused() {
        let mut b = [0u8; REQUEST_META_LEN];
        b[0] = 4;
        assert_eq!(decode_meta(&b), Err(AbiError::InvalidArgument));
        b[0] = 0;
        b[2] = 1;
        assert_eq!(decode_meta(&b), Err(AbiError::InvalidArgument));
        assert_eq!(decode_meta(&[0; 15]), Err(AbiError::OutOfBounds));
    }

    #[test]
    fn layout_puts_meta_request_and_reply_back_to_back() {
        let l = layout(7, 64).unwrap();
        assert_eq!(
            l,
            RequestLayout {
                alloc_len: 16 + 7 + 64,
                req_len: 7,
                req_at: 16,
                reply_at: 23,
                reply_cap: 64
            }
        );
        assert_eq!(layout(0, 10_000).unwrap().reply_cap, MAX_REPLY);
        assert_eq!(
            layout(MAX_REQUEST, MAX_REPLY).unwrap().alloc_len,
            MAX_REQUEST_BUFFER
        );
        assert_eq!(
            layout(MAX_REQUEST + 1, 8),
            Err(HandlerError::InvalidRequest)
        );
    }

    #[test]
    fn reply_results_are_lengths_codes_or_internal() {
        assert_eq!(reply_result(0, 8), Ok(0));
        assert_eq!(reply_result(8, 8), Ok(8));
        assert_eq!(reply_result(9, 8), Err(HandlerError::Internal));
        for e in HandlerError::ALL {
            assert_eq!(reply_result(reply_code(Err(e)), 8), Err(e));
        }
        for bad in [-8, -100, i32::MIN, i32::MAX] {
            assert_eq!(reply_result(bad, 8), Err(HandlerError::Internal));
        }
        assert_eq!(reply_code(Ok(7)), 7);
        assert_eq!(reply_code(Ok(usize::MAX)), -4);
    }
}
