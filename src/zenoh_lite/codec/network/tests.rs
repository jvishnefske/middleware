//! ZN1 for network messages and their bodies: byte-identical to
//! `zenoh-codec` 1.10, decoded both ways, truncations refused, unknown
//! extensions skipped and mandatory ones refused.

use std::vec;
use std::vec::Vec;

use super::super::zref;
use super::super::{EntityGlobalId, Zid};
use super::*;

fn zid(bytes: &[u8]) -> Zid {
    Zid::from_slice(bytes).expect("non-zero")
}

const ATT: &[u8] = b"mw\x02\xab\x01\x02\x03\x04\x05\x06\xff\x09\xef\xbe\xad\xde";
const TS_ID: &[u8] = &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];

fn ts() -> Timestamp<'static> {
    Timestamp {
        time: 0x6553_f100_0000_0001,
        id: TS_ID,
    }
}

fn source() -> SourceInfo {
    SourceInfo {
        id: EntityGlobalId {
            zid: zid(&[7, 7, 7]),
            eid: 300,
        },
        sn: 70_000,
    }
}

fn exts_all() -> NetExts<'static> {
    NetExts {
        qos: QOS_DEFAULT | QOS_BLOCK | QOS_EXPRESS,
        timestamp: Some(ts()),
        node_id: 3,
    }
}

fn put_full() -> Put<'static> {
    Put {
        timestamp: Some(ts()),
        encoding: Encoding {
            id: 10,
            schema: Some(b"schema"),
        },
        source: Some(source()),
        attachment: Some(ATT),
        payload: b"hello",
    }
}

fn plain(k: &'static str) -> WireExpr<'static> {
    WireExpr::plain(k)
}

fn scoped() -> WireExpr<'static> {
    WireExpr {
        scope: 300,
        suffix: "/accel",
        sender_mapping: true,
    }
}

fn samples() -> Vec<NetworkMessage<'static>> {
    let mut v = Vec::new();
    // Push: Put in every shape, Del.
    for (we, exts, body) in [
        (
            plain("imu/accel"),
            NetExts::DEFAULT,
            PushBody::Put(Put {
                attachment: Some(ATT),
                payload: b"x",
                ..Put::default()
            }),
        ),
        (
            plain("a"),
            NetExts {
                qos: QOS_DEFAULT | QOS_BLOCK,
                ..NetExts::DEFAULT
            },
            PushBody::Put(Put::default()),
        ),
        (scoped(), exts_all(), PushBody::Put(put_full())),
        (
            WireExpr {
                scope: 2,
                suffix: "",
                sender_mapping: false,
            },
            NetExts {
                node_id: 9,
                ..NetExts::DEFAULT
            },
            PushBody::Del(Del {
                timestamp: Some(ts()),
                source: Some(source()),
                attachment: Some(b"a"),
            }),
        ),
        (plain("d"), NetExts::DEFAULT, PushBody::Del(Del::default())),
    ] {
        v.push(NetworkMessage::Push(Push {
            wire_expr: we,
            exts,
            body,
        }));
    }
    // Request.
    let query = Query {
        consolidation: 1,
        parameters: "",
        source: None,
        body: Some(Value {
            encoding: Encoding::EMPTY,
            payload: b"req",
        }),
        attachment: Some(b"mw\x10\xab\x01\x02\x03\x04\x05\x06\xff\x88\x13\x00\x00"),
    };
    v.push(NetworkMessage::Request(Request {
        id: 1,
        wire_expr: plain("sys/info"),
        exts: NetExts::DEFAULT,
        target: QueryTarget::BestMatching,
        budget: None,
        timeout_ms: Some(5000),
        body: query,
    }));
    v.push(NetworkMessage::Request(Request {
        id: u32::MAX,
        wire_expr: scoped(),
        exts: exts_all(),
        target: QueryTarget::All,
        budget: Some(4),
        timeout_ms: None,
        body: Query {
            consolidation: 3,
            parameters: "a=1;b=2",
            source: Some(source()),
            body: Some(Value {
                encoding: Encoding {
                    id: 3,
                    schema: Some(b"s"),
                },
                payload: b"",
            }),
            attachment: None,
        },
    }));
    v.push(NetworkMessage::Request(Request {
        id: 0,
        wire_expr: plain("q"),
        exts: NetExts::DEFAULT,
        target: QueryTarget::AllComplete,
        budget: None,
        timeout_ms: None,
        body: Query::default(),
    }));
    // Response: reply (put, del) and error.
    v.push(NetworkMessage::Response(Response {
        rid: 1,
        wire_expr: plain("sys/info"),
        exts: NetExts::DEFAULT,
        responder: None,
        body: ResponseBody::Reply(Reply {
            consolidation: 0,
            body: PushBody::Put(Put {
                attachment: Some(b"mw\x11\xab\x01\x02\x03\x04\x05\x06\xff"),
                payload: b"info",
                ..Put::default()
            }),
        }),
    }));
    v.push(NetworkMessage::Response(Response {
        rid: 77,
        wire_expr: scoped(),
        exts: NetExts {
            qos: 2,
            timestamp: Some(ts()),
            node_id: 0,
        },
        responder: Some(EntityGlobalId {
            zid: zid(&[1; 16]),
            eid: 5,
        }),
        body: ResponseBody::Reply(Reply {
            consolidation: 2,
            body: PushBody::Del(Del::default()),
        }),
    }));
    v.push(NetworkMessage::Response(Response {
        rid: 2,
        wire_expr: plain("k"),
        exts: NetExts::DEFAULT,
        responder: None,
        body: ResponseBody::Err(ReplyErr {
            encoding: Encoding::EMPTY,
            source: None,
            payload: b"\x06\xab\x01\x02\x03\x04\x05\x06\xff",
        }),
    }));
    v.push(NetworkMessage::Response(Response {
        rid: 3,
        wire_expr: plain("k"),
        exts: NetExts::DEFAULT,
        responder: None,
        body: ResponseBody::Err(ReplyErr {
            encoding: Encoding {
                id: 1,
                schema: None,
            },
            source: Some(source()),
            payload: b"Timeout",
        }),
    }));
    // ResponseFinal.
    v.push(NetworkMessage::ResponseFinal(ResponseFinal {
        rid: 1,
        exts: NetExts::DEFAULT,
    }));
    v.push(NetworkMessage::ResponseFinal(ResponseFinal {
        rid: 1 << 30,
        exts: NetExts {
            qos: 1,
            timestamp: Some(ts()),
            node_id: 0,
        },
    }));
    // Declare: every body, with and without interest id.
    let bodies = [
        DeclareBody::KeyExpr {
            id: 300,
            wire_expr: plain("imu"),
        },
        DeclareBody::KeyExpr {
            id: 2,
            wire_expr: WireExpr {
                scope: 1,
                suffix: "",
                sender_mapping: false,
            },
        },
        DeclareBody::UndeclareKeyExpr { id: 300 },
        DeclareBody::Subscriber {
            id: 1,
            wire_expr: plain("imu/accel/**"),
        },
        DeclareBody::Subscriber {
            id: 70_000,
            wire_expr: scoped(),
        },
        DeclareBody::UndeclareSubscriber {
            id: 1,
            wire_expr: None,
        },
        DeclareBody::UndeclareSubscriber {
            id: 1,
            wire_expr: Some(plain("imu/accel/**")),
        },
        DeclareBody::Queryable {
            id: 2,
            wire_expr: plain("sys/info"),
            complete: false,
            distance: 0,
        },
        DeclareBody::Queryable {
            id: 3,
            wire_expr: scoped(),
            complete: true,
            distance: 7,
        },
        DeclareBody::UndeclareQueryable {
            id: 2,
            wire_expr: None,
        },
        DeclareBody::UndeclareQueryable {
            id: 2,
            wire_expr: Some(scoped()),
        },
        DeclareBody::Token {
            id: 4,
            wire_expr: plain("@mw/svc/cam/unit"),
        },
        DeclareBody::UndeclareToken {
            id: 4,
            wire_expr: None,
        },
        DeclareBody::Final,
    ];
    for (i, body) in bodies.into_iter().enumerate() {
        v.push(NetworkMessage::Declare(Declare {
            interest_id: (i % 2 == 1).then_some(i as u32 * 1000),
            exts: if i == 3 { exts_all() } else { NetExts::DEFAULT },
            body,
        }));
    }
    // Interest: every mode, restricted or not.
    for (mode, options, we, exts) in [
        (
            InterestMode::CurrentFuture,
            interest::KEYEXPRS | interest::SUBSCRIBERS | interest::AGGREGATE,
            None,
            NetExts::DEFAULT,
        ),
        (
            InterestMode::Current,
            interest::QUERYABLES | interest::TOKENS,
            Some(plain("sys/**")),
            exts_all(),
        ),
        (
            InterestMode::Future,
            interest::SUBSCRIBERS,
            Some(scoped()),
            NetExts::DEFAULT,
        ),
        (InterestMode::Final, 0, None, NetExts::DEFAULT),
    ] {
        let mut i = Interest {
            id: 42,
            mode,
            options,
            wire_expr: we,
            exts,
        };
        i.options = i.wire_options();
        v.push(NetworkMessage::Interest(i));
    }
    v
}

#[test]
fn zn1_network_messages_are_byte_identical_to_zenoh_codec() {
    for m in samples() {
        let ours = zref::ours(|w| m.write(w));
        let reference = zref::network(&m);
        let theirs = zref::zenoh_network(&reference);
        assert_eq!(ours, theirs, "{m:?}");
        assert_eq!(NetworkMessage::decode(&theirs), Ok((m, theirs.len())));
        let back = zref::zenoh_read_network(&ours).map(|z| z.body);
        assert_eq!(back, Some(reference.body), "{m:?}");
    }
}

#[test]
fn zn1_truncated_network_messages_are_refused() {
    for m in samples() {
        let bytes = zref::ours(|w| m.write(w));
        for cut in 0..bytes.len() {
            assert!(
                NetworkMessage::decode(&bytes[..cut]).is_err(),
                "{m:?} cut at {cut}"
            );
        }
    }
}

#[test]
fn zn1_unknown_body_extensions_are_skipped_and_mandatory_refused() {
    use zenoh_protocol::common::{ZExtBody, ZExtUnknown};
    use zenoh_protocol::network as zn;
    let m = NetworkMessage::Push(Push {
        wire_expr: plain("a/b"),
        exts: NetExts::DEFAULT,
        body: PushBody::Put(Put {
            attachment: Some(ATT),
            payload: b"p",
            ..Put::default()
        }),
    });
    let with_unknown = |exts: Vec<ZExtUnknown>| {
        let mut z = zref::network(&m);
        if let zn::NetworkBody::Push(p) = &mut z.body
            && let zenoh_protocol::zenoh::PushBody::Put(put) = &mut p.payload
        {
            put.ext_unknown = exts;
        }
        zref::zenoh_network(&z)
    };
    let skipped = with_unknown(vec![
        ZExtUnknown::new(0x0c, false, ZExtBody::Z64(5)),
        ZExtUnknown::new(
            0x0d,
            false,
            ZExtBody::ZBuf(zenoh_buffers::ZBuf::from(vec![1u8, 2])),
        ),
    ]);
    assert_eq!(NetworkMessage::decode(&skipped).map(|(m, _)| m), Ok(m));
    // The same with the last one mandatory: refused.
    let refused = with_unknown(vec![
        ZExtUnknown::new(0x0c, false, ZExtBody::Z64(5)),
        ZExtUnknown::new(0x0d, true, ZExtBody::Unit),
    ]);
    assert_eq!(
        NetworkMessage::decode(&refused).map(|(m, _)| m),
        Err(CodecError::Mandatory)
    );
    // A Push carrying an unknown mandatory network extension.
    let raw = [0xbd, 0x00, 0x01, b'k', 0x1a, 0x01, 0x00];
    assert_eq!(NetworkMessage::decode(&raw), Err(CodecError::Mandatory));
    // The timestamp stack extension (0x47) is skipped.
    let raw = [0xbd, 0x00, 0x01, b'k', 0x47, 0x01, 0xff, 0x01, 0x00];
    assert!(NetworkMessage::decode(&raw).is_ok());
    // Request with an invalid target value.
    let raw = [0x9c, 0x01, 0x00, 0x34, 0x09, 0x03];
    assert_eq!(NetworkMessage::decode(&raw), Err(CodecError::Invalid));
}

#[test]
fn zn1_network_oam_is_skipped() {
    // OAM, z64 body, id 1, value 7.
    assert_eq!(
        NetworkMessage::decode(&[0x3f, 0x01, 0x07]),
        Ok((NetworkMessage::Oam, 3))
    );
    assert_eq!(
        NetworkMessage::Oam.write(&mut Writer::new(&mut [0u8; 4])),
        Err(CodecError::Invalid)
    );
}
