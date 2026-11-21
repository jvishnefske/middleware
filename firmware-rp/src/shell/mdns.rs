//! The board's mDNS responder on its network link (`net`, D52/D53): it
//! announces the board as a `mw/node` ([`rp_base::mdns_node`]), instance
//! `rp-<id>` on host `rp-<id>.local`, so `avahi-browse -r _mw-node._tcp`
//! and mwctl find it.
//!
//! [`run`] binds the one UDP socket on port 5353, waits for the link and an
//! address, joins 224.0.0.251 and then, once a second ([`CHECK`]),
//! compares what the link offers ([`rp_base::mdns_node::wanted`]: the
//! address while the link is up) with what it announced ([`plan`]):
//!
//! * a new address or prefix (a DHCP lease, a renumbering): the on-link
//!   subnet follows it and the record is announced (probing first the
//!   first time);
//! * the link went down or the lease is lost: the record is withdrawn
//!   (its goodbye may not go out), and announced afresh, probing again,
//!   when it comes back;
//!
//! and in between serves queries (`MdnsAnnouncer::step`, dropped at each
//! check, which is cancel safe). A failure backs off [`BACKOFF`]
//! ([`CONFLICT_BACKOFF`] when every rename conflicted); errors are logged at
//! the 1st, 2nd, 4th, 8th... occurrence. This is the RT1050's loop
//! (firmware-imxrt `app::mdns`) with the link gate; both use the shared
//! `middleware::mdns::responder` decisions.
//!
//! Its buffers (the socket's 4 KiB receive and 2 KiB transmit buffers, the
//! announcer's two 1500-byte buffers and table) live in this task's
//! future, so in its task pool, a static.

use core::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use embassy_futures::select::{Either, select};
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{Ipv4Cidr, Stack};
use embassy_time::{Duration, Timer};
use middleware::discovery::Announce;
use middleware::mdns::announce::{AnnounceError, AnnounceTable, AnnouncerParams, MdnsAnnouncer};
use middleware::mdns::embassy_seam::{EmbassyClock, EmbassyError, EmbassyUdp, join_group, subnet};
use middleware::mdns::node::KIND;
use middleware::mdns::origin::{OnLink, OriginPolicy};
use middleware::mdns::socket::MdnsError;
use middleware::mdns::{MCU_BUF, MDNS_GROUP_V4, MDNS_PORT};
use rp_base::mdns_node::{Action, Board, Desired, identity, plan, service, txt, wanted};
use rp_base::node::node_id;

use super::board::FIRMWARE;
use super::bus::Bus;

/// How often the link and the address are compared with the record.
pub const CHECK: Duration = Duration::from_secs(1);
/// The wait after a failed announcement or step.
pub const BACKOFF: Duration = Duration::from_secs(2);
/// The wait after every rename of the record conflicted.
pub const CONFLICT_BACKOFF: Duration = Duration::from_secs(30);

/// The board's announcer: the embassy-net socket, embassy-time, Ethernet
/// MTU buffers.
type Announcer<'a> = MdnsAnnouncer<EmbassyUdp<'a>, EmbassyClock, MCU_BUF>;

/// Bytes of an [`Announcer`] (its buffers and table), logged at start.
pub const ANNOUNCER_BYTES: usize = core::mem::size_of::<Announcer<'static>>();

// The RAM plan (rp_base::chip) counts on an announcer of at most 8 KiB.
const _: () = assert!(ANNOUNCER_BYTES <= 8 * 1024);

#[embassy_executor::task]
pub async fn mdns_task(stack: Stack<'static>, bus: &'static Bus, uid: [u8; 8], board: Board) {
    run(stack, bus, uid, board).await;
}

/// Logs `args` on the 1st, 2nd, 4th, 8th... call with the same `count`.
fn log_rarely(bus: &Bus, count: &mut u32, args: core::fmt::Arguments<'_>) {
    *count = count.saturating_add(1);
    if count.is_power_of_two() {
        bus.log(format_args!("mdns: {args} ({count}x)"));
    }
}

/// Waits forever (a setup step failed and was logged).
async fn park() -> ! {
    loop {
        Timer::after_secs(3600).await;
    }
}

/// What the link asks to be announced.
fn wanted_now(stack: Stack<'_>) -> Option<Desired> {
    let v4 = stack
        .config_v4()
        .map(|c| (c.address.address().octets(), c.address.prefix_len()));
    wanted(stack.is_link_up(), v4)
}

/// The subnet of `d`'s address, as the only on-link one.
fn on_link(d: &Desired) -> OnLink {
    let cidr = Ipv4Cidr::new(Ipv4Addr::from(d.addr), d.prefix);
    match subnet(cidr) {
        Some(s) => OnLink::new().with(s),
        None => OnLink::new(),
    }
}

/// The wait after `e`.
fn backoff(e: &MdnsError<EmbassyError>) -> Duration {
    match e {
        MdnsError::Announce(AnnounceError::Conflict) => CONFLICT_BACKOFF,
        _ => BACKOFF,
    }
}

/// Runs the responder forever (see the module docs).
pub async fn run(stack: Stack<'static>, bus: &'static Bus, uid: [u8; 8], board: Board) -> ! {
    let id = identity(node_id(uid));
    let mut rx_meta = [PacketMetadata::EMPTY; 6];
    let mut tx_meta = [PacketMetadata::EMPTY; 4];
    let mut rx = [0u8; 4096];
    let mut tx = [0u8; 2048];
    let socket = UdpSocket::new(stack, &mut rx_meta, &mut rx, &mut tx_meta, &mut tx);
    let udp = match EmbassyUdp::bind(socket, MDNS_PORT) {
        Ok(u) => u,
        Err(e) => {
            bus.log(format_args!("mdns: bind to {MDNS_PORT} failed: {e:?}"));
            park().await
        }
    };
    // The membership report goes out on a live link.
    stack.wait_link_up().await;
    stack.wait_config_up().await;
    if let Err(e) = join_group(stack) {
        bus.log(format_args!("mdns: join {MDNS_GROUP_V4} failed: {e:?}"));
        park().await
    }
    let table = match AnnounceTable::new(&id.host) {
        Ok(t) => t,
        Err(e) => {
            bus.log(format_args!("mdns: host {} refused: {e:?}", id.host));
            park().await
        }
    };
    let params = AnnouncerParams {
        destination: SocketAddr::V4(SocketAddrV4::new(MDNS_GROUP_V4, MDNS_PORT)),
        policy: OriginPolicy::OnLink,
        on_link: OnLink::new(),
    };
    let mut announcer: Announcer<'_> = MdnsAnnouncer::new(udp, EmbassyClock, table, params);
    bus.log(format_args!(
        "mdns: {} on {} as {} ({} B announcer)",
        id.label, id.host, board.board, ANNOUNCER_BYTES
    ));
    let mut announced: Option<Desired> = None;
    let (mut announce_errors, mut step_errors) = (0u32, 0u32);
    loop {
        match plan(announced, wanted_now(stack)) {
            Action::Announce(d) => {
                // Redundant on 169.254/16 (OnLink accepts it), kept so a
                // DHCP lease (W5500) moves the accepted subnet.
                announcer.set_on_link(on_link(&d));
                let record = match txt(&board, &id, &d, FIRMWARE) {
                    Ok(t) => t,
                    Err(e) => {
                        log_rarely(bus, &mut announce_errors, format_args!("TXT: {e:?}"));
                        Timer::after(BACKOFF).await;
                        continue;
                    }
                };
                match announcer.announce(&service(&id, &d, &record)).await {
                    Ok(()) => {
                        let [a, b, c, e] = d.addr;
                        bus.log(format_args!(
                            "mdns: announced {} at {a}.{b}.{c}.{e}/{}",
                            announcer
                                .instance_label(KIND, &id.label)
                                .unwrap_or(&id.label),
                            d.prefix
                        ));
                        announced = Some(d);
                    }
                    Err(e) => {
                        // Nothing stays registered (a record left probing
                        // would fail every step); it is announced afresh on
                        // the next pass.
                        let _ = announcer.withdraw(KIND, &id.label).await;
                        announced = None;
                        log_rarely(bus, &mut announce_errors, format_args!("announce: {e}"));
                        Timer::after(backoff(&e)).await;
                        continue;
                    }
                }
            }
            Action::Withdraw => {
                // The table drops the record first: a goodbye that cannot
                // go out on a dead link is no loss.
                let _ = announcer.withdraw(KIND, &id.label).await;
                announced = None;
                bus.log(format_args!(
                    "mdns: link down or address lost, record withdrawn"
                ));
            }
            Action::Nothing => {}
        }
        if let Either::First(Err(e)) = select(announcer.step(), Timer::after(CHECK)).await {
            log_rarely(bus, &mut step_errors, format_args!("step: {e}"));
            Timer::after(BACKOFF).await;
        }
    }
}
