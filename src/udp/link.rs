//! [`UdpTransport`]: the core traits over one UDP socket connected to one
//! device.
//!
//! * A request gets a fresh correlation id (1..=255, wrapping). With no
//!   matching response after [`RESEND_AFTER`] it is resent with a new id,
//!   until the call's timeout ([`DEFAULT_TIMEOUT`] when the options give
//!   none) passes; a late answer to an earlier attempt is then stale and
//!   discarded. Error responses are never retried. Datagram loss is the
//!   normal failure of UDP, so the resend is what makes a request reliable.
//! * Publications received while a request waits are queued, in arrival
//!   order, for [`Subscriber::recv`]; at most [`INBOX_LEN`] are kept and a
//!   newer one is dropped when the queue is full.
//! * The device's address is the peer; nothing on the wire names a sender,
//!   so [`SampleMeta::sender`] is whatever [`UdpTransport::with_peer`] set
//!   (the node id the device reported in `sys/info`) and otherwise `None`.
//! * [`Target::Node`] is refused with [`UdpError::Target`] unless it names
//!   that peer: the socket reaches one device only.

use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;
use std::vec::Vec;

use tokio::net::UdpSocket;
use tokio::time::{Instant, timeout_at};

use super::{MAX_DATAGRAM, MAX_KEY, decode, encode};
use crate::key::{AsKey, Key, KeyBuf};
use crate::serial::frame::{FRAME_OVERHEAD, Frame, FrameError};
use crate::transport::{
    Delivery, HandlerError, Limits, Overflow, Publisher, RequestOptions, Requester, Sample,
    SampleMeta, SenderId, Subscriber, Target,
};

/// How long a request waits when its options give no timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);

/// How long one request attempt waits before it is resent with a new id.
pub const RESEND_AFTER: Duration = Duration::from_millis(300);

/// Publications kept while a request waits.
pub const INBOX_LEN: usize = 64;

/// Why a UDP operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UdpError {
    /// The socket failed.
    Io(io::ErrorKind),
    /// The frame could not be encoded (too large).
    Frame(FrameError),
    /// The device answered with an error.
    Remote(HandlerError),
    /// No response within the timeout.
    Timeout,
    /// The response did not fit the caller's buffer.
    BufferTooSmall,
    /// The request targeted a node other than this socket's peer.
    Target,
}

impl From<io::Error> for UdpError {
    fn from(e: io::Error) -> Self {
        UdpError::Io(e.kind())
    }
}

impl From<FrameError> for UdpError {
    fn from(e: FrameError) -> Self {
        UdpError::Frame(e)
    }
}

struct Queued {
    key: KeyBuf<MAX_KEY>,
    payload: Vec<u8>,
}

/// The middleware over a UDP socket to one device.
pub struct UdpTransport {
    socket: UdpSocket,
    peer: Option<SenderId>,
    next_id: u8,
    inbox: VecDeque<Queued>,
    dropped: u32,
}

impl UdpTransport {
    /// Binds an ephemeral local port and connects it to `device` (usually
    /// port [`super::PORT`]).
    pub async fn connect(device: SocketAddr) -> io::Result<Self> {
        let local: SocketAddr = if device.is_ipv4() {
            "0.0.0.0:0".parse().map_err(io::Error::other)?
        } else {
            "[::]:0".parse().map_err(io::Error::other)?
        };
        let socket = UdpSocket::bind(local).await?;
        socket.connect(device).await?;
        Ok(Self::from_socket(socket))
    }

    /// Wraps a socket already connected to the device.
    pub fn from_socket(socket: UdpSocket) -> Self {
        UdpTransport {
            socket,
            peer: None,
            next_id: 0,
            inbox: VecDeque::new(),
            dropped: 0,
        }
    }

    /// Names the device: the sender of every received sample, and the only
    /// node a [`Target::Node`] request may name.
    pub fn with_peer(mut self, peer: SenderId) -> Self {
        self.peer = Some(peer);
        self
    }

    /// Publications dropped because the inbox was full.
    pub fn inbox_dropped(&self) -> u32 {
        self.dropped
    }

    fn fresh_id(&mut self) -> u8 {
        self.next_id = self.next_id.checked_add(1).unwrap_or(1);
        self.next_id
    }

    fn meta(&self) -> SampleMeta {
        match self.peer {
            Some(p) => SampleMeta::from_sender(p),
            None => SampleMeta::NONE,
        }
    }

    async fn send(&self, frame: &Frame<'_>) -> Result<(), UdpError> {
        let mut out = [0u8; MAX_DATAGRAM];
        let n = encode(frame, &mut out)?;
        self.socket
            .send(out.get(..n).ok_or(UdpError::Frame(FrameError::TooLarge))?)
            .await?;
        Ok(())
    }

    fn queue(&mut self, key: Key<'_>, payload: &[u8]) {
        let Ok(key) = KeyBuf::from_key(key) else {
            return;
        };
        if self.inbox.len() >= INBOX_LEN {
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        self.inbox.push_back(Queued {
            key,
            payload: payload.to_vec(),
        });
    }

    fn pop_into<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Sample<'b>>, UdpError> {
        let Some(q) = self.inbox.front() else {
            return Ok(None);
        };
        let k = q.key.as_key().as_bytes().len();
        if buf.len() < k + q.payload.len() {
            return Err(UdpError::BufferTooSmall);
        }
        let Some(q) = self.inbox.pop_front() else {
            return Ok(None);
        };
        let meta = self.meta();
        let (kb, rest) = buf.split_at_mut(k);
        kb.copy_from_slice(q.key.as_key().as_bytes());
        let pb = rest
            .get_mut(..q.payload.len())
            .ok_or(UdpError::BufferTooSmall)?;
        pb.copy_from_slice(&q.payload);
        let key = core::str::from_utf8(kb)
            .ok()
            .and_then(|s| Key::new(s).ok())
            .ok_or(UdpError::Frame(FrameError::InvalidKey))?;
        Ok(Some(Sample::new(key, pb).with_meta(meta)))
    }

    /// Reads one datagram (waiting until `deadline`) and queues it if it is
    /// a publication. Returns the response it held, if any, as `(id,
    /// result)` with the body copied out.
    async fn read_one(
        &mut self,
        deadline: Option<Instant>,
    ) -> Result<Option<(u8, Result<Vec<u8>, HandlerError>)>, UdpError> {
        let mut buf = [0u8; MAX_DATAGRAM + 1];
        let n = match deadline {
            Some(d) => match timeout_at(d, self.socket.recv(&mut buf)).await {
                Ok(r) => r?,
                Err(_) => return Err(UdpError::Timeout),
            },
            None => self.socket.recv(&mut buf).await?,
        };
        let Some(dgram) = buf.get(..n) else {
            return Ok(None);
        };
        match decode(dgram) {
            Ok(Frame::Publish { key, payload }) => {
                self.queue(key, payload);
                Ok(None)
            }
            Ok(Frame::Response { id, result }) => Ok(Some((id, result.map(<[u8]>::to_vec)))),
            // Requests from the device and undecodable datagrams are dropped.
            Ok(Frame::Request { .. }) | Err(_) => Ok(None),
        }
    }
}

impl Requester for UdpTransport {
    type Error = UdpError;

    /// See the module docs: resends every [`RESEND_AFTER`] until the
    /// timeout ([`DEFAULT_TIMEOUT`] by default).
    async fn request_with<'b>(
        &mut self,
        key: Key<'_>,
        request: &[u8],
        response: &'b mut [u8],
        options: &RequestOptions,
    ) -> Result<&'b [u8], UdpError> {
        if let Target::Node(n) = options.target
            && self.peer != Some(n)
        {
            return Err(UdpError::Target);
        }
        let end = Instant::now() + options.timeout.unwrap_or(DEFAULT_TIMEOUT);
        loop {
            let id = self.fresh_id();
            self.send(&Frame::Request {
                id,
                key,
                payload: request,
            })
            .await?;
            let attempt_end = (Instant::now() + RESEND_AFTER).min(end);
            loop {
                match self.read_one(Some(attempt_end)).await {
                    Ok(Some((got, result))) if got == id => {
                        let body = result.map_err(UdpError::Remote)?;
                        let out = response
                            .get_mut(..body.len())
                            .ok_or(UdpError::BufferTooSmall)?;
                        out.copy_from_slice(&body);
                        return Ok(out);
                    }
                    Ok(_) => {}
                    Err(UdpError::Timeout) => break,
                    Err(e) => return Err(e),
                }
            }
            if Instant::now() >= end {
                return Err(UdpError::Timeout);
            }
        }
    }
}

impl Publisher for UdpTransport {
    type Error = UdpError;

    const LIMITS: Limits = Limits::shared(MAX_KEY, MAX_DATAGRAM - FRAME_OVERHEAD);

    async fn publish(&mut self, key: Key<'_>, payload: &[u8]) -> Result<(), UdpError> {
        self.send(&Frame::Publish { key, payload }).await
    }
}

impl Subscriber for UdpTransport {
    type Error = UdpError;

    /// The kernel's receive buffer and the inbox drop the newest datagram
    /// when full; UDP may reorder, so no order is promised.
    const DELIVERY: Delivery = Delivery::new(Overflow::DropNewest, false);

    const LIMITS: Limits = Limits::shared(MAX_KEY, MAX_DATAGRAM - FRAME_OVERHEAD);

    async fn recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Sample<'b>, UdpError> {
        while self.inbox.is_empty() {
            self.read_one(None).await?;
        }
        self.pop_into(buf)?.ok_or(UdpError::Timeout)
    }

    fn try_recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Sample<'b>>, UdpError> {
        let mut dgram = [0u8; MAX_DATAGRAM + 1];
        loop {
            match self.socket.try_recv(&mut dgram) {
                Ok(n) => {
                    if let Some(Ok(Frame::Publish { key, payload })) = dgram.get(..n).map(decode) {
                        self.queue(key, payload);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e.into()),
            }
        }
        self.pop_into(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pair() -> (UdpTransport, UdpSocket) {
        let dev = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let t = UdpTransport::connect(dev.local_addr().unwrap())
            .await
            .unwrap();
        (t, dev)
    }

    /// Answers requests on `dev` like a device: `sys/info` with "ecp5",
    /// anything else NotFound; before answering, publishes `demo/x`. Drops
    /// the first `drop_first` requests to test resends.
    async fn device(dev: UdpSocket, mut drop_first: usize) {
        let mut buf = [0u8; MAX_DATAGRAM];
        loop {
            let (n, from) = dev.recv_from(&mut buf).await.unwrap();
            let Ok(Frame::Request { id, key, .. }) = decode(&buf[..n]) else {
                continue;
            };
            if drop_first > 0 {
                drop_first -= 1;
                continue;
            }
            let mut out = [0u8; MAX_DATAGRAM];
            let pubn = encode(
                &Frame::Publish {
                    key: Key::from_static("demo/x"),
                    payload: b"p",
                },
                &mut out,
            )
            .unwrap();
            dev.send_to(&out[..pubn], from).await.unwrap();
            let result = if key.as_str() == "sys/info" {
                Ok(&b"ecp5"[..])
            } else {
                Err(HandlerError::NotFound)
            };
            let m = encode(&Frame::Response { id, result }, &mut out).unwrap();
            dev.send_to(&out[..m], from).await.unwrap();
        }
    }

    #[tokio::test]
    async fn cl1_request_response_and_queued_publication() {
        let (mut t, dev) = pair().await;
        tokio::spawn(device(dev, 0));
        let mut resp = [0u8; 64];
        let got = t
            .request(Key::from_static("sys/info"), b"", &mut resp)
            .await
            .unwrap();
        assert_eq!(got, b"ecp5");
        let mut buf = [0u8; 64];
        let s = t.try_recv(&mut buf).unwrap().expect("queued publish");
        assert_eq!((s.key.as_str(), s.payload), ("demo/x", &b"p"[..]));
        assert_eq!(s.meta.sender, None);
    }

    #[tokio::test]
    async fn cl1_remote_errors_are_not_retried() {
        let (mut t, dev) = pair().await;
        tokio::spawn(device(dev, 0));
        let mut resp = [0u8; 64];
        let r = t.request(Key::from_static("nope"), b"", &mut resp).await;
        assert_eq!(r, Err(UdpError::Remote(HandlerError::NotFound)));
    }

    #[tokio::test]
    async fn cl1_a_lost_request_is_resent_with_a_fresh_id() {
        let (mut t, dev) = pair().await;
        tokio::spawn(device(dev, 2));
        let mut resp = [0u8; 64];
        let opts = RequestOptions::DEFAULT.with_timeout(Duration::from_secs(3));
        let got = t
            .request_with(Key::from_static("sys/info"), b"", &mut resp, &opts)
            .await
            .unwrap();
        assert_eq!(got, b"ecp5");
        assert_eq!(t.next_id, 3);
    }

    #[tokio::test]
    async fn cl1_silence_times_out_and_foreign_targets_are_refused() {
        let (mut t, _dev) = pair().await;
        let mut resp = [0u8; 8];
        let opts = RequestOptions::DEFAULT.with_timeout(Duration::from_millis(50));
        let r = t
            .request_with(Key::from_static("sys/info"), b"", &mut resp, &opts)
            .await;
        assert_eq!(r, Err(UdpError::Timeout));
        let opts = opts.with_target(Target::Node(SenderId([1; 8])));
        let r = t
            .request_with(Key::from_static("sys/info"), b"", &mut resp, &opts)
            .await;
        assert_eq!(r, Err(UdpError::Target));
    }

    #[tokio::test]
    async fn cl1_publish_sends_one_raw_frame_and_recv_names_the_peer() {
        let (t, dev) = pair().await;
        let mut t = t.with_peer(SenderId([7; 8]));
        t.publish(Key::from_static("a/b"), b"hi").await.unwrap();
        let mut buf = [0u8; MAX_DATAGRAM];
        let (n, from) = dev.recv_from(&mut buf).await.unwrap();
        assert_eq!(
            decode(&buf[..n]).unwrap(),
            Frame::Publish {
                key: Key::from_static("a/b"),
                payload: b"hi"
            }
        );
        // Garbage first: dropped without disturbing the next datagram.
        dev.send_to(&[1, 2, 3], from).await.unwrap();
        let mut out = [0u8; 32];
        let m = encode(
            &Frame::Publish {
                key: Key::from_static("c"),
                payload: &[0],
            },
            &mut out,
        )
        .unwrap();
        dev.send_to(&out[..m], from).await.unwrap();
        let mut rbuf = [0u8; 16];
        let s = t.recv(&mut rbuf).await.unwrap();
        assert_eq!((s.key.as_str(), s.payload), ("c", &[0u8][..]));
        assert_eq!(s.meta.sender, Some(SenderId([7; 8])));
    }
}
