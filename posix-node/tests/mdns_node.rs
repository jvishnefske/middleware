//! The node's mDNS announcement (D31, D32, D34), end to end over real
//! sockets: `mdns_node::run` with a loopback [`Net`] announcing to an
//! `MdnsSocket` browser.
//!
//! Network conduct: both sockets are bound on 127.0.0.x and aimed at each
//! other with an explicit destination (as middleware's `tests/mdns.rs`), so
//! nothing joins a multicast group or sends to 224.0.0.251.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use middleware::SenderId;
use middleware::discovery::{Browse, Endpoint, Event};
use middleware::dns::wire::Txt;
use middleware::key::KeyFilter;
use middleware::mdns::announce::MdnsAnnouncer;
use middleware::mdns::node::{self, NodeTxt};
use middleware::mdns::socket::{MAX_EVENT_LEN, MdnsError, MdnsSocket};
use mw_posix_node::config::MdnsMode;
use mw_posix_node::mdns_node::{self, Identity, Net, Report, Timing};
use tokio::sync::oneshot;

const NODE: SenderId = SenderId([1, 2, 3, 4, 5, 6, 7, 8]);
const SENDER: SenderId = SenderId([0xa0, 0xb1, 0xc2, 0xd3, 0xe4, 0xf5, 0x06, 0x17]);
const WAIT: Duration = Duration::from_secs(5);

async fn within<T>(f: impl Future<Output = T>) -> T {
    tokio::time::timeout(WAIT, f).await.expect("timed out")
}

/// Fast timers, so an address change or a retry happens within a test.
fn fast() -> Timing {
    Timing {
        recheck: Duration::from_millis(200),
        retry_min: Duration::from_millis(50),
        retry_max: Duration::from_millis(200),
        conflict: Duration::from_millis(200),
        goodbye: Duration::from_secs(2),
    }
}

/// Binds on the current 127.0.0.x address, announcing to `browser`.
#[derive(Clone)]
struct LoopbackNet {
    ip: Arc<Mutex<Option<Ipv4Addr>>>,
    browser: SocketAddr,
    failing_binds: Arc<AtomicU32>,
    binds: Arc<AtomicU32>,
}

impl LoopbackNet {
    fn set(&self, ip: Option<Ipv4Addr>) {
        *self.ip.lock().unwrap() = ip;
    }
}

impl Net for LoopbackNet {
    fn local_ip(&self, _mode: MdnsMode) -> io::Result<Ipv4Addr> {
        self.ip.lock().unwrap().ok_or_else(|| io::Error::from(io::ErrorKind::NetworkUnreachable))
    }

    async fn bind(&self, ip: Ipv4Addr, host: &str) -> Result<MdnsAnnouncer, MdnsError> {
        self.binds.fetch_add(1, Ordering::SeqCst);
        let left = self.failing_binds.load(Ordering::SeqCst);
        if left > 0 {
            self.failing_binds.store(left - 1, Ordering::SeqCst);
            return Err(MdnsError::Io(io::Error::from(io::ErrorKind::AddrNotAvailable)));
        }
        let socket = std::net::UdpSocket::bind((ip, 0))?;
        MdnsAnnouncer::builder().destination(self.browser).host(host).build_on(socket)
    }
}

fn identity() -> Identity {
    let facts = mw_posix_node::service::Facts { board: None, ..mw_posix_node::facts(NODE) };
    Identity::new(&facts, SENDER, "tcp/127.0.0.1:7447")
}

struct Rig {
    browser: MdnsSocket,
    net: LoopbackNet,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<Report>,
}

async fn rig(failing_binds: u32) -> Rig {
    // The browser's queries go to a socket nobody reads: the test sees
    // the announcer's unsolicited announcements and goodbyes.
    let sink = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut browser = MdnsSocket::builder()
        .bind("127.0.0.1:0".parse().unwrap())
        .destination(sink.local_addr().unwrap())
        .build()
        .await
        .unwrap();
    std::mem::forget(sink);
    browser.browse(KeyFilter::Exact(node::KIND.key())).await.unwrap();
    let net = LoopbackNet {
        ip: Arc::new(Mutex::new(Some(Ipv4Addr::LOCALHOST))),
        browser: browser.local_addr().unwrap(),
        failing_binds: Arc::new(AtomicU32::new(failing_binds)),
        binds: Arc::new(AtomicU32::new(0)),
    };
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(mdns_node::run_with(identity(), MdnsMode::Auto, net.clone(), fast(), async {
        let _ = stopped.await;
    }));
    Rig { browser, net, stop: Some(stop), task }
}

/// The next event: `Ok((instance, endpoint, txt))` for an `Up`,
/// `Err(instance)` for a `Down`.
async fn next(b: &mut MdnsSocket) -> Result<(String, Endpoint<'static>, Vec<u8>), String> {
    let mut buf = [0u8; MAX_EVENT_LEN];
    match within(b.next(&mut buf)).await.unwrap() {
        Event::Up(s) => {
            assert_eq!(s.kind, node::KIND);
            assert_eq!(s.ttl_s, node::TTL_S);
            let endpoint = match s.endpoint {
                Endpoint::Socket(a) => Endpoint::Socket(a),
                other => panic!("{other:?}"),
            };
            Ok((s.instance.to_owned(), endpoint, s.txt.to_vec()))
        }
        Event::Down { kind, instance } => {
            assert_eq!(kind, node::KIND);
            Err(instance.to_owned())
        }
    }
}

fn at(ip: Ipv4Addr) -> Endpoint<'static> {
    Endpoint::Socket(SocketAddr::V4(SocketAddrV4::new(ip, node::NO_PORT)))
}

#[tokio::test]
async fn d34_the_announcement_parses_as_a_node_record() {
    let mut r = rig(0).await;
    let (instance, endpoint, txt) = next(&mut r.browser).await.expect("up");
    assert_eq!(instance, "posix-0102030405060708");
    assert_eq!(endpoint, at(Ipv4Addr::LOCALHOST));
    assert_eq!(
        NodeTxt::parse(Txt::new(&txt).unwrap()).unwrap(),
        NodeTxt {
            id: NODE,
            chip: mw_posix_node::service::CHIP,
            board: "unknown",
            fw: Some(env!("CARGO_PKG_VERSION")),
            sender: Some(SENDER),
            zenoh: None,
        }
    );
    r.stop.take().unwrap().send(()).unwrap();
    // The second announcement may come first; then the goodbye.
    loop {
        match next(&mut r.browser).await {
            Ok((i, ..)) => assert_eq!(i, instance),
            Err(i) => break assert_eq!(i, instance),
        }
    }
    let report = within(r.task).await.unwrap();
    assert_eq!((report.announced, report.withdrawn, report.failures), (1, 1, 0));
}

#[tokio::test]
async fn d31_a_new_address_is_announced_and_the_old_one_withdrawn() {
    let mut r = rig(0).await;
    let (instance, endpoint, _) = next(&mut r.browser).await.expect("up");
    assert_eq!(endpoint, at(Ipv4Addr::LOCALHOST));
    let moved = Ipv4Addr::new(127, 0, 0, 2);
    r.net.set(Some(moved));
    // A goodbye from the old address, then the new one.
    let mut down = false;
    loop {
        match next(&mut r.browser).await {
            Ok((i, e, _)) if e == at(moved) => {
                assert_eq!(i, instance);
                break;
            }
            Ok((_, e, _)) => assert!(!down && e == at(Ipv4Addr::LOCALHOST), "{e:?}"),
            Err(i) => {
                assert_eq!(i, instance);
                down = true;
            }
        }
    }
    assert!(down, "no goodbye before the new address");
    // No address: withdrawn, and nothing announced.
    r.net.set(None);
    loop {
        if let Err(i) = next(&mut r.browser).await {
            break assert_eq!(i, instance);
        }
    }
    r.stop.take().unwrap().send(()).unwrap();
    let report = within(r.task).await.unwrap();
    assert_eq!((report.announced, report.withdrawn), (2, 2));
}

#[tokio::test]
async fn d32_stop_sends_the_goodbye_within_the_wait() {
    let mut r = rig(0).await;
    let (instance, ..) = next(&mut r.browser).await.expect("up");
    let start = tokio::time::Instant::now();
    r.stop.take().unwrap().send(()).unwrap();
    let report = within(r.task).await.unwrap();
    assert!(start.elapsed() < fast().goodbye, "{:?}", start.elapsed());
    assert_eq!(report.withdrawn, 1);
    loop {
        if let Err(i) = next(&mut r.browser).await {
            break assert_eq!(i, instance);
        }
    }
}

#[tokio::test]
async fn d32_a_failing_bind_is_retried_not_fatal() {
    let mut r = rig(2).await;
    let (instance, ..) = next(&mut r.browser).await.expect("up after the retries");
    assert_eq!(instance, "posix-0102030405060708");
    assert_eq!(r.net.binds.load(Ordering::SeqCst), 3);
    r.stop.take().unwrap().send(()).unwrap();
    let report = within(r.task).await.unwrap();
    assert_eq!((report.announced, report.failures), (1, 2));
    assert!(report.last_error.is_some_and(|e| e.contains("not available")), "the bind's error is kept");
}

#[tokio::test]
async fn d32_stop_while_nothing_is_announced_returns_at_once() {
    let mut r = rig(u32::MAX).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    r.stop.take().unwrap().send(()).unwrap();
    let report = within(r.task).await.unwrap();
    assert_eq!((report.announced, report.withdrawn), (0, 0));
    assert!(report.failures >= 2, "{report:?}");
}
