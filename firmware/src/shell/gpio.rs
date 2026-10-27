//! GPIO for payloads: the pin bank, GPIO interrupts and peripheral signal
//! routing.
//!
//! [`PinBank`] owns every pin a payload may use, as an `esp_hal` [`Flex`]
//! indexed by GPIO number; pins the chip profile reserves (flash, PSRAM,
//! USB, console UART) are never inserted. Every operation is first checked
//! against [`Profile::check_pin`], so the pure policy in `esp_base::chip`
//! decides what a payload may touch.
//!
//! # Where the bank lives
//!
//! The GPIO interrupt handler must read and clear the pins that raised it,
//! so the bank lives in the static [`PINS`] (a critical-section mutex) for
//! the life of the firmware instead of moving into the payload's device.
//! Exclusive use is still linear: a payload's device holds the one
//! [`PinLease`]; dropping the lease (payload stop, trap or failed load)
//! releases every pin to high impedance, disables its interrupt and
//! detaches any peripheral signal.
//!
//! # GPIO interrupts (D5)
//!
//! [`init`] installs [`gpio_isr`] (a compile-time `#[handler]`) as the
//! esp-hal GPIO interrupt handler. For each pin with an edge enabled —
//! O(pins), no allocation — the ISR checks the pin's interrupt status,
//! reads the level, clears the status, and pushes an event when
//! [`Edge::matches`] the level (via `esp_base::periph::gpio_event`),
//! timestamped in microseconds. A pin must be an input (`gpio_input`)
//! before `gpio_irq`; reconfiguring it drops the interrupt.
//!
//! # Peripheral routing (D6, D7)
//!
//! SPI and UART pins stay in the bank: [`PinBank::route_out`] and
//! [`PinBank::route_in`] connect a peripheral signal to a pin through the
//! GPIO matrix by reference (`OutputSignal::connect_to(&flex)`), so the
//! drivers never consume the pin. [`PinBank::unroute`] reconnects the pin
//! to plain GPIO (alternate function and output select), which also undoes
//! a direct IO MUX function the routing may have chosen.

use core::cell::RefCell;

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use esp_base::chip::{Direction, Profile};
use esp_base::periph::gpio_event;
use esp_hal::gpio::{
    Event as HalEvent, Flex, InputConfig, InputSignal, Io, Level, OutputConfig, OutputSignal, Pull as HalPull,
};
use esp_hal::handler;
use esp_hal::peripherals::IO_MUX;
use wasm_host::{AbiError, Edge, Pull};

use super::irq;

/// One past the highest GPIO number on any supported chip (ESP32-S3: 48).
const MAX_PINS: usize = 49;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Idle,
    Input,
    Output,
    /// Routed to SPI or UART.
    Peripheral,
}

struct Slot {
    pin: Flex<'static>,
    mode: Mode,
    edge: Edge,
    /// The payload enabled the pull-down (an input that reads low unpressed).
    pull_down: bool,
}

impl Slot {
    fn drop_irq(&mut self, n: u8) {
        if self.edge != Edge::Off {
            self.pin.unlisten();
            self.pin.clear_interrupt();
            self.edge = Edge::Off;
            irq::purge(middleware::wasm_abi::events::EventKind::Gpio, i32::from(n));
        }
    }
}

/// The pins available to payloads.
pub struct PinBank {
    profile: &'static Profile,
    slots: [Option<Slot>; MAX_PINS],
}

impl PinBank {
    /// An empty bank governed by `profile`.
    pub fn new(profile: &'static Profile) -> Self {
        PinBank { profile, slots: [const { None }; MAX_PINS] }
    }

    /// Adds GPIO `n`; reserved or non-existent pins are dropped.
    pub fn insert(&mut self, n: u8, pin: Flex<'static>) {
        if self.profile.exists(n)
            && !self.profile.is_reserved(n)
            && let Some(slot) = self.slots.get_mut(usize::from(n))
        {
            *slot = Some(Slot { pin, mode: Mode::Idle, edge: Edge::Off, pull_down: false });
        }
    }

    fn slot(&mut self, n: u8, dir: Direction) -> Result<&mut Slot, AbiError> {
        self.profile.check_pin(n, dir)?;
        self.slots.get_mut(usize::from(n)).and_then(Option::as_mut).ok_or(AbiError::Unsupported)
    }

    /// Returns every pin to high impedance (input and output disabled, no
    /// pull, interrupt off, plain GPIO function), as after reset.
    fn release_all(&mut self) {
        for slot in self.slots.iter_mut().flatten() {
            slot.pin.unlisten();
            slot.pin.clear_interrupt();
            slot.edge = Edge::Off;
            OutputSignal::GPIO.connect_to(&slot.pin);
            slot.pin.set_output_enable(false);
            slot.pin.set_input_enable(false);
            slot.pin.apply_input_config(&InputConfig::default().with_pull(HalPull::None));
            slot.mode = Mode::Idle;
            slot.pull_down = false;
        }
    }

    /// Samples the BOOT button on pin `n` for the presence detector:
    /// `Some(true)` when it reads low (pressed). An idle pin is read with
    /// the pull-up briefly enabled and returned to high impedance; a
    /// payload's input is read as is. `None` when the pin is missing, an
    /// output, routed to a peripheral, or pulled down by the payload, so a
    /// payload can never fake a press.
    pub fn boot_button(&mut self, n: u8) -> Option<bool> {
        let slot = self.slots.get_mut(usize::from(n))?.as_mut()?;
        match slot.mode {
            Mode::Idle => {
                slot.pin.apply_input_config(&InputConfig::default().with_pull(HalPull::Up));
                slot.pin.set_input_enable(true);
                let low = slot.pin.is_low();
                slot.pin.set_input_enable(false);
                slot.pin.apply_input_config(&InputConfig::default().with_pull(HalPull::None));
                Some(low)
            }
            Mode::Input if !slot.pull_down => Some(slot.pin.is_low()),
            Mode::Input | Mode::Output | Mode::Peripheral => None,
        }
    }

    /// Drives `n` low as an output.
    pub fn output(&mut self, n: u8) -> Result<(), AbiError> {
        let slot = self.slot(n, Direction::Output)?;
        slot.drop_irq(n);
        slot.pin.set_input_enable(false);
        slot.pin.set_level(Level::Low);
        slot.pin.apply_output_config(&OutputConfig::default());
        slot.pin.set_output_enable(true);
        slot.mode = Mode::Output;
        Ok(())
    }

    /// Makes `n` an input with `pull`.
    pub fn input(&mut self, n: u8, pull: Pull) -> Result<(), AbiError> {
        let slot = self.slot(n, Direction::Input)?;
        slot.drop_irq(n);
        let pull = match pull {
            Pull::None => HalPull::None,
            Pull::Up => HalPull::Up,
            Pull::Down => HalPull::Down,
        };
        slot.pin.set_output_enable(false);
        slot.pin.apply_input_config(&InputConfig::default().with_pull(pull));
        slot.pin.set_input_enable(true);
        slot.mode = Mode::Input;
        slot.pull_down = pull == HalPull::Down;
        Ok(())
    }

    /// Sets an output's level.
    pub fn write(&mut self, n: u8, high: bool) -> Result<(), AbiError> {
        let slot = self.slot(n, Direction::Output)?;
        if slot.mode != Mode::Output {
            return Err(AbiError::InvalidArgument);
        }
        slot.pin.set_level(if high { Level::High } else { Level::Low });
        Ok(())
    }

    /// Reads an input's level (or an output's driven level).
    pub fn read(&mut self, n: u8) -> Result<bool, AbiError> {
        let slot = self.slot(n, Direction::Input)?;
        match slot.mode {
            Mode::Input => Ok(slot.pin.is_high()),
            Mode::Output => Ok(slot.pin.is_set_high()),
            Mode::Idle | Mode::Peripheral => Err(AbiError::InvalidArgument),
        }
    }

    /// Enables (or with [`Edge::Off`] disables) the interrupt on input `n`;
    /// disabling purges its pending events.
    pub fn set_irq(&mut self, n: u8, edge: Edge) -> Result<(), AbiError> {
        let slot = self.slot(n, Direction::Input)?;
        if slot.mode != Mode::Input {
            return Err(AbiError::InvalidArgument);
        }
        slot.drop_irq(n);
        let event = match edge {
            Edge::Off => return Ok(()),
            Edge::Rising => HalEvent::RisingEdge,
            Edge::Falling => HalEvent::FallingEdge,
            Edge::Any => HalEvent::AnyEdge,
        };
        slot.pin.clear_interrupt();
        slot.edge = edge;
        slot.pin.listen(event);
        Ok(())
    }

    /// ISR body: services every pin with an edge enabled.
    fn service_irqs(&mut self, time_us: u32) {
        for (n, slot) in self.slots.iter_mut().enumerate() {
            let Some(slot) = slot else { continue };
            if slot.edge == Edge::Off || !slot.pin.is_interrupt_set() {
                continue;
            }
            let level = slot.pin.is_high();
            slot.pin.clear_interrupt();
            if let Some(event) = gpio_event(n as u8, slot.edge, level, time_us) {
                irq::push(event);
            }
        }
    }

    /// Connects peripheral output `signal` to pin `n` (push-pull output).
    pub fn route_out(&mut self, n: u8, signal: OutputSignal) -> Result<(), AbiError> {
        let slot = self.slot(n, Direction::Output)?;
        slot.drop_irq(n);
        slot.pin.set_input_enable(false);
        slot.pin.apply_output_config(&OutputConfig::default());
        slot.pin.set_output_enable(true);
        signal.connect_to(&slot.pin);
        slot.mode = Mode::Peripheral;
        Ok(())
    }

    /// Connects pin `n` to peripheral input `signal`.
    pub fn route_in(&mut self, n: u8, signal: InputSignal) -> Result<(), AbiError> {
        let slot = self.slot(n, Direction::Input)?;
        slot.drop_irq(n);
        slot.pin.set_output_enable(false);
        slot.pin.apply_input_config(&InputConfig::default());
        slot.pin.set_input_enable(true);
        signal.connect_to(&slot.pin);
        slot.mode = Mode::Peripheral;
        Ok(())
    }

    /// Returns pin `n` from a peripheral to idle plain GPIO.
    pub fn unroute(&mut self, n: u8) {
        if let Some(Some(slot)) = self.slots.get_mut(usize::from(n)) {
            OutputSignal::GPIO.connect_to(&slot.pin);
            slot.pin.set_output_enable(false);
            slot.pin.set_input_enable(false);
            slot.mode = Mode::Idle;
        }
    }
}

struct Home {
    bank: Option<PinBank>,
    leased: bool,
}

/// Where the bank lives (see the module docs).
pub struct PinHome(Mutex<CriticalSectionRawMutex, RefCell<Home>>);

/// The firmware's pin bank.
pub static PINS: PinHome = PinHome(Mutex::new(RefCell::new(Home { bank: None, leased: false })));

impl PinHome {
    fn with_home<R>(&self, f: impl FnOnce(&mut Home) -> R) -> Option<R> {
        self.0.lock(|cell| cell.try_borrow_mut().ok().map(|mut h| f(&mut h)))
    }

    /// Installs the bank at boot.
    pub fn install(&self, bank: PinBank) {
        self.with_home(|h| h.bank = Some(bank));
    }

    /// Samples the BOOT button on pin `n` ([`PinBank::boot_button`]);
    /// `None` before the bank is installed or while it is borrowed.
    pub fn boot_button(&self, n: u8) -> Option<bool> {
        self.with_home(|h| h.bank.as_mut().and_then(|b| b.boot_button(n))).flatten()
    }

    /// Takes the lease for a payload, with every pin released; `None` when
    /// a lease is outstanding or no bank is installed.
    pub fn lease(&'static self) -> Option<PinLease> {
        let ok = self
            .with_home(|h| match (&mut h.bank, h.leased) {
                (Some(bank), false) => {
                    bank.release_all();
                    h.leased = true;
                    true
                }
                _ => false,
            })
            .unwrap_or(false);
        ok.then_some(PinLease { home: self })
    }
}

/// Exclusive use of the bank by one payload; releases all pins on drop.
pub struct PinLease {
    home: &'static PinHome,
}

impl PinLease {
    /// Runs `f` on the bank.
    pub fn with<R>(&mut self, f: impl FnOnce(&mut PinBank) -> Result<R, AbiError>) -> Result<R, AbiError> {
        self.home.with_home(|h| h.bank.as_mut().map(f)).flatten().unwrap_or(Err(AbiError::Unsupported))
    }
}

impl Drop for PinLease {
    fn drop(&mut self) {
        self.home.with_home(|h| {
            if let Some(bank) = h.bank.as_mut() {
                bank.release_all();
            }
            h.leased = false;
        });
    }
}

#[handler]
fn gpio_isr() {
    let now = irq::now_us() as u32;
    PINS.with_home(|h| {
        if let Some(bank) = h.bank.as_mut() {
            bank.service_irqs(now);
        }
    });
}

/// Installs [`gpio_isr`] as the GPIO interrupt handler.
pub fn init(io_mux: IO_MUX<'static>) {
    let mut io = Io::new(io_mux);
    io.set_interrupt_handler(gpio_isr);
}
