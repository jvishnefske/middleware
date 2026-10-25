//! Client side: [`GrpcTransport`] (requests and publishes) and
//! [`GrpcSubscriber`] (a `Subscribe` stream).
//!
//! The transport is generic over any tonic
//! [`GrpcService`], so the same code runs over a
//! bare [`Channel`] or a channel wrapped in an interceptor (for example
//! `istio::MeshInterceptor`, which adds trace headers). Each call copies the
//! caller's bytes into an owned [`Bytes`] at the boundary and copies the
//! reply back into the caller's buffer, refusing to truncate.

use super::GrpcError;
use super::bus::{KEY_HEADER, PUBLISH_PATH, SUBSCRIBE_PATH};
use super::codec::RawCodec;
use crate::key::Key;
use crate::transport::{Publisher, Requester, Sample, Subscriber, copy_into, sample_into};
use bytes::Bytes;
use http::uri::PathAndQuery;
use tonic::Streaming;
use tonic::body::Body;
use tonic::client::{Grpc, GrpcService};
use tonic::codegen::{Body as HttpBody, StdError};
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;

/// The HTTP/2 path `/Service/Method` for a two-segment key, or
/// [`GrpcError::NotServiceMethod`] for any other key.
pub fn method_path(key: Key<'_>) -> Result<PathAndQuery, GrpcError> {
    let (service, method) = key.service_method().ok_or(GrpcError::NotServiceMethod)?;
    PathAndQuery::try_from(format!("/{service}/{method}")).map_err(|_| GrpcError::InvalidHeader)
}

/// gRPC client implementing [`Requester`] and [`Publisher`].
#[derive(Debug, Clone)]
pub struct GrpcTransport<T = Channel> {
    grpc: Grpc<T>,
}

impl GrpcTransport<Channel> {
    /// Connects to `uri` (for example `http://127.0.0.1:50051`).
    pub async fn connect(uri: impl Into<Bytes>) -> Result<Self, GrpcError> {
        let channel = tonic::transport::Endpoint::from_shared(uri)?
            .connect()
            .await?;
        Ok(Self::new(channel))
    }
}

impl<T> GrpcTransport<T>
where
    T: GrpcService<Body>,
    T::Error: Into<StdError>,
    T::ResponseBody: HttpBody<Data = Bytes> + Send + 'static,
    <T::ResponseBody as HttpBody>::Error: Into<StdError>,
{
    /// Wraps an existing channel or intercepted service.
    pub fn new(inner: T) -> Self {
        Self {
            grpc: Grpc::new(inner),
        }
    }

    async fn ready(&mut self) -> Result<(), GrpcError> {
        self.grpc
            .ready()
            .await
            .map_err(|e| GrpcError::NotReady(e.into().to_string()))
    }

    async fn unary(
        &mut self,
        path: PathAndQuery,
        request: tonic::Request<Bytes>,
    ) -> Result<Bytes, GrpcError> {
        self.ready().await?;
        let resp = self.grpc.unary(request, path, RawCodec).await?;
        Ok(resp.into_inner())
    }

    /// Opens a `middleware.v1.Bus/Subscribe` stream for `key`.
    ///
    /// Returns once the server has accepted the subscription, so samples
    /// published afterwards are delivered.
    pub async fn subscribe(&mut self, key: Key<'_>) -> Result<GrpcSubscriber, GrpcError> {
        self.ready().await?;
        let request = tonic::Request::new(Bytes::copy_from_slice(key.as_bytes()));
        let resp = self
            .grpc
            .server_streaming(request, PathAndQuery::from_static(SUBSCRIBE_PATH), RawCodec)
            .await?;
        Ok(GrpcSubscriber {
            key: key.as_str().to_owned(),
            stream: resp.into_inner(),
        })
    }
}

impl<T> Requester for GrpcTransport<T>
where
    T: GrpcService<Body>,
    T::Error: Into<StdError>,
    T::ResponseBody: HttpBody<Data = Bytes> + Send + 'static,
    <T::ResponseBody as HttpBody>::Error: Into<StdError>,
{
    type Error = GrpcError;

    async fn request<'b>(
        &mut self,
        key: Key<'_>,
        request: &[u8],
        response: &'b mut [u8],
    ) -> Result<&'b [u8], GrpcError> {
        let path = method_path(key)?;
        let reply = self
            .unary(path, tonic::Request::new(Bytes::copy_from_slice(request)))
            .await?;
        let out = copy_into(response, &reply).ok_or(GrpcError::BufferTooSmall {
            needed: reply.len(),
        })?;
        Ok(out)
    }
}

impl<T> Publisher for GrpcTransport<T>
where
    T: GrpcService<Body>,
    T::Error: Into<StdError>,
    T::ResponseBody: HttpBody<Data = Bytes> + Send + 'static,
    <T::ResponseBody as HttpBody>::Error: Into<StdError>,
{
    type Error = GrpcError;

    async fn publish(&mut self, key: Key<'_>, payload: &[u8]) -> Result<(), GrpcError> {
        let mut request = tonic::Request::new(Bytes::copy_from_slice(payload));
        let value = MetadataValue::try_from(key.as_str()).map_err(|_| GrpcError::InvalidHeader)?;
        request.metadata_mut().insert(KEY_HEADER, value);
        self.unary(PathAndQuery::from_static(PUBLISH_PATH), request)
            .await?;
        Ok(())
    }
}

/// One `Subscribe` stream; implements [`Subscriber`].
#[derive(Debug)]
pub struct GrpcSubscriber {
    key: String,
    stream: Streaming<Bytes>,
}

impl GrpcSubscriber {
    /// The key this subscription was opened for.
    pub fn key(&self) -> &str {
        &self.key
    }
}

impl Subscriber for GrpcSubscriber {
    type Error = GrpcError;

    async fn recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Sample<'b>, GrpcError> {
        let payload = self.stream.message().await?.ok_or(GrpcError::StreamEnded)?;
        let needed = self.key.len() + payload.len();
        sample_into(buf, &self.key, &payload).ok_or(GrpcError::BufferTooSmall { needed })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_path_needs_two_segments() {
        let p = method_path(Key::from_static("pkg.Svc/Call")).unwrap();
        assert_eq!(p.as_str(), "/pkg.Svc/Call");
        assert!(matches!(
            method_path(Key::from_static("a")),
            Err(GrpcError::NotServiceMethod)
        ));
        assert!(matches!(
            method_path(Key::from_static("a/b/c")),
            Err(GrpcError::NotServiceMethod)
        ));
    }
}
