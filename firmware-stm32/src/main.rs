//! STM32 base firmware shell: BigTreeTech EBB42 CAN v1.2 (STM32G0B1CB).
//!
//! Serves the middleware serial protocol on the USB-C port (CDC-ACM,
//! PA11/PA12): `sys/info`, `sys/board`, `sys/manifest`; every other key is
//! `NotFound`. The decisions are in the pure core (`stm32_base`); this file
//! only brings up clocks and USB and moves frames.
//!
//! Clocks: HSI16 -> PLL -> 64 MHz SYSCLK; USB from HSI48 trimmed by the CRS
//! to the host's start-of-frame, so no crystal is assumed.
#![no_std]
#![no_main]
#![forbid(unsafe_code)]

use embassy_executor::Spawner;
use embassy_futures::join::join;
use embassy_stm32::peripherals::USB;
use embassy_stm32::rcc::{self, Hsi48Config, Pll, PllMul, PllPreDiv, PllRDiv, PllSource, Sysclk};
use embassy_stm32::usb::{self, Driver};
use embassy_stm32::{Config, bind_interrupts};
use embassy_time::{Duration, Timer, with_timeout};
use embassy_usb::Builder;
use embassy_usb::class::cdc_acm::{CdcAcmClass, Sender, State as CdcState};
use embedded_io_async::{ErrorKind, ErrorType, Write};
use middleware::serial::link::{Duplex, RECV_BUF_LEN, SerialLink};
use panic_halt as _;
use static_cell::StaticCell;
use stm32_base::board::{FIRMWARE, node_id};
use stm32_base::link::{RESPONSE_LEN, respond};
use stm32_base::requests::{Identity, NoApp, Node, System};
use stm32_base::usb::{MANUFACTURER, MAX_PACKET, PID, PRODUCT, VID, needs_zlp};

bind_interrupts!(struct Irqs {
    USB_UCPD1_2 => usb::InterruptHandler<USB>;
});

/// Longest one response write may take before it is abandoned (the host
/// is not reading); the link completes the frame on its next send.
const SEND_TIMEOUT: Duration = Duration::from_millis(200);

/// Pause after a stream error (USB not configured), so the loop cannot spin.
const ERROR_BACKOFF: Duration = Duration::from_millis(10);

type UsbDriver = Driver<'static, USB>;

/// A CDC write failed: the host has not configured the interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NotConnected;

impl core::fmt::Display for NotConnected {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("USB CDC not connected")
    }
}

impl core::error::Error for NotConnected {}

impl embedded_io_async::Error for NotConnected {
    fn kind(&self) -> ErrorKind {
        ErrorKind::NotConnected
    }
}

/// The CDC sender as a byte writer: one packet per `write`, and a
/// zero-length packet on `flush` after a full one ([`needs_zlp`]).
struct CdcWriter {
    tx: Sender<'static, UsbDriver>,
    last: usize,
}

impl ErrorType for CdcWriter {
    type Error = NotConnected;
}

impl Write for CdcWriter {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        let n = buf.len().min(usize::from(MAX_PACKET));
        let packet = buf.get(..n).unwrap_or_default();
        self.tx
            .write_packet(packet)
            .await
            .map_err(|_| NotConnected)?;
        self.last = n;
        Ok(n)
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        if needs_zlp(self.last, MAX_PACKET) {
            self.tx.write_packet(&[]).await.map_err(|_| NotConnected)?;
        }
        self.last = 0;
        Ok(())
    }
}

struct UsbMemory {
    config: [u8; 256],
    bos: [u8; 64],
    msos: [u8; 0],
    control: [u8; 64],
    rx: [u8; 256],
}

static USB_MEMORY: StaticCell<UsbMemory> = StaticCell::new();
static CDC_STATE: StaticCell<CdcState> = StaticCell::new();

fn clocks() -> Config {
    let mut config = Config::default();
    config.rcc.pll = Some(Pll {
        source: PllSource::HSI,
        prediv: PllPreDiv::DIV1,
        mul: PllMul::MUL8,
        divp: None,
        divq: None,
        divr: Some(PllRDiv::DIV2),
    });
    config.rcc.sys = Sysclk::PLL1_R;
    config.rcc.hsi48 = Some(Hsi48Config {
        sync_from_usb: true,
    });
    config.rcc.mux.usbsel = rcc::mux::Usbsel::HSI48;
    config
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let p = embassy_stm32::init(clocks());

    let driver = Driver::new(p.USB, Irqs, p.PA12, p.PA11);
    let mut config = embassy_usb::Config::new(VID, PID);
    config.manufacturer = Some(MANUFACTURER);
    config.product = Some(PRODUCT);
    config.serial_number = Some(embassy_stm32::uid::uid_hex());
    config.max_power = 100;
    config.max_packet_size_0 = 64;
    let mem = USB_MEMORY.init(UsbMemory {
        config: [0; 256],
        bos: [0; 64],
        msos: [],
        control: [0; 64],
        rx: [0; 256],
    });
    let mut builder = Builder::new(
        driver,
        config,
        &mut mem.config,
        &mut mem.bos,
        &mut mem.msos,
        &mut mem.control,
    );
    let class = CdcAcmClass::new(&mut builder, CDC_STATE.init(CdcState::new()), MAX_PACKET);
    let mut device = builder.build();
    let (tx, rx) = class.split();
    // No inbox: the loop reads with `next_frame` and never waits in a request.
    let mut link = SerialLink::<_, 0>::with_inbox(Duplex::new(
        rx.into_buffered(&mut mem.rx),
        CdcWriter { tx, last: 0 },
    ));

    let identity = Identity {
        firmware: FIRMWARE,
        node: node_id(embassy_stm32::uid::uid()),
    };
    let serve = async {
        let mut rx_buf = [0u8; RECV_BUF_LEN];
        let mut out = [0u8; RESPONSE_LEN];
        let mut node = Node {
            system: System::new(identity),
            app: NoApp,
        };
        loop {
            let frame = match link.next_frame(&mut rx_buf).await {
                Ok(frame) => frame,
                Err(_) => {
                    Timer::after(ERROR_BACKOFF).await;
                    continue;
                }
            };
            if let Some(response) = respond(&mut node, frame, &mut out) {
                let _ = with_timeout(SEND_TIMEOUT, link.send(&response)).await;
            }
        }
    };
    join(device.run(), serve).await;
}
