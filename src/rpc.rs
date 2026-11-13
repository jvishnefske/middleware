//! Request/response over a datagram radio (ESP-NOW): the wire format and the
//! pure requester and responder logic (always built, `no_std`, no heap).
//!
//! A datagram radio loses, repeats and (rarely) reorders frames, and a
//! request must not run twice: a resent `sys/loader/commit` or
//! `sys/payload/start` would act again. This module holds the decisions;
//! `espnow::rpc` (feature `espnow`) runs them over an
//! [`EspNowRadio`](crate::espnow::EspNowRadio), the ESP base firmware runs
//! them in its radio task, and `mwctl` through a USB↔ESP-NOW bridge.
//!
//! # Frame format (version 1)
//!
//! One ESP-NOW frame per message, at most [`MAX_FRAME`] (250) bytes:
//!
//! | bytes | field                                                        |
//! |-------|--------------------------------------------------------------|
//! | 2     | magic [`MAGIC`] (`MR`)                                       |
//! | 1     | version [`VERSION`] (1)                                      |
//! | 1     | kind: [`KIND_REQUEST`] (1) or [`KIND_RESPONSE`] (2)          |
//! | 2     | correlation id, u16 little-endian                            |
//! | 6     | `to`: the addressee's MAC (requests: the responder; responses: the requester) |
//! | 1     | request: key length `n` (1..=[`MAX_KEY`]); response: status (0 = success, else [`HandlerError::code`]) |
//! | n     | request only: the key                                        |
//! | rest  | body                                                         |
//!
//! The magic differs from the publication frame's `MW`, so publication
//! receivers (`espnow::EspNowTransport`, the conagotchi peers) skip these
//! frames as foreign instead of counting them as malformed. The sender's
//! MAC is not carried: ESP-NOW reports it with every frame. `to` lets a
//! station drop a frame that reached it but is meant for someone else (a
//! broadcast, or a bridge that forwards everything it hears).
//!
//! Key and body share [`ROOM`] (237) bytes, so a `sys/loader/chunk` over the
//! air carries at most [`CHUNK_DATA`] (217) image bytes; a response body is
//! at most [`MAX_RESPONSE_BODY`] (237) bytes.
//!
//! # Requester ([`Call`])
//!
//! One request is outstanding at a time. It gets a fresh id, and every
//! retransmission after a timeout reuses that id, so the responder can tell
//! a resend from a new request. [`Call::answers`] accepts only a response
//! with that id, from the peer it was sent to, addressed to us; anything
//! else (a late answer to an earlier call, another station's traffic) is
//! stale. [`Call::retry`] bounds the attempts.
//!
//! # Responder ([`Replies`])
//!
//! [`Replies::admit`] decides what to do with a request from `(peer, id)`:
//!
//! * a request whose response is cached (the response was lost, or the
//!   request was duplicated or delayed) is **replayed**: the cached response
//!   is sent again and the handler does not run;
//! * a repeat of the request being executed (the requester timed out while
//!   a slow request, such as a flash erase, runs) is **busy**: dropped, its
//!   response follows;
//! * any other request while one executes is busy too (the requester
//!   retries);
//! * otherwise it is **executed** and becomes the one in flight until
//!   [`Replies::complete`] caches its response or [`Replies::abandon`] drops
//!   it. A request in flight for [`IN_FLIGHT_LIMIT_MS`] is abandoned by the
//!   next admission, so a lost completion cannot wedge the responder.
//!
//! The cache keeps the `N` most recent responses (least recently used
//! replaced), each body up to `B` bytes; a body longer than `B` is not
//! cached, so a repeat of that request runs again. A requester sends
//! sequentially, so a duplicate that arrives after `N` newer responses
//! would run again: that needs a frame delayed across `N` round trips.

use crate::key::Key;
use crate::transport::{HandlerError, Limits};

/// First two bytes of every request/response frame.
pub const MAGIC: [u8; 2] = *b"MR";
/// Frame format version.
pub const VERSION: u8 = 1;
/// Kind byte of a request.
pub const KIND_REQUEST: u8 = 1;
/// Kind byte of a response.
pub const KIND_RESPONSE: u8 = 2;
/// Bytes before the key length (requests) or status (responses).
pub const HEADER_LEN: usize = 12;
/// Largest ESP-NOW v1 frame.
pub const MAX_FRAME: usize = 250;
/// Longest key a request carries.
pub const MAX_KEY: usize = 64;
/// Key and body of a request share this many bytes; it is also the largest
/// response body.
pub const ROOM: usize = MAX_FRAME - HEADER_LEN - 1;
/// Largest response body.
pub const MAX_RESPONSE_BODY: usize = ROOM;
/// What a request carries, as transport [`Limits`].
pub const LIMITS: Limits = Limits::shared(MAX_KEY, ROOM);
/// Image bytes in one `sys/loader/chunk` request over the air (its body is
/// `[offset u32][data]`).
pub const CHUNK_DATA: usize = ROOM - crate::loader::keys::CHUNK.as_str().len() - 4;
/// A request in flight this long is abandoned by the next admission.
pub const IN_FLIGHT_LIMIT_MS: u32 = 10_000;

/// A 48-bit ESP-NOW (Wi-Fi MAC) address.
pub type Mac = [u8; 6];

const STATUS_OK: u8 = 0;

/// Why bytes are not a valid frame, or a message cannot be framed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcError {
    /// Shorter than the header (plus the declared key).
    TooShort,
    /// Longer than [`MAX_FRAME`], or longer than the output buffer.
    TooLong,
    /// The first two bytes are not [`MAGIC`].
    BadMagic,
    /// A version this code does not understand.
    BadVersion(u8),
    /// A kind byte other than request or response.
    BadKind(u8),
    /// Key length 0 or above [`MAX_KEY`].
    BadKeyLength,
    /// The key bytes are not a valid middleware key.
    BadKey,
    /// Encoding: the key is longer than [`MAX_KEY`].
    KeyTooLong,
    /// Encoding: key and body exceed [`ROOM`].
    BodyTooLarge,
}

/// One request or response; borrowed fields point into the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Message<'a> {
    /// A request for the handler of `key` on station `to`.
    Request {
        /// Correlation id; resends reuse it.
        id: u16,
        /// The responder's MAC.
        to: Mac,
        /// Target key.
        key: Key<'a>,
        /// Request body.
        body: &'a [u8],
    },
    /// The answer to request `id`.
    Response {
        /// Correlation id of the request.
        id: u16,
        /// The requester's MAC.
        to: Mac,
        /// Response body or the handler's error.
        result: Result<&'a [u8], HandlerError>,
    },
}

/// True if `bytes` start with the request/response [`MAGIC`].
pub fn is_rpc(bytes: &[u8]) -> bool {
    bytes.starts_with(&MAGIC)
}

fn put(out: &mut [u8], at: usize, bytes: &[u8]) -> Result<usize, RpcError> {
    let end = at + bytes.len();
    out.get_mut(at..end)
        .ok_or(RpcError::TooLong)?
        .copy_from_slice(bytes);
    Ok(end)
}

impl<'a> Message<'a> {
    /// The correlation id.
    pub const fn id(&self) -> u16 {
        match self {
            Message::Request { id, .. } | Message::Response { id, .. } => *id,
        }
    }

    /// The addressee.
    pub const fn to(&self) -> Mac {
        match self {
            Message::Request { to, .. } | Message::Response { to, .. } => *to,
        }
    }

    /// Writes the frame into `out` and returns its length.
    ///
    /// # Errors
    ///
    /// [`RpcError::KeyTooLong`] or [`RpcError::BodyTooLarge`] when the
    /// message does not fit one frame, [`RpcError::TooLong`] when it does
    /// not fit `out`.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, RpcError> {
        let (kind, id, to, third, key, body): (u8, u16, Mac, u8, &[u8], &[u8]) = match *self {
            Message::Request { id, to, key, body } => {
                let key = key.as_bytes();
                if key.len() > MAX_KEY {
                    return Err(RpcError::KeyTooLong);
                }
                let n = u8::try_from(key.len()).map_err(|_| RpcError::KeyTooLong)?;
                (KIND_REQUEST, id, to, n, key, body)
            }
            Message::Response { id, to, result } => match result {
                Ok(body) => (KIND_RESPONSE, id, to, STATUS_OK, &[], body),
                Err(e) => (KIND_RESPONSE, id, to, e.code(), &[], &[]),
            },
        };
        if key.len() + body.len() > ROOM {
            return Err(RpcError::BodyTooLarge);
        }
        let idb = id.to_le_bytes();
        let head = [MAGIC[0], MAGIC[1], VERSION, kind, idb[0], idb[1]];
        let at = put(out, 0, &head)?;
        let at = put(out, at, &to)?;
        let at = put(out, at, &[third])?;
        let at = put(out, at, key)?;
        put(out, at, body)
    }

    /// Parses a received frame.
    ///
    /// # Errors
    ///
    /// Any [`RpcError`] other than the encode-only `KeyTooLong` and
    /// `BodyTooLarge`. A status byte that is not a known code decodes as
    /// [`HandlerError::Internal`] (see [`HandlerError::from_code`]).
    pub fn decode(bytes: &'a [u8]) -> Result<Message<'a>, RpcError> {
        if bytes.len() > MAX_FRAME {
            return Err(RpcError::TooLong);
        }
        if bytes.len() >= 2 && !is_rpc(bytes) {
            return Err(RpcError::BadMagic);
        }
        let [
            _,
            _,
            version,
            kind,
            id0,
            id1,
            t0,
            t1,
            t2,
            t3,
            t4,
            t5,
            third,
            rest @ ..,
        ] = bytes
        else {
            return Err(RpcError::TooShort);
        };
        if *version != VERSION {
            return Err(RpcError::BadVersion(*version));
        }
        let id = u16::from_le_bytes([*id0, *id1]);
        let to = [*t0, *t1, *t2, *t3, *t4, *t5];
        match *kind {
            KIND_REQUEST => {
                let n = usize::from(*third);
                if n == 0 || n > MAX_KEY {
                    return Err(RpcError::BadKeyLength);
                }
                let (key, body) = rest.split_at_checked(n).ok_or(RpcError::TooShort)?;
                let key = core::str::from_utf8(key).map_err(|_| RpcError::BadKey)?;
                let key = Key::new(key).map_err(|_| RpcError::BadKey)?;
                Ok(Message::Request { id, to, key, body })
            }
            KIND_RESPONSE => {
                let result = if *third == STATUS_OK {
                    Ok(rest)
                } else {
                    Err(HandlerError::from_code(*third))
                };
                Ok(Message::Response { id, to, result })
            }
            other => Err(RpcError::BadKind(other)),
        }
    }
}

/// The id after `id` (wrapping).
pub const fn next_id(id: u16) -> u16 {
    id.wrapping_add(1)
}

/// The requester's view of one outstanding request (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Call {
    id: u16,
    peer: Mac,
    sent: u8,
    max: u8,
}

impl Call {
    /// The first attempt of request `id` to `peer`, allowing `max_attempts`
    /// sends in all (at least one).
    pub const fn first(id: u16, peer: Mac, max_attempts: u8) -> Self {
        let max = if max_attempts == 0 { 1 } else { max_attempts };
        Call {
            id,
            peer,
            sent: 1,
            max,
        }
    }

    /// The correlation id every attempt carries.
    pub const fn id(&self) -> u16 {
        self.id
    }

    /// The responder.
    pub const fn peer(&self) -> Mac {
        self.peer
    }

    /// Sends so far, including the first.
    pub const fn attempts(&self) -> u8 {
        self.sent
    }

    /// The next attempt after a timeout, or `None` when every attempt is
    /// spent.
    pub const fn retry(self) -> Option<Self> {
        if self.sent >= self.max {
            None
        } else {
            Some(Call {
                sent: self.sent + 1,
                ..self
            })
        }
    }

    /// The result `msg`, received from `src`, carries for this call: `Some`
    /// for a response with this call's id from its peer and addressed to
    /// `own` (when we know our own address), `None` for anything else.
    pub fn answers<'a>(
        &self,
        own: Option<Mac>,
        src: Mac,
        msg: &Message<'a>,
    ) -> Option<Result<&'a [u8], HandlerError>> {
        match *msg {
            Message::Response { id, to, result }
                if id == self.id && src == self.peer && own.is_none_or(|o| o == to) =>
            {
                Some(result)
            }
            _ => None,
        }
    }
}

/// What a responder does with a request ([`Replies::admit`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admit {
    /// Run the handler; the request is now in flight.
    Execute,
    /// Send the cached response again ([`Replies::cached`]); do not run the
    /// handler.
    Replay,
    /// Drop the request: it repeats the one in flight, or another one is in
    /// flight.
    Busy,
}

/// The request a responder is executing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InFlight {
    /// The requester.
    pub peer: Mac,
    /// Its correlation id.
    pub id: u16,
    /// When it was admitted (milliseconds, wrapping).
    pub since_ms: u32,
}

/// One cached response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cached<const B: usize> {
    peer: Mac,
    id: u16,
    /// 0 for success, else the error code.
    status: u8,
    len: u8,
    body: [u8; B],
    stamp: u32,
}

impl<const B: usize> Cached<B> {
    fn result(&self) -> Result<&[u8], HandlerError> {
        if self.status == STATUS_OK {
            Ok(self.body.get(..usize::from(self.len)).unwrap_or(&[]))
        } else {
            Err(HandlerError::from_code(self.status))
        }
    }
}

/// Responder state: the request in flight and the last `N` responses, each
/// body up to `B` bytes (`B` at most 255). A value type: every transition
/// returns the next state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Replies<const N: usize, const B: usize> {
    entries: [Option<Cached<B>>; N],
    clock: u32,
    in_flight: Option<InFlight>,
}

impl<const N: usize, const B: usize> Default for Replies<N, B> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize, const B: usize> Replies<N, B> {
    /// Compile-time check that a cached body length fits its `u8`.
    const B_OK: () = assert!(B <= 255, "Replies: B exceeds 255");

    /// Nothing cached, nothing in flight.
    pub const fn new() -> Self {
        let () = Self::B_OK;
        Replies {
            entries: [None; N],
            clock: 0,
            in_flight: None,
        }
    }

    /// The request being executed, if any.
    pub const fn in_flight(&self) -> Option<InFlight> {
        self.in_flight
    }

    fn find(&self, peer: Mac, id: u16) -> Option<usize> {
        self.entries
            .iter()
            .position(|e| e.as_ref().is_some_and(|c| c.peer == peer && c.id == id))
    }

    /// The cached response to `(peer, id)`.
    pub fn cached(&self, peer: Mac, id: u16) -> Option<Result<&[u8], HandlerError>> {
        let i = self.find(peer, id)?;
        self.entries.get(i)?.as_ref().map(Cached::result)
    }

    /// Decides what to do with request `id` from `peer` at `now_ms` (see
    /// the module docs).
    pub fn admit(self, peer: Mac, id: u16, now_ms: u32) -> (Self, Admit) {
        let mut next = self;
        if let Some(f) = next.in_flight
            && now_ms.wrapping_sub(f.since_ms) >= IN_FLIGHT_LIMIT_MS
        {
            next.in_flight = None;
        }
        if let Some(i) = next.find(peer, id) {
            next.clock = next.clock.wrapping_add(1);
            if let Some(Some(c)) = next.entries.get_mut(i) {
                c.stamp = next.clock;
            }
            return (next, Admit::Replay);
        }
        if next.in_flight.is_some() {
            return (next, Admit::Busy);
        }
        next.in_flight = Some(InFlight {
            peer,
            id,
            since_ms: now_ms,
        });
        (next, Admit::Execute)
    }

    /// Request `(peer, id)` finished with `result`: it is no longer in
    /// flight and its response is cached (unless the body exceeds `B`).
    /// A completion for a request not in flight (abandoned meanwhile) is
    /// still cached.
    pub fn complete(self, peer: Mac, id: u16, result: Result<&[u8], HandlerError>) -> Self {
        let mut next = self;
        if next.in_flight.is_some_and(|f| f.peer == peer && f.id == id) {
            next.in_flight = None;
        }
        let (status, body) = match result {
            Ok(b) => (STATUS_OK, b),
            Err(e) => (e.code(), &[][..]),
        };
        let Ok(len) = u8::try_from(body.len()) else {
            return next;
        };
        let mut stored = [0u8; B];
        let Some(slot) = stored.get_mut(..body.len()) else {
            return next;
        };
        slot.copy_from_slice(body);
        next.clock = next.clock.wrapping_add(1);
        let entry = Cached {
            peer,
            id,
            status,
            len,
            body: stored,
            stamp: next.clock,
        };
        let clock = next.clock;
        let victim = next
            .find(peer, id)
            .or_else(|| next.entries.iter().position(Option::is_none))
            .or_else(|| {
                // Least recently used: the largest age (wrapping).
                (0..N).max_by_key(|&i| {
                    next.entries
                        .get(i)
                        .copied()
                        .flatten()
                        .map_or(0, |c| clock.wrapping_sub(c.stamp))
                })
            });
        if let Some(slot) = victim.and_then(|i| next.entries.get_mut(i)) {
            *slot = Some(entry);
        }
        next
    }

    /// Drops the request in flight without caching anything (it could not
    /// be handed to the handler); a resend executes it again.
    pub const fn abandon(self) -> Self {
        Replies {
            in_flight: None,
            ..self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEV: Mac = [0xe4, 0xb0, 0x63, 0x81, 0xd3, 0x24];
    const HOST: Mac = [0x5c, 0xcf, 0x7f, 0x72, 0xf6, 0x54];
    const OTHER: Mac = [2, 0, 0, 0, 0, 9];
    const INFO: Key<'static> = Key::from_static("sys/info");

    fn enc(m: &Message<'_>) -> ([u8; MAX_FRAME], usize) {
        let mut out = [0u8; MAX_FRAME];
        let n = m.encode(&mut out).unwrap();
        (out, n)
    }

    #[test]
    fn o1_frames_round_trip_and_layout() {
        let req = Message::Request {
            id: 0x1234,
            to: DEV,
            key: INFO,
            body: b"ab",
        };
        let (out, n) = enc(&req);
        assert_eq!(
            &out[..n],
            &[
                b'M', b'R', 1, 1, 0x34, 0x12, 0xe4, 0xb0, 0x63, 0x81, 0xd3, 0x24, 8, b's', b'y',
                b's', b'/', b'i', b'n', b'f', b'o', b'a', b'b'
            ]
        );
        assert_eq!(Message::decode(&out[..n]), Ok(req));
        let ok = Message::Response {
            id: 7,
            to: HOST,
            result: Ok(b"xyz"),
        };
        let (out, n) = enc(&ok);
        assert_eq!(
            &out[..n],
            &[
                b'M', b'R', 1, 2, 7, 0, 0x5c, 0xcf, 0x7f, 0x72, 0xf6, 0x54, 0, b'x', b'y', b'z'
            ]
        );
        assert_eq!(Message::decode(&out[..n]), Ok(ok));
        let denied = Message::Response {
            id: 8,
            to: HOST,
            result: Err(HandlerError::Denied),
        };
        let (out, n) = enc(&denied);
        assert_eq!(n, HEADER_LEN + 1);
        assert_eq!(out[12], 5);
        assert_eq!(Message::decode(&out[..n]), Ok(denied));
        assert_eq!((denied.id(), denied.to(), req.to()), (8, HOST, DEV));
        assert!(is_rpc(&out[..n]));
        assert!(!is_rpc(b"MW\x01"));
    }

    #[test]
    fn o1_largest_messages_fill_one_frame_and_larger_ones_are_refused() {
        let key =
            Key::new("k123456789/k123456789/k123456789/k123456789/k123456789/k12345678").unwrap();
        assert_eq!(key.as_str().len(), MAX_KEY);
        let body = [0x5a; ROOM - MAX_KEY];
        let (_, n) = enc(&Message::Request {
            id: 1,
            to: DEV,
            key,
            body: &body,
        });
        assert_eq!(n, MAX_FRAME);
        let big = [0u8; ROOM + 1];
        let mut out = [0u8; MAX_FRAME];
        assert_eq!(
            Message::Request {
                id: 1,
                to: DEV,
                key,
                body: &big[..ROOM - MAX_KEY + 1]
            }
            .encode(&mut out),
            Err(RpcError::BodyTooLarge)
        );
        let resp = [0x11; MAX_RESPONSE_BODY];
        let (_, n) = enc(&Message::Response {
            id: 1,
            to: HOST,
            result: Ok(&resp),
        });
        assert_eq!(n, MAX_FRAME);
        assert_eq!(
            Message::Response {
                id: 1,
                to: HOST,
                result: Ok(&big)
            }
            .encode(&mut out),
            Err(RpcError::BodyTooLarge)
        );
        let long =
            Key::new("a/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .unwrap();
        assert_eq!(
            Message::Request {
                id: 1,
                to: DEV,
                key: long,
                body: &[]
            }
            .encode(&mut out),
            Err(RpcError::KeyTooLong)
        );
        let mut tiny = [0u8; 10];
        assert_eq!(
            Message::Request {
                id: 1,
                to: DEV,
                key: INFO,
                body: &[]
            }
            .encode(&mut tiny),
            Err(RpcError::TooLong)
        );
        // A chunk of CHUNK_DATA bytes is the largest that fits.
        assert_eq!(CHUNK_DATA, 217);
        let chunk = [0u8; 4 + CHUNK_DATA];
        let key = crate::loader::keys::CHUNK;
        let (_, n) = enc(&Message::Request {
            id: 1,
            to: DEV,
            key,
            body: &chunk,
        });
        assert_eq!(n, MAX_FRAME);
        assert!(LIMITS.fits(key.as_str().len(), 4 + CHUNK_DATA));
        assert!(!LIMITS.fits(key.as_str().len(), 5 + CHUNK_DATA));
    }

    #[test]
    fn o1_malformed_frames_are_rejected() {
        let (good, n) = enc(&Message::Request {
            id: 1,
            to: DEV,
            key: INFO,
            body: &[],
        });
        let good = &good[..n];
        assert_eq!(Message::decode(b"MR\x01"), Err(RpcError::TooShort));
        assert_eq!(Message::decode(b"MW\x01\x00\x00"), Err(RpcError::BadMagic));
        let mut v = good.to_vec();
        v[2] = 9;
        assert_eq!(Message::decode(&v), Err(RpcError::BadVersion(9)));
        let mut v = good.to_vec();
        v[3] = 3;
        assert_eq!(Message::decode(&v), Err(RpcError::BadKind(3)));
        let mut v = good.to_vec();
        v[12] = 0;
        assert_eq!(Message::decode(&v), Err(RpcError::BadKeyLength));
        let mut v = good.to_vec();
        v[12] = 65;
        assert_eq!(Message::decode(&v), Err(RpcError::BadKeyLength));
        let mut v = good.to_vec();
        v[12] = 40;
        assert_eq!(Message::decode(&v), Err(RpcError::TooShort));
        let mut v = good.to_vec();
        v[13] = b' ';
        assert_eq!(Message::decode(&v), Err(RpcError::BadKey));
        assert_eq!(
            Message::decode(&[b'M'; MAX_FRAME + 1]),
            Err(RpcError::TooLong)
        );
        // An unknown status reads as Internal.
        let mut v = enc(&Message::Response {
            id: 1,
            to: HOST,
            result: Ok(&[]),
        })
        .0[..13]
            .to_vec();
        v[12] = 200;
        assert_eq!(
            Message::decode(&v),
            Ok(Message::Response {
                id: 1,
                to: HOST,
                result: Err(HandlerError::Internal)
            })
        );
    }

    #[test]
    fn o2_a_call_accepts_only_its_own_response_and_bounds_attempts() {
        let call = Call::first(41, DEV, 3);
        let ok = Message::Response {
            id: 41,
            to: HOST,
            result: Ok(b"x"),
        };
        assert_eq!(call.answers(Some(HOST), DEV, &ok), Some(Ok(&b"x"[..])));
        assert_eq!(
            call.answers(None, DEV, &ok),
            Some(Ok(&b"x"[..])),
            "own address unknown"
        );
        assert_eq!(
            call.answers(Some(HOST), OTHER, &ok),
            None,
            "from another station"
        );
        assert_eq!(
            call.answers(Some(OTHER), DEV, &ok),
            None,
            "addressed to someone else"
        );
        let stale = Message::Response {
            id: 40,
            to: HOST,
            result: Ok(b"x"),
        };
        assert_eq!(call.answers(Some(HOST), DEV, &stale), None, "earlier call");
        let req = Message::Request {
            id: 41,
            to: HOST,
            key: INFO,
            body: &[],
        };
        assert_eq!(call.answers(Some(HOST), DEV, &req), None);
        let r1 = call.retry().unwrap();
        assert_eq!((r1.id(), r1.peer(), r1.attempts()), (41, DEV, 2));
        let r2 = r1.retry().unwrap();
        assert_eq!(r2.attempts(), 3);
        assert_eq!(r2.retry(), None);
        assert_eq!(Call::first(1, DEV, 0).retry(), None, "at least one attempt");
        assert_eq!(next_id(u16::MAX), 0);
    }

    type R = Replies<3, 8>;

    #[test]
    fn o3_a_request_runs_once_and_repeats_replay_its_response() {
        let (r, a) = R::new().admit(HOST, 1, 0);
        assert_eq!(a, Admit::Execute);
        assert_eq!(
            r.in_flight(),
            Some(InFlight {
                peer: HOST,
                id: 1,
                since_ms: 0
            })
        );
        let (r, a) = r.admit(HOST, 1, 5);
        assert_eq!(a, Admit::Busy, "a resend while it runs");
        let (r, a) = r.admit(OTHER, 9, 5);
        assert_eq!(a, Admit::Busy, "another request while one runs");
        let r = r.complete(HOST, 1, Ok(b"done"));
        assert_eq!(r.in_flight(), None);
        let (r, a) = r.admit(HOST, 1, 6);
        assert_eq!(a, Admit::Replay);
        assert_eq!(r.cached(HOST, 1), Some(Ok(&b"done"[..])));
        assert_eq!(r.cached(OTHER, 1), None, "per requester");
        let (r, a) = r.admit(HOST, 2, 7);
        assert_eq!(a, Admit::Execute);
        let r = r.complete(HOST, 2, Err(HandlerError::Denied));
        let (r, a) = r.admit(HOST, 2, 8);
        assert_eq!(a, Admit::Replay);
        assert_eq!(r.cached(HOST, 2), Some(Err(HandlerError::Denied)));
        // A delayed duplicate of the first request still replays.
        assert_eq!(r.admit(HOST, 1, 9).1, Admit::Replay);
    }

    #[test]
    fn o3_the_cache_is_bounded_least_recently_used_and_skips_large_bodies() {
        let mut r = R::new();
        for id in 1..=3u16 {
            let (n, a) = r.admit(HOST, id, 0);
            assert_eq!(a, Admit::Execute);
            r = n.complete(HOST, id, Ok(&[id as u8]));
        }
        // Touch 1, so 2 is the least recently used.
        r = r.admit(HOST, 1, 0).0;
        r = r.admit(HOST, 4, 0).0.complete(HOST, 4, Ok(&[4]));
        assert_eq!(r.cached(HOST, 2), None);
        for id in [1u16, 3, 4] {
            assert_eq!(r.cached(HOST, id), Some(Ok(&[id as u8][..])));
        }
        // A body over B bytes is not cached: a repeat runs again.
        let (n, _) = r.admit(HOST, 5, 0);
        let r = n.complete(HOST, 5, Ok(&[0u8; 9]));
        assert_eq!(r.in_flight(), None);
        assert_eq!(r.admit(HOST, 5, 0).1, Admit::Execute);
    }

    #[test]
    fn o3_an_abandoned_or_stale_request_frees_the_responder() {
        let (r, _) = R::new().admit(HOST, 1, 100);
        assert_eq!(
            r.abandon().admit(HOST, 1, 101).1,
            Admit::Execute,
            "abandoned: a resend runs"
        );
        assert_eq!(
            r.admit(HOST, 2, 100 + IN_FLIGHT_LIMIT_MS - 1).1,
            Admit::Busy
        );
        let (late, a) = r.admit(HOST, 2, 100 + IN_FLIGHT_LIMIT_MS);
        assert_eq!(a, Admit::Execute, "a request in flight too long is dropped");
        // Its completion, arriving late, is cached without disturbing the new one.
        let late = late.complete(HOST, 1, Ok(&[]));
        assert_eq!(late.in_flight().map(|f| f.id), Some(2));
        assert_eq!(late.cached(HOST, 1), Some(Ok(&[][..])));
        // The clock wraps.
        let (r, _) = R::new().admit(HOST, 1, u32::MAX - 5);
        assert_eq!(r.admit(HOST, 2, 10).1, Admit::Busy);
    }
}
