//! An [`EspNowRadio`] and [`EspNowUnicast`] over the USB↔ESP-NOW bridge
//! (`tools/espnow-bridge/esp8266`): [`BridgeRadio`] speaks the serial
//! framing of [`crate::radio_bridge`] over any `embedded-io-async`
//! [`Read`] + [`Write`] stream. No heap. EG3–EG5.
//!
//! # Receiving
//!
//! [`BridgeRadio::receive`] yields the bridge's `Recv` frames. `Hello` and
//! `Stats` are kept ([`BridgeRadio::own_mac`], [`BridgeRadio::bridge_stats`]),
//! `Sent` completes the send in progress, and noise (the ESP8266 boot
//! banner, undecodable runs) is counted ([`BridgeCounters`]). Frames read
//! while a send waits for its `Sent` go into a bounded inline ring of `Q`
//! frames; when it is full the newest is dropped and counted, as the
//! transport's [`super::DELIVERY`] rule says.
//!
//! [`EspNowRadio::try_receive`] pops that ring only: it cannot poll the
//! stream without waiting, so "queued" means already read from the bridge.
//!
//! # Sending
//!
//! `send_to` and `broadcast` (to [`BROADCAST`]) write one `Send` and wait
//! for the bridge's `Sent` for that destination: acknowledged is `Ok`, a
//! failed `Sent` is [`BridgeIoError::NotAcked`], and none within the send
//! timeout (default [`SENT_TIMEOUT_MS`]) is [`BridgeIoError::SentTimeout`]
//! (the frame may well have gone out; [`super::rpc::RpcRequester`] resends
//! with the same id and the responder answers a repeat from its cache).
//! Only one `Send` is outstanding at a time, which keeps within the
//! bridge's four-slot send queue. A `Sent` for another destination is
//! ignored; a late `Sent` for the same destination from a cancelled
//! earlier send may complete a later one, which is harmless because
//! retries carry the same frame.
//!
//! # Cancel safety
//!
//! Every await reads the stream ([`Read::read`]) and handles what it read
//! before the next await, so a dropped `receive` loses no frame **provided
//! the stream's `read` is cancel-safe** (it consumes no byte unless it
//! completes; `mw_gateway::io::ChannelIo` is). A dropped send may or may
//! not have written its `Send`.

use super::rpc::{Delay, EspNowUnicast, First, first};
use super::{EspNowRadio, Mac};
use crate::radio_bridge::{self, BROADCAST, MAX_DATA, MAX_WIRE, Msg, Stats, VERSION};
use crate::serial::frame::{Split, Splitter};
use embedded_io_async::{Read, Write};

/// How long a send waits for the bridge's `Sent` (the bridge reports a
/// missing driver callback after 50 ms).
pub const SENT_TIMEOUT_MS: u32 = 300;

/// Bytes read from the stream per await.
const READ_CHUNK: usize = 64;

/// Who the bridge is (its `Hello`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hello {
    /// The bridge's station MAC.
    pub mac: Mac,
    /// Its Wi-Fi channel.
    pub channel: u8,
    /// Its protocol version.
    pub version: u8,
}

/// Failure of a [`BridgeRadio`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeIoError<E> {
    /// The stream failed.
    Io(E),
    /// The stream ended (a read returned 0 bytes).
    Closed,
    /// A frame to send is empty or longer than 250 bytes, or a received
    /// frame does not fit the caller's buffer (it is dropped, not
    /// truncated).
    FrameTooLarge,
    /// The bridge reported the send failed (no MAC-level acknowledgement).
    NotAcked,
    /// No `Sent` arrived within the send timeout.
    SentTimeout,
    /// The bridge speaks another protocol version.
    BadVersion(u8),
    /// No `Hello` arrived within the connect timeout.
    HelloTimeout,
}

impl<E: core::fmt::Debug> core::fmt::Display for BridgeIoError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BridgeIoError::Io(e) => write!(f, "bridge stream: {e:?}"),
            BridgeIoError::Closed => f.write_str("bridge stream closed"),
            BridgeIoError::FrameTooLarge => f.write_str("ESP-NOW frame too large"),
            BridgeIoError::NotAcked => f.write_str("send not acknowledged"),
            BridgeIoError::SentTimeout => f.write_str("no Sent from the bridge"),
            BridgeIoError::BadVersion(v) => write!(f, "bridge protocol version {v}"),
            BridgeIoError::HelloTimeout => f.write_str("no Hello from the bridge"),
        }
    }
}

/// Counters of a [`BridgeRadio`] (saturating).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BridgeCounters {
    /// `Recv` frames dropped because the ring was full.
    pub ring_dropped: u32,
    /// Runs that decoded after skipping leading garbage (a boot banner).
    pub noise_runs: u32,
    /// Runs that did not decode, or overflowed the splitter.
    pub bad_frames: u32,
    /// Sends with no `Sent` in time.
    pub sent_timeouts: u32,
    /// Sends the bridge reported failed.
    pub sent_failed: u32,
}

/// One queued `Recv` frame.
#[derive(Clone, Copy)]
struct Slot {
    src: Mac,
    len: u8,
    data: [u8; MAX_DATA],
}

const EMPTY: Slot = Slot {
    src: [0; 6],
    len: 0,
    data: [0; MAX_DATA],
};

/// Everything but the stream and the delay, so a send can pump the stream
/// while its timeout runs.
struct State<const Q: usize> {
    split: Splitter<MAX_WIRE>,
    ring: [Slot; Q],
    head: usize,
    len: usize,
    hello: Option<Hello>,
    stats: Option<Stats>,
    awaiting: Option<Mac>,
    outcome: Option<bool>,
    counters: BridgeCounters,
}

fn bump(c: &mut u32) {
    *c = c.saturating_add(1);
}

impl<const Q: usize> State<Q> {
    fn push(&mut self, src: Mac, data: &[u8]) {
        let Ok(len) = u8::try_from(data.len()) else {
            return bump(&mut self.counters.bad_frames);
        };
        if self.len >= Q {
            return bump(&mut self.counters.ring_dropped);
        }
        let at = (self.head + self.len) % Q;
        if let Some(slot) = self.ring.get_mut(at) {
            slot.src = src;
            slot.len = len;
            if let Some(d) = slot.data.get_mut(..data.len()) {
                d.copy_from_slice(data);
            }
            self.len += 1;
        }
    }

    fn pop_into<'b, E>(
        &mut self,
        buf: &'b mut [u8],
    ) -> Result<Option<(&'b [u8], Mac)>, BridgeIoError<E>> {
        if self.len == 0 {
            return Ok(None);
        }
        let slot = self.ring.get(self.head).copied().unwrap_or(EMPTY);
        self.head = (self.head + 1) % Q;
        self.len -= 1;
        let n = usize::from(slot.len);
        let out = buf.get_mut(..n).ok_or(BridgeIoError::FrameTooLarge)?;
        out.copy_from_slice(slot.data.get(..n).unwrap_or_default());
        Ok(Some((out, slot.src)))
    }

    fn handle(&mut self, msg: Msg<'_>) {
        match msg {
            Msg::Recv { src, data } => self.push(src, data),
            Msg::Hello {
                mac,
                channel,
                version,
            } => {
                self.hello = Some(Hello {
                    mac,
                    channel,
                    version,
                });
            }
            Msg::Stats(s) => self.stats = Some(s),
            Msg::Sent { ok, dst } => {
                if self.awaiting == Some(dst) {
                    self.outcome = Some(ok);
                }
            }
            // Host → bridge types heard back.
            Msg::Send { .. } | Msg::Query => {}
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            match self.split.push(b) {
                Split::Pending => {}
                Split::Overflow => bump(&mut self.counters.bad_frames),
                Split::Frame(_) => {
                    let mut scratch = [0u8; MAX_WIRE];
                    match radio_bridge::decode_run(self.split.frame(), &mut scratch) {
                        Some((skip, msg)) => {
                            if skip > 0 {
                                bump(&mut self.counters.noise_runs);
                            }
                            self.handle(msg);
                        }
                        None => bump(&mut self.counters.bad_frames),
                    }
                }
            }
        }
    }
}

/// Reads once from `io` and handles every complete message in what was
/// read, with no await in between (cancel-safe for a cancel-safe `read`).
async fn pump<IO: Read, const Q: usize>(
    io: &mut IO,
    st: &mut State<Q>,
) -> Result<(), BridgeIoError<IO::Error>> {
    let mut chunk = [0u8; READ_CHUNK];
    let n = io.read(&mut chunk).await.map_err(BridgeIoError::Io)?;
    if n == 0 {
        return Err(BridgeIoError::Closed);
    }
    st.feed(chunk.get(..n).unwrap_or_default());
    Ok(())
}

/// An ESP-NOW radio over the USB bridge on `io` (module docs); `delay`
/// times the sends; at most `Q` received frames wait while a send does.
pub struct BridgeRadio<IO, D, const Q: usize = 8> {
    io: IO,
    delay: D,
    st: State<Q>,
    sent_timeout_ms: u32,
}

impl<IO: Read + Write, D: Delay, const Q: usize> BridgeRadio<IO, D, Q> {
    /// A radio over the bridge on `io`, timing sends with `delay`.
    pub const fn new(io: IO, delay: D) -> Self {
        BridgeRadio {
            io,
            delay,
            st: State {
                split: Splitter::new(),
                ring: [EMPTY; Q],
                head: 0,
                len: 0,
                hello: None,
                stats: None,
                awaiting: None,
                outcome: None,
                counters: BridgeCounters {
                    ring_dropped: 0,
                    noise_runs: 0,
                    bad_frames: 0,
                    sent_timeouts: 0,
                    sent_failed: 0,
                },
            },
            sent_timeout_ms: SENT_TIMEOUT_MS,
        }
    }

    /// The same radio waiting `ms` for each `Sent`.
    pub fn with_sent_timeout_ms(mut self, ms: u32) -> Self {
        self.sent_timeout_ms = ms;
        self
    }

    /// The bridge's MAC, once its `Hello` was seen.
    pub fn own_mac(&self) -> Option<Mac> {
        self.st.hello.map(|h| h.mac)
    }

    /// The bridge's last `Hello`.
    pub fn hello(&self) -> Option<Hello> {
        self.st.hello
    }

    /// The bridge's last reported counters.
    pub fn bridge_stats(&self) -> Option<Stats> {
        self.st.stats
    }

    /// This side's counters.
    pub fn counters(&self) -> BridgeCounters {
        self.st.counters
    }

    /// Asks the bridge who it is (`Query`) and waits for its `Hello`, at
    /// most `timeout_ms` in all; the `Query` is sent twice (halfway), as
    /// the boot `Hello` may have been missed or the bridge still booting.
    /// Another protocol version is [`BridgeIoError::BadVersion`].
    pub async fn connect(&mut self, timeout_ms: u32) -> Result<Hello, BridgeIoError<IO::Error>> {
        self.st.hello = None;
        for _ in 0..2 {
            self.write_msg(&Msg::Query).await?;
            let BridgeRadio { io, delay, st, .. } = self;
            let wait = async {
                // Bounded by the stream and the timeout racing it.
                loop {
                    if let Some(h) = st.hello {
                        break Ok(h);
                    }
                    pump(io, st).await?;
                }
            };
            match first(wait, delay.delay_ms(timeout_ms / 2)).await {
                First::Left(Ok(h)) if h.version != VERSION => {
                    return Err(BridgeIoError::BadVersion(h.version));
                }
                First::Left(r) => return r,
                First::Right(()) => {}
            }
        }
        Err(BridgeIoError::HelloTimeout)
    }

    /// Asks the bridge for `Hello` and `Stats`; the answers are folded in
    /// by later reads ([`Self::bridge_stats`]).
    pub async fn request_stats(&mut self) -> Result<(), BridgeIoError<IO::Error>> {
        self.write_msg(&Msg::Query).await
    }

    /// Reads once from the bridge and handles what arrived.
    pub async fn pump_one(&mut self) -> Result<(), BridgeIoError<IO::Error>> {
        pump(&mut self.io, &mut self.st).await
    }

    async fn receive_frame<'b>(
        &mut self,
        buf: &'b mut [u8],
    ) -> Result<(&'b [u8], Mac), BridgeIoError<IO::Error>> {
        // Bounded by the stream: each pass pumps once or returns.
        while self.st.len == 0 {
            pump(&mut self.io, &mut self.st).await?;
        }
        self.st.pop_into(buf)?.ok_or(BridgeIoError::Closed)
    }

    async fn write_msg(&mut self, msg: &Msg<'_>) -> Result<(), BridgeIoError<IO::Error>> {
        let mut wire = [0u8; MAX_WIRE];
        let n = msg
            .encode_wire(&mut wire)
            .map_err(|_| BridgeIoError::FrameTooLarge)?;
        let wire = wire.get(..n).unwrap_or_default();
        self.io.write_all(wire).await.map_err(BridgeIoError::Io)?;
        self.io.flush().await.map_err(BridgeIoError::Io)
    }

    /// Writes one `Send` to `dst` and waits for its `Sent` (module docs).
    /// Starts a send without waiting: writes one `Send` to `dst` and
    /// records that its `Sent` is awaited. The caller keeps reading
    /// ([`Self::pump_one`], [`EspNowRadio::try_receive`]) so frames heard
    /// meanwhile flow on rather than fill the ring, and ends the send with
    /// [`Self::sent_outcome`] or [`Self::abandon_send`]. Replaces a send in
    /// progress.
    pub async fn begin_send(
        &mut self,
        dst: Mac,
        frame: &[u8],
    ) -> Result<(), BridgeIoError<IO::Error>> {
        if frame.is_empty() || frame.len() > MAX_DATA {
            return Err(BridgeIoError::FrameTooLarge);
        }
        self.st.awaiting = Some(dst);
        self.st.outcome = None;
        let written = self.write_msg(&Msg::Send { dst, data: frame }).await;
        if written.is_err() {
            self.st.awaiting = None;
        }
        written
    }

    /// The destination of the send in progress, if any.
    pub fn sending(&self) -> Option<Mac> {
        self.st.awaiting
    }

    /// The bridge's `Sent` for the send in progress, once read
    /// (`Some(acknowledged)`), which ends it; `None` while it is awaited.
    pub fn sent_outcome(&mut self) -> Option<bool> {
        let ok = self.st.outcome.take()?;
        self.st.awaiting = None;
        if !ok {
            bump(&mut self.st.counters.sent_failed);
        }
        Some(ok)
    }

    /// Gives up on the send in progress (no `Sent` in time): counted, and
    /// a late `Sent` for it is ignored.
    pub fn abandon_send(&mut self) {
        if self.st.awaiting.take().is_some() {
            bump(&mut self.st.counters.sent_timeouts);
        }
        self.st.outcome = None;
    }

    /// Writes one `Send` to `dst` and waits for its `Sent` (module docs).
    async fn send(&mut self, dst: Mac, frame: &[u8]) -> Result<(), BridgeIoError<IO::Error>> {
        self.begin_send(dst, frame).await?;
        let BridgeRadio {
            io,
            delay,
            st,
            sent_timeout_ms,
        } = self;
        let wait = async {
            // Bounded by the stream and the timeout racing it.
            loop {
                if st.outcome.is_some() {
                    break Ok(());
                }
                pump(io, st).await?;
            }
        };
        let raced = first(wait, delay.delay_ms(*sent_timeout_ms)).await;
        match raced {
            First::Left(Ok(())) => match self.sent_outcome() {
                Some(true) => Ok(()),
                _ => Err(BridgeIoError::NotAcked),
            },
            First::Left(Err(e)) => {
                self.st.awaiting = None;
                Err(e)
            }
            First::Right(()) => {
                self.abandon_send();
                Err(BridgeIoError::SentTimeout)
            }
        }
    }
}

impl<IO: Read + Write, D: Delay, const Q: usize> EspNowRadio for BridgeRadio<IO, D, Q> {
    type Error = BridgeIoError<IO::Error>;

    /// Sends to [`BROADCAST`] and waits for the bridge's `Sent`.
    async fn broadcast(&mut self, frame: &[u8]) -> Result<(), Self::Error> {
        self.send(BROADCAST, frame).await
    }

    async fn receive<'b>(&mut self, buf: &'b mut [u8]) -> Result<(&'b [u8], Mac), Self::Error> {
        self.receive_frame(buf).await
    }

    /// A frame already read from the bridge, if any; never reads.
    fn try_receive<'b>(
        &mut self,
        buf: &'b mut [u8],
    ) -> Result<Option<(&'b [u8], Mac)>, Self::Error> {
        self.st.pop_into(buf)
    }
}

impl<IO: Read + Write, D: Delay, const Q: usize> EspNowUnicast for BridgeRadio<IO, D, Q> {
    /// Sends to `peer` and waits for the bridge's `Sent` for it.
    async fn send_to(&mut self, peer: Mac, frame: &[u8]) -> Result<(), Self::Error> {
        self.send(peer, frame).await
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::key::Key;
    use crate::rpc::Message;
    use crate::transport::{RequestOptions, Requester};
    use core::future::{Future, poll_fn};
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};
    use std::collections::VecDeque;
    use std::vec::Vec;

    const BRIDGE: Mac = [0x5c, 0xcf, 0x7f, 0x72, 0xf6, 0x54];
    const DEV: Mac = [0xe4, 0xb0, 0x63, 0x81, 0xd3, 0x24];

    /// Polls `f` with a no-op waker until ready (bounded).
    fn block_on<F: Future>(f: F) -> F::Output {
        let mut f = pin!(f);
        let mut cx = Context::from_waker(Waker::noop());
        for _ in 0..10_000 {
            if let Poll::Ready(out) = f.as_mut().poll(&mut cx) {
                return out;
            }
        }
        panic!("future did not complete");
    }

    /// Polls `f` `n` times (or until ready) and drops it; whether it was
    /// ready.
    fn poll_n_then_drop<F: Future>(f: F, n: usize) -> bool {
        let mut f = pin!(f);
        let mut cx = Context::from_waker(Waker::noop());
        (0..n).any(|_| f.as_mut().poll(&mut cx).is_ready())
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct FakeIoError;

    impl core::fmt::Display for FakeIoError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str("fake io error")
        }
    }

    impl core::error::Error for FakeIoError {}

    impl embedded_io_async::Error for FakeIoError {
        fn kind(&self) -> embedded_io_async::ErrorKind {
            embedded_io_async::ErrorKind::Other
        }
    }

    /// In-memory bridge stream: `inbox` feeds reads (at most `chunk` bytes
    /// each, every read pending once first, so a read is a real await);
    /// an empty inbox stays pending, or reads 0 when `closed`. Writes
    /// collect in `outbox`; `on_write` answers each one.
    struct FakeIo {
        inbox: VecDeque<u8>,
        outbox: Vec<u8>,
        chunk: usize,
        closed: bool,
        yielded: bool,
        /// Bytes queued into the inbox after each write.
        on_write: VecDeque<Vec<u8>>,
    }

    impl FakeIo {
        fn new() -> Self {
            FakeIo {
                inbox: VecDeque::new(),
                outbox: Vec::new(),
                chunk: READ_CHUNK,
                closed: false,
                yielded: false,
                on_write: VecDeque::new(),
            }
        }

        fn queue(&mut self, msg: Msg<'_>) {
            self.inbox.extend(wire(&msg));
        }

        /// The messages written so far.
        fn written(&self) -> Vec<std::string::String> {
            let mut s: Splitter<MAX_WIRE> = Splitter::new();
            let mut out = Vec::new();
            for &b in &self.outbox {
                if let Split::Frame(_) = s.push(b) {
                    let mut scratch = [0u8; MAX_WIRE];
                    let m = radio_bridge::decode_run(s.frame(), &mut scratch).map(|(_, m)| m);
                    out.push(std::format!("{m:?}"));
                }
            }
            out
        }
    }

    fn wire(msg: &Msg<'_>) -> Vec<u8> {
        let mut out = [0u8; MAX_WIRE];
        let n = msg.encode_wire(&mut out).unwrap();
        out[..n].to_vec()
    }

    impl embedded_io_async::ErrorType for FakeIo {
        type Error = FakeIoError;
    }

    impl Read for FakeIo {
        async fn read(&mut self, buf: &mut [u8]) -> Result<usize, FakeIoError> {
            poll_fn(|_| {
                if !core::mem::replace(&mut self.yielded, true) {
                    return Poll::Pending;
                }
                if self.inbox.is_empty() && !self.closed {
                    Poll::Pending
                } else {
                    Poll::Ready(())
                }
            })
            .await;
            self.yielded = false;
            let n = buf.len().min(self.chunk).min(self.inbox.len());
            for b in buf.iter_mut().take(n) {
                *b = self.inbox.pop_front().unwrap_or(0);
            }
            Ok(n)
        }
    }

    impl Write for FakeIo {
        async fn write(&mut self, buf: &[u8]) -> Result<usize, FakeIoError> {
            self.outbox.extend_from_slice(buf);
            if buf.last() == Some(&0)
                && let Some(reply) = self.on_write.pop_front()
            {
                self.inbox.extend(reply);
            }
            Ok(buf.len())
        }

        async fn flush(&mut self) -> Result<(), FakeIoError> {
            Ok(())
        }
    }

    /// Completes after `polls` polls of each delay (never when `None`).
    struct CountDelay {
        polls: Option<u32>,
        started: u32,
    }

    impl Delay for CountDelay {
        async fn delay_ms(&mut self, _ms: u32) {
            self.started += 1;
            let mut left = self.polls;
            poll_fn(|_| match left {
                Some(0) => Poll::Ready(()),
                Some(ref mut n) => {
                    *n -= 1;
                    Poll::Pending
                }
                None => Poll::Pending,
            })
            .await;
        }
    }

    fn radio<const Q: usize>(io: FakeIo) -> BridgeRadio<FakeIo, CountDelay, Q> {
        BridgeRadio::new(
            io,
            CountDelay {
                polls: Some(50),
                started: 0,
            },
        )
    }

    #[test]
    fn eg3_receive_yields_recv_and_skips_noise() {
        let mut io = FakeIo::new();
        io.inbox.extend(b"\r\n ets Jan  8 2013,rst cause:2\r\n");
        io.queue(Msg::Recv {
            src: DEV,
            data: b"MW one",
        });
        io.inbox.extend(b"garbage\x00");
        io.queue(Msg::Recv {
            src: BRIDGE,
            data: b"two",
        });
        let mut r = radio::<8>(io);
        let mut buf = [0u8; 250];
        let (f, src) = block_on(r.receive_frame(&mut buf)).unwrap();
        assert_eq!((f, src), (&b"MW one"[..], DEV));
        let (f, src) = block_on(r.receive_frame(&mut buf)).unwrap();
        assert_eq!((f, src), (&b"two"[..], BRIDGE));
        let c = r.counters();
        assert_eq!((c.noise_runs, c.bad_frames), (1, 1));
        r.io.closed = true;
        assert_eq!(
            block_on(r.receive_frame(&mut buf)),
            Err(BridgeIoError::Closed)
        );
    }

    #[test]
    fn eg3_hello_and_stats_are_internal() {
        let mut io = FakeIo::new();
        io.queue(Msg::Hello {
            mac: BRIDGE,
            channel: 1,
            version: VERSION,
        });
        let stats = Stats {
            rx: 7,
            ..Stats::default()
        };
        io.queue(Msg::Stats(stats));
        io.queue(Msg::Sent { ok: true, dst: DEV });
        io.queue(Msg::Recv {
            src: DEV,
            data: b"x",
        });
        let mut r = radio::<8>(io);
        assert_eq!(r.own_mac(), None);
        let mut buf = [0u8; 250];
        let (f, _) = block_on(r.receive_frame(&mut buf)).unwrap();
        assert_eq!(f, b"x");
        assert_eq!(r.own_mac(), Some(BRIDGE));
        assert_eq!(r.hello().map(|h| h.channel), Some(1));
        assert_eq!(r.bridge_stats(), Some(stats));
    }

    #[test]
    fn eg3_dropped_receive_loses_no_frame() {
        let mut io = FakeIo::new();
        io.chunk = 3;
        io.queue(Msg::Recv {
            src: DEV,
            data: b"a frame long enough for several reads",
        });
        let mut r = radio::<8>(io);
        let mut buf = [0u8; 250];
        // Part of the frame is read, then the receive is dropped.
        assert!(!poll_n_then_drop(r.receive_frame(&mut buf), 6));
        assert!(!r.io.inbox.is_empty());
        let (f, src) = block_on(r.receive_frame(&mut buf)).unwrap();
        assert_eq!(f, b"a frame long enough for several reads");
        assert_eq!(src, DEV);
    }

    fn sent(ok: bool, dst: Mac) -> Vec<u8> {
        wire(&Msg::Sent { ok, dst })
    }

    #[test]
    fn eg4_send_to_ok() {
        let mut io = FakeIo::new();
        io.on_write.push_back(sent(true, DEV));
        let mut r = radio::<8>(io);
        assert_eq!(block_on(r.send_to(DEV, b"MR req")), Ok(()));
        assert_eq!(
            r.io.written(),
            [std::format!(
                "{:?}",
                Some(Msg::Send {
                    dst: DEV,
                    data: b"MR req"
                })
            )]
        );
        assert_eq!(
            block_on(r.send_to(DEV, &[])),
            Err(BridgeIoError::FrameTooLarge)
        );
        assert_eq!(
            block_on(r.send_to(DEV, &[1; MAX_DATA + 1])),
            Err(BridgeIoError::FrameTooLarge)
        );
        assert_eq!(r.io.written().len(), 1);
    }

    #[test]
    fn eg4_send_to_nack() {
        let mut io = FakeIo::new();
        io.on_write.push_back(sent(false, DEV));
        let mut r = radio::<8>(io);
        assert_eq!(block_on(r.send_to(DEV, b"x")), Err(BridgeIoError::NotAcked));
        assert_eq!(r.counters().sent_failed, 1);
    }

    #[test]
    fn eg4_lost_sent_times_out() {
        let mut r = radio::<8>(FakeIo::new());
        assert_eq!(
            block_on(r.send_to(DEV, b"x")),
            Err(BridgeIoError::SentTimeout)
        );
        assert_eq!(r.counters().sent_timeouts, 1);
        assert_eq!(r.delay.started, 1);
        // The next send works once its Sent arrives.
        r.io.on_write.push_back(sent(true, DEV));
        assert_eq!(block_on(r.send_to(DEV, b"x")), Ok(()));
    }

    #[test]
    fn eg4_stale_sent_ignored() {
        let mut io = FakeIo::new();
        // A Sent for another station, then nothing: still a timeout.
        io.on_write.push_back(sent(true, BRIDGE));
        let mut r = radio::<8>(io);
        assert_eq!(
            block_on(r.send_to(DEV, b"x")),
            Err(BridgeIoError::SentTimeout)
        );
        // A late Sent heard with no send waiting changes nothing.
        r.io.inbox.extend(sent(false, DEV));
        r.io.queue(Msg::Recv {
            src: DEV,
            data: b"after",
        });
        let mut buf = [0u8; 250];
        let (f, _) = block_on(r.receive(&mut buf)).unwrap();
        assert_eq!(f, b"after");
        r.io.on_write.push_back(sent(true, DEV));
        assert_eq!(block_on(r.send_to(DEV, b"x")), Ok(()));
    }

    #[test]
    fn eg4_broadcast_addresses_ff_and_waits_for_sent() {
        let mut r = radio::<8>(FakeIo::new());
        assert_eq!(
            block_on(r.broadcast(b"MW pub")),
            Err(BridgeIoError::SentTimeout)
        );
        assert_eq!(
            r.io.written(),
            [std::format!(
                "{:?}",
                Some(Msg::Send {
                    dst: BROADCAST,
                    data: b"MW pub"
                })
            )]
        );
        r.io.on_write.push_back(sent(true, BROADCAST));
        assert_eq!(block_on(r.broadcast(b"MW pub")), Ok(()));
        r.io.on_write.push_back(sent(false, BROADCAST));
        assert_eq!(
            block_on(r.broadcast(b"MW pub")),
            Err(BridgeIoError::NotAcked)
        );
    }

    #[test]
    fn eg4_rpc_requester_resends_with_the_same_id_after_a_lost_sent() {
        use super::super::rpc::{Retry, RpcRequester};
        let mut io = FakeIo::new();
        // First Send: no Sent at all. Second: acknowledged, then answered.
        io.on_write.push_back(Vec::new());
        let mut frame = [0u8; crate::rpc::MAX_FRAME];
        let n = Message::Response {
            id: 7,
            to: BRIDGE,
            result: Ok(b"INFO"),
        }
        .encode(&mut frame)
        .unwrap();
        let mut answer = sent(true, DEV);
        answer.extend(wire(&Msg::Recv {
            src: DEV,
            data: &frame[..n],
        }));
        io.on_write.push_back(answer);
        let r = radio::<8>(io);
        let retry = Retry {
            timeout_ms: 100,
            attempts: 3,
        };
        let mut req = RpcRequester::new(
            r,
            CountDelay {
                polls: Some(50),
                started: 0,
            },
            DEV,
            retry,
            7,
        );
        let mut out = [0u8; 64];
        let got = block_on(req.request_with(
            Key::from_static("sys/info"),
            b"",
            &mut out,
            &RequestOptions::default(),
        ));
        assert_eq!(got, Ok(&b"INFO"[..]));
        let written = req.radio_mut().io.written();
        assert_eq!(written.len(), 2);
        assert_eq!(written[0], written[1], "the resend carries the same id");
    }

    #[test]
    fn eg4_a_split_send_lets_frames_through_while_it_waits() {
        let mut r = radio::<2>(FakeIo::new());
        block_on(r.begin_send(DEV, b"req")).unwrap();
        assert_eq!(r.sending(), Some(DEV));
        assert_eq!(r.sent_outcome(), None);
        // More frames than the ring holds arrive while the Sent is pending;
        // the caller takes each as it comes.
        let mut buf = [0u8; 250];
        for d in [&b"one"[..], b"two", b"three", b"four"] {
            r.io.queue(Msg::Recv { src: DEV, data: d });
            block_on(r.pump_one()).unwrap();
            let (f, _) = r.try_receive(&mut buf).unwrap().unwrap();
            assert_eq!(f, d);
            assert_eq!(r.sent_outcome(), None);
        }
        assert_eq!(r.counters().ring_dropped, 0);
        r.io.inbox.extend(sent(false, DEV));
        block_on(r.pump_one()).unwrap();
        assert_eq!(r.sent_outcome(), Some(false));
        assert_eq!((r.sending(), r.counters().sent_failed), (None, 1));
        // A send given up on: the late Sent is ignored.
        block_on(r.begin_send(DEV, b"req")).unwrap();
        r.abandon_send();
        assert_eq!((r.sending(), r.counters().sent_timeouts), (None, 1));
        r.io.inbox.extend(sent(true, DEV));
        block_on(r.pump_one()).unwrap();
        assert_eq!(r.sent_outcome(), None);
        assert_eq!(
            block_on(r.begin_send(DEV, &[])),
            Err(BridgeIoError::FrameTooLarge)
        );
    }

    #[test]
    fn eg3_frames_heard_during_a_send_are_queued_and_full_ring_drops_newest() {
        let mut io = FakeIo::new();
        let mut reply = Vec::new();
        for d in [&b"one"[..], b"two", b"three"] {
            reply.extend(wire(&Msg::Recv { src: DEV, data: d }));
        }
        reply.extend(sent(true, DEV));
        io.on_write.push_back(reply);
        let mut r = radio::<2>(io);
        assert_eq!(block_on(r.send_to(DEV, b"req")), Ok(()));
        assert_eq!(r.counters().ring_dropped, 1);
        let mut buf = [0u8; 250];
        assert_eq!(
            r.try_receive(&mut buf).unwrap().map(|(f, _)| f.to_vec()),
            Some(b"one".to_vec())
        );
        assert_eq!(
            r.try_receive(&mut buf).unwrap().map(|(f, _)| f.to_vec()),
            Some(b"two".to_vec())
        );
        assert_eq!(r.try_receive(&mut buf).unwrap(), None);
    }

    fn hello(version: u8) -> Vec<u8> {
        wire(&Msg::Hello {
            mac: BRIDGE,
            channel: 1,
            version,
        })
    }

    #[test]
    fn eg5_connect_queries_for_hello() {
        let mut io = FakeIo::new();
        // The first Query goes unanswered; the second gets Hello and Stats.
        io.on_write.push_back(Vec::new());
        let mut reply = hello(VERSION);
        reply.extend(wire(&Msg::Stats(Stats {
            tx_ok: 3,
            ..Stats::default()
        })));
        io.on_write.push_back(reply);
        let mut r = radio::<8>(io);
        let h = block_on(r.connect(2000)).unwrap();
        assert_eq!(
            h,
            Hello {
                mac: BRIDGE,
                channel: 1,
                version: VERSION
            }
        );
        assert_eq!(r.own_mac(), Some(BRIDGE));
        let q = std::format!("{:?}", Some(Msg::Query));
        assert_eq!(r.io.written(), [q.clone(), q.clone()]);
        // Stats arrive with later reads.
        r.io.queue(Msg::Recv {
            src: DEV,
            data: b"x",
        });
        let mut buf = [0u8; 250];
        block_on(r.receive(&mut buf)).unwrap();
        assert_eq!(r.bridge_stats().map(|s| s.tx_ok), Some(3));
        block_on(r.request_stats()).unwrap();
        assert_eq!(r.io.written().len(), 3);
    }

    #[test]
    fn eg5_no_hello_is_a_timeout() {
        let mut r = radio::<8>(FakeIo::new());
        assert_eq!(block_on(r.connect(2000)), Err(BridgeIoError::HelloTimeout));
        assert_eq!(r.io.written().len(), 2);
    }

    #[test]
    fn eg5_bad_version_refused() {
        let mut io = FakeIo::new();
        io.on_write.push_back(hello(VERSION + 1));
        let mut r = radio::<8>(io);
        assert_eq!(
            block_on(r.connect(2000)),
            Err(BridgeIoError::BadVersion(VERSION + 1))
        );
    }

    #[test]
    fn eg3_a_frame_too_large_for_the_buffer_is_an_error() {
        let mut io = FakeIo::new();
        io.queue(Msg::Recv {
            src: DEV,
            data: b"twelve bytes",
        });
        let mut r = radio::<8>(io);
        let mut small = [0u8; 4];
        assert_eq!(
            block_on(r.receive_frame(&mut small)),
            Err(BridgeIoError::FrameTooLarge)
        );
    }
}
