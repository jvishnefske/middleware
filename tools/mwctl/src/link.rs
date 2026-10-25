//! Request/response and publications over a byte stream.
//!
//! [`Transport`] is the only I/O seam: write bytes, and read with a
//! timeout. The serial port implements it in the binary; the tests use an
//! in-memory fake device.
//!
//! [`Link`] mirrors the web page's `DeviceClient`:
//!
//! * The first write is a lone `0x00`, so the device drops any partial
//!   frame left on the line.
//! * Each request attempt gets a fresh correlation id (1..=255, wrapping).
//!   An attempt that sees no matching response within its timeout is resent
//!   with a new id, up to `retries` more times; a late answer to an earlier
//!   attempt is then stale and discarded. Error responses are never retried.
//! * Publications that arrive meanwhile are queued as [`Event`]s for the
//!   caller ([`Link::take_events`]).
//!
//! Incoming bytes go through the shared frame [`Splitter`]. A delimited run
//! that does not decode is not necessarily noise: a ROM boot banner printed
//! at 74880 baud (garbage at 115200) may carry no `0x00`, so it is glued to
//! the front of the next real frame. [`decode_run`] therefore retries the
//! run's suffixes and keeps the longest one that decodes (COBS + CRC-16), so
//! the frame after the garbage is still received. Only runs with no valid
//! suffix are reported as [`Event::Noise`].

use crate::error::Error;
use middleware::Key;
use middleware::loader::Command;
use middleware::serial::frame::{
    self, Frame, FrameError, MAX_RAW_FRAME, MAX_WIRE_FRAME, Split, Splitter,
};
use middleware::transport::HandlerError;
use std::io;
use std::time::{Duration, Instant};

/// A byte stream to the device.
pub trait Transport {
    /// Writes all of `bytes`.
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()>;
    /// Waits at most `timeout` for incoming bytes and copies them into
    /// `buf`. `Ok(0)` means nothing arrived in time.
    fn read(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize>;
}

/// Something the device sent that is not the response being waited for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A publication (`sys/log`, `sys/payload/status`, payload topics).
    Publish {
        /// Topic.
        key: String,
        /// Published bytes.
        payload: Vec<u8>,
    },
    /// A delimited byte run that holds no valid frame (boot banner, line
    /// noise, console text).
    Noise(Vec<u8>),
}

/// An owned, decoded incoming frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incoming {
    /// A response.
    Response {
        /// Correlation id.
        id: u8,
        /// Body or device status.
        result: Result<Vec<u8>, HandlerError>,
    },
    /// A publication.
    Publish {
        /// Topic.
        key: String,
        /// Payload.
        payload: Vec<u8>,
    },
    /// A request from the device (the protocol has none; ignored).
    Request,
}

impl Incoming {
    fn from_frame(f: &Frame<'_>) -> Self {
        match f {
            Frame::Response { id, result } => Incoming::Response {
                id: *id,
                result: result.map(<[u8]>::to_vec),
            },
            Frame::Publish { key, payload } => Incoming::Publish {
                key: key.as_str().to_owned(),
                payload: payload.to_vec(),
            },
            Frame::Request { .. } => Incoming::Request,
        }
    }
}

/// Decodes one delimited COBS run, resynchronising past leading garbage:
/// tries the whole run, then each shorter suffix, and returns the number of
/// garbage bytes skipped with the first frame that decodes.
pub fn decode_run(run: &[u8]) -> Option<(usize, Incoming)> {
    let mut buf = [0u8; MAX_RUN];
    // Bounded by the run length (at most MAX_WIRE_FRAME).
    for skip in 0..run.len() {
        let tail = run.get(skip..)?;
        let dst = buf.get_mut(..tail.len())?;
        dst.copy_from_slice(tail);
        if let Ok(f) = frame::decode_wire(dst) {
            return Some((skip, Incoming::from_frame(&f)));
        }
    }
    None
}

/// Per-request timing: the web page's defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// Per-attempt response timeout.
    pub request: Duration,
    /// Floor for `sys/loader/begin`, which erases flash before answering.
    pub begin: Duration,
    /// Additional attempts after a timeout.
    pub retries: u32,
}

impl Default for Timeouts {
    fn default() -> Self {
        Timeouts {
            request: Duration::from_millis(1000),
            begin: Duration::from_millis(5000),
            retries: 1,
        }
    }
}

impl Timeouts {
    /// The per-attempt timeout for a request on `key`.
    pub fn for_key(&self, key: Key<'_>) -> Duration {
        if key == middleware::loader::keys::BEGIN {
            self.request.max(self.begin)
        } else {
            self.request
        }
    }
}

const RX_CHUNK: usize = 256;

/// Longest delimited run kept for decoding: room for a boot banner glued to
/// the front of a maximal frame. Longer runs are discarded as noise.
pub const MAX_RUN: usize = 4 * MAX_WIRE_FRAME;

/// Host endpoint of the serial protocol.
pub struct Link<T> {
    io: T,
    splitter: Splitter<MAX_RUN>,
    rx: [u8; RX_CHUNK],
    rx_pos: usize,
    rx_len: usize,
    next_id: u8,
    synced: bool,
    events: Vec<Event>,
    timeouts: Timeouts,
}

impl<T: Transport> Link<T> {
    /// Wraps a transport.
    pub fn new(io: T, timeouts: Timeouts) -> Self {
        Link {
            io,
            splitter: Splitter::new(),
            rx: [0; RX_CHUNK],
            rx_pos: 0,
            rx_len: 0,
            next_id: 1,
            synced: false,
            events: Vec::new(),
            timeouts,
        }
    }

    /// The transport.
    pub fn transport(&self) -> &T {
        &self.io
    }

    /// The timing in use.
    pub fn timeouts(&self) -> Timeouts {
        self.timeouts
    }

    /// Publications and noise received since the last call.
    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    fn write(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if !self.synced {
            self.io.write_all(&[0])?;
            self.synced = true;
        }
        self.io.write_all(bytes)?;
        Ok(())
    }

    fn take_id(&mut self) -> u8 {
        let id = self.next_id;
        self.next_id = if id == u8::MAX { 1 } else { id + 1 };
        id
    }

    /// Waits until `deadline` for the next decodable frame. `None` on
    /// timeout. Undecodable runs are queued as [`Event::Noise`].
    pub fn poll(&mut self, deadline: Instant) -> Result<Option<Incoming>, Error> {
        loop {
            if self.rx_pos == self.rx_len {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Ok(None);
                }
                let n = self.io.read(&mut self.rx, left)?;
                if n == 0 {
                    return Ok(None);
                }
                self.rx_pos = 0;
                self.rx_len = n.min(RX_CHUNK);
            }
            // Bounded by the bytes already read.
            while self.rx_pos < self.rx_len {
                let byte = self.rx.get(self.rx_pos).copied().unwrap_or(0);
                self.rx_pos += 1;
                match self.splitter.push(byte) {
                    Split::Pending => {}
                    Split::Overflow => self.events.push(Event::Noise(Vec::new())),
                    Split::Frame(_) => {
                        let run = self.splitter.frame();
                        match decode_run(run) {
                            Some((skip, inc)) => {
                                if skip > 0 {
                                    let garbage = run.get(..skip).unwrap_or_default().to_vec();
                                    self.events.push(Event::Noise(garbage));
                                }
                                return Ok(Some(inc));
                            }
                            None => self.events.push(Event::Noise(run.to_vec())),
                        }
                    }
                }
            }
        }
    }

    /// Sends a request and returns the response body.
    pub fn request(&mut self, key: Key<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
        let timeout = self.timeouts.for_key(key);
        let attempts = 1 + self.timeouts.retries;
        let mut scratch = [0u8; MAX_RAW_FRAME];
        let mut wire = [0u8; MAX_WIRE_FRAME];
        for _ in 0..attempts {
            let id = self.take_id();
            let n = frame::encode(
                &Frame::Request {
                    id,
                    key,
                    payload: body,
                },
                &mut scratch,
                &mut wire,
            )
            .map_err(Error::Frame)?;
            let bytes = wire.get(..n).ok_or(Error::Frame(FrameError::TooLarge))?;
            self.write(bytes)?;
            let deadline = Instant::now() + timeout;
            // Each iteration consumes a frame or ends at the deadline.
            while let Some(inc) = self.poll(deadline)? {
                match inc {
                    Incoming::Response { id: got, result } if got == id => {
                        return result.map_err(|status| Error::Device {
                            key: key.as_str().to_owned(),
                            status,
                        });
                    }
                    // Stale: the answer to an earlier, timed-out attempt.
                    Incoming::Response { .. } | Incoming::Request => {}
                    Incoming::Publish { key, payload } => {
                        self.events.push(Event::Publish { key, payload });
                    }
                }
            }
        }
        Err(Error::Timeout {
            key: key.as_str().to_owned(),
            attempts,
        })
    }

    /// Sends a loader/system command.
    pub fn command(&mut self, cmd: &Command<'_>) -> Result<Vec<u8>, Error> {
        let mut body = [0u8; 8 + middleware::loader::CHUNK_MAX];
        let n = cmd
            .encode_body(&mut body)
            .ok_or(Error::Frame(FrameError::TooLarge))?;
        let body = body.get(..n).ok_or(Error::Frame(FrameError::TooLarge))?;
        self.request(cmd.key(), body)
    }

    /// Waits until `deadline` for the next [`Event`] (publication or noise).
    /// Responses are discarded: nothing is outstanding outside
    /// [`Link::request`].
    pub fn next_event(&mut self, deadline: Instant) -> Result<Option<Event>, Error> {
        loop {
            if !self.events.is_empty() {
                return Ok(Some(self.events.remove(0)));
            }
            match self.poll(deadline)? {
                None if self.events.is_empty() => return Ok(None),
                None => return Ok(Some(self.events.remove(0))),
                Some(Incoming::Publish { key, payload }) => {
                    self.events.push(Event::Publish { key, payload });
                }
                Some(_) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::loader::keys;

    fn wire(f: &Frame<'_>) -> Vec<u8> {
        let mut scratch = [0u8; MAX_RAW_FRAME];
        let mut out = [0u8; MAX_WIRE_FRAME];
        let n = frame::encode(f, &mut scratch, &mut out).unwrap();
        out[..n].to_vec()
    }

    fn split_all(bytes: &[u8]) -> Vec<Option<(usize, Incoming)>> {
        let mut s: Splitter<MAX_RUN> = Splitter::new();
        let mut out = Vec::new();
        for &b in bytes {
            if let Split::Frame(_) = s.push(b) {
                out.push(decode_run(s.frame()));
            }
        }
        out
    }

    const LOG: Frame<'static> = Frame::Publish {
        key: keys::LOG,
        payload: b"boot",
    };

    #[test]
    fn garbage_without_delimiter_before_a_frame_is_skipped() {
        // A 74880-baud ROM banner read at 115200: arbitrary non-zero bytes
        // glued to the front of the first real frame.
        let mut stream: Vec<u8> = (0..300u32)
            .map(|i| ((i * 151 + 3) % 255 + 1) as u8)
            .collect();
        stream.extend(wire(&LOG));
        let got = split_all(&stream);
        assert_eq!(got.len(), 1);
        let (skip, inc) = got[0].clone().unwrap();
        assert_eq!(skip, 300);
        assert_eq!(
            inc,
            Incoming::Publish {
                key: "sys/log".into(),
                payload: b"boot".to_vec()
            }
        );
    }

    #[test]
    fn garbage_with_delimiters_is_noise_and_frames_still_decode() {
        let mut stream = b"ets Jan  8 2013,rst cause:2, boot mode:(3,6)\r\n\0\0garbage\0".to_vec();
        stream.extend(wire(&LOG));
        let got = split_all(&stream);
        assert_eq!(got.len(), 3);
        assert!(got[0].is_none() && got[1].is_none());
        assert_eq!(got[2].as_ref().map(|g| g.0), Some(0));
    }

    #[test]
    fn timeouts_extend_begin_only() {
        let t = Timeouts::default();
        assert_eq!(t.for_key(keys::BEGIN), Duration::from_millis(5000));
        assert_eq!(t.for_key(keys::CHUNK), Duration::from_millis(1000));
        let slow = Timeouts {
            request: Duration::from_secs(9),
            ..t
        };
        assert_eq!(slow.for_key(keys::BEGIN), Duration::from_secs(9));
    }
}
