//! gRPC backend (feature `grpc`), built on tonic without generated code.
//!
//! # Client
//!
//! [`GrpcTransport`] wraps any tonic [`GrpcService`](tonic::client::GrpcService)
//! (normally a [`tonic::transport::Channel`], or a channel wrapped in an
//! interceptor such as `istio::MeshInterceptor`) and implements:
//!
//! * [`Requester`](crate::Requester) — a key `Service/Method` becomes a unary
//!   call on the HTTP/2 path `/Service/Method`. Request and response bodies
//!   are raw bytes ([`codec::RawCodec`]), so any protobuf codec — for example
//!   `codec::prost::Prost` through [`typed::Client`](crate::typed::Client) —
//!   sits on top and the call is wire compatible with a generated server.
//!   A key with any other segment count is rejected with
//!   [`GrpcError::NotServiceMethod`] before anything is sent.
//! * [`Publisher`](crate::Publisher) — a unary call to
//!   `middleware.v1.Bus/Publish` with the key in the [`bus::KEY_HEADER`]
//!   metadata entry and the payload as the body.
//!
//! * [`Subscribe`](crate::Subscribe) — opens the server-streaming
//!   `middleware.v1.Bus/Subscribe` call for an exact or prefix
//!   [`KeyFilter`](crate::KeyFilter) and returns a [`GrpcSubscriber`], which
//!   implements [`Subscriber`](crate::Subscriber) (including a non-blocking
//!   `try_recv`). The bus filters server side and does not identify
//!   publishers, so samples carry no sender metadata and a client receives
//!   its own publications on keys it subscribes to.
//!
//! [`Publisher::LIMITS`](crate::Publisher::LIMITS) is keys up to
//! [`MAX_KEY_LEN`](crate::key::MAX_KEY_LEN) and payloads up to
//! [`MAX_PAYLOAD`].
//!
//! # Server
//!
//! * [`bus::BusService`] — the `middleware.v1.Bus` service, a tokio broadcast
//!   fan-out. It implements tower `Service` and [`tonic::server::NamedService`]
//!   so it can be added to a stock `tonic::transport::Server` router.
//! * [`responder::GrpcResponder`] — implements [`Responder`](crate::Responder):
//!   its tonic half ([`responder::ResponderService`]) forwards each unary call
//!   over a bounded channel, and [`Responder::serve_one`](crate::Responder::serve_one)
//!   runs the caller's pure [`Handler`](crate::Handler) on it.
//! * [`server::GrpcServer`] — one tower service routing the bus paths to the
//!   bus and every other `/Service/Method` path to the responder, plus
//!   [`server::serve`] to run it on a bound listener.
//!
//! # Status mapping
//!
//! [`status`] maps [`HandlerError`] to
//! [`tonic::Code`] and back; [`GrpcError::handler_error`] applies the reverse
//! mapping on the client.
//!
//! # Istio
//!
//! With feature `istio`, the `istio` module adds mesh authorities, trace header
//! propagation, `grpc-timeout` formatting, retry classification and a
//! channel builder for the sidecar.

pub mod bus;
pub mod client;
pub mod codec;
pub mod responder;
pub mod server;
pub mod status;

#[cfg(feature = "istio")]
pub mod istio;

pub use bus::BusService;
pub use client::{GrpcSubscriber, GrpcTransport};
pub use codec::RawCodec;
pub use responder::{GrpcResponder, ResponderService, responder};
pub use server::GrpcServer;

use crate::transport::HandlerError;

/// tonic's default limit on a decoded message, which both the server (for
/// a publish body) and the client (for a stream item) enforce.
pub const MAX_MESSAGE: usize = 4 * 1024 * 1024;

/// Largest payload a publish carries: [`MAX_MESSAGE`] minus the stream
/// item's length byte and a maximal key.
pub const MAX_PAYLOAD: usize = MAX_MESSAGE - 1 - crate::key::MAX_KEY_LEN;

/// The filter kind a [`GrpcSubscriber`] was opened with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrpcSubscriberKind {
    /// [`KeyFilter::Exact`](crate::KeyFilter::Exact).
    Exact,
    /// [`KeyFilter::Prefix`](crate::KeyFilter::Prefix).
    Prefix,
}

/// Failure of a gRPC transport operation.
#[derive(Debug)]
pub enum GrpcError {
    /// A request key did not have exactly two segments (`Service/Method`).
    NotServiceMethod,
    /// The peer answered with a non-OK gRPC status.
    Status {
        /// The gRPC status code.
        code: tonic::Code,
        /// The status message.
        message: String,
    },
    /// Building or connecting the channel failed.
    Transport(tonic::transport::Error),
    /// The underlying service reported it could not accept a request.
    NotReady(String),
    /// A reply or sample did not fit the caller's buffer.
    BufferTooSmall {
        /// Bytes needed.
        needed: usize,
    },
    /// A key or path could not be encoded as a request header or URI path.
    InvalidHeader,
    /// The server closed a subscription stream.
    StreamEnded,
    /// A subscription stream item was not `[key_len][key][payload]` with a
    /// valid key.
    MalformedSample,
    /// Every sender feeding a [`GrpcResponder`] was dropped.
    Closed,
}

impl GrpcError {
    /// The [`HandlerError`] a remote handler most likely returned, recovered
    /// from the gRPC status via [`status::handler_error`]. `None` for
    /// failures that are not a gRPC status.
    pub fn handler_error(&self) -> Option<HandlerError> {
        match self {
            GrpcError::Status { code, .. } => status::handler_error(*code),
            _ => None,
        }
    }
}

impl From<tonic::Status> for GrpcError {
    fn from(s: tonic::Status) -> Self {
        GrpcError::Status {
            code: s.code(),
            message: s.message().to_owned(),
        }
    }
}

impl From<tonic::transport::Error> for GrpcError {
    fn from(e: tonic::transport::Error) -> Self {
        GrpcError::Transport(e)
    }
}

impl core::fmt::Display for GrpcError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            GrpcError::NotServiceMethod => f.write_str("key is not of the form Service/Method"),
            GrpcError::Status { code, message } => write!(f, "gRPC status {code:?}: {message}"),
            GrpcError::Transport(e) => write!(f, "gRPC transport error: {e}"),
            GrpcError::NotReady(m) => write!(f, "gRPC service not ready: {m}"),
            GrpcError::BufferTooSmall { needed } => {
                write!(f, "buffer too small, {needed} bytes needed")
            }
            GrpcError::InvalidHeader => {
                f.write_str("key cannot be encoded as gRPC metadata or path")
            }
            GrpcError::StreamEnded => f.write_str("subscription stream ended"),
            GrpcError::MalformedSample => f.write_str("malformed subscription stream item"),
            GrpcError::Closed => f.write_str("responder channel closed"),
        }
    }
}

impl std::error::Error for GrpcError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            GrpcError::Transport(e) => Some(e),
            _ => None,
        }
    }
}
