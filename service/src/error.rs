//! [`ServiceError`]: why a service could not start, run or answer a
//! command.

use std::fmt;

use crate::config::ConfigError;
use crate::run::ArgsError;
use crate::spec::SpecError;

/// Why a service failed. Non-exhaustive: the runner grows variants as it
/// gains I/O.
#[derive(Debug)]
#[non_exhaustive]
pub enum ServiceError {
    /// The declared spec is invalid.
    Spec(SpecError),
    /// The environment is malformed.
    Config(ConfigError),
    /// The command line is malformed.
    Args(ArgsError),
    /// A called kind has no `MW_SERVICE_MAP` entry.
    Unmapped(String),
    /// The named part is not implemented yet (the 0.6 skeleton; each later
    /// phase replaces one of these). Never a panic.
    Unimplemented(&'static str),
    /// Binding, signals or writing output failed.
    Io(std::io::Error),
    /// gRPC failed.
    Grpc(middleware::grpc::GrpcError),
    /// Zenoh failed.
    Zenoh(middleware::zenoh::ZenohError),
    /// Setting up tracing or the OTLP exporter failed.
    Telemetry(String),
}

impl fmt::Display for ServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ServiceError::Spec(e) => write!(f, "invalid service spec: {e}"),
            ServiceError::Config(e) => write!(f, "invalid environment: {e}"),
            ServiceError::Args(e) => write!(f, "invalid arguments: {e}"),
            ServiceError::Unmapped(k) => {
                write!(f, "called kind {k:?} has no MW_SERVICE_MAP entry")
            }
            ServiceError::Unimplemented(what) => write!(f, "not implemented yet: {what}"),
            ServiceError::Io(e) => write!(f, "i/o: {e}"),
            ServiceError::Grpc(e) => write!(f, "grpc: {e}"),
            ServiceError::Zenoh(e) => write!(f, "zenoh: {e}"),
            ServiceError::Telemetry(e) => write!(f, "telemetry: {e}"),
        }
    }
}

impl std::error::Error for ServiceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ServiceError::Spec(e) => Some(e),
            ServiceError::Config(e) => Some(e),
            ServiceError::Args(e) => Some(e),
            ServiceError::Io(e) => Some(e),
            ServiceError::Grpc(e) => Some(e),
            ServiceError::Zenoh(e) => Some(e),
            _ => None,
        }
    }
}

impl From<SpecError> for ServiceError {
    fn from(e: SpecError) -> Self {
        ServiceError::Spec(e)
    }
}

impl From<ConfigError> for ServiceError {
    fn from(e: ConfigError) -> Self {
        ServiceError::Config(e)
    }
}

impl From<ArgsError> for ServiceError {
    fn from(e: ArgsError) -> Self {
        ServiceError::Args(e)
    }
}

impl From<std::io::Error> for ServiceError {
    fn from(e: std::io::Error) -> Self {
        ServiceError::Io(e)
    }
}

impl From<middleware::grpc::GrpcError> for ServiceError {
    fn from(e: middleware::grpc::GrpcError) -> Self {
        ServiceError::Grpc(e)
    }
}

impl From<middleware::zenoh::ZenohError> for ServiceError {
    fn from(e: middleware::zenoh::ZenohError) -> Self {
        ServiceError::Zenoh(e)
    }
}
