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
//! | `0x10` | query | requester's sender id 8, deadline in ms 4 |
//! | `0x11` | successful reply | replier's sender id 8 |
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
use crate::transport::{HandlerError, SampleMeta, SenderId, Target};

const MAGIC: [u8; 2] = [b'm', b'w'];
const KIND_PUB_V1: u8 = 1;
const KIND_PUB: u8 = 2;
const KIND_QUERY: u8 = 0x10;
const KIND_REPLY: u8 = 0x11;

/// Length of a publication attachment.
pub const PUB_ATTACHMENT_LEN: usize = 3 + 8 + 1 + 4;
const PUB_V1_ATTACHMENT_LEN: usize = 3 + 8 + 1;
/// Length of a query attachment.
pub const QUERY_ATTACHMENT_LEN: usize = 3 + 8 + 4;
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

/// The attachment of a query: who asks and how many milliseconds it waits.
pub fn encode_query(sender: SenderId, deadline_ms: u32) -> [u8; QUERY_ATTACHMENT_LEN] {
    let mut out = [0u8; QUERY_ATTACHMENT_LEN];
    out[..3].copy_from_slice(&header(KIND_QUERY));
    out[3..11].copy_from_slice(&sender.0);
    out[11..].copy_from_slice(&deadline_ms.to_le_bytes());
    out
}

/// Reads a query attachment: requester and deadline in milliseconds.
pub fn decode_query(bytes: &[u8]) -> Option<(SenderId, u32)> {
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
        assert_eq!(decode_pub(&encode_query(ID, 5)), None);
    }

    #[test]
    fn stream_ids_separate_keys_and_origins() {
        let k1 = Key::from_static("a/b");
        let k2 = Key::from_static("a/c");
        assert_ne!(stream_id(0, k1), stream_id(0, k2));
        assert_ne!(stream_id(0, k1), stream_id(1, k1));
        assert_eq!(stream_id(7, k1), stream_id(7, k1));
    }

    #[test]
    fn query_and_reply_attachments_round_trip() {
        let q = encode_query(ID, 3000);
        assert_eq!(decode_query(&q), Some((ID, 3000)));
        assert_eq!(decode_query(&q[..14]), None);
        assert_eq!(decode_query(&encode_reply(ID)), None);
        let r = encode_reply(ID);
        assert_eq!(decode_reply(&r), Some(ID));
        assert_eq!(decode_reply(&q), None);
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
