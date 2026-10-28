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
//! event queue. Both peripherals go back to [`HW`] whatever state they were
//! in: SPI through [`SpiBus::close`] (idle, payload transfer or sink-route
//! write in flight; see `esp_base::periph::dma_hw`), UART by value, so the
//! next payload can open them again.
//!
//! The controller awaits [`EspDevice::io`] alongside messages and ticks; it
//! completes SPI and UART work and pushes `DmaDone` / `UartRx` events.
//!
//! # Routes (ABI 4)
//!
//! The device keeps the payload's [`Routes`] (handles, keys, what each sink
//! owns); all decisions are in `esp_base::route`.
//!
//! * `route_source` (feature `espnow`): `TestPattern` only (`Camera` is
//!   `Unsupported` until the OV2640 driver lands); the key must travel over
//!   ESP-NOW (`app/...`, at most 64 bytes) and a chunk must fit one frame
//!   (≤ 167 data bytes), else `Unsupported`. The route is entered in the
//!   radio task's [`SOURCES`](super::radio::SOURCES) table, which sends it.
//! * `route_sink`: bus 0 only (`InvalidArgument` otherwise), `Busy` if the
//!   bus is not open or a payload transfer is in flight, or another sink
//!   owns the bus, pin or key. The D/C pin must be a payload GPIO the chip
//!   lets drive (`Unsupported` otherwise) and not routed to SPI or UART or
//!   armed for an interrupt (`InvalidArgument`); a pin the payload already
//!   drives as an output is taken over. While the route runs the payload's
//!   `spi_open` / `spi_transfer` on the bus and `gpio_*` / `gpio_irq` on
//!   the D/C pin are `Busy`, and `uart_open` cannot take the pin.
//! * The controller hands every message on a sink route's key to
//!   [`EspDevice::route_message`] instead of `on_message`. It steps the
//!   route, then for each transfer drives D/C and DMAs the bytes
//!   ([`SpiBus::write_route`]) from the received buffer, and raises
//!   `RouteFrames` / `RouteDropped`. A failed transfer stops the route
//!   (bus and pin released, `RouteStopped` with `Transport`).
//! * `route_stop` releases the route and purges its pending events;
//!   `route_stat` reads the sink's counters or the source's in the table.
//!   Dropping the device clears every route.

use core::cell::RefCell;

use embassy_futures::select::select;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::Instant;
use esp_base::chip::{Direction, Profile};
use esp_base::periph::claims::{Claims, pins};
use esp_base::route::sink::{SinkRoute, transfers};
use esp_base::route::table::{Kind, Routes};
use esp_base::route::stopped_event;
use middleware::Key;
use middleware::loader::is_system_key;
use middleware::wasm_abi::events::EventKind;
use wasm_host::{
    AbiError, Device, DmaChannel, Edge, Pull, RouteStat, SinkConfig, SourceConfig, SourceKind,
    SpiConfig, TimerSpec, UartConfig,
};

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
    routes: Routes,
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
        #[cfg(feature = "espnow")]
        super::radio::with_sources(|t| t.clear());
        Some(EspDevice { pins, hw, claims: Claims::new(), routes: Routes::new(), profile, bus })
    }

    fn spi(&mut self) -> Result<&mut SpiBus, AbiError> {
        self.hw.as_mut().and_then(|h| h.spi.as_mut()).ok_or(AbiError::Unsupported)
    }

    fn uart(&mut self) -> Result<&mut Uart1Port, AbiError> {
        self.hw.as_mut().and_then(|h| h.uart.as_mut()).ok_or(AbiError::Unsupported)
    }

    /// `Busy` while a sink route drives `pin` as its D/C pin.
    fn check_route_pin(&self, pin: u8) -> Result<(), AbiError> {
        if self.routes.owns_pin(pin) { Err(AbiError::Busy) } else { Ok(()) }
    }

    /// The sink route consuming messages on `key`, if any: such messages go
    /// to [`EspDevice::route_message`], not to the payload.
    pub fn sink_for(&self, key: Key<'_>) -> Option<u8> {
        self.routes.sink_for(key).map(|(h, _)| h)
    }

    /// Runs one message received on sink route `handle`'s key: steps the
    /// route, makes its bus transfers (D/C per segment) and raises its
    /// events. Bounded: at most `MAX_TRANSFERS` transfers, each bounded by
    /// the SPI route timeout.
    pub async fn route_message(&mut self, handle: u8, msg: &[u8]) {
        let Some((_, route)) = self.routes.get(handle).and_then(|e| match e.kind {
            Kind::Sink { route, .. } => Some((handle, route)),
            Kind::Source => None,
        }) else {
            return;
        };
        let (next, plan) = route.receive(msg);
        self.routes = self.routes.with_sink(handle, next);
        let cfg = *next.config();
        let mut result = Ok(());
        for segment in transfers(&cfg.preamble, &plan) {
            result = match self.pins.with(|b| b.write(cfg.dc_pin, segment.dc)) {
                Ok(()) => match self.spi() {
                    Ok(spi) => spi.write_route(segment.bytes).await,
                    Err(e) => Err(e),
                },
                Err(e) => Err(e),
            };
            if result.is_err() {
                break;
            }
        }
        let time_us = irq::now_us() as u32;
        match result {
            Ok(()) => {
                for event in plan.report().events(handle, time_us).into_iter().flatten() {
                    irq::push(event);
                }
            }
            Err(_) => {
                if self.release_route(handle).is_ok() {
                    irq::push(stopped_event(handle, AbiError::Transport, time_us));
                }
            }
        }
    }

    /// Frees route `handle`: a source leaves the radio table; a sink gives
    /// its bus and D/C pin back to the payload (the pin stays an output at
    /// its last level).
    fn release_route(&mut self, handle: u8) -> Result<(), AbiError> {
        let (routes, _entry) = self.routes.remove(handle)?;
        self.routes = routes;
        #[cfg(feature = "espnow")]
        if _entry.kind == Kind::Source {
            super::radio::with_sources(|t| t.stop(handle));
        }
        Ok(())
    }

    /// Drops a GPIO interrupt claim when the pin is reconfigured.
    fn reconfigure(&mut self, pin: u8) -> Result<(), AbiError> {
        self.check_route_pin(pin)?;
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
        #[cfg(feature = "espnow")]
        super::radio::with_sources(|t| t.clear());
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
        #[cfg(feature = "espnow")]
        if esp_base::radio::goes_over_air(key) {
            super::radio::send(key, data);
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
        self.check_route_pin(pin)?;
        self.claims.check_gpio(pin)?;
        self.pins.with(|b| b.write(pin, high))
    }

    fn gpio_read(&mut self, pin: u8) -> Result<bool, AbiError> {
        self.check_route_pin(pin)?;
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
        self.check_route_pin(pin)?;
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
        if self.routes.owns_bus(cfg.bus) {
            return Err(AbiError::Busy);
        }
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
        if self.routes.owns_bus(bus) {
            return Err(AbiError::Busy);
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
        if [cfg.tx, cfg.rx].into_iter().flatten().any(|p| self.routes.owns_pin(p)) {
            return Err(AbiError::Busy);
        }
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

    #[cfg(feature = "espnow")]
    fn route_source(&mut self, source: SourceKind, key: Key<'_>, cfg: SourceConfig) -> Result<u8, AbiError> {
        if source != SourceKind::TestPattern || !esp_base::radio::goes_over_air(key) {
            return Err(AbiError::Unsupported);
        }
        let route = esp_base::route::SourceRoute::new(cfg, now_ms())?;
        let route_key = esp_base::route::sources::RouteKey::from_key(key).map_err(|_| AbiError::Unsupported)?;
        let (routes, handle) = self.routes.add(key, Kind::Source)?;
        self.routes = routes;
        super::radio::with_sources(|t| t.start(handle, route_key, route));
        Ok(handle)
    }

    fn route_sink(&mut self, key: Key<'_>, bus: u8, cfg: &SinkConfig) -> Result<u8, AbiError> {
        if bus != 0 {
            return Err(AbiError::InvalidArgument);
        }
        let dc = cfg.dc_pin;
        self.profile.check_pin(dc, Direction::Output)?;
        if self.claims.owner(dc).is_some() {
            return Err(AbiError::InvalidArgument);
        }
        let spi = self.spi()?;
        if !spi.is_open() || spi.busy() {
            return Err(AbiError::Busy);
        }
        let (routes, handle) = self.routes.add(key, Kind::Sink { bus, route: SinkRoute::new(*cfg) })?;
        // Take the pin over as an output (it may already be one) at the
        // data level, ready for the first frame's preamble.
        self.pins.with(|b| {
            b.output(dc)?;
            b.write(dc, true)
        })?;
        self.routes = routes;
        Ok(handle)
    }

    fn route_stop(&mut self, handle: u8) -> Result<(), AbiError> {
        self.release_route(handle)?;
        let source = i32::from(handle);
        for kind in [EventKind::RouteFrames, EventKind::RouteDropped, EventKind::RouteStopped] {
            irq::purge(kind, source);
        }
        Ok(())
    }

    fn route_stat(&mut self, handle: u8, stat: RouteStat) -> Result<u32, AbiError> {
        let entry = self.routes.get(handle).ok_or(AbiError::Busy)?;
        let stats = match entry.kind {
            Kind::Sink { route, .. } => route.stats(),
            #[cfg(feature = "espnow")]
            Kind::Source => super::radio::read_sources(|t| t.stats(handle)).ok_or(AbiError::Busy)?,
            #[cfg(not(feature = "espnow"))]
            Kind::Source => return Err(AbiError::Busy),
        };
        Ok(stat.of(&stats))
    }
}
