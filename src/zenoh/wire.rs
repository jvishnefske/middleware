//! The pure encodings of the Zenoh backend, re-exported from the
//! always-built, `no_std` [`crate::zenoh_wire`] (attachments, reply-error
//! payloads, reserved key expressions; see there for the formats), plus
//! the `String`-returning builders the host backend uses for its key
//! expressions. The board's client `zenoh_lite` uses the same module, so
//! both put the same bytes on the wire.

pub use crate::zenoh_wire::*;

use crate::discovery::ServiceKind;
use crate::key::Key;
use crate::transport::SenderId;

#[cfg(test)]
use crate::dedup::Sequence;
#[cfg(test)]
use crate::transport::{HandlerError, SampleMeta, Target};

/// `id` as 16 lowercase hex digits.
pub fn zid16(id: SenderId) -> String {
    zid16_hex(id).iter().map(|&c| char::from(c)).collect()
}

/// The key expression of the queryable that node `id` declares for `key`:
/// `@mw/node/<zid16>/<key>`.
pub fn node_key(id: SenderId, key: Key<'_>) -> String {
    format!("{NODE_PREFIX}{}/{key}", zid16(id))
}

/// `@mw/svc/<kind>/<instance>`.
pub fn service_key(kind: ServiceKind<'_>, instance: &str) -> Result<String, InvalidInstance> {
    if !is_valid_instance(instance) {
        return Err(InvalidInstance);
    }
    Ok(format!("{SERVICE_PREFIX}{kind}/{instance}"))
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
