//! One service kind, `demo/echo`, found three ways and called two ways, all
//! in this process over loopback.
//!
//! * **Zenoh**: two responder sessions (`zenoh-a`, `zenoh-b`) connected to
//!   a hub session over one explicit `tcp/127.0.0.1` endpoint (multicast
//!   scouting and gossip off, see [`SessionConfig`]). Each serves
//!   `demo/echo` with a [`ZenohResponder`] and announces itself with
//!   [`ZenohDiscovery`]; the hub browses.
//! * **mDNS**: an [`MdnsAnnouncer`] announces a gRPC server (`lan-1`) to an
//!   [`MdnsSocket`] browser. Both are bound on 127.0.0.1 and aimed at each
//!   other with an explicit destination, so nothing joins 224.0.0.251 or
//!   multicasts onto the LAN (as in `tests/mdns.rs`).
//! * **Cluster DNS**: a stub DNS server on 127.0.0.1:0 (below) serves the
//!   SRV record Kubernetes would publish for a headless Service, with two
//!   pods (`pod-a`, `pod-b`) whose A glue points at two more gRPC servers.
//!   [`UnicastDnsSd`] resolves it and [`MeshBrowse`] reports a pod `Up`
//!   only while its `grpc.health.v1.Health` says `SERVING` (direct dialer,
//!   no sidecar).
//!
//! Every gRPC server binds 127.0.0.1:0 and mounts the health service
//! (`GrpcServer::with_health`, `SERVING` through its [`GrpcAnnouncer`]) and
//! a responder for `demo/echo`.
//!
//! Every `Up` and `Down` goes into one [`Directory`] and is printed with
//! what the directory did. Then:
//!
//! * Zenoh `request_with` with a timeout and `Target::Node` (the node id
//!   the Zenoh browser reported for `zenoh-b`), then `Target::Any`;
//! * Zenoh [`Gather`] across both Zenoh responders;
//! * a gRPC [`MeshClient`] over the same directory (it calls the socket
//!   endpoints and skips the Zenoh ones): `request_with`, `Gather` across
//!   the three gRPC servers, and `Target::Node`, which is `UNAVAILABLE`
//!   because neither DNS-SD backend reports node ids;
//! * then `zenoh-b`, `lan-1` and `pod-b` go away (a Zenoh withdraw, an mDNS
//!   goodbye, a health `NOT_SERVING`) and the `Down` events empty their
//!   directory slots.
//!
//! Each handler answers with its name, the caller id and the deadline it
//! saw, so the output shows the caller identity and timeout each backend
//! carries. No principal is printed: there is no sidecar to authenticate
//! one.
//!
//! Run with
//! `cargo run --example discover_and_call --features zenoh,grpc,istio,dns-sd,mdns`.
//! It finishes in a few seconds and exits 0; `cargo test --all-features`
//! runs it as the test `discover_and_call_finds_and_calls_every_responder`.

use std::fmt::Debug;
use std::net::{Ipv4Addr, SocketAddr};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use middleware::discovery::{
    Announce, Browse, Change, Directory, Endpoint, Event, Service, ServiceKind,
};
use middleware::dns::unicast::{ServiceMap, ServiceRef, UnicastDnsSd};
use middleware::dns::wire::{
    FLAG_AA, FLAG_QR, FLAG_RA, FLAG_RD, Message, MessageWriter, RecordData, RecordOut, Section,
    TYPE_A, TYPE_SRV, TextName,
};
use middleware::grpc::mesh::{MeshBrowse, MeshClient, MeshDialer};
use middleware::grpc::server::serve;
use middleware::grpc::{GrpcAnnouncer, GrpcServer, health, responder};
use middleware::mdns::announce::MdnsAnnouncer;
use middleware::mdns::socket::MdnsSocket;
use middleware::transport::handler_fn;
use middleware::zenoh::{SessionConfig, ZenohDiscovery, ZenohTransport};
use middleware::{
    Flow, Gather, Handler, HandlerError, Key, KeyFilter, Request, RequestOptions, Requester,
    Responder, SenderId, Target,
};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::oneshot;

/// The service kind every backend announces.
const KIND: ServiceKind<'static> = ServiceKind::from_static("demo/echo");
/// The request key; two segments, so gRPC calls it as `/demo/echo`.
const KEY: Key<'static> = Key::from_static("demo/echo");
/// The SRV name Kubernetes publishes for port `grpc` of Service `echo` in
/// namespace `demo` ([`ServiceMap::srv_name`]).
const SRV_NAME: &str = "_grpc._tcp.echo.demo.svc.cluster.local";
/// The pods' DNS suffix.
const PODS: &str = "echo.demo.svc.cluster.local";
/// TTL of every record, seconds.
const TTL_S: u32 = 60;
/// Bound on each call and gather.
const CALL: RequestOptions = RequestOptions::DEFAULT.with_timeout(Duration::from_secs(1));
/// Bound on waiting for one discovery event.
const PROMPT: Duration = Duration::from_secs(5);
/// Directory slots: five services fit with room to spare.
const SLOTS: usize = 16;

/// What [`run`] saw, for the test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    /// `Up` events stored in the directory.
    pub ups: usize,
    /// `Down` events that removed a record.
    pub downs: usize,
    /// Whether the `Target::Node` request was answered by that node.
    pub node_answered: bool,
    /// Replies to the Zenoh gather.
    pub zenoh_replies: usize,
    /// Replies to the gRPC mesh gather.
    pub grpc_replies: usize,
    /// Whether gRPC `Target::Node` failed with `Unavailable`.
    pub grpc_node_unavailable: bool,
    /// Records left at the end.
    pub left: usize,
}

/// Maps any error to a message naming the step that failed.
fn fail<E: Debug>(what: &'static str) -> impl FnOnce(E) -> String {
    move |e| format!("{what}: {e:?}")
}

/// A node id as 16 hex digits, or `-`.
fn node(id: Option<SenderId>) -> String {
    id.map_or_else(
        || "-".to_owned(),
        |SenderId(b)| b.iter().map(|x| format!("{x:02x}")).collect(),
    )
}

fn endpoint(e: &Endpoint<'_>) -> String {
    match e {
        Endpoint::Socket(a) => a.to_string(),
        Endpoint::Host { name, port } => format!("{name}:{port}"),
        Endpoint::Locator(l) => (*l).to_owned(),
        Endpoint::Local => "local".to_owned(),
    }
}

/// The handler every responder runs: its name, the body, and the caller
/// identity and deadline it was given.
fn echo(name: &'static str) -> impl Handler + Send + 'static {
    handler_fn(move |r: &Request<'_>, out: &mut [u8]| {
        let text = format!(
            "{name} got {:?} from caller {}, deadline {} ms, principal {}",
            String::from_utf8_lossy(r.body),
            node(r.meta.sender),
            r.meta.deadline_ms.map_or("-".to_owned(), |d| d.to_string()),
            r.meta.principal.unwrap_or("-"),
        );
        let dst = out
            .get_mut(..text.len())
            .ok_or(HandlerError::BufferTooSmall)?;
        dst.copy_from_slice(text.as_bytes());
        Ok(text.len())
    })
}

/// A service record of [`KIND`].
fn service(instance: &'static str, endpoint: Endpoint<'static>) -> Service<'static> {
    Service {
        kind: KIND,
        instance,
        node: None,
        endpoint,
        txt: b"",
        ttl_s: TTL_S,
    }
}

/// The directory and the clock it is fed with.
struct Seen {
    directory: Directory<SLOTS>,
    start: Instant,
    ups: usize,
    downs: usize,
}

impl Seen {
    /// Applies `event` from `source`, prints it, and returns the new state.
    fn feed(self, source: &str, event: &Event<'_>) -> Seen {
        let now_ms = u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX);
        let (directory, change) = self.directory.observe(event, now_ms);
        let what = match event {
            Event::Up(s) => format!(
                "up   {} {:<7} node {:<16} at {} ttl {} s",
                s.kind,
                s.instance,
                node(s.node),
                endpoint(&s.endpoint),
                s.ttl_s
            ),
            Event::Down { kind, instance } => format!("down {kind} {instance:<7}"),
        };
        println!(
            "[{:>5} ms] {source:<5} {what} -> {change:?} ({} known)",
            now_ms,
            directory.len()
        );
        Seen {
            ups: self.ups + usize::from(change == Change::Added),
            downs: self.downs + usize::from(change == Change::Removed),
            directory,
            ..self
        }
    }
}

/// Waits for `n` events from `browser` and feeds them to `seen`.
async fn collect<B: Browse>(
    source: &str,
    browser: &mut B,
    n: usize,
    mut seen: Seen,
) -> Result<Seen, String> {
    let mut buf = [0u8; 512];
    for _ in 0..n {
        let event = tokio::time::timeout(PROMPT, browser.next(&mut buf))
            .await
            .map_err(fail("waiting for a discovery event"))?
            .map_err(fail("browse"))?;
        seen = seen.feed(source, &event);
    }
    Ok(seen)
}

/// A free loopback TCP port: bound on port 0 and released, never probed.
fn free_port() -> Result<u16, String> {
    let l = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(fail("bind"))?;
    Ok(l.local_addr().map_err(fail("local addr"))?.port())
}

/// A gRPC server on 127.0.0.1:0 with health and a `demo/echo` responder.
/// Dropping `_stop` shuts it down.
struct GrpcNode {
    addr: SocketAddr,
    health: GrpcAnnouncer,
    _stop: oneshot::Sender<()>,
}

async fn grpc_node(name: &'static str) -> Result<GrpcNode, String> {
    let (health_svc, health) = health();
    let (responder_svc, mut resp) = responder(8);
    tokio::spawn(async move {
        let mut handler = echo(name);
        let mut scratch = [0u8; 256];
        while resp.serve_one(&mut handler, &mut scratch).await.is_ok() {}
    });
    let server = GrpcServer::new()
        .with_health(health_svc)
        .with_responder(responder_svc);
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .map_err(fail("bind gRPC"))?;
    let addr = listener.local_addr().map_err(fail("gRPC addr"))?;
    let (stop, stopped) = oneshot::channel::<()>();
    tokio::spawn(serve(server, listener, async {
        let _ = stopped.await;
    }));
    Ok(GrpcNode {
        addr,
        health,
        _stop: stop,
    })
}

/// A Zenoh session connected to `endpoint`, serving `demo/echo` as `name`.
async fn zenoh_node(endpoint: &str, name: &'static str) -> Result<ZenohTransport, String> {
    let config = SessionConfig::new()
        .connect(endpoint)
        .build()
        .map_err(fail("zenoh config"))?;
    let session = ZenohTransport::open(config)
        .await
        .map_err(fail("zenoh open"))?;
    let mut r = session
        .responder(KEY)
        .await
        .map_err(fail("zenoh responder"))?;
    tokio::spawn(async move {
        let mut handler = echo(name);
        let mut scratch = [0u8; 256];
        while r.serve_one(&mut handler, &mut scratch).await.is_ok() {}
    });
    Ok(session)
}

/// The reply to one query to the stub DNS server: the SRV record of
/// [`SRV_NAME`] with A glue for each pod, or the A record of a pod, or
/// NXDOMAIN. Pure.
fn dns_reply(query: &[u8], pods: &[(String, u16)], out: &mut [u8]) -> Option<usize> {
    let message = Message::parse(query).ok()?;
    let question = message.questions().next()?;
    let mut text = [0u8; 256];
    let qname = question.name.write_text(&mut text).ok()?;
    let mut answers = Vec::new();
    let mut additional = Vec::new();
    for (target_text, port) in pods {
        let target = TextName::new(target_text).ok()?;
        let a = RecordOut {
            name: target,
            ttl: TTL_S,
            cache_flush: false,
            data: RecordData::A(Ipv4Addr::LOCALHOST),
        };
        match question.qtype {
            TYPE_SRV if qname.eq_ignore_ascii_case(SRV_NAME) => {
                answers.push(RecordOut {
                    name: TextName::new(SRV_NAME).ok()?,
                    ttl: TTL_S,
                    cache_flush: false,
                    data: RecordData::Srv {
                        priority: 0,
                        weight: 100,
                        port: *port,
                        target,
                    },
                });
                additional.push(a);
            }
            TYPE_A if qname.eq_ignore_ascii_case(target_text) => answers.push(a),
            _ => {}
        }
    }
    let nxdomain = if answers.is_empty() { 3 } else { 0 };
    let flags = FLAG_QR | FLAG_AA | FLAG_RD | FLAG_RA | nxdomain;
    let mut w = MessageWriter::new(out, message.id(), flags)
        .ok()?
        .question(TextName::new(qname).ok()?, question.qtype, false)
        .ok()?;
    for r in &answers {
        w = w.record(Section::Answer, r).ok()?;
    }
    for r in &additional {
        w = w.record(Section::Additional, r).ok()?;
    }
    Some(w.finish())
}

/// Starts the stub DNS server on 127.0.0.1:0 for `pods` (target, port);
/// it answers until the runtime stops.
async fn stub_dns(pods: Vec<(String, u16)>) -> Result<SocketAddr, String> {
    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .map_err(fail("bind DNS stub"))?;
    let addr = socket.local_addr().map_err(fail("DNS stub addr"))?;
    tokio::spawn(async move {
        let mut query = [0u8; 1500];
        let mut out = [0u8; 1500];
        while let Ok((len, from)) = socket.recv_from(&mut query).await {
            let reply = query
                .get(..len)
                .and_then(|q| dns_reply(q, &pods, &mut out));
            if let Some(n) = reply {
                let _ = socket.send_to(out.get(..n).unwrap_or_default(), from).await;
            }
        }
    });
    Ok(addr)
}

/// Discovers, calls and withdraws; see the module docs.
pub async fn run() -> Result<Summary, String> {
    let mut seen = Seen {
        directory: Directory::new(),
        start: Instant::now(),
        ups: 0,
        downs: 0,
    };

    // gRPC servers: two cluster pods and one LAN device.
    let mut pod_a = grpc_node("pod-a").await?;
    let mut pod_b = grpc_node("pod-b").await?;
    let mut lan = grpc_node("lan-1").await?;

    // Zenoh: a hub and two responder sessions on one loopback endpoint.
    let hub_endpoint = format!("tcp/127.0.0.1:{}", free_port()?);
    let hub_config = SessionConfig::new()
        .listen(hub_endpoint.clone())
        .build()
        .map_err(fail("zenoh config"))?;
    let mut hub = ZenohTransport::open(hub_config)
        .await
        .map_err(fail("zenoh open"))?;
    let zenoh_a = zenoh_node(&hub_endpoint, "zenoh-a").await?;
    let zenoh_b = zenoh_node(&hub_endpoint, "zenoh-b").await?;
    let (mut announce_a, mut announce_b) = (zenoh_a.discovery(), zenoh_b.discovery());
    let mut zenoh_browser: ZenohDiscovery = hub.discovery();

    // mDNS on loopback unicast only.
    let announcer_socket = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
        .map_err(fail("bind mDNS announcer"))?;
    let mut mdns_browser = MdnsSocket::builder()
        .bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .destination(announcer_socket.local_addr().map_err(fail("addr"))?)
        .build()
        .await
        .map_err(fail("mDNS browser"))?;
    let mut mdns_announcer = MdnsAnnouncer::builder()
        .destination(mdns_browser.local_addr().map_err(fail("addr"))?)
        .host("lan-1.local")
        .build_on(announcer_socket)
        .map_err(fail("mDNS announcer"))?;

    // Cluster DNS: the headless Service's SRV record lists both pods.
    let nameserver = stub_dns(vec![
        (format!("pod-a.{PODS}"), pod_a.addr.port()),
        (format!("pod-b.{PODS}"), pod_b.addr.port()),
    ])
    .await?;
    let map: ServiceMap = ServiceMap::new()
        .with(
            KIND,
            ServiceRef {
                service: "echo",
                namespace: "demo",
                port_name: "grpc",
            },
        )
        .map_err(fail("service map"))?;
    let dns = UnicastDnsSd::builder(map)
        .nameserver(nameserver)
        .bind(Ipv4Addr::LOCALHOST.into())
        .timeout(Duration::from_millis(500))
        .build()
        .await
        .map_err(fail("unicast DNS-SD"))?;
    let mut cluster: MeshBrowse<_, 4> = MeshBrowse::new(dns, MeshDialer::direct());

    // Announce everywhere at once (mDNS probing alone takes ~750 ms).
    let exact = KeyFilter::Exact(KIND.key());
    zenoh_browser.browse(exact).await.map_err(fail("zenoh browse"))?;
    mdns_browser.browse(exact).await.map_err(fail("mDNS browse"))?;
    cluster.browse(exact).await.map_err(fail("DNS-SD browse"))?;
    let records = [
        service("zenoh-a", Endpoint::Local),
        service("zenoh-b", Endpoint::Local),
        service("lan-1", Endpoint::Socket(lan.addr)),
        // gRPC health ignores the endpoint: only readiness is announced.
        service("pod-a", Endpoint::Local),
        service("pod-b", Endpoint::Local),
    ];
    let [zenoh_a_rec, zenoh_b_rec, lan_rec, pod_a_rec, pod_b_rec] = &records;
    let (za, zb, md, ha, hb, hl) = tokio::join!(
        announce_a.announce(zenoh_a_rec),
        announce_b.announce(zenoh_b_rec),
        mdns_announcer.announce(lan_rec),
        pod_a.health.announce(pod_a_rec),
        pod_b.health.announce(pod_b_rec),
        lan.health.announce(lan_rec),
    );
    za.map_err(fail("zenoh announce"))?;
    zb.map_err(fail("zenoh announce"))?;
    md.map_err(fail("mDNS announce"))?;
    ha.and(hb).and(hl).map_err(fail("health announce"))?;

    println!("-- discovery");
    seen = collect("zenoh", &mut zenoh_browser, 2, seen).await?;
    seen = collect("mdns", &mut mdns_browser, 1, seen).await?;
    seen = collect("dns", &mut cluster, 2, seen).await?;

    // Zenoh calls: by node, then any, then gather.
    println!("-- zenoh request_with (timeout 1 s)");
    let mut buf = [0u8; 256];
    let b_node = seen
        .directory
        .get(KIND, "zenoh-b")
        .and_then(|s| s.node)
        .ok_or("zenoh-b has no node id")?;
    let reply = hub
        .request_with(KEY, b"hi b", &mut buf, &CALL.with_target(Target::Node(b_node)))
        .await
        .map_err(fail("zenoh request to node"))?;
    let reply = String::from_utf8_lossy(reply).into_owned();
    println!("Target::Node({}) -> {reply}", node(Some(b_node)));
    let node_answered = reply.starts_with("zenoh-b ");
    let reply = hub
        .request_with(KEY, b"hi anyone", &mut buf, &CALL)
        .await
        .map_err(fail("zenoh request"))?;
    println!("Target::Any -> {}", String::from_utf8_lossy(reply));

    println!("-- zenoh gather");
    let zenoh_replies = hub
        .request_each(KEY, b"hi all", &mut buf, &CALL, 8, |r| {
            print_reply(&r);
            Flow::Continue
        })
        .await
        .map_err(fail("zenoh gather"))?;

    // gRPC calls over the same directory.
    println!("-- grpc mesh (direct, over the directory)");
    let mut mesh =
        MeshClient::<SLOTS>::new(KIND, MeshDialer::direct()).with_directory(seen.directory.clone());
    println!("mesh client id {}", node(Some(mesh.client_id())));
    let reply = mesh
        .request_with(KEY, b"hi grpc", &mut buf, &CALL)
        .await
        .map_err(fail("grpc request"))?;
    println!("Target::Any -> {}", String::from_utf8_lossy(reply));
    let grpc_replies = mesh
        .request_each(KEY, b"hi all grpc", &mut buf, &CALL, 8, |r| {
            print_reply(&r);
            Flow::Continue
        })
        .await
        .map_err(fail("grpc gather"))?;
    let by_node = mesh
        .request_with(KEY, b"hi", &mut buf, &CALL.with_target(Target::Node(b_node)))
        .await;
    let grpc_node_unavailable = matches!(
        &by_node,
        Err(e) if e.handler_error() == Some(HandlerError::Unavailable)
    );
    println!(
        "Target::Node({}) -> {:?} (DNS-SD records carry no node id)",
        node(Some(b_node)),
        by_node.map(|r| String::from_utf8_lossy(r).into_owned())
    );

    // Going away: a Zenoh withdraw, an mDNS goodbye, a health NOT_SERVING.
    println!("-- withdraw");
    announce_b
        .withdraw(KIND, "zenoh-b")
        .await
        .map_err(fail("zenoh withdraw"))?;
    seen = collect("zenoh", &mut zenoh_browser, 1, seen).await?;
    mdns_announcer
        .withdraw(KIND, "lan-1")
        .await
        .map_err(fail("mDNS withdraw"))?;
    seen = collect("mdns", &mut mdns_browser, 1, seen).await?;
    pod_b
        .health
        .withdraw(KIND, "pod-b")
        .await
        .map_err(fail("health withdraw"))?;
    seen = collect("dns", &mut cluster, 1, seen).await?;

    let left: Vec<&str> = seen.directory.lookup(KIND).map(|s| s.instance).collect();
    println!("-- left in the directory: {left:?}");
    Ok(Summary {
        ups: seen.ups,
        downs: seen.downs,
        node_answered,
        zenoh_replies,
        grpc_replies,
        grpc_node_unavailable,
        left: left.len(),
    })
}

fn print_reply(r: &middleware::Reply<'_>) {
    match r.result {
        Ok(body) => println!(
            "reply from {} -> {}",
            node(r.sender),
            String::from_utf8_lossy(body)
        ),
        Err(e) => println!("reply from {} -> error {e:?}", node(r.sender)),
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(summary) => {
            println!("-- {summary:?}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("discover_and_call: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs [`run`] as `main` does, on a thread with the main thread's
    /// 8 MiB stack: unoptimised, polling every backend's futures in one
    /// task needs a little more than a 2 MiB test thread has.
    #[test]
    fn discover_and_call_finds_and_calls_every_responder() {
        let thread = std::thread::Builder::new()
            .stack_size(8 << 20)
            .spawn(|| {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .map_err(fail("runtime"))?;
                rt.block_on(async {
                    tokio::time::timeout(Duration::from_secs(30), run())
                        .await
                        .map_err(fail("run"))?
                })
            })
            .expect("thread");
        let s = thread.join().expect("no panic").expect("runs");
        assert_eq!(
            s,
            Summary {
                ups: 5,
                downs: 3,
                node_answered: true,
                zenoh_replies: 2,
                grpc_replies: 3,
                grpc_node_unavailable: true,
                left: 2,
            }
        );
    }
}
