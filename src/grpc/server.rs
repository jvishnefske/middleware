//! [`GrpcServer`]: one tower service that routes the `middleware.v1.Bus`
//! paths to a [`BusService`] and every other path to a
//! [`ResponderService`], and [`serve`] to run it on a bound listener.
//!
//! A stock tonic `Router` matches services by a compile-time
//! [`NamedService::NAME`](tonic::server::NamedService::NAME), which cannot
//! express "any `/Service/Method`". `GrpcServer` therefore routes by path
//! itself and is passed straight to
//! [`tonic::transport::Server::serve_with_incoming_shutdown`]. A missing
//! half answers `UNIMPLEMENTED`.

use super::GrpcError;
use super::bus::{BusService, SERVICE_NAME};
use super::responder::ResponderService;
use core::convert::Infallible;
use core::future::Future;
use core::task::{Context, Poll};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::Status;
use tonic::body::Body;
use tonic::codegen::{BoxFuture, Service};

/// Path router over an optional bus and an optional responder. Build with
/// [`GrpcServer::new`], [`GrpcServer::with_bus`] and
/// [`GrpcServer::with_responder`].
#[derive(Debug, Clone, Default)]
pub struct GrpcServer {
    bus: Option<BusService>,
    responder: Option<ResponderService>,
}

/// Whether `path` addresses the bus service.
fn is_bus_path(path: &str) -> bool {
    path.strip_prefix('/')
        .and_then(|p| p.strip_prefix(SERVICE_NAME))
        .is_some_and(|rest| rest.starts_with('/'))
}

impl GrpcServer {
    /// A server with nothing mounted.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns this server with `bus` mounted on `middleware.v1.Bus`.
    pub fn with_bus(self, bus: BusService) -> Self {
        Self {
            bus: Some(bus),
            ..self
        }
    }

    /// Returns this server with `responder` answering all other paths.
    pub fn with_responder(self, responder: ResponderService) -> Self {
        Self {
            responder: Some(responder),
            ..self
        }
    }
}

impl Service<http::Request<Body>> for GrpcServer {
    type Response = http::Response<Body>;
    type Error = Infallible;
    type Future = BoxFuture<Self::Response, Infallible>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<Body>) -> Self::Future {
        let bus_path = is_bus_path(req.uri().path());
        let bus = self.bus.clone();
        let responder = self.responder.clone();
        Box::pin(async move {
            Ok(match (bus_path, bus, responder) {
                (true, Some(bus), _) => bus.handle(req).await,
                (false, _, Some(r)) => r.handle(req).await,
                _ => Status::unimplemented("no service mounted for path").into_http(),
            })
        })
    }
}

/// Serves `server` on `listener` (plaintext HTTP/2) until `shutdown`
/// completes.
pub async fn serve<F>(
    server: GrpcServer,
    listener: TcpListener,
    shutdown: F,
) -> Result<(), GrpcError>
where
    F: Future<Output = ()>,
{
    tonic::transport::Server::builder()
        .serve_with_incoming_shutdown(server, TcpListenerStream::new(listener), shutdown)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bus_paths_are_recognised() {
        assert!(is_bus_path("/middleware.v1.Bus/Publish"));
        assert!(is_bus_path("/middleware.v1.Bus/Other"));
        assert!(!is_bus_path("/middleware.v1.BusX/Publish"));
        assert!(!is_bus_path("/pkg.Svc/Call"));
    }
}
