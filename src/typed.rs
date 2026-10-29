//! Typed messaging: a transport paired with a [`Codec`].
//!
//! [`Client`] owns a transport, a codec and two fixed `N`-byte buffers (one
//! for outgoing encodings, one for incoming bytes), so typed calls allocate
//! nothing. Its methods exist only when the transport implements the matching
//! trait: `publish` needs [`Publisher`], `request` / `request_with` need
//! [`Requester`], and `recv` / `try_recv` need [`Subscriber`]. Cancel safety
//! is the transport's (see [`crate::transport`]): the client keeps no state
//! of its own between the transport call and decoding.
//!
//! On the serving side, [`TypedHandler`] turns a function of a decoded
//! request and its [`RequestMeta`] into a byte-level [`Handler`].
//!
//! For discovery, [`TypedBrowser`] wraps a [`Browse`] and decodes a typed
//! description of each service from its TXT record: the codec's bytes are
//! carried as the value of one TXT entry, [`TXT_TYPED_KEY`] (`m=<bytes>`,
//! at most 253 bytes, written by [`encode_txt`]); other entries are left
//! alone.

use core::marker::PhantomData;

use crate::codec::Codec;
use crate::discovery::record::TxtWriter;
use crate::discovery::{Browse, Event, Service, ServiceKind};
use crate::dns::wire::Txt;
use crate::key::{Key, KeyFilter};
use crate::transport::{
    Handler, HandlerError, Publisher, Request, RequestMeta, RequestOptions, Requester, Sample,
    SampleMeta, Subscriber,
};

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
    /// Sender and sequence, as reported by the transport.
    pub meta: SampleMeta,
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
        Self {
            transport,
            codec,
            tx: [0; N],
            rx: [0; N],
        }
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
        let n = self
            .codec
            .encode(message, &mut self.tx)
            .map_err(TypedError::Encode)?;
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
        self.request_with(key, request, &RequestOptions::DEFAULT)
            .await
    }

    /// As [`Client::request`], with a timeout and target
    /// ([`Requester::request_with`]).
    pub async fn request_with<Req, Resp>(
        &mut self,
        key: Key<'_>,
        request: &Req,
        options: &RequestOptions,
    ) -> Result<Resp, TypedError<T::Error, <C as Codec<Req>>::Error>>
    where
        C: Codec<Req> + Codec<Resp, Error = <C as Codec<Req>>::Error>,
    {
        let n = self
            .codec
            .encode(request, &mut self.tx)
            .map_err(TypedError::Encode)?;
        let resp = self
            .transport
            .request_with(key, &self.tx[..n], &mut self.rx, options)
            .await
            .map_err(TypedError::Transport)?;
        self.codec.decode(resp).map_err(TypedError::Decode)
    }
}

impl<T: Subscriber, C, const N: usize> Client<T, C, N> {
    /// Receives and decodes the next sample.
    pub async fn recv<M>(&mut self) -> Result<TypedSample<'_, M>, TypedError<T::Error, C::Error>>
    where
        C: Codec<M>,
    {
        let sample = self
            .transport
            .recv(&mut self.rx)
            .await
            .map_err(TypedError::Transport)?;
        decode_sample(&mut self.codec, sample)
    }

    /// Returns and decodes the next sample if the transport already has one,
    /// without waiting ([`Subscriber::try_recv`]).
    #[allow(clippy::type_complexity)]
    pub fn try_recv<M>(
        &mut self,
    ) -> Result<Option<TypedSample<'_, M>>, TypedError<T::Error, C::Error>>
    where
        C: Codec<M>,
    {
        let sample = self
            .transport
            .try_recv(&mut self.rx)
            .map_err(TypedError::Transport)?;
        sample
            .map(|s| decode_sample(&mut self.codec, s))
            .transpose()
    }
}

/// A [`Handler`] that decodes the request with a codec, calls `f` with it
/// and the request's [`RequestMeta`], and encodes the response.
///
/// A request that does not decode is [`HandlerError::InvalidRequest`]; a
/// response that does not encode into the response buffer is
/// [`HandlerError::BufferTooSmall`]; errors from `f` pass through.
pub struct TypedHandler<Req, Resp, C, F> {
    codec: C,
    f: F,
    _types: PhantomData<fn(&Req) -> Resp>,
}

impl<Req, Resp, C, F> TypedHandler<Req, Resp, C, F>
where
    C: Codec<Req> + Codec<Resp>,
    F: FnMut(&Req, &RequestMeta<'_>) -> Result<Resp, HandlerError>,
{
    /// Wraps `f`, decoding and encoding with `codec`.
    pub const fn new(codec: C, f: F) -> Self {
        TypedHandler {
            codec,
            f,
            _types: PhantomData,
        }
    }
}

impl<Req, Resp, C, F> Handler for TypedHandler<Req, Resp, C, F>
where
    C: Codec<Req> + Codec<Resp>,
    F: FnMut(&Req, &RequestMeta<'_>) -> Result<Resp, HandlerError>,
{
    fn handle(&mut self, req: &Request<'_>, response: &mut [u8]) -> Result<usize, HandlerError> {
        let request: Req = self
            .codec
            .decode(req.body)
            .map_err(|_| HandlerError::InvalidRequest)?;
        let resp = (self.f)(&request, &req.meta)?;
        self.codec
            .encode(&resp, response)
            .map_err(|_| HandlerError::BufferTooSmall)
    }
}

/// The TXT key whose value carries a service's typed description.
pub const TXT_TYPED_KEY: &[u8] = b"m";

/// Largest encoded description [`encode_txt`] fits in one TXT string
/// (255 minus `m=`).
pub const MAX_TXT_TYPED_LEN: usize = 253;

/// Failure of [`encode_txt`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxtError<C> {
    /// The codec failed.
    Encode(C),
    /// The encoding is over [`MAX_TXT_TYPED_LEN`] bytes or `out` is too
    /// small.
    TooLarge,
}

/// Writes `m=<message encoded with codec>` as TXT RDATA into `out`,
/// returning the RDATA for [`Service::txt`].
pub fn encode_txt<'o, M, C: Codec<M>>(
    codec: &C,
    message: &M,
    out: &'o mut [u8],
) -> Result<&'o [u8], TxtError<C::Error>> {
    let mut value = [0u8; MAX_TXT_TYPED_LEN];
    let n = codec
        .encode(message, &mut value)
        .map_err(TxtError::Encode)?;
    let txt = TxtWriter::new(out)
        .entry(TXT_TYPED_KEY, &value[..n])
        .map_err(|_| TxtError::TooLarge)?
        .finish();
    Ok(txt.as_bytes())
}

/// A typed discovery event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypedEvent<'a, M> {
    /// A service is up; `message` is its decoded description, `None` when
    /// its TXT has no [`TXT_TYPED_KEY`] entry.
    Up {
        /// The service as announced.
        service: Service<'a>,
        /// The decoded description.
        message: Option<M>,
    },
    /// A service withdrew.
    Down {
        /// Its kind.
        kind: ServiceKind<'a>,
        /// Its instance name.
        instance: &'a str,
    },
}

/// A [`Browse`] that decodes each service's description with a codec; see
/// the module docs. `N` is the event buffer size.
pub struct TypedBrowser<B, C, const N: usize> {
    browser: B,
    codec: C,
    buf: [u8; N],
}

impl<B: Browse, C, const N: usize> TypedBrowser<B, C, N> {
    /// Wraps `browser`, decoding with `codec`.
    pub const fn new(browser: B, codec: C) -> Self {
        TypedBrowser {
            browser,
            codec,
            buf: [0; N],
        }
    }

    /// Borrows the browser.
    pub fn browser(&mut self) -> &mut B {
        &mut self.browser
    }

    /// [`Browse::browse`].
    pub async fn browse(&mut self, filter: KeyFilter<'_>) -> Result<(), B::Error> {
        self.browser.browse(filter).await
    }

    /// The next event, decoded ([`Browse::next`]). A description that is
    /// present but does not decode is `TypedError::Decode(Some(e))`; a
    /// malformed TXT record is `TypedError::Decode(None)`. The event is
    /// consumed either way.
    #[allow(clippy::type_complexity)]
    pub async fn next<M>(
        &mut self,
    ) -> Result<TypedEvent<'_, M>, TypedError<B::Error, Option<C::Error>>>
    where
        C: Codec<M>,
    {
        let event = self
            .browser
            .next(&mut self.buf)
            .await
            .map_err(TypedError::Transport)?;
        decode_event(&self.codec, event)
    }

    /// The next event if one has arrived ([`Browse::try_next`]), decoded.
    #[allow(clippy::type_complexity)]
    pub fn try_next<M>(
        &mut self,
    ) -> Result<Option<TypedEvent<'_, M>>, TypedError<B::Error, Option<C::Error>>>
    where
        C: Codec<M>,
    {
        let event = self
            .browser
            .try_next(&mut self.buf)
            .map_err(TypedError::Transport)?;
        event.map(|e| decode_event(&self.codec, e)).transpose()
    }
}

/// Decodes the description of an event's service. The decode error is
/// `None` for a malformed TXT record.
fn decode_event<'a, M, C: Codec<M>, E>(
    codec: &C,
    event: Event<'a>,
) -> Result<TypedEvent<'a, M>, TypedError<E, Option<C::Error>>> {
    match event {
        Event::Down { kind, instance } => Ok(TypedEvent::Down { kind, instance }),
        Event::Up(service) => {
            let txt = Txt::new(service.txt).map_err(|_| TypedError::Decode(None))?;
            let message = txt
                .get(TXT_TYPED_KEY)
                .map(|v| codec.decode(v).map_err(|e| TypedError::Decode(Some(e))))
                .transpose()?;
            Ok(TypedEvent::Up { service, message })
        }
    }
}

/// Decodes a raw sample's payload, keeping its key and metadata.
fn decode_sample<'a, M, C: Codec<M>, E>(
    codec: &mut C,
    sample: Sample<'a>,
) -> Result<TypedSample<'a, M>, TypedError<E, C::Error>> {
    let message = codec.decode(sample.payload).map_err(TypedError::Decode)?;
    Ok(TypedSample {
        key: sample.key,
        message,
        meta: sample.meta,
    })
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::codec::Raw;
    use crate::transport::{SenderId, sample_into};
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
            b.get_mut(..payload.len())
                .ok_or(())?
                .copy_from_slice(payload);
            self.last = Some((b, payload.len()));
            Ok(())
        }
    }

    impl Subscriber for Loopback {
        type Error = ();
        const DELIVERY: crate::transport::Delivery =
            crate::transport::Delivery::new(crate::transport::Overflow::DropOldest, true);
        const LIMITS: crate::transport::Limits = crate::transport::Limits::new(16, 8);
        async fn recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Sample<'b>, ()> {
            let (b, n) = self.last.take().ok_or(())?;
            sample_into(buf, "loop/back", &b[..n]).ok_or(())
        }

        fn try_recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Sample<'b>>, ()> {
            match self.last.take() {
                None => Ok(None),
                Some((b, n)) => {
                    let meta = SampleMeta::from_sender(SenderId([7; 8])).with_seq(3);
                    let s = sample_into(buf, "loop/back", &b[..n]).ok_or(())?;
                    Ok(Some(s.with_meta(meta)))
                }
            }
        }
    }

    impl Requester for Loopback {
        type Error = ();
        /// Ignores the options: the loopback answers at once.
        async fn request_with<'b>(
            &mut self,
            _key: Key<'_>,
            request: &[u8],
            response: &'b mut [u8],
            _options: &RequestOptions,
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

    #[test]
    fn typed_try_recv_is_none_then_decodes_with_meta() {
        let mut c: Client<_, _, 32> = Client::new(Loopback::default(), Raw);
        assert_eq!(c.try_recv::<[u8; 2]>(), Ok(None));
        block_on(c.publish(K, &[4u8, 5])).unwrap();
        let s = c.try_recv::<[u8; 2]>().unwrap().expect("sample ready");
        assert_eq!((s.key, s.message), (K, [4, 5]));
        assert_eq!(s.meta.sender, Some(SenderId([7; 8])));
        assert_eq!(s.meta.seq, Some(3));
    }

    #[test]
    fn typed_request_with_passes_options() {
        let mut c: Client<_, _, 16> = Client::new(Loopback::default(), Raw);
        let o = RequestOptions::DEFAULT.with_timeout(core::time::Duration::from_millis(5));
        let resp: [u8; 2] = block_on(c.request_with(K, &[1u8, 2], &o)).unwrap();
        assert_eq!(resp, [2, 1]);
    }

    #[test]
    fn typed_handler_decodes_calls_and_encodes() {
        let mut seen = None;
        let mut h = TypedHandler::new(Raw, |req: &[u8; 2], meta: &RequestMeta<'_>| {
            seen = meta.principal.map(|p| p.len());
            if req[0] == 0 {
                return Err(HandlerError::Denied);
            }
            Ok([req[1], req[0], 9u8])
        });
        let meta = RequestMeta::NONE.with_principal("spiffe://a");
        let mut out = [0u8; 8];
        let req = Request::new(K, &[1, 2]).with_meta(meta);
        assert_eq!(h.handle(&req, &mut out), Ok(3));
        assert_eq!(&out[..3], &[2, 1, 9]);
        assert_eq!(
            h.handle(&Request::new(K, &[0, 2]), &mut out),
            Err(HandlerError::Denied)
        );
        assert_eq!(
            h.handle(&Request::new(K, &[1]), &mut out),
            Err(HandlerError::InvalidRequest)
        );
        assert_eq!(
            h.handle(&req, &mut out[..2]),
            Err(HandlerError::BufferTooSmall)
        );
        assert_eq!(seen, Some(10));
    }

    /// Hands out queued events.
    struct Events(std::vec::Vec<Event<'static>>);

    impl Browse for Events {
        type Error = ();
        async fn browse(&mut self, _filter: KeyFilter<'_>) -> Result<(), ()> {
            Ok(())
        }
        async fn next<'b>(&mut self, buf: &'b mut [u8]) -> Result<Event<'b>, ()> {
            self.try_next(buf)?.ok_or(())
        }
        fn try_next<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Event<'b>>, ()> {
            if self.0.is_empty() {
                return Ok(None);
            }
            let e = self.0.remove(0);
            crate::discovery::record::event_into(buf, &e)
                .map(Some)
                .map_err(|_| ())
        }
    }

    #[test]
    fn typed_browser_decodes_txt_descriptions() {
        use crate::discovery::Endpoint;
        const CAM: ServiceKind<'static> = ServiceKind::from_static("cam/jpeg");
        let mut txt = [0u8; 16];
        let txt = encode_txt(&Raw, &[240u8, 1], &mut txt).unwrap();
        assert_eq!(txt, b"\x04m=\xf0\x01");
        let txt: &'static [u8] = std::boxed::Box::leak(txt.to_vec().into_boxed_slice());
        let up = Service {
            kind: CAM,
            instance: "c",
            node: None,
            endpoint: Endpoint::Local,
            txt,
            ttl_s: 9,
        };
        let plain = Service {
            txt: b"\x03a=b",
            ..up
        };
        let broken = Service {
            txt: b"\x02m=",
            ..up
        };
        let events = Events(std::vec![
            Event::Up(up),
            Event::Up(plain),
            Event::Up(broken),
            Event::Down {
                kind: CAM,
                instance: "c"
            }
        ]);
        let mut b: TypedBrowser<_, _, 64> = TypedBrowser::new(events, Raw);
        block_on(b.browse(KeyFilter::Exact(CAM.key()))).unwrap();
        assert_eq!(
            block_on(b.next::<[u8; 2]>()),
            Ok(TypedEvent::Up {
                service: up,
                message: Some([240, 1])
            })
        );
        assert_eq!(
            b.try_next::<[u8; 2]>(),
            Ok(Some(TypedEvent::Up {
                service: plain,
                message: None
            }))
        );
        assert!(matches!(
            b.try_next::<[u8; 2]>(),
            Err(TypedError::Decode(Some(_)))
        ));
        assert_eq!(
            b.try_next::<[u8; 2]>(),
            Ok(Some(TypedEvent::Down {
                kind: CAM,
                instance: "c"
            }))
        );
        assert_eq!(b.try_next::<[u8; 2]>(), Ok(None));
    }
}
