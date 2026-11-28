//! One step of the link loop.

use middleware::serial::frame::Frame;
use middleware::transport::{Handler, HandlerError, Request};

/// Largest response the firmware builds; `sys/info` (85 bytes),
/// `sys/board` (96) and `sys/manifest` fit.
pub const RESPONSE_LEN: usize = 256;

/// The response to `frame` if it is a request, built in `out` by `handler`;
/// `None` for responses and publications (nothing to answer).
pub fn respond<'o, H: Handler>(
    handler: &mut H,
    frame: Frame<'_>,
    out: &'o mut [u8],
) -> Option<Frame<'o>> {
    let Frame::Request { id, key, payload } = frame else {
        return None;
    };
    let handled = handler.handle(&Request::new(key, payload), out);
    let out: &'o [u8] = out;
    let result = handled.and_then(|n| out.get(..n).ok_or(HandlerError::BufferTooSmall));
    Some(Frame::Response { id, result })
}
