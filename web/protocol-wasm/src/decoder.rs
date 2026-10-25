//! Streaming decoder for the serial line: frames plus console text.
//!
//! The browser reads arbitrary byte chunks from Web Serial. [`StreamDecoder`]
//! cuts them into [`Event`]s:
//!
//! * [`Event::Frame`] — a frame that passed COBS decoding and the CRC-16
//!   check of [`middleware::serial::frame::decode_wire`].
//! * [`Event::Text`] — bytes that cannot be a frame (ROM boot banners,
//!   `println!` output), passed through for the console.
//! * [`Event::Invalid`] — a delimited segment that looked like a frame but
//!   failed to decode, or exceeded [`MAX_WIRE_FRAME`].
//!
//! # Telling text from frames
//!
//! The frame layout itself is not reimplemented here; only the segmentation
//! policy is. Every raw frame starts with a non-zero kind byte (1, 2 or 3),
//! so its COBS encoding starts with a code byte ≥ 2 followed by that kind
//! byte. A segment whose second byte is anything else is text: it is
//! streamed out as it arrives (at a newline, at the end of each
//! [`StreamDecoder::push`], or at a `0x00`). After a newline the decoder
//! starts a fresh segment, so a frame that follows a text line without a
//! leading `0x00` is still recognised. A frame glued to the end of a partial
//! text line (no newline, no `0x00` between) is reported as text; the
//! firmware avoids this by sending `0x00` before its first frame.
//!
//! The decoder is fed once per received chunk and mutates its segment buffer
//! in place, like [`middleware::serial::frame::Splitter`]: rebuilding the
//! buffer per byte would dominate the cost.

use middleware::HandlerError;
use middleware::serial::frame::{self, Frame, FrameError, MAX_WIRE_FRAME};

/// Which of the three frame kinds was received.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// A request (normally host → device).
    Request,
    /// A response correlated by id.
    Response,
    /// A publication.
    Publish,
}

impl FrameKind {
    /// Lower-case name used in the JS objects.
    pub const fn name(self) -> &'static str {
        match self {
            FrameKind::Request => "request",
            FrameKind::Response => "response",
            FrameKind::Publish => "publish",
        }
    }
}

/// An owned copy of a decoded [`Frame`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedFrame {
    /// Frame kind.
    pub kind: FrameKind,
    /// Correlation id (0 for publications).
    pub id: u8,
    /// Key for requests and publications.
    pub key: Option<String>,
    /// Body; empty for an error response.
    pub payload: Vec<u8>,
    /// For responses: `Ok(())` or the handler error. `None` otherwise.
    pub status: Option<Result<(), HandlerError>>,
}

impl OwnedFrame {
    /// Copies a borrowed frame.
    pub fn from_frame(frame: &Frame<'_>) -> Self {
        match frame {
            Frame::Request { id, key, payload } => OwnedFrame {
                kind: FrameKind::Request,
                id: *id,
                key: Some(key.as_str().to_owned()),
                payload: payload.to_vec(),
                status: None,
            },
            Frame::Response { id, result } => OwnedFrame {
                kind: FrameKind::Response,
                id: *id,
                key: None,
                payload: result.map(<[u8]>::to_vec).unwrap_or_default(),
                status: Some(result.map(|_| ())),
            },
            Frame::Publish { key, payload } => OwnedFrame {
                kind: FrameKind::Publish,
                id: 0,
                key: Some(key.as_str().to_owned()),
                payload: payload.to_vec(),
                status: None,
            },
        }
    }
}

/// Why a delimited segment was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidReason {
    /// The frame layer refused it.
    Frame(FrameError),
    /// The segment grew past [`MAX_WIRE_FRAME`] and was discarded up to the
    /// next delimiter.
    Overflow,
}

impl InvalidReason {
    /// Short human-readable reason.
    pub fn describe(self) -> String {
        match self {
            InvalidReason::Frame(FrameError::TooLarge) => "too large".into(),
            InvalidReason::Frame(FrameError::Cobs) => "bad COBS".into(),
            InvalidReason::Frame(FrameError::Malformed) => "malformed".into(),
            InvalidReason::Frame(FrameError::Crc) => "CRC mismatch".into(),
            InvalidReason::Frame(FrameError::UnknownKind(k)) => format!("unknown kind {k}"),
            InvalidReason::Frame(FrameError::InvalidKey) => "invalid key".into(),
            InvalidReason::Overflow => "overflow".into(),
        }
    }
}

/// One decoded item from the line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A valid frame.
    Frame(OwnedFrame),
    /// Console text (lossy UTF-8).
    Text(String),
    /// A rejected segment and its length in bytes.
    Invalid {
        /// Why it was rejected.
        reason: InvalidReason,
        /// Bytes discarded.
        len: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Not yet known whether the segment is a frame or text.
    Undecided,
    /// The segment is text; stream it out.
    Text,
    /// The segment overflowed; drop bytes until the next delimiter.
    Discard,
}

/// Longest text run held before it is emitted even without a newline.
const TEXT_RUN_MAX: usize = 256;

/// Splits a serial byte stream into [`Event`]s. See the module docs.
#[derive(Debug, Clone)]
pub struct StreamDecoder {
    seg: Vec<u8>,
    mode: Mode,
    discarded: usize,
}

impl Default for StreamDecoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether a segment that starts with these two bytes can be a frame.
fn may_be_frame(code: u8, kind: u8) -> bool {
    code >= 2 && (1..=3).contains(&kind)
}

impl StreamDecoder {
    /// An empty decoder.
    pub fn new() -> Self {
        Self {
            seg: Vec::with_capacity(MAX_WIRE_FRAME),
            mode: Mode::Undecided,
            discarded: 0,
        }
    }

    /// Feeds a chunk and returns the events it completed. Any text pending
    /// at the end of the chunk is emitted too, so console output is not
    /// delayed until the next newline.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Event> {
        let mut out = Vec::new();
        for &b in bytes {
            self.push_byte(b, &mut out);
        }
        if self.mode == Mode::Text {
            self.emit_text(&mut out);
        }
        out
    }

    fn emit_text(&mut self, out: &mut Vec<Event>) {
        if !self.seg.is_empty() {
            out.push(Event::Text(String::from_utf8_lossy(&self.seg).into_owned()));
            self.seg.clear();
        }
    }

    fn push_byte(&mut self, b: u8, out: &mut Vec<Event>) {
        if b == 0 {
            match self.mode {
                Mode::Text => self.emit_text(out),
                Mode::Discard => out.push(Event::Invalid {
                    reason: InvalidReason::Overflow,
                    len: core::mem::take(&mut self.discarded),
                }),
                Mode::Undecided if self.seg.is_empty() => {}
                Mode::Undecided => {
                    let len = self.seg.len();
                    out.push(match frame::decode_wire(&mut self.seg) {
                        Ok(f) => Event::Frame(OwnedFrame::from_frame(&f)),
                        Err(e) => Event::Invalid {
                            reason: InvalidReason::Frame(e),
                            len,
                        },
                    });
                }
            }
            self.seg.clear();
            self.mode = Mode::Undecided;
            return;
        }
        match self.mode {
            Mode::Discard => self.discarded += 1,
            Mode::Text => {
                self.seg.push(b);
                if b == b'\n' {
                    self.emit_text(out);
                    self.mode = Mode::Undecided;
                } else if self.seg.len() >= TEXT_RUN_MAX {
                    self.emit_text(out);
                }
            }
            Mode::Undecided => {
                self.seg.push(b);
                if let [code, kind] = self.seg[..] {
                    if !may_be_frame(code, kind) {
                        self.mode = Mode::Text;
                        if b == b'\n' {
                            self.emit_text(out);
                            self.mode = Mode::Undecided;
                        }
                    }
                } else if self.seg.len() >= MAX_WIRE_FRAME {
                    self.discarded = self.seg.len();
                    self.seg.clear();
                    self.mode = Mode::Discard;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode;

    fn texts(events: &[Event]) -> String {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    fn frames(events: &[Event]) -> Vec<&OwnedFrame> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Frame(f) => Some(f),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn frames_amid_boot_banner_noise() {
        let mut stream =
            b"ESP-ROM:esp32c3-api1-20210207\r\nrst:0x1 (POWERON),boot:0xc\r\n".to_vec();
        stream.extend(encode::publish("sys/log", b"hello").unwrap());
        stream.extend(b"I (31) boot: ESP-IDF\n");
        stream.extend(encode::response(9, Ok(&[1, 2, 3])).unwrap());
        let mut d = StreamDecoder::new();
        let ev = d.push(&stream);
        assert_eq!(
            texts(&ev),
            "ESP-ROM:esp32c3-api1-20210207\r\nrst:0x1 (POWERON),boot:0xc\r\nI (31) boot: ESP-IDF\n"
        );
        let f = frames(&ev);
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].kind, FrameKind::Publish);
        assert_eq!(f[0].key.as_deref(), Some("sys/log"));
        assert_eq!(f[0].payload, b"hello");
        assert_eq!(
            (f[1].kind, f[1].id, f[1].status),
            (FrameKind::Response, 9, Some(Ok(())))
        );
        assert_eq!(f[1].payload, [1, 2, 3]);
    }

    #[test]
    fn byte_at_a_time_matches_whole_chunk() {
        let mut stream = b"boot\n".to_vec();
        stream.extend(encode::request(4, "app/x", &[0, 0, 7]).unwrap());
        stream.extend(encode::response(4, Err(HandlerError::NotFound)).unwrap());
        let mut d = StreamDecoder::new();
        let ev: Vec<Event> = stream.iter().flat_map(|b| d.push(&[*b])).collect();
        assert_eq!(texts(&ev), "boot\n");
        let f = frames(&ev);
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].payload, [0, 0, 7]);
        assert_eq!(f[1].status, Some(Err(HandlerError::NotFound)));
        assert!(f[1].payload.is_empty());
    }

    #[test]
    fn partial_text_is_flushed_per_chunk() {
        let mut d = StreamDecoder::new();
        assert_eq!(d.push(b"> "), vec![Event::Text("> ".into())]);
        assert_eq!(d.push(b"ok\n"), vec![Event::Text("ok\n".into())]);
    }

    #[test]
    fn corrupt_and_oversized_segments_are_invalid() {
        let mut wire = encode::publish("a/b", b"xyz").unwrap();
        wire[4] ^= 0x40;
        let mut d = StreamDecoder::new();
        let ev = d.push(&wire);
        assert!(matches!(
            ev[..],
            [Event::Invalid {
                reason: InvalidReason::Frame(_),
                ..
            }]
        ));

        let mut big = vec![0x05u8, 0x01];
        big.resize(MAX_WIRE_FRAME + 10, 0x07);
        big.push(0);
        let ev = d.push(&big);
        assert_eq!(
            ev,
            vec![Event::Invalid {
                reason: InvalidReason::Overflow,
                len: MAX_WIRE_FRAME + 10
            }]
        );
        // Still in sync afterwards.
        let ev = d.push(&encode::publish("a/b", b"").unwrap());
        assert_eq!(frames(&ev).len(), 1);
    }

    #[test]
    fn reasons_are_described() {
        assert_eq!(
            InvalidReason::Frame(FrameError::UnknownKind(9)).describe(),
            "unknown kind 9"
        );
        assert_eq!(InvalidReason::Overflow.describe(), "overflow");
    }
}
