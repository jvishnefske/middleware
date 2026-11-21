//! The board on a WIZnet W5500 (`w5500`, D53): an embassy-net stack on the
//! chip's MACRAW socket over SPI0, with DHCP.
//!
//! Pins (the W5500-EVB-Pico's, [`rp_base::wiznet`]): MISO 16, CS 17 (a
//! plain output), SCK 18, MOSI 19, reset 20, interrupt 21 (pull-up). The
//! profile keeps them and SPI0 from payloads
//! ([`rp_base::chip::RP2350_W5500`]). The bus runs at most
//! [`wiznet::SPI_HZ`], mode 0, with DMA channels 0 and 1.
//!
//! [`start`] resets the chip and initialises it through
//! [`wiznet::NoMacFilter`], so multicast (mDNS) reaches the board. A board
//! without the module (or with a dead one) logs the error and carries on:
//! the serial link and mwctl work without it. On success the stack asks
//! for a DHCP lease with the board's label (`rp-<id>`) as host name, seeded
//! from boot entropy ([`netid::seed`]), and the driver's runner, the stack
//! and the mDNS responder ([`super::mdns`], board `pico-w5500` or
//! `pico2-w5500`) are spawned. The responder announces while the PHY link
//! is up and a lease is held, and withdraws when either goes.
//!
//! Buffers: two receive and two transmit frames in the channel state, the
//! chip's own 16 KiB per direction, and [`SOCKETS`] socket slots (mDNS,
//! DHCP and a spare).

use embassy_embedded_hal::shared_bus::asynch::spi::SpiDevice;
use embassy_executor::Spawner;
use embassy_net::{Config, DhcpConfig, Stack, StackResources};
use embassy_net_wiznet::chip::W5500;
use embassy_net_wiznet::{Device, Runner, State};
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::peripherals::{
    DMA_CH0, DMA_CH1, PIN_16, PIN_17, PIN_18, PIN_19, PIN_20, PIN_21, SPI0,
};
use embassy_rp::spi::{Async, Spi};
use embassy_rp::{Peri, bind_interrupts, dma};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::mutex::Mutex;
use rp_base::mdns_node::{self, Link};
use rp_base::netid::{self, Side};
use rp_base::node::node_id;
use rp_base::wiznet::{self, NoMacFilter};
use static_cell::StaticCell;

use super::board::PROFILE;
use super::bus::Bus;
use super::mdns::mdns_task;

bind_interrupts!(struct DmaIrqs {
    DMA_IRQ_0 => dma::InterruptHandler<DMA_CH0>, dma::InterruptHandler<DMA_CH1>;
});

/// Receive and transmit frames the channel holds.
const N_RX: usize = 2;
const N_TX: usize = 2;
/// Socket slots: mDNS, DHCP and a spare.
pub const SOCKETS: usize = 3;

/// The SPI0 controller with DMA.
type SpiBus0 = Spi<'static, SPI0, Async>;
/// The W5500 on it, behind the MAC filter rewrite.
type W5500Spi = NoMacFilter<SpiDevice<'static, NoopRawMutex, SpiBus0, Output<'static>>>;
/// The driver's runner.
type W5500Runner = Runner<'static, W5500, W5500Spi, Input<'static>, Output<'static>>;

static SPI: StaticCell<Mutex<NoopRawMutex, SpiBus0>> = StaticCell::new();
static CHANNEL: StaticCell<State<N_RX, N_TX>> = StaticCell::new();
static RESOURCES: StaticCell<StackResources<SOCKETS>> = StaticCell::new();

/// The W5500's peripherals.
pub struct Parts {
    pub spi: Peri<'static, SPI0>,
    pub miso: Peri<'static, PIN_16>,
    pub cs: Peri<'static, PIN_17>,
    pub sck: Peri<'static, PIN_18>,
    pub mosi: Peri<'static, PIN_19>,
    pub rst: Peri<'static, PIN_20>,
    pub int: Peri<'static, PIN_21>,
    pub tx_dma: Peri<'static, DMA_CH0>,
    pub rx_dma: Peri<'static, DMA_CH1>,
}

#[embassy_executor::task]
async fn wiznet_task(runner: W5500Runner) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, Device<'static>>) -> ! {
    runner.run().await
}

/// Brings the W5500 up and spawns its runner, the stack and the mDNS
/// responder; logs and returns if the chip does not answer.
pub async fn start(
    spawner: Spawner,
    parts: Parts,
    uid: [u8; 8],
    entropy: [u8; 8],
    bus: &'static Bus,
) {
    let node = node_id(uid);
    let mut config = embassy_rp::spi::Config::default();
    config.frequency = wiznet::SPI_HZ;
    let spi = Spi::new(
        parts.spi,
        parts.sck,
        parts.mosi,
        parts.miso,
        parts.tx_dma,
        parts.rx_dma,
        DmaIrqs,
        config,
    );
    let spi = SPI.init(Mutex::new(spi));
    let device = NoMacFilter::new(SpiDevice::new(spi, Output::new(parts.cs, Level::High)));
    let int = Input::new(parts.int, Pull::Up);
    let rst = Output::new(parts.rst, Level::High);
    let mac = netid::mac(node, Side::Device);
    let (device, runner) = match embassy_net_wiznet::new::<N_RX, N_TX, W5500, _, _, _>(
        mac,
        CHANNEL.init(State::new()),
        device,
        int,
        rst,
    )
    .await
    {
        Ok(up) => up,
        Err(e) => {
            bus.log(format_args!("w5500: init failed ({e:?}); no network"));
            return;
        }
    };
    let id = mdns_node::identity(node);
    let mut dhcp = DhcpConfig::default();
    dhcp.hostname = heapless::String::try_from(id.label.as_str()).ok();
    let (stack, net): (Stack<'static>, _) = embassy_net::new(
        device,
        Config::dhcpv4(dhcp),
        RESOURCES.init(StackResources::new()),
        netid::seed(entropy, node),
    );
    spawner.spawn(wiznet_task(runner).unwrap());
    spawner.spawn(net_task(net).unwrap());
    let board = mdns_node::board(PROFILE.chip, Link::W5500);
    spawner.spawn(mdns_task(stack, bus, uid, board).unwrap());
    let [a, b, c, d, e, f] = mac;
    bus.log(format_args!(
        "net: w5500 up, mac {a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{f:02x}, dhcp as {}",
        id.label
    ));
}
