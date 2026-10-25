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

use super::frame::{self, Frame, FrameError, MAX_RAW_FRAME, MAX_WIRE_FRAME, Split, Splitter};
use crate::key::Key;
use crate::transport::{
    Handler, HandlerError, Publisher, Requester, Responder, Sample, Subscriber,
};
use embedded_io_async::{Read, Write};

/// Frames of the wrong kind a trait method discards before giving up.
pub const MAX_SKIPPED_FRAMES: usize = 64;

const RX_CHUNK: usize = 64;

/// Failure of a [`SerialLink`] operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkError<E> {
    /// The underlying stream failed.
    Io(E),
    /// The stream reached end of file.
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
            next_id: 0,
            rejected: 0,
        }
    }

    /// Delimited byte runs that were not valid frames (noise, ROM banners,
    /// overflow), since creation.
    pub fn rejected(&self) -> u32 {
        self.rejected
    }

    /// Returns the stream.
    pub fn into_inner(self) -> IO {
        self.io
    }
}

impl<IO: Read + Write> SerialLink<IO> {
    /// Encodes and writes one frame, then flushes.
    pub async fn send(&mut self, frame: &Frame<'_>) -> Result<(), LinkError<IO::Error>> {
        let n = frame::encode(frame, &mut self.scratch, &mut self.tx).map_err(LinkError::Frame)?;
        let bytes = self
            .tx
            .get(..n)
            .ok_or(LinkError::Frame(FrameError::TooLarge))?;
        self.io.write_all(bytes).await.map_err(LinkError::Io)?;
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
            if self.rx_pos == self.rx_len {
                let n = self.io.read(&mut self.rx).await.map_err(LinkError::Io)?;
                if n == 0 {
                    return Err(LinkError::Closed);
                }
                self.rx_pos = 0;
                self.rx_len = n;
            }
            while self.rx_pos < self.rx_len {
                let byte = self.rx.get(self.rx_pos).copied().unwrap_or(0);
                self.rx_pos += 1;
                match self.splitter.push(byte) {
                    Split::Pending => {}
                    Split::Overflow => self.rejected = self.rejected.saturating_add(1),
                    Split::Frame(len) => {
                        if self.peek_valid() {
                            return Ok(len);
                        }
                        self.rejected = self.rejected.saturating_add(1);
                    }
                }
            }
        }
    }

    /// Checks the splitter's current run decodes, using `scratch`.
    fn peek_valid(&mut self) -> bool {
        let src = self.splitter.frame();
        match self.scratch.get_mut(..src.len()) {
            Some(dst) => {
                dst.copy_from_slice(src);
                frame::decode_wire(dst).is_ok()
            }
            None => false,
        }
    }

    fn take_id(&mut self) -> u8 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        id
    }
}

impl<IO: Read + Write> Publisher for SerialLink<IO> {
    type Error = LinkError<IO::Error>;

    async fn publish(&mut self, key: Key<'_>, payload: &[u8]) -> Result<(), Self::Error> {
        self.send(&Frame::Publish { key, payload }).await
    }
}

impl<IO: Read + Write> Subscriber for SerialLink<IO> {
    type Error = LinkError<IO::Error>;

    /// Yields the next publication, whatever its key; callers filter.
    async fn recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Sample<'b>, Self::Error> {
        let mut found = None;
        for _ in 0..MAX_SKIPPED_FRAMES {
            let len = self.next_wire_frame().await?;
            if let Ok(Frame::Publish { .. }) = self.decode_current() {
                found = Some(len);
                break;
            }
        }
        let len = found.ok_or(LinkError::SkipLimit)?;
        let dst = buf.get_mut(..len).ok_or(LinkError::BufferTooSmall)?;
        dst.copy_from_slice(self.splitter.frame());
        match frame::decode_wire(dst) {
            Ok(Frame::Publish { key, payload }) => Ok(Sample { key, payload }),
            _ => Err(LinkError::Frame(FrameError::Malformed)),
        }
    }
}

impl<IO: Read + Write> SerialLink<IO> {
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
}

impl<IO: Read + Write> Requester for SerialLink<IO> {
    type Error = LinkError<IO::Error>;

    /// Sends a request and waits for the response with the same id; other
    /// frames, including stale responses, are discarded.
    async fn request<'b>(
        &mut self,
        key: Key<'_>,
        request: &[u8],
        response: &'b mut [u8],
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
    /// in the remainder.
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
        let result = handler
            .handle(key, payload, resp_buf)
            .and_then(|n| resp_buf.get(..n).ok_or(HandlerError::BufferTooSmall));
        self.send(&Frame::Response { id, result }).await
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::transport::HandlerError;
    use futures::executor::block_on;
    use std::vec::Vec;

    /// Scripted stream: reads come from `input`, writes collect in `output`.
    struct Pipe {
        input: Vec<u8>,
        pos: usize,
        output: Vec<u8>,
    }

    impl Pipe {
        fn new(input: Vec<u8>) -> Self {
            Self {
                input,
                pos: 0,
                output: Vec::new(),
            }
        }
    }

    impl embedded_io_async::ErrorType for Pipe {
        type Error = core::convert::Infallible;
    }

    impl Read for Pipe {
        async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
            // Deliver in small pieces to exercise chunk handling.
            let n = buf.len().min(5).min(self.input.len() - self.pos);
            buf[..n].copy_from_slice(&self.input[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    impl Write for Pipe {
        async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
            self.output.extend_from_slice(buf);
            Ok(buf.len())
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
        let mut upper = |_: Key<'_>, req: &[u8], resp: &mut [u8]| {
            if req.is_empty() {
                return Err(HandlerError::InvalidRequest);
            }
            for (o, i) in resp.iter_mut().zip(req) {
                *o = i.to_ascii_uppercase();
            }
            Ok(req.len())
        };
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
}
