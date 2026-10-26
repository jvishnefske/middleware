//! WASM payload runtime: loads a payload module, links the host imports of
//! [`middleware::wasm_abi`] to a [`Device`], and drives its exports.
//!
//! The crate is `#![no_std]` with `alloc` and runs wherever wasmi runs: the
//! ESP base firmware (with `esp-alloc`), the browser simulator (compiled to
//! `wasm32-unknown-unknown`) and the desktop `payload-sim`.
//!
//! # Lifecycle
//!
//! 1. [`Payload::load`] validates the module (requirement W1): every import
//!    must be an `mw` function from [`wasm_abi::IMPORTS`] with the listed
//!    signature, `memory` must be exported and start within
//!    [`Limits::max_memory_bytes`], and the optional exports `init`, `tick`,
//!    `mw_alloc` and `on_message` must have their ABI signatures. It then
//!    instantiates the module and calls `init` if exported. If `init` traps,
//!    the error, [`LoadError::Init`], carries an [`InitLog`] with the last
//!    [`INIT_LOG_LINES`] lines `init` logged before the trap (W8).
//! 2. The firmware loop asks [`Payload::next_tick`] when the next `tick` is
//!    due (an absolute `now_ms`, wrapping) and calls [`Payload::tick`]; the
//!    payload chooses the period with `set_tick` (W2). `tick` reports with
//!    [`Ticked`] whether the export actually ran (W9).
//! 3. Messages for subscribed keys go through [`Payload::deliver`], which
//!    matches the key exactly, copies the data into a buffer from
//!    `mw_alloc`, and calls `on_message(slot, ptr, len)` (W2). `mw_alloc`
//!    returning 0 (or an out-of-memory range) means "not delivered".
//!
//! # Safety limits
//!
//! * Host imports validate every pointer, pin, level, pull, key (no `sys/`),
//!   period, the subscription limit and (ABI 2) every edge, timer, SPI and
//!   UART argument and (ABI 3) every PWM, ADC and I2C argument with the
//!   shared [`wasm_abi::periph`] parsers, returning
//!   [`AbiError`] codes instead of trapping (W3). Messages over
//!   [`wasm_abi::MAX_MESSAGE`] are refused; `log` lines are truncated to
//!   [`wasm_abi::MAX_LOG`] bytes on a character boundary.
//! * SPI and UART transfers are copied through a host buffer of at most
//!   [`periph::MAX_DMA`](wasm_abi::periph::MAX_DMA) bytes; the device never
//!   sees guest memory. `dma_read` / `uart_read` copy at most that buffer's
//!   worth back and return the count.
//! * `i2c_transfer` (ABI 3) copies through two stack buffers of
//!   [`periph::MAX_I2C`](wasm_abi::periph::MAX_I2C) bytes: the transmit
//!   bytes are copied out of guest memory before the device is called, and
//!   the received bytes are copied back only if the device succeeds.
//!   `adc_read` results are clamped to
//!   [`periph::ADC_MAX`](wasm_abi::periph::ADC_MAX).
//! * Every call into the payload (`init`, `tick`, `mw_alloc`, `on_message`,
//!   `on_irq`) is refuelled to [`Limits::fuel_per_call`] first, so an infinite loop
//!   ends with [`Trap::OutOfFuel`] instead of hanging the host (W4).
//! * Linear memory is capped at [`Limits::max_memory_bytes`], at load time
//!   (the module's initial memory) and on every `memory.grow` (which then
//!   returns `-1`) (W5). The cap is in bytes, not pages, because modules may
//!   use either page size:
//!   * standard 64 KiB pages, and
//!   * 1-byte pages from the WebAssembly custom-page-sizes proposal (enabled
//!     in the engine), which let a payload ask for, say, 12 KiB instead of
//!     a whole 64 KiB page. The Rust SDK builds this way with
//!     `-C link-arg=--page-size=1` (see `payloads/README.md`).
//!
//!   The page size is read from the module's memory section
//!   ([`abi::initial_memory_bytes`]) because wasmi does not expose it.
//!
//! # Interpreter build
//!
//! wasmi is built with `validate` (the safe `Module::new`) and
//! `portable-dispatch`, the loop-based instruction dispatcher. The default
//! tail-call dispatcher needs LLVM's sibling-call optimisation; without it
//! (unoptimised builds, and targets LLVM cannot tail-call on, such as
//! Xtensa) every executed instruction nests a native stack frame, and a long
//! guest loop running down its fuel overflows the firmware's stack.
//! `auto-dispatch` would pick the loop dispatcher in exactly those cases, but
//! `portable-dispatch` makes the choice explicit and identical on every
//! target (ESP32, ESP32-S3, the browser and the desktop simulator), so a
//! payload's native stack use does not depend on the build. The cost is
//! some interpreter speed, which [`Limits::fuel_per_call`] already bounds.
//! * Any trap poisons the payload: later calls return [`Trap::Poisoned`] and
//!   [`Payload::next_tick`] returns `None`, because guest state may be
//!   inconsistent. Reload to recover.
//!
//! Decision logic lives in pure functions in [`abi`]; [`link`] and
//! [`Payload`] are the thin wasmi shell around it.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod abi;
mod error;
pub mod link;

use alloc::string::ToString;
use alloc::vec::Vec;

use middleware::wasm_abi::{self, MAX_MESSAGE};
use wasmi::{
    Config, Engine, ExternType, FuncType, Instance, Module, Store, StoreLimitsBuilder, TrapCode,
    ValType,
};

pub use error::{INIT_LOG_LINES, InitLog, LoadError, Trap};
pub use middleware::wasm_abi::events::{DmaChannel, Event, EventKind};
pub use middleware::wasm_abi::periph::{
    Edge, I2cConfig, I2cTransfer, PwmDuty, PwmSpec, SpiConfig, TimerSpec, UartConfig,
};
pub use middleware::wasm_abi::{AbiError, Pull};

use abi::{ImportKind, Shape};
use link::{Exports, State};

/// The board a payload drives. The firmware implements it over real
/// peripherals and the serial link; simulators implement it in memory.
///
/// Arguments have already been validated against the ABI; implementations
/// only refuse what the board cannot do (reserved pins → `Unsupported`,
/// link failures → `Transport`, a peripheral not opened or still busy →
/// `Busy`).
///
/// The ABI 2 methods (from [`Device::now_us`] on) and the ABI 3 methods
/// (from [`Device::pwm_open`] on) have defaults (`Unsupported`, or a
/// derived clock) so a board without interrupts, DMA, PWM, ADC or I2C keeps
/// compiling; see the crate docs.
pub trait Device {
    /// A payload log line (UTF-8, at most [`wasm_abi::MAX_LOG`] bytes).
    fn log(&mut self, line: &str);
    /// Publishes `data` (at most [`wasm_abi::MAX_MESSAGE`] bytes) on a
    /// non-`sys` key.
    fn publish(&mut self, key: middleware::Key<'_>, data: &[u8]) -> Result<(), AbiError>;
    /// Milliseconds since start, wrapping.
    fn now_ms(&mut self) -> u32;
    /// Configures `pin` as a push-pull output.
    fn gpio_output(&mut self, pin: u8) -> Result<(), AbiError>;
    /// Configures `pin` as an input with `pull`.
    fn gpio_input(&mut self, pin: u8, pull: Pull) -> Result<(), AbiError>;
    /// Drives an output pin.
    fn gpio_write(&mut self, pin: u8, high: bool) -> Result<(), AbiError>;
    /// Reads a pin level.
    fn gpio_read(&mut self, pin: u8) -> Result<bool, AbiError>;

    /// Microseconds since start, wrapping. Defaults to `now_ms * 1000`.
    fn now_us(&mut self) -> u32 {
        self.now_ms().wrapping_mul(1000)
    }
    /// Events dropped since the last call; the device resets its count.
    /// Defaults to 0.
    fn irq_overflow(&mut self) -> u32 {
        0
    }
    /// Enables (or with [`Edge::Off`] disables) the interrupt on an input
    /// pin.
    fn gpio_irq(&mut self, pin: u8, edge: Edge) -> Result<(), AbiError> {
        let _ = (pin, edge);
        Err(AbiError::Unsupported)
    }
    /// Starts (or restarts) a hardware timer.
    fn timer_start(&mut self, spec: TimerSpec) -> Result<(), AbiError> {
        let _ = spec;
        Err(AbiError::Unsupported)
    }
    /// Stops a timer and purges its pending events.
    fn timer_stop(&mut self, id: u8) -> Result<(), AbiError> {
        let _ = id;
        Err(AbiError::Unsupported)
    }
    /// Configures an SPI master bus.
    fn spi_open(&mut self, cfg: SpiConfig) -> Result<(), AbiError> {
        let _ = cfg;
        Err(AbiError::Unsupported)
    }
    /// Starts a full-duplex transfer of `len` bytes (`len = max(tx_len,
    /// rx_len)`, `tx.len() <= len`); the device pads `tx` with `0x00`.
    /// Completion is a [`EventKind::DmaDone`] event on
    /// [`DmaChannel::Spi0`].
    fn spi_transfer(&mut self, bus: u8, tx: &[u8], len: usize) -> Result<(), AbiError> {
        let _ = (bus, tx, len);
        Err(AbiError::Unsupported)
    }
    /// Copies received DMA data into `out` (at most
    /// [`periph::MAX_DMA`](wasm_abi::periph::MAX_DMA) bytes); returns the
    /// count.
    fn dma_read(&mut self, channel: DmaChannel, out: &mut [u8]) -> Result<usize, AbiError> {
        let _ = (channel, out);
        Err(AbiError::Unsupported)
    }
    /// Configures the secondary UART.
    fn uart_open(&mut self, cfg: UartConfig) -> Result<(), AbiError> {
        let _ = cfg;
        Err(AbiError::Unsupported)
    }
    /// Starts transmitting `data` (1..=`MAX_DMA` bytes); completion is a
    /// [`EventKind::DmaDone`] event on [`DmaChannel::Uart1Tx`].
    fn uart_write(&mut self, port: u8, data: &[u8]) -> Result<(), AbiError> {
        let _ = (port, data);
        Err(AbiError::Unsupported)
    }
    /// Moves buffered received bytes into `out`; returns the count (may be
    /// 0).
    fn uart_read(&mut self, port: u8, out: &mut [u8]) -> Result<usize, AbiError> {
        let _ = (port, out);
        Err(AbiError::Unsupported)
    }

    /// Starts PWM timer `spec.timer` at `spec.freq_hz` with every channel
    /// at duty 0 (ABI 3). Reopening an open timer changes its frequency.
    /// The board maps `(timer, channel)` to fixed pins.
    fn pwm_open(&mut self, spec: PwmSpec) -> Result<(), AbiError> {
        let _ = spec;
        Err(AbiError::Unsupported)
    }
    /// Sets one channel's duty, a fraction of `u16::MAX` (ABI 3); `Busy`
    /// if the timer is not open.
    fn pwm_set(&mut self, timer: u8, channel: u8, duty: u16) -> Result<(), AbiError> {
        let _ = (timer, channel, duty);
        Err(AbiError::Unsupported)
    }
    /// Stops a PWM timer and drives its outputs low (ABI 3).
    fn pwm_stop(&mut self, timer: u8) -> Result<(), AbiError> {
        let _ = timer;
        Err(AbiError::Unsupported)
    }
    /// Samples the analog input on `pin` (ABI 3); the result is 12-bit
    /// (`0..=ADC_MAX`; larger values are clamped by the runtime).
    /// `Unsupported` for a pin without an analog input.
    fn adc_read(&mut self, pin: u8) -> Result<u16, AbiError> {
        let _ = pin;
        Err(AbiError::Unsupported)
    }
    /// Configures I2C controller `cfg.bus` at `cfg.freq_hz` (ABI 3).
    fn i2c_open(&mut self, cfg: I2cConfig) -> Result<(), AbiError> {
        let _ = cfg;
        Err(AbiError::Unsupported)
    }
    /// Blocking transfer to the 7-bit address `addr` (ABI 3): writes `tx`,
    /// then (repeated start) fills `rx`; either may be empty (both empty is
    /// an address probe). Each is at most
    /// [`periph::MAX_I2C`](wasm_abi::periph::MAX_I2C) bytes. A missing
    /// acknowledge or bus error is `Transport`; a bus not opened is `Busy`.
    fn i2c_transfer(
        &mut self,
        bus: u8,
        addr: u8,
        tx: &[u8],
        rx: &mut [u8],
    ) -> Result<(), AbiError> {
        let _ = (bus, addr, tx, rx);
        Err(AbiError::Unsupported)
    }
}

/// Resource limits for one payload.
///
/// Build with [`Limits::pages`] or [`Limits::bytes`] and adjust the fuel
/// with [`Limits::with_fuel`]; the fields are public for direct use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Largest linear memory, in bytes, whatever the module's page size.
    /// Checked against the module's initial memory at load and enforced on
    /// every `memory.grow`.
    pub max_memory_bytes: u32,
    /// Fuel granted to every call into the payload (`init`, `tick`,
    /// `mw_alloc`, `on_message`, `on_irq`). This is the knob that bounds how
    /// long one call may run: roughly one unit per executed instruction, so
    /// an infinite loop ends with [`Trap::OutOfFuel`] after this many. The
    /// default is [`wasm_abi::FUEL_PER_CALL`].
    pub fuel_per_call: u64,
}

impl Limits {
    /// A cap of `n` 64 KiB pages (saturating at `u32::MAX` bytes) and the
    /// default fuel.
    pub const fn pages(n: u32) -> Self {
        let bytes = (n as u64).saturating_mul(abi::PAGE_BYTES as u64);
        Limits::bytes(if bytes > u32::MAX as u64 {
            u32::MAX
        } else {
            bytes as u32
        })
    }

    /// A cap of `n` bytes and the default fuel.
    pub const fn bytes(n: u32) -> Self {
        Limits {
            max_memory_bytes: n,
            fuel_per_call: wasm_abi::FUEL_PER_CALL,
        }
    }

    /// The same limits with `fuel` per call.
    pub const fn with_fuel(self, fuel: u64) -> Self {
        Limits {
            fuel_per_call: fuel,
            ..self
        }
    }
}

impl Default for Limits {
    /// Two 64 KiB pages (128 KiB) and [`wasm_abi::FUEL_PER_CALL`].
    fn default() -> Self {
        Limits::pages(2)
    }
}

/// What [`Payload::tick`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ticked {
    /// A tick was due: the schedule advanced and the `tick` export ran
    /// (and returned).
    Ran,
    /// A tick is scheduled but not yet due at the given `now_ms`; nothing
    /// ran.
    NotDue,
    /// The module exports `tick` but no period is set (`set_tick` was never
    /// called, or was called with 0); nothing ran.
    Disabled,
    /// The module does not export `tick`, so it can never run.
    NoTickExport,
}

/// A loaded, running payload bound to a device.
///
/// `D: 'static` is required because wasmi stores host closures and the
/// store's resource limiter as `'static` values parameterised by `D`.
pub struct Payload<D: Device + 'static> {
    store: Store<State<D>>,
    instance: Instance,
    fuel: u64,
    poisoned: bool,
}

/// Converts a wasmi function type to an all-`i32` [`Shape`].
fn shape(ty: &FuncType) -> Option<Shape> {
    let all_i32 = |v: &[ValType]| v.iter().all(|t| *t == ValType::I32);
    (all_i32(ty.params()) && all_i32(ty.results())).then(|| Shape {
        params: ty.params().len(),
        results: ty.results().len(),
    })
}

/// Classifies a wasmi error as a [`Trap`].
fn trap(e: &wasmi::Error) -> Trap {
    match e.as_trap_code() {
        Some(TrapCode::OutOfFuel) => Trap::OutOfFuel,
        _ => Trap::Wasm(e.to_string()),
    }
}

/// Checks imports and exports of a compiled module; returns the optional
/// exports it provides.
fn validate(wasm: &[u8], module: &Module, limits: &Limits) -> Result<Exports, LoadError> {
    for import in module.imports() {
        let kind = match import.ty() {
            ExternType::Func(f) => ImportKind::Func(shape(f)),
            _ => ImportKind::Other,
        };
        abi::check_import(import.module(), import.name(), kind)?;
    }
    let mut exports = Exports::default();
    let (mut alloc, mut on_message, mut memory) = (false, false, false);
    for export in module.exports() {
        match export.ty() {
            ExternType::Func(f) => {
                abi::check_export(export.name(), shape(f))?;
                match export.name() {
                    wasm_abi::exports::INIT => exports.init = true,
                    wasm_abi::exports::TICK => exports.tick = true,
                    wasm_abi::exports::ALLOC => alloc = true,
                    wasm_abi::exports::ON_MESSAGE => on_message = true,
                    wasm_abi::exports::ON_IRQ => exports.irq = true,
                    _ => {}
                }
            }
            ExternType::Memory(_) if export.name() == wasm_abi::exports::MEMORY => {
                memory = true;
            }
            _ => {}
        }
    }
    if !memory {
        return Err(LoadError::MissingMemory);
    }
    let initial = abi::initial_memory_bytes(wasm).unwrap_or(u64::MAX);
    abi::check_memory(initial, limits.max_memory_bytes)?;
    exports.messages = alloc && on_message;
    Ok(exports)
}

impl<D: Device + 'static> Payload<D> {
    /// Validates and instantiates `wasm`, then calls `init` if exported.
    ///
    /// While `init` runs, its log lines are forwarded to [`Device::log`] and
    /// also kept in a bounded [`InitLog`] (the last [`INIT_LOG_LINES`]). If
    /// `init` traps, the device is dropped and the error is
    /// [`LoadError::Init`] carrying the trap and that log, so the caller can
    /// show why `init` failed. On success the log is discarded.
    pub fn load(wasm: &[u8], device: D, limits: Limits) -> Result<Self, LoadError> {
        let mut config = Config::default();
        config.consume_fuel(true).wasm_custom_page_sizes(true);
        let engine = Engine::new(&config);
        let module = Module::new(&engine, wasm).map_err(|e| LoadError::Invalid(e.to_string()))?;
        let exports = validate(wasm, &module, &limits)?;
        let state = State {
            device,
            subscriptions: Vec::new(),
            schedule: None,
            exports,
            limits: StoreLimitsBuilder::new()
                .memory_size(abi::memory_bytes(limits.max_memory_bytes))
                .memories(1)
                .instances(1)
                .build(),
            scratch: Vec::new(),
            init_log: None,
        };
        let mut store = Store::new(&engine, state);
        store.limiter(|s| &mut s.limits);
        let instantiate = |e: wasmi::Error| LoadError::Instantiate(e.to_string());
        let linker = link::linker::<D>(&engine).map_err(instantiate)?;
        store.set_fuel(limits.fuel_per_call).map_err(instantiate)?;
        let instance = linker
            .instantiate_and_start(&mut store, &module)
            .map_err(instantiate)?;
        let mut payload = Payload {
            store,
            instance,
            fuel: limits.fuel_per_call,
            poisoned: false,
        };
        if exports.init {
            payload.store.data_mut().init_log = Some(InitLog::new());
            let result = payload.call::<(), ()>(wasm_abi::exports::INIT, ());
            let log = payload.store.data_mut().init_log.take();
            result.map_err(|trap| LoadError::Init {
                trap,
                log: log.unwrap_or_default(),
            })?;
        }
        Ok(payload)
    }

    /// Refuels and calls the export `name`, poisoning the payload on a trap.
    fn call<P, R>(&mut self, name: &str, params: P) -> Result<R, Trap>
    where
        P: wasmi::WasmParams,
        R: wasmi::WasmResults,
    {
        if self.poisoned {
            return Err(Trap::Poisoned);
        }
        let result = self
            .store
            .set_fuel(self.fuel)
            .and_then(|()| self.instance.get_typed_func::<P, R>(&self.store, name))
            .and_then(|f| f.call(&mut self.store, params))
            .map_err(|e| trap(&e));
        self.poisoned = result.is_err();
        result
    }

    /// Keys the payload subscribed to, in slot order.
    pub fn subscriptions(&self) -> impl Iterator<Item = &str> {
        self.store.data().subscriptions.iter().map(|s| s.as_str())
    }

    /// Absolute `now_ms` at which the next `tick` is due; `None` when ticking
    /// is disabled or the payload is poisoned.
    pub fn next_tick(&self) -> Option<u32> {
        if self.poisoned {
            return None;
        }
        self.store.data().schedule.map(|s| s.due)
    }

    /// Calls `tick(now_ms)` if a tick is due at `now_ms`, after advancing the
    /// schedule by one period (missed periods are skipped), and returns
    /// [`Ticked::Ran`]. When nothing runs it returns why:
    /// [`Ticked::NotDue`], [`Ticked::Disabled`] (no period set) or
    /// [`Ticked::NoTickExport`]. A `set_tick` inside the callback replaces
    /// the schedule.
    ///
    /// Errors: the trap of the `tick` call (which poisons the payload), or
    /// [`Trap::Poisoned`] after an earlier trap.
    pub fn tick(&mut self, now_ms: u32) -> Result<Ticked, Trap> {
        if self.poisoned {
            return Err(Trap::Poisoned);
        }
        let state = self.store.data_mut();
        match abi::tick_decision(state.exports.tick, state.schedule, now_ms) {
            (Ticked::Ran, next) => state.schedule = next,
            (other, _) => return Ok(other),
        }
        self.call::<i32, ()>(wasm_abi::exports::TICK, now_ms as i32)
            .map(|()| Ticked::Ran)
    }

    /// Delivers a message. Returns `Ok(true)` if `key` exactly matches a
    /// subscription and `on_message` ran. Returns `Ok(false)` (not
    /// delivered, payload still healthy) if nothing is subscribed to `key`,
    /// `data` exceeds [`wasm_abi::MAX_MESSAGE`], or `mw_alloc` refused the
    /// message by returning 0 or a buffer outside linear memory.
    pub fn deliver(&mut self, key: middleware::Key<'_>, data: &[u8]) -> Result<bool, Trap> {
        if self.poisoned {
            return Err(Trap::Poisoned);
        }
        let Some(slot) = abi::slot_for(&self.store.data().subscriptions, key.as_str()) else {
            return Ok(false);
        };
        if data.len() > MAX_MESSAGE {
            return Ok(false);
        }
        // Both fit in i32: slot < MAX_SUBSCRIPTIONS, len <= MAX_MESSAGE.
        let (slot, len) = (slot as i32, data.len() as i32);
        let ptr = self.call::<i32, i32>(wasm_abi::exports::ALLOC, len)?;
        let memory = self
            .instance
            .get_memory(&self.store, wasm_abi::exports::MEMORY);
        let written = memory.and_then(|m| {
            let range = abi::alloc_range(ptr, len, m.data_size(&self.store))?;
            m.write(&mut self.store, range.start, data).ok()
        });
        if written.is_none() {
            // The guest refused the message (0) or returned an unusable buffer.
            return Ok(false);
        }
        self.call::<(i32, i32, i32), ()>(wasm_abi::exports::ON_MESSAGE, (slot, ptr, len))?;
        Ok(true)
    }

    /// Whether the module exports `on_irq` (ABI 2).
    pub fn handles_irq(&self) -> bool {
        self.store.data().exports.irq
    }

    /// Delivers one interrupt event by calling
    /// `on_irq(kind, source, value, time_us)` with fresh fuel. Returns
    /// `Ok(false)` (event dropped, payload healthy) when `on_irq` is not
    /// exported, and [`Trap::Poisoned`] after an earlier trap.
    pub fn dispatch(&mut self, event: Event) -> Result<bool, Trap> {
        if self.poisoned {
            return Err(Trap::Poisoned);
        }
        if !self.handles_irq() {
            return Ok(false);
        }
        let [kind, source, value, time] = event.to_args();
        self.call::<(i32, i32, i32, i32), ()>(
            wasm_abi::exports::ON_IRQ,
            (kind, source, value, time),
        )?;
        Ok(true)
    }

    /// Whether an earlier call trapped (see [`Trap::Poisoned`]).
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// The device, e.g. to inspect a simulator's state.
    pub fn device(&mut self) -> &mut D {
        &mut self.store.data_mut().device
    }

    /// Drops the interpreter and returns the device.
    pub fn into_device(self) -> D {
        self.store.into_data().device
    }
}
