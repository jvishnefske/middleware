//! Frame encoders producing wire bytes (COBS-stuffed, `0x00`-terminated).
//!
//! Thin owned-buffer wrappers over [`middleware::serial::frame::encode`]:
//! the layout, CRC-16 and COBS stuffing all come from the middleware crate.

use middleware::serial::frame::{self, Frame, FrameError, MAX_RAW_FRAME, MAX_WIRE_FRAME};
use middleware::{HandlerError, Key};

/// Why a frame could not be encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeError {
    /// The key is not a valid middleware key.
    InvalidKey,
    /// The frame exceeds [`MAX_RAW_FRAME`].
    TooLarge,
}

impl EncodeError {
    /// Short human-readable description.
    pub const fn describe(self) -> &'static str {
        match self {
            EncodeError::InvalidKey => "invalid key",
            EncodeError::TooLarge => "frame too large",
        }
    }
}

fn wire(f: &Frame<'_>) -> Result<Vec<u8>, EncodeError> {
    let mut scratch = [0u8; MAX_RAW_FRAME];
    let mut out = vec![0u8; MAX_WIRE_FRAME];
    let n = frame::encode(f, &mut scratch, &mut out).map_err(|e| match e {
        FrameError::InvalidKey => EncodeError::InvalidKey,
        _ => EncodeError::TooLarge,
    })?;
    out.truncate(n);
    Ok(out)
}

fn key(k: &str) -> Result<Key<'_>, EncodeError> {
    Key::new(k).map_err(|_| EncodeError::InvalidKey)
}

/// A request frame for `key` with correlation `id`.
pub fn request(id: u8, k: &str, payload: &[u8]) -> Result<Vec<u8>, EncodeError> {
    wire(&Frame::Request {
        id,
        key: key(k)?,
        payload,
    })
}

/// A publish frame on `key`.
pub fn publish(k: &str, payload: &[u8]) -> Result<Vec<u8>, EncodeError> {
    wire(&Frame::Publish {
        key: key(k)?,
        payload,
    })
}

/// A response frame (used by device fakes in tests and by the simulator UI).
pub fn response(id: u8, result: Result<&[u8], HandlerError>) -> Result<Vec<u8>, EncodeError> {
    wire(&Frame::Response { id, result })
}

/// The [`HandlerError`] for a one-byte wire status, or `None` for 0 (success).
pub const fn status_from_code(code: u8) -> Option<HandlerError> {
    if code == 0 {
        None
    } else {
        Some(HandlerError::from_code(code))
    }
}

/// Name of a handler error as shown in the UI and the JS objects.
pub const fn status_name(status: Option<HandlerError>) -> &'static str {
    match status {
        None => "ok",
        Some(HandlerError::NotFound) => "NotFound",
        Some(HandlerError::InvalidRequest) => "InvalidRequest",
        Some(HandlerError::BufferTooSmall) => "BufferTooSmall",
        Some(HandlerError::Internal) => "Internal",
        Some(HandlerError::Denied) => "Denied",
        Some(HandlerError::Unavailable) => "Unavailable",
        Some(HandlerError::DeadlineExceeded) => "DeadlineExceeded",
        // A status added to the middleware after this build.
        Some(_) => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_frames_decode_with_the_middleware_decoder() {
        let mut w = request(3, "sys/info", &[]).unwrap();
        assert_eq!(w.last(), Some(&0));
        let n = w.len() - 1;
        assert_eq!(
            frame::decode_wire(&mut w[..n]).unwrap(),
            Frame::Request {
                id: 3,
                key: Key::from_static("sys/info"),
                payload: &[]
            }
        );
        let mut w = response(3, Err(HandlerError::BufferTooSmall)).unwrap();
        let n = w.len() - 1;
        assert_eq!(
            frame::decode_wire(&mut w[..n]).unwrap(),
            Frame::Response {
                id: 3,
                result: Err(HandlerError::BufferTooSmall)
            }
        );
    }

    #[test]
    fn bad_keys_and_oversized_payloads_are_errors() {
        assert_eq!(publish("bad//key", b""), Err(EncodeError::InvalidKey));
        assert_eq!(
            publish("a", &[0; MAX_RAW_FRAME]),
            Err(EncodeError::TooLarge)
        );
        assert_eq!(EncodeError::TooLarge.describe(), "frame too large");
    }

    #[test]
    fn status_codes_map_to_names() {
        assert_eq!(status_name(status_from_code(0)), "ok");
        assert_eq!(status_name(status_from_code(1)), "NotFound");
        assert_eq!(status_name(status_from_code(2)), "InvalidRequest");
        assert_eq!(status_name(status_from_code(3)), "BufferTooSmall");
        assert_eq!(status_name(status_from_code(5)), "Denied");
        assert_eq!(status_name(status_from_code(99)), "Internal");
    }
}
