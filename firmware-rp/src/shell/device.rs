//! The board as a payload sees it: `wasm_host::Device` over the Pico's
//! GPIO, ADC, SPI and I2C controllers, clock and the message bus (`wasm`
//! only).
//!
//! | ABI call | RP base |
//! |----------|---------|
//! | `log`, `publish` | `sys/log` and publications on the bus (`sys/...` keys refused) |
//! | `now_ms`, `now_us` | embassy-time (1 MHz timer) |
//! | `gpio_output`, `gpio_input`, `gpio_write`, `gpio_read` | `Flex` pins, [`rp_base::pins`] rules; `Busy` on a pin a bus holds |
//! | `timer_start`, `timer_stop` | four software timers (`esp_base::periph::timers`) fired by the controller loop as `Timer` events |
//! | `irq_overflow` | the event queue's drop count |
//! | `adc_read` | GPIO 26–28, ADC0–2, blocking 12-bit read |
//! | `spi_open`, `spi_transfer`, `dma_read` (`Spi0`) | bus 0 on SPI0 or SPI1 (the pins choose, [`rp_base::periph::func`]), FIFO pumped by the controller loop, `DmaDone` on completion ([`super::spi`]) |
//! | `i2c_open`, `i2c_transfer` | buses 0 (SDA 4 / SCL 5) and 1 (SDA 6 / SCL 7), 10 kHz–1 MHz, blocking and time bounded ([`super::i2c`]) |
//! | `serve` (ABI 5) | accepted for any non-`sys` key (`esp_base::serve::servable`); wasm-host keeps the live table, the controller routes its requests ([`rp_base::requests`]) |
//! | `node_id` (ABI 5) | the chip unique id, the node `sys/info` reports |
//! | `gpio_irq`, `uart_*`, `dma_read` (`Uart1Tx`), `pwm_*`, `route_*` | `Unsupported` |
//!
//! Pins a bus holds are decided by [`rp_base::periph::claims`]: a bus
//! cannot take a pin another bus holds or one the payload configured as
//! GPIO, and `gpio_*` on a bus pin is `Busy`, until the payload stops.
//!
//! The hardware ([`Hw`]: the pin bank, the ADC, the SPI and I2C
//! controllers) rests in [`HW`] between runs. [`RpDevice::new`] takes it
//! and `Drop` puts it back with both buses closed (controllers in reset,
//! a running SPI transfer abandoned) and every pin the payload configured
//! or a bus held returned to an input without pull, so a payload that
//! fails to instantiate, traps or is stopped leaves the board as it found
//! it.

use core::cell::RefCell;

use embassy_futures::yield_now;
use embassy_rp::adc::{Adc, Channel};
use embassy_rp::gpio::{Flex, Level, Pull as HalPull};
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::Instant;
use esp_base::periph::timers::Timers;
use esp_base::periph::{dma_done_event, timer_event};
use middleware::loader::is_system_key;
use middleware::{Key, SenderId};
use middleware::wasm_abi::events::{DmaChannel, Event, EventKind, EventQueue, MAX_EVENTS};
use middleware::wasm_abi::periph::{I2cConfig, SpiConfig, TimerSpec};
use middleware::wasm_abi::{AbiError, Pull};
use rp_base::chip::Profile;
use rp_base::periph::claims::{Claims, Owner, pins};
use rp_base::periph::func::SpiRoute;
use rp_base::pins::{GPIOS, Pins, ReadFrom};

use super::bus::Bus;
use super::i2c::I2cBuses;
use super::pads::{self, Function};
use super::spi::SpiBus;

/// Microseconds since boot.
pub fn now_us() -> u64 {
    Instant::now().as_micros()
}

/// Milliseconds since boot, wrapping.
pub fn now_ms() -> u32 {
    Instant::now().as_millis() as u32
}

/// ADC0–ADC2 on GPIO 26–28.
pub struct Analog {
    adc: Adc<'static, embassy_rp::adc::Blocking>,
    channels: [Channel<'static>; 3],
}

impl Analog {
    /// The converter and its three pin channels.
    pub fn new(adc: Adc<'static, embassy_rp::adc::Blocking>, channels: [Channel<'static>; 3]) -> Self {
        Analog { adc, channels }
    }
}

/// The payload hardware.
pub struct Hw {
    /// The pins a payload may use as GPIO (`None` for the others).
    pub pins: [Option<Flex<'static>>; GPIOS],
    /// The ADC.
    pub adc: Analog,
    /// SPI bus 0 (SPI0 and SPI1).
    pub spi: SpiBus,
    /// I2C buses 0 and 1.
    pub i2c: I2cBuses,
}

/// Where [`Hw`] rests while no payload runs.
pub struct HwSlot(Mutex<CriticalSectionRawMutex, RefCell<Option<Hw>>>);

impl HwSlot {
    /// Empty.
    pub const fn new() -> Self {
        HwSlot(Mutex::new(RefCell::new(None)))
    }

    /// Stores the hardware.
    pub fn put(&self, hw: Hw) {
        self.0.lock(|c| c.replace(Some(hw)));
    }

    /// Takes the hardware, if it is there.
    pub fn take(&self) -> Option<Hw> {
        self.0.lock(|c| c.take())
    }
}

/// The payload hardware between runs.
pub static HW: HwSlot = HwSlot::new();

/// The device a payload runs against.
pub struct RpDevice {
    hw: Option<Hw>,
    modes: Pins,
    claims: Claims,
    timers: Timers,
    events: EventQueue<MAX_EVENTS>,
    bus: &'static Bus,
    profile: &'static Profile,
    node: SenderId,
}

impl RpDevice {
    /// Takes the hardware from [`HW`]; `None` if it is missing. `node` is
    /// what `node_id` answers (the node `sys/info` reports).
    pub fn new(bus: &'static Bus, profile: &'static Profile, node: SenderId) -> Option<Self> {
        let hw = HW.take()?;
        Some(RpDevice {
            hw: Some(hw),
            modes: Pins::new(),
            claims: Claims::new(),
            timers: Timers::new(),
            events: EventQueue::new(),
            bus,
            profile,
            node,
        })
    }

    /// When the earliest running timer is due, in µs since boot.
    pub fn next_alarm_us(&self) -> Option<u64> {
        self.timers.next_deadline()
    }

    /// Fires the timers due at `now` (µs) into the event queue.
    pub fn fire_timers(&mut self, now: u64) {
        let (next, fired) = self.timers.expire(now);
        self.timers = next;
        for f in fired {
            // A full queue drops and counts the event.
            let _ = self.events.push(timer_event(f.id, f.periods, now as u32));
        }
    }

    /// The next queued event.
    pub fn pop_event(&mut self) -> Option<Event> {
        self.events.pop()
    }

    /// Events dropped so far (not reset).
    pub fn overflow(&self) -> u32 {
        self.events.overflow()
    }

    fn pin(&mut self, pin: u8) -> Result<&mut Flex<'static>, AbiError> {
        self.hw
            .as_mut()
            .and_then(|hw| hw.pins.get_mut(usize::from(pin)))
            .and_then(Option::as_mut)
            .ok_or(AbiError::Unsupported)
    }

    fn hw(&mut self) -> Result<&mut Hw, AbiError> {
        self.hw.as_mut().ok_or(AbiError::Unsupported)
    }

    /// Drives the SPI chip select, if the bus has one (low = selected).
    fn select(&mut self, selected: bool) {
        let cs = self.hw.as_ref().and_then(|hw| hw.spi.cs());
        if let Some(f) = cs.and_then(|p| self.pin(p).ok()) {
            f.set_level(if selected { Level::Low } else { Level::High });
        }
    }

    /// Pumps a running SPI transfer, yielding between bounded steps, and
    /// raises its `DmaDone` event when it ends (chip select released).
    /// Pending forever while no transfer runs. The controller awaits it
    /// alongside messages and timers; dropping it mid-transfer loses
    /// nothing (the progress lives in the bus).
    pub async fn io(&mut self) {
        loop {
            let Some(hw) = self.hw.as_mut() else { return core::future::pending().await };
            if !hw.spi.busy() {
                return core::future::pending().await;
            }
            if let Some(value) = hw.spi.step() {
                self.select(false);
                let _ = self.events.push(dma_done_event(DmaChannel::Spi0, value, now_us() as u32));
                return;
            }
            yield_now().await;
        }
    }
}

/// Returns `pin` to an input without pull: its SIO function back (if a
/// bus held it), then the `Flex` mode.
fn release_pin(hw: &mut Hw, pin: u8, routed: bool) {
    if routed {
        pads::to_gpio(pin);
    }
    if let Some(Some(f)) = hw.pins.get_mut(usize::from(pin)) {
        f.set_as_input();
        f.set_pull(HalPull::None);
    }
}

impl Drop for RpDevice {
    fn drop(&mut self) {
        let Some(mut hw) = self.hw.take() else { return };
        hw.spi.close();
        hw.i2c.close();
        for pin in pins(self.claims.all()) {
            release_pin(&mut hw, pin, true);
        }
        for pin in self.modes.configured() {
            release_pin(&mut hw, pin, false);
        }
        HW.put(hw);
    }
}

impl wasm_host::Device for RpDevice {
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

    fn now_us(&mut self) -> u32 {
        now_us() as u32
    }

    fn gpio_output(&mut self, pin: u8) -> Result<(), AbiError> {
        self.claims.check_gpio(pin)?;
        let next = self.modes.output(self.profile, pin)?;
        let f = self.pin(pin)?;
        f.set_low();
        f.set_as_output();
        self.modes = next;
        Ok(())
    }

    fn gpio_input(&mut self, pin: u8, pull: Pull) -> Result<(), AbiError> {
        self.claims.check_gpio(pin)?;
        let next = self.modes.input(self.profile, pin)?;
        let f = self.pin(pin)?;
        f.set_as_input();
        f.set_pull(match pull {
            Pull::None => HalPull::None,
            Pull::Up => HalPull::Up,
            Pull::Down => HalPull::Down,
        });
        self.modes = next;
        Ok(())
    }

    fn gpio_write(&mut self, pin: u8, high: bool) -> Result<(), AbiError> {
        self.claims.check_gpio(pin)?;
        self.modes.check_write(self.profile, pin)?;
        self.pin(pin)?.set_level(if high { Level::High } else { Level::Low });
        Ok(())
    }

    fn gpio_read(&mut self, pin: u8) -> Result<bool, AbiError> {
        self.claims.check_gpio(pin)?;
        let from = self.modes.read_from(self.profile, pin)?;
        let f = self.pin(pin)?;
        Ok(match from {
            ReadFrom::Pad => f.is_high(),
            ReadFrom::Latch => f.is_set_high(),
        })
    }

    fn irq_overflow(&mut self) -> u32 {
        self.events.take_overflow()
    }

    fn timer_start(&mut self, spec: TimerSpec) -> Result<(), AbiError> {
        self.events.purge(EventKind::Timer, i32::from(spec.id));
        self.timers = self.timers.start(spec, now_us());
        Ok(())
    }

    fn timer_stop(&mut self, id: u8) -> Result<(), AbiError> {
        self.timers = self.timers.stop(id);
        self.events.purge(EventKind::Timer, i32::from(id));
        Ok(())
    }

    fn adc_read(&mut self, pin: u8) -> Result<u16, AbiError> {
        let channel = usize::from(self.profile.adc_channel(pin)?);
        let analog = &mut self.hw.as_mut().ok_or(AbiError::Unsupported)?.adc;
        let ch = analog.channels.get_mut(channel).ok_or(AbiError::Unsupported)?;
        analog.adc.blocking_read(ch).map_err(|_| AbiError::Transport)
    }

    /// Resolves the pins onto SPI0 or SPI1, claims them (pins the bus held
    /// before and no longer needs go back to GPIO), configures the
    /// controller and routes SCK / MOSI / MISO; CS becomes an output, high.
    fn spi_open(&mut self, cfg: SpiConfig) -> Result<(), AbiError> {
        let route = SpiRoute::resolve(self.profile, &cfg)?;
        let next = self.claims.claim_spi(self.profile, &self.modes, &route)?;
        let old = self.claims.pins_of(Owner::Spi);
        let hw = self.hw()?;
        hw.spi.open(route, &cfg)?;
        for pin in pins(old & !next.pins_of(Owner::Spi)) {
            release_pin(hw, pin, true);
        }
        for pin in route.data_pins() {
            pads::route(pin, Function::Spi);
        }
        if let Some(cs) = route.cs {
            pads::to_gpio(cs);
            if let Some(Some(f)) = hw.pins.get_mut(usize::from(cs)) {
                f.set_high();
                f.set_as_output();
            }
        }
        self.claims = next;
        Ok(())
    }

    /// Starts the transfer with CS low; the controller loop pumps it
    /// ([`RpDevice::io`]).
    fn spi_transfer(&mut self, bus: u8, tx: &[u8], len: usize) -> Result<(), AbiError> {
        if bus != 0 {
            return Err(AbiError::InvalidArgument);
        }
        self.hw()?.spi.transfer(tx, len)?;
        self.select(true);
        Ok(())
    }

    fn dma_read(&mut self, channel: DmaChannel, out: &mut [u8]) -> Result<usize, AbiError> {
        match channel {
            DmaChannel::Spi0 => self.hw()?.spi.read(out),
            DmaChannel::Uart1Tx => Err(AbiError::Unsupported),
        }
    }

    /// Claims the bus's board-mapped pins, configures the controller at
    /// the clock, then routes SDA and SCL (pull-ups on).
    fn i2c_open(&mut self, cfg: I2cConfig) -> Result<(), AbiError> {
        let (next, (sda, scl)) = self.claims.claim_i2c(self.profile, &self.modes, cfg.bus)?;
        self.hw()?.i2c.open(cfg.bus, cfg.freq_hz)?;
        pads::route(sda, Function::I2c);
        pads::route(scl, Function::I2c);
        self.claims = next;
        Ok(())
    }

    fn i2c_transfer(&mut self, bus: u8, addr: u8, tx: &[u8], rx: &mut [u8]) -> Result<(), AbiError> {
        self.hw()?.i2c.transfer(bus, addr, tx, rx)
    }

    /// Accepts any key a payload may serve; the controller routes its
    /// requests to `Payload::request`.
    fn serve(&mut self, key: Key<'_>) -> Result<(), AbiError> {
        esp_base::serve::servable(key)
    }

    fn node_id(&mut self) -> Result<SenderId, AbiError> {
        Ok(self.node)
    }
}
