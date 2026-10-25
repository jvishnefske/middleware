//! Typed messaging: a transport paired with a [`Codec`].
//!
//! [`Client`] owns a transport, a codec and two fixed `N`-byte buffers (one
//! for outgoing encodings, one for incoming bytes), so typed calls allocate
//! nothing. Its methods exist only when the transport implements the matching
//! trait: `publish` needs [`Publisher`], `request` needs [`Requester`], and
//! `recv` needs [`Subscriber`].

use crate::codec::Codec;
use crate::key::Key;
use crate::transport::{Publisher, Requester, Subscriber};

/// Failure of a typed call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypedError<T, C> {
    /// The transport failed.
    Transport(T),
    /// The outgoing message could not be encoded.
    Encode(C),
    /// The incoming bytes could not be decoded.
    Decode(C),
}

/// A typed decoded sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedSample<'a, M> {
    /// The key the message arrived on.
    pub key: Key<'a>,
    /// The decoded message.
    pub message: M,
}

/// Transport + codec + `N`-byte buffers.
pub struct Client<T, C, const N: usize> {
    transport: T,
    codec: C,
    tx: [u8; N],
    rx: [u8; N],
}

impl<T, C, const N: usize> Client<T, C, N> {
    /// Wraps `transport`, encoding with `codec`.
    pub const fn new(transport: T, codec: C) -> Self {
        Self { transport, codec, tx: [0; N], rx: [0; N] }
    }

    /// Borrows the transport, e.g. to reach backend-specific methods.
    pub fn transport(&mut self) -> &mut T {
        &mut self.transport
    }

    /// Returns the transport and codec.
    pub fn into_parts(self) -> (T, C) {
        (self.transport, self.codec)
    }
}

impl<T: Publisher, C, const N: usize> Client<T, C, N> {
    /// Encodes `message` and publishes it on `key`.
    pub async fn publish<M>(
        &mut self,
        key: Key<'_>,
        message: &M,
    ) -> Result<(), TypedError<T::Error, C::Error>>
    where
        C: Codec<M>,
    {
        let n = self.codec.encode(message, &mut self.tx).map_err(TypedError::Encode)?;
        self.transport
            .publish(key, &self.tx[..n])
            .await
            .map_err(TypedError::Transport)
    }
}

impl<T: Requester, C, const N: usize> Client<T, C, N> {
    /// Encodes `request`, sends it to `key` and decodes the response.
    pub async fn request<Req, Resp>(
        &mut self,
        key: Key<'_>,
        request: &Req,
    ) -> Result<Resp, TypedError<T::Error, <C as Codec<Req>>::Error>>
    where
        C: Codec<Req> + Codec<Resp, Error = <C as Codec<Req>>::Error>,
    {
        let n = self.codec.encode(request, &mut self.tx).map_err(TypedError::Encode)?;
        let resp = self
            .transport
            .request(key, &self.tx[..n], &mut self.rx)
            .await
            .map_err(TypedError::Transport)?;
        self.codec.decode(resp).map_err(TypedError::Decode)
    }
}

impl<T: Subscriber, C, const N: usize> Client<T, C, N> {
    /// Receives and decodes the next sample.
    pub async fn recv<M>(
        &mut self,
    ) -> Result<TypedSample<'_, M>, TypedError<T::Error, C::Error>>
    where
        C: Codec<M>,
    {
        let sample = self.transport.recv(&mut self.rx).await.map_err(TypedError::Transport)?;
        let message = self.codec.decode(sample.payload).map_err(TypedError::Decode)?;
        Ok(TypedSample { key: sample.key, message })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::Raw;
    use crate::transport::{Sample, sample_into};
    use futures::executor::block_on;

    /// Loopback transport: requests are echoed reversed, publishes are
    /// stored and handed back to `recv`.
    #[derive(Default)]
    struct Loopback {
        last: Option<([u8; 8], usize)>,
    }

    impl Publisher for Loopback {
        type Error = ();
        async fn publish(&mut self, _key: Key<'_>, payload: &[u8]) -> Result<(), ()> {
            let mut b = [0u8; 8];
            b.get_mut(..payload.len()).ok_or(())?.copy_from_slice(payload);
            self.last = Some((b, payload.len()));
            Ok(())
        }
    }

    impl Subscriber for Loopback {
        type Error = ();
        async fn recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Sample<'b>, ()> {
            let (b, n) = self.last.take().ok_or(())?;
            sample_into(buf, "loop/back", &b[..n]).ok_or(())
        }
    }

    impl Requester for Loopback {
        type Error = ();
        async fn request<'b>(
            &mut self,
            _key: Key<'_>,
            request: &[u8],
            response: &'b mut [u8],
        ) -> Result<&'b [u8], ()> {
            let out = response.get_mut(..request.len()).ok_or(())?;
            out.copy_from_slice(request);
            out.reverse();
            Ok(out)
        }
    }

    const K: Key<'static> = Key::from_static("loop/back");

    #[test]
    fn typed_request_encodes_and_decodes() {
        let mut c: Client<_, _, 16> = Client::new(Loopback::default(), Raw);
        let resp: [u8; 3] = block_on(c.request(K, &[1u8, 2, 3])).unwrap();
        assert_eq!(resp, [3, 2, 1]);
    }

    #[test]
    fn typed_publish_then_recv() {
        let mut c: Client<_, _, 32> = Client::new(Loopback::default(), Raw);
        block_on(c.publish(K, &[9u8, 8])).unwrap();
        let s = block_on(c.recv::<[u8; 2]>()).unwrap();
        assert_eq!(s.key, K);
        assert_eq!(s.message, [9, 8]);
    }

    #[test]
    fn decode_failure_is_reported() {
        let mut c: Client<_, _, 16> = Client::new(Loopback::default(), Raw);
        let r: Result<[u8; 4], _> = block_on(c.request(K, &[1u8, 2, 3]));
        assert!(matches!(r, Err(TypedError::Decode(_))));
    }
}
