//! ZN1 for transport messages: every message of the subset encodes to the
//! bytes `zenoh-codec` 1.10 writes for the same message, decodes from
//! them, and `zenoh-codec` decodes ours back; unknown extensions are
//! skipped, mandatory ones refused, truncations are errors.

use std::vec;
use std::vec::Vec;

use super::super::network::{NetExts, NetworkMessage, Push, PushBody, Put};
use super::super::zref;
use super::*;

fn zid(bytes: &[u8]) -> Zid {
    Zid::from_slice(bytes).expect("non-zero")
}

const COOKIE: &[u8] = &[
    0xC0, 0x0C, 0x1E, 7, 0, 255, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11,
];

fn samples() -> Vec<TransportMessage<'static>> {
    let one = zid(&[0x2a]);
    let full = zid(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 0x80]);
    let syn = InitSyn {
        version: super::super::VERSION,
        whatami: WhatAmI::Client,
        zid: full,
        resolution: Resolution::DEFAULT,
        batch_size: DEFAULT_BATCH,
        qos: false,
        patch: 0,
    };
    let ack = InitAck {
        version: super::super::VERSION,
        whatami: WhatAmI::Router,
        zid: one,
        resolution: Resolution::DEFAULT,
        batch_size: 2048,
        cookie: COOKIE,
        qos: false,
        patch: 0,
    };
    vec![
        TransportMessage::InitSyn(syn),
        TransportMessage::InitSyn(InitSyn {
            batch_size: 2048,
            ..syn
        }),
        TransportMessage::InitSyn(InitSyn {
            resolution: Resolution(0b0001),
            whatami: WhatAmI::Peer,
            zid: one,
            qos: true,
            patch: 1,
            ..syn
        }),
        TransportMessage::InitSyn(InitSyn { patch: 1, ..syn }),
        TransportMessage::InitAck(ack),
        TransportMessage::InitAck(InitAck {
            batch_size: DEFAULT_BATCH,
            cookie: &[],
            whatami: WhatAmI::Peer,
            qos: true,
            ..ack
        }),
        TransportMessage::InitAck(InitAck { patch: 1, ..ack }),
        TransportMessage::OpenSyn(OpenSyn {
            lease_ms: 10_000,
            initial_sn: 0x0fff_fffe,
            cookie: COOKIE,
            qos: false,
        }),
        TransportMessage::OpenSyn(OpenSyn {
            lease_ms: 2_500,
            initial_sn: 0,
            cookie: &[],
            qos: true,
        }),
        TransportMessage::OpenAck(OpenAck {
            lease_ms: 10_000,
            initial_sn: 12345,
            qos: false,
        }),
        TransportMessage::OpenAck(OpenAck {
            lease_ms: 1_234,
            initial_sn: 0x7f,
            qos: true,
        }),
        TransportMessage::Close(Close {
            reason: reason::EXPIRED,
            session: true,
        }),
        TransportMessage::Close(Close {
            reason: reason::GENERIC,
            session: false,
        }),
        TransportMessage::KeepAlive,
        TransportMessage::Frame(FrameHeader {
            reliable: true,
            sn: 0x0fff_ffff,
            qos: FrameHeader::DEFAULT_QOS,
        }),
        TransportMessage::Frame(FrameHeader {
            reliable: false,
            sn: 3,
            qos: 1,
        }),
    ]
}

fn decode_one(bytes: &[u8]) -> Result<TransportMessage<'_>, CodecError> {
    let mut batch = Batch::new(bytes);
    match batch.next() {
        Some(Ok(Item::Transport(m))) => {
            assert!(batch.cursor().done(bytes), "consumed everything");
            Ok(m)
        }
        Some(Ok(other)) => panic!("not a transport message: {other:?}"),
        Some(Err(e)) => Err(e),
        None => Err(CodecError::Truncated),
    }
}

#[test]
fn zn1_transport_messages_are_byte_identical_to_zenoh_codec() {
    for m in samples() {
        let ours = zref::ours(|w| m.write(w));
        let reference = zref::transport(&m);
        let theirs = zref::zenoh_transport(&reference);
        assert_eq!(ours, theirs, "{m:?}");
        // We decode theirs to the same message, they decode ours.
        assert_eq!(decode_one(&theirs), Ok(m));
        assert_eq!(zref::zenoh_read_transport(&ours), Some(reference), "{m:?}");
    }
}

#[test]
fn zn1_truncated_transport_messages_are_refused() {
    for m in samples() {
        let bytes = zref::ours(|w| m.write(w));
        for cut in 0..bytes.len() {
            let r = Batch::new(&bytes[..cut]).next();
            assert!(
                !matches!(r, Some(Ok(Item::Transport(x))) if x == m),
                "{m:?} cut at {cut}"
            );
            // A Frame header may stand alone; nothing else decodes cut.
            if !matches!(m, TransportMessage::Frame(_)) && cut > 0 {
                assert!(matches!(r, Some(Err(_))), "{m:?} cut at {cut}: {r:?}");
            }
        }
    }
}

#[test]
fn zn1_unknown_handshake_extensions_are_skipped() {
    use zenoh_buffers::ZBuf;
    use zenoh_protocol::transport as zt;
    let ack = InitAck {
        version: super::super::VERSION,
        whatami: WhatAmI::Router,
        zid: zid(&[9, 9]),
        resolution: Resolution::DEFAULT,
        batch_size: 4096,
        cookie: COOKIE,
        qos: true,
        patch: 1,
    };
    let reference = zref::transport(&TransportMessage::InitAck(ack));
    let zt::TransportBody::InitAck(mut z) = reference.body else {
        unreachable!()
    };
    z.ext_qos_link = Some(zt::init::ext::QoSLink::new(77));
    z.ext_auth = Some(zt::init::ext::Auth::new(ZBuf::from(vec![1u8, 2, 3])));
    z.ext_mlink = Some(zt::init::ext::MultiLink::new(ZBuf::from(vec![4u8])));
    z.ext_lowlatency = Some(zt::init::ext::LowLatency::new());
    z.ext_compression = Some(zt::init::ext::Compression::new());
    z.ext_region_name = Some(zt::init::ext::RegionName::new(ZBuf::from(
        b"south".to_vec(),
    )));
    let bytes = zref::zenoh_transport(&zt::TransportMessage::from(zt::TransportBody::InitAck(z)));
    assert_eq!(decode_one(&bytes), Ok(TransportMessage::InitAck(ack)));

    let open = zt::OpenAck {
        lease: std::time::Duration::from_millis(10_000),
        initial_sn: 5,
        ext_qos: None,
        ext_auth: Some(zt::open::ext::Auth::new(ZBuf::from(vec![1u8]))),
        ext_mlink: Some(zt::open::ext::MultiLinkAck::new()),
        ext_lowlatency: None,
        ext_compression: Some(zt::open::ext::Compression::new()),
        ext_remote_bound: Some(zt::open::ext::RemoteBound::new(3)),
    };
    let bytes = zref::zenoh_transport(&zt::TransportMessage::from(zt::TransportBody::OpenAck(
        open,
    )));
    assert_eq!(
        decode_one(&bytes),
        Ok(TransportMessage::OpenAck(OpenAck {
            lease_ms: 10_000,
            initial_sn: 5,
            qos: false
        }))
    );
}

#[test]
fn zn1_mandatory_unknown_extensions_are_refused() {
    // KeepAlive with Z and a mandatory unit extension id 9.
    assert_eq!(decode_one(&[0x84, 0x19]), Err(CodecError::Mandatory));
    // The same extension not mandatory is skipped.
    assert_eq!(decode_one(&[0x84, 0x09]), Ok(TransportMessage::KeepAlive));
    // Close with a mandatory z64 extension.
    assert_eq!(
        decode_one(&[0x83, 0x00, 0x3a, 0x01]),
        Err(CodecError::Mandatory)
    );
    // Frame with an unknown mandatory extension (not the QoS one).
    assert_eq!(
        decode_one(&[0xa5, 0x01, 0x32, 0x00]),
        Err(CodecError::Mandatory)
    );
}

fn push(key: &'static str, payload: &'static [u8]) -> NetworkMessage<'static> {
    NetworkMessage::Push(Push {
        wire_expr: super::super::WireExpr::plain(key),
        exts: NetExts::DEFAULT,
        body: PushBody::Put(Put {
            payload,
            ..Put::default()
        }),
    })
}

#[test]
fn zn1_batch_walks_frames_network_messages_and_transport_messages() {
    let header = FrameHeader {
        reliable: true,
        sn: 7,
        qos: FrameHeader::DEFAULT_QOS,
    };
    let (a, b) = (push("a/b", b"one"), push("a/c", b"two"));
    // Reference frame with both pushes, then a KeepAlive, in one batch.
    let mut theirs = zref::zenoh_transport(&zenoh_protocol::transport::TransportMessage::from(
        zenoh_protocol::transport::TransportBody::Frame(zref::frame(
            &header,
            vec![zref::network(&a), zref::network(&b)],
        )),
    ));
    theirs.extend(zref::zenoh_transport(&zref::transport(
        &TransportMessage::KeepAlive,
    )));
    let ours = zref::ours(|w| {
        header.write(w)?;
        a.write(w)?;
        b.write(w)?;
        write_keep_alive(w)
    });
    assert_eq!(ours, theirs);
    let items: Vec<_> = Batch::new(&ours).collect();
    assert_eq!(
        items,
        vec![
            Ok(Item::Transport(TransportMessage::Frame(header))),
            Ok(Item::Network(a)),
            Ok(Item::Network(b)),
            Ok(Item::Transport(TransportMessage::KeepAlive)),
        ]
    );
}

#[test]
fn zn1_batch_refuses_what_this_client_does_not_take() {
    // A network message outside a frame.
    let p = zref::ours(|w| push("k", b"v").write(w));
    assert_eq!(
        Batch::new(&p).collect::<Vec<_>>(),
        vec![Err(CodecError::Invalid)]
    );
    // Join (multicast) and unknown ids.
    assert_eq!(Batch::new(&[0x07]).next(), Some(Err(CodecError::Invalid)));
    // After an error the walk stops.
    let mut b = Batch::new(&[0x07, 0x04]);
    assert_eq!(b.next(), Some(Err(CodecError::Invalid)));
    assert_eq!(b.next(), None);
    // A fragment swallows the rest of the batch.
    let frag = [0x26, 0x05, 1, 2, 3, 0x04];
    assert_eq!(
        Batch::new(&frag).collect::<Vec<_>>(),
        vec![Ok(Item::Fragment(FragmentHeader {
            reliable: true,
            more: false,
            sn: 5
        }))]
    );
    // A transport OAM (z64 body) is skipped.
    assert_eq!(
        Batch::new(&[0x20, 0x01, 0x05, 0x04]).collect::<Vec<_>>(),
        vec![
            Ok(Item::Oam),
            Ok(Item::Transport(TransportMessage::KeepAlive))
        ]
    );
}

#[test]
fn zn1_batches_carry_a_little_endian_length_and_never_exceed_the_limit() {
    let mut buf = [0u8; 64];
    let n = write_batch(&mut buf, 64, |w| {
        write_keep_alive(w)?;
        write_keep_alive(w)
    })
    .expect("fits");
    assert_eq!(&buf[..n], &[2, 0, 0x04, 0x04]);
    assert_eq!(
        write_batch(&mut buf, 1, |w| {
            write_keep_alive(w)?;
            write_keep_alive(w)
        }),
        Err(CodecError::NoSpace)
    );
    let mut tiny = [0u8; 3];
    assert_eq!(
        write_batch(&mut tiny, 64, |w| w.bytes(&[1, 2])),
        Err(CodecError::NoSpace)
    );
}
