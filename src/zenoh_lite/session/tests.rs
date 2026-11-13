//! ZN2: the session state machine. Handshake, lease and keep-alive
//! timing, sequence numbers, close and reconnect with bounded backoff, and
//! malformed or out-of-order input refused without panicking.

use super::super::codec::network::{NetExts, Push, PushBody, Put};
use super::super::codec::transport::{Batch, FragmentHeader};
use super::super::codec::{WireExpr, Writer};
use super::*;

fn zid(b: u8) -> Zid {
    Zid::from_slice(&[b, 0x5a]).expect("non-zero")
}

const COOKIE: &[u8] = b"opaque-cookie";

fn config() -> Config {
    Config {
        lease_ms: 4_000,
        open_timeout_ms: 1_000,
        backoff_min_ms: 100,
        backoff_max_ms: 1_000,
        ..Config::new(zid(1))
    }
}

fn ack(resolution: Resolution, batch: u16) -> Item<'static> {
    Item::Transport(TransportMessage::InitAck(InitAck {
        version: VERSION,
        whatami: WhatAmI::Router,
        zid: zid(2),
        resolution,
        batch_size: batch,
        cookie: COOKIE,
        qos: false,
        patch: 0,
    }))
}

fn open_ack(lease_ms: u64, initial_sn: u32) -> Item<'static> {
    Item::Transport(TransportMessage::OpenAck(OpenAck {
        lease_ms,
        initial_sn,
        qos: false,
    }))
}

fn frame(reliable: bool, sn: u32) -> Item<'static> {
    Item::Transport(TransportMessage::Frame(FrameHeader {
        reliable,
        sn,
        qos: FrameHeader::DEFAULT_QOS,
    }))
}

fn push() -> Item<'static> {
    Item::Network(NetworkMessage::Push(Push {
        wire_expr: WireExpr::plain("k"),
        exts: NetExts::DEFAULT,
        body: PushBody::Put(Put::default()),
    }))
}

/// A session opened at t=0 with the router's lease 3000 ms and initial
/// sequence number `rx_initial`, under `resolution`.
fn opened(resolution: Resolution, rx_initial: u32) -> Session {
    let (s, _) = Session::new(config()).connect(0).expect("connect");
    let (s, r) = s.on_item(0, ack(resolution, 1024));
    assert!(matches!(r, Reaction::Send(TransportMessage::OpenSyn(_))));
    let (s, r) = s.on_item(0, open_ack(3_000, rx_initial));
    assert_eq!(r, Reaction::Opened);
    s
}

#[test]
fn zn2_handshake_sends_init_then_open_with_the_cookie_and_opens() {
    let s = Session::new(config());
    let (s, syn) = s.connect(5).expect("may connect");
    assert_eq!(
        syn,
        InitSyn {
            version: VERSION,
            whatami: WhatAmI::Client,
            zid: zid(1),
            resolution: Resolution::DEFAULT,
            batch_size: 2048,
            qos: false,
            patch: 0,
        }
    );
    assert_eq!(s.phase(), Phase::InitSent { since: 5 });
    assert_eq!(s.connect(6).err(), Some(SessionError::NotDown));
    let (s, r) = s.on_item(7, ack(Resolution::DEFAULT, 1024));
    let Reaction::Send(TransportMessage::OpenSyn(o)) = r else {
        panic!("{r:?}")
    };
    assert_eq!((o.lease_ms, o.cookie), (4_000, COOKIE));
    assert_eq!(o.initial_sn & !Resolution::DEFAULT.sn_mask(), 0);
    // Deterministic for the same pair of ids.
    let (_, r2) = Session::new(config())
        .connect(0)
        .expect("connect")
        .0
        .on_item(0, ack(Resolution::DEFAULT, 1024));
    assert_eq!(r2, Reaction::Send(TransportMessage::OpenSyn(o)));
    let (s, r) = s.on_item(8, open_ack(3_000, 99));
    assert_eq!(r, Reaction::Opened);
    let link = s.link().expect("open");
    assert_eq!(
        (link.peer, link.batch_size, link.peer_lease_ms),
        (zid(2), 1024, 3_000)
    );
    // Our first frame carries the initial number from the OpenSyn.
    let (_, h) = s.next_frame(9, true).expect("open");
    assert_eq!(h.sn, o.initial_sn);
}

#[test]
fn zn2_keep_alive_and_lease_follow_the_clock() {
    let s = opened(Resolution::DEFAULT, 0);
    // Keep-alive every lease/4 = 1000 ms of silence; lease 3000 ms.
    assert_eq!(s.deadline(), Some(1_000));
    let (s, r) = s.tick(999);
    assert_eq!(r, Reaction::Nothing);
    let (s, r) = s.tick(1_000);
    assert_eq!(r, Reaction::Send(TransportMessage::KeepAlive));
    assert_eq!(s.deadline(), Some(2_000));
    // Sending a frame postpones the next keep-alive.
    let (s, _) = s.next_frame(1_500, true).expect("open");
    let (s, r) = s.tick(2_000);
    assert_eq!(r, Reaction::Nothing);
    // Receiving refreshes the router's lease.
    let (s, r) = s.on_item(2_900, Item::Transport(TransportMessage::KeepAlive));
    assert_eq!(r, Reaction::Nothing);
    let (s, r) = s.tick(5_000);
    assert_eq!(r, Reaction::Send(TransportMessage::KeepAlive));
    assert!(s.is_open());
    // Nothing heard for the lease: down, with a Close to send.
    let (s, r) = s.tick(5_900);
    assert_eq!(
        r,
        Reaction::Lost {
            reason: DownReason::LeaseExpired,
            close: Some(Close {
                reason: reason::EXPIRED,
                session: true
            })
        }
    );
    assert_eq!(s.phase(), Phase::Down { retry_at: 6_000 });
}

#[test]
fn zn2_outgoing_sequence_numbers_count_per_channel_and_wrap() {
    let (s, _) = Session::new(config()).connect(0).expect("connect");
    // 8-bit resolution: numbers wrap at 0x7f.
    let (s, r) = s.on_item(0, ack(Resolution(0b1000), 1024));
    let Reaction::Send(TransportMessage::OpenSyn(o)) = r else {
        panic!()
    };
    assert!(o.initial_sn <= 0x7f);
    let (mut s, _) = s.on_item(0, open_ack(3_000, 0));
    let mut expected = o.initial_sn;
    for _ in 0..300 {
        let (next, h) = s.next_frame(1, true).expect("open");
        assert_eq!(h.sn, expected);
        assert!(h.reliable);
        expected = (expected + 1) & 0x7f;
        s = next;
    }
    // Best effort has its own counter.
    let (_, h) = s.next_frame(1, false).expect("open");
    assert_eq!(h.sn, o.initial_sn);
    // Request ids count from 0 within the 32-bit id resolution.
    let (s, a) = s.next_request_id().expect("open");
    let (_, b) = s.next_request_id().expect("open");
    assert_eq!((a, b), (0, 1));
}

#[test]
fn zn2_incoming_frames_must_follow_and_out_of_order_ones_are_refused() {
    let s = opened(Resolution::DEFAULT, 10);
    // A network message before any frame.
    let (s, r) = s.on_item(1, push());
    assert_eq!(r, Reaction::Refused(Refusal::Unexpected));
    // 10 follows the initial number; its messages are delivered.
    let (s, r) = s.on_item(1, frame(true, 10));
    assert_eq!(r, Reaction::Nothing);
    let (s, r) = s.on_item(1, push());
    assert!(matches!(r, Reaction::Deliver(_)));
    // A repeat and an older number are refused, and so are their messages.
    for sn in [10, 9, 0x0fff_fff0] {
        let (s2, r) = s.on_item(1, frame(true, sn));
        assert_eq!(r, Reaction::Refused(Refusal::OutOfOrder), "{sn}");
        let (_, r) = s2.on_item(1, push());
        assert_eq!(r, Reaction::Refused(Refusal::OutOfOrder));
    }
    // A number past the resolution is refused.
    let (_, r) = s.on_item(1, frame(true, 0x1000_0000));
    assert_eq!(r, Reaction::Refused(Refusal::OutOfOrder));
    // A forward jump within half the range is followed (as Zenoh does).
    let (s, r) = s.on_item(1, frame(true, 15));
    assert_eq!(r, Reaction::Nothing);
    let (s, r) = s.on_item(1, frame(true, 16));
    assert_eq!(r, Reaction::Nothing);
    // Best effort is a separate channel starting at the same number.
    let (_, r) = s.on_item(1, frame(false, 10));
    assert_eq!(r, Reaction::Nothing);
    // Wrap-around: from the top of the range back to 0.
    let s = opened(Resolution::DEFAULT, 0x0fff_ffff);
    let (s, r) = s.on_item(1, frame(true, 0x0fff_ffff));
    assert_eq!(r, Reaction::Nothing);
    let (_, r) = s.on_item(1, frame(true, 0));
    assert_eq!(r, Reaction::Nothing);
}

#[test]
fn zn2_handshake_refuses_unexpected_or_unusable_answers() {
    let start = || Session::new(config()).connect(0).expect("connect").0;
    let refused = Reaction::Lost {
        reason: DownReason::HandshakeRefused,
        close: Some(Close {
            reason: reason::INVALID,
            session: true,
        }),
    };
    // OpenAck before InitAck, a frame, a KeepAlive.
    for item in [
        open_ack(3_000, 1),
        frame(true, 1),
        Item::Transport(TransportMessage::KeepAlive),
        Item::Oam,
    ] {
        let (s, r) = start().on_item(1, item);
        assert_eq!(r, refused, "{item:?}");
        assert_eq!(s.phase(), Phase::Down { retry_at: 101 });
    }
    // Another version, a client, a zero batch.
    let Item::Transport(TransportMessage::InitAck(good)) = ack(Resolution::DEFAULT, 1024) else {
        unreachable!()
    };
    for bad in [
        InitAck {
            version: VERSION + 1,
            ..good
        },
        InitAck {
            whatami: WhatAmI::Client,
            ..good
        },
        InitAck {
            batch_size: 0,
            ..good
        },
    ] {
        let (_, r) = start().on_item(1, Item::Transport(TransportMessage::InitAck(bad)));
        assert_eq!(r, refused);
    }
    // After InitAck: a zero lease, an initial number past the resolution,
    // a second InitAck.
    let after_ack = || start().on_item(1, ack(Resolution::DEFAULT, 1024)).0;
    for item in [
        open_ack(0, 1),
        open_ack(3_000, 0x1000_0000),
        ack(Resolution::DEFAULT, 1024),
    ] {
        let (_, r) = after_ack().on_item(2, item);
        assert_eq!(r, refused, "{item:?}");
    }
    // A Close from the router during the handshake.
    let close = Item::Transport(TransportMessage::Close(Close {
        reason: 3,
        session: true,
    }));
    let (_, r) = after_ack().on_item(2, close);
    assert_eq!(
        r,
        Reaction::Lost {
            reason: DownReason::Remote(3),
            close: None
        }
    );
    // Malformed input during the handshake ends it.
    let (_, r) = start().on_malformed(3, CodecError::Truncated);
    assert!(matches!(
        r,
        Reaction::Lost {
            reason: DownReason::Malformed(CodecError::Truncated),
            ..
        }
    ));
}

#[test]
fn zn2_open_session_refuses_handshake_messages_fragments_and_malformed_batches() {
    let s = opened(Resolution::DEFAULT, 0);
    for item in [
        ack(Resolution::DEFAULT, 1024),
        open_ack(3_000, 1),
        Item::Transport(TransportMessage::InitSyn(
            Session::new(config()).connect(0).expect("connect").1,
        )),
    ] {
        let (s2, r) = s.on_item(1, item);
        assert_eq!(r, Reaction::Refused(Refusal::Unexpected));
        assert!(s2.is_open());
    }
    let (s, r) = s.on_item(
        1,
        Item::Fragment(FragmentHeader {
            reliable: true,
            more: true,
            sn: 0,
        }),
    );
    assert_eq!(r, Reaction::Refused(Refusal::Fragment));
    let (s, r) = s.on_malformed(1, CodecError::Invalid);
    assert_eq!(
        r,
        Reaction::Refused(Refusal::Malformed(CodecError::Invalid))
    );
    assert!(s.is_open());
    // A Close from the router ends the session.
    let (s, r) = s.on_item(
        2,
        Item::Transport(TransportMessage::Close(Close {
            reason: 0,
            session: true,
        })),
    );
    assert_eq!(
        r,
        Reaction::Lost {
            reason: DownReason::Remote(0),
            close: None
        }
    );
    assert!(!s.is_open());
    // Down, items are refused.
    let (_, r) = s.on_item(3, frame(true, 1));
    assert_eq!(r, Reaction::Refused(Refusal::Unexpected));
}

#[test]
fn zn2_handshake_timeout_and_reconnect_backoff_are_bounded() {
    let s = Session::new(config());
    let (s, _) = s.connect(0).expect("connect");
    assert_eq!(s.deadline(), Some(1_000));
    let (s, r) = s.tick(999);
    assert_eq!(r, Reaction::Nothing);
    let (mut s, r) = s.tick(1_000);
    assert_eq!(
        r,
        Reaction::Lost {
            reason: DownReason::HandshakeTimeout,
            close: None
        }
    );
    // Backoff: 100, 200, 400, 800, then capped at 1000 ms.
    let mut now = 1_000u64;
    for expected in [100u64, 200, 400, 800, 1_000, 1_000, 1_000] {
        let Phase::Down { retry_at } = s.phase() else {
            panic!()
        };
        assert_eq!(retry_at, now + expected);
        assert_eq!(
            s.connect(retry_at - 1).err(),
            Some(SessionError::Backoff { until: retry_at })
        );
        now = retry_at;
        s = s.connect(now).expect("after backoff").0;
        s = s.link_lost(now);
    }
    // The cap holds for any number of attempts.
    for _ in 0..300 {
        s = s.link_lost(now);
    }
    assert_eq!(s.attempts(), u8::MAX);
    assert_eq!(
        s.phase(),
        Phase::Down {
            retry_at: now + 1_000
        }
    );
    // Opening resets the count.
    let now = now + 1_000;
    let (s, _) = s.connect(now).expect("connect");
    let (s, _) = s.on_item(now, ack(Resolution::DEFAULT, 1024));
    let (s, _) = s.on_item(now, open_ack(3_000, 0));
    assert_eq!(s.attempts(), 0);
    let s = s.link_lost(now);
    assert_eq!(
        s.phase(),
        Phase::Down {
            retry_at: now + 100
        }
    );
}

#[test]
fn zn2_close_stays_closed_until_restart() {
    let s = opened(Resolution::DEFAULT, 0);
    let (s, close) = s.close();
    assert_eq!(
        close,
        Some(Close {
            reason: reason::GENERIC,
            session: true
        })
    );
    assert_eq!(s.phase(), Phase::Closed);
    assert_eq!(s.deadline(), None);
    assert_eq!(s.connect(1_000_000).err(), Some(SessionError::NotDown));
    assert_eq!(s.next_frame(1, true).err(), Some(SessionError::NotOpen));
    assert_eq!(s.link_lost(5).phase(), Phase::Closed);
    let (s, close) = s.close();
    assert_eq!(close, None);
    let s = s.restart();
    assert!(s.connect(0).is_ok());
}

/// A deterministic byte source (xorshift).
struct Noise(u64);

impl Noise {
    fn next(&mut self) -> u8 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 24) as u8
    }
}

#[test]
fn zn2_random_and_mutated_batches_never_panic() {
    let mut noise = Noise(0x9e37_79b9_7f4a_7c15);
    // Valid seeds to mutate: a frame with a push, a handshake answer.
    let mut seed = [0u8; 64];
    let n = {
        let mut w = Writer::new(&mut seed);
        frame_bytes(&mut w);
        w.len()
    };
    let mut buf = [0u8; 96];
    for round in 0..20_000 {
        let len = usize::from(noise.next()) % buf.len();
        if round % 2 == 0 {
            buf[..n].copy_from_slice(&seed[..n]);
            for _ in 0..1 + noise.next() % 4 {
                let i = usize::from(noise.next()) % n;
                buf[i] = noise.next();
            }
        } else {
            for b in buf.iter_mut().take(len) {
                *b = noise.next();
            }
        }
        let bytes = &buf[..if round % 2 == 0 { n } else { len }];
        for start in [
            Session::new(config()).connect(0).expect("connect").0,
            opened(Resolution::DEFAULT, 0),
        ] {
            let mut s = start;
            for item in Batch::new(bytes) {
                s = match item {
                    Ok(i) => s.on_item(1, i).0,
                    Err(e) => s.on_malformed(1, e).0,
                };
            }
            let _ = s.tick(2);
        }
    }
}

fn frame_bytes(w: &mut Writer<'_>) {
    let h = FrameHeader {
        reliable: true,
        sn: 0,
        qos: FrameHeader::DEFAULT_QOS,
    };
    h.write(w).expect("fits");
    if let Item::Network(m) = push() {
        m.write(w).expect("fits");
    }
}
