//! Pure mapping between [`HandlerError`] and [`tonic::Code`].
//!
//! | `HandlerError`   | gRPC code (server sends) | gRPC codes mapped back            |
//! |------------------|--------------------------|-----------------------------------|
//! | `NotFound`       | `UNIMPLEMENTED`          | `UNIMPLEMENTED`, `NOT_FOUND`      |
//! | `InvalidRequest` | `INVALID_ARGUMENT`       | `INVALID_ARGUMENT`, `FAILED_PRECONDITION`, `OUT_OF_RANGE` |
//! | `BufferTooSmall` | `RESOURCE_EXHAUSTED`     | `RESOURCE_EXHAUSTED`              |
//! | `Internal`       | `INTERNAL`               | every other non-OK code           |
//! | `Denied`         | `PERMISSION_DENIED`      | `PERMISSION_DENIED`, `UNAUTHENTICATED` |
//! | `Unavailable`    | `UNAVAILABLE`            | `UNAVAILABLE`                     |
//! | `DeadlineExceeded` | `DEADLINE_EXCEEDED`    | `DEADLINE_EXCEEDED`               |
//!
//! `NotFound` means "no handler for this key", which gRPC spells
//! `UNIMPLEMENTED` (unknown method). `OK` maps back to `None`. The mapping
//! round-trips: `handler_error(code(e)) == Some(e)` for every variant.

use crate::transport::HandlerError;
use tonic::Code;

/// The gRPC status code a server sends for `e`.
pub const fn code(e: HandlerError) -> Code {
    match e {
        HandlerError::NotFound => Code::Unimplemented,
        HandlerError::InvalidRequest => Code::InvalidArgument,
        HandlerError::BufferTooSmall => Code::ResourceExhausted,
        HandlerError::Internal => Code::Internal,
        HandlerError::Denied => Code::PermissionDenied,
        HandlerError::Unavailable => Code::Unavailable,
        HandlerError::DeadlineExceeded => Code::DeadlineExceeded,
    }
}

/// The [`HandlerError`] a client reports for gRPC status `c`; `None` for
/// `OK`.
pub const fn handler_error(c: Code) -> Option<HandlerError> {
    match c {
        Code::Ok => None,
        Code::Unimplemented | Code::NotFound => Some(HandlerError::NotFound),
        Code::InvalidArgument | Code::FailedPrecondition | Code::OutOfRange => {
            Some(HandlerError::InvalidRequest)
        }
        Code::ResourceExhausted => Some(HandlerError::BufferTooSmall),
        Code::PermissionDenied | Code::Unauthenticated => Some(HandlerError::Denied),
        Code::Unavailable => Some(HandlerError::Unavailable),
        Code::DeadlineExceeded => Some(HandlerError::DeadlineExceeded),
        _ => Some(HandlerError::Internal),
    }
}

/// A [`tonic::Status`] for `e`, with a short fixed message.
pub fn to_status(e: HandlerError) -> tonic::Status {
    let msg = match e {
        HandlerError::NotFound => "no handler for key",
        HandlerError::InvalidRequest => "invalid request",
        HandlerError::BufferTooSmall => "response exceeds server buffer",
        HandlerError::Internal => "handler failed",
        HandlerError::Denied => "request refused by policy",
        HandlerError::Unavailable => "handler unavailable",
        HandlerError::DeadlineExceeded => "deadline exceeded",
    };
    tonic::Status::new(code(e), msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [HandlerError; 7] = HandlerError::ALL;

    #[test]
    fn handler_errors_round_trip_through_codes() {
        for e in ALL {
            assert_eq!(handler_error(code(e)), Some(e));
            assert_eq!(to_status(e).code(), code(e));
        }
    }

    #[test]
    fn codes_map_back_to_handler_errors() {
        assert_eq!(handler_error(Code::Ok), None);
        assert_eq!(handler_error(Code::NotFound), Some(HandlerError::NotFound));
        assert_eq!(
            handler_error(Code::OutOfRange),
            Some(HandlerError::InvalidRequest)
        );
        assert_eq!(
            handler_error(Code::FailedPrecondition),
            Some(HandlerError::InvalidRequest)
        );
        assert_eq!(
            handler_error(Code::Unavailable),
            Some(HandlerError::Unavailable)
        );
        assert_eq!(
            handler_error(Code::DeadlineExceeded),
            Some(HandlerError::DeadlineExceeded)
        );
        assert_eq!(handler_error(Code::Aborted), Some(HandlerError::Internal));
        assert_eq!(handler_error(Code::Unknown), Some(HandlerError::Internal));
        assert_eq!(
            handler_error(Code::Unauthenticated),
            Some(HandlerError::Denied)
        );
    }
}
