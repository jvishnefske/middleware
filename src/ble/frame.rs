//! Fragmentation and reassembly of middleware messages over ATT (pure).
//!
//! One ATT write or notification carries at most `ATT_MTU - 3` bytes, so a
//! message (a publication, a request or a response) is split into fragments
//! that each start with a fixed [`HEADER_LEN`]-byte header:
//!
//! | offset | size | field                                              |
//! |--------|------|----------------------------------------------------|
//! | 0      | 1    | message id (the correlation id for requests)       |
//! | 1      | 1    | fragment index, starting at 0                      |
//! | 2      | 2    | total message length in bytes, little-endian       |
//! | 4      | ..   | fragment data                                      |
//!
//! Every fragment repeats the total length, so the receiver knows when the
//! message is complete without first/last flags. A message therefore holds
//! at most `u16::MAX` bytes and [`MAX_FRAGMENTS`] fragments. Only an empty
//! message may have an empty fragment.
//!
//! Message bodies:
//!
//! * publication: the payload as is;
//! * request: `[key length u8][key bytes][payload]` — the key lets one shared
//!   RPC characteristic dispatch every key ([`parse_request`]);
//! * response: `[status u8][payload]`, where status [`STATUS_OK`] means
//!   success and any other value is a [`HandlerError::code`]
//!   ([`parse_response`]).
//!
//! [`Fragmenter`] computes fragment `i` of a message on demand from borrowed
//! parts (no copy of the whole message). [`Reassembler`] is a value-typed
//! state machine: [`Reassembler::accept`] consumes the state and returns
//! either the next state or the completed [`Message`]. Out-of-order,
//! duplicate, interleaved, oversized and malformed fragments are rejected
//! with a [`FrameError`]; the state is consumed, so the caller starts over
//! with a fresh [`Reassembler`].

use crate::key::Key;
use crate::transport::HandlerError;

/// Bytes of header at the start of every fragment.
pub const HEADER_LEN: usize = 4;

/// Most fragments one message may be split into (the index is one byte).
pub const MAX_FRAGMENTS: usize = 256;

/// Response status byte meaning success.
pub const STATUS_OK: u8 = 0;

/// Why a fragment or message was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// Shorter than a header, data overruns the announced total, an empty
    /// fragment in a non-empty message, or a total length that changed
    /// between fragments of one message; or a body that does not decode.
    Malformed,
    /// A fragment index ahead of the next expected one, or a message that
    /// did not start at index 0.
    OutOfOrder,
    /// A fragment index that was already accepted.
    Duplicate,
    /// A fragment of a different message arrived while one was in progress.
    Interleaved,
    /// The announced total length exceeds the reassembly buffer.
    Oversized,
    /// The message is longer than `u16::MAX` bytes or needs more than
    /// [`MAX_FRAGMENTS`] fragments.
    TooLarge,
    /// The ATT payload cannot hold a header plus at least one data byte.
    MtuTooSmall,
    /// The output buffer cannot hold the fragment.
    BufferTooSmall,
    /// A request named a key that is not valid UTF-8 or not a valid [`Key`].
    InvalidKey,
}

/// The decoded fragment header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FragmentHeader {
    /// Message id; requests and their responses share it.
    pub id: u8,
    /// Position of this fragment in the message.
    pub index: u8,
    /// Length of the whole message body.
    pub total_len: u16,
}

impl FragmentHeader {
    /// Serialises the header.
    pub const fn encode(self) -> [u8; HEADER_LEN] {
        let t = self.total_len.to_le_bytes();
        [self.id, self.index, t[0], t[1]]
    }

    /// Splits a fragment into its header and data.
    pub fn parse(fragment: &[u8]) -> Result<(Self, &[u8]), FrameError> {
        match fragment {
            [id, index, lo, hi, data @ ..] => Ok((
                FragmentHeader {
                    id: *id,
                    index: *index,
                    total_len: u16::from_le_bytes([*lo, *hi]),
                },
                data,
            )),
            _ => Err(FrameError::Malformed),
        }
    }
}

/// Splits one message into fragments of at most `att_payload` bytes.
///
/// The message is described by borrowed parts (an optional lead byte and two
/// slices) so request and response bodies are never assembled in a buffer.
#[derive(Debug, Clone, Copy)]
pub struct Fragmenter<'a> {
    id: u8,
    lead: Option<u8>,
    parts: [&'a [u8]; 2],
    total: u16,
    chunk: usize,
}

impl<'a> Fragmenter<'a> {
    fn new(
        id: u8,
        lead: Option<u8>,
        parts: [&'a [u8]; 2],
        att_payload: usize,
    ) -> Result<Self, FrameError> {
        let chunk = att_payload
            .checked_sub(HEADER_LEN)
            .filter(|c| *c > 0)
            .ok_or(FrameError::MtuTooSmall)?;
        let len = usize::from(lead.is_some()) + parts[0].len() + parts[1].len();
        let total = u16::try_from(len).map_err(|_| FrameError::TooLarge)?;
        if len.div_ceil(chunk) > MAX_FRAGMENTS {
            return Err(FrameError::TooLarge);
        }
        Ok(Fragmenter {
            id,
            lead,
            parts,
            total,
            chunk,
        })
    }

    /// A publication: the body is `payload` itself.
    pub fn message(id: u8, payload: &'a [u8], att_payload: usize) -> Result<Self, FrameError> {
        Self::new(id, None, [payload, &[]], att_payload)
    }

    /// A request on `key` with correlation id `id`.
    pub fn request(
        id: u8,
        key: Key<'a>,
        payload: &'a [u8],
        att_payload: usize,
    ) -> Result<Self, FrameError> {
        // Keys are at most MAX_KEY_LEN (128) bytes, so the length fits a u8.
        let key_len = u8::try_from(key.as_bytes().len()).map_err(|_| FrameError::InvalidKey)?;
        Self::new(id, Some(key_len), [key.as_bytes(), payload], att_payload)
    }

    /// The response to request `id`: a payload, or a handler error carried as
    /// its [`HandlerError::code`].
    pub fn response(
        id: u8,
        outcome: Result<&'a [u8], HandlerError>,
        att_payload: usize,
    ) -> Result<Self, FrameError> {
        match outcome {
            Ok(payload) => Self::new(id, Some(STATUS_OK), [payload, &[]], att_payload),
            Err(e) => Self::new(id, Some(e.code()), [&[], &[]], att_payload),
        }
    }

    /// The message id written into every fragment header.
    pub const fn id(&self) -> u8 {
        self.id
    }

    /// Length of the message body.
    pub const fn total_len(&self) -> usize {
        self.total as usize
    }

    /// Number of fragments (at least one, even for an empty message).
    pub const fn count(&self) -> usize {
        let n = self.total_len().div_ceil(self.chunk);
        if n == 0 { 1 } else { n }
    }

    /// Writes fragment `index` into `out` and returns the filled prefix.
    pub fn fragment<'o>(&self, index: usize, out: &'o mut [u8]) -> Result<&'o [u8], FrameError> {
        if index >= self.count() {
            return Err(FrameError::OutOfOrder);
        }
        let index_byte = u8::try_from(index).map_err(|_| FrameError::TooLarge)?;
        let start = index * self.chunk;
        let len = self.chunk.min(self.total_len().saturating_sub(start));
        let frame = out
            .get_mut(..HEADER_LEN + len)
            .ok_or(FrameError::BufferTooSmall)?;
        let (head, data) = frame.split_at_mut(HEADER_LEN);
        head.copy_from_slice(
            &FragmentHeader {
                id: self.id,
                index: index_byte,
                total_len: self.total,
            }
            .encode(),
        );
        let lead = self.lead.as_slice();
        let body = lead
            .iter()
            .chain(self.parts[0])
            .chain(self.parts[1])
            .skip(start);
        for (d, s) in data.iter_mut().zip(body) {
            *d = *s;
        }
        Ok(frame)
    }
}

/// A completely reassembled message body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message<const N: usize> {
    id: u8,
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> Message<N> {
    /// The message id (correlation id for requests and responses).
    pub const fn id(&self) -> u8 {
        self.id
    }

    /// The message body.
    pub fn as_bytes(&self) -> &[u8] {
        self.buf.get(..self.len).unwrap_or(&[])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Receiving {
        id: u8,
        next: u16,
        total: u16,
        filled: usize,
    },
}

/// Outcome of a successful [`Reassembler::accept`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress<const N: usize> {
    /// More fragments are needed; continue with this state.
    Partial(Reassembler<N>),
    /// The message is complete.
    Complete(Message<N>),
}

/// Reassembles one message of at most `N` bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reassembler<const N: usize> {
    buf: [u8; N],
    state: State,
}

impl<const N: usize> Default for Reassembler<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Reassembler<N> {
    /// An idle reassembler waiting for fragment 0 of a message.
    pub const fn new() -> Self {
        Reassembler {
            buf: [0; N],
            state: State::Idle,
        }
    }

    /// True while a message is partially received.
    pub const fn in_progress(&self) -> bool {
        matches!(self.state, State::Receiving { .. })
    }

    /// Feeds one fragment, consuming the state.
    pub fn accept(self, fragment: &[u8]) -> Result<Progress<N>, FrameError> {
        let (h, data) = FragmentHeader::parse(fragment)?;
        let total = usize::from(h.total_len);
        if total > N {
            return Err(FrameError::Oversized);
        }
        let (id, next, filled) = match self.state {
            State::Idle if h.index != 0 => return Err(FrameError::OutOfOrder),
            State::Idle => (h.id, 0u16, 0usize),
            State::Receiving {
                id,
                next,
                total: t,
                filled,
            } => {
                if h.id != id {
                    return Err(FrameError::Interleaved);
                }
                if h.total_len != t {
                    return Err(FrameError::Malformed);
                }
                let index = u16::from(h.index);
                if index < next {
                    return Err(FrameError::Duplicate);
                }
                if index > next {
                    return Err(FrameError::OutOfOrder);
                }
                (id, next, filled)
            }
        };
        if data.is_empty() && total != 0 {
            return Err(FrameError::Malformed);
        }
        let end = filled + data.len();
        if end > total {
            return Err(FrameError::Malformed);
        }
        let mut buf = self.buf;
        buf.get_mut(filled..end)
            .ok_or(FrameError::Malformed)?
            .copy_from_slice(data);
        Ok(if end == total {
            Progress::Complete(Message {
                id,
                buf,
                len: total,
            })
        } else {
            Progress::Partial(Reassembler {
                buf,
                state: State::Receiving {
                    id,
                    next: next + 1,
                    total: h.total_len,
                    filled: end,
                },
            })
        })
    }
}

/// Decodes a request body into its key and payload.
pub fn parse_request(body: &[u8]) -> Result<(Key<'_>, &[u8]), FrameError> {
    let (len, rest) = body.split_first().ok_or(FrameError::Malformed)?;
    let (key, payload) = rest
        .split_at_checked(usize::from(*len))
        .ok_or(FrameError::Malformed)?;
    let key = core::str::from_utf8(key).map_err(|_| FrameError::InvalidKey)?;
    let key = Key::new(key).map_err(|_| FrameError::InvalidKey)?;
    Ok((key, payload))
}

/// Decodes a response body into the payload or the handler error it carries.
pub fn parse_response(body: &[u8]) -> Result<Result<&[u8], HandlerError>, FrameError> {
    match body.split_first() {
        Some((&STATUS_OK, payload)) => Ok(Ok(payload)),
        Some((code, _)) => Ok(Err(HandlerError::from_code(*code))),
        None => Err(FrameError::Malformed),
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    const K: Key<'static> = Key::from_static("sensor/temperature");

    fn fragments(f: &Fragmenter<'_>) -> Vec<Vec<u8>> {
        let mut out = [0u8; 64];
        (0..f.count())
            .map(|i| f.fragment(i, &mut out).unwrap().to_vec())
            .collect()
    }

    fn reassemble<const N: usize>(frags: &[Vec<u8>]) -> Result<Message<N>, FrameError> {
        let mut r = Reassembler::<N>::new();
        for (i, f) in frags.iter().enumerate() {
            match r.accept(f)? {
                Progress::Partial(next) => r = next,
                Progress::Complete(m) => {
                    assert_eq!(i, frags.len() - 1, "completed early");
                    return Ok(m);
                }
            }
        }
        panic!("never completed");
    }

    #[test]
    fn header_round_trip() {
        let h = FragmentHeader {
            id: 7,
            index: 3,
            total_len: 0x1234,
        };
        let mut bytes = h.encode().to_vec();
        bytes.push(0xAA);
        assert_eq!(FragmentHeader::parse(&bytes), Ok((h, &[0xAA][..])));
        assert_eq!(
            FragmentHeader::parse(&[1, 2, 3]),
            Err(FrameError::Malformed)
        );
    }

    #[test]
    fn large_message_is_fragmented_and_reassembled() {
        let payload: Vec<u8> = (0..100u8).collect();
        // Default ATT MTU 23 → 20-byte payload → 16 data bytes per fragment.
        let f = Fragmenter::message(9, &payload, 20).unwrap();
        assert_eq!(f.count(), 7);
        let frags = fragments(&f);
        assert!(frags.iter().all(|x| x.len() <= 20));
        let m = reassemble::<128>(&frags).unwrap();
        assert_eq!(m.id(), 9);
        assert_eq!(m.as_bytes(), &payload[..]);
    }

    #[test]
    fn empty_message_is_one_fragment() {
        let f = Fragmenter::message(1, &[], 20).unwrap();
        assert_eq!(f.count(), 1);
        let m = reassemble::<4>(&fragments(&f)).unwrap();
        assert!(m.as_bytes().is_empty());
    }

    #[test]
    fn request_carries_key_and_payload() {
        let f = Fragmenter::request(42, K, b"hello world", 20).unwrap();
        let m = reassemble::<64>(&fragments(&f)).unwrap();
        assert_eq!(m.id(), 42);
        assert_eq!(parse_request(m.as_bytes()), Ok((K, &b"hello world"[..])));
    }

    #[test]
    fn response_carries_status() {
        let ok = Fragmenter::response(3, Ok(b"data"), 20).unwrap();
        let m = reassemble::<16>(&fragments(&ok)).unwrap();
        assert_eq!(m.as_bytes()[0], STATUS_OK);
        assert_eq!(parse_response(m.as_bytes()), Ok(Ok(&b"data"[..])));

        let err = Fragmenter::response(3, Err(HandlerError::NotFound), 20).unwrap();
        let m = reassemble::<16>(&fragments(&err)).unwrap();
        assert_eq!(m.as_bytes(), &[HandlerError::NotFound.code()]);
        assert_eq!(
            parse_response(m.as_bytes()),
            Ok(Err(HandlerError::NotFound))
        );
        assert_eq!(parse_response(&[]), Err(FrameError::Malformed));
    }

    #[test]
    fn malformed_request_bodies_are_rejected() {
        assert_eq!(parse_request(&[]), Err(FrameError::Malformed));
        assert_eq!(parse_request(&[5, b'a']), Err(FrameError::Malformed));
        assert_eq!(parse_request(&[1, b' ']), Err(FrameError::InvalidKey));
        assert_eq!(parse_request(&[1, 0xFF]), Err(FrameError::InvalidKey));
    }

    #[test]
    fn out_of_order_fragment_is_rejected() {
        let payload = [1u8; 40];
        let frags = fragments(&Fragmenter::message(1, &payload, 20).unwrap());
        // Starting mid-message.
        assert_eq!(
            Reassembler::<64>::new().accept(&frags[1]),
            Err(FrameError::OutOfOrder)
        );
        // Skipping a fragment.
        let Progress::Partial(r) = Reassembler::<64>::new().accept(&frags[0]).unwrap() else {
            panic!()
        };
        assert_eq!(r.accept(&frags[2]), Err(FrameError::OutOfOrder));
    }

    #[test]
    fn duplicate_fragment_is_rejected() {
        let payload = [1u8; 40];
        let frags = fragments(&Fragmenter::message(1, &payload, 20).unwrap());
        let Progress::Partial(r) = Reassembler::<64>::new().accept(&frags[0]).unwrap() else {
            panic!()
        };
        assert!(r.in_progress());
        assert_eq!(r.accept(&frags[0]), Err(FrameError::Duplicate));
    }

    #[test]
    fn oversized_message_is_rejected() {
        let payload = [1u8; 40];
        let frags = fragments(&Fragmenter::message(1, &payload, 20).unwrap());
        assert_eq!(
            Reassembler::<39>::new().accept(&frags[0]),
            Err(FrameError::Oversized)
        );
    }

    #[test]
    fn interleaved_and_inconsistent_fragments_are_rejected() {
        let a = fragments(&Fragmenter::message(1, &[1u8; 40], 20).unwrap());
        let b = fragments(&Fragmenter::message(2, &[2u8; 40], 20).unwrap());
        let Progress::Partial(r) = Reassembler::<64>::new().accept(&a[0]).unwrap() else {
            panic!()
        };
        assert_eq!(r.clone().accept(&b[1]), Err(FrameError::Interleaved));
        // Same id, different total length.
        let mut bad = a[1].clone();
        bad[2] = 41;
        assert_eq!(r.accept(&bad), Err(FrameError::Malformed));
    }

    #[test]
    fn malformed_fragments_are_rejected() {
        let r = Reassembler::<64>::new();
        assert_eq!(r.clone().accept(&[]), Err(FrameError::Malformed));
        // Data longer than the announced total.
        let mut f = FragmentHeader {
            id: 0,
            index: 0,
            total_len: 2,
        }
        .encode()
        .to_vec();
        f.extend_from_slice(&[1, 2, 3]);
        assert_eq!(r.clone().accept(&f), Err(FrameError::Malformed));
        // Empty fragment in a non-empty message.
        let f = FragmentHeader {
            id: 0,
            index: 0,
            total_len: 2,
        }
        .encode();
        assert_eq!(r.accept(&f), Err(FrameError::Malformed));
    }

    #[test]
    fn fragmenter_rejects_bad_sizes() {
        assert_eq!(
            Fragmenter::message(0, b"x", 4).unwrap_err(),
            FrameError::MtuTooSmall
        );
        let big = [0u8; 300];
        // 5-byte payload → 1 data byte per fragment → 300 fragments > 256.
        assert_eq!(
            Fragmenter::message(0, &big, 5).unwrap_err(),
            FrameError::TooLarge
        );
        let f = Fragmenter::message(0, &big, 20).unwrap();
        let mut small = [0u8; 8];
        assert_eq!(f.fragment(0, &mut small), Err(FrameError::BufferTooSmall));
        let mut out = [0u8; 20];
        assert_eq!(f.fragment(f.count(), &mut out), Err(FrameError::OutOfOrder));
    }
}
