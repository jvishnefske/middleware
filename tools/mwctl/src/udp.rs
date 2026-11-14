//! [`UdpLink`]: the protocol over UDP, one raw serial frame per datagram
//! ([`middleware::udp`], port [`middleware::udp::PORT`]).
//!
//! The request rules are [`crate::link::Link`]'s: each attempt gets a
//! fresh correlation id (1..=255, wrapping), waits
//! [`Timeouts::for_key`](crate::link::Timeouts::for_key) and is resent with
//! a new id up to `retries` more times; a late answer to an earlier attempt
//! is stale and discarded, and error responses are never retried.
//! Publications that arrive meanwhile are queued as [`Event`]s. A datagram
//! that does not decode is reported as [`Event::Noise`].
//!
//! There is no leading `0x00` and no COBS: datagrams have boundaries.
//! [`Datagram`] is the I/O seam; a connected [`std::net::UdpSocket`]
//! implements it.

use crate::error::Error;
use crate::link::{Endpoint, Event, Timeouts};
use middleware::Key;
use middleware::serial::frame::{Frame, FrameError};
use middleware::udp::{self, MAX_DATAGRAM};
use std::io;
use std::net::UdpSocket;
use std::time::{Duration, Instant};

/// A datagram path to the device.
pub trait Datagram {
    /// Sends one datagram.
    fn send(&mut self, datagram: &[u8]) -> io::Result<()>;
    /// Waits at most `timeout` for one datagram and copies it into `buf`.
    /// `Ok(None)` means nothing arrived in time.
    fn recv(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<Option<usize>>;
}

impl Datagram for UdpSocket {
    fn send(&mut self, datagram: &[u8]) -> io::Result<()> {
        UdpSocket::send(self, datagram).map(drop)
    }

    fn recv(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<Option<usize>> {
        // A zero read timeout means "block forever" to the OS.
        self.set_read_timeout(Some(timeout.max(Duration::from_millis(1))))?;
        match UdpSocket::recv(self, buf) {
            Ok(n) => Ok(Some(n)),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }
}

/// Parses `HOST[:PORT]` (an IPv6 host in brackets when a port follows),
/// with [`udp::PORT`] as the default port, into a `host:port` string for
/// [`std::net::ToSocketAddrs`].
pub fn with_default_port(addr: &str) -> Result<String, String> {
    if addr.is_empty() {
        return Err("--udp needs HOST[:PORT]".into());
    }
    let has_port = if let Some(rest) = addr.strip_prefix('[') {
        let (_, tail) = rest
            .split_once(']')
            .ok_or_else(|| format!("--udp: '{addr}' has no closing ']'"))?;
        !tail.is_empty()
    } else {
        // A bare IPv6 address has several colons and no port.
        addr.matches(':').count() == 1
    };
    if !has_port {
        return Ok(if addr.contains(':') && !addr.starts_with('[') {
            format!("[{addr}]:{}", udp::PORT)
        } else {
            format!("{addr}:{}", udp::PORT)
        });
    }
    let port = addr.rsplit_once(':').map(|(_, p)| p).unwrap_or_default();
    match port.parse::<u16>() {
        Ok(p) if p != 0 => Ok(addr.to_owned()),
        _ => Err(format!("--udp: '{port}' is not a port number")),
    }
}

/// Host endpoint of the protocol over UDP.
pub struct UdpLink<D> {
    io: D,
    next_id: u8,
    events: Vec<Event>,
    timeouts: Timeouts,
}

impl<D: Datagram> UdpLink<D> {
    /// Wraps a datagram path.
    pub fn new(io: D, timeouts: Timeouts) -> Self {
        UdpLink {
            io,
            next_id: 1,
            events: Vec::new(),
            timeouts,
        }
    }

    /// The datagram path.
    pub fn transport(&self) -> &D {
        &self.io
    }

    fn take_id(&mut self) -> u8 {
        let id = self.next_id;
        self.next_id = if id == u8::MAX { 1 } else { id + 1 };
        id
    }

    /// Waits until `deadline` for one datagram. Publications and noise are
    /// queued as events; a response is returned. `Ok(None)` at the deadline
    /// or after a non-response datagram.
    #[allow(clippy::type_complexity)]
    fn poll(
        &mut self,
        deadline: Instant,
    ) -> Result<Option<Option<(u8, Result<Vec<u8>, middleware::transport::HandlerError>)>>, Error>
    {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(None);
        }
        let mut buf = [0u8; MAX_DATAGRAM + 1];
        let Some(n) = self.io.recv(&mut buf, left)? else {
            return Ok(None);
        };
        let dgram = buf.get(..n).unwrap_or_default();
        Ok(Some(match udp::decode(dgram) {
            Ok(Frame::Response { id, result }) => Some((id, result.map(<[u8]>::to_vec))),
            Ok(Frame::Publish { key, payload }) => {
                self.events.push(Event::Publish {
                    key: key.as_str().to_owned(),
                    payload: payload.to_vec(),
                });
                None
            }
            Ok(Frame::Request { .. }) => None,
            Err(_) => {
                self.events.push(Event::Noise(dgram.to_vec()));
                None
            }
        }))
    }
}

impl<D: Datagram> Endpoint for UdpLink<D> {
    fn request(&mut self, key: Key<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
        let timeout = self.timeouts.for_key(key);
        let attempts = 1 + self.timeouts.retries;
        let mut out = [0u8; MAX_DATAGRAM];
        for _ in 0..attempts {
            let id = self.take_id();
            let n = udp::encode(
                &Frame::Request {
                    id,
                    key,
                    payload: body,
                },
                &mut out,
            )
            .map_err(Error::Frame)?;
            self.io
                .send(out.get(..n).ok_or(Error::Frame(FrameError::TooLarge))?)?;
            let deadline = Instant::now() + timeout;
            // Each iteration consumes a datagram or ends at the deadline.
            while let Some(got) = self.poll(deadline)? {
                if let Some((got, result)) = got
                    && got == id
                {
                    return result.map_err(|status| Error::Device {
                        key: key.as_str().to_owned(),
                        status,
                    });
                }
            }
        }
        Err(Error::Timeout {
            key: key.as_str().to_owned(),
            attempts,
        })
    }

    fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    fn next_event(&mut self, deadline: Instant) -> Result<Option<Event>, Error> {
        loop {
            if !self.events.is_empty() {
                return Ok(Some(self.events.remove(0)));
            }
            if self.poll(deadline)?.is_none() {
                return Ok(None);
            }
        }
    }

    fn chunk_size(&self) -> usize {
        middleware::loader::CHUNK_MAX
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::{Device, UploadOptions};
    use crate::fake::{FakeDevice, Script};
    use crate::link::Transport;
    use middleware::loader::PayloadStatus;
    use middleware::serial::cobs;
    use middleware::serial::frame::{MAX_RAW_FRAME, MAX_WIRE_FRAME, Split, Splitter};
    use std::sync::mpsc;
    use std::thread;

    /// Serves `fake` on a loopback UDP socket: each datagram is handed to
    /// the fake as one COBS frame, and each frame it answers with goes back
    /// as one datagram. Returns the socket address and a channel that
    /// yields the fake when the client hangs up (an empty datagram).
    fn serve(mut fake: FakeDevice) -> (std::net::SocketAddr, mpsc::Receiver<FakeDevice>) {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = sock.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut split: Splitter<MAX_WIRE_FRAME> = Splitter::new();
            let mut buf = [0u8; MAX_DATAGRAM + 1];
            loop {
                let (n, from) = sock.recv_from(&mut buf).unwrap();
                if n == 0 {
                    tx.send(fake).unwrap();
                    return;
                }
                let mut wire = [0u8; MAX_WIRE_FRAME];
                let m = cobs::encode(&buf[..n], &mut wire).unwrap();
                wire[m] = 0;
                fake.write_all(&wire[..=m]).unwrap();
                let mut back = [0u8; 4 * MAX_WIRE_FRAME];
                let k = fake.read(&mut back, Duration::ZERO).unwrap();
                for &b in &back[..k] {
                    if let Split::Frame(_) = split.push(b) {
                        let f = split.frame();
                        let len = cobs::decode_in_place(f).unwrap();
                        assert!(len <= MAX_RAW_FRAME);
                        sock.send_to(&f[..len], from).unwrap();
                    }
                }
            }
        });
        (addr, rx)
    }

    fn client(addr: std::net::SocketAddr) -> Device<UdpLink<UdpSocket>> {
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        s.connect(addr).unwrap();
        let t = Timeouts {
            request: Duration::from_millis(200),
            begin: Duration::from_millis(200),
            retries: 1,
        };
        Device::new(UdpLink::new(s, t))
    }

    fn hang_up(d: &Device<UdpLink<UdpSocket>>) {
        UdpSocket::send(d.link().transport(), &[]).unwrap();
    }

    #[test]
    fn cl1_default_port_and_address_forms() {
        assert_eq!(with_default_port("10.0.0.5").unwrap(), "10.0.0.5:7448");
        assert_eq!(with_default_port("board.lan:9000").unwrap(), "board.lan:9000");
        assert_eq!(with_default_port("fe80::1").unwrap(), "[fe80::1]:7448");
        assert_eq!(with_default_port("[fe80::1]").unwrap(), "[fe80::1]:7448");
        assert_eq!(with_default_port("[fe80::1]:7").unwrap(), "[fe80::1]:7");
        assert!(with_default_port("").is_err());
        assert!(with_default_port("h:0").is_err());
        assert!(with_default_port("h:x").is_err());
        assert!(with_default_port("[fe80::1").is_err());
    }

    #[test]
    fn cl1_info_and_upload_over_loopback_udp() {
        let (addr, back) = serve(FakeDevice::new("ecp5", "0.1.0", 16384, true));
        let mut d = client(addr);
        let i = d.info().unwrap();
        assert_eq!((i.chip.as_str(), i.capacity), ("ecp5", 16384));
        let img: Vec<u8> = (0..1300u32).map(|i| (i * 7) as u8).collect();
        d.upload(
            &img,
            UploadOptions {
                start: true,
                ..UploadOptions::default()
            },
            |_| {},
        )
        .unwrap();
        assert_eq!(d.info().unwrap().status, PayloadStatus::Running);
        let ev = d.take_events();
        assert!(
            ev.iter()
                .any(|e| matches!(e, Event::Publish { key, .. } if key == "sys/payload/status")),
            "{ev:?}"
        );
        hang_up(&d);
        let fake = back.recv().unwrap();
        assert_eq!(fake.installed(), Some(img.as_slice()));
    }

    #[test]
    fn cl1_a_lost_datagram_is_resent_and_noise_is_reported() {
        // The first request never arrives; its resend does.
        let fake = FakeDevice::new("ecp5", "0.1.0", 4096, true).with_script(Script {
            drop_request: vec![1],
            ..Script::default()
        });
        let (addr, back) = serve(fake);
        let mut d = client(addr);
        assert_eq!(d.info().unwrap().chip, "ecp5");
        hang_up(&d);
        assert_eq!(back.recv().unwrap().requests(), 2);

        // A datagram that is not a frame becomes noise.
        let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        s.connect(peer.local_addr().unwrap()).unwrap();
        peer.send_to(&[9, 9, 9], s.local_addr().unwrap()).unwrap();
        let mut link = UdpLink::new(s, Timeouts::default());
        let ev = link
            .next_event(Instant::now() + Duration::from_secs(1))
            .unwrap();
        assert_eq!(ev, Some(Event::Noise(vec![9, 9, 9])));
        assert_eq!(
            link.next_event(Instant::now() + Duration::from_millis(20))
                .unwrap(),
            None
        );
    }
}
