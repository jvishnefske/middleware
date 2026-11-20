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
//! | `0x10` | query | requester's sender id 8, deadline in ms 4, optional trace extension |
//! | `0x11` | successful reply | replier's sender id 8 |
//!
//! ## Query trace extension (0.6)
//!
//! A query whose [`RequestOptions`](crate::RequestOptions) carry a trace
//! context or request id appends, after the 15 bytes above:
//!
//! | bytes | field |
//! |-------|-------|
//! | 1 | extension flags: bit 0 trace, bit 1 request id, others reserved (0) |
//! | 16 + 8 + 1 | if bit 0: W3C trace id, parent (span) id, trace flags |
//! | 1 + n | if bit 1: request id length `n` (1..=64), then `n` bytes of visible ASCII |
//!
//! A query with neither sends no extension, so its attachment is exactly
//! the 0.5 one ([`QUERY_ATTACHMENT_LEN`] bytes). [`decode_query`] reads the
//! 0.5 form and the extended one; it ignores reserved flag bits and any
//! bytes after the fields it knows (room for later fields), and a
//! malformed extension (truncated, an all-zero id, an invalid request id)
//! loses only the extension fields, never the sender and deadline. A 0.5
//! responder accepts only the exact 0.5 length, so it ignores the whole
//! attachment of an extended query (no sender, no deadline): mixed
//! deployments see full metadata at 0.5 responders only for queries
//! without trace context. [`encode_query`] writes into a fixed-capacity
//! buffer of [`QUERY_ATTACHMENT_MAX`] bytes, so the board client builds the
//! extended form without a heap too.
//!
//! A publication's sequence numbers one **stream**: one key published
//! through one transport value. The stream id ([`stream_id`]) tells streams
//! of one sender apart, so a subscriber computes exact per-key gaps with
//! [`SeenTable::check_stream`](crate::dedup::SeenTable::check_stream).
//! Kind 1 attachments (from 0.4 publishers) still decode, without a stream
//! and so without gaps. Anything else is ignored: the sample or query then
//! carries no metadata.
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
use crate::transport::{HandlerError, RequestId, SampleMeta, SenderId, Target, TraceParent};

const MAGIC: [u8; 2] = [b'm', b'w'];
const KIND_PUB_V1: u8 = 1;
const KIND_PUB: u8 = 2;
const KIND_QUERY: u8 = 0x10;
const KIND_REPLY: u8 = 0x11;

/// Length of a publication attachment.
pub const PUB_ATTACHMENT_LEN: usize = 3 + 8 + 1 + 4;
const PUB_V1_ATTACHMENT_LEN: usize = 3 + 8 + 1;
/// Length of a query attachment without the trace extension (the 0.5
/// form).
pub const QUERY_ATTACHMENT_LEN: usize = 3 + 8 + 4;
/// Extension flag: a trace context follows.
pub const QUERY_EXT_TRACE: u8 = 0x01;
/// Extension flag: a request id follows (after the trace, if both).
pub const QUERY_EXT_REQUEST_ID: u8 = 0x02;
const TRACE_EXT_LEN: usize = 16 + 8 + 1;
/// Longest query attachment this version writes.
pub const QUERY_ATTACHMENT_MAX: usize =
    QUERY_ATTACHMENT_LEN + 1 + TRACE_EXT_LEN + 1 + crate::transport::trace::MAX_REQUEST_ID_LEN;
/// A query attachment as [`encode_query`] builds it: at most
/// [`QUERY_ATTACHMENT_MAX`] bytes, no heap.
pub type QueryAttachment = heapless::Vec<u8, QUERY_ATTACHMENT_MAX>;
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

/// Reads a publication attachment (kind 2, or kind 1 without a stream).
pub fn decode_pub(bytes: &[u8]) -> Option<PubHeader> {
    let (kind, rest) = strip_header(bytes)?;
    let (sender, rest) = rest.split_first_chunk::<8>()?;
    let (&seq, rest) = rest.split_first()?;
    let stream = match (kind, rest.len()) {
        (KIND_PUB_V1, 0) if bytes.len() == PUB_V1_ATTACHMENT_LEN => None,
        (KIND_PUB, 4) => Some(u32::from_le_bytes(rest.try_into().ok()?)),
        _ => return None,
    };
    Some(PubHeader {
        sender: SenderId(*sender),
        seq,
        stream,
    })
}

/// What a query attachment says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryHeader<'a> {
    /// The requesting node.
    pub sender: SenderId,
    /// How many milliseconds it waits.
    pub deadline_ms: u32,
    /// Its trace context, if the extension carried a valid one.
    pub trace: Option<TraceParent>,
    /// Its request id, if the extension carried a valid one.
    pub request_id: Option<RequestId<'a>>,
}

/// The attachment of a query: who asks, how many milliseconds it waits
/// and, when either is present, the trace extension (module docs).
pub fn encode_query(
    sender: SenderId,
    deadline_ms: u32,
    trace: Option<TraceParent>,
    request_id: Option<RequestId<'_>>,
) -> QueryAttachment {
    let mut buf = [0u8; QUERY_ATTACHMENT_MAX];
    buf[..3].copy_from_slice(&header(KIND_QUERY));
    buf[3..11].copy_from_slice(&sender.0);
    buf[11..QUERY_ATTACHMENT_LEN].copy_from_slice(&deadline_ms.to_le_bytes());
    let mut len = QUERY_ATTACHMENT_LEN;
    if trace.is_some() || request_id.is_some() {
        buf[len] =
            trace.map_or(0, |_| QUERY_EXT_TRACE) | request_id.map_or(0, |_| QUERY_EXT_REQUEST_ID);
        len += 1;
        if let Some(t) = trace {
            buf[len..len + 16].copy_from_slice(&t.trace_id());
            buf[len + 16..len + 24].copy_from_slice(&t.span_id());
            buf[len + 24] = t.flags();
            len += TRACE_EXT_LEN;
        }
        if let Some(r) = request_id {
            let id = r.as_str().as_bytes();
            // At most MAX_REQUEST_ID_LEN (64) bytes by construction, so
            // both the length byte and the copy fit the buffer.
            let n = id.len().min(crate::transport::trace::MAX_REQUEST_ID_LEN);
            buf[len] = u8::try_from(n).unwrap_or(0);
            buf[len + 1..len + 1 + n].copy_from_slice(&id[..n]);
            len += 1 + n;
        }
    }
    // `len` never exceeds the capacity, so this copy cannot fail.
    QueryAttachment::from_slice(&buf[..len]).unwrap_or_default()
}

/// Reads a query attachment, with or without the trace extension (module
/// docs). `None` if it is not a query attachment of at least the 0.5
/// length.
pub fn decode_query(bytes: &[u8]) -> Option<QueryHeader<'_>> {
    let (kind, rest) = strip_header(bytes)?;
    if kind != KIND_QUERY {
        return None;
    }
    let (sender, rest) = rest.split_first_chunk::<8>()?;
    let (deadline, ext) = rest.split_first_chunk::<4>()?;
    let (trace, request_id) = decode_query_ext(ext).unwrap_or((None, None));
    Some(QueryHeader {
        sender: SenderId(*sender),
        deadline_ms: u32::from_le_bytes(*deadline),
        trace,
        request_id,
    })
}

/// The trace extension's fields; `Some((None, None))` for no extension,
/// `None` when it is malformed.
fn decode_query_ext(ext: &[u8]) -> Option<(Option<TraceParent>, Option<RequestId<'_>>)> {
    let Some((&flags, mut rest)) = ext.split_first() else {
        return Some((None, None));
    };
    let mut trace = None;
    if flags & QUERY_EXT_TRACE != 0 {
        let (trace_id, after) = rest.split_first_chunk::<16>()?;
        let (span_id, after) = after.split_first_chunk::<8>()?;
        let (&trace_flags, after) = after.split_first()?;
        trace = Some(TraceParent::new(*trace_id, *span_id, trace_flags)?);
        rest = after;
    }
    let mut request_id = None;
    if flags & QUERY_EXT_REQUEST_ID != 0 {
        let (&len, after) = rest.split_first()?;
        let id = after.get(..usize::from(len))?;
        request_id = Some(RequestId::from_bytes(id)?);
    }
    Some((trace, request_id))
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

/// Hashed in front of a posix-node's node id for its Zenoh session id
/// ([`posix_zenoh_id`]). FROZEN: changing it changes every posix-node's
/// sender id.
pub const POSIX_ZENOH_ID_DOMAIN: &[u8] = b"mw-posix-node/zenoh-id/v1\0";

/// The sender id of a Zenoh session whose id (`zid`) has the 16
/// little-endian bytes `zid`: FNV-1a 64 over them (what
/// `zenoh::sender_id_of` computes from a session's `ZenohId`). Pure.
pub const fn sender_id_of_zid(zid: &[u8; 16]) -> SenderId {
    SenderId(crate::dedup::fnv1a64(zid).to_le_bytes())
}

/// The Zenoh session id a posix-node with node id `node` takes
/// (mw-posix-node's `session_config`), as its 16 little-endian bytes: the
/// first 16 bytes of SHA-256 over [`POSIX_ZENOH_ID_DOMAIN`] and the node
/// id; `1` in the unlikely case those are all zero (Zenoh refuses a zero
/// id). So a posix-node's sender id follows from the node id its
/// `sys/info` reports ([`posix_sender_id`]). Pure.
#[cfg(any(feature = "trust", feature = "zenoh"))]
pub fn posix_zenoh_id(node: SenderId) -> [u8; 16] {
    use sha2::{Digest, Sha256};
    let digest = Sha256::new()
        .chain_update(POSIX_ZENOH_ID_DOMAIN)
        .chain_update(node.0)
        .finalize();
    let mut zid = [0u8; 16];
    zid.copy_from_slice(&digest[..16]);
    if zid == [0; 16] {
        zid[0] = 1;
    }
    zid
}

/// The sender id of the posix-node with node id `node`: the id in its
/// `@mw/node/<id>/...` keys. A board that reports a posix-node chip and a
/// node id whose sender id is not the one it answers at is not that node.
/// Pure.
#[cfg(any(feature = "trust", feature = "zenoh"))]
pub fn posix_sender_id(node: SenderId) -> SenderId {
    sender_id_of_zid(&posix_zenoh_id(node))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dedup::fnv1a32;

    const ID: SenderId = SenderId([0xAB, 1, 2, 3, 4, 5, 6, 0xFF]);

    #[cfg(any(feature = "trust", feature = "zenoh"))]
    #[test]
    fn pn21_a_posix_nodes_session_id_is_derived_from_its_node_id() {
        use crate::dedup::fnv1a64;
        use sha2::{Digest, Sha256};
        let unit = SenderId([0x59, 0x40, 0xe9, 0xa5, 0xe7, 0x35, 0x3c, 0x17]);
        let process = SenderId([0x59, 0x40, 0xe9, 0xa5, 0xe7, 0x35, 0x3c, 0x18]);
        let digest = Sha256::new()
            .chain_update(b"mw-posix-node/zenoh-id/v1\0")
            .chain_update(unit.0)
            .finalize();
        let zid = posix_zenoh_id(unit);
        assert_eq!(zid[..], digest[..16]);
        assert_ne!(posix_zenoh_id(process), zid);
        // The session's sender id: FNV-1a over the zid's 16 bytes, as for
        // any session (`zenoh::sender_id_of`).
        assert_eq!(
            sender_id_of_zid(&zid),
            SenderId(fnv1a64(&zid).to_le_bytes())
        );
        assert_eq!(posix_sender_id(unit), sender_id_of_zid(&zid));
        assert_ne!(posix_sender_id(unit), unit);
    }

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
        let a = encode_query(ID, 10, None, None);
        let q = decode_query(&a).expect("query");
        assert_eq!((q.sender, q.deadline_ms), (ID, 10));
        assert_eq!(decode_reply(&encode_reply(ID)), Some(ID));
        assert_eq!(
            decode_reply_error(&encode_reply_error(HandlerError::Denied, ID)),
            (HandlerError::Denied, Some(ID))
        );
    }

    /// `decode_query` as 0.5 shipped it: exactly 12 bytes after the kind.
    fn decode_query_v05(bytes: &[u8]) -> Option<(SenderId, u32)> {
        let (kind, rest) = strip_header(bytes)?;
        if kind != KIND_QUERY || rest.len() != 12 {
            return None;
        }
        let (sender, deadline) = rest.split_first_chunk::<8>()?;
        Some((
            SenderId(*sender),
            u32::from_le_bytes(deadline.try_into().ok()?),
        ))
    }

    const TP: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    #[test]
    fn k18_query_trace_extension_round_trips() {
        let t = TraceParent::parse(TP).unwrap();
        let id = RequestId::new("req-9").unwrap();
        for (trace, request_id) in [
            (Some(t), Some(id)),
            (Some(t), None),
            (None, Some(id)),
            (None, None),
        ] {
            let a = encode_query(ID, 3000, trace, request_id);
            assert!(a.len() <= QUERY_ATTACHMENT_MAX);
            assert_eq!(
                decode_query(&a),
                Some(QueryHeader {
                    sender: ID,
                    deadline_ms: 3000,
                    trace,
                    request_id
                })
            );
        }
        let full = encode_query(ID, 1, Some(t), Some(id));
        assert_eq!(full.len(), QUERY_ATTACHMENT_LEN + 1 + 25 + 1 + 5);
        assert_eq!(
            full[QUERY_ATTACHMENT_LEN],
            QUERY_EXT_TRACE | QUERY_EXT_REQUEST_ID
        );
        let longest = RequestId::from_bytes(&[b'r'; 64]).unwrap();
        assert_eq!(
            encode_query(ID, 1, Some(t), Some(longest)).len(),
            QUERY_ATTACHMENT_MAX
        );
    }

    #[test]
    fn k19_query_extension_is_compatible_both_ways() {
        let t = TraceParent::parse(TP).unwrap();
        let id = RequestId::new("req-9").unwrap();
        // Without trace context the attachment is byte for byte 0.5's, so
        // a 0.5 responder decodes it fully.
        let plain = encode_query(ID, 3000, None, None);
        assert_eq!(plain.len(), QUERY_ATTACHMENT_LEN);
        assert_eq!(decode_query_v05(&plain), Some((ID, 3000)));
        // A 0.5 responder's exact-length rule refuses the extended form
        // (it then sees no metadata at all, as for a foreign query).
        let extended = encode_query(ID, 3000, Some(t), Some(id));
        assert_eq!(decode_query_v05(&extended), None);
        // A 0.6 responder reads a 0.5 requester's attachment, with no trace.
        let mut v05 = [0u8; QUERY_ATTACHMENT_LEN];
        v05[..3].copy_from_slice(b"mw\x10");
        v05[3..11].copy_from_slice(&ID.0);
        v05[11..].copy_from_slice(&3000u32.to_le_bytes());
        let h = decode_query(&v05).expect("0.5 attachment");
        assert_eq!(
            (h.sender, h.deadline_ms, h.trace, h.request_id),
            (ID, 3000, None, None)
        );
        // Fields from a later version (reserved flag bits, trailing bytes)
        // are ignored.
        let mut later = [0u8; QUERY_ATTACHMENT_MAX + 12];
        later[..extended.len()].copy_from_slice(&extended);
        later[QUERY_ATTACHMENT_LEN] |= 0x80;
        let tail = b"future field";
        later[extended.len()..extended.len() + tail.len()].copy_from_slice(tail);
        let later = &later[..extended.len() + tail.len()];
        assert_eq!(decode_query(later), decode_query(&extended));
    }

    #[test]
    fn k19_malformed_extensions_keep_sender_and_deadline() {
        let t = TraceParent::parse(TP).unwrap();
        let id = RequestId::new("req-9").unwrap();
        let full = encode_query(ID, 7, Some(t), Some(id));
        for cut in QUERY_ATTACHMENT_LEN + 1..full.len() {
            let h = decode_query(&full[..cut]).expect("base fields");
            assert_eq!((h.sender, h.deadline_ms), (ID, 7), "cut {cut}");
            assert_eq!((h.trace, h.request_id), (None, None), "cut {cut}");
        }
        // Zero ids and invalid request ids are dropped.
        let mut zero = encode_query(ID, 7, Some(t), None);
        zero[QUERY_ATTACHMENT_LEN + 1..QUERY_ATTACHMENT_LEN + 17].fill(0);
        assert_eq!(decode_query(&zero).unwrap().trace, None);
        let mut bad_id = encode_query(ID, 7, None, Some(id));
        let last = bad_id.len() - 1;
        bad_id[last] = b' ';
        assert_eq!(decode_query(&bad_id).unwrap().request_id, None);
        let mut zero_len = encode_query(ID, 7, None, Some(id));
        zero_len[QUERY_ATTACHMENT_LEN + 1] = 0;
        assert_eq!(decode_query(&zero_len).unwrap().request_id, None);
    }
}
