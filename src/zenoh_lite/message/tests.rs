//! The adapter on a scripted message link: reads in every split size,
//! writes in pieces, oversized and empty messages skipped, a closed link,
//! and a whole [`ZenohLite`] handshake and request over it with no length
//! prefix on the wire. (U41)

use std::collections::VecDeque;
use std::vec;
use std::vec::Vec;

use core::convert::Infallible;
use core::future::poll_fn;
use core::task::Poll;

use super::*;
use crate::key::Key;
use crate::transport::{RequestOptions, Requester, SenderId, Target};
use crate::zenoh_lite::codec::network::{
    NetExts, NetworkMessage, PushBody, Put, Reply, Response, ResponseBody, ResponseFinal,
};
use crate::zenoh_lite::codec::transport::{
    Batch, FrameHeader, InitAck, Item, OpenAck, Resolution, TransportMessage, write_batch,
};
use crate::zenoh_lite::codec::{CodecError, VERSION, WhatAmI, WireExpr, Writer, Zid};
use crate::zenoh_lite::{Clock, Config, ZenohLite};
use crate::zenoh_wire as wire;

/// Messages handed out in order, then nothing ever again (or `Closed`);
/// sent messages collected.
#[derive(Default)]
struct Link {
    input: VecDeque<Recv>,
    bodies: VecDeque<Vec<u8>>,
    sent: Vec<Vec<u8>>,
    then_close: bool,
}

impl Link {
    fn message(&mut self, bytes: &[u8]) {
        self.input.push_back(Recv::Message(bytes.len()));
        self.bodies.push_back(bytes.to_vec());
    }
}

impl ErrorType for Link {
    type Error = Infallible;
}

impl MessageLink for Link {
    async fn recv(&mut self, buf: &mut [u8]) -> Result<Recv, Infallible> {
        match self.input.pop_front() {
            Some(Recv::Message(n)) => {
                let body = self.bodies.pop_front().expect("body");
                buf[..n].copy_from_slice(&body);
                Ok(Recv::Message(n))
            }
            Some(other) => Ok(other),
            None if self.then_close => Ok(Recv::Closed),
            None => poll_fn(|_| Poll::Pending).await,
        }
    }

    async fn send(&mut self, message: &[u8]) -> Result<(), Infallible> {
        self.sent.push(message.to_vec());
        Ok(())
    }
}

fn run<F: core::future::Future>(f: F) -> F::Output {
    futures::executor::block_on(f)
}

fn prefixed(messages: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for m in messages {
        out.extend_from_slice(&u16::try_from(m.len()).expect("len").to_le_bytes());
        out.extend_from_slice(m);
    }
    out
}

#[test]
fn u41_reads_put_the_length_before_each_message_in_every_split_size() {
    let a: Vec<u8> = (0..40u8).collect();
    let b = vec![0xab; 3];
    let want = prefixed(&[&a, &b]);
    for split in 1..=want.len() + 3 {
        let mut link = Link {
            then_close: true,
            ..Link::default()
        };
        link.message(&a);
        link.message(&b);
        let mut s: Unframed<Link, 64> = Unframed::new(link);
        let mut got = Vec::new();
        run(async {
            let mut buf = vec![0u8; split];
            loop {
                let n = s.read(&mut buf).await.expect("read");
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&buf[..n]);
            }
        });
        assert_eq!(got, want, "split {split}");
    }
}

#[test]
fn u41_empty_and_oversized_messages_are_skipped_and_counted() {
    let mut link = Link {
        then_close: true,
        ..Link::default()
    };
    link.input.push_back(Recv::Oversized);
    link.message(&[]);
    link.message(b"ok");
    let mut s: Unframed<Link, 64> = Unframed::new(link);
    let mut buf = [0u8; 16];
    let n = run(s.read(&mut buf)).expect("read");
    assert_eq!(&buf[..n], &prefixed(&[b"ok"])[..]);
    assert_eq!(s.oversized(), 1);
    // Then the link is closed: the stream ends.
    assert_eq!(run(s.read(&mut buf)).expect("read"), 0);
}

#[test]
fn u41_writes_in_any_pieces_send_each_batch_alone_without_its_length() {
    let a: Vec<u8> = (0..30u8).collect();
    let b = vec![7u8; 5];
    let stream = prefixed(&[&a, &b]);
    for piece in 1..=stream.len() {
        let mut s: Unframed<Link, 64> = Unframed::new(Link::default());
        run(async {
            let mut rest = &stream[..];
            while !rest.is_empty() {
                let n = s
                    .write(&rest[..piece.min(rest.len())])
                    .await
                    .expect("write");
                assert!(n > 0, "piece {piece}");
                rest = &rest[n..];
            }
            s.flush().await.expect("flush");
        });
        assert_eq!(
            s.link_mut().sent,
            vec![a.clone(), b.clone()],
            "piece {piece}"
        );
    }
    // A batch that cannot fit the buffer writes nothing.
    let mut s: Unframed<Link, 64> = Unframed::new(Link::default());
    let n = run(s.write(&prefixed(&[&[0u8; 63]]))).expect("write");
    assert_eq!(n, 0);
    assert!(s.link_mut().sent.is_empty());
}

/// Time that jumps to whatever deadline is awaited.
struct Fake(u64);

impl Clock for Fake {
    fn now_ms(&mut self) -> u64 {
        self.0
    }

    async fn sleep_until_ms(&mut self, deadline_ms: u64) {
        self.0 = self.0.max(deadline_ms);
    }
}

/// One batch as a link message: what `write_batch` writes, minus the
/// length.
fn batch(body: impl FnOnce(&mut Writer<'_>) -> Result<(), CodecError>) -> Vec<u8> {
    let mut buf = [0u8; 512];
    let n = write_batch(&mut buf, u16::MAX, body).expect("fits");
    buf[LEN_PREFIX..n].to_vec()
}

#[test]
fn u41_zenoh_lite_opens_and_requests_over_a_message_link() {
    let node = SenderId([0x5e; 8]);
    let reply = wire::encode_reply(node);
    let key = wire::node_key_buf(node, Key::from_static("sys/info")).expect("fits");
    let mut link = Link::default();
    link.message(&batch(|w| {
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
    }));
    link.message(&batch(|w| {
        OpenAck {
            lease_ms: 10_000,
            initial_sn: 100,
            qos: false,
        }
        .write(w)
    }));
    link.message(&batch(|w| {
        FrameHeader {
            reliable: true,
            sn: 100,
            qos: FrameHeader::DEFAULT_QOS,
        }
        .write(w)?;
        NetworkMessage::Response(Response {
            rid: 0,
            wire_expr: WireExpr::plain(&key),
            exts: NetExts::DEFAULT,
            responder: None,
            body: ResponseBody::Reply(Reply {
                consolidation: 0,
                body: PushBody::Put(Put {
                    attachment: Some(&reply),
                    payload: b"info",
                    ..Put::default()
                }),
            }),
        })
        .write(w)?;
        NetworkMessage::ResponseFinal(ResponseFinal {
            rid: 0,
            exts: NetExts::DEFAULT,
        })
        .write(w)
    }));
    let zid = Zid::from_slice(&[0x11, 0x22]).expect("zid");
    let mut lite: ZenohLite<Unframed<Link, 1026>, Fake, 1024, 1024> =
        ZenohLite::new(Unframed::new(link), Fake(0), Config::new(zid));
    let got = run(async {
        lite.open().await.expect("open");
        let mut out = [0u8; 32];
        let options = RequestOptions::DEFAULT.with_target(Target::Node(node));
        let r = lite
            .request_with(Key::from_static("sys/info"), b"", &mut out, &options)
            .await
            .expect("reply");
        r.to_vec()
    });
    assert_eq!(got, b"info");
    // Every message the client sent is one whole batch, no length first.
    let sent = core::mem::take(&mut lite.stream_mut().link_mut().sent);
    let items: Vec<Vec<Item<'_>>> = sent
        .iter()
        .map(|m| Batch::new(m).map(|i| i.expect("a whole batch")).collect())
        .collect();
    assert!(
        matches!(
            items[0][..],
            [Item::Transport(TransportMessage::InitSyn(_))]
        ),
        "{items:?}"
    );
    assert!(
        matches!(
            items[1][..],
            [Item::Transport(TransportMessage::OpenSyn(_))]
        ),
        "{items:?}"
    );
    let request = items.iter().flatten().find_map(|i| match i {
        Item::Network(NetworkMessage::Request(r)) => Some(r.wire_expr.suffix),
        _ => None,
    });
    assert_eq!(request, Some(key.as_str()));
}

#[test]
fn u41_a_length_written_alone_too_large_for_the_buffer_writes_nothing() {
    // N = 64 holds a batch of at most 62 bytes; 63 does not fit.
    for head in [&[63u8, 0][..], &[0xff, 0xff][..]] {
        let mut s: Unframed<Link, 64> = Unframed::new(Link::default());
        // The 2-byte length alone, then each byte alone.
        assert_eq!(run(s.write(head)).expect("write"), 0, "{head:?}");
        let mut s1: Unframed<Link, 64> = Unframed::new(Link::default());
        assert_eq!(run(s1.write(&head[..1])).expect("write"), 1);
        assert_eq!(
            run(s1.write(&head[1..])).expect("write"),
            0,
            "{head:?} one byte at a time"
        );
        // Nothing is left half-taken: a batch that fits goes out, in pieces of 2.
        for s in [&mut s, &mut s1] {
            let ok = prefixed(&[&[9u8; 10]]);
            run(async {
                for piece in ok.chunks(2) {
                    assert_eq!(s.write(piece).await.expect("write"), piece.len());
                }
                s.flush().await.expect("flush");
            });
            assert_eq!(s.link_mut().sent, vec![vec![9u8; 10]]);
        }
    }
}
