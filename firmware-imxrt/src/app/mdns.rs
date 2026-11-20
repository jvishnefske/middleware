//! The board's mDNS responder (feature `eth`, D43): it announces the
//! board as a `mw/node` ([`imxrt_base::mdns_node`]) on the LAN its DHCP
//! lease is on, so `avahi-browse -r _mw-node._tcp` and mwctl find it and
//! `rt1050-<id>.local` resolves to its address.
//!
//! [`run`] binds the one UDP socket on port 5353, waits for the lease,
//! joins 224.0.0.251 (the ENET's group hash admits it and 224.0.0.1, whose
//! IGMP queries keep the membership alive on a snooping switch) and then,
//! once a second ([`CHECK`]), compares the lease and the Zenoh endpoint
//! with what it announced ([`plan`]):
//!
//! * a new lease, address, prefix or endpoint: the on-link subnet follows
//!   the lease (queries from elsewhere are refused) and the record is
//!   announced (probing first the first time; an address change is sent
//!   at once with the cache-flush bit, so peers drop the old address);
//! * the lease is lost: the record is withdrawn (its goodbye may fail to
//!   go out without an address);
//!
//! and in between serves queries and the second announcement
//! (`MdnsAnnouncer::step`, dropped at each check, which is cancel safe).
//! A failure backs off [`BACKOFF`] ([`CONFLICT_BACKOFF`] when every rename
//! conflicted) before trying again; errors are logged at the 1st, 2nd, 4th,
//! 8th... occurrence.
//!
//! Its buffers (the socket's 4 KiB receive and 2 KiB transmit buffers,
//! the announcer's two 1500-byte buffers and table) live in this task's
//! future, so in its task pool, which `memory-app.x` places in SDRAM.

use core::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use embassy_futures::select::{Either, select};
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{Ipv4Cidr, Stack};
use embassy_time::{Duration, Timer};
use imxrt_base::mdns_node::{Action, Desired, Identity, plan, service, txt};
use middleware::discovery::Announce;
use middleware::mdns::announce::{AnnounceError, AnnounceTable, AnnouncerParams, MdnsAnnouncer};
use middleware::mdns::embassy_seam::{EmbassyClock, EmbassyError, EmbassyUdp, join_group, subnet};
use middleware::mdns::node::KIND;
use middleware::mdns::origin::{OnLink, OriginPolicy};
use middleware::mdns::socket::MdnsError;
use middleware::mdns::{MCU_BUF, MDNS_GROUP_V4, MDNS_PORT};
use middleware::zenoh_lite::client::sender_id_of;

use super::FIRMWARE;
use super::bus::Bus;

/// How often the lease and the endpoint are compared with the record.
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

/// What the lease and the configuration ask to be announced.
fn wanted(stack: Stack<'_>) -> Option<Desired> {
    stack.config_v4().map(|c| Desired {
        addr: c.address.address().octets(),
        prefix: c.address.prefix_len(),
        zenoh: super::zenoh::endpoint(),
    })
}

/// The subnet of `d`'s lease, as the only on-link one.
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
pub async fn run(stack: Stack<'static>, bus: &'static Bus, uid: [u8; 8]) -> ! {
    let id = Identity::of(uid);
    let sender = super::zenoh::zid(uid).map(sender_id_of);
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
        "mdns: {} on {} ({} B announcer)",
        id.label, id.host, ANNOUNCER_BYTES
    ));
    let mut announced: Option<Desired> = None;
    let (mut announce_errors, mut step_errors) = (0u32, 0u32);
    loop {
        match plan(announced, wanted(stack)) {
            Action::Announce(d) => {
                announcer.set_on_link(on_link(&d));
                let record = match txt(&id, &d, FIRMWARE, sender) {
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
                        // would fail every step without a lease); it is
                        // announced afresh on the next pass.
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
                // go out without an address is no loss.
                let _ = announcer.withdraw(KIND, &id.label).await;
                announced = None;
                bus.log(format_args!("mdns: lease lost, record withdrawn"));
            }
            Action::Nothing => {}
        }
        if let Either::First(Err(e)) = select(announcer.step(), Timer::after(CHECK)).await {
            log_rarely(bus, &mut step_errors, format_args!("step: {e}"));
            Timer::after(BACKOFF).await;
        }
    }
}
