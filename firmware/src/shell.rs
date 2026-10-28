//! Imperative shell: hardware bring-up and the tasks around the pure core.
//!
//! Boot sequence ([`run`]):
//!
//! 1. `esp_hal::init` at maximum CPU clock; heap regions per
//!    [`board::PROFILE`]; `esp-rtos` scheduler and embassy time driver on
//!    TIMG0.
//! 2. One [`link`] task per serial port: UART0 at 115200 baud on every chip,
//!    plus USB-Serial-JTAG on ESP32-S3 and ESP32-C3. Nothing is printed as
//!    plain text after this point; logs are `sys/log` publications. Each
//!    link first writes one `0x00` delimiter to separate its frames from
//!    any ROM banner text.
//! 3. The partition table is read and the `payload` and `trust` data
//!    partitions located (see `partitions.csv`), and the SPI flash mode and
//!    clock read from the bootloader's image header logged
//!    ([`esp_base::chip::FlashSettings`]: code runs from flash through the
//!    cache at that speed); the stored image header is
//!    CRC verified and the pinned signing key, if any, read
//!    ([`esp_base::trust_store`]). The [`presence`] task starts watching the
//!    BOOT button for the re-key window.
//! 4. With `wasm`, the payload peripherals come up: the GPIO interrupt
//!    handler, the timer alarm on TIMG1, SPI2 with DMA ([`spi`]) and UART1
//!    ([`uart1`]), all without pins until a payload opens them.
//! 5. With `espnow` (ESP32, ESP32-S3), Wi-Fi comes up in station mode on
//!    the fixed ESP-NOW channel and the [`radio`] task starts: `app/...`
//!    publishes go out over ESP-NOW, `app/...` samples come in, and source
//!    routes stream from it. A radio that fails to start is logged and the
//!    firmware runs without it.
//! 6. The [`controller`] runs on the main task and starts the stored payload
//!    if one is valid (and the `wasm` runtime is built in).
//!
//! Ownership is a tree: the main task owns the controller (state, flash
//! store, trust store, payload); each link task owns its port. They meet
//! only through the `'static` [`bus::Bus`] channels; the presence task and
//! the controller share the re-key window through [`presence::WINDOW`]. With `wasm`, the payload's
//! peripherals rest in statics shared with their ISRs or between runs: the
//! pin bank ([`gpio::PINS`]), the interrupt event queue ([`irq`]), the
//! timer alarm ([`timer`]) and the SPI/UART drivers ([`device::HW`]).


mod board;
mod bus;
mod controller;
#[cfg(feature = "wasm")]
mod device;
#[cfg(feature = "wasm")]
mod gpio;
#[cfg(feature = "wasm")]
mod irq;
mod link;
mod presence;
#[cfg(feature = "espnow")]
mod radio;
#[cfg(feature = "wasm")]
mod spi;
#[cfg(feature = "wasm")]
mod timer;
#[cfg(feature = "wasm")]
mod uart1;

use embassy_executor::Spawner;
use esp_base::chip::FlashSettings;
use esp_base::dispatch::{Context, State};
use esp_base::store::PayloadStore;
use esp_base::trust_store::TrustStore;
use middleware::loader::trust::TrustState;
use esp_bootloader_esp_idf::partitions::{PARTITION_TABLE_MAX_LEN, read_partition_table};
use esp_hal::Async;
use esp_hal::clock::CpuClock;
use esp_hal::timer::timg::TimerGroup;
use esp_hal::uart::Uart;
use esp_storage::FlashStorage;
use middleware::serial::link::SerialLink;
use static_cell::ConstStaticCell;

use bus::{Bus, Port};
use controller::{Controller, Store};

#[cfg(any(feature = "esp32s3", feature = "esp32c3"))]
use esp_hal::usb::usb_serial_jtag::UsbSerialJtag;

/// Label of the payload data partition in `partitions.csv`.
const PAYLOAD_PARTITION: &str = "payload";
/// Label of the trust data partition in `partitions.csv`.
const TRUST_PARTITION: &str = "trust";

/// Firmware version reported in `sys/info`.
#[cfg(feature = "wasm")]
const FIRMWARE: &str = env!("CARGO_PKG_VERSION");
/// Firmware version reported in `sys/info`; the suffix marks a build
/// without the WASM runtime.
#[cfg(not(feature = "wasm"))]
const FIRMWARE: &str = concat!(env!("CARGO_PKG_VERSION"), "+no-wasm");

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

/// What [`open_storage`] found on flash.
struct Storage {
    store: Option<Store>,
    /// The trust partition, when present and readable.
    trust: Option<TrustStore>,
    /// The persisted root of trust (Unclaimed without a trust partition).
    state: TrustState,
}

/// Opens the payload partition named [`PAYLOAD_PARTITION`] and the trust
/// partition named [`TRUST_PARTITION`]. The trust record is read before the
/// flash moves into the payload store, which then lends it back for trust
/// saves ([`PayloadStore::flash`]).
fn open_storage(mut flash: FlashStorage<'static>, limit: u32) -> Storage {
    let none = Storage { store: None, trust: None, state: TrustState::Unclaimed };
    let mut table = [0u8; PARTITION_TABLE_MAX_LEN];
    let (payload, trust) = {
        let Ok(parts) = read_partition_table(&mut flash, &mut table) else { return none };
        let find = |label: &str| {
            parts.iter().find(|e| e.label_as_str() == label).map(|e| (e.offset(), e.len()))
        };
        (find(PAYLOAD_PARTITION), find(TRUST_PARTITION))
    };
    let (trust, state) = match trust.map(|(at, len)| TrustStore::open(&mut flash, at, len)) {
        Some(Ok((t, s))) => (Some(t), s),
        _ => (None, TrustState::Unclaimed),
    };
    let store = payload.and_then(|(at, len)| PayloadStore::new(flash, at, len, limit).ok());
    Storage { store, trust, state }
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

    let mut flash = FlashStorage::new(board.flash);
    // The bootloader's image header: the flash mode and clock code runs
    // from (read before the flash moves into the payload store).
    let mut header = [0u8; 4];
    let flash_settings = flash.read(0, &mut header).ok().and_then(|()| FlashSettings::from_header(header));
    let Storage { mut store, trust, state: trust_state } = open_storage(flash, profile.max_image);
    let installed = store.as_mut().map(|s| s.verify());
    let capacity = store.as_ref().map_or(0, |s| s.capacity());
    let state = State::boot(matches!(installed, Some(Ok(_)))).with_trust(trust_state);
    let ctx = Context {
        chip: profile.name,
        firmware: FIRMWARE,
        capacity,
        runtime: cfg!(feature = "wasm"),
        rekey_window: false,
    };

    bus.log(format_args!(
        "esp-base {FIRMWARE} on {} (heap {} B, WASM memory {} B, radio {} B, uart {})",
        profile.name,
        profile.heap_total(),
        profile.max_memory_bytes,
        profile.radio_heap,
        if uart_ok { "ok" } else { "failed" },
    ));
    match flash_settings {
        Some(s) => bus.log(format_args!("flash: {s} (bootloader header)")),
        None => bus.log(format_args!("flash: mode unknown (header {header:02x?})")),
    }
    match installed {
        None => bus.log(format_args!("no '{PAYLOAD_PARTITION}' partition; loader disabled")),
        Some(Ok(h)) => bus.log(format_args!("stored payload: {} bytes, crc {:08x}", h.len, h.crc)),
        Some(Err(e)) => bus.log(format_args!("no valid stored payload: {e}")),
    }
    match (&trust, trust_state) {
        (None, _) => bus.log(format_args!("no '{TRUST_PARTITION}' partition; unsigned payloads only (TOFU)")),
        (Some(_), TrustState::Unclaimed) => {
            bus.log(format_args!("trust: unclaimed; any payload accepted until sys/trust/claim"))
        }
        (Some(_), TrustState::Claimed { key, min_version }) => bus.log(format_args!(
            "trust: claimed by key {:02x}{:02x}{:02x}{:02x}..., min_version {}",
            key.0[0], key.0[1], key.0[2], key.0[3], min_version.0
        )),
    }
    spawner.spawn(presence::watch(board.boot, bus).unwrap());

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
        #[cfg(feature = "espnow")]
        match radio::start(board.wifi) {
            Ok(transport) => spawner.spawn(radio::radio_task(transport, bus).unwrap()),
            Err(e) => bus.log(format_args!("espnow: radio start failed: {e:?}")),
        }
        bus.log(format_args!("heap: {} B free after boot", esp_alloc::HEAP.free()));
        Controller::new(state, ctx, store, trust, bus, profile)
    };
    #[cfg(not(feature = "wasm"))]
    let controller = Controller::new(state, ctx, store, trust, bus);
    controller.run().await
}
