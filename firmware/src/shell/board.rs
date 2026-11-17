//! Per-chip board bring-up: the compiled chip's [`Profile`], the heap, and
//! the split of `esp_hal` peripherals into the parts the shell uses, for
//! the [`Board`] the shell runs.
//!
//! This is the only module with per-chip `cfg`s besides `Cargo.toml`:
//!
//! | chip     | UART0 TX/RX | USB-Serial-JTAG | payload GPIOs (chip default)      | a board may release |
//! |----------|-------------|-----------------|-----------------------------------|---------------------|
//! | ESP32    | 1 / 3       | —               | 0 2 4 5 12–15 18 19 21–23 25–27 32–39 (34–39 input) | 16 17 |
//! | ESP32-S3 | 43 / 44     | yes             | 0–18 21 38–42 45–48               | 33–37               |
//! | ESP32-C3 | 21 / 20     | yes             | 0–11                              | —                   |
//!
//! The presence button ([`Hw::boot`], physical presence for re-keying) is
//! the board's [`Board::presence`] (by default the BOOT button: GPIO0 on
//! ESP32 and ESP32-S3, GPIO9 on ESP32-C3). With `wasm` it is sampled
//! through the pin bank; without (ESP32-C3) the shell owns it as a
//! pulled-up input.
//!
//! The pin lists name every pin a payload could ever get, including the
//! releasable ones. A pin the board's profile reserves is skipped before
//! its `Flex` is made, and before ADC1 enables it (either reconfigures the
//! pad, which must never happen to a flash or PSRAM pin or one the board
//! keeps), and [`PinBank`] drops any other pin the profile refuses, so the
//! pure tables in `crate::chip` and `crate::board` stay authoritative.
//!
//! With `wasm` (ESP32, ESP32-S3), [`PayloadPeripherals`] adds the payload
//! hardware: IO_MUX (GPIO interrupt handler), TIMG1 (payload timer alarm),
//! SPI2 with its DMA channel (`DMA_SPI2` on ESP32, GDMA `DMA_CH0` on
//! ESP32-S3), UART1, I2C0 (ABI 3 I2C bus 0, built without pins; its pins
//! are the profile's `i2c_pins(0)`: 26/27 on the ESP32, 5/6 on the
//! ESP32-S3), the LEDC (PWM) and ADC1 with every ADC1 pin enabled at 11 dB
//! (ESP32 GPIO32–39, ESP32-S3 GPIO1–10). ADC1 is built before the pin bank,
//! because building a `Flex` takes a pin back from the analog mux that
//! enabling it for the ADC selected (see `adc.rs`). On the ESP32 (the
//! ESP32-CAM) also [`CamPeripherals`]: I2S0 with `DMA_I2S0` for the camera
//! (R15), whose sensor's SCCB is the same I2C0 bus 0 the payload drives.
//! On both chips the RMT drives ABI 7 LED strips (channel 0).
//! With `espnow` (ESP32, ESP32-S3) the board also hands over `WIFI`.
//!
//! The heap ([`init_heap`]) is one allocator over the two regions of the
//! compiled chip's profile ([`PROFILE`]; a [`Board`] cannot change them,
//! they are statics); esp-radio's Wi-Fi driver allocates from it too, which the
//! profile budgets as `radio_heap`.

use crate::board::Board;
use crate::chip::Profile;
use esp_hal::Async;
use esp_hal::peripherals::{FLASH, FROM_CPU_INTR0, Peripherals, TIMG0};
use esp_hal::uart::{Config as UartConfig, ConfigError, Uart};

use super::presence::BootButton;

#[cfg(feature = "wasm")]
use {
    super::adc::{ATTENUATION, AdcUnit},
    super::gpio::PinBank,
    esp_hal::Blocking,
    esp_hal::analog::adc::{Adc, AdcConfig},
    esp_hal::gpio::Flex,
    esp_hal::i2c::master::I2c,
    esp_hal::peripherals::{IO_MUX, LEDC, TIMG1},
    esp_hal::spi::master::{Config as SpiConfig, Spi, SpiDma},
};

#[cfg(any(feature = "esp32s3", feature = "esp32c3"))]
use esp_hal::usb::usb_serial_jtag::UsbSerialJtag;

/// The compiled chip's profile.
#[cfg(feature = "esp32")]
pub const PROFILE: Profile = crate::chip::ESP32;
/// The compiled chip's profile.
#[cfg(feature = "esp32s3")]
pub const PROFILE: Profile = crate::chip::ESP32S3;
/// The compiled chip's profile.
#[cfg(feature = "esp32c3")]
pub const PROFILE: Profile = crate::chip::ESP32C3;

/// Protocol baud rate on UART0.
pub const BAUD: u32 = 115_200;

/// Registers the heap regions sized by the profile.
pub fn init_heap() {
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: PROFILE.heap_reclaimed);
    esp_alloc::heap_allocator!(size: PROFILE.heap_main);
}

/// Peripherals the shell uses, split per chip.
pub struct Hw {
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
    /// The presence button, for physical presence ([`super::presence`]).
    pub boot: BootButton,
    /// Pins available to payloads.
    #[cfg(feature = "wasm")]
    pub pins: PinBank,
    /// Peripherals behind the ABI 2 imports.
    #[cfg(feature = "wasm")]
    pub payload: PayloadPeripherals,
    /// The Wi-Fi radio, for ESP-NOW ([`super::radio`]).
    #[cfg(feature = "espnow")]
    pub wifi: esp_hal::peripherals::WIFI<'static>,
}

/// Peripherals behind the ABI 2 imports (interrupts, timers, SPI, UART) and
/// the ABI 3 PWM, ADC and I2C imports.
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
    /// The LED controller (PWM outputs; on the ESP32-CAM also the sensor
    /// clock).
    pub ledc: LEDC<'static>,
    /// ADC1 with every payload ADC1 pin enabled.
    pub adc: AdcUnit,
    /// I2C0 without pins (ABI 3 I2C bus 0; on the ESP32-CAM also the
    /// camera sensor's SCCB), blocking; `None` if configuration failed.
    pub i2c0: Option<I2c<'static, Blocking>>,
    /// The ESP32-CAM's camera control and capture peripherals (R15).
    #[cfg(feature = "esp32")]
    pub cam: CamPeripherals,
    /// The RMT, for ABI 7 LED strips (channel 0).
    pub rmt: esp_hal::peripherals::RMT<'static>,
}

/// ESP32: I2S0 with its DMA channel (the camera port). The sensor's SCCB is
/// I2C bus 0 ([`PayloadPeripherals::i2c0`]).
#[cfg(feature = "esp32")]
pub struct CamPeripherals {
    /// I2S0.
    pub i2s0: esp_hal::peripherals::I2S0<'static>,
    /// I2S0's DMA channel.
    pub dma_i2s0: esp_hal::peripherals::DMA_I2S0<'static>,
}

/// Builds the payload peripherals from SPI2, its DMA channel, UART1, I2C0,
/// the LEDC and the ADC1 unit built by [`adc1!`].
#[cfg(feature = "wasm")]
macro_rules! payload_peripherals {
    ($p:ident, $dma:ident, $adc:ident $(, $field:ident: $value:expr)?) => {
        PayloadPeripherals {
            io_mux: $p.IO_MUX,
            timg1: $p.TIMG1,
            spi: Spi::new($p.SPI2, SpiConfig::default()).ok().map(|s| s.with_dma($p.$dma)),
            uart1: Uart::new($p.UART1, UartConfig::default()).ok(),
            ledc: $p.LEDC,
            adc: $adc,
            i2c0: I2c::new($p.I2C0, Default::default()).ok(),
            rmt: $p.RMT,
            $($field: $value,)?
        }
    };
}

/// Builds ADC1 with `GPIOn` fields of `p` enabled at [`ATTENUATION`],
/// skipping pins `$board`'s profile reserves. The pins are only
/// reborrowed: the pin bank takes them next.
#[cfg(feature = "wasm")]
macro_rules! adc1 {
    ($p:ident, $board:ident; $($n:literal => $field:ident),* $(,)?) => {{
        let profile = $board.profile();
        let mut config = AdcConfig::new();
        $( if !profile.is_reserved($n) { let _ = config.enable_pin($p.$field.reborrow(), ATTENUATION); } )*
        AdcUnit::new(Adc::new($p.ADC1, config))
    }};
}

fn uart(
    uart0: esp_hal::peripherals::UART0<'static>,
    tx: impl esp_hal::gpio::interconnect::PeripheralOutput<'static>,
    rx: impl esp_hal::gpio::interconnect::PeripheralInput<'static>,
) -> Result<Uart<'static, Async>, ConfigError> {
    let config = UartConfig::default().with_baudrate(BAUD);
    Ok(Uart::new(uart0, config)?
        .with_tx(tx)
        .with_rx(rx)
        .into_async())
}

/// Builds a [`PinBank`] governed by `$board`'s profile from `GPIOn`
/// fields of `p`, skipping reserved pins before a `Flex` is made.
#[cfg(feature = "wasm")]
macro_rules! pin_bank {
    ($p:ident, $board:ident; $($n:literal => $field:ident),* $(,)?) => {{
        let profile = $board.profile();
        let mut bank = PinBank::new(profile);
        $( if !profile.is_reserved($n) { bank.insert($n, Flex::new($p.$field)); } )*
        bank
    }};
}

/// Splits the peripherals.
#[cfg(feature = "esp32")]
pub fn split(mut p: Peripherals, board: &'static Board) -> Hw {
    #[cfg(feature = "wasm")]
    let adc = adc1!(p, board;
        32 => GPIO32, 33 => GPIO33, 34 => GPIO34, 35 => GPIO35,
        36 => GPIO36, 37 => GPIO37, 38 => GPIO38, 39 => GPIO39,
    );
    #[cfg(feature = "wasm")]
    let pins = pin_bank!(p, board;
        0 => GPIO0, 2 => GPIO2, 4 => GPIO4, 5 => GPIO5,
        12 => GPIO12, 13 => GPIO13, 14 => GPIO14, 15 => GPIO15, 16 => GPIO16, 17 => GPIO17,
        18 => GPIO18, 19 => GPIO19, 21 => GPIO21, 22 => GPIO22, 23 => GPIO23,
        25 => GPIO25, 26 => GPIO26, 27 => GPIO27, 32 => GPIO32, 33 => GPIO33,
        34 => GPIO34, 35 => GPIO35, 36 => GPIO36, 37 => GPIO37, 38 => GPIO38, 39 => GPIO39,
    );
    Hw {
        timg0: p.TIMG0,
        sw_int: p.FROM_CPU_INTR0,
        uart: uart(p.UART0, p.GPIO1, p.GPIO3),
        flash: p.FLASH,
        boot: BootButton::in_bank(board.presence()),
        #[cfg(feature = "wasm")]
        pins,
        #[cfg(feature = "wasm")]
        payload: payload_peripherals!(p, DMA_SPI2, adc, cam: CamPeripherals {
            i2s0: p.I2S0,
            dma_i2s0: p.DMA_I2S0,
        }),
        #[cfg(feature = "espnow")]
        wifi: p.WIFI,
    }
}

/// Splits the peripherals.
#[cfg(feature = "esp32s3")]
pub fn split(mut p: Peripherals, board: &'static Board) -> Hw {
    #[cfg(feature = "wasm")]
    let adc = adc1!(p, board;
        1 => GPIO1, 2 => GPIO2, 3 => GPIO3, 4 => GPIO4, 5 => GPIO5,
        6 => GPIO6, 7 => GPIO7, 8 => GPIO8, 9 => GPIO9, 10 => GPIO10,
    );
    #[cfg(feature = "wasm")]
    let pins = pin_bank!(p, board;
        0 => GPIO0, 1 => GPIO1, 2 => GPIO2, 3 => GPIO3, 4 => GPIO4, 5 => GPIO5,
        6 => GPIO6, 7 => GPIO7, 8 => GPIO8, 9 => GPIO9, 10 => GPIO10, 11 => GPIO11,
        12 => GPIO12, 13 => GPIO13, 14 => GPIO14, 15 => GPIO15, 16 => GPIO16,
        17 => GPIO17, 18 => GPIO18, 21 => GPIO21, 33 => GPIO33, 34 => GPIO34,
        35 => GPIO35, 36 => GPIO36, 37 => GPIO37, 38 => GPIO38, 39 => GPIO39,
        40 => GPIO40, 41 => GPIO41, 42 => GPIO42, 45 => GPIO45, 46 => GPIO46,
        47 => GPIO47, 48 => GPIO48,
    );
    Hw {
        timg0: p.TIMG0,
        sw_int: p.FROM_CPU_INTR0,
        uart: uart(p.UART0, p.GPIO43, p.GPIO44),
        usb: UsbSerialJtag::new(p.USB_DEVICE).into_async(),
        flash: p.FLASH,
        boot: BootButton::in_bank(board.presence()),
        #[cfg(feature = "wasm")]
        pins,
        #[cfg(feature = "wasm")]
        payload: payload_peripherals!(p, DMA_CH0, adc),
        #[cfg(feature = "espnow")]
        wifi: p.WIFI,
    }
}

/// The pulled-up input on GPIO `$n` among the `GPIOn` fields of `p`;
/// `None` for a pin not listed.
#[cfg(all(feature = "esp32c3", not(feature = "wasm")))]
macro_rules! owned_input {
    ($p:ident, $n:expr; $($k:literal => $field:ident),* $(,)?) => {{
        let config = esp_hal::gpio::InputConfig::default().with_pull(esp_hal::gpio::Pull::Up);
        match $n {
            $( $k => Some(esp_hal::gpio::Input::new($p.$field, config)), )*
            _ => None,
        }
    }};
}

/// Splits the peripherals.
#[cfg(feature = "esp32c3")]
pub fn split(p: Peripherals, board: &'static Board) -> Hw {
    Hw {
        timg0: p.TIMG0,
        sw_int: p.FROM_CPU_INTR0,
        uart: uart(p.UART0, p.GPIO21, p.GPIO20),
        usb: UsbSerialJtag::new(p.USB_DEVICE).into_async(),
        flash: p.FLASH,
        // No runtime on ESP32-C3, so no pin bank: the shell owns the
        // presence pin (a payload pin, so one of 0–11).
        #[cfg(not(feature = "wasm"))]
        boot: BootButton::owned(owned_input!(p, board.presence();
            0 => GPIO0, 1 => GPIO1, 2 => GPIO2, 3 => GPIO3, 4 => GPIO4, 5 => GPIO5,
            6 => GPIO6, 7 => GPIO7, 8 => GPIO8, 9 => GPIO9, 10 => GPIO10, 11 => GPIO11,
        )),
        #[cfg(feature = "wasm")]
        boot: BootButton::in_bank(board.presence()),
        #[cfg(feature = "wasm")]
        pins: pin_bank!(p, board;
            0 => GPIO0, 1 => GPIO1, 2 => GPIO2, 3 => GPIO3, 4 => GPIO4, 5 => GPIO5,
            6 => GPIO6, 7 => GPIO7, 8 => GPIO8, 9 => GPIO9, 10 => GPIO10, 11 => GPIO11,
        ),
    }
}
