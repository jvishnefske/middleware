//! The adapter against a scripted router on an in-memory stream with a
//! fake clock: handshake bytes, declarations re-sent after open, answers
//! to an interest, router key expression ids, refused input counted, and
//! the lease ending the session. (ZN2 at the shell; ZN3 runs it against
//! real Zenoh.)

use std::vec;
use std::vec::Vec;

use core::convert::Infallible;

use super::*;
use crate::key::Filter;
use crate::zenoh_lite::codec::network::{Declare, DeclareBody, Interest, InterestMode, interest};
use crate::zenoh_lite::codec::transport::{
    FrameHeader, InitAck, Item, OpenAck, Resolution, TransportMessage,
};
use crate::zenoh_lite::codec::{VERSION, WhatAmI};

/// Input bytes handed out a few at a time, then nothing ever again;
/// output collected.
struct Script {
    input: Vec<u8>,
    pos: usize,
    out: Vec<u8>,
}

impl ErrorType for Script {
    type Error = Infallible;
}

impl Read for Script {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Infallible> {
        if self.pos >= self.input.len() {
            return poll_fn(|_| Poll::Pending).await;
        }
        let n = buf.len().min(self.input.len() - self.pos).min(7);
        buf[..n].copy_from_slice(&self.input[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl Write for Script {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Infallible> {
        self.out.extend_from_slice(buf);
        Ok(buf.len())
    }

    async fn flush(&mut self) -> Result<(), Infallible> {
        Ok(())
    }
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

const COOKIE: &[u8] = b"cookie!";

fn router() -> Zid {
    Zid::from_slice(&[0xee, 2]).expect("zid")
}

fn batch(
    input: &mut Vec<u8>,
    body: impl FnOnce(&mut super::super::codec::Writer<'_>) -> Result<(), CodecError>,
) {
    let mut buf = [0u8; 512];
    let n = write_batch(&mut buf, u16::MAX, body).expect("fits");
    input.extend_from_slice(&buf[..n]);
}

fn frame(sn: u32) -> FrameHeader {
    FrameHeader {
        reliable: true,
        sn,
        qos: FrameHeader::DEFAULT_QOS,
    }
}

fn put_push(we: WireExpr<'static>, payload: &'static [u8]) -> NetworkMessage<'static> {
    NetworkMessage::Push(Push {
        wire_expr: we,
        exts: NetExts::DEFAULT,
        body: PushBody::Put(Put {
            payload,
            ..Put::default()
        }),
    })
}

/// Splits the client's output into its batches' items.
fn written(out: &[u8]) -> Vec<Item<'_>> {
    let mut items = Vec::new();
    let mut rest = out;
    while let Some((len, tail)) = rest.split_first_chunk::<2>() {
        let len = usize::from(u16::from_le_bytes(*len));
        let (b, tail) = tail.split_at(len);
        items.extend(Batch::new(b).map(|i| i.expect("client output decodes")));
        rest = tail;
    }
    items
}

fn script() -> Vec<u8> {
    let mut input = Vec::new();
    batch(&mut input, |w| {
        InitAck {
            version: VERSION,
            whatami: WhatAmI::Router,
            zid: router(),
            resolution: Resolution::DEFAULT,
            batch_size: 1024,
            cookie: COOKIE,
            qos: false,
            patch: 0,
        }
        .write(w)
    });
    batch(&mut input, |w| {
        OpenAck {
            lease_ms: 10_000,
            initial_sn: 100,
            qos: false,
        }
        .write(w)
    });
    // A malformed batch, then an out-of-order frame (99 precedes 100).
    batch(&mut input, |w| w.bytes(&[0x07, 0xff]));
    batch(&mut input, |w| {
        frame(99).write(w)?;
        put_push(WireExpr::plain("k/dropped"), b"no").write(w)
    });
    // In order: an interest for subscribers and queryables, a key
    // expression id, a push using it, and a push for an undeclared key.
    batch(&mut input, |w| {
        frame(100).write(w)?;
        NetworkMessage::Interest(Interest {
            id: 5,
            mode: InterestMode::Current,
            options: interest::SUBSCRIBERS | interest::QUERYABLES,
            wire_expr: None,
            exts: NetExts::DEFAULT,
        })
        .write(w)?;
        NetworkMessage::Declare(Declare {
            interest_id: None,
            exts: NetExts::DEFAULT,
            body: DeclareBody::KeyExpr {
                id: 3,
                wire_expr: WireExpr::plain("k"),
            },
        })
        .write(w)?;
        put_push(WireExpr::plain("other"), b"not ours").write(w)?;
        put_push(
            WireExpr {
                scope: 3,
                suffix: "/a",
                sender_mapping: true,
            },
            b"payload",
        )
        .write(w)
    });
    input
}

fn client() -> ZenohLite<Script, Fake, 512, 512> {
    let zid = Zid::from_slice(&[0x11, 0x22]).expect("zid");
    let stream = Script {
        input: script(),
        pos: 0,
        out: Vec::new(),
    };
    ZenohLite::new(stream, Fake(0), Config::new(zid))
}

#[test]
fn zn2_client_opens_redeclares_answers_interests_and_resolves_router_ids() {
    let mut lite = client();
    futures::executor::block_on(async {
        lite.declare_subscriber(Filter::Prefix(Key::from_static("k")))
            .await
            .expect("table");
        lite.declare_queryable(Key::from_static("q"))
            .await
            .expect("table");
        // Not open yet: nothing written, everything sent at open.
        assert!(lite.stream_mut().out.is_empty());
        lite.open().await.expect("open");
        let mut buf = [0u8; 64];
        let ev = lite.next_event(&mut buf).await.expect("event");
        let Event::Sample(s) = ev else {
            panic!("{ev:?}")
        };
        assert_eq!((s.key.as_str(), s.payload), ("k/a", &b"payload"[..]));
        assert_eq!(s.meta, SampleMeta::NONE);
        // Then nothing more: the router's lease ends the session.
        let end = lite.next_event(&mut buf).await;
        assert_eq!(end, Err(LiteError::Down(DownReason::LeaseExpired)));
    });
    let stats = lite.stats();
    // The malformed batch, the out-of-order frame and its message.
    assert_eq!(stats.refused, 3, "{stats:?}");
    assert_eq!(stats.dropped_samples, 1, "{stats:?}");

    let out = core::mem::take(&mut lite.stream_mut().out);
    let items = written(&out);
    let node = wire::node_key_buf(lite.sender_id(), Key::from_static("q")).expect("fits");
    let mut it = items.iter();
    // Handshake: InitSyn with our batch size, OpenSyn echoing the cookie.
    let Some(Item::Transport(TransportMessage::InitSyn(syn))) = it.next() else {
        panic!("{items:?}")
    };
    assert_eq!(syn.batch_size, 512);
    let Some(Item::Transport(TransportMessage::OpenSyn(open))) = it.next() else {
        panic!("{items:?}")
    };
    assert_eq!(open.cookie, COOKIE);
    // Then frames numbered from our initial number, each with one message:
    // three declarations, the answers to interest 5 and its final.
    let mut sn = open.initial_sn;
    let mut declares = Vec::new();
    while let Some(item) = it.next() {
        match item {
            Item::Transport(TransportMessage::Frame(f)) => {
                assert_eq!(f.sn, sn);
                sn += 1;
                let Some(Item::Network(NetworkMessage::Declare(d))) = it.next() else {
                    panic!("{items:?}")
                };
                declares.push(*d);
            }
            Item::Transport(TransportMessage::KeepAlive)
            | Item::Transport(TransportMessage::Close(_)) => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    let shape: Vec<(Option<u32>, Option<&str>)> = declares
        .iter()
        .map(|d| {
            let expr = match d.body {
                DeclareBody::Subscriber { wire_expr, .. }
                | DeclareBody::Queryable { wire_expr, .. } => Some(wire_expr.suffix),
                _ => None,
            };
            (d.interest_id, expr)
        })
        .collect();
    assert_eq!(
        shape,
        vec![
            (None, Some("k/**")),
            (None, Some("q")),
            (None, Some(node.as_str())),
            (Some(5), Some("k/**")),
            (Some(5), Some("q")),
            (Some(5), Some(node.as_str())),
            (Some(5), None),
        ]
    );
    assert!(matches!(
        declares.last().map(|d| d.body),
        Some(DeclareBody::Final)
    ));
}

#[test]
fn zn2_client_refuses_to_send_before_open_and_oversized_messages() {
    let mut lite = client();
    futures::executor::block_on(async {
        let k = Key::from_static("k");
        assert_eq!(lite.publish(k, b"x").await, Err(LiteError::NotOpen));
        let mut buf = [0u8; 8];
        assert_eq!(
            lite.next_event(&mut buf).await.err(),
            Some(LiteError::NotOpen)
        );
        lite.open().await.expect("open");
        // Larger than the 512-byte send buffer: refused, nothing sent.
        let before = lite.stream_mut().out.len();
        let big = [0u8; 600];
        assert_eq!(lite.publish(k, &big).await, Err(LiteError::TooLarge));
        assert_eq!(lite.stream_mut().out.len(), before);
        // Within the limit it goes out.
        let fits = [0u8; 512 - PUB_OVERHEAD];
        lite.publish(Key::new(&"k".repeat(MAX_KEY_LEN)).expect("key"), &fits)
            .await
            .expect("fits with the longest key");
        assert!(lite.stream_mut().out.len() > before);
        // A sample that does not fit the buffer is kept for a larger one.
        lite.declare_subscriber(Filter::Prefix(k))
            .await
            .expect("table");
        let mut tiny = [0u8; 4];
        assert_eq!(
            lite.next_event(&mut tiny).await.err(),
            Some(LiteError::BufferTooSmall {
                needed: 10,
                capacity: 4
            })
        );
        let mut buf = [0u8; 16];
        let ev = lite.next_event(&mut buf).await.expect("kept");
        assert!(matches!(ev, Event::Sample(s) if s.payload == b"payload"));
        // Closing sends Close and stays closed.
        lite.close().await.expect("close");
        assert_eq!(lite.open().await, Err(LiteError::NotOpen));
        lite.restart();
    });
}
