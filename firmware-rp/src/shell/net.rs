//! The board on USB CDC-NCM (`usb-ncm`, D48): a second USB function beside
//! the CDC-ACM port and an embassy-net stack on it.
//!
//! [`add_ncm`] adds the function after CDC-ACM (so ACM keeps interfaces 0
//! and 1 and the port stays `...-if00`); the host's end of the link gets
//! [`netid::mac`]`(Host)` (`iMACAddress`), so Linux names it
//! `enx064d<8 hex digits>`. [`start`] gives the board
//! [`netid::mac`]`(Device)` and the static link-local address
//! [`netid::link_local`]/16 (no gateway, no DNS), seeds the stack from
//! boot entropy ([`netid::seed`]) and spawns the NCM runner and the stack
//! runner. The link is up while the host has the NCM data interface
//! enabled (`ncm::Runner` reports it), which gates the mDNS responder
//! (`super::mdns`).
//!
//! Buffers: two receive and two transmit packets of [`MTU`] bytes in the
//! channel state, the NCM runner's transfer buffers in its task future,
//! and [`SOCKETS`] socket slots (the mDNS socket and a spare).

use embassy_executor::Spawner;
use embassy_net::{Config, Ipv4Address, Ipv4Cidr, Stack, StackResources, StaticConfigV4};
use embassy_usb::Builder;
use embassy_usb::class::cdc_ncm::embassy_net::{
    Device, Runner as NcmRunner, State as ChannelState,
};
use embassy_usb::class::cdc_ncm::{CdcNcmClass, State as NcmState};
use rp_base::netid::{self, Side};
use rp_base::node::node_id;
use rp_base::usb::MAX_PACKET;
use static_cell::StaticCell;

use super::bus::Bus;
use super::link::UsbDriver;

/// The Ethernet frame size the link carries.
pub const MTU: usize = 1514;
/// Receive and transmit packets the channel holds.
const N_RX: usize = 2;
const N_TX: usize = 2;
/// Socket slots: the mDNS socket and a spare.
pub const SOCKETS: usize = 2;

static NCM_STATE: StaticCell<NcmState<'static>> = StaticCell::new();
static CHANNEL: StaticCell<ChannelState<MTU, N_RX, N_TX>> = StaticCell::new();
static RESOURCES: StaticCell<StackResources<SOCKETS>> = StaticCell::new();

/// The NCM class on the composite device; call after CDC-ACM is added.
pub fn add_ncm(
    builder: &mut Builder<'static, UsbDriver>,
    uid: [u8; 8],
) -> CdcNcmClass<'static, UsbDriver> {
    let host = netid::mac(node_id(uid), Side::Host);
    CdcNcmClass::new(builder, NCM_STATE.init(NcmState::new()), host, MAX_PACKET)
}

#[embassy_executor::task]
async fn ncm_task(runner: NcmRunner<'static, UsbDriver, MTU>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, Device<'static, MTU>>) -> ! {
    runner.run().await
}

/// Brings up the stack on `class` and spawns its runners; the stack, for
/// the responder. `entropy` comes from the chip's random source.
pub fn start(
    spawner: Spawner,
    class: CdcNcmClass<'static, UsbDriver>,
    uid: [u8; 8],
    entropy: [u8; 8],
    bus: &Bus,
) -> Stack<'static> {
    let node = node_id(uid);
    let (ncm, device) = class.into_embassy_net_device::<MTU, N_RX, N_TX>(
        CHANNEL.init(ChannelState::new()),
        netid::mac(node, Side::Device),
    );
    let [a, b, c, d] = netid::link_local(node);
    let config = Config::ipv4_static(StaticConfigV4 {
        address: Ipv4Cidr::new(Ipv4Address::new(a, b, c, d), netid::PREFIX_LEN),
        gateway: None,
        dns_servers: Default::default(),
    });
    let (stack, runner) = embassy_net::new(
        device,
        config,
        RESOURCES.init(StackResources::new()),
        netid::seed(entropy, node),
    );
    spawner.spawn(ncm_task(ncm).unwrap());
    spawner.spawn(net_task(runner).unwrap());
    bus.log(format_args!(
        "net: usb ncm at {a}.{b}.{c}.{d}/{}, host interface enx{}",
        netid::PREFIX_LEN,
        HostIf(netid::mac(node, Side::Host)),
    ));
    stack
}

/// A MAC as Linux's `enx` interface suffix (12 lower-case hex digits).
struct HostIf([u8; 6]);

impl core::fmt::Display for HostIf {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.0.iter().try_for_each(|b| write!(f, "{b:02x}"))
    }
}
