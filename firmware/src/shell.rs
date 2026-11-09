//! Imperative shell: hardware bring-up and the tasks around the pure core.
//!
//! Boot sequence ([`run`]):
//!
//! 1. `esp_hal::init` at maximum CPU clock; heap regions per
//!    [`board::PROFILE`]; `esp-rtos` scheduler and embassy time driver on
//!    TIMG0.
//! 2. One link task ([`esp_base::embassy::link::serve`]) per serial port: UART0 at 115200 baud on every chip,
//!    plus USB-Serial-JTAG on ESP32-S3 and ESP32-C3. Nothing is printed as
//!    plain text after this point; logs are `sys/log` publications. Each
//!    link first writes one `0x00` delimiter to separate its frames from
//!    any ROM banner text.
//! 3. The partition table is read and the `payload` data partition located
//!    (see `partitions.csv`); the stored image header is CRC verified.
//! 4. With `wasm`, the payload peripherals come up: the GPIO interrupt
//!    handler, the timer alarm on TIMG1, SPI2 with DMA ([`spi`]) and UART1
//!    ([`uart1`]), all without pins until a payload opens them.
//! 5. The portable [`Controller`] from `esp_base::embassy`, instantiated
//!    with [`FlashStorage`], the [`Port`] set and (with `wasm`)
//!    [`device::EspDevice`] and [`irq::IrqEvents`], runs on the main task and
//!    starts the stored payload if one is valid (and the `wasm` runtime is
//!    built in).
//! 6. When the controller returns a `sys/reboot` mode (its reply already
//!    flushed), the chip is reset with `esp_hal::system::software_reset`.
//!    Only [`RebootSupport::Normal`] is advertised: no ROM download-mode
//!    entry is wired, so a bootloader request is answered `NotFound` by
//!    dispatch and never reaches the shell.
//!
//! Ownership is a tree: the main task owns the controller (state, flash
//! store, payload); each link task owns its port. They meet only through
//! the `'static` [`Bus`] channels. With `wasm`, the payload's
//! peripherals rest in statics shared with their ISRs or between runs: the
//! pin bank ([`gpio::PINS`]), the interrupt event queue ([`irq`]), the
//! timer alarm ([`timer`]) and the SPI/UART drivers ([`device::HW`]).

mod board;
#[cfg(feature = "wasm")]
mod device;
#[cfg(feature = "wasm")]
mod gpio;
#[cfg(feature = "wasm")]
mod irq;
#[cfg(feature = "wasm")]
mod spi;
#[cfg(feature = "wasm")]
mod timer;
#[cfg(feature = "wasm")]
mod uart1;

use embassy_executor::Spawner;
use esp_base::dispatch::{Context, RebootMode, RebootSupport, State};
use esp_base::embassy::bus::Port as PortSet;
use esp_base::embassy::link;
use esp_base::store::PayloadStore;
use esp_bootloader_esp_idf::partitions::{PARTITION_TABLE_MAX_LEN, read_partition_table};
use esp_hal::Async;
use esp_hal::clock::CpuClock;
use esp_hal::timer::timg::TimerGroup;
use esp_hal::uart::Uart;
use esp_storage::FlashStorage;
use middleware::serial::link::SerialLink;
use static_cell::ConstStaticCell;


#[cfg(any(feature = "esp32s3", feature = "esp32c3"))]
use esp_hal::usb::usb_serial_jtag::UsbSerialJtag;

/// Label of the payload data partition in `partitions.csv`.
const PAYLOAD_PARTITION: &str = "payload";

/// Firmware version reported in `sys/info`.
#[cfg(feature = "wasm")]
const FIRMWARE: &str = env!("CARGO_PKG_VERSION");
/// Firmware version reported in `sys/info`; the suffix marks a build
/// without the WASM runtime.
#[cfg(not(feature = "wasm"))]
const FIRMWARE: &str = concat!(env!("CARGO_PKG_VERSION"), "+no-wasm");

/// A serial port running the protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Port {
    /// UART0, behind the board's USB-serial bridge chip.
    Uart,
    /// The chip's native USB-Serial-JTAG (ESP32-S3, ESP32-C3).
    #[cfg_attr(feature = "esp32", allow(dead_code))]
    Usb,
}

impl PortSet for Port {
    const COUNT: usize = 2;
    fn index(self) -> usize {
        match self {
            Port::Uart => 0,
            Port::Usb => 1,
        }
    }
}

/// The message bus over the ESP port set.
pub type Bus = esp_base::embassy::bus::Bus<Port, { Port::COUNT }>;

/// The flash-backed payload store.
pub type Store = PayloadStore<FlashStorage<'static>>;

/// The controller over the ESP flash, ports and payload device.
#[cfg(feature = "wasm")]
type Controller = esp_base::embassy::controller::Controller<
    FlashStorage<'static>,
    Port,
    { Port::COUNT },
    device::EspDevice,
    irq::IrqEvents,
>;
/// The controller over the ESP flash and ports.
#[cfg(not(feature = "wasm"))]
type Controller =
    esp_base::embassy::controller::Controller<FlashStorage<'static>, Port, { Port::COUNT }>;

static BUS: ConstStaticCell<Bus> = ConstStaticCell::new(Bus::new());

#[embassy_executor::task]
async fn uart_link(mut uart: Uart<'static, Async>, bus: &'static Bus) {
    link::prime(&mut uart).await;
    link::serve(SerialLink::new(uart), Port::Uart, bus).await;
}

#[cfg(any(feature = "esp32s3", feature = "esp32c3"))]
#[embassy_executor::task]
async fn usb_link(mut usb: UsbSerialJtag<'static, Async>, bus: &'static Bus) {
    link::prime(&mut usb).await;
    link::serve(SerialLink::new(usb), Port::Usb, bus).await;
}

/// Opens the payload partition named [`PAYLOAD_PARTITION`].
fn open_store(mut flash: FlashStorage<'static>, limit: u32) -> Option<Store> {
    let mut table = [0u8; PARTITION_TABLE_MAX_LEN];
    let (offset, len) = {
        let parts = read_partition_table(&mut flash, &mut table).ok()?;
        let entry = parts.iter().find(|e| e.label_as_str() == PAYLOAD_PARTITION)?;
        (entry.offset(), entry.len())
    };
    PayloadStore::new(flash, offset, len, limit).ok()
}

/// Brings up the board and runs the firmware forever.
pub async fn run(spawner: Spawner) -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    board::init_heap();
    let board = board::split(peripherals);
    let timg0 = TimerGroup::new(board.timg0);
    esp_rtos::start(timg0.timer0, board.sw_int);

    let bus: &'static Bus = BUS.take();
    let profile = board::profile();

    let uart_ok = board.uart.is_ok();
    if let Ok(uart) = board.uart {
        spawner.spawn(uart_link(uart, bus).unwrap());
    }
    #[cfg(any(feature = "esp32s3", feature = "esp32c3"))]
    spawner.spawn(usb_link(board.usb, bus).unwrap());

    let mut store = open_store(FlashStorage::new(board.flash), profile.max_image);
    let installed = store.as_mut().map(|s| s.verify());
    let capacity = store.as_ref().map_or(0, |s| s.capacity());
    let state = State::boot(matches!(installed, Some(Ok(_))));
    let ctx = Context {
        chip: profile.name,
        firmware: FIRMWARE,
        capacity,
        runtime: cfg!(feature = "wasm"),
        reboot: RebootSupport::Normal,
    };

    bus.log(format_args!(
        "esp-base {FIRMWARE} on {} (heap {} B, WASM memory {} B, uart {})",
        profile.name,
        profile.heap_total(),
        profile.max_memory_bytes,
        if uart_ok { "ok" } else { "failed" },
    ));
    match installed {
        None => bus.log(format_args!("no '{PAYLOAD_PARTITION}' partition; loader disabled")),
        Some(Ok(h)) => bus.log(format_args!("stored payload: {} bytes, crc {:08x}", h.len, h.crc)),
        Some(Err(e)) => bus.log(format_args!("no valid stored payload: {e}")),
    }

    #[cfg(feature = "wasm")]
    let controller = {
        gpio::PINS.install(board.pins);
        gpio::init(board.payload.io_mux);
        timer::init(TimerGroup::new(board.payload.timg1).timer0);
        let spi = board.payload.spi.and_then(spi::SpiParts::new);
        let uart = board.payload.uart1.and_then(uart1::Uart1Parts::new);
        if spi.is_none() || uart.is_none() {
            bus.log(format_args!(
                "payload peripherals: spi {}, uart1 {}",
                if spi.is_some() { "ok" } else { "failed" },
                if uart.is_some() { "ok" } else { "failed" },
            ));
        }
        device::HW.put(device::Hw { spi, uart });
        Controller::new(state, ctx, store, bus, profile, irq::IrqEvents)
    };
    #[cfg(not(feature = "wasm"))]
    let controller = Controller::new(state, ctx, store, bus);
    match controller.run().await {
        // Dispatch only asks for modes in `ctx.reboot` (normal only).
        RebootMode::Normal | RebootMode::Bootloader => esp_hal::system::software_reset(),
    }
}
