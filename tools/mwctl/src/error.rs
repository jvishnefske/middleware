//! Errors, with human-readable explanations of device status codes.
//!
//! The device answers a refused request with a [`HandlerError`] status.
//! [`explain`] turns `(key, status)` into what it means for that request,
//! so `sys/payload/start` answering `NotFound` reads as "this firmware has
//! no WASM runtime" rather than a bare code.

use crate::plan::PlanError;
use middleware::loader::keys;
use middleware::serial::frame::FrameError;
use middleware::transport::HandlerError;
use std::fmt;

/// Anything an `mwctl` operation can fail with.
#[derive(Debug)]
pub enum Error {
    /// The serial port failed.
    Io(std::io::Error),
    /// No response within the timeout, after every attempt.
    Timeout {
        /// Request key.
        key: String,
        /// Attempts made.
        attempts: u32,
    },
    /// The device answered with a non-zero status.
    Device {
        /// Request key.
        key: String,
        /// Status returned.
        status: HandlerError,
    },
    /// A response body could not be decoded.
    Malformed {
        /// Request key.
        key: String,
    },
    /// A request frame could not be encoded.
    Frame(FrameError),
    /// The upload plan could not be built.
    Plan(PlanError),
    /// One step of an upload failed.
    Upload {
        /// Zero-based step index.
        step: usize,
        /// Number of steps (including a trailing start, if requested).
        steps: usize,
        /// Key of the failed step.
        key: String,
        /// What went wrong.
        cause: Box<Error>,
    },
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<PlanError> for Error {
    fn from(e: PlanError) -> Self {
        Error::Plan(e)
    }
}

impl Error {
    /// The device status behind this error, looking through upload context.
    pub fn status(&self) -> Option<HandlerError> {
        match self {
            Error::Device { status, .. } => Some(*status),
            Error::Upload { cause, .. } => cause.status(),
            _ => None,
        }
    }
}

/// The status name used on the wire documentation and in the web UI.
pub const fn status_name(status: HandlerError) -> &'static str {
    match status {
        HandlerError::NotFound => "NotFound",
        HandlerError::InvalidRequest => "InvalidRequest",
        HandlerError::BufferTooSmall => "BufferTooSmall",
        HandlerError::Internal => "Internal",
    }
}

/// What `status` means as the answer to a request on `key`.
pub fn explain(key: &str, status: HandlerError) -> &'static str {
    use HandlerError::*;
    let is = |k: middleware::Key<'static>| key == k.as_str();
    match status {
        NotFound if is(keys::START) => {
            "this firmware has no WASM runtime (a `+no-wasm` build such as ESP32-C3 or ESP8266): \
             payloads are stored but cannot be started"
        }
        InvalidRequest if is(keys::START) => {
            "no payload is installed (or a transfer is in progress)"
        }
        InvalidRequest if is(keys::COMMIT) => {
            "the device rejected the image: CRC-32 mismatch or incomplete transfer \
             (the transfer was abandoned; any previously installed payload is kept)"
        }
        InvalidRequest if is(keys::CHUNK) => {
            "chunk out of order or no transfer in progress (the transfer was abandoned)"
        }
        BufferTooSmall if is(keys::BEGIN) || is(keys::CHUNK) => {
            "the image is larger than the device's payload capacity"
        }
        NotFound => "the device does not handle this key",
        InvalidRequest => "the device rejected the request body",
        BufferTooSmall => "the response did not fit the device's buffer",
        Internal => "the device failed internally (for example a flash error)",
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "serial port: {e}"),
            Error::Timeout { key, attempts } => write!(
                f,
                "{key}: no response after {attempts} attempt(s) \
                 (wrong port or baud rate, device not running the base firmware, or still booting?)"
            ),
            Error::Device { key, status } => write!(
                f,
                "{key}: device returned {}: {}",
                status_name(*status),
                explain(key, *status)
            ),
            Error::Malformed { key } => write!(f, "{key}: malformed response body"),
            Error::Frame(e) => write!(f, "could not encode request frame: {e:?}"),
            Error::Plan(e) => write!(f, "{e}"),
            Error::Upload {
                step,
                steps,
                key,
                cause,
            } => write!(
                f,
                "upload failed at step {}/{steps} ({key}): {cause}",
                step + 1
            ),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_not_found_explains_missing_runtime() {
        let e = Error::Device {
            key: keys::START.as_str().into(),
            status: HandlerError::NotFound,
        };
        let s = e.to_string();
        assert!(s.contains("NotFound"), "{s}");
        assert!(s.contains("no WASM runtime"), "{s}");
    }

    #[test]
    fn upload_context_wraps_cause_and_exposes_status() {
        let e = Error::Upload {
            step: 4,
            steps: 5,
            key: keys::COMMIT.as_str().into(),
            cause: Box::new(Error::Device {
                key: keys::COMMIT.as_str().into(),
                status: HandlerError::InvalidRequest,
            }),
        };
        assert_eq!(e.status(), Some(HandlerError::InvalidRequest));
        let s = e.to_string();
        assert!(
            s.starts_with("upload failed at step 5/5 (sys/loader/commit)"),
            "{s}"
        );
        assert!(s.contains("CRC-32 mismatch"), "{s}");
    }

    #[test]
    fn generic_explanations_cover_every_status() {
        for s in [
            HandlerError::NotFound,
            HandlerError::InvalidRequest,
            HandlerError::BufferTooSmall,
            HandlerError::Internal,
        ] {
            assert!(!explain("app/x", s).is_empty());
        }
        assert!(explain("sys/loader/begin", HandlerError::BufferTooSmall).contains("capacity"));
    }
}
