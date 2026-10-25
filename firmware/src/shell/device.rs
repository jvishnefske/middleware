//! The board as a payload sees it: the [`wasm_host::Device`] implementation.
//!
//! [`EspDevice`] holds, for one payload run:
//!
//! * the [`PinLease`] on the static pin bank (GPIO, interrupts, routing);
//! * the SPI bus 0 (DMA) and UART 1 drivers, taken from the static [`HW`]
//!   home as blocking-mode [`Hw`] parts and turned async for the run, then
//!   returned on drop, because `wasm_host::Payload` needs a `'static`
//!   device and drops it inside a failed load (esp-hal's async drivers are
//!   not `Send`, so only the blocking parts can rest in a static);
//! * the pure [`Claims`] table that decides which pins SPI, UART and GPIO
//!   interrupts may hold.
//!
//! The four payload timers ([`super::timer`]) and the event queue
//! ([`super::irq`]) are firmware-wide statics shared with their ISRs.
//!
//! Dropping the device (payload stop, trap, reload, failed load) stops all
//! timers, cancels SPI DMA and UART transmit, closes both peripherals,
//! releases every pin (interrupts off, signals detached) and clears the
//! event queue.
//!
//! The controller awaits [`EspDevice::io`] alongside messages and ticks; it
//! completes SPI and UART work and pushes `DmaDone` / `UartRx` events.

use core::cell::RefCell;

use embassy_futures::select::select;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::Instant;
use esp_base::chip::Profile;
use esp_base::periph::claims::{Claims, pins};
use middleware::Key;
use middleware::loader::is_system_key;
use wasm_host::{AbiError, Device, DmaChannel, Edge, Pull, SpiConfig, TimerSpec, UartConfig};

use super::bus::Bus;
use super::gpio::{PINS, PinBank, PinLease};
use super::spi::{self, SpiBus, SpiParts};
use super::uart1::{self, Uart1Parts, Uart1Port};
use super::{irq, timer};

/// Idle peripherals a payload drives besides pins and timers.
pub struct Hw {
    /// SPI bus 0, if its driver came up at boot.
    pub spi: Option<SpiParts>,
    /// UART 1, if its driver came up at boot.
    pub uart: Option<Uart1Parts>,
}

/// The same peripherals, async, during a payload run.
struct Live {
    spi: Option<SpiBus>,
    uart: Option<Uart1Port>,
}

/// Where [`Hw`] rests between payload runs.
pub struct HwHome(Mutex<CriticalSectionRawMutex, RefCell<Option<Hw>>>);

/// The firmware's payload peripherals.
pub static HW: HwHome = HwHome(Mutex::new(RefCell::new(None)));

impl HwHome {
    /// Stores `hw` (at boot, and when a device is dropped).
    pub fn put(&self, hw: Hw) {
        self.0.lock(|c| {
            if let Ok(mut slot) = c.try_borrow_mut() {
                *slot = Some(hw);
            }
        });
    }

    fn take(&self) -> Option<Hw> {
        self.0.lock(|c| c.try_borrow_mut().ok().and_then(|mut s| s.take()))
    }
}

/// Milliseconds since boot, wrapping.
pub fn now_ms() -> u32 {
    Instant::now().as_millis() as u32
}

/// The board as a payload sees it.
pub struct EspDevice {
    pins: PinLease,
    hw: Option<Live>,
    claims: Claims,
    profile: &'static Profile,
    bus: &'static Bus,
}

impl EspDevice {
    /// A device for one payload run; `None` while another device holds the
    /// pins.
    pub fn new(profile: &'static Profile, bus: &'static Bus) -> Option<Self> {
        let pins = PINS.lease()?;
        irq::clear();
        let hw = HW.take().map(|h| Live { spi: h.spi.map(SpiBus::new), uart: h.uart.map(Uart1Port::new) });
        Some(EspDevice { pins, hw, claims: Claims::new(), profile, bus })
    }

    fn spi(&mut self) -> Result<&mut SpiBus, AbiError> {
        self.hw.as_mut().and_then(|h| h.spi.as_mut()).ok_or(AbiError::Unsupported)
    }

    fn uart(&mut self) -> Result<&mut Uart1Port, AbiError> {
        self.hw.as_mut().and_then(|h| h.uart.as_mut()).ok_or(AbiError::Unsupported)
    }

    /// Drops a GPIO interrupt claim when the pin is reconfigured.
    fn reconfigure(&mut self, pin: u8) -> Result<(), AbiError> {
        self.claims.check_gpio(pin)?;
        self.claims = self.claims.set_irq(self.profile, pin, false)?;
        Ok(())
    }

    /// Completes SPI or UART work; pending forever when none is running.
    pub async fn io(&mut self) {
        match self.hw.as_mut() {
            Some(Live { spi, uart }) => {
                let spi_done = async {
                    match spi {
                        Some(s) => s.completion().await,
                        None => core::future::pending().await,
                    }
                };
                let uart_step = async {
                    match uart {
                        Some(u) => u.io().await,
                        None => core::future::pending().await,
                    }
                };
                select(spi_done, uart_step).await;
            }
            None => core::future::pending().await,
        }
    }
}

impl Drop for EspDevice {
    fn drop(&mut self) {
        timer::stop_all();
        if let Some(live) = self.hw.take() {
            HW.put(Hw { spi: live.spi.and_then(SpiBus::close), uart: live.uart.map(Uart1Port::close) });
        }
        irq::clear();
        // `pins` drops next and releases every pin.
    }
}

/// Re-routes the pins of one peripheral: pins in `old` but not `new` go
/// back to plain GPIO, then `route` connects the new ones.
fn reroute(
    bank: &mut PinBank,
    old: u64,
    new: u64,
    route: impl FnOnce(&mut PinBank) -> Result<(), AbiError>,
) -> Result<(), AbiError> {
    for p in pins(old & !new) {
        bank.unroute(p);
    }
    route(bank)
}

impl Device for EspDevice {
    fn log(&mut self, line: &str) {
        self.bus.log(format_args!("{line}"));
    }

    fn publish(&mut self, key: Key<'_>, data: &[u8]) -> Result<(), AbiError> {
        if is_system_key(key) {
            return Err(AbiError::InvalidKey);
        }
        self.bus.publish(key, data).map_err(|_| AbiError::Transport)
    }

    fn now_ms(&mut self) -> u32 {
        now_ms()
    }

    fn gpio_output(&mut self, pin: u8) -> Result<(), AbiError> {
        self.reconfigure(pin)?;
        self.pins.with(|b| b.output(pin))
    }

    fn gpio_input(&mut self, pin: u8, pull: Pull) -> Result<(), AbiError> {
        self.reconfigure(pin)?;
        self.pins.with(|b| b.input(pin, pull))
    }

    fn gpio_write(&mut self, pin: u8, high: bool) -> Result<(), AbiError> {
        self.claims.check_gpio(pin)?;
        self.pins.with(|b| b.write(pin, high))
    }

    fn gpio_read(&mut self, pin: u8) -> Result<bool, AbiError> {
        self.claims.check_gpio(pin)?;
        self.pins.with(|b| b.read(pin))
    }

    fn now_us(&mut self) -> u32 {
        irq::now_us() as u32
    }

    fn irq_overflow(&mut self) -> u32 {
        irq::take_overflow()
    }

    fn gpio_irq(&mut self, pin: u8, edge: Edge) -> Result<(), AbiError> {
        let next = self.claims.set_irq(self.profile, pin, edge != Edge::Off)?;
        self.pins.with(|b| b.set_irq(pin, edge))?;
        self.claims = next;
        Ok(())
    }

    fn timer_start(&mut self, spec: TimerSpec) -> Result<(), AbiError> {
        timer::start(spec)
    }

    fn timer_stop(&mut self, id: u8) -> Result<(), AbiError> {
        timer::stop(id)
    }

    fn spi_open(&mut self, cfg: SpiConfig) -> Result<(), AbiError> {
        let next = self.claims.claim_spi(self.profile, &cfg)?;
        let old = self.claims.spi_pins();
        let bus = self.spi()?;
        if bus.busy() {
            return Err(AbiError::Busy);
        }
        bus.open(&cfg)?;
        self.pins.with(|b| {
            reroute(b, old, next.spi_pins(), |b| {
                b.route_out(cfg.sck, spi::SCK)?;
                if let Some(p) = cfg.mosi {
                    b.route_out(p, spi::MOSI)?;
                }
                match cfg.miso {
                    Some(p) => b.route_in(p, spi::MISO)?,
                    None => spi::MISO.connect_to(&esp_hal::gpio::Level::Low),
                }
                match cfg.cs {
                    Some(p) => b.route_out(p, spi::CS),
                    None => Ok(()),
                }
            })
        })?;
        self.claims = next;
        Ok(())
    }

    fn spi_transfer(&mut self, bus: u8, tx: &[u8], len: usize) -> Result<(), AbiError> {
        if bus != 0 {
            return Err(AbiError::InvalidArgument);
        }
        self.spi()?.transfer(tx, len)
    }

    fn dma_read(&mut self, channel: DmaChannel, out: &mut [u8]) -> Result<usize, AbiError> {
        match channel {
            DmaChannel::Spi0 => self.spi()?.read(out),
            // Transmit only: nothing to read back.
            DmaChannel::Uart1Tx => Ok(0),
        }
    }

    fn uart_open(&mut self, cfg: UartConfig) -> Result<(), AbiError> {
        let next = self.claims.claim_uart(self.profile, &cfg)?;
        let old = self.claims.uart_pins();
        let port = self.uart()?;
        if port.busy() {
            return Err(AbiError::Busy);
        }
        port.open(&cfg)?;
        self.pins.with(|b| {
            reroute(b, old, next.uart_pins(), |b| {
                if let Some(p) = cfg.tx {
                    b.route_out(p, uart1::TXD)?;
                }
                match cfg.rx {
                    Some(p) => b.route_in(p, uart1::RXD),
                    None => Ok(()),
                }
            })
        })?;
        self.claims = next;
        Ok(())
    }

    fn uart_write(&mut self, port: u8, data: &[u8]) -> Result<(), AbiError> {
        self.uart()?.write(port, data)
    }

    fn uart_read(&mut self, port: u8, out: &mut [u8]) -> Result<usize, AbiError> {
        self.uart()?.read(port, out)
    }
}
