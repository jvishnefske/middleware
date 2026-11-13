//! FL1: the multi-link bus's routing core.

extern crate std;

use super::*;
use std::vec::Vec as StdVec;

/// Two publications per queue, two replies, two expressions, 16-byte bodies.
type Bus = Links<2, 2, 2, 16>;

fn k(s: &str) -> Key<'_> {
    Key::new(s).expect("valid key")
}

/// No Zenoh peer subscribed to anything.
fn no_peer(_: LinkId, _: Key<'_>) -> bool {
    false
}

fn drain(bus: &mut Bus, link: LinkId) -> StdVec<(std::string::String, StdVec<u8>)> {
    core::iter::from_fn(|| bus.pop(link))
        .map(|p| (p.key().as_str().into(), p.payload.to_vec()))
        .collect()
}

fn sub(bus: &mut Bus, link: LinkId, expr: &str) -> Option<Result<(), HandlerError>> {
    bus.link_request(link, keys::SUB, expr.as_bytes())
}

#[test]
fn fl1_expressions_match_chunks_with_star_and_double_star() {
    let yes = [
        ("a/b", "a/b"),
        ("a/*", "a/b"),
        ("*/status", "payload/status"),
        ("a/**", "a"),
        ("a/**", "a/b/c"),
        ("**", "x"),
        ("**", "x/y/z"),
        ("a/**/c", "a/c"),
        ("a/**/c", "a/b/b/c"),
        ("**/c", "a/b/c"),
        ("a/**/b/**/c", "a/x/b/y/z/c"),
        ("*/*", "a/b"),
    ];
    for (e, key) in yes {
        assert!(Expr::new(e).unwrap().matches(k(key)), "{e} {key}");
    }
    let no = [
        ("a/b", "a/b/c"),
        ("a/b", "a"),
        ("a/*", "a"),
        ("a/*", "a/b/c"),
        ("*/status", "status"),
        ("*/status", "a/b/status"),
        ("a/**/c", "a/b/d"),
        ("app", "apple"),
        ("a/**/b/**/c", "a/x/b/y/z"),
    ];
    for (e, key) in no {
        assert!(!Expr::new(e).unwrap().matches(k(key)), "{e} {key}");
    }
}

#[test]
fn fl1_invalid_expressions_are_refused() {
    let long = "a".repeat(MAX_KEY_LEN + 1);
    for (e, err) in [
        ("", ExprError::Length),
        (long.as_str(), ExprError::Length),
        ("/a", ExprError::Chunk),
        ("a/", ExprError::Chunk),
        ("a//b", ExprError::Chunk),
        ("a/***", ExprError::Chunk),
        ("a/b*", ExprError::Chunk),
        ("a/$*", ExprError::Chunk),
        ("@mw/x", ExprError::Chunk),
        ("a b", ExprError::Chunk),
    ] {
        assert_eq!(Expr::new(e), Err(err), "{e:?}");
    }
    assert_eq!(Expr::from_body(&[0xff, 0xfe]), Err(ExprError::Chunk));
    assert_eq!(
        Expr::new(&"a".repeat(MAX_KEY_LEN)).map(|e| e.as_str().len()),
        Ok(MAX_KEY_LEN)
    );
}

#[test]
fn fl1_a_publication_reaches_exactly_the_links_whose_filter_matches() {
    let mut bus = Bus::evkb();
    assert_eq!(bus.filter(LinkId::UART), FilterView::Everything);
    assert_eq!(bus.filter(LinkId::ZENOH), FilterView::Remote);
    assert_eq!(sub(&mut bus, LinkId::USB, "sensor/**"), Some(Ok(())));

    // The Zenoh link: only what a peer declared (here `sensor/imu`).
    let peers = |link: LinkId, key: Key<'_>| {
        assert_eq!(link, LinkId::ZENOH, "asked only for the remote link");
        key.as_str() == "sensor/imu"
    };
    let f = bus.publish(k("sensor/imu"), b"xyz", peers).unwrap();
    assert_eq!(
        f.queued,
        LinkSet::EMPTY
            .with(LinkId::UART)
            .with(LinkId::USB)
            .with(LinkId::ZENOH)
    );
    assert!(f.dropped.is_empty());
    let f = bus.publish(k("sensor/mic"), b"m", peers).unwrap();
    assert_eq!(
        f.queued,
        LinkSet::EMPTY.with(LinkId::UART).with(LinkId::USB)
    );
    // Only the UART link selects `sys/log`, and its queue is full.
    let f = bus.publish(k("sys/log"), b"hi", peers).unwrap();
    assert_eq!(
        (f.queued, f.dropped),
        (LinkSet::EMPTY, LinkSet::EMPTY.with(LinkId::UART))
    );
    assert_eq!(bus.dropped(LinkId::UART), 1);

    let s = |v: &str| std::string::String::from(v);
    assert_eq!(
        drain(&mut bus, LinkId::UART),
        [
            (s("sensor/imu"), b"xyz".to_vec()),
            (s("sensor/mic"), b"m".to_vec())
        ],
        "the UART queue holds two; sys/log was the third and dropped"
    );
    assert_eq!(
        drain(&mut bus, LinkId::USB),
        [
            (s("sensor/imu"), b"xyz".to_vec()),
            (s("sensor/mic"), b"m".to_vec())
        ]
    );
    assert_eq!(
        drain(&mut bus, LinkId::ZENOH),
        [(s("sensor/imu"), b"xyz".to_vec())]
    );
    let spare = LinkId::new(3).unwrap();
    assert_eq!(bus.kind(spare), LinkKind::Absent);
    assert_eq!(bus.pop(spare), None);
    assert_eq!(bus.dropped(LinkId::USB), 0);
    assert_eq!(bus.dropped(LinkId::ZENOH), 0);
}

#[test]
fn fl1_a_full_queue_drops_and_counts_only_that_links_publications() {
    let mut bus = Bus::evkb();
    let all = |_: LinkId, _: Key<'_>| true;
    for i in 0..5u8 {
        let f = bus.publish(k("t"), &[i], all).unwrap();
        // The USB link is drained as it goes; the others are not.
        assert_eq!(
            bus.pop(LinkId::USB).map(|p| p.payload.to_vec()),
            Some(std::vec![i])
        );
        if i >= 2 {
            assert_eq!(
                f.dropped,
                LinkSet::EMPTY.with(LinkId::UART).with(LinkId::ZENOH)
            );
            assert!(f.queued.contains(LinkId::USB));
        }
    }
    assert_eq!(bus.dropped(LinkId::UART), 3);
    assert_eq!(bus.dropped(LinkId::ZENOH), 3);
    assert_eq!(bus.dropped(LinkId::USB), 0, "a draining link loses nothing");
    // The full queues kept their oldest publications.
    let oldest: StdVec<_> = drain(&mut bus, LinkId::UART)
        .into_iter()
        .map(|(_, p)| p)
        .collect();
    assert_eq!(oldest, [[0u8], [1u8]]);
    // Room again: queued, the count stays.
    assert!(
        bus.publish(k("t"), &[9], all)
            .unwrap()
            .queued
            .contains(LinkId::UART)
    );
    assert_eq!(bus.dropped(LinkId::UART), 3);
}

#[test]
fn fl1_too_large_publications_are_queued_nowhere() {
    let mut bus = Bus::evkb();
    assert_eq!(bus.publish(k("t"), &[0; 17], |_, _| true), Err(TooLarge));
    for link in LinkId::ALL {
        assert_eq!(bus.queued(link), 0);
        assert_eq!(bus.dropped(link), 0);
    }
    assert!(bus.publish(k("t"), &[0; 16], |_, _| true).is_ok());
}

#[test]
fn fl1_a_reply_goes_back_on_the_requests_link_only() {
    let mut bus = Bus::evkb();
    let usb = ReplyTo {
        link: LinkId::USB,
        id: 7,
    };
    let zenoh = ReplyTo {
        link: LinkId::ZENOH,
        id: 1_000_001,
    };
    assert_eq!(bus.reply(usb, Ok(b"info")), Ok(()));
    assert_eq!(bus.reply(zenoh, Err(HandlerError::Denied)), Ok(()));
    assert_eq!(bus.pop_reply(LinkId::UART), None);
    let r = bus.pop_reply(LinkId::USB).unwrap();
    assert_eq!(
        (r.id, r.result.map(|b| b.to_vec())),
        (7, Ok(b"info".to_vec()))
    );
    assert_eq!(bus.pop_reply(LinkId::USB), None);
    let r = bus.pop_reply(LinkId::ZENOH).unwrap();
    assert_eq!((r.id, r.result), (1_000_001, Err(HandlerError::Denied)));
    // An oversized body becomes BufferTooSmall; a full queue refuses.
    let uart = ReplyTo {
        link: LinkId::UART,
        id: 1,
    };
    assert_eq!(bus.reply(uart, Ok(&[0; 17])), Ok(()));
    assert_eq!(bus.reply(uart, Ok(b"")), Ok(()));
    assert_eq!(bus.reply(uart, Ok(b"")), Err(ReplyFull));
    assert_eq!(
        bus.pop_reply(LinkId::UART).unwrap().result,
        Err(HandlerError::BufferTooSmall)
    );
    let spare = ReplyTo {
        link: LinkId::new(3).unwrap(),
        id: 0,
    };
    assert_eq!(bus.reply(spare, Ok(b"")), Err(ReplyFull));
}

#[test]
fn fl1_sub_and_unsub_change_only_the_asking_link() {
    let mut bus = Bus::evkb();
    let only = |bus: &Bus, link| match bus.filter(link) {
        FilterView::Only(e) => e
            .iter()
            .map(|e| e.as_str().into())
            .collect::<StdVec<std::string::String>>(),
        other => panic!("{other:?}"),
    };
    assert_eq!(sub(&mut bus, LinkId::UART, "a/*"), Some(Ok(())));
    assert_eq!(only(&bus, LinkId::UART), ["a/*"]);
    assert_eq!(
        bus.filter(LinkId::USB),
        FilterView::Everything,
        "the other serial link is untouched"
    );
    assert_eq!(bus.filter(LinkId::ZENOH), FilterView::Remote);

    // Idempotent add; then capacity (two expressions).
    assert_eq!(sub(&mut bus, LinkId::UART, "a/*"), Some(Ok(())));
    assert_eq!(sub(&mut bus, LinkId::UART, "b"), Some(Ok(())));
    assert_eq!(
        sub(&mut bus, LinkId::UART, "c"),
        Some(Err(HandlerError::Unavailable))
    );
    assert_eq!(only(&bus, LinkId::UART), ["a/*", "b"]);

    // Bad bodies leave the filter as it was.
    for body in [&b""[..], b"a//b", b"\xff", b"a/b*"] {
        assert_eq!(
            bus.link_request(LinkId::UART, keys::SUB, body),
            Some(Err(HandlerError::InvalidRequest))
        );
        assert_eq!(
            bus.link_request(LinkId::UART, keys::UNSUB, body),
            Some(Err(HandlerError::InvalidRequest))
        );
    }
    assert_eq!(only(&bus, LinkId::UART), ["a/*", "b"]);

    // Unsub: removes; absent expression is fine; the last one leaves nothing.
    assert_eq!(
        bus.link_request(LinkId::UART, keys::UNSUB, b"a/*"),
        Some(Ok(()))
    );
    assert_eq!(
        bus.link_request(LinkId::UART, keys::UNSUB, b"zzz"),
        Some(Ok(()))
    );
    assert_eq!(only(&bus, LinkId::UART), ["b"]);
    assert_eq!(
        bus.link_request(LinkId::UART, keys::UNSUB, b"b"),
        Some(Ok(()))
    );
    assert_eq!(bus.filter(LinkId::UART), FilterView::Only(&[]));
    let f = bus.publish(k("b"), b"", no_peer).unwrap();
    assert!(!f.queued.contains(LinkId::UART) && f.queued.contains(LinkId::USB));

    // Unsub on a link that selects everything changes nothing.
    assert_eq!(
        bus.link_request(LinkId::USB, keys::UNSUB, b"b"),
        Some(Ok(()))
    );
    assert_eq!(bus.filter(LinkId::USB), FilterView::Everything);

    // `**` selects everything again; so does a reset.
    assert_eq!(sub(&mut bus, LinkId::UART, "**"), Some(Ok(())));
    assert!(
        bus.publish(k("x/y"), b"", no_peer)
            .unwrap()
            .queued
            .contains(LinkId::UART)
    );
    assert_eq!(sub(&mut bus, LinkId::UART, "q"), Some(Ok(())));
    bus.reset(LinkId::UART);
    assert_eq!(bus.filter(LinkId::UART), FilterView::Everything);
    assert_eq!(bus.queued(LinkId::UART), 0);
}

#[test]
fn fl1_link_requests_are_only_for_serial_links_and_link_keys() {
    let mut bus = Bus::evkb();
    assert_eq!(bus.link_request(LinkId::UART, k("sys/info"), b""), None);
    assert_eq!(
        bus.link_request(LinkId::UART, k("sys/link/other"), b"a"),
        None
    );
    assert!(is_link_key(keys::SUB) && is_link_key(keys::UNSUB) && !is_link_key(k("sys/link")));
    assert_eq!(
        sub(&mut bus, LinkId::ZENOH, "a"),
        Some(Err(HandlerError::NotFound))
    );
    assert_eq!(
        sub(&mut bus, LinkId::new(3).unwrap(), "a"),
        Some(Err(HandlerError::NotFound))
    );
    assert_eq!(bus.filter(LinkId::ZENOH), FilterView::Remote);
    assert_eq!(LinkId::new(MAX_LINKS), None);
    assert_eq!(LinkId::new(2), Some(LinkId::ZENOH));
}

#[test]
fn fl1_no_input_panics() {
    // Arbitrary bodies against both keys, on every link, with a matcher run
    // against the resulting filters.
    let mut bus = Bus::evkb();
    let bodies: [&[u8]; 9] = [
        b"**/**/**",
        b"*",
        b"a/**/**/b",
        b"\0",
        b"/",
        b"**/",
        b"a/./-_",
        b"*/*/*/*",
        b"\xc3\x28",
    ];
    for link in LinkId::ALL {
        for body in bodies {
            let _ = bus.link_request(link, keys::SUB, body);
            let _ = bus.publish(
                k("a/b/c/d"),
                body.get(..body.len().min(16)).unwrap_or(&[]),
                |_, _| true,
            );
            let _ = bus.link_request(link, keys::UNSUB, body);
        }
        while bus.pop(link).is_some() {}
    }
}
