//! The board as a payload sees it: `wasm_host::Device` over the Pico's
//! GPIO, ADC, clock and the message bus (`wasm` only).
//!
//! | ABI call | RP base |
//! |----------|---------|
//! | `log`, `publish` | `sys/log` and publications on the bus (`sys/...` keys refused) |
//! | `now_ms`, `now_us` | embassy-time (1 MHz timer) |
//! | `gpio_output`, `gpio_input`, `gpio_write`, `gpio_read` | `Flex` pins, [`rp_base::pins`] rules |
//! | `timer_start`, `timer_stop` | four software timers (`esp_base::periph::timers`) fired by the controller loop as `Timer` events |
//! | `irq_overflow` | the event queue's drop count |
//! | `adc_read` | GPIO 26–28, ADC0–2, blocking 12-bit read |
//! | `gpio_irq`, `spi_*`, `dma_read`, `uart_*`, `pwm_*`, `i2c_*`, `route_*` | `Unsupported` (wasm-host's defaults) |
//!
//! The hardware ([`Hw`]: the pin bank and the ADC) rests in [`HW`] between
//! runs. [`RpDevice::new`] takes it and `Drop` puts it back with every pin
//! the payload configured returned to an input without pull, so a payload
//! that fails to instantiate, traps or is stopped leaves the board as it
//! found it.

use core::cell::RefCell;

use embassy_rp::adc::{Adc, Channel};
use embassy_rp::gpio::{Flex, Level, Pull as HalPull};
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::Instant;
use esp_base::periph::timer_event;
use esp_base::periph::timers::Timers;
use middleware::Key;
use middleware::loader::is_system_key;
use middleware::wasm_abi::events::{Event, EventKind, EventQueue, MAX_EVENTS};
use middleware::wasm_abi::periph::TimerSpec;
use middleware::wasm_abi::{AbiError, Pull};
use rp_base::chip::Profile;
use rp_base::pins::{GPIOS, Pins, ReadFrom};

use super::bus::Bus;

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
    timers: Timers,
    events: EventQueue<MAX_EVENTS>,
    bus: &'static Bus,
    profile: &'static Profile,
}

impl RpDevice {
    /// Takes the hardware from [`HW`]; `None` if it is missing.
    pub fn new(bus: &'static Bus, profile: &'static Profile) -> Option<Self> {
        let hw = HW.take()?;
        Some(RpDevice {
            hw: Some(hw),
            modes: Pins::new(),
            timers: Timers::new(),
            events: EventQueue::new(),
            bus,
            profile,
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
}

impl Drop for RpDevice {
    fn drop(&mut self) {
        let Some(mut hw) = self.hw.take() else { return };
        for pin in self.modes.configured() {
            if let Some(Some(f)) = hw.pins.get_mut(usize::from(pin)) {
                f.set_as_input();
                f.set_pull(HalPull::None);
            }
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
        let next = self.modes.output(self.profile, pin)?;
        let f = self.pin(pin)?;
        f.set_low();
        f.set_as_output();
        self.modes = next;
        Ok(())
    }

    fn gpio_input(&mut self, pin: u8, pull: Pull) -> Result<(), AbiError> {
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
        self.modes.check_write(self.profile, pin)?;
        self.pin(pin)?.set_level(if high { Level::High } else { Level::Low });
        Ok(())
    }

    fn gpio_read(&mut self, pin: u8) -> Result<bool, AbiError> {
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
}
