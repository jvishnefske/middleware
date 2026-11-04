//! Imperative shell: hardware bring-up and the tasks around the pure core.
//!
//! Boot sequence ([`run`]):
//!
//! 1. With `wasm`, the heap arena goes to the allocator ([`heap`]).
//!    `embassy_rp::init` with the default clocks (RP2040 125 MHz, RP2350
//!    150 MHz) and the embassy time driver.
//! 2. The QSPI flash is opened in blocking mode and the chip's unique id
//!    read ([`board::unique_id`]); it becomes the node id and the USB
//!    serial number ([`rp_base::node`]).
//! 3. USB comes up as one CDC-ACM interface ([`rp_base::usb`] identity) and
//!    two tasks start: the USB device task and the [`link`] task, which
//!    runs the serial protocol on the CDC port. Nothing is printed as text;
//!    logs are `sys/log` publications.
//! 4. On the RP2350 the whole flash is mapped untranslated at XIP window 1
//!    ([`rom::map_untranslated_window`]) for the flash adapter's reads,
//!    and the firmware updater starts ([`rp_base::ota::AbUpdate::boot`]:
//!    which slot booted, on trial or not, and the persisted record,
//!    reconciled). The [`rp_base::service::Service`] then opens the trust
//!    record and the payload store at the chip's flash map
//!    ([`rp_base::layout`]) through the one-sector-per-erase adapter, and
//!    the boot log reports the build, the heap plan, the stored payload,
//!    the trust state and the updater's state.
//! 5. With `wasm`, the payload hardware is set aside in [`device::HW`]: a
//!    `Flex` for every payload pin (GPIO 0–22, 24, 25), the ADC with
//!    GPIO 26–28, SPI0 and SPI1 with the 4 KiB transfer buffer
//!    ([`spi`]), and I2C0 and I2C1 ([`i2c`]), all controllers held in
//!    reset until a payload opens them. The SPI and I2C register drivers
//!    ([`pads`], [`spi`], [`i2c`]) are built on the RP2040 too, which runs
//!    no payloads, so their register code is checked for both chips.
//! 6. The [`controller`] runs on the main task and starts the stored
//!    payload if one is valid (and the runtime is built in).
//!
//! Ownership is a tree: the main task owns the controller (service, flash,
//! payload); the link task owns the CDC class; they meet only through the
//! `'static` [`bus::Bus`]. The payload hardware rests in [`device::HW`]
//! between runs.

mod board;
mod bus;
mod controller;
#[cfg(feature = "wasm")]
mod device;
#[cfg(feature = "wasm")]
mod heap;
#[cfg_attr(not(feature = "wasm"), allow(dead_code, reason = "the RP2040 runs no payloads yet; built so its register code is checked"))]
mod i2c;
mod link;
#[cfg_attr(not(feature = "wasm"), allow(dead_code, reason = "the RP2040 runs no payloads yet; built so its register code is checked"))]
mod pads;
#[cfg(all(feature = "fw-update", feature = "rp2350"))]
mod rom;
#[cfg_attr(not(feature = "wasm"), allow(dead_code, reason = "the RP2040 runs no payloads yet; built so its register code is checked"))]
mod spi;

use embassy_executor::Spawner;
use embassy_rp::bind_interrupts;
use embassy_rp::flash::Flash;
use embassy_rp::peripherals::USB;
use embassy_rp::usb::{Driver, InterruptHandler};
use embassy_usb::class::cdc_acm::{CdcAcmClass, State as CdcState};
use embassy_usb::{Builder, Handler, UsbDevice};
use esp_base::trust_store::Persisted;
use middleware::loader::trust::TrustState;
use middleware::serial::link::{Duplex, SerialLink};
use rp_base::node::{Serial, node_id};
use rp_base::service::{Facts, Service};
use rp_base::usb::{MANUFACTURER, MAX_PACKET, PRODUCT, VID, pid};
use static_cell::{ConstStaticCell, StaticCell};

use board::{FIRMWARE, PROFILE, PROFILE_REF};
use bus::Bus;
use controller::Controller;
use link::{CdcWriter, UsbDriver, UsbLink};

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => InterruptHandler<USB>;
});

static BUS: ConstStaticCell<Bus> = ConstStaticCell::new(Bus::new());

/// Buffers the USB stack borrows for the life of the firmware.
struct UsbMemory {
    config: [u8; 256],
    bos: [u8; 256],
    msos: [u8; 0],
    control: [u8; 64],
    rx: [u8; MAX_PACKET as usize],
}

static USB_MEMORY: ConstStaticCell<UsbMemory> = ConstStaticCell::new(UsbMemory {
    config: [0; 256],
    bos: [0; 256],
    msos: [0; 0],
    control: [0; 64],
    rx: [0; MAX_PACKET as usize],
});

/// The CDC class state (not `Sync`, so initialised at run time).
static CDC_STATE: StaticCell<CdcState<'static>> = StaticCell::new();

static SERIAL: StaticCell<Serial> = StaticCell::new();

/// Tells the bus when the host configures (or deconfigures) the device.
struct UsbState {
    bus: &'static Bus,
}

impl Handler for UsbState {
    fn configured(&mut self, configured: bool) {
        self.bus.set_usb_configured(configured);
    }

    fn reset(&mut self) {
        self.bus.set_usb_configured(false);
    }
}

static USB_STATE: StaticCell<UsbState> = StaticCell::new();

#[embassy_executor::task]
async fn usb_task(mut device: UsbDevice<'static, UsbDriver>) -> ! {
    device.run().await
}

#[embassy_executor::task]
async fn link_task(link: UsbLink, bus: &'static Bus) {
    link::serve(link, bus).await;
}

/// Brings up USB: the device task and the CDC link task.
fn start_usb(spawner: Spawner, usb: embassy_rp::Peri<'static, USB>, serial: &'static str, bus: &'static Bus) {
    let driver = Driver::new(usb, Irqs);
    let mut config = embassy_usb::Config::new(VID, pid(PROFILE.chip));
    config.manufacturer = Some(MANUFACTURER);
    config.product = Some(PRODUCT);
    config.serial_number = Some(serial);
    config.max_power = 100;
    config.max_packet_size_0 = 64;
    let mem = USB_MEMORY.take();
    let mut builder =
        Builder::new(driver, config, &mut mem.config, &mut mem.bos, &mut mem.msos, &mut mem.control);
    builder.handler(USB_STATE.init(UsbState { bus }));
    let class = CdcAcmClass::new(&mut builder, CDC_STATE.init(CdcState::new()), MAX_PACKET);
    let device = builder.build();
    let (tx, rx) = class.split();
    let link = SerialLink::with_inbox(Duplex::new(rx.into_buffered(&mut mem.rx), CdcWriter::new(tx)));
    spawner.spawn(usb_task(device).unwrap());
    spawner.spawn(link_task(link, bus).unwrap());
}

/// Logs what the boot found.
fn log_boot(bus: &Bus, uid: [u8; 8], boot: &rp_base::service::Boot, capacity: u32) {
    bus.log(format_args!(
        "rp-base {FIRMWARE} on {} (node {}, heap {} B, WASM memory {} B, payload capacity {} B)",
        PROFILE.name,
        Serial::new(uid).as_str(),
        PROFILE.heap,
        PROFILE.max_memory_bytes,
        capacity,
    ));
    match boot.installed {
        None => bus.log(format_args!("payload store unreadable; loader disabled")),
        Some(Ok(h)) => bus.log(format_args!("stored payload: {} bytes, crc {:08x}", h.len, h.crc)),
        Some(Err(e)) => bus.log(format_args!("no valid stored payload: {e}")),
    }
    match boot.trust {
        None => bus.log(format_args!("trust record unreadable; payload and firmware installs refused")),
        Some(Persisted { trust: TrustState::Unclaimed, .. }) => {
            bus.log(format_args!("trust: unclaimed; any payload accepted until sys/trust/claim"))
        }
        Some(Persisted { trust: TrustState::Claimed { key, min_version }, .. }) => bus.log(format_args!(
            "trust: claimed by key {:02x}{:02x}{:02x}{:02x}..., min_version {}",
            key.0[0], key.0[1], key.0[2], key.0[3], min_version.0
        )),
    }
}

/// The A/B slots of the RP2350 map (a compile-time error if it had none).
#[cfg(all(feature = "fw-update", feature = "rp2350"))]
const AB: rp_base::layout::AbMap = match PROFILE.flash.ab() {
    Some(ab) => ab,
    None => panic!("the RP2350 flash map has A/B slots"),
};

/// Starts the firmware updater on the flash and logs what it found.
#[cfg(all(feature = "fw-update", feature = "rp2350"))]
fn open_fw(
    mut flash: board::StoreFlash,
    watchdog: embassy_rp::Peri<'static, embassy_rp::peripherals::WATCHDOG>,
    bus: &Bus,
) -> (board::StoreFlash, board::Fw) {
    let (fw, boot) = rp_base::ota::AbUpdate::boot(&mut flash, rom::RpRom::new(watchdog), AB);
    match (boot, fw.state(), fw.record()) {
        (Ok(b), Some(state), Some(record)) => bus.log(format_args!(
            "fw: booted slot {}{}, state {}, min_version {}.{}",
            b.running.name(),
            if b.trial { " on trial" } else { "" },
            state.code().name(),
            record.min_version.0 >> 16,
            record.min_version.0 & 0xffff,
        )),
        (Err(e), _, _) => bus.log(format_args!("fw: updater unavailable ({e:?}); flash the factory UF2")),
        _ => bus.log(format_args!("fw: updater unavailable")),
    }
    (flash, fw)
}

/// The SPI bus's transfer buffer.
#[cfg(feature = "wasm")]
static SPI_BUFFER: ConstStaticCell<spi::Buffer> = ConstStaticCell::new([0; middleware::wasm_abi::periph::MAX_DMA]);

/// The payload's bus controllers.
#[cfg(feature = "wasm")]
struct Buses {
    spi0: embassy_rp::Peri<'static, embassy_rp::peripherals::SPI0>,
    spi1: embassy_rp::Peri<'static, embassy_rp::peripherals::SPI1>,
    i2c0: embassy_rp::Peri<'static, embassy_rp::peripherals::I2C0>,
    i2c1: embassy_rp::Peri<'static, embassy_rp::peripherals::I2C1>,
}

/// Sets the payload hardware aside: the GPIO bank, the ADC and the SPI and
/// I2C controllers.
#[cfg(feature = "wasm")]
fn install_payload_hw(
    pins: [embassy_rp::Peri<'static, embassy_rp::gpio::AnyPin>; 25],
    adc: embassy_rp::Peri<'static, embassy_rp::peripherals::ADC>,
    analog: [embassy_rp::adc::Channel<'static>; 3],
    buses: Buses,
) {
    use embassy_rp::gpio::{Flex, Pin};
    use rp_base::pins::GPIOS;
    let mut bank: [Option<Flex<'static>>; GPIOS] = [const { None }; GPIOS];
    for pin in pins {
        let n = usize::from(pin.pin());
        if let Some(slot) = bank.get_mut(n) {
            *slot = Some(Flex::new(pin));
        }
    }
    let adc = embassy_rp::adc::Adc::new_blocking(adc, embassy_rp::adc::Config::default());
    device::HW.put(device::Hw {
        pins: bank,
        adc: device::Analog::new(adc, analog),
        spi: spi::SpiBus::new(buses.spi0, buses.spi1, SPI_BUFFER.take()),
        i2c: i2c::I2cBuses::new(buses.i2c0, buses.i2c1),
    });
}

/// Brings up the board and runs the firmware forever.
pub async fn run(spawner: Spawner) -> ! {
    #[cfg(feature = "wasm")]
    heap::init();
    let p = embassy_rp::init(Default::default());
    let bus: &'static Bus = BUS.take();

    let mut flash: board::FlashDriver = Flash::new_blocking(p.FLASH);
    let uid = board::unique_id(&mut flash);
    let serial: &'static Serial = SERIAL.init(Serial::new(uid));
    start_usb(spawner, p.USB, serial.as_str(), bus);

    let facts = Facts {
        chip: PROFILE.name,
        firmware: FIRMWARE,
        runtime: PROFILE.runtime,
        node: node_id(uid),
        max_image: PROFILE.max_image,
    };
    #[cfg(all(feature = "fw-update", feature = "rp2350"))]
    let (flash, fw) = open_fw(board::store_flash(flash), p.WATCHDOG, bus);
    #[cfg(not(all(feature = "fw-update", feature = "rp2350")))]
    let (flash, fw) = (board::store_flash(flash), board::Fw::default());
    let (svc, boot) = Service::open(flash, &PROFILE_REF.flash, facts, fw);
    log_boot(bus, uid, &boot, svc.capacity());

    #[cfg(feature = "wasm")]
    {
        use embassy_rp::adc::Channel;
        use embassy_rp::gpio::Pull;
        let analog = [
            Channel::new_pin(p.PIN_26, Pull::None),
            Channel::new_pin(p.PIN_27, Pull::None),
            Channel::new_pin(p.PIN_28, Pull::None),
        ];
        let pins = [
            p.PIN_0.into(), p.PIN_1.into(), p.PIN_2.into(), p.PIN_3.into(), p.PIN_4.into(),
            p.PIN_5.into(), p.PIN_6.into(), p.PIN_7.into(), p.PIN_8.into(), p.PIN_9.into(),
            p.PIN_10.into(), p.PIN_11.into(), p.PIN_12.into(), p.PIN_13.into(), p.PIN_14.into(),
            p.PIN_15.into(), p.PIN_16.into(), p.PIN_17.into(), p.PIN_18.into(), p.PIN_19.into(),
            p.PIN_20.into(), p.PIN_21.into(), p.PIN_22.into(), p.PIN_24.into(), p.PIN_25.into(),
        ];
        let buses = Buses { spi0: p.SPI0, spi1: p.SPI1, i2c0: p.I2C0, i2c1: p.I2C1 };
        install_payload_hw(pins, p.ADC, analog, buses);
        bus.log(format_args!("heap: {} B free after boot", heap::free()));
    }

    Controller::new(svc, bus).run().await
}
