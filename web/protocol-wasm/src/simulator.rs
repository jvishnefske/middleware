//! In-browser payload simulator (`simulator` feature).
//!
//! [`Simulator`] runs a payload in [`wasm_host::Payload`], the same runtime
//! the firmware uses, against [`SimDevice`], a virtual device:
//!
//! * `log`, `publish`, GPIO configuration/writes, SPI transfers, UART
//!   transmissions and interrupt deliveries are recorded as [`SimEvent`]s in
//!   a queue that the JS binding drains after every call and forwards to a
//!   JS callback. Keeping the device a plain value (no JS handles) lets the
//!   logic be tested natively.
//! * `gpio_read` returns the level of a virtual input pin, set from JS with
//!   [`Simulator::set_input`]; an unset input reads its pull level (high for
//!   [`Pull::Up`], low otherwise). Reading an output returns its last write.
//!   A level change on an input armed with `gpio_irq` queues a GPIO event
//!   when the edge matches.
//! * Time is supplied by JS, in microseconds since load, through
//!   [`Simulator::step`] (or [`Simulator::tick`] / [`Simulator::set_now`] in
//!   milliseconds), so tests run in virtual time. `now_ms` and `now_us` are
//!   that clock, wrapped to 32 bits.
//! * Timers, the SPI device (loopback or a pattern), UART 1 and the event
//!   queue live in [`periph::Peripherals`].
//! * ABI 3 (X15): `pwm_open`, `pwm_set` and `pwm_stop` update a
//!   [`periph::Pwm`] and are recorded as [`SimEvent::PwmOpen`],
//!   [`SimEvent::PwmSet`] and [`SimEvent::PwmStop`] (only when they
//!   succeed); [`Simulator::pwm`] lists the open timers. `adc_read` returns
//!   the value JS set with [`Simulator::set_adc`] (0 until set; the web
//!   `SimSession` clamps to 12 bits). Every ABI timer, channel and pin
//!   is accepted, as on the desktop simulator; the ESP32 boards back only
//!   timers 0–3, channels 0–1, and ADC1 pins.
//! * ABI 5: the device has a node id ([`SimDevice::with_node`], default
//!   [`DEFAULT_NODE`], `0000000000000001` like payload-sim), `node_id`
//!   returns it and `serve` accepts every key (recorded as
//!   [`SimEvent::Serve`]); [`Simulator::request`] hands a request to the
//!   payload serving its key (`Payload::request`, a [`REPLY_CAP`]-byte
//!   reply buffer) and returns the reply or the `HandlerError` a device
//!   would answer.
//!
//! [`Simulator::step`] is the main loop: it completes DMA transfers started
//! before the step (raising `DmaDone`), raises due timer alarms, runs `tick`
//! if due, then pops at most [`MAX_DISPATCH_PER_STEP`] queued events into
//! [`Payload::dispatch`] (`on_irq`). Without an `on_irq` export, events are
//! discarded.
//!
//! Pins are `0..PIN_COUNT`; others are [`AbiError::InvalidArgument`], as is
//! writing a pin not configured as an output, reading an unconfigured pin,
//! or arming an interrupt on a pin that is not an input. Reconfiguring a pin
//! disarms its interrupt. After a trap the simulator refuses further calls
//! until reloaded.

pub mod periph;

use middleware::wasm_abi::events::MAX_EVENTS;
use middleware::{HandlerError, Key, RequestMeta, SenderId};
use wasm_host::{
    AbiError, Device, DmaChannel, Edge, Event, Limits, Payload, Pull, PwmSpec, SpiConfig,
    TimerSpec, UartConfig,
};

use periph::{Adc, Peripherals, Pwm, PwmInfo, SpiResponder, SpiTransfer, TimerInfo};

/// Number of virtual GPIO pins.
pub const PIN_COUNT: usize = 49;

/// Most `on_irq` calls one [`Simulator::step`] makes; the rest wait for
/// the next step. One full queue.
pub const MAX_DISPATCH_PER_STEP: usize = MAX_EVENTS;

/// The node id a [`SimDevice`] has unless told otherwise (payload-sim's
/// default too).
pub const DEFAULT_NODE: SenderId = SenderId([0, 0, 0, 0, 0, 0, 0, 1]);

/// The reply buffer [`Simulator::request`] offers (the ABI's maximum).
pub const REPLY_CAP: usize = middleware::wasm_abi::serve::MAX_REPLY;

/// Configuration of one virtual pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinMode {
    /// Not configured by the payload.
    Unused,
    /// Output, with its current level.
    Output(bool),
    /// Input with a pull; `Some(level)` once JS has driven it.
    Input(Pull, Option<bool>),
}

impl PinMode {
    /// The level a read would return, if the pin is configured.
    pub fn level(self) -> Option<bool> {
        match self {
            PinMode::Unused => None,
            PinMode::Output(level) | PinMode::Input(_, Some(level)) => Some(level),
            PinMode::Input(pull, None) => Some(pull == Pull::Up),
        }
    }
}

/// Something the payload did, for the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SimEvent {
    /// A log line.
    Log(String),
    /// A publication.
    Publish {
        /// Key.
        key: String,
        /// Bytes.
        data: Vec<u8>,
    },
    /// A pin was configured as an output.
    Output(u8),
    /// A pin was configured as an input.
    Input(u8, Pull),
    /// An output pin was driven.
    Write(u8, bool),
    /// An SPI transfer was clocked through the virtual device.
    Spi(SpiTransfer),
    /// Bytes transmitted on UART 1.
    Uart(Vec<u8>),
    /// A PWM timer was opened or retuned.
    PwmOpen {
        /// Timer id.
        timer: u8,
        /// Frequency in Hz.
        freq_hz: u32,
    },
    /// A PWM channel's duty was set.
    PwmSet {
        /// Timer id.
        timer: u8,
        /// Channel.
        channel: u8,
        /// Duty, a fraction of 65535.
        duty: u16,
    },
    /// A PWM timer was stopped.
    PwmStop(u8),
    /// The payload serves this key (ABI 5 `serve`).
    Serve(String),
    /// An interrupt event was popped from the queue; `handled` is whether
    /// the payload's `on_irq` ran.
    Irq {
        /// The event.
        event: Event,
        /// Whether `on_irq` is exported and was called.
        handled: bool,
    },
}

/// The virtual device behind the simulator.
#[derive(Debug, Clone)]
pub struct SimDevice {
    now_us: u64,
    pins: [PinMode; PIN_COUNT],
    periph: Peripherals,
    pwm: Pwm,
    adc: Adc,
    events: Vec<SimEvent>,
    node: SenderId,
}

impl Default for SimDevice {
    fn default() -> Self {
        Self::new()
    }
}

impl SimDevice {
    /// A device with every pin unused, time 0, no events, node
    /// [`DEFAULT_NODE`].
    pub fn new() -> Self {
        Self {
            now_us: 0,
            pins: [PinMode::Unused; PIN_COUNT],
            periph: Peripherals::new(),
            pwm: Pwm::default(),
            adc: Adc::default(),
            events: Vec::new(),
            node: DEFAULT_NODE,
        }
    }

    /// The same device with node id `node`.
    pub fn with_node(self, node: SenderId) -> Self {
        Self { node, ..self }
    }

    /// The node id `node_id` returns.
    pub const fn node(&self) -> SenderId {
        self.node
    }

    /// Takes the recorded events.
    pub fn drain(&mut self) -> Vec<SimEvent> {
        core::mem::take(&mut self.events)
    }

    /// The mode of `pin`, if it exists.
    pub fn pin(&self, pin: u8) -> Option<PinMode> {
        self.pins.get(usize::from(pin)).copied()
    }

    /// The clock, in microseconds since load.
    pub const fn time_us(&self) -> u64 {
        self.now_us
    }

    /// Sets the clock.
    pub fn set_time_us(&mut self, now_us: u64) {
        self.now_us = now_us;
    }

    /// The ABI 2 peripherals.
    pub fn periph(&mut self) -> &mut Peripherals {
        &mut self.periph
    }

    /// Drives a virtual input. Pins not configured as inputs remember
    /// nothing; returns whether the level was applied. A change matching
    /// the pin's armed edge queues a GPIO event stamped with the clock.
    pub fn set_input(&mut self, pin: u8, high: bool) -> bool {
        let Some(mode) = self.pins.get_mut(usize::from(pin)) else {
            return false;
        };
        let PinMode::Input(pull, level) = *mode else {
            return false;
        };
        *mode = PinMode::Input(pull, Some(high));
        if level.unwrap_or(pull == Pull::Up) != high {
            self.periph.level_changed(pin, high, self.now_us);
        }
        true
    }

    /// Feeds bytes to UART 1's receiver; returns how many were buffered.
    pub fn uart_inject(&mut self, data: &[u8]) -> usize {
        self.periph.inject_rx(data, self.now_us)
    }

    /// The PWM timers.
    pub fn pwm(&self) -> &Pwm {
        &self.pwm
    }

    /// Sets the value `adc_read(pin)` returns (`0..=ADC_MAX`).
    pub fn set_adc(&mut self, pin: u8, value: u16) -> Result<(), String> {
        self.adc.set(pin, value)
    }

    fn slot(&mut self, pin: u8) -> Result<&mut PinMode, AbiError> {
        self.pins
            .get_mut(usize::from(pin))
            .ok_or(AbiError::InvalidArgument)
    }

    fn reconfigure(&mut self, pin: u8, mode: PinMode) -> Result<(), AbiError> {
        *self.slot(pin)? = mode;
        self.periph.set_edge(pin, Edge::Off)
    }
}

impl Device for SimDevice {
    fn log(&mut self, line: &str) {
        self.events.push(SimEvent::Log(line.to_owned()));
    }

    fn publish(&mut self, key: Key<'_>, data: &[u8]) -> Result<(), AbiError> {
        self.events.push(SimEvent::Publish {
            key: key.as_str().to_owned(),
            data: data.to_vec(),
        });
        Ok(())
    }

    fn now_ms(&mut self) -> u32 {
        (self.now_us / 1000) as u32
    }

    fn gpio_output(&mut self, pin: u8) -> Result<(), AbiError> {
        self.reconfigure(pin, PinMode::Output(false))?;
        self.events.push(SimEvent::Output(pin));
        Ok(())
    }

    fn gpio_input(&mut self, pin: u8, pull: Pull) -> Result<(), AbiError> {
        self.reconfigure(pin, PinMode::Input(pull, None))?;
        self.events.push(SimEvent::Input(pin, pull));
        Ok(())
    }

    fn gpio_write(&mut self, pin: u8, high: bool) -> Result<(), AbiError> {
        let slot = self.slot(pin)?;
        match slot {
            PinMode::Output(_) => {
                *slot = PinMode::Output(high);
                self.events.push(SimEvent::Write(pin, high));
                Ok(())
            }
            _ => Err(AbiError::InvalidArgument),
        }
    }

    fn gpio_read(&mut self, pin: u8) -> Result<bool, AbiError> {
        self.slot(pin)?.level().ok_or(AbiError::InvalidArgument)
    }

    fn now_us(&mut self) -> u32 {
        self.now_us as u32
    }

    fn irq_overflow(&mut self) -> u32 {
        self.periph.take_overflow()
    }

    fn gpio_irq(&mut self, pin: u8, edge: Edge) -> Result<(), AbiError> {
        match *self.slot(pin)? {
            PinMode::Input(..) => self.periph.set_edge(pin, edge),
            _ => Err(AbiError::InvalidArgument),
        }
    }

    fn timer_start(&mut self, spec: TimerSpec) -> Result<(), AbiError> {
        self.periph.timer_start(spec, self.now_us)
    }

    fn timer_stop(&mut self, id: u8) -> Result<(), AbiError> {
        self.periph.timer_stop(id)
    }

    fn spi_open(&mut self, cfg: SpiConfig) -> Result<(), AbiError> {
        self.periph.spi_open(cfg)
    }

    fn spi_transfer(&mut self, bus: u8, tx: &[u8], len: usize) -> Result<(), AbiError> {
        let t = self.periph.spi_transfer(bus, tx, len)?;
        self.events.push(SimEvent::Spi(t));
        Ok(())
    }

    fn dma_read(&mut self, channel: DmaChannel, out: &mut [u8]) -> Result<usize, AbiError> {
        self.periph.dma_read(channel, out)
    }

    fn uart_open(&mut self, cfg: UartConfig) -> Result<(), AbiError> {
        self.periph.uart_open(cfg)
    }

    fn uart_write(&mut self, port: u8, data: &[u8]) -> Result<(), AbiError> {
        self.periph.uart_write(port, data)?;
        self.events.push(SimEvent::Uart(data.to_vec()));
        Ok(())
    }

    fn uart_read(&mut self, port: u8, out: &mut [u8]) -> Result<usize, AbiError> {
        self.periph.uart_read(port, out)
    }

    fn pwm_open(&mut self, spec: PwmSpec) -> Result<(), AbiError> {
        self.pwm.open(spec)?;
        self.events.push(SimEvent::PwmOpen {
            timer: spec.timer,
            freq_hz: spec.freq_hz,
        });
        Ok(())
    }

    fn pwm_set(&mut self, timer: u8, channel: u8, duty: u16) -> Result<(), AbiError> {
        self.pwm.set(timer, channel, duty)?;
        self.events.push(SimEvent::PwmSet {
            timer,
            channel,
            duty,
        });
        Ok(())
    }

    fn pwm_stop(&mut self, timer: u8) -> Result<(), AbiError> {
        self.pwm.stop(timer)?;
        self.events.push(SimEvent::PwmStop(timer));
        Ok(())
    }

    fn adc_read(&mut self, pin: u8) -> Result<u16, AbiError> {
        self.adc.read(pin)
    }

    fn serve(&mut self, key: Key<'_>) -> Result<(), AbiError> {
        self.events.push(SimEvent::Serve(key.as_str().to_owned()));
        Ok(())
    }

    fn node_id(&mut self) -> Result<SenderId, AbiError> {
        Ok(self.node)
    }
}

/// Why a simulator call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SimError {
    /// The module was rejected at load time.
    Load(String),
    /// The payload trapped (including fuel exhaustion).
    Trap(String),
    /// A previous trap stopped the payload.
    Stopped,
    /// The key is malformed.
    InvalidKey,
}

impl SimError {
    /// Human-readable description.
    pub fn describe(&self) -> String {
        match self {
            SimError::Load(e) => format!("load failed: {e}"),
            SimError::Trap(e) => format!("trap: {e}"),
            SimError::Stopped => "payload stopped after a trap; reload it".into(),
            SimError::InvalidKey => "invalid key".into(),
        }
    }
}

/// A payload running on a [`SimDevice`].
pub struct Simulator {
    payload: Payload<SimDevice>,
    trapped: bool,
}

impl Simulator {
    /// Validates and instantiates `wasm` (running its `init`), with the
    /// default [`Limits`] unless overridden, on a device with node
    /// [`DEFAULT_NODE`].
    pub fn load(wasm: &[u8], limits: Option<Limits>) -> Result<Self, SimError> {
        Self::load_on(wasm, limits, SimDevice::new())
    }

    /// [`Simulator::load`] on `device` (e.g. [`SimDevice::with_node`]:
    /// `init` already asks for the node id).
    pub fn load_on(
        wasm: &[u8],
        limits: Option<Limits>,
        device: SimDevice,
    ) -> Result<Self, SimError> {
        let payload = Payload::load(wasm, device, limits.unwrap_or_default())
            .map_err(|e| SimError::Load(e.to_string()))?;
        Ok(Self {
            payload,
            trapped: false,
        })
    }

    fn guard(&self) -> Result<(), SimError> {
        if self.trapped {
            Err(SimError::Stopped)
        } else {
            Ok(())
        }
    }

    fn trap<T, E: core::fmt::Display>(&mut self, r: Result<T, E>) -> Result<T, SimError> {
        r.map_err(|e| {
            self.trapped = true;
            SimError::Trap(e.to_string())
        })
    }

    /// Sets the clock to `now_ms` milliseconds without running anything.
    pub fn set_now(&mut self, now_ms: u32) {
        self.set_time_us(u64::from(now_ms) * 1000);
    }

    /// Sets the clock to `now_us` microseconds without running anything.
    pub fn set_time_us(&mut self, now_us: u64) {
        self.payload.device().set_time_us(now_us);
    }

    /// [`Simulator::step`] at `now_ms` milliseconds.
    pub fn tick(&mut self, now_ms: u32) -> Result<(), SimError> {
        self.step(u64::from(now_ms) * 1000).map(|_| ())
    }

    /// One main-loop pass at `now_us`: completes DMA started earlier,
    /// raises due timers, runs `tick` if due, then dispatches up to
    /// [`MAX_DISPATCH_PER_STEP`] queued events to `on_irq`. Returns the
    /// number of events popped.
    pub fn step(&mut self, now_us: u64) -> Result<usize, SimError> {
        self.guard()?;
        let dev = self.payload.device();
        dev.set_time_us(now_us);
        dev.periph().complete_dma(now_us);
        dev.periph().advance(now_us);
        let r = self.payload.tick((now_us / 1000) as u32);
        self.trap(r)?;
        let mut popped = 0;
        while popped < MAX_DISPATCH_PER_STEP {
            let Some(event) = self.payload.device().periph().pop() else {
                break;
            };
            popped += 1;
            let r = self.payload.dispatch(event);
            let handled = self.trap(r)?;
            self.payload
                .device()
                .events
                .push(SimEvent::Irq { event, handled });
        }
        Ok(popped)
    }

    /// Delivers a message; `Ok(false)` when no subscription matched.
    pub fn deliver(&mut self, key: &str, data: &[u8]) -> Result<bool, SimError> {
        self.guard()?;
        let key = Key::new(key).map_err(|_| SimError::InvalidKey)?;
        let r = self.payload.deliver(key, data);
        self.trap(r)
    }

    /// Hands a request to the payload serving `key` (ABI 5) with
    /// `deadline_ms` and `sender` in its meta block, and a [`REPLY_CAP`]
    /// reply buffer. `Ok(Ok(reply))` is the reply, `Ok(Err(e))` what a
    /// device answers instead (`NotFound` when nothing serves the key,
    /// `InvalidRequest` for an oversized body, ...). A trap stops the
    /// simulator like any other.
    pub fn request(
        &mut self,
        key: &str,
        body: &[u8],
        deadline_ms: Option<u32>,
        sender: Option<SenderId>,
    ) -> Result<Result<Vec<u8>, HandlerError>, SimError> {
        self.guard()?;
        let key = Key::new(key).map_err(|_| SimError::InvalidKey)?;
        let meta = RequestMeta {
            sender,
            principal: None,
            deadline_ms,
        };
        let mut reply = [0u8; REPLY_CAP];
        let r = self.payload.request(key, body, &meta, &mut reply);
        let r = self.trap(r)?;
        Ok(r.map(|n| reply.get(..n).unwrap_or_default().to_vec()))
    }

    /// Keys the payload serves (ABI 5), in slot order.
    pub fn serves(&self) -> Vec<String> {
        self.payload.serves().map(str::to_owned).collect()
    }

    /// Whether the payload exports `mw_alloc` and `on_request`.
    pub fn handles_requests(&self) -> bool {
        self.payload.handles_requests()
    }

    /// The device's node id.
    pub fn node(&mut self) -> SenderId {
        self.payload.device().node()
    }

    /// When the next tick is due, if the payload set a period.
    pub fn next_tick(&self) -> Option<u32> {
        self.payload.next_tick()
    }

    /// The payload's subscriptions.
    pub fn subscriptions(&self) -> Vec<String> {
        self.payload.subscriptions().map(str::to_owned).collect()
    }

    /// Whether the payload exports `on_irq`.
    pub fn handles_irq(&self) -> bool {
        self.payload.handles_irq()
    }

    /// Drives a virtual input pin (see [`SimDevice::set_input`]); the edge
    /// event, if any, is dispatched on the next step.
    pub fn set_input(&mut self, pin: u8, high: bool) -> bool {
        self.payload.device().set_input(pin, high)
    }

    /// The mode of a pin.
    pub fn pin(&mut self, pin: u8) -> Option<PinMode> {
        self.payload.device().pin(pin)
    }

    /// Feeds bytes to UART 1; returns how many were buffered. The `UartRx`
    /// event is dispatched on the next step.
    pub fn uart_inject(&mut self, data: &[u8]) -> usize {
        self.payload.device().uart_inject(data)
    }

    /// Sets what the virtual SPI device answers.
    pub fn set_spi_responder(&mut self, responder: SpiResponder) {
        self.payload.device().periph().set_responder(responder);
    }

    /// Sets the value `adc_read(pin)` returns (`0..=ADC_MAX`); an error
    /// message for a value above it or a pin beyond [`PIN_COUNT`].
    pub fn set_adc(&mut self, pin: u8, value: u16) -> Result<(), String> {
        self.payload.device().set_adc(pin, value)
    }

    /// The open PWM timers with their channels' duties.
    pub fn pwm(&mut self) -> Vec<PwmInfo> {
        self.payload.device().pwm().timers()
    }

    /// Timers the payload has started.
    pub fn timers(&mut self) -> Vec<TimerInfo> {
        self.payload.device().periph().timers()
    }

    /// Events dropped from the full queue since load.
    pub fn dropped(&mut self) -> u32 {
        self.payload.device().periph().dropped()
    }

    /// Events waiting for dispatch.
    pub fn pending(&mut self) -> usize {
        self.payload.device().periph().pending()
    }

    /// Takes the events recorded since the last drain.
    pub fn drain(&mut self) -> Vec<SimEvent> {
        self.payload.device().drain()
    }

    /// Whether a trap stopped the payload.
    pub const fn trapped(&self) -> bool {
        self.trapped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_records_events_and_reads_virtual_pins() {
        let mut d = SimDevice::new();
        d.log("hi");
        d.publish(Key::from_static("app/x"), &[1]).unwrap();
        d.gpio_output(2).unwrap();
        d.gpio_write(2, true).unwrap();
        assert_eq!(d.gpio_read(2), Ok(true));
        d.gpio_input(4, Pull::Up).unwrap();
        assert_eq!(d.gpio_read(4), Ok(true));
        assert!(d.set_input(4, false));
        assert_eq!(d.gpio_read(4), Ok(false));
        assert!(!d.set_input(2, false));
        assert_eq!(
            d.drain(),
            vec![
                SimEvent::Log("hi".into()),
                SimEvent::Publish {
                    key: "app/x".into(),
                    data: vec![1]
                },
                SimEvent::Output(2),
                SimEvent::Write(2, true),
                SimEvent::Input(4, Pull::Up),
            ]
        );
        assert!(d.drain().is_empty());
    }

    #[test]
    fn device_rejects_bad_pins_and_modes() {
        let mut d = SimDevice::new();
        assert_eq!(
            d.gpio_output(PIN_COUNT as u8),
            Err(AbiError::InvalidArgument)
        );
        assert_eq!(d.gpio_write(1, true), Err(AbiError::InvalidArgument));
        assert_eq!(d.gpio_read(1), Err(AbiError::InvalidArgument));
        d.gpio_input(1, Pull::Down).unwrap();
        assert_eq!(d.gpio_read(1), Ok(false));
        assert_eq!(d.gpio_write(1, true), Err(AbiError::InvalidArgument));
        assert_eq!(d.pin(1), Some(PinMode::Input(Pull::Down, None)));
        assert_eq!(d.pin(200), None);
    }

    const BLINK_ECHO: &str = r#"
        (module
          (import "mw" "log" (func $log (param i32 i32)))
          (import "mw" "publish" (func $publish (param i32 i32 i32 i32) (result i32)))
          (import "mw" "subscribe" (func $subscribe (param i32 i32) (result i32)))
          (import "mw" "set_tick" (func $set_tick (param i32) (result i32)))
          (import "mw" "gpio_output" (func $out (param i32) (result i32)))
          (import "mw" "gpio_input" (func $in (param i32 i32) (result i32)))
          (import "mw" "gpio_write" (func $write (param i32 i32) (result i32)))
          (import "mw" "gpio_read" (func $read (param i32) (result i32)))
          (memory (export "memory") 1)
          (data (i32.const 0) "start")
          (data (i32.const 16) "app/in")
          (data (i32.const 32) "app/out")
          (global $led (mut i32) (i32.const 0))
          (func (export "init")
            (call $log (i32.const 0) (i32.const 5))
            (drop (call $out (i32.const 2)))
            (drop (call $in (i32.const 5) (i32.const 1)))
            (drop (call $subscribe (i32.const 16) (i32.const 6)))
            (drop (call $set_tick (i32.const 100))))
          (func (export "tick") (param i32)
            (global.set $led (i32.xor (global.get $led) (call $read (i32.const 5))))
            (drop (call $write (i32.const 2) (global.get $led))))
          (func (export "mw_alloc") (param i32) (result i32) (i32.const 1024))
          (func (export "on_message") (param i32 i32 i32)
            (drop (call $publish (i32.const 32) (i32.const 7) (local.get 1) (local.get 2)))))
    "#;

    #[test]
    fn simulator_runs_payload_with_virtual_gpio_and_messages() {
        let wasm = wat::parse_str(BLINK_ECHO).unwrap();
        let mut sim = Simulator::load(&wasm, None).unwrap();
        assert_eq!(
            sim.drain(),
            vec![
                SimEvent::Log("start".into()),
                SimEvent::Output(2),
                SimEvent::Input(5, Pull::Up)
            ]
        );
        assert_eq!(sim.subscriptions(), ["app/in"]);
        let due = sim.next_tick().unwrap();
        sim.tick(due).unwrap();
        assert_eq!(sim.drain(), vec![SimEvent::Write(2, true)]);
        assert!(sim.set_input(5, false));
        let due = sim.next_tick().unwrap();
        sim.tick(due).unwrap();
        assert_eq!(sim.drain(), vec![SimEvent::Write(2, true)]);
        assert_eq!(sim.pin(2), Some(PinMode::Output(true)));

        assert_eq!(sim.deliver("app/in", b"ping"), Ok(true));
        assert_eq!(
            sim.drain(),
            vec![SimEvent::Publish {
                key: "app/out".into(),
                data: b"ping".to_vec()
            }]
        );
        assert_eq!(sim.deliver("app/other", b"x"), Ok(false));
        assert_eq!(sim.deliver("bad//", b"x"), Err(SimError::InvalidKey));
    }

    #[test]
    fn simulator_reports_rejection_and_traps() {
        let err = Simulator::load(b"not wasm", None).err().unwrap();
        assert!(matches!(err, SimError::Load(_)));
        let spin = wat::parse_str(
            r#"(module (import "mw" "set_tick" (func $t (param i32) (result i32)))
                 (memory (export "memory") 1)
                 (func (export "init") (drop (call $t (i32.const 10))))
                 (func (export "tick") (param i32) (loop $l (br $l))))"#,
        )
        .unwrap();
        let mut sim = Simulator::load(&spin, None).unwrap();
        let due = sim.next_tick().unwrap();
        assert!(matches!(sim.tick(due), Err(SimError::Trap(_))));
        assert!(sim.trapped());
        assert_eq!(sim.tick(due + 10), Err(SimError::Stopped));
        assert!(SimError::Stopped.describe().contains("reload"));
    }
    fn fixture(src: &str) -> Simulator {
        Simulator::load(&wat::parse_str(src).unwrap(), None).unwrap()
    }

    fn publishes(events: &[SimEvent], key: &str) -> Vec<Vec<u8>> {
        events
            .iter()
            .filter_map(|e| match e {
                SimEvent::Publish { key: k, data } if k == key => Some(data.clone()),
                _ => None,
            })
            .collect()
    }

    fn last_i32(events: &[SimEvent], key: &str) -> Option<i32> {
        let data = publishes(events, key).pop()?;
        Some(i32::from_le_bytes(data.try_into().ok()?))
    }

    const TIMER: &str = include_str!("../../test/fixtures/timer_count.wat");
    const BUTTON: &str = include_str!("../../test/fixtures/button_irq.wat");
    const SPI: &str = include_str!("../../test/fixtures/spi_loopback.wat");
    const UART: &str = include_str!("../../test/fixtures/uart_echo.wat");
    const OVERFLOW: &str = include_str!("../../test/fixtures/overflow.wat");
    const PWM_ADC: &str = include_str!("../../test/fixtures/pwm_adc.wat");

    #[test]
    fn d10_periodic_timer_ticks_are_counted_in_virtual_time() {
        let mut sim = fixture(TIMER);
        assert!(sim.handles_irq());
        let mut events = Vec::new();
        for ms in (10..=100).step_by(10) {
            assert_eq!(sim.step(ms * 1000), Ok(1), "one coalesced event per step");
            events.extend(sim.drain());
        }
        assert_eq!(last_i32(&events, "app/ticks"), Some(100));
        assert_eq!(sim.timers()[0].fires, 100);
        let irq = events.iter().find_map(|e| match e {
            SimEvent::Irq { event, handled } => Some((event.kind, event.value, *handled)),
            _ => None,
        });
        assert_eq!(irq, Some((wasm_host::EventKind::Timer, 10, true)));
    }

    #[test]
    fn d10_button_edge_reaches_on_irq() {
        let mut sim = fixture(BUTTON);
        sim.drain();
        sim.set_time_us(5_000);
        assert!(
            sim.set_input(4, true),
            "rising: no event for a falling-edge IRQ"
        );
        assert_eq!(sim.pending(), 0, "already high (pull-up)");
        assert!(sim.set_input(4, false));
        assert_eq!(sim.pending(), 1);
        assert_eq!(sim.step(6_000), Ok(1));
        let events = sim.drain();
        assert!(events.contains(&SimEvent::Write(8, true)));
        assert!(events.contains(&SimEvent::Log("press".into())));
        assert!(events.iter().any(|e| matches!(e,
            SimEvent::Irq { event, .. } if event.time_us == 5_000 && event.value == 0)));
        sim.set_input(4, true);
        assert_eq!(sim.step(7_000), Ok(0), "release is a rising edge");
        assert_eq!(sim.pin(8), Some(PinMode::Output(true)));
    }

    #[test]
    fn d10_spi_loopback_round_trip_through_dma_read() {
        let mut sim = fixture(SPI);
        let init = sim.drain();
        let tx = vec![0xde, 0xad, 0xbe, 0xef];
        assert_eq!(
            init,
            [SimEvent::Spi(SpiTransfer {
                tx: tx.clone(),
                rx: tx.clone()
            })]
        );
        assert_eq!(sim.step(1_000), Ok(1));
        assert_eq!(publishes(&sim.drain(), "app/spi"), [tx]);
    }

    #[test]
    fn d10_spi_pattern_responder_answers_device_transfers() {
        let mut d = SimDevice::new();
        d.periph()
            .set_responder(SpiResponder::from_pattern(&[0x42, 0x43]));
        d.spi_open(SpiConfig::parse(0, 6, 7, 2, -1, 1000, 0).unwrap())
            .unwrap();
        d.spi_transfer(0, &[1], 3).unwrap();
        assert_eq!(
            d.drain(),
            [SimEvent::Spi(SpiTransfer {
                tx: vec![1, 0, 0],
                rx: vec![0x42, 0x43, 0x42]
            })]
        );
        assert_eq!(
            d.dma_read(DmaChannel::Spi0, &mut [0; 3]),
            Err(AbiError::Busy)
        );
    }

    #[test]
    fn d10_uart_echo() {
        let mut sim = fixture(UART);
        assert_eq!(sim.uart_inject(b"hello"), 5);
        assert_eq!(sim.step(1_000), Ok(1));
        let events = sim.drain();
        assert!(events.contains(&SimEvent::Uart(b"hello".to_vec())));
        assert_eq!(sim.step(2_000), Ok(1), "DmaDone(Uart1Tx) on the next step");
        assert!(sim.drain().contains(&SimEvent::Log("sent".into())));
    }

    #[test]
    fn d10_flooded_queue_reports_overflow() {
        let mut sim = fixture(OVERFLOW);
        let extra = 8;
        for i in 0..(MAX_EVENTS + extra) {
            sim.set_input(4, i % 2 == 1);
        }
        assert_eq!(sim.pending(), MAX_EVENTS);
        assert_eq!(sim.dropped(), extra as u32);
        assert_eq!(sim.step(1_000), Ok(MAX_DISPATCH_PER_STEP));
        let events = sim.drain();
        assert_eq!(publishes(&events, "app/overflow").len(), 1, "reported once");
        assert_eq!(last_i32(&events, "app/overflow"), Some(extra as i32));
        assert_eq!(sim.dropped(), extra as u32, "UI total kept");
    }

    #[test]
    fn d10_dispatch_is_bounded_per_step_and_events_without_on_irq_are_discarded() {
        let mut sim = fixture(
            r#"(module
                 (import "mw" "gpio_input" (func $in (param i32 i32) (result i32)))
                 (import "mw" "gpio_irq" (func $irq (param i32 i32) (result i32)))
                 (memory (export "memory") 1)
                 (func (export "init")
                   (drop (call $in (i32.const 4) (i32.const 0)))
                   (drop (call $irq (i32.const 4) (i32.const 3)))))"#,
        );
        assert!(!sim.handles_irq());
        sim.set_input(4, true);
        assert_eq!(sim.step(1), Ok(1));
        assert!(
            sim.drain()
                .iter()
                .any(|e| matches!(e, SimEvent::Irq { handled: false, .. }))
        );
    }

    #[test]
    fn d10_irq_requires_an_input_and_reconfiguring_disarms() {
        let mut d = SimDevice::new();
        assert_eq!(d.gpio_irq(3, Edge::Any), Err(AbiError::InvalidArgument));
        d.gpio_input(3, Pull::None).unwrap();
        d.gpio_irq(3, Edge::Any).unwrap();
        d.set_input(3, true);
        assert_eq!(d.periph().pending(), 1);
        d.gpio_output(3).unwrap();
        assert_eq!(d.periph().pending(), 0, "purged");
        assert_eq!(d.periph().edge(3), Edge::Off);
        d.set_time_us(2_500_000);
        assert_eq!((d.now_ms(), d.now_us()), (2_500, 2_500_000));
    }

    /// The worked example's LED unit, as generated by the web editor
    /// (kept current by web/test/phase.gen.test.ts).
    const PICO_LED: &[u8] =
        include_bytes!("../../../tools/payload-sim/tests/fixtures/phase/pico-led.wasm");

    #[test]
    fn q19_a_phase_unit_serves_its_phase_under_its_node_id() {
        let node = SenderId([0x24, 0x6f, 0x28, 1, 2, 3, 0, 0]);
        let mut sim = Simulator::load_on(PICO_LED, None, SimDevice::new().with_node(node)).unwrap();
        let key = "unit/246f280102030000/phase/LED.Blink";
        assert_eq!(sim.serves(), [key]);
        assert!(sim.handles_requests());
        assert_eq!(sim.node(), node);
        let init = sim.drain();
        assert!(init.contains(&SimEvent::Serve(key.into())), "{init:?}");
        // Start #1 with period 250 ms, 3 times: accepted, now Running.
        let mut buf = [0u8; recipe_core::wire::MAX_BODY];
        let body =
            recipe_core::wire::encode_command(recipe_core::Command::Start, 1, &[250, 3], &mut buf)
                .unwrap();
        let reply = sim.request(key, body, Some(1_000), None).unwrap().unwrap();
        assert_eq!(
            recipe_core::wire::decode_ack(&reply).unwrap(),
            (
                1,
                recipe_core::engine::Ack::Accepted(recipe_core::PhaseState::Running)
            )
        );
        // Out of the class's range: InvalidRequest, no state change.
        let body =
            recipe_core::wire::encode_command(recipe_core::Command::Start, 2, &[1, 3], &mut buf)
                .unwrap();
        assert_eq!(
            sim.request(key, body, None, None),
            Ok(Err(HandlerError::InvalidRequest))
        );
        assert_eq!(
            sim.request("unit/246f280102030000/phase/LED.Other", &[], None, None),
            Ok(Err(HandlerError::NotFound))
        );
        assert_eq!(
            sim.request("bad//", &[], None, None),
            Err(SimError::InvalidKey)
        );
        // The default node is payload-sim's.
        let mut other = Simulator::load(PICO_LED, None).unwrap();
        assert_eq!(other.serves(), ["unit/0000000000000001/phase/LED.Blink"]);
        assert_eq!(other.node(), DEFAULT_NODE);
    }

    #[test]
    fn d10_trap_in_on_irq_stops_the_simulator() {
        let mut sim = fixture(
            r#"(module
                 (import "mw" "timer_start" (func $t (param i32 i32 i32) (result i32)))
                 (memory (export "memory") 1)
                 (func (export "init") (drop (call $t (i32.const 1) (i32.const 100) (i32.const 0))))
                 (func (export "on_irq") (param i32 i32 i32 i32) (loop $l (br $l))))"#,
        );
        assert!(matches!(sim.step(200), Err(SimError::Trap(_))));
        assert_eq!(sim.step(300), Err(SimError::Stopped));
    }

    #[test]
    fn x15_device_records_pwm_events_and_reads_set_adc_values() {
        let mut d = SimDevice::new();
        assert_eq!(d.pwm_set(0, 0, 1), Err(AbiError::Busy), "not open");
        assert_eq!(d.adc_read(4), Ok(0), "0 until set");
        d.set_adc(4, 4095).unwrap();
        assert!(d.set_adc(4, 4096).is_err());
        assert!(d.set_adc(PIN_COUNT as u8, 1).is_err());
        assert_eq!(d.adc_read(4), Ok(4095));
        assert_eq!(d.adc_read(PIN_COUNT as u8), Err(AbiError::Unsupported));
        d.pwm_open(PwmSpec::parse(5, 20_000).unwrap()).unwrap();
        d.pwm_set(5, 3, 65_535).unwrap();
        d.pwm_stop(5).unwrap();
        assert_eq!(
            d.drain(),
            [
                SimEvent::PwmOpen {
                    timer: 5,
                    freq_hz: 20_000
                },
                SimEvent::PwmSet {
                    timer: 5,
                    channel: 3,
                    duty: 65_535
                },
                SimEvent::PwmStop(5),
            ]
        );
    }

    #[test]
    fn x15_payload_reads_adc_drives_pwm_and_stop_makes_set_busy() {
        let mut sim = fixture(PWM_ADC);
        assert_eq!(
            sim.drain(),
            [SimEvent::PwmOpen {
                timer: 0,
                freq_hz: 1000
            }]
        );
        assert_eq!(
            sim.pwm(),
            [PwmInfo {
                timer: 0,
                freq_hz: 1000,
                duty: [0; 4]
            }]
        );
        sim.set_adc(4, 1000).unwrap();
        sim.tick(10).unwrap();
        let events = sim.drain();
        assert_eq!(last_i32(&events, "app/adc"), Some(1000));
        assert!(events.contains(&SimEvent::PwmSet {
            timer: 0,
            channel: 1,
            duty: 16_000
        }));
        sim.set_adc(4, 4095).unwrap();
        sim.tick(20).unwrap();
        assert_eq!(last_i32(&sim.drain(), "app/adc"), Some(4095));
        assert_eq!(sim.pwm()[0].duty, [0, 65_520, 0, 0]);
        sim.tick(30).unwrap();
        let events = sim.drain();
        assert!(events.contains(&SimEvent::PwmStop(0)));
        assert_eq!(last_i32(&events, "app/err"), Some(AbiError::Busy.code()));
        assert!(sim.pwm().is_empty());
    }
}
