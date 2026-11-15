//! The board as a Zenoh node on Ethernet (feature `eth`, FL7): one
//! `middleware::zenoh_lite` client session over an embassy-net TCP socket
//! to the configured router or peer listener.
//!
//! [`run`] waits for the DHCP lease, connects, opens the session and
//! declares the board's management queryables (`sys/info`,
//! `sys/trust/info`, `sys/board`, `sys/fw/info`, `sys/payload/start`,
//! `sys/payload/stop`, each also as its per-node key `@mw/node/<id>/...`),
//! then serves the session: whichever comes first of
//!
//! * a query: copied into an [`Inbound`] request on [`LinkId::ZENOH`] for
//!   the controller, which gates it ([`imxrt_base::zenoh_mgmt`]: only
//!   `sys/info` and `sys/trust/info` on an unclaimed board, signed
//!   start/stop once claimed) and answers on this link; the answer goes
//!   back as the query's reply;
//! * a sample on a declared subscription: an inbound publication;
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

use embassy_futures::select::{Either, select};
use embassy_net::Stack;
use embassy_net::tcp::TcpSocket;
use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::{Duration, Instant, Timer};
use imxrt_base::config::Endpoint;
use imxrt_base::links::LinkId;
use middleware::loader::keys;
use middleware::transport::Publisher;
use middleware::zenoh_lite::{Clock, Config, Event, ZenohLite, Zid};

use super::bus::{Bus, Inbound, Kind};

/// Delay before a reconnect.
const RETRY: Duration = Duration::from_secs(2);

/// The management keys declared as queryables.
const QUERYABLES: [middleware::Key<'static>; 6] =
    [keys::INFO, keys::TRUST_INFO, keys::BOARD, middleware::fw_ota::keys::INFO, keys::START, keys::STOP];

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
static ENDPOINT: BlockingMutex<CriticalSectionRawMutex, Cell<Option<Endpoint>>> = BlockingMutex::new(Cell::new(None));

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
                if stack.is_config_up() { "bound" } else { "pending" },
                st.rx_frames, st.rx_dropped, st.tx_frames, st.bus_errors
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
        let remote = (embassy_net::Ipv4Address::new(ep.addr[0], ep.addr[1], ep.addr[2], ep.addr[3]), ep.port);
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
            declared &= client.declare_queryable(key).await.is_ok();
        }
        bus.log(format_args!(
            "zenoh: session open (sender {:02x}{:02x}{:02x}{:02x}..., queryables {})",
            client.sender_id().0[0],
            client.sender_id().0[1],
            client.sender_id().0[2],
            client.sender_id().0[3],
            if declared { "declared" } else { "partly declared" }
        ));
        let mut buf = [0u8; 1024];
        loop {
            match select(client.next_event(&mut buf), bus.next_outbound(LinkId::ZENOH)).await {
                Either::First(Ok(Event::Query(q))) => {
                    next_id = next_id.wrapping_add(1);
                    let id = next_id;
                    let result = match Inbound::new(LinkId::ZENOH, Kind::Request { id }, q.key, q.body, q.meta.sender) {
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
                Either::First(Ok(Event::Sample(s))) => {
                    if let Some(msg) = Inbound::new(LinkId::ZENOH, Kind::Publish, s.key, s.payload, s.meta.sender) {
                        bus.send_inbound(msg).await;
                    }
                }
                Either::First(Err(e)) => {
                    bus.log(format_args!("zenoh: session lost: {e:?}"));
                    break;
                }
                Either::Second(p) => {
                    if client.publish(p.key(), &p.payload).await.is_err() {
                        break;
                    }
                }
            }
        }
        Timer::after(RETRY).await;
    }
}
