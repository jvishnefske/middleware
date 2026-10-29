//! [`SerialLink`]: the middleware over any `embedded-io-async` byte stream.
//!
//! The same link type serves a UART behind a CP2102/CH340 bridge, a native
//! USB-Serial-JTAG or CDC-ACM port, and an in-memory pipe in tests.
//!
//! Event loops that must handle several frame kinds call
//! [`SerialLink::next_frame`] and [`SerialLink::send`] directly. The core
//! traits are also implemented; each waits for the frame kind it needs and
//! discards others, giving up after [`MAX_SKIPPED_FRAMES`] so a chatty peer
//! cannot stall a caller forever.
//!
//! # Subscriptions
//!
//! A link is one point-to-point stream, so [`Subscriber`] on the link itself
//! yields every publication whatever its key. [`Subscribe::subscribe`]
//! borrows the link as a [`Filtered`] subscriber that keeps its own copy of
//! the [`KeyFilter`] (a [`KeyFilterBuf`], no heap) and yields only matching
//! keys, discarding the rest. The view borrows the one stream it reads from,
//! so there is one subscriber at a time and the link cannot be used
//! directly while it lives; drop it to publish or request again.
//!
//! # Cancel safety
//!
//! All receive state (the read chunk, its cursor and the frame splitter) and
//! all transmit state (the encoded frame and how much of it was written) live
//! in `self`, so dropping a future at any `.await` loses nothing, provided
//! the stream's own `read` and `write` are cancel safe (a dropped call has
//! consumed or written no bytes it did not report):
//!
//! * [`Subscriber::recv`] (and [`SerialLink::next_frame`]) dropped mid-frame
//!   keeps the partial frame in the splitter; the next receive completes it.
//! * [`Publisher::publish`] / [`SerialLink::send`] dropped mid-write leaves
//!   the rest of the frame pending; the next send writes it out before its
//!   own frame, so the peer never sees a torn frame and nothing is sent twice.
//! * [`Requester::request`] dropped after sending abandons the request; its
//!   late response is discarded by the next request (correlation ids).
//! * [`Responder::serve_one`] keeps no state across calls besides the above.
//!
//! [`Subscriber::try_recv`] polls the stream's `read` once with a no-op waker
//! and drops the future if it is pending, so it relies on the same cancel
//! safety of `read`.

use super::frame::{
    self, FRAME_OVERHEAD, Frame, FrameError, MAX_RAW_FRAME, MAX_WIRE_FRAME, Split, Splitter,
};
use crate::key::{Key, KeyFilter, KeyFilterBuf, MAX_KEY_LEN};
use crate::transport::{
    Delivery, Handler, HandlerError, Limits, Overflow, Publisher, Request, RequestOptions,
    Requester, Responder, Sample, Subscribe, Subscriber,
};
use core::future::Future;
use core::pin::pin;
use core::task::{Context, Poll, Waker};
use embedded_io_async::{Read, Write};

/// Frames of the wrong kind a trait method discards before giving up.
pub const MAX_SKIPPED_FRAMES: usize = 64;

/// Size of the link's read chunk.
const RX_CHUNK: usize = 64;

/// Reads one [`Subscriber::try_recv`] call attempts before reporting "nothing
/// yet": enough for a maximal frame when every read fills the chunk. A
/// stream that delivers fewer bytes per read finishes the frame on a later
/// call; nothing is lost in between.
const TRY_RECV_READS: usize = MAX_WIRE_FRAME.div_ceil(RX_CHUNK) + 1;

/// Size limits of a serial link: any key up to [`MAX_KEY_LEN`] together with
/// any payload up to `max_payload` fits one [`MAX_RAW_FRAME`] publish frame.
/// (A shorter key leaves room for a longer payload, which `send` accepts.)
pub const LIMITS: Limits = Limits::shared(MAX_KEY_LEN, MAX_RAW_FRAME - FRAME_OVERHEAD);

/// What a serial link's subscribers deliver, as their
/// [`Subscriber::DELIVERY`]: the link reads the byte stream only when
/// asked, so an unread publication stays in the peer's write path and the
/// sender waits (flow control of the underlying stream; a UART without it
/// loses bytes in its driver, which the frame CRC then rejects). Frames
/// arrive in the order sent.
pub const DELIVERY: Delivery = Delivery::new(Overflow::Block, true);

/// A receive buffer size that holds any frame [`Subscriber::recv`] can
/// return. Received frames are decoded in place from their COBS form, so the
/// buffer must hold the wire frame, not just key and payload.
pub const RECV_BUF_LEN: usize = MAX_WIRE_FRAME;

/// Failure of a [`SerialLink`] operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkError<E> {
    /// The underlying stream failed.
    Io(E),
    /// The stream reached end of file (a read or write returned 0 bytes).
    Closed,
    /// An outgoing frame could not be encoded.
    Frame(FrameError),
    /// The caller's buffer is smaller than the received frame.
    BufferTooSmall,
    /// The peer answered a request with a handler error.
    Handler(HandlerError),
    /// [`MAX_SKIPPED_FRAMES`] unrelated frames arrived while waiting.
    SkipLimit,
}

/// Middleware endpoint over a byte stream.
pub struct SerialLink<IO> {
    io: IO,
    splitter: Splitter<MAX_WIRE_FRAME>,
    rx: [u8; RX_CHUNK],
    rx_pos: usize,
    rx_len: usize,
    scratch: [u8; MAX_RAW_FRAME],
    tx: [u8; MAX_WIRE_FRAME],
    tx_pos: usize,
    tx_end: usize,
    next_id: u8,
    rejected: u32,
}

impl<IO> SerialLink<IO> {
    /// Wraps a stream.
    pub fn new(io: IO) -> Self {
        Self {
            io,
            splitter: Splitter::new(),
            rx: [0; RX_CHUNK],
            rx_pos: 0,
            rx_len: 0,
            scratch: [0; MAX_RAW_FRAME],
            tx: [0; MAX_WIRE_FRAME],
            tx_pos: 0,
            tx_end: 0,
            next_id: 0,
            rejected: 0,
        }
    }

    /// Delimited byte runs that were not valid frames (noise, ROM banners,
    /// overflow), since creation.
    pub fn rejected(&self) -> u32 {
        self.rejected
    }

    /// Returns the stream. Bytes of a frame whose send was cancelled and not
    /// yet completed are discarded.
    pub fn into_inner(self) -> IO {
        self.io
    }

    /// Feeds buffered bytes to the splitter until a delimited run that
    /// decodes is complete (returned as its COBS length, left in the
    /// splitter) or the buffer is exhausted. Bounded by [`RX_CHUNK`].
    fn pump(&mut self) -> Option<usize> {
        while self.rx_pos < self.rx_len {
            let byte = self.rx.get(self.rx_pos).copied().unwrap_or(0);
            self.rx_pos += 1;
            match self.splitter.push(byte) {
                Split::Pending => {}
                Split::Overflow => self.rejected = self.rejected.saturating_add(1),
                Split::Frame(len) => {
                    if self.peek_valid() {
                        return Some(len);
                    }
                    self.rejected = self.rejected.saturating_add(1);
                }
            }
        }
        None
    }

    /// Records a completed read of `n` bytes into the chunk.
    fn filled<E>(&mut self, n: usize) -> Result<(), LinkError<E>> {
        if n == 0 {
            return Err(LinkError::Closed);
        }
        self.rx_pos = 0;
        self.rx_len = n.min(RX_CHUNK);
        Ok(())
    }

    /// Checks the splitter's current run decodes, using `scratch`.
    fn peek_valid(&mut self) -> bool {
        self.decode_current().is_ok()
    }

    /// Decodes the splitter's current run into `scratch`, for inspection.
    fn decode_current(&mut self) -> Result<Frame<'_>, FrameError> {
        let src = self.splitter.frame();
        let dst = self
            .scratch
            .get_mut(..src.len())
            .ok_or(FrameError::TooLarge)?;
        dst.copy_from_slice(src);
        frame::decode_wire(dst)
    }

    /// True if the current run is a publication selected by `filter`
    /// (`None` selects every publication).
    fn current_is_publish(&mut self, filter: Option<KeyFilter<'_>>) -> bool {
        match self.decode_current() {
            Ok(Frame::Publish { key, .. }) => filter.is_none_or(|f| f.matches(key)),
            _ => false,
        }
    }

    /// Copies the current run (a publication of COBS length `len`) into
    /// `buf` and decodes it there.
    fn take_publish<'b, E>(
        &mut self,
        buf: &'b mut [u8],
        len: usize,
    ) -> Result<Sample<'b>, LinkError<E>> {
        let dst = buf.get_mut(..len).ok_or(LinkError::BufferTooSmall)?;
        dst.copy_from_slice(self.splitter.frame());
        match frame::decode_wire(dst) {
            Ok(Frame::Publish { key, payload }) => Ok(Sample::new(key, payload)),
            _ => Err(LinkError::Frame(FrameError::Malformed)),
        }
    }

    fn take_id(&mut self) -> u8 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        id
    }
}

impl<IO: Read + Write> SerialLink<IO> {
    /// Encodes and writes one frame, then flushes. The rest of a frame whose
    /// send was cancelled is written first.
    pub async fn send(&mut self, frame: &Frame<'_>) -> Result<(), LinkError<IO::Error>> {
        self.write_pending().await?;
        let n = frame::encode(frame, &mut self.scratch, &mut self.tx).map_err(LinkError::Frame)?;
        self.tx_pos = 0;
        self.tx_end = n;
        self.write_pending().await
    }

    /// Writes the not yet written part of the encoded frame in `tx`, then
    /// flushes. Each write advances the cursor or fails, so the loop is
    /// bounded by [`MAX_WIRE_FRAME`].
    async fn write_pending(&mut self) -> Result<(), LinkError<IO::Error>> {
        if self.tx_pos == self.tx_end {
            return Ok(());
        }
        while self.tx_pos < self.tx_end {
            let chunk = self
                .tx
                .get(self.tx_pos..self.tx_end)
                .ok_or(LinkError::Frame(FrameError::TooLarge))?;
            let n = self.io.write(chunk).await.map_err(LinkError::Io)?;
            if n == 0 {
                return Err(LinkError::Closed);
            }
            self.tx_pos = (self.tx_pos + n).min(self.tx_end);
        }
        self.io.flush().await.map_err(LinkError::Io)
    }

    /// Waits for the next valid frame and decodes it into `buf`. Invalid byte
    /// runs are counted in [`SerialLink::rejected`] and skipped.
    pub async fn next_frame<'b>(
        &mut self,
        buf: &'b mut [u8],
    ) -> Result<Frame<'b>, LinkError<IO::Error>> {
        let len = self.next_wire_frame().await?;
        let dst = buf.get_mut(..len).ok_or(LinkError::BufferTooSmall)?;
        dst.copy_from_slice(self.splitter.frame());
        // `next_wire_frame` only stops on runs that decode, so this succeeds.
        frame::decode_wire(dst).map_err(LinkError::Frame)
    }

    /// Reads until the splitter yields a delimited run that decodes, leaving
    /// it in the splitter. Returns its COBS length.
    async fn next_wire_frame(&mut self) -> Result<usize, LinkError<IO::Error>> {
        loop {
            if let Some(len) = self.pump() {
                return Ok(len);
            }
            let n = self.io.read(&mut self.rx).await.map_err(LinkError::Io)?;
            self.filled(n)?;
        }
    }

    /// Like [`Self::next_wire_frame`] but never waits: polls `read` once per
    /// round with a no-op waker and stops at the first pending read.
    fn try_next_wire_frame(&mut self) -> Result<Option<usize>, LinkError<IO::Error>> {
        for _ in 0..TRY_RECV_READS {
            if let Some(len) = self.pump() {
                return Ok(Some(len));
            }
            let polled = {
                let read = pin!(self.io.read(&mut self.rx));
                read.poll(&mut Context::from_waker(Waker::noop()))
            };
            match polled {
                Poll::Pending => return Ok(None),
                Poll::Ready(r) => self.filled(r.map_err(LinkError::Io)?)?,
            }
        }
        Ok(self.pump())
    }

    /// Waits for the next publication selected by `filter`.
    async fn recv_matching<'b>(
        &mut self,
        filter: Option<KeyFilter<'_>>,
        buf: &'b mut [u8],
    ) -> Result<Sample<'b>, LinkError<IO::Error>> {
        for _ in 0..MAX_SKIPPED_FRAMES {
            let len = self.next_wire_frame().await?;
            if self.current_is_publish(filter) {
                return self.take_publish(buf, len);
            }
        }
        Err(LinkError::SkipLimit)
    }

    /// Returns an already received publication selected by `filter`, if any.
    fn try_recv_matching<'b>(
        &mut self,
        filter: Option<KeyFilter<'_>>,
        buf: &'b mut [u8],
    ) -> Result<Option<Sample<'b>>, LinkError<IO::Error>> {
        for _ in 0..MAX_SKIPPED_FRAMES {
            let Some(len) = self.try_next_wire_frame()? else {
                return Ok(None);
            };
            if self.current_is_publish(filter) {
                return self.take_publish(buf, len).map(Some);
            }
        }
        Ok(None)
    }
}

impl<IO: Read + Write> Publisher for SerialLink<IO> {
    type Error = LinkError<IO::Error>;

    /// See [`LIMITS`]: a 128-byte key leaves room for 891 payload bytes.
    const LIMITS: Limits = LIMITS;

    /// Sends a publish frame. Cancelling may leave the frame partly written;
    /// the next send completes it (see the module docs).
    async fn publish(&mut self, key: Key<'_>, payload: &[u8]) -> Result<(), Self::Error> {
        self.send(&Frame::Publish { key, payload }).await
    }
}

impl<IO: Read + Write> Subscriber for SerialLink<IO> {
    type Error = LinkError<IO::Error>;

    /// See [`DELIVERY`].
    const DELIVERY: Delivery = DELIVERY;
    const LIMITS: Limits = LIMITS;

    /// Yields the next publication, whatever its key; other frame kinds are
    /// discarded. Cancel safe: a partial frame stays in the splitter. `buf`
    /// must hold the COBS wire frame ([`RECV_BUF_LEN`] always suffices); a
    /// smaller buffer is [`LinkError::BufferTooSmall`] and drops the sample.
    /// The sample carries no sender metadata (point-to-point link).
    async fn recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Sample<'b>, Self::Error> {
        self.recv_matching(None, buf).await
    }

    /// Returns a publication already readable without waiting: bytes left in
    /// the read chunk, then `read` polled once per chunk with a no-op waker.
    fn try_recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Sample<'b>>, Self::Error> {
        self.try_recv_matching(None, buf)
    }
}

impl<IO: Read + Write> Subscribe for SerialLink<IO> {
    type Subscriber<'a>
        = Filtered<'a, IO>
    where
        Self: 'a;
    type Error = LinkError<IO::Error>;

    /// Borrows the link as a [`Filtered`] subscriber holding a copy of
    /// `filter`. Never waits and never fails: every valid key fits the
    /// view's [`MAX_KEY_LEN`]-byte filter buffer.
    async fn subscribe<'a>(
        &'a mut self,
        filter: KeyFilter<'_>,
    ) -> Result<Filtered<'a, IO>, Self::Error> {
        // Unreachable error: a valid key is at most MAX_KEY_LEN bytes.
        let filter = filter
            .to_buf()
            .map_err(|_| LinkError::Frame(FrameError::InvalidKey))?;
        Ok(Filtered { link: self, filter })
    }
}

/// A [`SerialLink`] borrowed as a subscriber to the keys matching a
/// [`KeyFilter`]; made by [`Subscribe::subscribe`]. The filter is copied
/// into the view, so it does not borrow the caller's key. Publications on
/// other keys, and frames of other kinds, are read and discarded.
pub struct Filtered<'l, IO> {
    link: &'l mut SerialLink<IO>,
    filter: KeyFilterBuf<MAX_KEY_LEN>,
}

impl<IO> Filtered<'_, IO> {
    /// The filter this view applies.
    pub fn filter(&self) -> KeyFilter<'_> {
        self.filter.as_filter()
    }
}

impl<IO: Read + Write> Subscriber for Filtered<'_, IO> {
    type Error = LinkError<IO::Error>;

    /// See [`DELIVERY`].
    const DELIVERY: Delivery = DELIVERY;
    const LIMITS: Limits = LIMITS;

    /// As [`SerialLink`]'s `recv`, skipping keys the filter rejects (they
    /// count towards [`MAX_SKIPPED_FRAMES`]).
    async fn recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Sample<'b>, Self::Error> {
        self.link
            .recv_matching(Some(self.filter.as_filter()), buf)
            .await
    }

    /// As [`SerialLink`]'s `try_recv`, skipping keys the filter rejects.
    fn try_recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Sample<'b>>, Self::Error> {
        self.link
            .try_recv_matching(Some(self.filter.as_filter()), buf)
    }
}

impl<IO: Read + Write> Requester for SerialLink<IO> {
    type Error = LinkError<IO::Error>;

    /// Sends a request and waits for the response with the same id; other
    /// frames, including stale responses, are discarded. Cancelling abandons
    /// the request; its late response is discarded by the next request.
    ///
    /// The link has no clock, so `options.timeout` is **not** honoured: the
    /// wait is bounded by [`MAX_SKIPPED_FRAMES`] frames, and a caller that
    /// needs a time bound races the call against a timer (it is cancel
    /// safe). The target is ignored: the link has one peer.
    async fn request_with<'b>(
        &mut self,
        key: Key<'_>,
        request: &[u8],
        response: &'b mut [u8],
        _options: &RequestOptions,
    ) -> Result<&'b [u8], Self::Error> {
        let id = self.take_id();
        self.send(&Frame::Request {
            id,
            key,
            payload: request,
        })
        .await?;
        for _ in 0..MAX_SKIPPED_FRAMES {
            self.next_wire_frame().await?;
            match self.decode_current() {
                Ok(Frame::Response { id: got, result }) if got == id => {
                    let body = result.map_err(LinkError::Handler)?;
                    let n = body.len();
                    let dst = response.get_mut(..n).ok_or(LinkError::BufferTooSmall)?;
                    dst.copy_from_slice(body);
                    return Ok(dst);
                }
                _ => continue,
            }
        }
        Err(LinkError::SkipLimit)
    }
}

impl<IO: Read + Write> Responder for SerialLink<IO> {
    type Error = LinkError<IO::Error>;

    /// Waits for one request, runs `handler`, and sends the response. The
    /// request is decoded into the front of `scratch`; the response is built
    /// in the remainder. Cancelling while waiting loses nothing; cancelling
    /// while the response is written leaves it pending for the next send.
    async fn serve_one<H: Handler>(
        &mut self,
        handler: &mut H,
        scratch: &mut [u8],
    ) -> Result<(), Self::Error> {
        let mut found = None;
        for _ in 0..MAX_SKIPPED_FRAMES {
            let len = self.next_wire_frame().await?;
            if let Ok(Frame::Request { .. }) = self.decode_current() {
                found = Some(len);
                break;
            }
        }
        let len = found.ok_or(LinkError::SkipLimit)?;
        let (req_buf, resp_buf) = scratch
            .split_at_mut_checked(len)
            .ok_or(LinkError::BufferTooSmall)?;
        req_buf.copy_from_slice(self.splitter.frame());
        let Ok(Frame::Request { id, key, payload }) = frame::decode_wire(req_buf) else {
            return Err(LinkError::Frame(FrameError::Malformed));
        };
        // A serial peer is unauthenticated and sends no deadline.
        let result = handler
            .handle(&Request::new(key, payload), resp_buf)
            .and_then(|n| resp_buf.get(..n).ok_or(HandlerError::BufferTooSmall));
        self.send(&Frame::Response { id, result }).await
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::transport::{HandlerError, SampleMeta};
    use futures::executor::block_on;
    use std::vec::Vec;

    /// Polls `fut` exactly once with a no-op waker, then drops it.
    fn poll_once_and_drop<F: Future>(fut: F) -> bool {
        let fut = pin!(fut);
        fut.poll(&mut Context::from_waker(Waker::noop())).is_ready()
    }

    /// Scripted stream: reads come from `input`, writes collect in `output`.
    ///
    /// A closed pipe reads 0 (end of file) once `input` is exhausted; an open
    /// one is pending instead, like a UART with no bytes yet. Writes accept at
    /// most `write_budget` bytes in total and are pending when it is spent.
    struct Pipe {
        input: Vec<u8>,
        pos: usize,
        output: Vec<u8>,
        open: bool,
        write_budget: usize,
    }

    impl Pipe {
        fn new(input: Vec<u8>) -> Self {
            Self {
                input,
                pos: 0,
                output: Vec::new(),
                open: false,
                write_budget: usize::MAX,
            }
        }

        fn open() -> Self {
            Self {
                open: true,
                ..Self::new(Vec::new())
            }
        }
    }

    impl embedded_io_async::ErrorType for Pipe {
        type Error = core::convert::Infallible;
    }

    impl Read for Pipe {
        async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
            if self.open && self.pos == self.input.len() {
                core::future::pending::<()>().await;
            }
            // Deliver in small pieces to exercise chunk handling.
            let n = buf.len().min(5).min(self.input.len() - self.pos);
            buf[..n].copy_from_slice(&self.input[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    impl Write for Pipe {
        async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
            if self.write_budget == 0 {
                core::future::pending::<()>().await;
            }
            let n = buf.len().min(self.write_budget);
            self.write_budget -= n;
            self.output.extend_from_slice(&buf[..n]);
            Ok(n)
        }

        async fn flush(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    const K: Key<'static> = Key::from_static("demo/key");

    fn wire(frames: &[Frame<'_>]) -> Vec<u8> {
        let mut out = Vec::new();
        for f in frames {
            let mut scratch = [0u8; MAX_RAW_FRAME];
            let mut buf = [0u8; MAX_WIRE_FRAME];
            let n = frame::encode(f, &mut scratch, &mut buf).unwrap();
            out.extend_from_slice(&buf[..n]);
        }
        out
    }

    fn decode_all(bytes: &[u8]) -> Vec<Vec<u8>> {
        bytes
            .split(|&b| b == 0)
            .filter(|r| !r.is_empty())
            .map(|r| r.to_vec())
            .collect()
    }

    #[test]
    fn request_skips_noise_and_stale_responses() {
        let mut input = b"rst:0x1 (POWERON)\n".to_vec();
        input.push(0);
        input.extend(wire(&[
            Frame::Response {
                id: 9,
                result: Ok(b"stale"),
            },
            Frame::Publish {
                key: K,
                payload: b"event",
            },
            Frame::Response {
                id: 0,
                result: Ok(b"pong"),
            },
        ]));
        let mut link = SerialLink::new(Pipe::new(input));
        let mut resp = [0u8; 16];
        let got = block_on(link.request(K, b"ping", &mut resp)).unwrap();
        assert_eq!(got, b"pong");
        assert_eq!(link.rejected(), 1);
        let sent = decode_all(&link.into_inner().output);
        let mut first = sent[0].clone();
        assert_eq!(
            frame::decode_wire(&mut first).unwrap(),
            Frame::Request {
                id: 0,
                key: K,
                payload: b"ping"
            }
        );
    }

    #[test]
    fn request_surfaces_handler_error_and_small_buffer() {
        let input = wire(&[
            Frame::Response {
                id: 0,
                result: Err(HandlerError::InvalidRequest),
            },
            Frame::Response {
                id: 1,
                result: Ok(b"too long"),
            },
        ]);
        let mut link = SerialLink::new(Pipe::new(input));
        let mut resp = [0u8; 4];
        assert_eq!(
            block_on(link.request(K, b"", &mut resp)),
            Err(LinkError::Handler(HandlerError::InvalidRequest))
        );
        assert_eq!(
            block_on(link.request(K, b"", &mut resp)),
            Err(LinkError::BufferTooSmall)
        );
        assert_eq!(
            block_on(link.request(K, b"", &mut resp)),
            Err(LinkError::Closed)
        );
    }

    #[test]
    fn serve_one_runs_handler_and_replies() {
        let input = wire(&[
            Frame::Publish {
                key: K,
                payload: b"ignored",
            },
            Frame::Request {
                id: 42,
                key: K,
                payload: b"abc",
            },
            Frame::Request {
                id: 43,
                key: K,
                payload: b"",
            },
        ]);
        let mut link = SerialLink::new(Pipe::new(input));
        let mut upper = crate::transport::handler_fn(|r, resp| {
            let req = r.body;
            if req.is_empty() {
                return Err(HandlerError::InvalidRequest);
            }
            for (o, i) in resp.iter_mut().zip(req) {
                *o = i.to_ascii_uppercase();
            }
            Ok(req.len())
        });
        let mut scratch = [0u8; 256];
        block_on(link.serve_one(&mut upper, &mut scratch)).unwrap();
        block_on(link.serve_one(&mut upper, &mut scratch)).unwrap();
        let sent = decode_all(&link.into_inner().output);
        let mut a = sent[0].clone();
        let mut b = sent[1].clone();
        assert_eq!(
            frame::decode_wire(&mut a).unwrap(),
            Frame::Response {
                id: 42,
                result: Ok(b"ABC")
            }
        );
        assert_eq!(
            frame::decode_wire(&mut b).unwrap(),
            Frame::Response {
                id: 43,
                result: Err(HandlerError::InvalidRequest)
            }
        );
    }

    #[test]
    fn publish_and_subscribe_round_trip() {
        let mut tx = SerialLink::new(Pipe::new(Vec::new()));
        block_on(tx.publish(K, b"\x00data\x00")).unwrap();
        let bytes = tx.into_inner().output;
        let mut rx = SerialLink::new(Pipe::new(bytes));
        let mut buf = [0u8; 64];
        let s = block_on(rx.recv(&mut buf)).unwrap();
        assert_eq!(s.key, K);
        assert_eq!(s.payload, b"\x00data\x00");
    }

    #[test]
    fn next_frame_reports_small_buffer() {
        let input = wire(&[Frame::Publish {
            key: K,
            payload: &[7u8; 40],
        }]);
        let mut link = SerialLink::new(Pipe::new(input));
        let mut buf = [0u8; 8];
        assert_eq!(
            block_on(link.next_frame(&mut buf)).err(),
            Some(LinkError::BufferTooSmall)
        );
    }

    #[test]
    fn try_recv_is_none_when_empty_and_some_without_waiting() {
        let mut link = SerialLink::new(Pipe::open());
        let mut buf = [0u8; RECV_BUF_LEN];
        assert_eq!(link.try_recv(&mut buf), Ok(None));
        link.io.input.extend(wire(&[
            Frame::Response {
                id: 1,
                result: Ok(b"skip"),
            },
            Frame::Publish {
                key: K,
                payload: b"now",
            },
        ]));
        let s = link.try_recv(&mut buf).unwrap().expect("sample ready");
        assert_eq!((s.key, s.payload), (K, &b"now"[..]));
        assert_eq!(s.meta, SampleMeta::default());
        assert_eq!(link.try_recv(&mut buf), Ok(None));
    }

    #[test]
    fn filtered_prefix_delivers_app_x_but_not_apple() {
        let app_x = Key::from_static("app/x");
        let input = wire(&[
            Frame::Publish {
                key: Key::from_static("apple"),
                payload: b"no",
            },
            Frame::Publish {
                key: app_x,
                payload: b"yes",
            },
            Frame::Publish {
                key: Key::from_static("apple"),
                payload: b"no",
            },
            Frame::Publish {
                key: Key::from_static("app"),
                payload: b"self",
            },
        ]);
        let mut link = SerialLink::new(Pipe::new(input));
        let mut sub = block_on(link.subscribe(KeyFilter::Prefix(Key::from_static("app")))).unwrap();
        let mut buf = [0u8; RECV_BUF_LEN];
        let s = block_on(sub.recv(&mut buf)).unwrap();
        assert_eq!((s.key, s.payload), (app_x, &b"yes"[..]));
        let s = sub.try_recv(&mut buf).unwrap().expect("app itself matches");
        assert_eq!(s.key.as_str(), "app");
        assert_eq!(sub.try_recv(&mut buf), Err(LinkError::Closed));
    }

    #[test]
    fn c12_subscribe_borrows_the_link_and_owns_its_filter() {
        let wanted = Key::from_static("sensor/t");
        let input = wire(&[
            Frame::Response {
                id: 3,
                result: Ok(b"skip"),
            },
            Frame::Publish {
                key: Key::from_static("sensor/tx"),
                payload: b"no",
            },
            Frame::Publish {
                key: wanted,
                payload: b"yes",
            },
        ]);
        let mut link = SerialLink::new(Pipe::new(input));
        let mut buf = [0u8; RECV_BUF_LEN];
        {
            // The filter's key text lives only in this block; the view keeps
            // its own copy.
            let text = std::string::String::from("sensor/t");
            let filter = KeyFilter::Exact(Key::new(&text).unwrap());
            let mut sub = block_on(link.subscribe(filter)).unwrap();
            drop(text);
            assert_eq!(sub.filter(), KeyFilter::Exact(wanted));
            let s = block_on(sub.recv(&mut buf)).unwrap();
            assert_eq!((s.key, s.payload), (wanted, &b"yes"[..]));
        }
        // The borrow ended with the view: the link is usable again.
        block_on(link.publish(K, b"after")).unwrap();
        assert_eq!(decode_all(&link.into_inner().output).len(), 1);
    }

    /// Generic code reaches the link through the trait alone.
    async fn first_matching<S: Subscribe>(
        transport: &mut S,
        filter: KeyFilter<'_>,
        buf: &mut [u8],
    ) -> Option<usize> {
        let mut sub = transport.subscribe(filter).await.ok()?;
        sub.recv(buf).await.ok().map(|s| s.payload.len())
    }

    #[test]
    fn c12_serial_link_is_usable_through_generic_subscribe() {
        let input = wire(&[
            Frame::Publish {
                key: Key::from_static("b"),
                payload: b"x",
            },
            Frame::Publish {
                key: Key::from_static("a/1"),
                payload: b"abc",
            },
        ]);
        let mut link = SerialLink::new(Pipe::new(input));
        let mut buf = [0u8; RECV_BUF_LEN];
        let got = block_on(first_matching(
            &mut link,
            KeyFilter::Prefix(Key::from_static("a")),
            &mut buf,
        ));
        assert_eq!(got, Some(3));
    }

    #[test]
    fn cancelled_recv_keeps_partial_frame() {
        let bytes = wire(&[Frame::Publish {
            key: K,
            payload: b"split across reads",
        }]);
        let (first, rest) = bytes.split_at(bytes.len() / 2);
        let mut link = SerialLink::new(Pipe::open());
        link.io.input.extend_from_slice(first);
        let mut buf = [0u8; RECV_BUF_LEN];
        assert!(!poll_once_and_drop(link.recv(&mut buf)), "frame incomplete");
        assert_eq!(link.io.pos, first.len(), "first half consumed");
        link.io.input.extend_from_slice(rest);
        let s = block_on(link.recv(&mut buf)).unwrap();
        assert_eq!(s.payload, b"split across reads");
    }

    #[test]
    fn cancelled_publish_is_completed_by_next_send() {
        let mut pipe = Pipe::new(Vec::new());
        pipe.write_budget = 3;
        let mut link = SerialLink::new(pipe);
        assert!(!poll_once_and_drop(link.publish(K, b"first")));
        link.io.write_budget = usize::MAX;
        block_on(link.publish(K, b"second")).unwrap();
        let sent = decode_all(&link.into_inner().output);
        assert_eq!(sent.len(), 2, "no torn frame");
        let payloads: Vec<Vec<u8>> = sent
            .into_iter()
            .map(|mut f| match frame::decode_wire(&mut f).unwrap() {
                Frame::Publish { payload, .. } => payload.to_vec(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(payloads, [b"first".to_vec(), b"second".to_vec()]);
    }

    #[test]
    fn limits_admit_longest_key_with_largest_payload() {
        type L = SerialLink<Pipe>;
        assert_eq!(<L as Publisher>::LIMITS, LIMITS);
        let long = [b'k'; MAX_KEY_LEN];
        let key = Key::new(core::str::from_utf8(&long).unwrap()).unwrap();
        let mut raw = [0u8; MAX_RAW_FRAME + 8];
        let payload = [7u8; LIMITS.max_payload + 1];
        let fit = Frame::Publish {
            key,
            payload: &payload[..LIMITS.max_payload],
        };
        assert_eq!(frame::encode_raw(&fit, &mut raw), Ok(MAX_RAW_FRAME));
        let over = Frame::Publish {
            key,
            payload: &payload,
        };
        assert_eq!(
            frame::encode_raw(&over, &mut raw),
            Err(FrameError::TooLarge)
        );
    }
}
