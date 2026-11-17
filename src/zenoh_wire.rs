//! Pure encodings the Zenoh backends put on the wire next to Zenoh's own
//! framing: attachments, the reply-error payload and the reserved key
//! expressions. Always built, `no_std`, no heap, no I/O; shared by the host
//! backend (re-exported as `zenoh::wire`, feature `zenoh`) and the
//! board's client `zenoh_lite` (feature `zenoh-lite`), so both put the same
//! bytes on the wire.
//!
//! # Attachments
//!
//! Every attachment starts with `"mw"` and a kind byte; integers are little
//! endian.
//!
//! | kind | on | bytes after the kind |
//! |------|----|----------------------|
//! | 1 | publication (0.4) | sender id 8, sequence 1 |
//! | 2 | publication | sender id 8, sequence 1, stream id 4 |
//! | 3 | publication with trace context | as kind 2, then [`TraceContext`] 25 |
//! | `0x10` | query | requester's sender id 8, deadline in ms 4 |
//! | `0x11` | successful reply | replier's sender id 8 |
//! | `0x12` | query with trace context | as `0x10`, then [`TraceContext`] 25 |
//!
//! A publication's sequence numbers one **stream**: one key published
//! through one transport value. The stream id ([`stream_id`]) tells streams
//! of one sender apart, so a subscriber computes exact per-key gaps with
//! [`SeenTable::check_stream`](crate::dedup::SeenTable::check_stream).
//! Kind 1 attachments (from 0.4 publishers) still decode, without a stream
//! and so without gaps. Anything else is ignored: the sample or query then
//! carries no metadata.
//!
//! Kinds 3 and `0x12` add the W3C trace context of the publishing or
//! requesting span (trace id 16, parent span id 8, flags 1: the binary
//! form of a `traceparent` header, version 00), so a subscriber or
//! responder can continue the trace ([`decode_trace`]). A sender writes
//! them only when told to (`ZenohTransport::with_trace_context`): a
//! decoder older than these kinds ignores the whole attachment, losing
//! the sender and sequence, so they are for networks whose nodes all
//! decode them (middleware 0.6 and later, `zenoh_lite` included).
//!
//! # Reply errors
//!
//! Zenoh reply errors carry no attachment, so the error payload is the
//! [`HandlerError::code`] byte followed by the replier's 8-byte sender id
//! ([`encode_reply_error`]). Only the first byte is required: a bare code
//! (0.4 responders) decodes with no sender. Zenoh itself answers a query
//! that timed out with a reply error whose payload is the 7 bytes
//! `Timeout`; [`is_timeout_error`] tells it apart, since a middleware error
//! payload is 1 or 9 bytes.
//!
//! # Reserved key expressions
//!
//! * `@mw/node/<zid16>/<key>`: the per-node queryable of a responder on
//!   `key` ([`node_key_buf`]); `zid16` is the node's [`SenderId`] as 16
//!   lowercase hex digits ([`zid16_hex`]).
//! * `@mw/svc/<kind>/<instance>`: the liveliness token and record
//!   queryable of an announced service ([`parse_service_key`]).
//!
//! The leading `@mw` chunk is a Zenoh verbatim chunk: it is matched only by
//! itself, never by `*` or `**`, so no application subscription or query
//! (application keys never start with `@`) reaches these. The host backend
//! adds `String`-returning forms of these builders in `zenoh::wire`.

use crate::dedup::Sequence;
use crate::discovery::ServiceKind;
use crate::key::{Key, MAX_KEY_LEN};
use crate::transport::{HandlerError, SampleMeta, SenderId, Target};

const MAGIC: [u8; 2] = [b'm', b'w'];
const KIND_PUB_V1: u8 = 1;
const KIND_PUB: u8 = 2;
const KIND_PUB_TRACED: u8 = 3;
const KIND_QUERY: u8 = 0x10;
const KIND_REPLY: u8 = 0x11;
const KIND_QUERY_TRACED: u8 = 0x12;

/// Length of a publication attachment.
pub const PUB_ATTACHMENT_LEN: usize = 3 + 8 + 1 + 4;
const PUB_V1_ATTACHMENT_LEN: usize = 3 + 8 + 1;
/// Length of a query attachment.
pub const QUERY_ATTACHMENT_LEN: usize = 3 + 8 + 4;
/// Length of a [`TraceContext`] on the wire.
pub const TRACE_CONTEXT_LEN: usize = 16 + 8 + 1;
/// Length of a publication attachment with trace context (kind 3).
pub const PUB_TRACED_ATTACHMENT_LEN: usize = PUB_ATTACHMENT_LEN + TRACE_CONTEXT_LEN;
/// Length of a query attachment with trace context (kind `0x12`).
pub const QUERY_TRACED_ATTACHMENT_LEN: usize = QUERY_ATTACHMENT_LEN + TRACE_CONTEXT_LEN;
/// Length of a reply attachment.
pub const REPLY_ATTACHMENT_LEN: usize = 3 + 8;
/// Length of a reply-error payload.
pub const REPLY_ERROR_LEN: usize = 1 + 8;

/// Prefix of the per-node queryables.
pub const NODE_PREFIX: &str = "@mw/node/";
/// Prefix of service liveliness tokens and record queryables.
pub const SERVICE_PREFIX: &str = "@mw/svc/";
/// Longest per-node key expression: the prefix, 16 hex digits, `/` and the
/// longest key.
pub const NODE_KEY_MAX: usize = NODE_PREFIX.len() + 16 + 1 + MAX_KEY_LEN;

/// A W3C trace context (`traceparent`, version 00) in binary: the trace,
/// the span the receiver's span continues from, and the trace flags (bit
/// 0: sampled).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraceContext {
    pub trace_id: [u8; 16],
    pub span_id: [u8; 8],
    pub flags: u8,
}

impl TraceContext {
    /// A valid context: neither id all zero (W3C trace context 3.2.2).
    pub fn is_valid(&self) -> bool {
        self.trace_id != [0; 16] && self.span_id != [0; 8]
    }

    /// The sampled flag.
    pub const fn sampled(&self) -> bool {
        self.flags & 1 != 0
    }

    fn encode(&self) -> [u8; TRACE_CONTEXT_LEN] {
        let mut out = [0u8; TRACE_CONTEXT_LEN];
        out[..16].copy_from_slice(&self.trace_id);
        out[16..24].copy_from_slice(&self.span_id);
        out[24] = self.flags;
        out
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let bytes: &[u8; TRACE_CONTEXT_LEN] = bytes.try_into().ok()?;
        let ctx = TraceContext {
            trace_id: bytes[..16].try_into().ok()?,
            span_id: bytes[16..24].try_into().ok()?,
            flags: bytes[24],
        };
        ctx.is_valid().then_some(ctx)
    }

    /// The `traceparent` header value: `00-<trace 32 hex>-<span 16 hex>-<flags 2 hex>`.
    pub fn to_traceparent(&self) -> [u8; 55] {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = [b'-'; 55];
        out[0] = b'0';
        out[1] = b'0';
        let mut put = |at: usize, bytes: &[u8]| {
            for (i, b) in bytes.iter().enumerate() {
                out[at + 2 * i] = HEX[usize::from(b >> 4)];
                out[at + 2 * i + 1] = HEX[usize::from(b & 0x0f)];
            }
        };
        put(3, &self.trace_id);
        put(36, &self.span_id);
        put(53, &[self.flags]);
        out
    }

    /// Parses a version-00 `traceparent` value (lowercase hex, as the
    /// specification requires); `None` if malformed or invalid.
    pub fn parse_traceparent(s: &str) -> Option<Self> {
        let b = s.as_bytes();
        if b.len() != 55 || &b[..3] != b"00-" || b[35] != b'-' || b[52] != b'-' {
            return None;
        }
        fn nibble(c: u8) -> Option<u8> {
            match c {
                b'0'..=b'9' => Some(c - b'0'),
                b'a'..=b'f' => Some(c - b'a' + 10),
                _ => None,
            }
        }
        fn hex<const N: usize>(s: &[u8]) -> Option<[u8; N]> {
            let mut out = [0u8; N];
            for (o, pair) in out.iter_mut().zip(s.chunks_exact(2)) {
                *o = (nibble(pair[0])? << 4) | nibble(pair[1])?;
            }
            Some(out)
        }
        let ctx = TraceContext {
            trace_id: hex::<16>(&b[3..35])?,
            span_id: hex::<8>(&b[36..52])?,
            flags: hex::<1>(&b[53..55])?[0],
        };
        ctx.is_valid().then_some(ctx)
    }
}

/// What a publication attachment says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PubHeader {
    /// The publishing node.
    pub sender: SenderId,
    /// Its sequence number in `stream`.
    pub seq: u8,
    /// The stream (key and transport value), absent in 0.4 attachments.
    pub stream: Option<u32>,
}

impl PubHeader {
    /// The sample metadata this header gives before gap computation.
    pub const fn meta(&self) -> SampleMeta {
        SampleMeta::from_sender(self.sender).with_seq(self.seq)
    }
}

const FNV32_OFFSET: u32 = 0x811c_9dc5;
const FNV32_PRIME: u32 = 0x0100_0193;

const fn fnv32_extend(mut hash: u32, bytes: &[u8]) -> u32 {
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u32;
        hash = hash.wrapping_mul(FNV32_PRIME);
        i += 1;
    }
    hash
}

/// The stream id of `key` published by the transport value `origin`:
/// FNV-1a over the origin (little endian) followed by the key (the same
/// value as [`fnv1a32`](crate::dedup::fnv1a32) over the concatenation).
pub const fn stream_id(origin: u32, key: Key<'_>) -> u32 {
    let hash = fnv32_extend(FNV32_OFFSET, &origin.to_le_bytes());
    fnv32_extend(hash, key.as_bytes())
}

const fn header(kind: u8) -> [u8; 3] {
    [MAGIC[0], MAGIC[1], kind]
}

/// The attachment of a publication.
pub fn encode_pub(sender: SenderId, seq: Sequence, stream: u32) -> [u8; PUB_ATTACHMENT_LEN] {
    let mut out = [0u8; PUB_ATTACHMENT_LEN];
    out[..3].copy_from_slice(&header(KIND_PUB));
    out[3..11].copy_from_slice(&sender.0);
    out[11] = seq.get();
    out[12..].copy_from_slice(&stream.to_le_bytes());
    out
}

/// The attachment of a publication carrying the publishing span's trace
/// context (kind 3; see the module docs for when to send it).
pub fn encode_pub_traced(
    sender: SenderId,
    seq: Sequence,
    stream: u32,
    trace: &TraceContext,
) -> [u8; PUB_TRACED_ATTACHMENT_LEN] {
    let mut out = [0u8; PUB_TRACED_ATTACHMENT_LEN];
    out[..PUB_ATTACHMENT_LEN].copy_from_slice(&encode_pub(sender, seq, stream));
    out[2] = KIND_PUB_TRACED;
    out[PUB_ATTACHMENT_LEN..].copy_from_slice(&trace.encode());
    out
}

/// Reads a publication attachment (kind 2 or 3, or kind 1 without a
/// stream). A kind 3 trace context is read by [`decode_trace`].
pub fn decode_pub(bytes: &[u8]) -> Option<PubHeader> {
    let (kind, rest) = strip_header(bytes)?;
    let (sender, rest) = rest.split_first_chunk::<8>()?;
    let (&seq, rest) = rest.split_first()?;
    let stream = match (kind, rest.len()) {
        (KIND_PUB_V1, 0) if bytes.len() == PUB_V1_ATTACHMENT_LEN => None,
        (KIND_PUB, 4) => Some(u32::from_le_bytes(rest.try_into().ok()?)),
        (KIND_PUB_TRACED, n) if n == 4 + TRACE_CONTEXT_LEN => {
            Some(u32::from_le_bytes(rest[..4].try_into().ok()?))
        }
        _ => return None,
    };
    Some(PubHeader {
        sender: SenderId(*sender),
        seq,
        stream,
    })
}

/// The attachment of a query: who asks and how many milliseconds it waits.
pub fn encode_query(sender: SenderId, deadline_ms: u32) -> [u8; QUERY_ATTACHMENT_LEN] {
    let mut out = [0u8; QUERY_ATTACHMENT_LEN];
    out[..3].copy_from_slice(&header(KIND_QUERY));
    out[3..11].copy_from_slice(&sender.0);
    out[11..].copy_from_slice(&deadline_ms.to_le_bytes());
    out
}

/// The attachment of a query carrying the requesting span's trace context
/// (kind `0x12`; see the module docs for when to send it).
pub fn encode_query_traced(
    sender: SenderId,
    deadline_ms: u32,
    trace: &TraceContext,
) -> [u8; QUERY_TRACED_ATTACHMENT_LEN] {
    let mut out = [0u8; QUERY_TRACED_ATTACHMENT_LEN];
    out[..QUERY_ATTACHMENT_LEN].copy_from_slice(&encode_query(sender, deadline_ms));
    out[2] = KIND_QUERY_TRACED;
    out[QUERY_ATTACHMENT_LEN..].copy_from_slice(&trace.encode());
    out
}

/// Reads a query attachment (kind `0x10` or `0x12`): requester and
/// deadline in milliseconds.
pub fn decode_query(bytes: &[u8]) -> Option<(SenderId, u32)> {
    let (kind, rest) = strip_header(bytes)?;
    let fields = match (kind, rest.len()) {
        (KIND_QUERY, 12) => rest,
        (KIND_QUERY_TRACED, n) if n == 12 + TRACE_CONTEXT_LEN => &rest[..12],
        _ => return None,
    };
    let (sender, deadline) = fields.split_first_chunk::<8>()?;
    Some((
        SenderId(*sender),
        u32::from_le_bytes(deadline.try_into().ok()?),
    ))
}

/// The trace context of a kind 3 publication or kind `0x12` query
/// attachment; `None` for every other attachment, and for an all-zero
/// (invalid) context.
pub fn decode_trace(bytes: &[u8]) -> Option<TraceContext> {
    let (kind, _) = strip_header(bytes)?;
    let at = match (kind, bytes.len()) {
        (KIND_PUB_TRACED, PUB_TRACED_ATTACHMENT_LEN) => PUB_ATTACHMENT_LEN,
        (KIND_QUERY_TRACED, QUERY_TRACED_ATTACHMENT_LEN) => QUERY_ATTACHMENT_LEN,
        _ => return None,
    };
    TraceContext::decode(&bytes[at..])
}

/// The attachment of a successful reply.
pub fn encode_reply(sender: SenderId) -> [u8; REPLY_ATTACHMENT_LEN] {
    let mut out = [0u8; REPLY_ATTACHMENT_LEN];
    out[..3].copy_from_slice(&header(KIND_REPLY));
    out[3..].copy_from_slice(&sender.0);
    out
}

/// Reads a reply attachment.
pub fn decode_reply(bytes: &[u8]) -> Option<SenderId> {
    let (kind, rest) = strip_header(bytes)?;
    let id: [u8; 8] = rest.try_into().ok()?;
    (kind == KIND_REPLY).then_some(SenderId(id))
}

/// The payload of a reply error: code byte, then the replier.
pub fn encode_reply_error(error: HandlerError, sender: SenderId) -> [u8; REPLY_ERROR_LEN] {
    let mut out = [0u8; REPLY_ERROR_LEN];
    out[0] = error.code();
    out[1..].copy_from_slice(&sender.0);
    out
}

/// Decodes a reply-error payload: the handler error from the first byte
/// ([`HandlerError::from_code`]; empty is [`HandlerError::Internal`]) and
/// the replier if the 8 bytes after it are present.
pub fn decode_reply_error(payload: &[u8]) -> (HandlerError, Option<SenderId>) {
    let error = HandlerError::from_code(payload.first().copied().unwrap_or(0));
    let sender = payload
        .get(1..)
        .and_then(|id| <[u8; 8]>::try_from(id).ok())
        .map(SenderId);
    (error, sender)
}

/// True if a reply whose replier claims to be `replier` is accepted for a
/// request to `target`: any reply for [`Target::Any`], only replies naming
/// the node for [`Target::Node`]. The claim itself is self-asserted. Pure.
pub fn accepts(target: Target, replier: Option<SenderId>) -> bool {
    match target {
        Target::Any => true,
        Target::Node(id) => replier == Some(id),
    }
}

/// True if a reply-error payload is Zenoh's own query timeout, not a
/// responder's error.
pub fn is_timeout_error(payload: &[u8]) -> bool {
    payload == b"Timeout"
}

fn strip_header(bytes: &[u8]) -> Option<(u8, &[u8])> {
    let rest = bytes.strip_prefix(&MAGIC[..])?;
    let (&kind, rest) = rest.split_first()?;
    Some((kind, rest))
}

/// `id` as 16 lowercase ASCII hex digits.
pub fn zid16_hex(id: SenderId) -> [u8; 16] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = [0u8; 16];
    for (pair, byte) in out.chunks_exact_mut(2).zip(id.0) {
        pair[0] = HEX[usize::from(byte >> 4)];
        pair[1] = HEX[usize::from(byte & 0x0f)];
    }
    out
}

/// The key expression of the queryable that node `id` declares for `key`,
/// `@mw/node/<zid16>/<key>`, in a fixed-capacity string. `None` only if
/// it does not fit [`NODE_KEY_MAX`], which a valid [`Key`] never causes.
pub fn node_key_buf(id: SenderId, key: Key<'_>) -> Option<heapless::String<NODE_KEY_MAX>> {
    let hex = zid16_hex(id);
    let mut out = heapless::String::new();
    out.push_str(NODE_PREFIX).ok()?;
    out.push_str(core::str::from_utf8(&hex).ok()?).ok()?;
    out.push('/').ok()?;
    out.push_str(key.as_str()).ok()?;
    Some(out)
}

/// Splits a per-node key expression `@mw/node/<zid16>/<key>` into the node
/// it names and the key. `None` for anything else (wrong prefix, not 16
/// lowercase hex digits, invalid key).
pub fn parse_node_key(expr: &str) -> Option<(SenderId, Key<'_>)> {
    let rest = expr.strip_prefix(NODE_PREFIX)?;
    let (hex, key) = rest.split_at_checked(16)?;
    let key = key.strip_prefix('/')?;
    let mut id = [0u8; 8];
    for (byte, pair) in id.iter_mut().zip(hex.as_bytes().chunks_exact(2)) {
        let hi = hex_digit(*pair.first()?)?;
        let lo = hex_digit(*pair.get(1)?)?;
        *byte = (hi << 4) | lo;
    }
    Some((SenderId(id), Key::new(key).ok()?))
}

fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

/// Why a service cannot be given a Zenoh key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidInstance;

/// True if `instance` is one Zenoh key chunk with no special meaning:
/// non-empty, without `/`, `*`, `$`, `?` or `#`, not starting with `@`.
pub fn is_valid_instance(instance: &str) -> bool {
    !instance.is_empty()
        && !instance.starts_with('@')
        && !instance.contains(['/', '*', '$', '?', '#'])
}

/// Splits `@mw/svc/<kind>/<instance>` into kind and instance. The
/// instance is the last chunk, so kinds with several chunks are
/// unambiguous.
pub fn parse_service_key(key: &str) -> Option<(ServiceKind<'_>, &str)> {
    let rest = key.strip_prefix(SERVICE_PREFIX)?;
    let (kind, instance) = rest.rsplit_once('/')?;
    if !is_valid_instance(instance) {
        return None;
    }
    Key::new(kind).ok().map(|k| (ServiceKind::new(k), instance))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dedup::fnv1a32;

    const ID: SenderId = SenderId([0xAB, 1, 2, 3, 4, 5, 6, 0xFF]);

    #[test]
    fn fl4_stream_id_is_fnv1a_over_origin_then_key() {
        let key = Key::from_static("imu/accel");
        let mut joined = [0u8; 4 + 9];
        joined[..4].copy_from_slice(&7u32.to_le_bytes());
        joined[4..].copy_from_slice(key.as_bytes());
        assert_eq!(stream_id(7, key), fnv1a32(&joined));
    }

    #[test]
    fn fl4_node_keys_build_and_parse_without_heap() {
        let key = Key::from_static("conformance/echo");
        let expr = node_key_buf(ID, key).expect("fits");
        assert_eq!(expr.as_str(), "@mw/node/ab010203040506ff/conformance/echo");
        assert_eq!(&zid16_hex(ID), b"ab010203040506ff");
        assert_eq!(parse_node_key(&expr), Some((ID, key)));
        let longest = [b'k'; MAX_KEY_LEN];
        let longest = Key::new(core::str::from_utf8(&longest).expect("ascii")).expect("key");
        assert_eq!(
            node_key_buf(ID, longest).map(|s| s.len()),
            Some(NODE_KEY_MAX)
        );
        for bad in [
            "conformance/echo",
            "@mw/node/ab010203040506f/x",
            "@mw/node/AB010203040506ff/x",
            "@mw/node/ab010203040506ffx",
            "@mw/node/ab010203040506ff/",
        ] {
            assert_eq!(parse_node_key(bad), None, "{bad}");
        }
    }

    #[test]
    fn fl4_attachments_round_trip_in_no_std_module() {
        let a = encode_pub(ID, Sequence::new(3), 9);
        assert_eq!(
            decode_pub(&a),
            Some(PubHeader {
                sender: ID,
                seq: 3,
                stream: Some(9)
            })
        );
        assert_eq!(decode_query(&encode_query(ID, 10)), Some((ID, 10)));
        assert_eq!(decode_reply(&encode_reply(ID)), Some(ID));
        assert_eq!(
            decode_reply_error(&encode_reply_error(HandlerError::Denied, ID)),
            (HandlerError::Denied, Some(ID))
        );
    }
}
