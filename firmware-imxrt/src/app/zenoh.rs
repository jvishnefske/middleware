//! The board as a Zenoh node on Ethernet (feature `eth`, FL7): one
//! `middleware::zenoh_lite` client session over an embassy-net TCP socket
//! to the configured router or peer listener.
//!
//! [`run`] waits for the DHCP lease, connects, opens the session and
//! declares the board's management queryables: the broadcast ones
//! (`sys/info`, `sys/trust/info`, `sys/board`, `sys/fw/info`,
//! `sys/payload/start`, `sys/payload/stop`) and one per-node queryable for
//! every system key, `@mw/node/<id>/sys/**` (ZN5: uploads, firmware
//! updates and their signed controls address one board), seven
//! declarations in all, which leaves the table room for every
//! subscription a payload may have; then it serves the session:
//! whichever comes first of
//!
//! * a query: copied into an [`Inbound`] request on [`LinkId::ZENOH`] for
//!   the controller, which gates it ([`imxrt_base::zenoh_mgmt`]: only
//!   `sys/info` and `sys/trust/info` on an unclaimed board; once claimed,
//!   read-only keys, staged uploads and firmware transfers whose commits
//!   are signed, and signed start/stop/fw reboot/confirm/abort) and
//!   answers on this link; the answer goes back as the query's reply;
//! * a sample on a declared subscription: an inbound publication (the
//!   session declares a subscriber for each of the running payload's
//!   subscription keys, [`Bus::subscriptions`], and follows their changes;
//!   a new session declares them again);
//! * a publication the bus queued for this link: a Zenoh Put with the
//!   middleware attachment (`Publisher::publish`).
//!
//! The endpoint is `sys/zenoh/endpoint` (persisted by the service; set it
//! over a serial link with `mwctl request sys/zenoh/endpoint tcp/A.B.C.D:P`).
//! A lost session or a failed connect is retried after [`RETRY`]; the
//! session's own keep-alives run inside `next_event`.
//!
//! The Zenoh id is the chip's unique id followed by `"RT1050\0\x01"`, so
//! the board's sender id (FNV-1a of the id, as the host computes it) is
//! the same on every boot.

use core::cell::Cell;

use embassy_futures::select::{Either4, select4};
use embassy_net::Stack;
use embassy_net::tcp::TcpSocket;
use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::{Duration, Instant, Timer};
use imxrt_base::config::Endpoint;
use imxrt_base::links::LinkId;
use imxrt_base::zenoh_mgmt::{SubSet, subscription_diff};
use middleware::key::Filter;
use middleware::loader::keys;
use middleware::transport::{HandlerError, Publisher, RequestOptions, Requester, Target};
use middleware::zenoh_lite::{Clock, Config, Event, LiteError, ZenohLite, Zid};

use super::bus::{Bus, Forward, Inbound, Kind, Reply};

/// Delay before a reconnect.
const RETRY: Duration = Duration::from_secs(2);

/// The management keys declared as broadcast queryables.
const QUERYABLES: [middleware::Key<'static>; 6] = [
    keys::INFO,
    keys::TRUST_INFO,
    keys::BOARD,
    middleware::fw_ota::keys::INFO,
    keys::START,
    keys::STOP,
];

/// embassy-time as the client's clock.
pub struct EmbassyClock;

impl Clock for EmbassyClock {
    fn now_ms(&mut self) -> u64 {
        Instant::now().as_millis()
    }

    async fn sleep_until_ms(&mut self, deadline_ms: u64) {
        Timer::at(Instant::from_millis(deadline_ms)).await;
    }
}

/// The board's Zenoh id.
pub fn zid(uid: [u8; 8]) -> Option<Zid> {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&uid);
    b[8..].copy_from_slice(b"RT1050\0\x01");
    Zid::new(b)
}

/// The endpoint the next connect uses: the service's
/// `sys/zenoh/endpoint`, kept current by the controller ([`set_endpoint`]).
static ENDPOINT: BlockingMutex<CriticalSectionRawMutex, Cell<Option<Endpoint>>> =
    BlockingMutex::new(Cell::new(None));

/// Sets the endpoint the next connect uses.
pub fn set_endpoint(e: Option<Endpoint>) {
    ENDPOINT.lock(|c| c.set(e));
}

fn endpoint() -> Option<Endpoint> {
    ENDPOINT.lock(Cell::get)
}

/// Serves the Zenoh link forever (see the module docs). `endpoint` is
/// read again before every connect.
pub async fn run(stack: Stack<'static>, bus: &'static Bus, uid: [u8; 8]) -> ! {
    let Some(zid) = zid(uid) else {
        bus.log(format_args!("zenoh: no Zenoh id"));
        loop {
            Timer::after_secs(3600).await;
        }
    };
    // Link and lease, reported every few seconds until both are there.
    let mut waited = 0u32;
    while !(stack.is_link_up() && stack.is_config_up()) {
        if waited.is_multiple_of(5) {
            let st = super::enet::stats();
            bus.log(format_args!(
                "eth: link {}, DHCP {} (rx {} dropped {} tx {} bus errors {})",
                if stack.is_link_up() { "up" } else { "down" },
                if stack.is_config_up() {
                    "bound"
                } else {
                    "pending"
                },
                st.rx_frames,
                st.rx_dropped,
                st.tx_frames,
                st.bus_errors
            ));
        }
        waited = waited.wrapping_add(1);
        Timer::after_secs(1).await;
    }
    if let Some(c) = stack.config_v4() {
        bus.log(format_args!("eth: DHCP lease {}", c.address));
    }
    // The send buffer covers about 0.9 s of a 16 kHz PCM route's batches
    // (SC8) beyond the bus queue, for a peer whose ACKs stall (a Wi-Fi
    // host); the task's future, and with it both buffers, is in SDRAM.
    let mut rx = [0u8; 2048];
    let mut tx = [0u8; 32 * 1024];
    let mut next_id: u8 = 0;
    loop {
        let Some(ep) = endpoint() else {
            Timer::after(RETRY).await;
            continue;
        };
        let mut sock = TcpSocket::new(stack, &mut rx, &mut tx);
        sock.set_timeout(Some(Duration::from_secs(30)));
        let remote = (
            embassy_net::Ipv4Address::new(ep.addr[0], ep.addr[1], ep.addr[2], ep.addr[3]),
            ep.port,
        );
        if let Err(e) = sock.connect(remote).await {
            let st = super::enet::stats();
            bus.log(format_args!(
                "zenoh: connect to {ep} failed: {e:?} (eth rx {} dropped {} tx {} bus errors {})",
                st.rx_frames, st.rx_dropped, st.tx_frames, st.bus_errors
            ));
            Timer::after(RETRY).await;
            continue;
        }
        let mut client: ZenohLite<_, _> = ZenohLite::new(sock, EmbassyClock, Config::new(zid));
        if let Err(e) = client.open().await {
            bus.log(format_args!("zenoh: open failed: {e:?}"));
            Timer::after(RETRY).await;
            continue;
        }
        let mut declared = true;
        for key in QUERYABLES {
            declared &= client.declare_plain_queryable(key).await.is_ok();
        }
        declared &= client
            .declare_node_prefix_queryable(middleware::Key::from_static("sys"))
            .await
            .is_ok();
        bus.log(format_args!(
            "zenoh: session open (sender {:02x}{:02x}{:02x}{:02x}..., queryables {})",
            client.sender_id().0[0],
            client.sender_id().0[1],
            client.sender_id().0[2],
            client.sender_id().0[3],
            if declared {
                "declared"
            } else {
                "partly declared"
            }
        ));
        let mut declared = SubSet::new();
        if follow_subscriptions(&mut client, bus, &mut declared)
            .await
            .is_err()
        {
            Timer::after(RETRY).await;
            continue;
        }
        let mut buf = [0u8; 1024];
        bus.set_zenoh_open(true);
        loop {
            match select4(
                client.next_event(&mut buf),
                bus.next_outbound(LinkId::ZENOH),
                bus.subscriptions_changed(),
                bus.next_forward(),
            )
            .await
            {
                Either4::Fourth(f) => {
                    if !forward(&mut client, bus, f).await {
                        break;
                    }
                }
                Either4::Third(()) => {
                    if follow_subscriptions(&mut client, bus, &mut declared)
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Either4::First(Ok(Event::Query(q))) => {
                    next_id = next_id.wrapping_add(1);
                    let id = next_id;
                    let result = match Inbound::new(
                        LinkId::ZENOH,
                        Kind::Request { id },
                        q.key,
                        q.body,
                        q.meta.sender,
                    ) {
                        Some(msg) => {
                            bus.send_inbound(msg).await;
                            bus.wait_reply(LinkId::ZENOH, id).await.result
                        }
                        None => Err(middleware::transport::HandlerError::BufferTooSmall),
                    };
                    let body = result.as_ref().map(|v| v.as_slice()).map_err(|e| *e);
                    let sent = client.reply(&q, body).await;
                    bus.reply_sent(LinkId::ZENOH, id);
                    if sent.is_err() {
                        break;
                    }
                }
                Either4::First(Ok(Event::Sample(s))) => {
                    if let Some(node) = s.meta.sender {
                        bus.heard(node);
                    }
                    if let Some(msg) = Inbound::new(
                        LinkId::ZENOH,
                        Kind::Publish,
                        s.key,
                        s.payload,
                        s.meta.sender,
                    ) {
                        bus.send_inbound(msg).await;
                    }
                }
                Either4::First(Err(e)) => {
                    bus.log(format_args!("zenoh: session lost: {e:?}"));
                    break;
                }
                Either4::Second(p) => {
                    if client.publish(p.key(), &p.payload).await.is_err() {
                        break;
                    }
                }
            }
        }
        bus.set_zenoh_open(false);
        Timer::after(RETRY).await;
    }
}

/// How long a forwarded request waits for the node's reply.
const FORWARD_TIMEOUT: core::time::Duration = core::time::Duration::from_secs(2);

/// Asks `f.node` for `f.key` (PR1) and replies on `f.link`: the node's
/// reply, its error, or `Unavailable` when it did not answer. `false` when
/// the session failed meanwhile.
async fn forward<S, C>(client: &mut ZenohLite<S, C>, bus: &Bus, f: Forward) -> bool
where
    S: embedded_io_async::Read + embedded_io_async::Write,
    C: Clock,
{
    let opts = RequestOptions::DEFAULT
        .with_timeout(FORWARD_TIMEOUT)
        .with_target(Target::Node(f.node));
    let mut out = [0u8; 512];
    let (result, alive) = match client.request_with(f.key, &[], &mut out, &opts).await {
        Ok(r) => {
            bus.heard(f.node);
            (
                heapless::Vec::from_slice(r).map_err(|_| HandlerError::BufferTooSmall),
                true,
            )
        }
        Err(LiteError::Handler(e)) => {
            bus.heard(f.node);
            (Err(e), true)
        }
        Err(LiteError::NoReply | LiteError::BufferTooSmall { .. } | LiteError::InvalidKey) => {
            (Err(HandlerError::Unavailable), true)
        }
        Err(_) => (Err(HandlerError::Unavailable), false),
    };
    bus.reply(Reply {
        link: f.link,
        id: f.id,
        result,
    });
    alive
}

/// Declares a subscriber for each of the payload's subscription keys not
/// yet `declared` and undeclares those it dropped; `declared` follows.
/// An error is a lost session.
async fn follow_subscriptions<S, C>(
    client: &mut ZenohLite<S, C>,
    bus: &Bus,
    declared: &mut SubSet,
) -> Result<(), ()>
where
    S: embedded_io_async::Read + embedded_io_async::Write,
    C: Clock,
{
    let want = bus.subscriptions();
    let (gone, new) = subscription_diff(declared, &want);
    for k in &gone {
        let Ok(key) = middleware::Key::new(k) else {
            continue;
        };
        client
            .undeclare_subscriber(Filter::Exact(key))
            .await
            .map_err(|_| ())?;
    }
    for k in &new {
        let Ok(key) = middleware::Key::new(k) else {
            continue;
        };
        client
            .declare_subscriber(Filter::Exact(key))
            .await
            .map_err(|_| ())?;
    }
    if !gone.is_empty() || !new.is_empty() {
        bus.log(format_args!(
            "zenoh: payload subscriptions: {} declared, {} undeclared",
            new.len(),
            gone.len()
        ));
    }
    *declared = want;
    Ok(())
}
