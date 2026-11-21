//! The pump against a scripted router: each test feeds the router's batches
//! as WebSocket messages and reads what the session sent. (U43)

use super::*;
use middleware::zenoh_lite::codec::network::{
    NetExts, NetworkMessage, PushBody, Put, Reply, ReplyErr, Response, ResponseBody, ResponseFinal,
};
use middleware::zenoh_lite::codec::transport::{
    Batch, FrameHeader, InitAck, Item, OpenAck, Resolution, TransportMessage, write_batch,
};
use middleware::zenoh_lite::codec::{CodecError, VERSION, WhatAmI, WireExpr, Writer};
use middleware::zenoh_wire as wire;

const NODE: SenderId = SenderId([0x5e, 0x11, 0x0d, 0xe0, 0x2a, 0x9c, 0x01, 0x02]);
const OTHER: SenderId = SenderId([0xb2; 8]);

/// A router batch as one WebSocket message: no length prefix.
fn batch(body: impl FnOnce(&mut Writer<'_>) -> Result<(), CodecError>) -> Vec<u8> {
    let mut buf = vec![0u8; 2048];
    let n = write_batch(&mut buf, u16::MAX, body).expect("fits");
    buf[2..n].to_vec()
}

fn init_ack() -> Vec<u8> {
    batch(|w| {
        InitAck {
            version: VERSION,
            whatami: WhatAmI::Router,
            zid: Zid::from_slice(&[0xee, 2]).expect("zid"),
            resolution: Resolution::DEFAULT,
            batch_size: 4096,
            cookie: b"cookie",
            qos: false,
            patch: 0,
        }
        .write(w)
    })
}

fn open_ack() -> Vec<u8> {
    batch(|w| {
        OpenAck {
            lease_ms: 10_000,
            initial_sn: 100,
            qos: false,
        }
        .write(w)
    })
}

fn frame(sn: u32, msgs: &[NetworkMessage<'_>]) -> Vec<u8> {
    batch(|w| {
        FrameHeader {
            reliable: true,
            sn,
            qos: FrameHeader::DEFAULT_QOS,
        }
        .write(w)?;
        msgs.iter().try_for_each(|m| m.write(w))
    })
}

fn reply<'a>(
    rid: u32,
    expr: &'a str,
    attachment: &'a [u8],
    payload: &'a [u8],
) -> NetworkMessage<'a> {
    NetworkMessage::Response(Response {
        rid,
        wire_expr: WireExpr::plain(expr),
        exts: NetExts::DEFAULT,
        responder: None,
        body: ResponseBody::Reply(Reply {
            consolidation: 0,
            body: PushBody::Put(Put {
                attachment: Some(attachment),
                payload,
                ..Put::default()
            }),
        }),
    })
}

fn error<'a>(rid: u32, expr: &'a str, payload: &'a [u8]) -> NetworkMessage<'a> {
    NetworkMessage::Response(Response {
        rid,
        wire_expr: WireExpr::plain(expr),
        exts: NetExts::DEFAULT,
        responder: None,
        body: ResponseBody::Err(ReplyErr {
            payload,
            ..ReplyErr::default()
        }),
    })
}

fn last(rid: u32) -> NetworkMessage<'static> {
    NetworkMessage::ResponseFinal(ResponseFinal {
        rid,
        exts: NetExts::DEFAULT,
    })
}

/// Every message decodes as one whole batch.
fn items(out: &[Vec<u8>]) -> Vec<Vec<Item<'_>>> {
    out.iter()
        .map(|m| {
            Batch::new(m)
                .map(|i| i.expect("a whole batch, no length prefix"))
                .collect()
        })
        .collect()
}

fn zid() -> Zid {
    Zid::from_slice(&[0x11, 0x22, 0x33]).expect("zid")
}

fn only(it: &[Vec<Item<'_>>]) -> Option<String> {
    match it {
        [b] => match b[..] {
            [Item::Transport(ref t)] => Some(format!("{t:?}")),
            _ => None,
        },
        _ => None,
    }
}

/// A pump whose session is open at time 0.
fn opened() -> Pump {
    let mut p = Pump::new(zid());
    let id = p.submit(Cmd::Open);
    assert!(p.poll(0).is_empty());
    let out = p.take_output();
    let it = items(&out);
    assert!(
        only(&it).is_some_and(|t| t.starts_with("InitSyn")),
        "{it:?}"
    );
    p.feed(&init_ack());
    assert!(p.poll(0).is_empty());
    let out = p.take_output();
    let it = items(&out);
    assert!(
        only(&it).is_some_and(|t| t.starts_with("OpenSyn")),
        "{it:?}"
    );
    p.feed(&open_ack());
    let done = p.poll(0);
    assert_eq!(
        done,
        vec![Done {
            id,
            outcome: Outcome::Opened(p.sender_id())
        }]
    );
    p.take_output();
    p
}

/// The id, key expression and target of the request in `out`.
fn sent_request(out: &[Vec<u8>]) -> (u32, String, String) {
    let it = items(out);
    it.iter()
        .flatten()
        .find_map(|i| match i {
            Item::Network(NetworkMessage::Request(r)) => Some((
                r.id,
                r.wire_expr.suffix.to_owned(),
                format!("{:?}", r.target),
            )),
            _ => None,
        })
        .expect("a request was sent")
}

fn request(node: Option<SenderId>, key: &str, timeout_ms: u32) -> Cmd {
    Cmd::Request {
        node,
        key: key.into(),
        body: vec![],
        timeout_ms,
    }
}

#[test]
fn u43_the_handshake_goes_out_unprefixed_and_open_reports_our_sender() {
    let p = opened();
    // The sender id is the host's: FNV-1a of the Zenoh id.
    assert_eq!(
        p.sender_id(),
        middleware::zenoh_lite::client::sender_id_of(zid())
    );
}

#[test]
fn u43_a_request_to_a_node_returns_its_reply() {
    let mut p = opened();
    let id = p.submit(request(Some(NODE), "sys/info", 8000));
    assert!(p.poll(5).is_empty());
    let (rid, expr, _) = sent_request(&p.take_output());
    let key = wire::node_key_buf(NODE, Key::from_static("sys/info")).expect("fits");
    assert_eq!(expr, key.as_str());
    // Another node's reply is not accepted; the addressed node's is.
    let other = wire::encode_reply(OTHER);
    let ours = wire::encode_reply(NODE);
    p.feed(&frame(
        100,
        &[
            reply(rid, &key, &other, b"not it"),
            reply(rid, &key, &ours, b"info"),
            last(rid),
        ],
    ));
    assert_eq!(
        p.poll(10),
        vec![Done {
            id,
            outcome: Outcome::Reply(b"info".to_vec())
        }]
    );
}

#[test]
fn u43_a_handler_error_a_timeout_and_a_bad_key_fail_the_request() {
    let mut p = opened();
    let id = p.submit(request(Some(NODE), "sys/trust/claim", 1000));
    p.poll(0);
    let (rid, expr, _) = sent_request(&p.take_output());
    let denied = wire::encode_reply_error(HandlerError::Denied, NODE);
    p.feed(&frame(100, &[error(rid, &expr, &denied), last(rid)]));
    assert_eq!(
        p.poll(1),
        vec![Done {
            id,
            outcome: Outcome::Failed(Failure::Handler(HandlerError::Denied.code()))
        }]
    );
    let id = p.submit(request(None, "sys/info", 1000));
    assert!(p.poll(2).is_empty());
    assert_eq!(p.next_wake_ms(), Some(1002));
    assert_eq!(
        p.poll(1002),
        vec![Done {
            id,
            outcome: Outcome::Failed(Failure::NoReply)
        }]
    );
    // An invalid key fails without a query.
    p.take_output();
    let id = p.submit(request(None, "a//b", 1000));
    let done = p.poll(1003);
    assert!(
        matches!(&done[..], [Done { id: i, outcome: Outcome::Failed(Failure::Other(_)) }] if *i == id),
        "{done:?}"
    );
    assert!(p.take_output().is_empty());
}

#[test]
fn u43_discover_asks_every_node_and_gathers_each_sender() {
    let mut p = opened();
    let id = p.submit(Cmd::Discover {
        key: "sys/info".into(),
        body: vec![],
        timeout_ms: 3000,
        max: 16,
    });
    p.poll(0);
    let (rid, expr, target) = sent_request(&p.take_output());
    assert_eq!(expr, "@mw/node/*/sys/info");
    assert_eq!(target, "All");
    let ka = wire::node_key_buf(NODE, Key::from_static("sys/info")).expect("fits");
    let kb = wire::node_key_buf(OTHER, Key::from_static("sys/info")).expect("fits");
    let (ra, rb) = (wire::encode_reply(NODE), wire::encode_reply(OTHER));
    let eb = wire::encode_reply_error(HandlerError::Denied, OTHER);
    p.feed(&frame(
        100,
        &[reply(rid, &ka, &ra, b"a"), reply(rid, &kb, &rb, b"b")],
    ));
    assert!(p.poll(1).is_empty(), "gathering until the final response");
    p.feed(&frame(101, &[error(rid, &kb, &eb), last(rid)]));
    assert_eq!(
        p.poll(2),
        vec![Done {
            id,
            outcome: Outcome::Replies(vec![
                (Some(NODE), Ok(b"a".to_vec())),
                (Some(OTHER), Ok(b"b".to_vec())),
                (Some(OTHER), Err(HandlerError::Denied.code())),
            ])
        }]
    );
}

#[test]
fn u43_an_idle_session_keeps_its_lease_with_keep_alives() {
    let mut p = opened();
    let wake = p.next_wake_ms().expect("the session waits for a deadline");
    assert!(wake <= 2500, "{wake}");
    assert!(p.poll(wake).is_empty());
    let out = p.take_output();
    let it = items(&out);
    assert!(
        it.iter()
            .flatten()
            .any(|i| matches!(i, Item::Transport(TransportMessage::KeepAlive))),
        "{it:?}"
    );
    // Still up: a request goes out.
    p.submit(request(None, "sys/info", 1000));
    p.poll(wake + 1);
    sent_request(&p.take_output());
}

#[test]
fn u43_a_closed_link_fails_what_is_pending_and_what_follows() {
    let mut p = opened();
    let id = p.submit(request(Some(NODE), "sys/info", 8000));
    p.poll(1);
    p.close();
    assert_eq!(
        p.poll(2),
        vec![Done {
            id,
            outcome: Outcome::Failed(Failure::Down)
        }]
    );
    let id = p.submit(Cmd::Discover {
        key: "sys/info".into(),
        body: vec![],
        timeout_ms: 1000,
        max: 4,
    });
    assert_eq!(
        p.poll(3),
        vec![Done {
            id,
            outcome: Outcome::Failed(Failure::Down)
        }]
    );
    // Before open, too.
    let mut q = Pump::new(zid());
    let id = q.submit(request(None, "sys/info", 1000));
    assert_eq!(
        q.poll(0),
        vec![Done {
            id,
            outcome: Outcome::Failed(Failure::Down)
        }]
    );
}

#[test]
fn u43_malformed_and_oversized_input_never_panics() {
    let mut p = opened();
    for junk in [
        vec![],
        vec![0xff; 3],
        vec![0x07, 0xff],
        vec![0u8; BATCH + 1],
        (0..=255u8).collect(),
    ] {
        p.feed(&junk);
        p.poll(1);
    }
    p.take_output();
    // The junk may have ended the session; either way a request is
    // answered, never left waiting past its timeout.
    let id = p.submit(request(None, "sys/info", 500));
    let mut done = p.poll(2);
    if done.is_empty() {
        done = p.poll(600);
    }
    assert!(
        matches!(&done[..], [Done { id: i, outcome: Outcome::Failed(_) }] if *i == id),
        "{done:?}"
    );
}

#[test]
fn u43_a_session_that_dies_while_idle_says_so_and_takes_no_more_input() {
    // The router goes silent: its 10 s lease runs out while nothing runs.
    let mut p = opened();
    assert!(!p.is_down());
    let mut now = 0;
    while !p.is_down() && now < 30_000 {
        now = p.next_wake_ms().expect("a deadline while up").max(now + 1);
        assert!(p.poll(now).is_empty());
    }
    assert!(p.is_down(), "the lease ended the session");
    assert!(now <= 10_000 + 2_500, "{now}");
    // Input is ignored now, and commands fail at once.
    p.feed(&init_ack());
    let id = p.submit(request(None, "sys/info", 1000));
    assert_eq!(
        p.poll(now + 1),
        vec![Done {
            id,
            outcome: Outcome::Failed(Failure::Down)
        }]
    );
    // The router closes the session while it is idle.
    let mut q = opened();
    q.feed(&batch(|w| {
        middleware::zenoh_lite::codec::transport::Close {
            reason: middleware::zenoh_lite::codec::transport::reason::GENERIC,
            session: true,
        }
        .write(w)
    }));
    assert!(q.poll(1).is_empty());
    assert!(q.is_down());
    // A link that closes while idle is down too.
    let mut c = opened();
    c.close();
    c.poll(1);
    assert!(c.is_down());
}
