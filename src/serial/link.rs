//! [`SerialLink`]: the middleware over any `embedded-io-async` byte stream.
//!
//! The same link type serves a UART behind a CP2102/CH340 bridge, a native
//! USB-Serial-JTAG or CDC-ACM port, and an in-memory pipe in tests.
//!
//! Event loops that must handle several frame kinds call
//! [`SerialLink::next_frame`] and [`SerialLink::send`] directly. The core
//! traits are also implemented; each waits for the frame kind it needs,
//! giving up after [`MAX_SKIPPED_FRAMES`] frames so a chatty peer cannot
//! stall a caller forever.
//!
//! # Peer identity
//!
//! A link has one peer and the wire carries no address, so the link names
//! the peer only when told: [`SerialLink::with_peer`] sets the
//! [`SenderId`] put in [`SampleMeta::sender`] of every received sample and
//! in [`RequestMeta::sender`] of every request a [`Responder::serve_one`]
//! handler sees (for example the node id a device reported in `sys/info`).
//! Without it both are `None`. The peer is not authenticated, so the
//! principal is always `None`, and the wire carries no deadline.
//!
//! # Inbox
//!
//! [`Requester::request_with`] and [`Responder::serve_one`] read the
//! stream until their frame arrives. Publications read meanwhile are kept,
//! in arrival order, in an inbox of `I` bytes inside the link (a const
//! generic, [`INBOX_BYTES`] by default: one maximal publication, or many
//! small ones; each costs its raw frame length plus
//! [`INBOX_ENTRY_OVERHEAD`]). Every receive ([`Subscriber::recv`],
//! [`Subscriber::try_recv`], a [`Filtered`] view, [`SerialLink::next_frame`])
//! returns queued publications before reading the stream again. A
//! publication that does not fit the inbox's free space is dropped and
//! counted ([`SerialLink::inbox_dropped`]); queued ones are kept. Other
//! frames read while waiting (stale responses, requests while a request
//! waits, responses while serving) are discarded, as before. `I = 0`
//! keeps nothing ([`SerialLink::with_inbox`]).
//!
//! # Delivery
//!
//! [`DELIVERY`] is [`Overflow::DropNewest`] with per-sender order: the
//! inbox drops the arriving publication when full; outside a request the
//! link reads the stream only when asked, so what an unread stream does is
//! the stream's: a flow-controlled one (USB CDC) makes the sender wait, a
//! UART without flow control drops the bytes that overrun its driver's
//! buffer (newest first; the CRC rejects a cut frame). Nothing older is
//! ever dropped for something newer. The link has no sequence numbers, so
//! [`SampleMeta::gap`] is always `None`.
//!
//! # Subscriptions and split
//!
//! A link is one point-to-point stream, so [`Subscriber`] on the link itself
//! yields every publication whatever its key. [`Subscribe::subscribe`]
//! borrows the link as a [`Filtered`] subscriber that keeps its own copy of
//! the [`KeyFilter`] (a [`KeyFilterBuf`], no heap) and yields only matching
//! keys, discarding the rest. The view borrows the whole link, so the link
//! cannot be used directly while it lives; drop it to publish or request.
//!
//! To publish while a receive waits, build the link over a [`Duplex`]
//! (a reading half and a writing half, as a UART driver's `split` returns)
//! and [`Split::split`] it into a [`SerialTx`] ([`Publisher`]) and a
//! [`SerialRx`] ([`Subscriber`], inbox first). The halves borrow disjoint
//! parts of the link (the writer with the transmit state, the reader with
//! the receive state and inbox): no shared ownership, no `RefCell`.
//!
//! # Cancel safety
//!
//! All receive state (the read chunk, its cursor, the frame splitter and the
//! inbox) and all transmit state (the encoded frame and how much of it was
//! written) live in `self`, so dropping a future at any `.await` loses
//! nothing, provided the stream's own `read` and `write` are cancel safe (a
//! dropped call has consumed or written no bytes it did not report):
//!
//! * [`Subscriber::recv`] (and [`SerialLink::next_frame`]) dropped mid-frame
//!   keeps the partial frame in the splitter; the next receive completes it.
//! * [`Publisher::publish`] / [`SerialLink::send`] dropped mid-write leaves
//!   the rest of the frame pending; the next send writes it out before its
//!   own frame, so the peer never sees a torn frame and nothing is sent twice.
//! * [`Requester::request`] dropped after sending abandons the request; its
//!   late response is discarded by the next request (correlation ids).
//!   Publications it had read stay in the inbox.
//! * [`Responder::serve_one`] keeps no state across calls besides the above.
//!
//! [`Subscriber::try_recv`] polls the stream's `read` once with a no-op waker
//! and drops the future if it is pending, so it relies on the same cancel
//! safety of `read`.

use super::cobs;
use super::frame::{
    self, FRAME_OVERHEAD, Frame, FrameError, MAX_RAW_FRAME, MAX_WIRE_FRAME, Split as Cut, Splitter,
};
use crate::key::{Key, KeyFilter, KeyFilterBuf, MAX_KEY_LEN};
use crate::transport::{
    Delivery, Handler, HandlerError, Limits, Overflow, Publisher, Request, RequestMeta,
    RequestOptions, Requester, Responder, Sample, SampleMeta, SenderId, Split, Subscribe,
    Subscriber,
};
use core::future::Future;
use core::pin::pin;
use core::task::{Context, Poll, Waker};
use embedded_io_async::{ErrorKind, ErrorType, Read, Write};

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
/// [`Subscriber::DELIVERY`]: the arriving publication is dropped when the
/// inbox is full (see the module docs, "Delivery"); frames arrive in the
/// order sent.
pub const DELIVERY: Delivery = Delivery::new(Overflow::DropNewest, true);

/// A receive buffer size that holds any publication [`Subscriber::recv`] or
/// any frame [`SerialLink::next_frame`] can return: a received frame is
/// copied into the caller's buffer in its raw (COBS-decoded) form.
pub const RECV_BUF_LEN: usize = MAX_RAW_FRAME;

/// Bytes an inbox entry costs beyond its raw frame (a 16-bit length).
pub const INBOX_ENTRY_OVERHEAD: usize = 2;

/// Default inbox size: one maximal publication.
pub const INBOX_BYTES: usize = MAX_RAW_FRAME + INBOX_ENTRY_OVERHEAD;

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

/// A byte stream made of a reading half `R` and a writing half `W`, such
/// as the two halves a UART or USB-Serial-JTAG driver's `split` returns. A
/// [`SerialLink`] over a `Duplex` implements [`Split`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Duplex<R, W> {
    /// The reading half.
    pub rx: R,
    /// The writing half.
    pub tx: W,
}

impl<R, W> Duplex<R, W> {
    /// Joins a reading and a writing half.
    pub const fn new(rx: R, tx: W) -> Self {
        Duplex { rx, tx }
    }
}

/// Failure of a [`Duplex`] stream: of its reading or of its writing half.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DuplexError<R, W> {
    /// The reading half failed.
    Read(R),
    /// The writing half failed.
    Write(W),
}

impl<R: core::fmt::Debug, W: core::fmt::Debug> core::fmt::Display for DuplexError<R, W> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DuplexError::Read(e) => write!(f, "read: {e:?}"),
            DuplexError::Write(e) => write!(f, "write: {e:?}"),
        }
    }
}

impl<R: core::fmt::Debug, W: core::fmt::Debug> core::error::Error for DuplexError<R, W> {}

impl<R: embedded_io_async::Error, W: embedded_io_async::Error> embedded_io_async::Error
    for DuplexError<R, W>
{
    fn kind(&self) -> ErrorKind {
        match self {
            DuplexError::Read(e) => e.kind(),
            DuplexError::Write(e) => e.kind(),
        }
    }
}

impl<R: ErrorType, W: ErrorType> ErrorType for Duplex<R, W> {
    type Error = DuplexError<R::Error, W::Error>;
}

impl<R: Read, W: Write> Read for Duplex<R, W> {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        self.rx.read(buf).await.map_err(DuplexError::Read)
    }
}

impl<R: Read, W: Write> Write for Duplex<R, W> {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        self.tx.write(buf).await.map_err(DuplexError::Write)
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        self.tx.flush().await.map_err(DuplexError::Write)
    }
}

/// Publications kept while a request or `serve_one` waits: raw publish
/// frames, each behind a little-endian `u16` length, oldest first, in `I`
/// bytes. Mutated in place, like the [`Splitter`]: it holds up to `I`
/// bytes and is updated per frame.
struct Inbox<const I: usize> {
    buf: [u8; I],
    used: usize,
    dropped: u32,
}

impl<const I: usize> Inbox<I> {
    const fn new() -> Self {
        Inbox {
            buf: [0; I],
            used: 0,
            dropped: 0,
        }
    }

    /// Appends `raw`, or drops and counts it when it does not fit.
    fn push(&mut self, raw: &[u8]) {
        let Ok(len) = u16::try_from(raw.len()) else {
            self.dropped = self.dropped.saturating_add(1);
            return;
        };
        let end = self.used + INBOX_ENTRY_OVERHEAD + raw.len();
        match self.buf.get_mut(self.used..end) {
            Some(slot) => {
                let (head, body) = slot.split_at_mut(INBOX_ENTRY_OVERHEAD);
                head.copy_from_slice(&len.to_le_bytes());
                body.copy_from_slice(raw);
                self.used = end;
            }
            None => self.dropped = self.dropped.saturating_add(1),
        }
    }

    /// The oldest entry's raw frame.
    fn front(&self) -> Option<&[u8]> {
        let head = self.buf.get(..INBOX_ENTRY_OVERHEAD.min(self.used))?;
        let [lo, hi] = *head else { return None };
        let len = usize::from(u16::from_le_bytes([lo, hi]));
        self.buf
            .get(INBOX_ENTRY_OVERHEAD..INBOX_ENTRY_OVERHEAD + len)
    }

    /// Removes the oldest entry (bounded by `I` bytes moved).
    fn pop(&mut self) {
        let Some(len) = self.front().map(<[u8]>::len) else {
            return;
        };
        let next = INBOX_ENTRY_OVERHEAD + len;
        self.buf.copy_within(next..self.used, 0);
        self.used -= next;
    }

    /// Entries queued (bounded: every entry is at least
    /// [`INBOX_ENTRY_OVERHEAD`] bytes).
    fn len(&self) -> usize {
        let mut at = 0;
        let mut n = 0;
        for _ in 0..=I / INBOX_ENTRY_OVERHEAD {
            let head = self.buf.get(at..(at + INBOX_ENTRY_OVERHEAD).min(self.used));
            let Some(Ok(head)) = head.map(<[u8; 2]>::try_from) else {
                break;
            };
            at += INBOX_ENTRY_OVERHEAD + usize::from(u16::from_le_bytes(head));
            n += 1;
        }
        n
    }
}

/// Receive state: the read chunk, the splitter (whose current run is
/// decoded in place once complete) and the inbox.
struct RxState<const I: usize> {
    splitter: Splitter<MAX_WIRE_FRAME>,
    chunk: [u8; RX_CHUNK],
    pos: usize,
    len: usize,
    /// Raw length of the splitter's current, valid frame.
    raw: usize,
    rejected: u32,
    inbox: Inbox<I>,
}

/// Transmit state: the encoded frame, how much of it was written, and the
/// next correlation id.
struct TxState {
    scratch: [u8; MAX_RAW_FRAME],
    wire: [u8; MAX_WIRE_FRAME],
    pos: usize,
    end: usize,
    next_id: u8,
}

/// Middleware endpoint over a byte stream, with an inbox of `I` bytes (see
/// the module docs).
pub struct SerialLink<IO, const I: usize = INBOX_BYTES> {
    io: IO,
    rx: RxState<I>,
    tx: TxState,
    peer: Option<SenderId>,
}

impl<IO> SerialLink<IO> {
    /// Wraps a stream, with the default inbox ([`INBOX_BYTES`]).
    pub fn new(io: IO) -> Self {
        Self::with_inbox(io)
    }
}

impl<IO, const I: usize> SerialLink<IO, I> {
    /// Wraps a stream, with an inbox of `I` bytes:
    /// `SerialLink::<_, 0>::with_inbox(io)` keeps no publication that
    /// arrives while a request waits.
    pub fn with_inbox(io: IO) -> Self {
        Self {
            io,
            rx: RxState {
                splitter: Splitter::new(),
                chunk: [0; RX_CHUNK],
                pos: 0,
                len: 0,
                raw: 0,
                rejected: 0,
                inbox: Inbox::new(),
            },
            tx: TxState {
                scratch: [0; MAX_RAW_FRAME],
                wire: [0; MAX_WIRE_FRAME],
                pos: 0,
                end: 0,
                next_id: 0,
            },
            peer: None,
        }
    }

    /// The same link naming its peer: received samples and served requests
    /// carry `peer` as their sender (see the module docs).
    pub fn with_peer(self, peer: SenderId) -> Self {
        SerialLink {
            peer: Some(peer),
            ..self
        }
    }

    /// The peer named by [`SerialLink::with_peer`].
    pub const fn peer(&self) -> Option<SenderId> {
        self.peer
    }

    /// Delimited byte runs that were not valid frames (noise, ROM banners,
    /// overflow), since creation.
    pub fn rejected(&self) -> u32 {
        self.rx.rejected
    }

    /// Publications queued in the inbox (read while a request waited).
    pub fn inbox_len(&self) -> usize {
        self.rx.inbox.len()
    }

    /// Publications dropped because the inbox was full, since creation.
    pub fn inbox_dropped(&self) -> u32 {
        self.rx.inbox.dropped
    }

    /// Returns the stream. Bytes of a frame whose send was cancelled and not
    /// yet completed are discarded, as are queued publications.
    pub fn into_inner(self) -> IO {
        self.io
    }

    fn take_id(&mut self) -> u8 {
        let id = self.tx.next_id;
        self.tx.next_id = id.wrapping_add(1);
        id
    }
}

impl<const I: usize> RxState<I> {
    /// Feeds buffered bytes to the splitter until a delimited run that
    /// decodes is complete (decoded in place, its raw length kept in `raw`)
    /// or the chunk is exhausted. Bounded by [`RX_CHUNK`].
    fn pump(&mut self) -> bool {
        while self.pos < self.len {
            let byte = self.chunk.get(self.pos).copied().unwrap_or(0);
            self.pos += 1;
            match self.splitter.push(byte) {
                Cut::Pending => {}
                Cut::Overflow => self.rejected = self.rejected.saturating_add(1),
                Cut::Frame(_) => {
                    let run = self.splitter.frame();
                    let valid = cobs::decode_in_place(run)
                        .filter(|&n| run.get(..n).is_some_and(|raw| frame::decode(raw).is_ok()));
                    match valid {
                        Some(n) => {
                            self.raw = n;
                            return true;
                        }
                        None => self.rejected = self.rejected.saturating_add(1),
                    }
                }
            }
        }
        false
    }

    /// Records a completed read of `n` bytes into the chunk.
    fn filled<E>(&mut self, n: usize) -> Result<(), LinkError<E>> {
        if n == 0 {
            return Err(LinkError::Closed);
        }
        self.pos = 0;
        self.len = n.min(RX_CHUNK);
        Ok(())
    }

    /// The raw bytes of the current frame (valid after `pump` returned true).
    fn raw(&mut self) -> &[u8] {
        let raw = self.raw;
        self.splitter.frame().get(..raw).unwrap_or_default()
    }

    /// The current frame, decoded.
    fn current(&mut self) -> Result<Frame<'_>, FrameError> {
        frame::decode(self.raw())
    }

    /// Queues the current frame if it is a publication.
    fn keep_if_publish(&mut self) {
        if matches!(self.current(), Ok(Frame::Publish { .. })) {
            let raw = self.raw;
            let run = self.splitter.frame();
            if let Some(bytes) = run.get(..raw) {
                self.inbox.push(bytes);
            }
        }
    }

    /// Waits for the next valid frame, leaving it decoded in the splitter.
    async fn next<R: Read>(&mut self, io: &mut R) -> Result<(), LinkError<R::Error>> {
        loop {
            if self.pump() {
                return Ok(());
            }
            let n = io.read(&mut self.chunk).await.map_err(LinkError::Io)?;
            self.filled(n)?;
        }
    }

    /// Like [`Self::next`] but never waits: polls `read` once per round
    /// with a no-op waker and stops at the first pending read.
    fn try_next<R: Read>(&mut self, io: &mut R) -> Result<bool, LinkError<R::Error>> {
        for _ in 0..TRY_RECV_READS {
            if self.pump() {
                return Ok(true);
            }
            let polled = {
                let read = pin!(io.read(&mut self.chunk));
                read.poll(&mut Context::from_waker(Waker::noop()))
            };
            match polled {
                Poll::Pending => return Ok(false),
                Poll::Ready(r) => self.filled(r.map_err(LinkError::Io)?)?,
            }
        }
        Ok(self.pump())
    }

    /// Discards queued publications `filter` does not select, up to the
    /// first one it does; true if there is one (bounded by the inbox size).
    fn queued_selected(&mut self, filter: Option<KeyFilter<'_>>) -> bool {
        for _ in 0..=I / INBOX_ENTRY_OVERHEAD {
            let Some(raw) = self.inbox.front() else {
                return false;
            };
            match frame::decode(raw) {
                Ok(Frame::Publish { key, .. }) if filter.is_none_or(|f| f.matches(key)) => {
                    return true;
                }
                _ => self.inbox.pop(),
            }
        }
        false
    }

    /// Copies the oldest queued publication into `buf` as a sample and
    /// removes it; a `buf` too small keeps it, so a retry with a larger
    /// buffer returns it.
    fn take_queued<'b, E>(
        &mut self,
        buf: &'b mut [u8],
        peer: Option<SenderId>,
    ) -> Result<Sample<'b>, LinkError<E>> {
        let raw = self
            .inbox
            .front()
            .ok_or(LinkError::Frame(FrameError::Malformed))?;
        let dst = buf.get_mut(..raw.len()).ok_or(LinkError::BufferTooSmall)?;
        dst.copy_from_slice(raw);
        self.inbox.pop();
        publication(dst, peer)
    }

    /// Copies the current frame (a publication) into `buf` as a sample.
    fn take_current<'b, E>(
        &mut self,
        buf: &'b mut [u8],
        peer: Option<SenderId>,
    ) -> Result<Sample<'b>, LinkError<E>> {
        let raw = self.raw();
        let dst = buf.get_mut(..raw.len()).ok_or(LinkError::BufferTooSmall)?;
        dst.copy_from_slice(raw);
        publication(dst, peer)
    }

    /// True if the current frame is a publication selected by `filter`.
    fn current_selected(&mut self, filter: Option<KeyFilter<'_>>) -> bool {
        match self.current() {
            Ok(Frame::Publish { key, .. }) => filter.is_none_or(|f| f.matches(key)),
            _ => false,
        }
    }

    /// Waits for the next publication selected by `filter`, inbox first.
    async fn recv_matching<'b, R: Read>(
        &mut self,
        io: &mut R,
        filter: Option<KeyFilter<'_>>,
        buf: &'b mut [u8],
        peer: Option<SenderId>,
    ) -> Result<Sample<'b>, LinkError<R::Error>> {
        if self.queued_selected(filter) {
            return self.take_queued(buf, peer);
        }
        for _ in 0..MAX_SKIPPED_FRAMES {
            self.next(io).await?;
            if self.current_selected(filter) {
                return self.take_current(buf, peer);
            }
        }
        Err(LinkError::SkipLimit)
    }

    /// Returns an already received publication selected by `filter`, if
    /// any, inbox first.
    fn try_recv_matching<'b, R: Read>(
        &mut self,
        io: &mut R,
        filter: Option<KeyFilter<'_>>,
        buf: &'b mut [u8],
        peer: Option<SenderId>,
    ) -> Result<Option<Sample<'b>>, LinkError<R::Error>> {
        if self.queued_selected(filter) {
            return self.take_queued(buf, peer).map(Some);
        }
        for _ in 0..MAX_SKIPPED_FRAMES {
            if !self.try_next(io)? {
                return Ok(None);
            }
            if self.current_selected(filter) {
                return self.take_current(buf, peer).map(Some);
            }
        }
        Ok(None)
    }
}

/// Decodes the raw publication in `raw` (already copied into the caller's
/// buffer) as a sample from `peer`.
fn publication<E>(raw: &[u8], peer: Option<SenderId>) -> Result<Sample<'_>, LinkError<E>> {
    match frame::decode(raw) {
        Ok(Frame::Publish { key, payload }) => {
            Ok(Sample::new(key, payload).with_meta(SampleMeta {
                sender: peer,
                ..SampleMeta::NONE
            }))
        }
        _ => Err(LinkError::Frame(FrameError::Malformed)),
    }
}

impl TxState {
    /// Writes the not yet written part of the encoded frame, then flushes.
    /// Each write advances the cursor or fails, so the loop is bounded by
    /// [`MAX_WIRE_FRAME`].
    async fn write_pending<W: Write>(&mut self, io: &mut W) -> Result<(), LinkError<W::Error>> {
        if self.pos == self.end {
            return Ok(());
        }
        while self.pos < self.end {
            let chunk = self
                .wire
                .get(self.pos..self.end)
                .ok_or(LinkError::Frame(FrameError::TooLarge))?;
            let n = io.write(chunk).await.map_err(LinkError::Io)?;
            if n == 0 {
                return Err(LinkError::Closed);
            }
            self.pos = (self.pos + n).min(self.end);
        }
        io.flush().await.map_err(LinkError::Io)
    }

    /// Encodes and writes one frame, then flushes; the rest of a frame
    /// whose send was cancelled is written first.
    async fn send<W: Write>(
        &mut self,
        io: &mut W,
        frame: &Frame<'_>,
    ) -> Result<(), LinkError<W::Error>> {
        self.write_pending(io).await?;
        let n =
            frame::encode(frame, &mut self.scratch, &mut self.wire).map_err(LinkError::Frame)?;
        self.pos = 0;
        self.end = n;
        self.write_pending(io).await
    }
}

impl<IO: Read + Write, const I: usize> SerialLink<IO, I> {
    /// Encodes and writes one frame, then flushes. The rest of a frame whose
    /// send was cancelled is written first.
    pub async fn send(&mut self, frame: &Frame<'_>) -> Result<(), LinkError<IO::Error>> {
        self.tx.send(&mut self.io, frame).await
    }

    /// Waits for the next valid frame and decodes it into `buf` (raw form,
    /// [`RECV_BUF_LEN`] always suffices). Publications queued in the inbox
    /// come first. Invalid byte runs are counted in
    /// [`SerialLink::rejected`] and skipped.
    pub async fn next_frame<'b>(
        &mut self,
        buf: &'b mut [u8],
    ) -> Result<Frame<'b>, LinkError<IO::Error>> {
        if let Some(raw) = self.rx.inbox.front() {
            let dst = buf.get_mut(..raw.len()).ok_or(LinkError::BufferTooSmall)?;
            dst.copy_from_slice(raw);
            self.rx.inbox.pop();
            return frame::decode(dst).map_err(LinkError::Frame);
        }
        self.rx.next(&mut self.io).await?;
        let raw = self.rx.raw();
        let dst = buf.get_mut(..raw.len()).ok_or(LinkError::BufferTooSmall)?;
        dst.copy_from_slice(raw);
        // `next` only stops on frames that decode, so this succeeds.
        frame::decode(dst).map_err(LinkError::Frame)
    }
}

impl<IO: Read + Write, const I: usize> Publisher for SerialLink<IO, I> {
    type Error = LinkError<IO::Error>;

    /// See [`LIMITS`]: a 128-byte key leaves room for 891 payload bytes.
    const LIMITS: Limits = LIMITS;

    /// Sends a publish frame. Cancelling may leave the frame partly written;
    /// the next send completes it (see the module docs).
    async fn publish(&mut self, key: Key<'_>, payload: &[u8]) -> Result<(), Self::Error> {
        self.send(&Frame::Publish { key, payload }).await
    }
}

impl<IO: Read + Write, const I: usize> Subscriber for SerialLink<IO, I> {
    type Error = LinkError<IO::Error>;

    /// See [`DELIVERY`].
    const DELIVERY: Delivery = DELIVERY;
    const LIMITS: Limits = LIMITS;

    /// Yields the next publication, whatever its key: the inbox first, then
    /// the stream, where other frame kinds are discarded. Cancel safe: a
    /// partial frame stays in the splitter. `buf` must hold the raw frame
    /// (key, payload and 5 bytes; [`RECV_BUF_LEN`] always suffices); a
    /// smaller buffer is [`LinkError::BufferTooSmall`], which keeps a queued
    /// publication and drops one read from the stream. The sample's sender
    /// is the peer ([`SerialLink::with_peer`]); it has no sequence or gap.
    async fn recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Sample<'b>, Self::Error> {
        self.rx
            .recv_matching(&mut self.io, None, buf, self.peer)
            .await
    }

    /// Returns a publication available without waiting: the inbox, bytes
    /// left in the read chunk, then `read` polled once per chunk with a
    /// no-op waker.
    fn try_recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Sample<'b>>, Self::Error> {
        self.rx
            .try_recv_matching(&mut self.io, None, buf, self.peer)
    }
}

impl<IO: Read + Write, const I: usize> Subscribe for SerialLink<IO, I> {
    type Subscriber<'a>
        = Filtered<'a, IO, I>
    where
        Self: 'a;
    type Error = LinkError<IO::Error>;

    /// Borrows the link as a [`Filtered`] subscriber holding a copy of
    /// `filter`. Never waits and never fails: every valid key fits the
    /// view's [`MAX_KEY_LEN`]-byte filter buffer.
    async fn subscribe<'a>(
        &'a mut self,
        filter: KeyFilter<'_>,
    ) -> Result<Filtered<'a, IO, I>, Self::Error> {
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
/// other keys (queued or read), and frames of other kinds, are discarded.
pub struct Filtered<'l, IO, const I: usize = INBOX_BYTES> {
    link: &'l mut SerialLink<IO, I>,
    filter: KeyFilterBuf<MAX_KEY_LEN>,
}

impl<IO, const I: usize> Filtered<'_, IO, I> {
    /// The filter this view applies.
    pub fn filter(&self) -> KeyFilter<'_> {
        self.filter.as_filter()
    }
}

impl<IO: Read + Write, const I: usize> Subscriber for Filtered<'_, IO, I> {
    type Error = LinkError<IO::Error>;

    /// See [`DELIVERY`].
    const DELIVERY: Delivery = DELIVERY;
    const LIMITS: Limits = LIMITS;

    /// As [`SerialLink`]'s `recv`, skipping keys the filter rejects (they
    /// count towards [`MAX_SKIPPED_FRAMES`]).
    async fn recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Sample<'b>, Self::Error> {
        let link = &mut *self.link;
        link.rx
            .recv_matching(&mut link.io, Some(self.filter.as_filter()), buf, link.peer)
            .await
    }

    /// As [`SerialLink`]'s `try_recv`, skipping keys the filter rejects.
    fn try_recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Sample<'b>>, Self::Error> {
        let link = &mut *self.link;
        link.rx
            .try_recv_matching(&mut link.io, Some(self.filter.as_filter()), buf, link.peer)
    }
}

impl<IO: Read + Write, const I: usize> Requester for SerialLink<IO, I> {
    type Error = LinkError<IO::Error>;

    /// Sends a request and waits for the response with the same id.
    /// Publications read meanwhile go to the inbox; other frames, including
    /// stale responses, are discarded. Cancelling abandons the request; its
    /// late response is discarded by the next request.
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
            self.rx.next(&mut self.io).await?;
            match self.rx.current() {
                Ok(Frame::Response { id: got, result }) if got == id => {
                    let body = result.map_err(LinkError::Handler)?;
                    let dst = response
                        .get_mut(..body.len())
                        .ok_or(LinkError::BufferTooSmall)?;
                    dst.copy_from_slice(body);
                    return Ok(dst);
                }
                _ => self.rx.keep_if_publish(),
            }
        }
        Err(LinkError::SkipLimit)
    }
}

impl<IO: Read + Write, const I: usize> Responder for SerialLink<IO, I> {
    type Error = LinkError<IO::Error>;

    /// Waits for one request, runs `handler`, and sends the response.
    /// Publications read meanwhile go to the inbox; responses are discarded.
    /// The request is copied into the front of `scratch` (raw form); the
    /// response is built in the remainder. The handler sees the peer
    /// ([`SerialLink::with_peer`]) as `RequestMeta::sender`, no principal
    /// and no deadline. Cancelling while waiting loses nothing; cancelling
    /// while the response is written leaves it pending for the next send.
    async fn serve_one<H: Handler>(
        &mut self,
        handler: &mut H,
        scratch: &mut [u8],
    ) -> Result<(), Self::Error> {
        let mut found = false;
        for _ in 0..MAX_SKIPPED_FRAMES {
            self.rx.next(&mut self.io).await?;
            if let Ok(Frame::Request { .. }) = self.rx.current() {
                found = true;
                break;
            }
            self.rx.keep_if_publish();
        }
        if !found {
            return Err(LinkError::SkipLimit);
        }
        let raw = self.rx.raw();
        let (req_buf, resp_buf) = scratch
            .split_at_mut_checked(raw.len())
            .ok_or(LinkError::BufferTooSmall)?;
        req_buf.copy_from_slice(raw);
        let Ok(Frame::Request { id, key, payload }) = frame::decode(req_buf) else {
            return Err(LinkError::Frame(FrameError::Malformed));
        };
        let meta = RequestMeta {
            sender: self.peer,
            ..RequestMeta::NONE
        };
        let result = handler
            .handle(&Request::new(key, payload).with_meta(meta), resp_buf)
            .and_then(|n| resp_buf.get(..n).ok_or(HandlerError::BufferTooSmall));
        self.send(&Frame::Response { id, result }).await
    }
}

/// The publishing half of a [`SerialLink`] over a [`Duplex`], made by
/// [`Split::split`]: the writer and the transmit state.
pub struct SerialTx<'l, W> {
    io: &'l mut W,
    tx: &'l mut TxState,
}

impl<W: Write> SerialTx<'_, W> {
    /// Encodes and writes one frame, as [`SerialLink::send`].
    pub async fn send(&mut self, frame: &Frame<'_>) -> Result<(), LinkError<W::Error>> {
        self.tx.send(self.io, frame).await
    }
}

impl<W: Write> Publisher for SerialTx<'_, W> {
    type Error = LinkError<W::Error>;

    /// See [`LIMITS`].
    const LIMITS: Limits = LIMITS;

    /// As [`SerialLink`]'s `publish`.
    async fn publish(&mut self, key: Key<'_>, payload: &[u8]) -> Result<(), Self::Error> {
        self.send(&Frame::Publish { key, payload }).await
    }
}

/// The receiving half of a [`SerialLink`] over a [`Duplex`], made by
/// [`Split::split`]: the reader, the receive state and the inbox. Yields
/// every publication, inbox first, like the link's own [`Subscriber`].
pub struct SerialRx<'l, R, const I: usize = INBOX_BYTES> {
    io: &'l mut R,
    rx: &'l mut RxState<I>,
    peer: Option<SenderId>,
}

impl<R: Read, const I: usize> Subscriber for SerialRx<'_, R, I> {
    type Error = LinkError<R::Error>;

    /// See [`DELIVERY`].
    const DELIVERY: Delivery = DELIVERY;
    const LIMITS: Limits = LIMITS;

    /// As [`SerialLink`]'s `recv`.
    async fn recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Sample<'b>, Self::Error> {
        self.rx.recv_matching(self.io, None, buf, self.peer).await
    }

    /// As [`SerialLink`]'s `try_recv`.
    fn try_recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Sample<'b>>, Self::Error> {
        self.rx.try_recv_matching(self.io, None, buf, self.peer)
    }
}

impl<R: Read, W: Write, const I: usize> Split for SerialLink<Duplex<R, W>, I> {
    type Tx<'a>
        = SerialTx<'a, W>
    where
        Self: 'a;
    type Rx<'a>
        = SerialRx<'a, R, I>
    where
        Self: 'a;

    /// Borrows the writer with the transmit state and the reader with the
    /// receive state: a task can `publish` on the first while another waits
    /// in `recv` on the second.
    fn split(&mut self) -> (SerialTx<'_, W>, SerialRx<'_, R, I>) {
        (
            SerialTx {
                io: &mut self.io.tx,
                tx: &mut self.tx,
            },
            SerialRx {
                io: &mut self.io.rx,
                rx: &mut self.rx,
                peer: self.peer,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::transport::{HandlerError, SampleMeta};
    use core::cell::Cell;
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

    const PEER: SenderId = SenderId([0xA5; 8]);

    fn publish(key: &'static str, payload: &'static [u8]) -> Frame<'static> {
        Frame::Publish {
            key: Key::from_static(key),
            payload,
        }
    }

    fn raw_len(f: &Frame<'_>) -> usize {
        let mut raw = [0u8; MAX_RAW_FRAME];
        frame::encode_raw(f, &mut raw).unwrap()
    }

    #[test]
    fn s8_publications_during_request_are_kept_in_order() {
        let input = wire(&[
            publish("app/a", b"1"),
            Frame::Response {
                id: 7,
                result: Ok(b"stale"),
            },
            publish("app/b", b"2"),
            Frame::Response {
                id: 0,
                result: Ok(b"pong"),
            },
            publish("app/c", b"3"),
        ]);
        let mut link = SerialLink::new(Pipe::new(input));
        let mut resp = [0u8; 16];
        assert_eq!(
            block_on(link.request(K, b"ping", &mut resp)).unwrap(),
            b"pong"
        );
        assert_eq!((link.inbox_len(), link.inbox_dropped()), (2, 0));
        let mut buf = [0u8; RECV_BUF_LEN];
        let s = block_on(link.recv(&mut buf)).unwrap();
        assert_eq!((s.key.as_str(), s.payload), ("app/a", &b"1"[..]));
        let s = link.try_recv(&mut buf).unwrap().expect("queued");
        assert_eq!((s.key.as_str(), s.payload), ("app/b", &b"2"[..]));
        assert_eq!(link.inbox_len(), 0);
        // Then the stream again.
        let s = link.try_recv(&mut buf).unwrap().expect("read");
        assert_eq!(s.key.as_str(), "app/c");
    }

    #[test]
    fn s8_publications_during_serve_one_are_kept() {
        let input = wire(&[
            publish("app/a", b"1"),
            Frame::Response {
                id: 1,
                result: Ok(b"ignored"),
            },
            Frame::Request {
                id: 5,
                key: K,
                payload: b"q",
            },
        ]);
        let mut link = SerialLink::new(Pipe::new(input));
        let mut h = crate::transport::handler_fn(|_r, _out| Ok(0));
        let mut scratch = [0u8; 64];
        block_on(link.serve_one(&mut h, &mut scratch)).unwrap();
        let mut buf = [0u8; RECV_BUF_LEN];
        let s = block_on(link.recv(&mut buf)).unwrap();
        assert_eq!(s.key.as_str(), "app/a");
    }

    #[test]
    fn s8_full_inbox_drops_the_newest_and_counts() {
        let a = publish("app/a", b"first");
        let b = publish("app/b", b"second");
        let room = raw_len(&a) + INBOX_ENTRY_OVERHEAD;
        let input = wire(&[
            a,
            b,
            Frame::Response {
                id: 0,
                result: Ok(b""),
            },
        ]);
        // Room for exactly the first publication.
        type Small = SerialLink<Pipe, 17>;
        assert_eq!(room, 17);
        let mut link = Small::with_inbox(Pipe::new(input));
        let mut resp = [0u8; 4];
        block_on(link.request(K, b"", &mut resp)).unwrap();
        assert_eq!((link.inbox_len(), link.inbox_dropped()), (1, 1));
        let mut buf = [0u8; RECV_BUF_LEN];
        let s = link.try_recv(&mut buf).unwrap().expect("kept");
        assert_eq!(s.payload, b"first");
        assert_eq!(link.try_recv(&mut buf), Err(LinkError::Closed));
    }

    #[test]
    fn s8_zero_inbox_keeps_nothing() {
        let input = wire(&[
            publish("app/a", b"1"),
            Frame::Response {
                id: 0,
                result: Ok(b""),
            },
        ]);
        let mut link = SerialLink::<_, 0>::with_inbox(Pipe::new(input));
        let mut resp = [0u8; 4];
        block_on(link.request(K, b"", &mut resp)).unwrap();
        assert_eq!((link.inbox_len(), link.inbox_dropped()), (0, 1));
    }

    #[test]
    fn s8_small_buffer_keeps_a_queued_publication() {
        let input = wire(&[
            publish("app/a", b"payload"),
            Frame::Response {
                id: 0,
                result: Ok(b""),
            },
        ]);
        let mut link = SerialLink::new(Pipe::new(input));
        let mut resp = [0u8; 4];
        block_on(link.request(K, b"", &mut resp)).unwrap();
        let mut small = [0u8; 4];
        assert_eq!(link.try_recv(&mut small), Err(LinkError::BufferTooSmall));
        let mut buf = [0u8; RECV_BUF_LEN];
        assert_eq!(
            link.try_recv(&mut buf).unwrap().unwrap().payload,
            b"payload"
        );
    }

    #[test]
    fn s8_filtered_view_and_next_frame_take_the_inbox_first() {
        let input = wire(&[
            publish("other", b"x"),
            publish("app/a", b"1"),
            publish("app/b", b"2"),
            Frame::Response {
                id: 0,
                result: Ok(b""),
            },
            publish("app/c", b"3"),
        ]);
        let mut link = SerialLink::new(Pipe::new(input));
        let mut resp = [0u8; 4];
        block_on(link.request(K, b"", &mut resp)).unwrap();
        assert_eq!(link.inbox_len(), 3);
        let mut buf = [0u8; RECV_BUF_LEN];
        {
            let mut sub =
                block_on(link.subscribe(KeyFilter::Prefix(Key::from_static("app")))).unwrap();
            // `other` is discarded, `app/a` delivered.
            let s = block_on(sub.recv(&mut buf)).unwrap();
            assert_eq!(s.key.as_str(), "app/a");
        }
        assert_eq!(link.inbox_len(), 1);
        assert_eq!(
            block_on(link.next_frame(&mut buf)).unwrap(),
            publish("app/b", b"2")
        );
        assert_eq!(
            block_on(link.next_frame(&mut buf)).unwrap(),
            publish("app/c", b"3")
        );
    }

    #[test]
    fn s9_peer_is_the_sender_of_samples_and_requests() {
        let input = wire(&[
            publish("app/a", b"1"),
            Frame::Request {
                id: 1,
                key: K,
                payload: b"",
            },
        ]);
        let mut link = SerialLink::new(Pipe::new(input)).with_peer(PEER);
        assert_eq!(link.peer(), Some(PEER));
        let mut buf = [0u8; RECV_BUF_LEN];
        let s = block_on(link.recv(&mut buf)).unwrap();
        assert_eq!(s.meta, SampleMeta::from_sender(PEER));
        let seen = Cell::new(None);
        let mut h = crate::transport::handler_fn(|r, _out| {
            seen.set(Some((
                r.meta.sender,
                r.meta.principal.is_none(),
                r.meta.deadline_ms,
            )));
            Ok(0)
        });
        let mut scratch = [0u8; 64];
        block_on(link.serve_one(&mut h, &mut scratch)).unwrap();
        assert_eq!(seen.get(), Some((Some(PEER), true, None)));
    }

    #[test]
    fn s9_without_a_peer_meta_is_empty() {
        let input = wire(&[Frame::Request {
            id: 1,
            key: K,
            payload: b"",
        }]);
        let mut link = SerialLink::new(Pipe::new(input));
        let seen = Cell::new(None);
        let mut h = crate::transport::handler_fn(|r, _out| {
            seen.set(Some((
                r.meta.sender,
                r.meta.principal.is_none(),
                r.meta.deadline_ms,
            )));
            Ok(0)
        });
        let mut scratch = [0u8; 64];
        block_on(link.serve_one(&mut h, &mut scratch)).unwrap();
        assert_eq!(seen.get(), Some((None, true, None)));
    }

    #[test]
    fn k11_split_halves_publish_while_a_receive_waits() {
        let rx = Pipe::open();
        let tx = Pipe::new(Vec::new());
        let mut link = SerialLink::new(Duplex::new(rx, tx)).with_peer(PEER);
        let mut buf = [0u8; RECV_BUF_LEN];
        {
            let (mut tx, mut rx) = link.split();
            let mut recv = pin!(rx.recv(&mut buf));
            let mut cx = Context::from_waker(Waker::noop());
            assert!(
                recv.as_mut().poll(&mut cx).is_pending(),
                "nothing to read yet"
            );
            // The publishing half is usable while the receive is pending.
            block_on(tx.publish(K, b"out")).unwrap();
            assert!(recv.as_mut().poll(&mut cx).is_pending());
        }
        // The halves wrote and read through the one link.
        let out = decode_all(&link.io.tx.output);
        assert_eq!(out.len(), 1);
        link.io.rx.input.extend(wire(&[publish("app/in", b"in")]));
        let (_tx, mut rx) = link.split();
        let s = block_on(rx.recv(&mut buf)).unwrap();
        assert_eq!(
            (s.key.as_str(), s.payload, s.meta.sender),
            ("app/in", &b"in"[..], Some(PEER))
        );
        assert_eq!(rx.try_recv(&mut buf), Ok(None));
    }

    #[test]
    fn k11_split_rx_drains_the_inbox_first() {
        let input = wire(&[
            publish("app/a", b"1"),
            Frame::Response {
                id: 0,
                result: Ok(b""),
            },
            publish("app/b", b"2"),
        ]);
        let mut link = SerialLink::new(Duplex::new(Pipe::new(input), Pipe::new(Vec::new())));
        let mut resp = [0u8; 4];
        block_on(link.request(K, b"", &mut resp)).unwrap();
        let (_tx, mut rx) = link.split();
        let mut buf = [0u8; RECV_BUF_LEN];
        assert_eq!(block_on(rx.recv(&mut buf)).unwrap().key.as_str(), "app/a");
        assert_eq!(block_on(rx.recv(&mut buf)).unwrap().key.as_str(), "app/b");
    }

    #[test]
    fn duplex_errors_name_their_half() {
        use embedded_io_async::Error as _;
        let e: DuplexError<ErrorKind, ErrorKind> = DuplexError::Write(ErrorKind::TimedOut);
        assert_eq!(e.kind(), ErrorKind::TimedOut);
        assert_eq!(std::format!("{e}"), "write: TimedOut");
        let e: DuplexError<ErrorKind, ErrorKind> = DuplexError::Read(ErrorKind::Other);
        assert_eq!(e.kind(), ErrorKind::Other);
    }

    #[test]
    fn delivery_is_drop_newest_in_order() {
        assert_eq!(DELIVERY, Delivery::new(Overflow::DropNewest, true));
        assert_eq!(<SerialLink<Pipe> as Subscriber>::DELIVERY, DELIVERY);
        assert_eq!(INBOX_BYTES, MAX_RAW_FRAME + 2);
    }
}
