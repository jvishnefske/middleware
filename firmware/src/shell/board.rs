//! Per-chip board bring-up: the selected [`Profile`], the heap, and the
//! split of `esp_hal` peripherals into the parts the shell uses.
//!
//! This is the only module with per-chip `cfg`s besides `Cargo.toml`:
//!
//! | chip     | UART0 TX/RX | USB-Serial-JTAG | payload GPIOs                     |
//! |----------|-------------|-----------------|-----------------------------------|
//! | ESP32    | 1 / 3       | —               | 0 2 4 5 12–15 18 19 21–23 25–27 32–39 (34–39 input) |
//! | ESP32-S3 | 43 / 44     | yes             | 0–18 21 38–42 45–48               |
//! | ESP32-C3 | 21 / 20     | yes             | 0–11                              |
//!
//! The pin lists hand pins to the [`PinBank`], which drops any the profile
//! reserves, so the pure tables in `esp_base::chip` stay authoritative.
//!
//! With `wasm`, [`PayloadPeripherals`] adds the ABI 2 hardware: IO_MUX (GPIO
//! interrupt handler), TIMG1 (payload timer alarm), SPI2 with its DMA
//! channel (`DMA_SPI2` on ESP32, GDMA `DMA_CH0` on ESP32-S3) and UART1.

use esp_base::chip::Profile;
use esp_hal::Async;
use esp_hal::peripherals::{FLASH, FROM_CPU_INTR0, Peripherals, TIMG0};
use esp_hal::uart::{Config as UartConfig, ConfigError, Uart};

#[cfg(feature = "wasm")]
use {
    super::gpio::PinBank,
    esp_hal::gpio::Flex,
    esp_hal::peripherals::{IO_MUX, TIMG1},
    esp_hal::Blocking,
    esp_hal::spi::master::{Config as SpiConfig, Spi, SpiDma},
};

#[cfg(any(feature = "esp32s3", feature = "esp32c3"))]
use esp_hal::usb::usb_serial_jtag::UsbSerialJtag;

/// The selected chip's profile.
#[cfg(feature = "esp32")]
pub const PROFILE: Profile = esp_base::chip::ESP32;
/// The selected chip's profile.
#[cfg(feature = "esp32s3")]
pub const PROFILE: Profile = esp_base::chip::ESP32S3;
/// The selected chip's profile.
#[cfg(feature = "esp32c3")]
pub const PROFILE: Profile = esp_base::chip::ESP32C3;

static PROFILE_REF: Profile = PROFILE;

/// The selected chip's profile, by reference.
pub fn profile() -> &'static Profile {
    &PROFILE_REF
}

/// Protocol baud rate on UART0.
pub const BAUD: u32 = 115_200;

/// Registers the heap regions sized by the profile.
pub fn init_heap() {
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: PROFILE.heap_reclaimed);
    esp_alloc::heap_allocator!(size: PROFILE.heap_main);
}

/// Peripherals the shell uses, split per chip.
pub struct Board {
    /// Timer group driving the embassy time driver.
    pub timg0: TIMG0<'static>,
    /// Software interrupt used by the scheduler.
    pub sw_int: FROM_CPU_INTR0<'static>,
    /// UART0 at [`BAUD`] on the console pins.
    pub uart: Result<Uart<'static, Async>, ConfigError>,
    /// Native USB-Serial-JTAG.
    #[cfg(any(feature = "esp32s3", feature = "esp32c3"))]
    pub usb: UsbSerialJtag<'static, Async>,
    /// SPI flash.
    pub flash: FLASH<'static>,
    /// Pins available to payloads.
    #[cfg(feature = "wasm")]
    pub pins: PinBank,
    /// Peripherals behind the ABI 2 imports.
    #[cfg(feature = "wasm")]
    pub payload: PayloadPeripherals,
}

/// Peripherals behind the ABI 2 imports (interrupts, timers, SPI, UART).
#[cfg(feature = "wasm")]
pub struct PayloadPeripherals {
    /// IO MUX, for the GPIO interrupt handler.
    pub io_mux: IO_MUX<'static>,
    /// Timer group 1; timer 0 multiplexes the payload timers.
    pub timg1: TIMG1<'static>,
    /// SPI2 as a DMA master without pins (`DMA_SPI2` on ESP32, GDMA
    /// `DMA_CH0` on ESP32-S3), blocking until a payload runs; `None` if
    /// configuration failed.
    pub spi: Option<SpiDma<'static, Blocking>>,
    /// UART1 without pins, blocking until a payload runs; `None` if
    /// configuration failed.
    pub uart1: Option<Uart<'static, Blocking>>,
}

/// Builds the payload peripherals from SPI2, its DMA channel and UART1.
#[cfg(feature = "wasm")]
macro_rules! payload_peripherals {
    ($p:ident, $dma:ident) => {
        PayloadPeripherals {
            io_mux: $p.IO_MUX,
            timg1: $p.TIMG1,
            spi: Spi::new($p.SPI2, SpiConfig::default()).ok().map(|s| s.with_dma($p.$dma)),
            uart1: Uart::new($p.UART1, UartConfig::default()).ok(),
        }
    };
}

fn uart(
    uart0: esp_hal::peripherals::UART0<'static>,
    tx: impl esp_hal::gpio::interconnect::PeripheralOutput<'static>,
    rx: impl esp_hal::gpio::interconnect::PeripheralInput<'static>,
) -> Result<Uart<'static, Async>, ConfigError> {
    let config = UartConfig::default().with_baudrate(BAUD);
    Ok(Uart::new(uart0, config)?.with_tx(tx).with_rx(rx).into_async())
}

/// Builds a [`PinBank`] from `GPIOn` fields of `p`.
#[cfg(feature = "wasm")]
macro_rules! pin_bank {
    ($p:ident; $($n:literal => $field:ident),* $(,)?) => {{
        let mut bank = PinBank::new(profile());
        $( bank.insert($n, Flex::new($p.$field)); )*
        bank
    }};
}

/// Splits the peripherals.
#[cfg(feature = "esp32")]
pub fn split(p: Peripherals) -> Board {
    Board {
        timg0: p.TIMG0,
        sw_int: p.FROM_CPU_INTR0,
        uart: uart(p.UART0, p.GPIO1, p.GPIO3),
        flash: p.FLASH,
        #[cfg(feature = "wasm")]
        pins: pin_bank!(p;
            0 => GPIO0, 2 => GPIO2, 4 => GPIO4, 5 => GPIO5,
            12 => GPIO12, 13 => GPIO13, 14 => GPIO14, 15 => GPIO15,
            18 => GPIO18, 19 => GPIO19, 21 => GPIO21, 22 => GPIO22, 23 => GPIO23,
            25 => GPIO25, 26 => GPIO26, 27 => GPIO27, 32 => GPIO32, 33 => GPIO33,
            34 => GPIO34, 35 => GPIO35, 36 => GPIO36, 37 => GPIO37, 38 => GPIO38, 39 => GPIO39,
        ),
        #[cfg(feature = "wasm")]
        payload: payload_peripherals!(p, DMA_SPI2),
    }
}

/// Splits the peripherals.
#[cfg(feature = "esp32s3")]
pub fn split(p: Peripherals) -> Board {
    Board {
        timg0: p.TIMG0,
        sw_int: p.FROM_CPU_INTR0,
        uart: uart(p.UART0, p.GPIO43, p.GPIO44),
        usb: UsbSerialJtag::new(p.USB_DEVICE).into_async(),
        flash: p.FLASH,
        #[cfg(feature = "wasm")]
        pins: pin_bank!(p;
            0 => GPIO0, 1 => GPIO1, 2 => GPIO2, 3 => GPIO3, 4 => GPIO4, 5 => GPIO5,
            6 => GPIO6, 7 => GPIO7, 8 => GPIO8, 9 => GPIO9, 10 => GPIO10, 11 => GPIO11,
            12 => GPIO12, 13 => GPIO13, 14 => GPIO14, 15 => GPIO15, 16 => GPIO16,
            17 => GPIO17, 18 => GPIO18, 21 => GPIO21, 38 => GPIO38, 39 => GPIO39,
            40 => GPIO40, 41 => GPIO41, 42 => GPIO42, 45 => GPIO45, 46 => GPIO46,
            47 => GPIO47, 48 => GPIO48,
        ),
        #[cfg(feature = "wasm")]
        payload: payload_peripherals!(p, DMA_CH0),
    }
}

/// Splits the peripherals.
#[cfg(feature = "esp32c3")]
pub fn split(p: Peripherals) -> Board {
    Board {
        timg0: p.TIMG0,
        sw_int: p.FROM_CPU_INTR0,
        uart: uart(p.UART0, p.GPIO21, p.GPIO20),
        usb: UsbSerialJtag::new(p.USB_DEVICE).into_async(),
        flash: p.FLASH,
        #[cfg(feature = "wasm")]
        pins: pin_bank!(p;
            0 => GPIO0, 1 => GPIO1, 2 => GPIO2, 3 => GPIO3, 4 => GPIO4, 5 => GPIO5,
            6 => GPIO6, 7 => GPIO7, 8 => GPIO8, 9 => GPIO9, 10 => GPIO10, 11 => GPIO11,
        ),
    }
}
