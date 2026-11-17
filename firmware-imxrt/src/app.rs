//! Imperative shell of the firmware: bring-up and the tasks around the
//! pure core.
//!
//! Boot sequence ([`run`]):
//!
//! 1. `embassy_nxp::init` (peripheral singletons, the PIT time driver);
//!    the bootloader's [`Handoff`] word is read from SRC_GPR8 and cleared;
//!    the I and D caches are enabled (code, data and the heap are in
//!    SDRAM; everything a DMA engine touches is in the uncached DTCM, and
//!    the HyperFlash is read by IP command, not through the cached AHB
//!    window).
//! 2. The unique id is read from OCOTP's fuse shadows (clock gate first)
//!    and becomes the node id ([`imxrt_base::node`]).
//! 3. LPUART1 comes up as the protocol link ([`uart`], [`link`]) on its own
//!    task.
//! 4. The HyperFlash driver opens and the request service
//!    ([`imxrt_base::service::Service`]) opens the trust record, the A/B
//!    updater (from the handoff) and the payload store at the EVKB map;
//!    the boot log reports what it found on `sys/log`.
//! 5. The [`controller`] runs on the main task.
//!
//! Ownership is a tree: the main task owns the controller (service,
//! flash); the link task owns the UART; they meet only through the
//! `'static` [`bus::Bus`].

#[cfg(feature = "wasm")]
mod adc;
mod bus;
mod controller;
#[cfg(feature = "wasm")]
mod device;
mod edma;
#[cfg(feature = "eth")]
pub mod enet;
#[cfg(feature = "wasm")]
mod heap;
#[cfg(feature = "wasm")]
mod i2c;
mod link;
mod reset;
#[cfg(feature = "wasm")]
mod sai;
mod uart;
#[cfg(feature = "usb")]
pub mod usb;

use core::fmt::Write as _;

use embassy_executor::Spawner;
use embassy_nxp::pac;
use esp_base::trust_store::Persisted;
use imxrt_base::ab::boot::Handoff;
use imxrt_base::chip::RT1050;
use imxrt_base::layout::RT1050_EVKB;
use imxrt_base::node::{Serial, node_id, unique_id};
use imxrt_base::service::{Boot, Facts, Service};
use imxrt_base::shell::console::Console;
use imxrt_base::shell::flexspi::{FlashError, HyperFlash};
use middleware::loader::trust::TrustState;
use static_cell::ConstStaticCell;

use bus::Bus;
use controller::Controller;

/// Whether this build runs payloads (feature `wasm`).
pub const RUNTIME: bool = cfg!(feature = "wasm");

/// The firmware version `sys/info` reports; a build without the payload
/// runtime carries the `+no-wasm` suffix hosts read it from (as the
/// RP2040's).
#[cfg(feature = "wasm")]
pub const FIRMWARE: &str = env!("CARGO_PKG_VERSION");
/// The firmware version `sys/info` reports; a build without the payload
/// runtime carries the `+no-wasm` suffix hosts read it from (as the
/// RP2040's).
#[cfg(not(feature = "wasm"))]
pub const FIRMWARE: &str = concat!(env!("CARGO_PKG_VERSION"), "+no-wasm");

/// The bus, in SDRAM (`memory-app.x`: statics named `*_SDRAM`).
static BUS_SDRAM: ConstStaticCell<Bus> = ConstStaticCell::new(Bus::new());

#[cfg(feature = "eth")]
mod zenoh;

#[cfg(feature = "eth")]
static NET: static_cell::StaticCell<embassy_net::StackResources<4>> =
    static_cell::StaticCell::new();

#[cfg(feature = "eth")]
#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, enet::EnetDriver>) -> ! {
    runner.run().await
}

#[cfg(feature = "eth")]
#[embassy_executor::task]
async fn enet_poll_task(poller: enet::EnetPoller) -> ! {
    enet::poll_task(poller).await
}

#[cfg(feature = "eth")]
#[embassy_executor::task]
async fn zenoh_task(stack: embassy_net::Stack<'static>, bus: &'static Bus, uid: [u8; 8]) -> ! {
    zenoh::run(stack, bus, uid).await
}

/// Brings up Ethernet (the KSZ8081 reset takes about 110 ms), the
/// embassy-net stack with DHCP and the Zenoh session task.
#[cfg(feature = "eth")]
async fn start_eth(spawner: Spawner, bus: &'static Bus, uid: [u8; 8]) {
    let mac = imxrt_base::enet::mac_from_uid(uid);
    match enet::new(mac).await {
        Ok((driver, poller)) => {
            let seed = u64::from_be_bytes(uid) ^ embassy_time::Instant::now().as_ticks();
            let (stack, runner) = embassy_net::new(
                driver,
                embassy_net::Config::dhcpv4(Default::default()),
                NET.init(embassy_net::StackResources::new()),
                seed,
            );
            spawner.spawn(net_task(runner).unwrap());
            spawner.spawn(enet_poll_task(poller).unwrap());
            spawner.spawn(zenoh_task(stack, bus, uid).unwrap());
            bus.log(format_args!(
                "eth: up, MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
            ));
        }
        Err(e) => bus.log(format_args!("eth: not started ({e:?})")),
    }
}

/// A compile-time decimal (the stress periods); 1000 if it is not one.
#[cfg(feature = "timerstress")]
const fn parse_us(s: &str) -> u64 {
    let b = s.as_bytes();
    let mut v = 0u64;
    let mut i = 0;
    while i < b.len() {
        if b[i] < b'0' || b[i] > b'9' {
            return 1000;
        }
        v = v * 10 + (b[i] - b'0') as u64;
        i += 1;
    }
    v
}

#[cfg(feature = "timerstress")]
#[embassy_executor::task(pool_size = 2)]
async fn timer_stress_task(period_us: u64) -> ! {
    loop {
        embassy_time::Timer::after_micros(period_us).await;
    }
}

#[cfg(feature = "usb")]
static USB_SERIAL: static_cell::StaticCell<Serial> = static_cell::StaticCell::new();

#[cfg(feature = "usb")]
#[embassy_executor::task]
async fn usb_link_task(link: usb::UsbLink, bus: &'static Bus) {
    link::serve(link, imxrt_base::links::LinkId::USB, bus).await
}

#[embassy_executor::task]
async fn uart_task(link: link::UartLink, bus: &'static Bus) {
    link::serve(link, imxrt_base::links::LinkId::UART, bus).await
}

/// The chip's unique id from OCOTP CFG0/CFG1 (OCOTP's clock gate, CCGR2
/// CG6, enabled first).
fn read_unique_id() -> [u8; 8] {
    pac::CCM.ccgr2().modify(|w| w.set_cg6(0b11));
    unique_id(pac::OCOTP.cfg0().read().0, pac::OCOTP.cfg1().read().0)
}

/// Logs what the boot found.
fn log_boot(
    bus: &Bus,
    uid: [u8; 8],
    boot: &Boot<FlashError>,
    capacity: u32,
    handoff: Option<Handoff>,
) {
    bus.log(format_args!(
        "imxrt-base {FIRMWARE} on {} (node {}, payload capacity {} B)",
        RT1050.name,
        Serial::new(uid).as_str(),
        capacity,
    ));
    match handoff {
        Some(h) => bus.log(format_args!(
            "fw: booted slot {}{}",
            h.slot.name(),
            if h.trial { " on trial" } else { "" }
        )),
        None => bus.log(format_args!("fw: no handoff from the bootloader")),
    }
    match &boot.fw {
        Ok(_) => {}
        Err(e) => bus.log(format_args!("fw: updater unavailable ({e:?})")),
    }
    match &boot.installed {
        Ok(h) => bus.log(format_args!(
            "stored payload: {} bytes, crc {:08x}",
            h.len, h.crc
        )),
        Err(e) => bus.log(format_args!("no valid stored payload: {e}")),
    }
    match boot.trust {
        None => bus.log(format_args!(
            "trust record unreadable; payload and firmware installs refused"
        )),
        Some(Persisted {
            trust: TrustState::Unclaimed,
            ..
        }) => bus.log(format_args!(
            "trust: unclaimed; any payload accepted until sys/trust/claim"
        )),
        Some(Persisted {
            trust: TrustState::Claimed { key, min_version },
            ..
        }) => bus.log(format_args!(
            "trust: claimed by {} key {:02x}{:02x}{:02x}{:02x}..., min_version {}",
            key.kind().name(),
            key.as_bytes()[0],
            key.as_bytes()[1],
            key.as_bytes()[2],
            key.as_bytes()[3],
            min_version.0
        )),
    }
}

/// Boots nothing more: prints `why` on LPUART1 every few seconds (the
/// service could not open, so there is no protocol to report it on).
fn halt(
    uart: embassy_nxp::Peri<'static, embassy_nxp::peripherals::LPUART1>,
    why: core::fmt::Arguments<'_>,
) -> ! {
    let mut console = Console::new(uart);
    loop {
        let _ = writeln!(console, "imxrt-base: HALTED: {why}\r");
        embassy_time::block_for(embassy_time::Duration::from_secs(3));
    }
}

/// Whether the embassy pin `p` is the board pad `want` carries (its GPIO
/// bank and bit), so a mistake in [`install_payload_hw`]'s list leaves the
/// pin out instead of driving the wrong pad.
#[cfg(feature = "wasm")]
fn same_line(
    p: &embassy_nxp::Peri<'static, embassy_nxp::gpio::AnyPin>,
    want: board_pins::imxrt::GpioLine,
) -> bool {
    use embassy_nxp::gpio::{Bank, Pin};
    let bank = match p.bank() {
        Bank::Gpio1 => 1,
        Bank::Gpio2 => 2,
        Bank::Gpio3 => 3,
        Bank::Gpio4 => 4,
        Bank::Gpio5 => 5,
        _ => 0,
    };
    bank == want.bank().number() && p.pin() == want.bit()
}

/// Sets the payload hardware aside: a `Flex` for every payload pin whose
/// pad matches the board table, ADC1 and LPI2C1. Called once.
#[cfg(feature = "wasm")]
fn install_payload_hw(
    bus: &Bus,
    pins: [Option<embassy_nxp::Peri<'static, embassy_nxp::gpio::AnyPin>>; imxrt_base::pins::GPIOS],
) {
    use embassy_nxp::gpio::Flex;
    let mut bank: [Option<Flex<'static>>; imxrt_base::pins::GPIOS] =
        [const { None }; imxrt_base::pins::GPIOS];
    for ((n, slot), pin) in (0u8..).zip(bank.iter_mut()).zip(pins) {
        let Some(pin) = pin else { continue };
        match RT1050.board_pin(n) {
            Ok(b) if same_line(&pin, b.pad.gpio()) => {
                let mut f = Flex::new(pin);
                f.set_as_input();
                f.set_pull(embassy_nxp::gpio::Pull::None);
                *slot = Some(f);
            }
            _ => bus.log(format_args!(
                "pin {n}: pad does not match the board table; left out"
            )),
        }
    }
    device::HW.put(device::Hw {
        pins: bank,
        adc: adc::Adc::new(),
        i2c: i2c::I2c::new(),
    });
}

/// Core cycles for 1000 reads of PIT channel 2's CVAL (diagnostic: the
/// time driver's cost per register access), with DWT's cycle counter.
#[cfg(feature = "timerstress")]
fn pit_read_cycles(
    dcb: &mut cortex_m::peripheral::DCB,
    dwt: &mut cortex_m::peripheral::DWT,
) -> u32 {
    dcb.enable_trace();
    dwt.enable_cycle_counter();
    let t0 = cortex_m::peripheral::DWT::cycle_count();
    let mut x = 0u32;
    for _ in 0..1000 {
        x = x.wrapping_add(pac::PIT.timer(2).cval().read().0);
    }
    core::hint::black_box(x);
    cortex_m::peripheral::DWT::cycle_count().wrapping_sub(t0)
}

/// Brings up the board and runs the firmware forever.
pub async fn run(spawner: Spawner) -> ! {
    #[cfg(feature = "wasm")]
    heap::init();
    let p = embassy_nxp::init(Default::default());
    let handoff = Handoff::decode(pac::SRC.gpr8().read());
    pac::SRC.gpr8().write_value(0);
    if handoff.is_some_and(|h| h.trial) {
        reset::arm_trial();
    }
    #[cfg(feature = "timerstress")]
    let mut pit_cycles = 0u32;
    if let Some(mut cp) = cortex_m::Peripherals::take() {
        cp.SCB.enable_icache();
        cp.SCB.enable_dcache(&mut cp.CPUID);
        #[cfg(feature = "timerstress")]
        {
            pit_cycles = pit_read_cycles(&mut cp.DCB, &mut cp.DWT);
        }
    }
    let bus: &'static Bus = BUS_SDRAM.take();
    let uid = read_unique_id();

    let flash = match HyperFlash::new(p.FLEXSPI1) {
        Ok(f) => f,
        Err(e) => halt(p.LPUART1, format_args!("HyperFlash unavailable: {e:?}")),
    };
    let facts = Facts {
        chip: RT1050.name,
        firmware: FIRMWARE,
        runtime: RUNTIME,
        node: node_id(uid),
        max_image: RT1050.max_image,
    };
    let (svc, boot) = match Service::open(flash, &RT1050_EVKB, facts, handoff) {
        Ok(opened) => opened,
        Err(e) => halt(p.LPUART1, format_args!("service: {e:?}")),
    };

    let (rx, tx) = uart::split(p.LPUART1);
    spawner.spawn(uart_task(link::new(rx, tx), bus).unwrap());
    #[cfg(feature = "timerstress")]
    {
        spawner.spawn(
            timer_stress_task(option_env!("IMXRT_STRESS_A_US").map_or(125, parse_us)).unwrap(),
        );
        spawner.spawn(
            timer_stress_task(option_env!("IMXRT_STRESS_B_US").map_or(250, parse_us)).unwrap(),
        );
    }
    #[cfg(feature = "eth")]
    {
        zenoh::set_endpoint(svc.zenoh_endpoint());
        start_eth(spawner, bus, uid).await;
    }
    #[cfg(feature = "usb")]
    match usb::start(spawner, USB_SERIAL.init(Serial::new(uid)).as_str()) {
        Ok(link) => spawner.spawn(usb_link_task(link, bus).unwrap()),
        Err(e) => bus.log(format_args!("usb: not started ({e:?})")),
    }
    log_boot(bus, uid, &boot, svc.capacity(), handoff);
    #[cfg(feature = "timerstress")]
    bus.log(format_args!(
        "pit: 1000 register reads take {pit_cycles} core cycles"
    ));
    #[cfg(feature = "wasm")]
    {
        // The logical pins in order (board_pins::imxrt::evkb): D0–D15,
        // A0–A5, SW8; D4, D5 (the PHY's reset and interrupt) and the
        // LPUART1 pins are reserved.
        let pins = [
            Some(p.GPIO_AD_B1_07.into()),
            Some(p.GPIO_AD_B1_06.into()),
            Some(p.GPIO_AD_B0_11.into()),
            Some(p.GPIO_AD_B1_08.into()),
            None,
            None,
            Some(p.GPIO_AD_B1_02.into()),
            Some(p.GPIO_AD_B1_03.into()),
            Some(p.GPIO_AD_B0_03.into()),
            Some(p.GPIO_AD_B0_02.into()),
            Some(p.GPIO_SD_B0_01.into()),
            Some(p.GPIO_SD_B0_02.into()),
            Some(p.GPIO_SD_B0_03.into()),
            Some(p.GPIO_SD_B0_00.into()),
            Some(p.GPIO_AD_B0_01.into()),
            Some(p.GPIO_AD_B0_00.into()),
            Some(p.GPIO_AD_B1_10.into()),
            Some(p.GPIO_AD_B1_11.into()),
            Some(p.GPIO_AD_B1_04.into()),
            Some(p.GPIO_AD_B1_05.into()),
            Some(p.GPIO_AD_B1_01.into()),
            Some(p.GPIO_AD_B1_00.into()),
            Some(p.WAKEUP.into()),
            None,
            None,
        ];
        install_payload_hw(bus, pins);
        bus.log(format_args!("heap: {} B free after boot", heap::free()));
    }
    Controller::new(svc, bus, imxrt_base::enet::mac_from_uid(uid))
        .run()
        .await
}
