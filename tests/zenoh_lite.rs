//! ZN3: the board's Zenoh client (`zenoh_lite`) against a real `zenoh` 1.10
//! session on the host.
//!
//! The client runs over a tokio `TcpStream` adapted to `embedded-io-async`
//! (as the board's embassy-net socket is), with a tokio clock. The host is
//! a `ZenohTransport` whose session listens on one explicit `127.0.0.1`
//! endpoint with multicast scouting and gossip off ([`SessionConfig`],
//! design.md Z4); the port is reserved by binding port 0, never probed, and
//! the client connects only to it.
#![cfg(all(feature = "zenoh", feature = "zenoh-lite"))]

use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

use embedded_io_async::{ErrorKind, ErrorType, Read, Write};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use middleware::transport::{Flow, Gather, HandlerError, Request, RequestOptions, Target};
use middleware::zenoh::{SessionConfig, ZenohError, ZenohTransport};
use middleware::zenoh_lite::{Clock, Config, LiteError, ZenohLite, Zid};
use middleware::{Key, KeyFilter, Publisher, Requester, Responder, Subscribe, Subscriber};

const STEP: Duration = Duration::from_secs(10);

/// A tokio TCP stream as an `embedded-io-async` stream.
struct Tcp(TcpStream);

#[derive(Debug, PartialEq)]
struct IoError(std::io::ErrorKind);

impl std::fmt::Display for IoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

impl std::error::Error for IoError {}

impl embedded_io_async::Error for IoError {
    fn kind(&self) -> ErrorKind {
        ErrorKind::Other
    }
}

impl ErrorType for Tcp {
    type Error = IoError;
}

impl Read for Tcp {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, IoError> {
        self.0.read(buf).await.map_err(|e| IoError(e.kind()))
    }
}

impl Write for Tcp {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, IoError> {
        self.0.write(buf).await.map_err(|e| IoError(e.kind()))
    }

    async fn flush(&mut self) -> Result<(), IoError> {
        self.0.flush().await.map_err(|e| IoError(e.kind()))
    }
}

/// Milliseconds since the test started, and tokio sleeps.
struct TokioClock(Instant);

impl Clock for TokioClock {
    fn now_ms(&mut self) -> u64 {
        u64::try_from(self.0.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    async fn sleep_until_ms(&mut self, deadline_ms: u64) {
        tokio::time::sleep_until((self.0 + Duration::from_millis(deadline_ms)).into()).await;
    }
}

type Lite = ZenohLite<Tcp, TokioClock>;

/// A port on 127.0.0.1 nothing listens on: bound once and released.
async fn free_loopback_port() -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind port 0");
    l.local_addr().expect("addr").port()
}

/// A host session listening on one explicit loopback endpoint, and a
/// client connected and opened to it.
async fn host_and_lite(zid_byte: u8) -> (ZenohTransport, Lite) {
    host_and_lite_with(zid_byte, |c| c).await
}

async fn host_and_lite_with(
    zid_byte: u8,
    tweak: impl FnOnce(Config) -> Config,
) -> (ZenohTransport, Lite) {
    let port = free_loopback_port().await;
    let config = SessionConfig::new()
        .listen(format!("tcp/127.0.0.1:{port}"))
        .build()
        .expect("config");
    let host = ZenohTransport::open(config)
        .await
        .expect("host session")
        .with_timeout(Duration::from_secs(2));
    let tcp = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect to the host listener");
    let mut zid = [zid_byte; 16];
    zid[0] = 0x4c;
    let config = tweak(Config::new(Zid::new(zid).expect("non-zero")));
    let mut lite = ZenohLite::new(Tcp(tcp), TokioClock(Instant::now()), config);
    tokio::time::timeout(STEP, lite.open())
        .await
        .expect("handshake in time")
        .expect("handshake");
    assert!(lite.session().is_open());
    (host, lite)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zn3_host_subscriber_receives_lite_publications_with_attachments_and_gaps() {
    let (host, mut lite) = host_and_lite(1).await;
    let key = Key::from_static("zn3/imu");
    let mut sub = host.subscriber(key).await.expect("host subscriber");
    let mut buf = [0u8; 256];

    // Repeat until the subscription is in place on the host's side.
    let mut first = None;
    for _ in 0..50 {
        lite.publish(key, b"one").await.expect("publish");
        if let Ok(Ok(s)) =
            tokio::time::timeout(Duration::from_millis(100), sub.recv(&mut buf)).await
        {
            first = Some((s.key.as_str().to_owned(), s.payload.to_vec(), s.meta));
            break;
        }
    }
    let (k, payload, meta) = first.expect("a publication arrived");
    assert_eq!((k.as_str(), payload.as_slice()), ("zn3/imu", &b"one"[..]));
    assert_eq!(meta.sender, Some(lite.sender_id()));
    let mut first_seq = meta.seq.expect("sequence in the attachment");
    // Repeats sent while waiting may still be on their way.
    while let Ok(Ok(s)) = tokio::time::timeout(Duration::from_millis(300), sub.recv(&mut buf)).await
    {
        assert_eq!(s.payload, b"one");
        assert_eq!(s.meta.gap, Some(0));
        first_seq = s.meta.seq.expect("sequence");
    }

    // The next one follows with no gap.
    lite.publish(key, b"two").await.expect("publish");
    let s = tokio::time::timeout(STEP, sub.recv(&mut buf))
        .await
        .expect("in time")
        .expect("sample");
    assert_eq!(s.payload, b"two");
    assert_eq!(s.meta.seq, Some(first_seq.wrapping_add(1)));
    assert_eq!(s.meta.gap, Some(0));

    // A publication too large for one batch is refused but numbered, so
    // the next one shows a gap of one.
    let big = vec![0u8; 4096];
    assert_eq!(lite.publish(key, &big).await, Err(LiteError::TooLarge));
    lite.publish(key, b"three").await.expect("publish");
    let s = tokio::time::timeout(STEP, sub.recv(&mut buf))
        .await
        .expect("in time")
        .expect("sample");
    assert_eq!(s.payload, b"three");
    assert_eq!(s.meta.seq, Some(first_seq.wrapping_add(3)));
    assert_eq!(s.meta.gap, Some(1));
    assert_eq!(s.meta.sender, Some(lite.sender_id()));
    assert_eq!(lite.stats().refused, 0);
}

/// Echoes the request reversed; empty requests are `InvalidRequest`.
fn reverse(r: &Request<'_>, resp: &mut [u8]) -> Result<usize, HandlerError> {
    if r.body.is_empty() {
        return Err(HandlerError::InvalidRequest);
    }
    let out = resp
        .get_mut(..r.body.len())
        .ok_or(HandlerError::BufferTooSmall)?;
    out.copy_from_slice(r.body);
    out.reverse();
    Ok(r.body.len())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zn3_lite_queryable_answers_host_gets() {
    let (mut host, mut lite) = host_and_lite(2).await;
    let key = Key::from_static("zn3/echo");
    lite.declare_queryable(key).await.expect("declare");
    let lite_id = lite.sender_id();
    let host_id = host.sender_id();
    let seen = RefCell::new(Vec::new());

    // Exactly the four queries below reach the board (a get before the
    // declaration is known finds no queryable and never gets here).
    let serve = async {
        let mut handler = |r: &Request<'_>, resp: &mut [u8]| {
            seen.borrow_mut()
                .push((r.key.as_str().to_owned(), r.meta.sender, r.meta.deadline_ms));
            reverse(r, resp)
        };
        let mut scratch = [0u8; 256];
        for _ in 0..4 {
            lite.serve_one(&mut handler, &mut scratch)
                .await
                .expect("serve");
        }
    };
    let ask = async {
        let mut out = [0u8; 64];
        // Until the queryable is known on the host's side.
        let mut reply = None;
        for _ in 0..50 {
            match host.request(key, b"abc", &mut out).await {
                Ok(r) => {
                    reply = Some(r.to_vec());
                    break;
                }
                Err(ZenohError::NoReply) => tokio::time::sleep(Duration::from_millis(50)).await,
                Err(e) => panic!("{e:?}"),
            }
        }
        assert_eq!(reply.as_deref(), Some(&b"cba"[..]));
        // Addressed to the board's node id: its per-node queryable answers.
        let node = RequestOptions::DEFAULT.with_target(Target::Node(lite_id));
        let r = host
            .request_with(key, b"xyz", &mut out, &node)
            .await
            .expect("node request");
        assert_eq!(r, b"zyx");
        // A handler error reaches the host as the handler's error.
        let err = host.request(key, b"", &mut out).await;
        assert!(
            matches!(err, Err(ZenohError::Handler(HandlerError::InvalidRequest))),
            "{err:?}"
        );
        // Gather sees the board as the replier.
        let mut repliers = Vec::new();
        let n = host
            .request_each(key, b"ok", &mut out, &RequestOptions::DEFAULT, 4, |rep| {
                repliers.push((rep.sender, rep.result.map(<[u8]>::to_vec)));
                Flow::Continue
            })
            .await
            .expect("gather");
        assert_eq!(n, 1);
        assert_eq!(repliers, vec![(Some(lite_id), Ok(b"ko".to_vec()))]);
    };
    tokio::time::timeout(Duration::from_secs(30), async { tokio::join!(serve, ask) })
        .await
        .expect("in time");
    let seen = seen.into_inner();
    assert_eq!(seen.len(), 4, "{seen:?}");
    assert_eq!(lite.stats().refused, 0);
    for (k, sender, deadline) in &seen {
        assert_eq!(k, "zn3/echo");
        assert_eq!(*sender, Some(host_id));
        assert_eq!(*deadline, Some(2000));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zn5_one_node_prefix_queryable_answers_every_per_node_key_under_it() {
    let (mut host, mut lite) = host_and_lite(3).await;
    lite.declare_plain_queryable(Key::from_static("zn5/info"))
        .await
        .expect("declare");
    lite.declare_node_prefix_queryable(Key::from_static("zn5"))
        .await
        .expect("declare");
    let lite_id = lite.sender_id();
    let seen = RefCell::new(Vec::new());
    let serve = async {
        let mut handler = |r: &Request<'_>, resp: &mut [u8]| {
            seen.borrow_mut().push(r.key.as_str().to_owned());
            reverse(r, resp)
        };
        let mut scratch = [0u8; 256];
        for _ in 0..4 {
            lite.serve_one(&mut handler, &mut scratch)
                .await
                .expect("serve");
        }
    };
    let ask = async {
        let mut out = [0u8; 64];
        let node = RequestOptions::DEFAULT.with_target(Target::Node(lite_id));
        // Until the declarations are known on the host's side.
        let mut first = None;
        for _ in 0..50 {
            match host
                .request_with(
                    Key::from_static("zn5/loader/chunk"),
                    b"abc",
                    &mut out,
                    &node,
                )
                .await
            {
                Ok(r) => {
                    first = Some(r.to_vec());
                    break;
                }
                Err(ZenohError::NoReply) => tokio::time::sleep(Duration::from_millis(50)).await,
                Err(e) => panic!("{e:?}"),
            }
        }
        assert_eq!(first.as_deref(), Some(&b"cba"[..]));
        let r = host
            .request_with(Key::from_static("zn5/fw/reboot"), b"xy", &mut out, &node)
            .await
            .expect("node request");
        assert_eq!(r, b"yx");
        // The plain key answers broadcast queries, and per-node through
        // the prefix.
        let r = host
            .request(Key::from_static("zn5/info"), b"12", &mut out)
            .await
            .expect("plain request");
        assert_eq!(r, b"21");
        let r = host
            .request_with(Key::from_static("zn5/info"), b"34", &mut out, &node)
            .await
            .expect("node request");
        assert_eq!(r, b"43");
        // A key outside the prefix finds no queryable.
        let r = host
            .request_with(Key::from_static("other/x"), b"1", &mut out, &node)
            .await;
        assert!(matches!(r, Err(ZenohError::NoReply)), "{r:?}");
    };
    tokio::time::timeout(Duration::from_secs(30), async { tokio::join!(serve, ask) })
        .await
        .expect("in time");
    assert_eq!(
        seen.into_inner(),
        ["zn5/loader/chunk", "zn5/fw/reboot", "zn5/info", "zn5/info"],
        "the board sees each concrete key"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zn3_lite_receives_host_publications_on_declared_keys() {
    let (mut host, mut lite) = host_and_lite(3).await;
    let host_id = host.sender_id();
    let done = Cell::new(false);
    let mut sub = lite
        .subscribe(KeyFilter::Prefix(Key::from_static("zn3/cmd")))
        .await
        .expect("declare");
    let receive = async {
        let mut buf = [0u8; 256];
        let mut got = Vec::new();
        while got.len() < 3 {
            let s = tokio::time::timeout(STEP, sub.recv(&mut buf))
                .await
                .expect("in time")
                .expect("sample");
            got.push((s.key.as_str().to_owned(), s.payload.to_vec(), s.meta));
        }
        done.set(true);
        got
    };
    let publish = async {
        let mut i = 0u8;
        while !done.get() && i < 200 {
            // A key the board did not declare, then one it did.
            host.publish(Key::from_static("zn3/other"), b"no")
                .await
                .expect("publish");
            host.publish(Key::from_static("zn3/cmd/led"), &[i])
                .await
                .expect("publish");
            i += 1;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    let (got, ()) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(receive, publish)
    })
    .await
    .expect("in time");
    for (k, payload, meta) in &got {
        assert_eq!(k, "zn3/cmd/led");
        assert_eq!(payload.len(), 1);
        assert_eq!(meta.sender, Some(host_id));
        assert!(meta.seq.is_some());
    }
    // Consecutive samples of one host stream: no gap.
    assert_eq!(got[1].2.gap, Some(0));
    assert_eq!(got[2].2.seq, got[1].2.seq.map(|s| s.wrapping_add(1)));
    assert_eq!(lite.stats().refused, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zn3_lite_requests_reach_a_host_responder() {
    let (host, mut lite) = host_and_lite(4).await;
    let key = Key::from_static("zn3/host/echo");
    let mut responder = host.responder(key).await.expect("responder");
    let lite_id = lite.sender_id();
    let host_id = host.sender_id();
    let done = Cell::new(false);
    let serve = async {
        let mut handler = |r: &Request<'_>, resp: &mut [u8]| {
            assert_eq!(r.meta.sender, Some(lite_id));
            reverse(r, resp)
        };
        let mut scratch = [0u8; 256];
        while !done.get() {
            if let Ok(r) = tokio::time::timeout(
                Duration::from_millis(100),
                responder.serve_one(&mut handler, &mut scratch),
            )
            .await
            {
                r.expect("serve");
            }
        }
    };
    let ask = async {
        let mut out = [0u8; 64];
        let mut reply = None;
        for _ in 0..50 {
            let opts = RequestOptions::DEFAULT.with_timeout(Duration::from_millis(500));
            match lite.request_with(key, b"lite", &mut out, &opts).await {
                Ok(r) => {
                    reply = Some(r.to_vec());
                    break;
                }
                Err(LiteError::NoReply) => tokio::time::sleep(Duration::from_millis(50)).await,
                Err(e) => panic!("{e:?}"),
            }
        }
        assert_eq!(reply.as_deref(), Some(&b"etil"[..]));
        // Addressed to the host's node, and an error reply.
        let node = RequestOptions::DEFAULT.with_target(Target::Node(host_id));
        assert_eq!(
            lite.request_with(key, b"ab", &mut out, &node)
                .await
                .map(<[u8]>::to_vec),
            Ok(b"ba".to_vec())
        );
        assert_eq!(
            lite.request(key, b"", &mut out).await.map(<[u8]>::to_vec),
            Err(LiteError::Handler(HandlerError::InvalidRequest))
        );
        // Nobody serves this key: no reply, quickly.
        let opts = RequestOptions::DEFAULT.with_timeout(Duration::from_millis(300));
        assert_eq!(
            lite.request_with(Key::from_static("zn3/nobody"), b"", &mut out, &opts)
                .await
                .map(<[u8]>::to_vec),
            Err(LiteError::NoReply)
        );
        done.set(true);
    };
    tokio::time::timeout(Duration::from_secs(30), async { tokio::join!(serve, ask) })
        .await
        .expect("in time");
    assert_eq!(lite.stats().refused, 0);
    lite.close().await.expect("close");
    assert!(!lite.session().is_open());
    assert_eq!(lite.publish(key, b"x").await, Err(LiteError::NotOpen));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zn3_keep_alives_hold_a_short_lease_while_idle() {
    // The router expires the board after the 1 s lease it announced
    // unless keep-alives (every 250 ms of silence) arrive.
    let (host, mut lite) = host_and_lite_with(5, |c| Config {
        lease_ms: 1_000,
        ..c
    })
    .await;
    let key = Key::from_static("zn3/alive");
    let mut sub = host.subscriber(key).await.expect("host subscriber");
    let mut buf = [0u8; 64];
    // Idle for three leases, driven by a receive that times out.
    let idle = tokio::time::timeout(Duration::from_secs(3), lite.next_event(&mut buf)).await;
    assert!(idle.is_err(), "nothing was expected: {idle:?}");
    assert!(lite.session().is_open());
    let mut got = false;
    for _ in 0..50 {
        lite.publish(key, b"still here").await.expect("publish");
        if let Ok(Ok(s)) =
            tokio::time::timeout(Duration::from_millis(100), sub.recv(&mut buf)).await
        {
            assert_eq!(s.payload, b"still here");
            got = true;
            break;
        }
    }
    assert!(got, "the link survived the idle time");
    assert_eq!(lite.stats().refused, 0);
}
