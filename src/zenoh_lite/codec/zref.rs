//! Test-only bridge to the reference: converts this codec's messages to
//! the `zenoh-protocol` 1.10 types so the tests can encode the same
//! message with `zenoh-codec` 1.10 and compare bytes (ZN1).

use std::borrow::ToOwned;
use std::string::String;
use std::time::Duration;
use std::vec;
use std::vec::Vec;

use core::num::NonZeroU32;

use zenoh_buffers::{ZBuf, ZSlice, reader::HasReader, writer::HasWriter};
use zenoh_codec::{RCodec, WCodec, Zenoh080};
use zenoh_protocol::common::ZExtZ64;
use zenoh_protocol::core as zc;
use zenoh_protocol::network as zn;
use zenoh_protocol::transport as zt;
use zenoh_protocol::zenoh as zz;

use super::network::{
    self as n, DeclareBody, InterestMode, NetExts, NetworkMessage, PushBody, QueryTarget,
    ResponseBody,
};
use super::transport::{self as t, TransportMessage};
use super::{Encoding, EntityGlobalId, Timestamp, WhatAmI, WireExpr, Writer, Zid};

/// Encodes with this codec.
pub fn ours<F>(f: F) -> Vec<u8>
where
    F: FnOnce(&mut Writer<'_>) -> Result<(), super::CodecError>,
{
    let mut buf = vec![0u8; 70_000];
    let mut w = Writer::new(&mut buf);
    f(&mut w).expect("our encoder");
    w.written().to_vec()
}

/// Encodes a transport message with `zenoh-codec`.
pub fn zenoh_transport(m: &zt::TransportMessage) -> Vec<u8> {
    let mut v = Vec::new();
    let mut w = (&mut v).writer();
    Zenoh080::new().write(&mut w, m).expect("zenoh encodes");
    v
}

/// Encodes a network message with `zenoh-codec`.
pub fn zenoh_network(m: &zn::NetworkMessage) -> Vec<u8> {
    let mut v = Vec::new();
    let mut w = (&mut v).writer();
    Zenoh080::new().write(&mut w, m).expect("zenoh encodes");
    v
}

/// Decodes a transport message with `zenoh-codec`.
pub fn zenoh_read_transport(bytes: &[u8]) -> Option<zt::TransportMessage> {
    let mut r = bytes.reader();
    let m: Result<zt::TransportMessage, _> = Zenoh080::new().read(&mut r);
    m.ok()
}

/// Decodes a network message with `zenoh-codec`.
pub fn zenoh_read_network(bytes: &[u8]) -> Option<zn::NetworkMessage> {
    let mut r = bytes.reader();
    let m: Result<zn::NetworkMessage, _> = Zenoh080::new().read(&mut r);
    m.ok()
}

pub fn zid(z: Zid) -> zc::ZenohIdProto {
    zc::ZenohIdProto::try_from(z.as_wire()).expect("valid zid")
}

fn whatami(w: WhatAmI) -> zc::WhatAmI {
    match w {
        WhatAmI::Router => zc::WhatAmI::Router,
        WhatAmI::Peer => zc::WhatAmI::Peer,
        WhatAmI::Client => zc::WhatAmI::Client,
    }
}

fn zslice(b: &[u8]) -> ZSlice {
    ZSlice::from(b.to_vec())
}

fn zbuf(b: &[u8]) -> ZBuf {
    ZBuf::from(b.to_vec())
}

pub fn wire_expr(we: &WireExpr<'_>) -> zc::WireExpr<'static> {
    zc::WireExpr {
        scope: we.scope,
        suffix: String::from(we.suffix).into(),
        mapping: if we.sender_mapping {
            zn::Mapping::Sender
        } else {
            zn::Mapping::Receiver
        },
    }
}

fn timestamp(ts: &Timestamp<'_>) -> zc::Timestamp {
    let id = zc::TimestampId::try_from(ts.id).expect("valid id");
    zc::Timestamp::new(zc::NTP64(ts.time), id)
}

fn encoding(e: &Encoding<'_>) -> zc::Encoding {
    zc::Encoding {
        id: e.id,
        schema: e.schema.map(zslice),
    }
}

fn entity(e: &EntityGlobalId) -> zc::EntityGlobalIdProto {
    zc::EntityGlobalIdProto {
        zid: zid(e.zid),
        eid: e.eid,
    }
}

fn sinfo<const ID: u8>(s: &n::SourceInfo) -> zz::ext::SourceInfoType<ID> {
    zz::ext::SourceInfoType {
        id: entity(&s.id),
        sn: s.sn,
    }
}

fn attachment<const ID: u8>(a: &[u8]) -> zz::ext::AttachmentType<ID> {
    zz::ext::AttachmentType { buffer: zbuf(a) }
}

fn qos<const ID: u8>(v: u8) -> zn::ext::QoSType<ID> {
    zn::ext::QoSType::from(ZExtZ64::<ID>::new(u64::from(v)))
}

fn tstamp_ext<const ID: u8>(e: &NetExts<'_>) -> Option<zn::ext::TimestampType<ID>> {
    e.timestamp.as_ref().map(|ts| zn::ext::TimestampType {
        timestamp: timestamp(ts),
    })
}

fn node_id<const ID: u8>(e: &NetExts<'_>) -> zn::ext::NodeIdType<ID> {
    zn::ext::NodeIdType { node_id: e.node_id }
}

fn consolidation(c: u8) -> zz::ConsolidationMode {
    match c {
        1 => zz::ConsolidationMode::None,
        2 => zz::ConsolidationMode::Monotonic,
        3 => zz::ConsolidationMode::Latest,
        _ => zz::ConsolidationMode::Auto,
    }
}

pub fn put(p: &n::Put<'_>) -> zz::Put {
    zz::Put {
        timestamp: p.timestamp.as_ref().map(timestamp),
        encoding: encoding(&p.encoding),
        ext_sinfo: p.source.as_ref().map(sinfo),
        ext_attachment: p.attachment.map(attachment),
        ext_unknown: vec![],
        payload: zbuf(p.payload),
    }
}

fn del(d: &n::Del<'_>) -> zz::Del {
    zz::Del {
        timestamp: d.timestamp.as_ref().map(timestamp),
        ext_sinfo: d.source.as_ref().map(sinfo),
        ext_attachment: d.attachment.map(attachment),
        ext_unknown: vec![],
    }
}

fn push_body(b: &PushBody<'_>) -> zz::PushBody {
    match b {
        PushBody::Put(p) => zz::PushBody::Put(put(p)),
        PushBody::Del(d) => zz::PushBody::Del(del(d)),
    }
}

fn query(q: &n::Query<'_>) -> zz::Query {
    zz::Query {
        consolidation: consolidation(q.consolidation),
        parameters: q.parameters.to_owned(),
        ext_sinfo: q.source.as_ref().map(sinfo),
        ext_body: q.body.as_ref().map(|v| zz::query::ext::QueryBodyType {
            encoding: encoding(&v.encoding),
            payload: zbuf(v.payload),
        }),
        ext_attachment: q.attachment.map(attachment),
        ext_unknown: vec![],
    }
}

fn undeclare_expr(we: &Option<WireExpr<'_>>) -> zn::declare::common::ext::WireExprType {
    match we {
        Some(we) => zn::declare::common::ext::WireExprType {
            wire_expr: wire_expr(we),
        },
        None => zn::declare::common::ext::WireExprType::null(),
    }
}

fn declare_body(b: &DeclareBody<'_>) -> zn::DeclareBody {
    use zn::declare as d;
    match b {
        DeclareBody::KeyExpr { id, wire_expr: we } => {
            d::DeclareBody::DeclareKeyExpr(d::DeclareKeyExpr {
                id: *id,
                wire_expr: wire_expr(we),
            })
        }
        DeclareBody::UndeclareKeyExpr { id } => {
            d::DeclareBody::UndeclareKeyExpr(d::UndeclareKeyExpr { id: *id })
        }
        DeclareBody::Subscriber { id, wire_expr: we } => {
            d::DeclareBody::DeclareSubscriber(d::DeclareSubscriber {
                id: *id,
                wire_expr: wire_expr(we),
            })
        }
        DeclareBody::UndeclareSubscriber { id, wire_expr: we } => {
            d::DeclareBody::UndeclareSubscriber(d::UndeclareSubscriber {
                id: *id,
                ext_wire_expr: undeclare_expr(we),
            })
        }
        DeclareBody::Queryable {
            id,
            wire_expr: we,
            complete,
            distance,
        } => d::DeclareBody::DeclareQueryable(d::DeclareQueryable {
            id: *id,
            wire_expr: wire_expr(we),
            ext_info: d::queryable::ext::QueryableInfoType {
                complete: *complete,
                distance: *distance,
            },
        }),
        DeclareBody::UndeclareQueryable { id, wire_expr: we } => {
            d::DeclareBody::UndeclareQueryable(d::UndeclareQueryable {
                id: *id,
                ext_wire_expr: undeclare_expr(we),
            })
        }
        DeclareBody::Token { id, wire_expr: we } => d::DeclareBody::DeclareToken(d::DeclareToken {
            id: *id,
            wire_expr: wire_expr(we),
        }),
        DeclareBody::UndeclareToken { id, wire_expr: we } => {
            d::DeclareBody::UndeclareToken(d::UndeclareToken {
                id: *id,
                ext_wire_expr: undeclare_expr(we),
            })
        }
        DeclareBody::Final => d::DeclareBody::DeclareFinal(d::DeclareFinal),
    }
}

/// The reference form of a network message.
pub fn network(m: &NetworkMessage<'_>) -> zn::NetworkMessage {
    let body = match m {
        NetworkMessage::Push(p) => zn::NetworkBody::Push(zn::Push {
            wire_expr: wire_expr(&p.wire_expr),
            ext_qos: qos(p.exts.qos),
            ext_tstamp: tstamp_ext(&p.exts),
            ext_nodeid: node_id(&p.exts),
            ext_ts_stack: None,
            payload: push_body(&p.body),
        }),
        NetworkMessage::Request(r) => zn::NetworkBody::Request(zn::Request {
            id: r.id,
            wire_expr: wire_expr(&r.wire_expr),
            ext_qos: qos(r.exts.qos),
            ext_tstamp: tstamp_ext(&r.exts),
            ext_nodeid: node_id(&r.exts),
            ext_target: match r.target {
                QueryTarget::BestMatching => zn::request::ext::QueryTarget::BestMatching,
                QueryTarget::All => zn::request::ext::QueryTarget::All,
                QueryTarget::AllComplete => zn::request::ext::QueryTarget::AllComplete,
            },
            ext_budget: r.budget.and_then(NonZeroU32::new),
            ext_timeout: r.timeout_ms.map(Duration::from_millis),
            ext_ts_stack: None,
            payload: zz::RequestBody::Query(query(&r.body)),
        }),
        NetworkMessage::Response(r) => zn::NetworkBody::Response(zn::Response {
            rid: r.rid,
            wire_expr: wire_expr(&r.wire_expr),
            payload: match &r.body {
                ResponseBody::Reply(rep) => zz::ResponseBody::Reply(zz::Reply {
                    consolidation: consolidation(rep.consolidation),
                    ext_unknown: vec![],
                    payload: push_body(&rep.body),
                }),
                ResponseBody::Err(e) => zz::ResponseBody::Err(zz::Err {
                    encoding: encoding(&e.encoding),
                    ext_sinfo: e.source.as_ref().map(sinfo),
                    ext_unknown: vec![],
                    payload: zbuf(e.payload),
                }),
            },
            ext_qos: qos(r.exts.qos),
            ext_tstamp: tstamp_ext(&r.exts),
            ext_respid: r.responder.as_ref().map(|e| zn::ext::EntityGlobalIdType {
                zid: zid(e.zid),
                eid: e.eid,
            }),
            ext_ts_stack: None,
        }),
        NetworkMessage::ResponseFinal(f) => zn::NetworkBody::ResponseFinal(zn::ResponseFinal {
            rid: f.rid,
            ext_qos: qos(f.exts.qos),
            ext_tstamp: tstamp_ext(&f.exts),
        }),
        NetworkMessage::Declare(d) => zn::NetworkBody::Declare(zn::Declare {
            interest_id: d.interest_id,
            ext_qos: qos(d.exts.qos),
            ext_tstamp: tstamp_ext(&d.exts),
            ext_nodeid: node_id(&d.exts),
            body: declare_body(&d.body),
        }),
        NetworkMessage::Interest(i) => zn::NetworkBody::Interest(zn::Interest {
            id: i.id,
            mode: match i.mode {
                InterestMode::Final => zn::interest::InterestMode::Final,
                InterestMode::Current => zn::interest::InterestMode::Current,
                InterestMode::Future => zn::interest::InterestMode::Future,
                InterestMode::CurrentFuture => zn::interest::InterestMode::CurrentFuture,
            },
            options: zn::interest::InterestOptions::from(i.options),
            wire_expr: i.wire_expr.as_ref().map(wire_expr),
            ext_qos: qos(i.exts.qos),
            ext_tstamp: tstamp_ext(&i.exts),
            ext_nodeid: node_id(&i.exts),
        }),
        NetworkMessage::Oam => unreachable!("not converted"),
    };
    zn::NetworkMessage::from(body)
}

fn patch(p: u8) -> zt::init::ext::PatchType {
    zt::init::ext::PatchType::new(p)
}

/// The reference form of a transport message.
pub fn transport(m: &TransportMessage<'_>) -> zt::TransportMessage {
    let body = match m {
        TransportMessage::InitSyn(s) => zt::TransportBody::InitSyn(zt::InitSyn {
            version: s.version,
            whatami: whatami(s.whatami),
            zid: zid(s.zid),
            resolution: zc::Resolution::from(s.resolution.0),
            batch_size: s.batch_size,
            ext_qos: s.qos.then(zt::init::ext::QoS::new),
            ext_qos_link: None,
            ext_auth: None,
            ext_mlink: None,
            ext_lowlatency: None,
            ext_compression: None,
            ext_patch: patch(s.patch),
            ext_region_name: None,
        }),
        TransportMessage::InitAck(a) => zt::TransportBody::InitAck(zt::InitAck {
            version: a.version,
            whatami: whatami(a.whatami),
            zid: zid(a.zid),
            resolution: zc::Resolution::from(a.resolution.0),
            batch_size: a.batch_size,
            cookie: zslice(a.cookie),
            ext_qos: a.qos.then(zt::init::ext::QoS::new),
            ext_qos_link: None,
            ext_auth: None,
            ext_mlink: None,
            ext_lowlatency: None,
            ext_compression: None,
            ext_patch: patch(a.patch),
            ext_region_name: None,
        }),
        TransportMessage::OpenSyn(o) => zt::TransportBody::OpenSyn(zt::OpenSyn {
            lease: Duration::from_millis(o.lease_ms),
            initial_sn: o.initial_sn,
            cookie: zslice(o.cookie),
            ext_qos: o.qos.then(zt::open::ext::QoS::new),
            ext_auth: None,
            ext_mlink: None,
            ext_lowlatency: None,
            ext_compression: None,
            ext_remote_bound: None,
        }),
        TransportMessage::OpenAck(o) => zt::TransportBody::OpenAck(zt::OpenAck {
            lease: Duration::from_millis(o.lease_ms),
            initial_sn: o.initial_sn,
            ext_qos: o.qos.then(zt::open::ext::QoS::new),
            ext_auth: None,
            ext_mlink: None,
            ext_lowlatency: None,
            ext_compression: None,
            ext_remote_bound: None,
        }),
        TransportMessage::Close(c) => zt::TransportBody::Close(zt::Close {
            reason: c.reason,
            session: c.session,
        }),
        TransportMessage::KeepAlive => zt::TransportBody::KeepAlive(zt::KeepAlive),
        TransportMessage::Frame(f) => zt::TransportBody::Frame(frame(f, vec![])),
    };
    zt::TransportMessage::from(body)
}

/// A reference frame with `payload`.
pub fn frame(f: &t::FrameHeader, payload: Vec<zn::NetworkMessage>) -> zt::Frame {
    zt::Frame {
        reliability: if f.reliable {
            zc::Reliability::Reliable
        } else {
            zc::Reliability::BestEffort
        },
        sn: f.sn,
        ext_qos: zt::frame::ext::QoSType::from(ZExtZ64::<{ zt::frame::ext::QoS::ID }>::new(
            u64::from(f.qos),
        )),
        payload,
    }
}
