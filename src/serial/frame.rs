//! Serial frame layout and the byte-stream frame splitter.
//!
//! Before COBS encoding a frame is:
//!
//! | kind          | layout                                                   |
//! |---------------|----------------------------------------------------------|
//! | `Request`  1  | `[1][id][key_len][key][payload][crc16 LE]`               |
//! | `Response` 2  | `[2][id][status][payload][crc16 LE]`                     |
//! | `Publish`  3  | `[3][0][key_len][key][payload][crc16 LE]`                |
//!
//! `status` is 0 for success, otherwise [`HandlerError::code`]. The CRC is
//! [`crc16`] over every preceding byte. The encoded frame is COBS-stuffed and
//! followed by one `0x00` delimiter, so bytes that are not frames (a ROM
//! bootloader banner, for example) fail the CRC and are skipped.

use super::cobs;
use crate::crc::crc16;
use crate::key::Key;
use crate::transport::HandlerError;

/// Largest raw (pre-COBS) frame accepted by [`decode`] and produced by
/// [`encode`]: a 512-byte loader chunk plus headers and a maximal key.
pub const MAX_RAW_FRAME: usize = 1024;

/// Largest frame on the wire including COBS overhead and delimiter.
pub const MAX_WIRE_FRAME: usize = cobs::max_encoded_len(MAX_RAW_FRAME) + 1;

const KIND_REQUEST: u8 = 1;
const KIND_RESPONSE: u8 = 2;
const KIND_PUBLISH: u8 = 3;
const STATUS_OK: u8 = 0;

/// A decoded frame; borrows the buffer it was decoded from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frame<'a> {
    /// A request to the handler for `key`, correlated by `id`.
    Request {
        /// Correlation id echoed by the response.
        id: u8,
        /// Target key.
        key: Key<'a>,
        /// Request body.
        payload: &'a [u8],
    },
    /// The answer to the request with the same `id`.
    Response {
        /// Correlation id of the request.
        id: u8,
        /// Response body or the handler's error.
        result: Result<&'a [u8], HandlerError>,
    },
    /// A publication on `key`.
    Publish {
        /// Topic.
        key: Key<'a>,
        /// Published bytes.
        payload: &'a [u8],
    },
}

/// Why a frame could not be encoded or decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// The output buffer is too small, or the frame exceeds [`MAX_RAW_FRAME`].
    TooLarge,
    /// The COBS encoding is invalid.
    Cobs,
    /// The frame is shorter than its header or its lengths are inconsistent.
    Malformed,
    /// The checksum does not match.
    Crc,
    /// The kind byte is unknown.
    UnknownKind(u8),
    /// The key bytes are not a valid [`Key`].
    InvalidKey,
}

struct Writer<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl Writer<'_> {
    fn put(&mut self, bytes: &[u8]) -> Result<(), FrameError> {
        let end = self.len + bytes.len();
        self.buf
            .get_mut(self.len..end)
            .ok_or(FrameError::TooLarge)?
            .copy_from_slice(bytes);
        self.len = end;
        Ok(())
    }
}

/// Writes the raw (pre-COBS) bytes of `frame`, including the CRC, into
/// `raw`. Returns the length.
pub fn encode_raw(frame: &Frame<'_>, raw: &mut [u8]) -> Result<usize, FrameError> {
    let limit = raw.len().min(MAX_RAW_FRAME);
    let mut w = Writer {
        buf: raw.get_mut(..limit).ok_or(FrameError::TooLarge)?,
        len: 0,
    };
    match frame {
        Frame::Request { id, key, payload } => {
            w.put(&[KIND_REQUEST, *id, key_len(key)?])?;
            w.put(key.as_bytes())?;
            w.put(payload)?;
        }
        Frame::Response { id, result } => match result {
            Ok(payload) => {
                w.put(&[KIND_RESPONSE, *id, STATUS_OK])?;
                w.put(payload)?;
            }
            Err(e) => w.put(&[KIND_RESPONSE, *id, e.code()])?,
        },
        Frame::Publish { key, payload } => {
            w.put(&[KIND_PUBLISH, 0, key_len(key)?])?;
            w.put(key.as_bytes())?;
            w.put(payload)?;
        }
    }
    let crc = crc16(w.buf.get(..w.len).ok_or(FrameError::TooLarge)?);
    w.put(&crc.to_le_bytes())?;
    Ok(w.len)
}

fn key_len(key: &Key<'_>) -> Result<u8, FrameError> {
    u8::try_from(key.as_bytes().len()).map_err(|_| FrameError::InvalidKey)
}

/// Encodes `frame` for the wire: raw bytes into `scratch`, then COBS into
/// `out` followed by the `0x00` delimiter. Returns the bytes written to `out`.
pub fn encode(frame: &Frame<'_>, scratch: &mut [u8], out: &mut [u8]) -> Result<usize, FrameError> {
    let n = encode_raw(frame, scratch)?;
    let raw = scratch.get(..n).ok_or(FrameError::TooLarge)?;
    let m = cobs::encode(raw, out).ok_or(FrameError::TooLarge)?;
    *out.get_mut(m).ok_or(FrameError::TooLarge)? = 0;
    Ok(m + 1)
}

/// Parses raw (already COBS-decoded) frame bytes.
pub fn decode(raw: &[u8]) -> Result<Frame<'_>, FrameError> {
    if raw.len() > MAX_RAW_FRAME {
        return Err(FrameError::TooLarge);
    }
    let body_len = raw.len().checked_sub(2).ok_or(FrameError::Malformed)?;
    let (body, crc) = raw.split_at(body_len);
    if crc16(body) != u16::from_le_bytes([crc[0], crc[1]]) {
        return Err(FrameError::Crc);
    }
    let [kind, id, third, rest @ ..] = body else {
        return Err(FrameError::Malformed);
    };
    match *kind {
        KIND_RESPONSE => Ok(Frame::Response {
            id: *id,
            result: if *third == STATUS_OK {
                Ok(rest)
            } else {
                Err(HandlerError::from_code(*third))
            },
        }),
        KIND_REQUEST | KIND_PUBLISH => {
            let (key, payload) = rest
                .split_at_checked(usize::from(*third))
                .ok_or(FrameError::Malformed)?;
            let key = core::str::from_utf8(key).map_err(|_| FrameError::InvalidKey)?;
            let key = Key::new(key).map_err(|_| FrameError::InvalidKey)?;
            Ok(if *kind == KIND_REQUEST {
                Frame::Request {
                    id: *id,
                    key,
                    payload,
                }
            } else {
                Frame::Publish { key, payload }
            })
        }
        other => Err(FrameError::UnknownKind(other)),
    }
}

/// COBS-decodes one delimited frame in place and parses it.
pub fn decode_wire(cobs_bytes: &mut [u8]) -> Result<Frame<'_>, FrameError> {
    let n = cobs::decode_in_place(cobs_bytes).ok_or(FrameError::Cobs)?;
    decode(cobs_bytes.get(..n).ok_or(FrameError::Cobs)?)
}

/// Outcome of feeding one byte to a [`Splitter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Split {
    /// More bytes are needed.
    Pending,
    /// A delimiter arrived; [`Splitter::frame`] holds this many COBS bytes.
    Frame(usize),
    /// The frame exceeded the buffer and was discarded up to the next
    /// delimiter.
    Overflow,
}

/// Splits a byte stream at `0x00` delimiters into an `N`-byte buffer.
///
/// Mutated in place rather than returned by value: it is fed once per
/// received byte, and moving an `N`-byte buffer per byte would dominate the
/// cost.
pub struct Splitter<const N: usize> {
    buf: [u8; N],
    len: usize,
    ready: usize,
    overflowed: bool,
}

impl<const N: usize> Default for Splitter<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Splitter<N> {
    /// An empty splitter.
    pub const fn new() -> Self {
        Self {
            buf: [0; N],
            len: 0,
            ready: 0,
            overflowed: false,
        }
    }

    /// Feeds one byte. After [`Split::Frame`] the frame stays available from
    /// [`Splitter::frame`] until the next call to `push`. Empty frames
    /// (consecutive delimiters) are ignored.
    pub fn push(&mut self, byte: u8) -> Split {
        if self.ready != 0 {
            self.ready = 0;
            self.len = 0;
        }
        if byte == 0 {
            let len = core::mem::take(&mut self.len);
            if core::mem::take(&mut self.overflowed) {
                return Split::Overflow;
            }
            if len == 0 {
                return Split::Pending;
            }
            self.len = len;
            self.ready = len;
            return Split::Frame(len);
        }
        if self.overflowed {
            return Split::Pending;
        }
        match self.buf.get_mut(self.len) {
            Some(slot) => {
                *slot = byte;
                self.len += 1;
            }
            None => self.overflowed = true,
        }
        Split::Pending
    }

    /// The COBS bytes of the frame completed by the last `push`, or an empty
    /// slice.
    pub fn frame(&mut self) -> &mut [u8] {
        let ready = self.ready;
        self.buf.get_mut(..ready).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const K: Key<'static> = Key::from_static("sys/info");

    fn wire(frame: &Frame<'_>) -> ([u8; MAX_WIRE_FRAME], usize) {
        let mut scratch = [0u8; MAX_RAW_FRAME];
        let mut out = [0u8; MAX_WIRE_FRAME];
        let n = encode(frame, &mut scratch, &mut out).unwrap();
        (out, n)
    }

    #[test]
    fn every_kind_round_trips_through_the_wire() {
        let frames = [
            Frame::Request {
                id: 7,
                key: K,
                payload: &[0, 1, 2, 0],
            },
            Frame::Response {
                id: 7,
                result: Ok(&[9, 0, 9]),
            },
            Frame::Response {
                id: 8,
                result: Err(HandlerError::NotFound),
            },
            Frame::Publish {
                key: K,
                payload: b"",
            },
        ];
        for f in frames {
            let (mut out, n) = wire(&f);
            assert_eq!(out[n - 1], 0);
            assert!(!out[..n - 1].contains(&0));
            assert_eq!(decode_wire(&mut out[..n - 1]).unwrap(), f);
        }
    }

    #[test]
    fn corrupt_frames_are_rejected() {
        let f = Frame::Publish {
            key: K,
            payload: b"abc",
        };
        let mut raw = [0u8; 64];
        let n = encode_raw(&f, &mut raw).unwrap();
        raw[4] ^= 0x01;
        assert_eq!(decode(&raw[..n]), Err(FrameError::Crc));
        assert_eq!(decode(&[1]), Err(FrameError::Malformed));
        let mut bad_kind = [9u8, 0, 0, 0, 0];
        let crc = crc16(&bad_kind[..3]).to_le_bytes();
        bad_kind[3..].copy_from_slice(&crc);
        assert_eq!(decode(&bad_kind), Err(FrameError::UnknownKind(9)));
        let mut long_key = [3u8, 0, 200, b'a', 0, 0];
        let crc = crc16(&long_key[..4]).to_le_bytes();
        long_key[4..].copy_from_slice(&crc);
        assert_eq!(decode(&long_key), Err(FrameError::Malformed));
        assert_eq!(decode_wire(&mut [0x05, 1]), Err(FrameError::Cobs));
    }

    #[test]
    fn oversized_frames_are_rejected() {
        let big = [1u8; MAX_RAW_FRAME];
        let f = Frame::Publish {
            key: K,
            payload: &big,
        };
        let mut raw = [0u8; MAX_RAW_FRAME + 64];
        assert_eq!(encode_raw(&f, &mut raw), Err(FrameError::TooLarge));
        assert_eq!(decode(&raw), Err(FrameError::TooLarge));
    }

    #[test]
    fn splitter_finds_frames_amid_noise() {
        let f = Frame::Publish {
            key: K,
            payload: b"hi",
        };
        let (out, n) = wire(&f);
        let mut stream = [0u8; 128];
        let banner = b"ESP-ROM:esp32c3\r\n";
        stream[..banner.len()].copy_from_slice(banner);
        stream[banner.len()] = 0;
        stream[banner.len() + 1..banner.len() + 1 + n].copy_from_slice(&out[..n]);
        let total = banner.len() + 1 + n;

        let mut s: Splitter<256> = Splitter::new();
        let mut decoded = 0;
        let mut rejected = 0;
        for &b in &stream[..total] {
            if let Split::Frame(_) = s.push(b) {
                match decode_wire(s.frame()) {
                    Ok(got) => {
                        assert_eq!(got, f);
                        decoded += 1;
                    }
                    Err(_) => rejected += 1,
                }
            }
        }
        assert_eq!((decoded, rejected), (1, 1));
    }

    #[test]
    fn splitter_discards_overflow_until_delimiter() {
        let mut s: Splitter<4> = Splitter::new();
        for b in [1, 2, 3, 4, 5, 6] {
            assert_eq!(s.push(b), Split::Pending);
        }
        assert_eq!(s.push(0), Split::Overflow);
        assert_eq!(s.push(0), Split::Pending);
        s.push(7);
        assert_eq!(s.push(0), Split::Frame(1));
        assert_eq!(s.frame(), &[7]);
        s.push(8);
        assert_eq!(s.frame(), &[] as &[u8]);
    }
}
