//! micro:bit v2 base firmware, milestone 1: the middleware serial protocol
//! over the DAPLink VCOM (nRF TX = P0.06, nRF RX = P1.08, 115200 8N1). Native
//! USB is not wired to the connector on v2, so the VCOM is the host link.
//!
//! Answers `sys/info`; every other request gets `NotFound`. The LED matrix
//! blinks row 1 column 1 as a heartbeat (row P0.21, column P0.28, active low).
#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_futures::join::join;
use embassy_nrf::gpio::{Level, Output, OutputDrive};
use embassy_nrf::{bind_interrupts, peripherals::UARTE0};
use embassy_nrf::uarte::{self, Uarte, UarteRxWithIdle};
use embassy_time::Timer;
use middleware::loader::{DeviceInfo, PayloadStatus};
use middleware::serial::frame::Frame;
use middleware::transport::reqrep::HandlerError;
use middleware::serial::link::{Duplex, SerialLink, RECV_BUF_LEN};
use middleware::wasm_abi::ABI_VERSION;
use middleware::loader::BoardInfo;
use middleware::SenderId;
use middleware::loader::manifest::{Link, LinkKind, Manifest};
use panic_halt as _;

bind_interrupts!(struct Irqs {
    UARTE0 => uarte::InterruptHandler<UARTE0>;
});

/// The receive half as an `embedded-io` reader. A frame arrives as one burst
/// at 115200 baud, and a byte-at-a-time DMA read loses bytes behind it, so
/// this reads until the line goes idle: each call returns the whole burst.
/// (embassy-nrf 0.11 gives the UART receive half no `embedded-io` `Read`.)
struct Rx<'d>(UarteRxWithIdle<'d>);

impl embedded_io_async::ErrorType for Rx<'_> {
    type Error = uarte::Error;
}

impl embedded_io_async::Read for Rx<'_> {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.0.read_until_idle(buf).await
    }
}

/// Chip name in `sys/info`; `mwctl` shows it and the HIL suite matches it.
const CHIP: &str = "nrf52833";
/// `+no-wasm`: this milestone has no payload runtime, so tools must not
/// offer `sys/payload/start` (mwctl reads the suffix).
const FIRMWARE: &str = concat!(env!("CARGO_PKG_VERSION"), "+no-wasm");

/// micro:bit v2 pin policy (board-pins): payload pins, reserved pins and
/// the console UART (TX P0.06 = 6, RX P1.08 = 40). No payload runtime.
fn board_info() -> BoardInfo<'static> {
    let pins = &board_pins::boards::MICROBIT;
    BoardInfo {
        abi_version: ABI_VERSION,
        max_image: 0,
        max_memory: 0,
        gpio: pins.gpio_mask(),
        reserved: pins.reserved_mask(),
        input_only: pins.input_only_mask(),
        strapping: 0,
        adc: pins.adc_mask(),
        pwm_max_bits: 0,
        flags: 0,
        uart_tx: 6,
        uart_rx: 40,
        pwm_pins: &[],
    }
}

/// The node id: the chip's FICR device id (the same role as the RP unique id).
fn node_id() -> SenderId {
    let ficr = embassy_nrf::pac::FICR;
    let lo = ficr.deviceid(0).read().to_le_bytes();
    let hi = ficr.deviceid(1).read().to_le_bytes();
    SenderId([lo[0], lo[1], lo[2], lo[3], hi[0], hi[1], hi[2], hi[3]])
}

/// `sys/manifest`: no payload runtime, one serial link (the VCOM).
const MANIFEST: Manifest = Manifest::new(ABI_VERSION)
    .without_runtime()
    .with_link(Link { id: 0, kind: LinkKind::SERIAL });

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let p = embassy_nrf::init(Default::default());

    let mut config = uarte::Config::default();
    config.baudrate = uarte::Baudrate::Baud115200;

    // TIMER0 and two PPI channels detect the idle gap at the end of a burst.
    let uart = Uarte::new(p.UARTE0, p.P1_08, p.P0_06, Irqs, config);
    let (tx, rx) = uart.split_with_idle(p.TIMER0, p.PPI_CH0, p.PPI_CH1);
    let mut link = SerialLink::new(Duplex::new(Rx(rx), tx));

    let mut row = Output::new(p.P0_21, Level::High, OutputDrive::Standard);
    let mut col = Output::new(p.P0_28, Level::Low, OutputDrive::Standard);
    let heartbeat = async {
        loop {
            col.set_low();
            Timer::after_millis(500).await;
            col.set_high();
            Timer::after_millis(500).await;
        }
    };
    let _ = &mut row;

    let serve = async {
        let mut rx_buf = [0u8; RECV_BUF_LEN];
        let mut body = [0u8; 256];
        loop {
            let Ok(frame) = link.next_frame(&mut rx_buf).await else {
                continue; // a corrupt or partial frame is skipped by the link
            };
            let Frame::Request { id, key, .. } = frame else {
                continue; // responses and publications: nothing to answer
            };
            let result = if key.as_str() == "sys/board" {
                match board_info().encode(&mut body) {
                    Some(n) => Ok(&body[..n]),
                    None => Err(HandlerError::BufferTooSmall),
                }
            } else if key.as_str() == "sys/manifest" {
                match MANIFEST.encode(&mut body) {
                    Some(n) => Ok(&body[..n]),
                    None => Err(HandlerError::BufferTooSmall),
                }
            } else if key.as_str() == "sys/info" {
                let info = DeviceInfo {
                    abi_version: ABI_VERSION,
                    capacity: 0,
                    status: PayloadStatus::Empty,
                    chip: CHIP,
                    firmware: FIRMWARE,
                    node: Some(node_id()),
                    caps: middleware::loader::Caps::NONE,
                };
                match info.encode(&mut body) {
                    Some(n) => Ok(&body[..n]),
                    None => Err(HandlerError::BufferTooSmall),
                }
            } else {
                Err(HandlerError::NotFound)
            };
            let _ = link.send(&Frame::Response { id, result }).await;
        }
    };

    join(serve, heartbeat).await;
}
