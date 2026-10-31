//! Pure encodings the Zenoh backend puts on the wire next to Zenoh's own
//! framing: attachments, the reply-error payload and the reserved key
//! expressions. No I/O; everything here is unit tested.
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
//! without trace context.
//!
//! A publication's sequence numbers one **stream**: one key published
//! through one [`ZenohTransport`](super::ZenohTransport) value. The stream
//! id ([`stream_id`]) tells streams of one sender apart, so a subscriber
//! computes exact per-key gaps with
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
//! `Timeout` (from the querying session, or from the peer that routed the
//! query); [`is_timeout_error`] tells it apart, since a middleware error
//! payload is 1 or 9 bytes.
//!
//! # Reserved key expressions
//!
//! * `@mw/node/<zid16>/<key>`: the per-node queryable of a responder on
//!   `key` ([`node_key`]); `zid16` is the node's [`SenderId`] as 16
//!   lowercase hex digits.
//! * `@mw/svc/<kind>/<instance>`: the liveliness token and record
//!   queryable of an announced service ([`service_key`],
//!   [`parse_service_key`]).
//!
//! The leading `@mw` chunk is a Zenoh verbatim chunk: it is matched only by
//! itself, never by `*` or `**`, so no application subscription or query
//! (application keys never start with `@`) reaches these.

use crate::dedup::{Sequence, fnv1a32};
use crate::discovery::ServiceKind;
use crate::key::Key;
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
/// Length of a reply attachment.
pub const REPLY_ATTACHMENT_LEN: usize = 3 + 8;
/// Length of a reply-error payload.
pub const REPLY_ERROR_LEN: usize = 1 + 8;

/// Prefix of the per-node queryables.
pub const NODE_PREFIX: &str = "@mw/node/";
/// Prefix of service liveliness tokens and record queryables.
pub const SERVICE_PREFIX: &str = "@mw/svc/";

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

/// The stream id of `key` published by the transport value `origin`:
/// FNV-1a over the origin (little endian) followed by the key.
pub fn stream_id(origin: u32, key: Key<'_>) -> u32 {
    let mut bytes = Vec::with_capacity(4 + key.as_bytes().len());
    bytes.extend_from_slice(&origin.to_le_bytes());
    bytes.extend_from_slice(key.as_bytes());
    fnv1a32(&bytes)
}

fn header(kind: u8) -> [u8; 3] {
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
) -> Vec<u8> {
    let mut out = Vec::with_capacity(QUERY_ATTACHMENT_MAX);
    out.extend_from_slice(&header(KIND_QUERY));
    out.extend_from_slice(&sender.0);
    out.extend_from_slice(&deadline_ms.to_le_bytes());
    if trace.is_none() && request_id.is_none() {
        return out;
    }
    let flags =
        trace.map_or(0, |_| QUERY_EXT_TRACE) | request_id.map_or(0, |_| QUERY_EXT_REQUEST_ID);
    out.push(flags);
    if let Some(t) = trace {
        out.extend_from_slice(&t.trace_id());
        out.extend_from_slice(&t.span_id());
        out.push(t.flags());
    }
    if let Some(r) = request_id {
        let id = r.as_str().as_bytes();
        // At most MAX_REQUEST_ID_LEN (64) bytes by construction.
        out.push(u8::try_from(id.len()).unwrap_or(0));
        out.extend_from_slice(id);
    }
    out
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

/// `id` as 16 lowercase hex digits.
pub fn zid16(id: SenderId) -> String {
    id.0.iter().map(|b| format!("{b:02x}")).collect()
}

/// The key expression of the queryable that node `id` declares for `key`:
/// `@mw/node/<zid16>/<key>`.
pub fn node_key(id: SenderId, key: Key<'_>) -> String {
    format!("{NODE_PREFIX}{}/{key}", zid16(id))
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

/// `@mw/svc/<kind>/<instance>`.
pub fn service_key(kind: ServiceKind<'_>, instance: &str) -> Result<String, InvalidInstance> {
    if !is_valid_instance(instance) {
        return Err(InvalidInstance);
    }
    Ok(format!("{SERVICE_PREFIX}{kind}/{instance}"))
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

    const ID: SenderId = SenderId([0xAB, 1, 2, 3, 4, 5, 6, 0xFF]);

    #[test]
    fn publication_attachments_round_trip_and_v1_still_decodes() {
        let a = encode_pub(ID, Sequence::new(9), 0xDEAD_BEEF);
        assert_eq!(a.len(), 16);
        assert_eq!(
            decode_pub(&a),
            Some(PubHeader {
                sender: ID,
                seq: 9,
                stream: Some(0xDEAD_BEEF)
            })
        );
        let mut v1 = [0u8; 12];
        v1[..3].copy_from_slice(b"mw\x01");
        v1[3..11].copy_from_slice(&ID.0);
        v1[11] = 4;
        let h = decode_pub(&v1).expect("v1");
        assert_eq!(
            (h.stream, h.meta()),
            (None, SampleMeta::from_sender(ID).with_seq(4))
        );
        for bad in [&a[..15], &a[..11], b"xx\x02123456789abcd", b"", &v1[..11]] {
            assert_eq!(decode_pub(bad), None, "{bad:?}");
        }
        // A query attachment is not a publication.
        assert_eq!(decode_pub(&encode_query(ID, 5, None, None)), None);
    }

    #[test]
    fn stream_ids_separate_keys_and_origins() {
        let k1 = Key::from_static("a/b");
        let k2 = Key::from_static("a/c");
        assert_ne!(stream_id(0, k1), stream_id(0, k2));
        assert_ne!(stream_id(0, k1), stream_id(1, k1));
        assert_eq!(stream_id(7, k1), stream_id(7, k1));
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

    fn base(h: Option<QueryHeader<'_>>) -> Option<(SenderId, u32)> {
        h.map(|h| (h.sender, h.deadline_ms))
    }

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
        let mut later = extended.clone();
        later[QUERY_ATTACHMENT_LEN] |= 0x80;
        later.extend_from_slice(b"future field");
        assert_eq!(decode_query(&later), decode_query(&extended));
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

    #[test]
    fn query_and_reply_attachments_round_trip() {
        let q = encode_query(ID, 3000, None, None);
        assert_eq!(base(decode_query(&q)), Some((ID, 3000)));
        assert_eq!(decode_query(&q[..14]), None);
        assert_eq!(decode_query(&encode_reply(ID)), None);
        let r = encode_reply(ID);
        assert_eq!(decode_reply(&r), Some(ID));
        assert_eq!(decode_reply(&q), None);
        assert_eq!(decode_query(&r), None);
        assert_eq!(decode_reply(&r[..10]), None);
    }

    #[test]
    fn reply_error_payload_decodes_code_byte_and_sender() {
        for e in HandlerError::ALL {
            assert_eq!(
                decode_reply_error(&encode_reply_error(e, ID)),
                (e, Some(ID))
            );
            assert_eq!(decode_reply_error(&[e.code()]), (e, None));
        }
        assert_eq!(decode_reply_error(&[]), (HandlerError::Internal, None));
        assert_eq!(
            decode_reply_error(&[6, 1, 2]),
            (HandlerError::Unavailable, None)
        );
        assert!(is_timeout_error(b"Timeout"));
        for e in HandlerError::ALL {
            assert!(!is_timeout_error(&encode_reply_error(e, ID)));
            assert!(!is_timeout_error(&[e.code()]));
        }
    }

    #[test]
    fn node_targets_accept_only_replies_naming_the_node() {
        let other = SenderId([9; 8]);
        assert!(accepts(Target::Any, None));
        assert!(accepts(Target::Any, Some(other)));
        assert!(accepts(Target::Node(ID), Some(ID)));
        assert!(!accepts(Target::Node(ID), Some(other)));
        assert!(!accepts(Target::Node(ID), None));
    }

    #[test]
    fn node_and_service_keys() {
        let k = Key::from_static("conformance/echo");
        assert_eq!(zid16(ID), "ab010203040506ff");
        assert_eq!(
            node_key(ID, k),
            "@mw/node/ab010203040506ff/conformance/echo"
        );
        let kind = ServiceKind::from_static("cam/jpeg");
        let key = service_key(kind, "unit-1").expect("valid");
        assert_eq!(key, "@mw/svc/cam/jpeg/unit-1");
        assert_eq!(parse_service_key(&key), Some((kind, "unit-1")));
        for bad in ["", "a/b", "a*", "@x", "a$b", "a?", "a#"] {
            assert_eq!(service_key(kind, bad), Err(InvalidInstance), "{bad:?}");
        }
        assert_eq!(parse_service_key("@mw/svc/unit-1"), None);
        assert_eq!(parse_service_key("svc/cam/x"), None);
        assert_eq!(parse_service_key("@mw/svc/cam/"), None);
    }
}
