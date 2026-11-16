//! UDP transport: one raw serial frame per datagram.
//!
//! A datagram already has boundaries, so there is no COBS stuffing and no
//! delimiter: each datagram carries exactly the raw (pre-COBS) bytes of a
//! [`serial::frame`](crate::serial::frame) frame,
//!
//! | kind          | datagram                                    |
//! |---------------|---------------------------------------------|
//! | `Request`  1  | `[1][id][key_len][key][payload][crc16 LE]`  |
//! | `Response` 2  | `[2][id][status][payload][crc16 LE]`        |
//! | `Publish`  3  | `[3][0][key_len][key][payload][crc16 LE]`   |
//!
//! with the frame's CRC-16/CCITT-FALSE kept. UDP's own checksum is optional
//! for IPv4 and the gateware decoder (`gateware-5a75b/`) checks the CRC
//! while the datagram streams in, so the frame stays self-checking. A
//! datagram that fails to decode is dropped. The well-known port is
//! [`PORT`]; a device answers a request to the address and port it came
//! from, and sends its publications to the last peer that sent it a frame.
//!
//! The framing ([`encode`], [`decode`]) is `no_std` and always built. The
//! host transport [`UdpTransport`] (feature `udp`, std and tokio)
//! implements [`Requester`](crate::Requester),
//! [`Publisher`](crate::Publisher) and [`Subscriber`](crate::Subscriber)
//! over one socket connected to one device.

use crate::serial::frame::{self, Frame, FrameError};

/// The well-known UDP port of a middleware node.
pub const PORT: u16 = 7448;

/// Largest datagram [`encode`] produces and [`decode`] accepts: one raw
/// serial frame, [`frame::MAX_RAW_FRAME`] bytes.
pub const MAX_DATAGRAM: usize = frame::MAX_RAW_FRAME;

/// Longest key a datagram carries: [`MAX_KEY_LEN`](crate::key::MAX_KEY_LEN),
/// which is also the gateware's key buffer.
pub const MAX_KEY: usize = crate::key::MAX_KEY_LEN;

/// Writes `frame` as one datagram into `out` and returns its length: exactly
/// [`frame::encode_raw`]'s bytes.
pub fn encode(frame: &Frame<'_>, out: &mut [u8]) -> Result<usize, FrameError> {
    frame::encode_raw(frame, out)
}

/// Parses one received datagram ([`frame::decode`]).
pub fn decode(datagram: &[u8]) -> Result<Frame<'_>, FrameError> {
    frame::decode(datagram)
}

#[cfg(feature = "udp")]
mod link;
#[cfg(feature = "udp")]
pub use link::{DEFAULT_TIMEOUT, RESEND_AFTER, UdpError, UdpTransport};

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec;
    use crate::crc::crc16;
    use crate::key::Key;
    use crate::transport::HandlerError;

    const K: Key<'static> = Key::from_static("demo/a");

    #[test]
    fn cl1_a_datagram_is_the_raw_serial_frame() {
        let frames = [
            Frame::Request {
                id: 9,
                key: K,
                payload: &[0, 0, 1],
            },
            Frame::Response {
                id: 9,
                result: Ok(b"ok"),
            },
            Frame::Response {
                id: 10,
                result: Err(HandlerError::NotFound),
            },
            Frame::Publish {
                key: K,
                payload: b"",
            },
        ];
        for f in frames {
            let mut a = [0u8; MAX_DATAGRAM];
            let mut b = [0u8; MAX_DATAGRAM];
            let n = encode(&f, &mut a).unwrap();
            let m = frame::encode_raw(&f, &mut b).unwrap();
            assert_eq!(a[..n], b[..m]);
            assert_eq!(decode(&a[..n]).unwrap(), f);
        }
    }

    #[test]
    fn cl1_publish_layout_is_byte_exact() {
        let mut out = [0u8; 32];
        let n = encode(
            &Frame::Publish {
                key: Key::from_static("a/b"),
                payload: &[7, 0],
            },
            &mut out,
        )
        .unwrap();
        let body = [3u8, 0, 3, b'a', b'/', b'b', 7, 0];
        let crc = crc16(&body).to_le_bytes();
        assert_eq!(out[..n], [&body[..], &crc[..]].concat()[..]);
    }

    #[test]
    fn cl1_keys_over_128_bytes_are_refused() {
        // A hand-built frame whose 129-byte key is otherwise well formed.
        let mut raw = vec![3u8, 0, (MAX_KEY + 1) as u8];
        raw.extend(core::iter::repeat_n(b'k', MAX_KEY + 1));
        let crc = crc16(&raw).to_le_bytes();
        raw.extend(crc);
        assert_eq!(decode(&raw), Err(FrameError::InvalidKey));
        let ok = "k".repeat(MAX_KEY);
        let f = Frame::Publish {
            key: Key::new(&ok).unwrap(),
            payload: b"",
        };
        let mut out = [0u8; MAX_DATAGRAM];
        let n = encode(&f, &mut out).unwrap();
        assert_eq!(decode(&out[..n]).unwrap(), f);
    }

    #[test]
    fn cl1_corrupt_and_oversized_datagrams_are_dropped() {
        let mut out = [0u8; MAX_DATAGRAM + 8];
        let n = encode(
            &Frame::Publish {
                key: K,
                payload: b"x",
            },
            &mut out,
        )
        .unwrap();
        out[3] ^= 0x40;
        assert_eq!(decode(&out[..n]), Err(FrameError::Crc));
        assert_eq!(decode(&out), Err(FrameError::TooLarge));
        assert_eq!(decode(&[]), Err(FrameError::Malformed));
    }
}
