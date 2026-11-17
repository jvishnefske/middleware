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

    const TRACE: TraceContext = TraceContext {
        trace_id: [
            0x4b, 0xf9, 0x2f, 0x35, 0x77, 0xb3, 0x4d, 0xa6, 0xa3, 0xce, 0x92, 0x9d, 0x0e, 0x0e,
            0x47, 0x36,
        ],
        span_id: [0x00, 0xf0, 0x67, 0xaa, 0x0b, 0xa9, 0x02, 0xb7],
        flags: 1,
    };

    #[test]
    fn traced_publications_keep_their_header_and_carry_the_context() {
        let a = encode_pub_traced(ID, Sequence::new(9), 0xDEAD_BEEF, &TRACE);
        assert_eq!(a.len(), 41);
        assert_eq!(&a[..2], b"mw");
        assert_eq!(a[2], 3);
        assert_eq!(
            decode_pub(&a),
            Some(PubHeader {
                sender: ID,
                seq: 9,
                stream: Some(0xDEAD_BEEF)
            })
        );
        assert_eq!(decode_trace(&a), Some(TRACE));
        // Untraced attachments have none; a truncated one decodes as nothing.
        assert_eq!(decode_trace(&encode_pub(ID, Sequence::new(9), 1)), None);
        assert_eq!(decode_pub(&a[..40]), None);
        assert_eq!(decode_trace(&a[..40]), None);
        // An all-zero context is invalid: the header still decodes.
        let zero = TraceContext {
            trace_id: [0; 16],
            span_id: [0; 8],
            flags: 0,
        };
        let z = encode_pub_traced(ID, Sequence::new(1), 1, &zero);
        assert!(decode_pub(&z).is_some());
        assert_eq!(decode_trace(&z), None);
    }

    #[test]
    fn traced_queries_keep_their_header_and_carry_the_context() {
        let q = encode_query_traced(ID, 3000, &TRACE);
        assert_eq!(q.len(), QUERY_TRACED_ATTACHMENT_LEN);
        assert_eq!(decode_query(&q), Some((ID, 3000)));
        assert_eq!(decode_trace(&q), Some(TRACE));
        assert_eq!(decode_trace(&encode_query(ID, 3000)), None);
        assert_eq!(decode_query(&q[..QUERY_TRACED_ATTACHMENT_LEN - 1]), None);
        // Neither kind is mistaken for the other.
        assert_eq!(decode_pub(&q), None);
        assert_eq!(
            decode_query(&encode_pub_traced(ID, Sequence::new(1), 1, &TRACE)),
            None
        );
    }

    #[test]
    fn traceparent_text_round_trips() {
        // The example from the W3C Trace Context specification.
        let text = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let ctx = TraceContext::parse_traceparent(text).expect("valid");
        assert_eq!(ctx, TRACE);
        assert!(ctx.sampled());
        assert_eq!(&ctx.to_traceparent()[..], text.as_bytes());
        for bad in [
            "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01",
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-0",
            "00-4bf92f3577b34da6a3ce929d0e0e4736_00f067aa0ba902b7-01",
        ] {
            assert_eq!(TraceContext::parse_traceparent(bad), None, "{bad}");
        }
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
