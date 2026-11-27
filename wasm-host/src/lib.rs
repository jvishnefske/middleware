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
//!    Loading is two steps, also available separately: [`Compiled::new`]
//!    validates and translates every function to wasmi bytecode
//!    (eagerly), then [`Payload::instantiate`] links and instantiates it
//!    and calls `init`. A caller may free the image between them.
//! 2. The firmware loop asks [`Payload::next_tick`] when the next `tick` is
//!    due (an absolute `now_ms`, wrapping) and calls [`Payload::tick`]; the
//!    payload chooses the period with `set_tick` (W2). `tick` reports with
//!    [`Ticked`] whether the export actually ran (W9).
//! 3. Messages for subscribed keys go through [`Payload::deliver`], which
//!    matches the key exactly, copies the data into a buffer from
//!    `mw_alloc`, and calls `on_message(slot, ptr, len)` (W2). `mw_alloc`
//!    returning 0 (or an out-of-memory range) means "not delivered".
//! 4. Requests on keys the payload serves (ABI 5, `serve`) go through
//!    [`Payload::request`]: one `mw_alloc` buffer holds the meta block,
//!    the body and the reply space ([`wasm_abi::serve::layout`]), then
//!    `on_request(slot, req_ptr, req_len, meta_ptr, reply_ptr, reply_cap)`
//!    returns the reply length or a `HandlerError` code, which
//!    [`wasm_abi::serve::reply_result`] checks before any reply byte is
//!    copied out.
//!
//! # Safety limits
//!
//! * Host imports validate every pointer, pin, level, pull, key (no `sys/`),
//!   period, the subscription limit and (ABI 2) every edge, timer, SPI and
//!   UART argument and (ABI 3) every PWM, ADC and I2C argument with the
//!   shared [`wasm_abi::periph`] parsers, and (ABI 4) every route argument
//!   and config with [`wasm_abi::route`], returning
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
//! * The ABI 4 route imports copy the key and config out of guest memory
//!   and hand the device parsed values ([`SourceConfig`], [`SinkConfig`]);
//!   frame data never passes through guest memory or this crate. Handles
//!   the device returns outside `0..MAX_ROUTES` reach the payload as
//!   `NoResources`, and `route_stat` counts saturate at `i32::MAX`.
//! * ABI 5: `serve` keys follow the publish rules and take one of
//!   [`wasm_abi::serve::MAX_SERVES`] slots reserved at load; `node_id`
//!   checks its 8-byte target before asking the device. A request writes
//!   only inside the `mw_alloc` buffer after checking it lies in linear
//!   memory, and reads back at most the reply capacity.
//! * `ledstrip_write` (ABI 7) checks the handle and frame length
//!   ([`wasm_abi::ledstrip::frame_len`]) before the pointer and copies the
//!   frame through a stack buffer of
//!   [`ledstrip::MAX_FRAME`](wasm_abi::ledstrip::MAX_FRAME) bytes, so the
//!   device never sees guest memory. Handles the device returns outside
//!   `0..MAX_LEDSTRIPS` reach the payload as `NoResources`.
//! * Every call into the payload (`init`, `tick`, `mw_alloc`, `on_message`,
//!   `on_irq`, `on_request`) is refuelled to [`Limits::fuel_per_call`] first, so an infinite loop
//!   ends with [`Trap::OutOfFuel`] instead of hanging the host (W4).
//!   After each call the fuel it used (the budget minus what is left; the
//!   whole budget if it ran dry) is added to the payload's [`FuelStats`]
//!   (call count, largest single call, saturating total), read with
//!   [`Payload::fuel_stats`].
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
//! * Heap (W10): every function is translated when the payload loads
//!   (wasmi's `CompilationMode::Eager`), not on its first call, so a call
//!   never allocates bytecode; the value stack is allocated in full on the
//!   first call and capped at [`Limits::max_stack_bytes`], the call depth
//!   at [`Limits::max_call_depth`] (a deeper call or a fuller stack traps
//!   with a stack overflow instead of allocating); the DMA copy buffer and
//!   the subscription list are reserved at load. What a payload can then
//!   allocate is bounded by [`Limits::heap_needed`] ([`heap`]; linear
//!   memory charged at the module's declared maximum when that is below
//!   the cap, [`abi::memory_reach`]), which a firmware checks against its
//!   free heap before starting one; [`Limits::load_heap_needed`] is the
//!   same bound for any image of a given length. Eager
//!   translation also means the first call into a function costs the same
//!   fuel as later ones.
//!
//! # Interpreter backends
//!
//! The interpreter under [`Payload`] is chosen at build time; everything
//! above it (the ABI, [`Device`], limits, channels) is the same code.
//!
//! * `wasmi` (default): validates each module and translates it to wasmi
//!   bytecode at load. Needs CAS atomics, so it does not build for
//!   `thumbv6m-none-eabi` (RP2040) or `riscv32imc-unknown-none-elf`
//!   (ESP32-C3).
//! * `micro` (`default-features = false, features = ["micro"]`): the
//!   `wasm-micro` crate. It keeps a copy of the image and interprets it in
//!   place: no validator, so a module wasmi would refuse at load traps (or
//!   computes garbage in its own memory) when the bad code runs; fuel is
//!   one unit per instruction, a different figure from wasmi's for the same
//!   work; float arithmetic needs `micro-float`; trap and load messages are
//!   worded differently. A base firmware comes out 450 to 800 KB smaller
//!   (RP2350: 688 KB with wasmi, 223 KB with `micro`), and a module's heap
//!   is its image plus a small index ([`heap`]) instead of two to four
//!   times its image.
//!
//! With both features enabled wasmi is used, so enabling `micro` somewhere
//! in a build never changes a build that already had wasmi. [`BACKEND`]
//! names the one in use. `scripts/check-wasm-backends.sh` builds both for
//! every platform architecture.
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
pub mod channels;
mod error;
pub mod fixed;
pub mod link;

// The interpreter behind `Payload`, chosen by feature: `wasmi` (the
// default), or `micro` when `wasmi` is off. See "Interpreter backends".
#[cfg(feature = "wasmi")]
#[path = "backend/wasmi.rs"]
mod backend;
#[cfg(all(feature = "micro", not(feature = "wasmi")))]
#[path = "backend/micro.rs"]
mod backend;
#[cfg(not(any(feature = "wasmi", feature = "micro")))]
compile_error!("wasm-host needs an interpreter: enable feature `wasmi` or `micro`");

#[cfg(feature = "wasmi")]
pub mod heap;
#[cfg(not(feature = "wasmi"))]
#[path = "heap_micro.rs"]
pub mod heap;

/// The interpreter this build runs payloads on: `"wasmi"` or `"micro"`.
pub const BACKEND: &str = backend::NAME;

use middleware::wasm_abi::{self, MAX_MESSAGE, route};

pub use abi::FuelStats;
pub use error::{INIT_LOG_LINES, InitLog, LoadError, Trap};
pub use middleware::wasm_abi::events::{DmaChannel, Event, EventKind};
pub use middleware::wasm_abi::ledstrip::{LedCount, LedStripSpec};
pub use middleware::wasm_abi::periph::{
    Edge, I2cConfig, I2cTransfer, PwmDuty, PwmSpec, SpiConfig, TimerSpec, UartConfig,
};
pub use middleware::wasm_abi::route::{
    AudioMode, CameraPins, I2cDevice, ImuConfig, MicConfig, Preamble, RouteStat, SampleSource,
    Segment, SinkConfig, Source, SourceConfig, SourceKind,
};
pub use middleware::wasm_abi::{AbiError, Pull};
pub use middleware::{HandlerError, RequestMeta, SenderId};

use link::{Exports, State};

/// The board a payload drives. The firmware implements it over real
/// peripherals and the serial link; simulators implement it in memory.
///
/// Arguments have already been validated against the ABI; implementations
/// only refuse what the board cannot do (reserved pins → `Unsupported`,
/// link failures → `Transport`, a peripheral not opened or still busy →
/// `Busy`).
///
/// The ABI 2 methods (from [`Device::now_us`] on), the ABI 3 methods
/// (from [`Device::pwm_open`] on), the ABI 4 route methods (from
/// [`Device::route_source`] on) and the ABI 5 methods ([`Device::serve`],
/// [`Device::node_id`]), the ABI 6 [`Device::route_bridge`] and the ABI 7
/// LED-strip methods (from [`Device::ledstrip_open`] on) have defaults
/// (`Unsupported`, or a derived clock) so a board without interrupts, DMA,
/// PWM, ADC, I2C, routes, request routing, bridges or LED strips keeps
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

    /// Starts a source route (ABI 4): every `cfg.period_ms` the device
    /// captures a frame from `source` into its own DMA-capable memory and
    /// publishes it on `key` as [`middleware::stream`] chunks of
    /// `cfg.chunk_data` bytes, one sequence number per frame. Returns the
    /// route handle (`0..MAX_ROUTES`; any other value reaches the payload as
    /// `NoResources`). Raises `RouteFrames` / `RouteDropped` events for the
    /// handle. `Unsupported` for a source the board lacks (or camera pins
    /// it cannot use), `NoResources` when every route is taken. For
    /// [`Source::Camera`] the payload has already configured the sensor;
    /// the device only receives its frames on the given pins.
    fn route_source(
        &mut self,
        source: Source,
        key: middleware::Key<'_>,
        cfg: SourceConfig,
    ) -> Result<u8, AbiError> {
        let _ = (source, key, cfg);
        Err(AbiError::Unsupported)
    }
    /// Starts a sampled source route (`route_source` with
    /// [`SourceKind::Imu`] or [`SourceKind::Microphone`]): every
    /// `source.period_us()` the device publishes one
    /// [`middleware::stream::samples`] batch of `source.samples()` units on
    /// `key`, `first_index` counting from 0 when the route starts. Returns
    /// the route handle, as [`Device::route_source`]; the route counts
    /// against `MAX_ROUTES` and stops with the payload. A board without the
    /// source answers `InvalidArgument` (the default), and lists the ones
    /// it has in `sys/info` ([`middleware::loader::Caps::source`]).
    fn route_samples(
        &mut self,
        source: SampleSource,
        key: middleware::Key<'_>,
    ) -> Result<u8, AbiError> {
        let _ = (source, key);
        Err(AbiError::InvalidArgument)
    }
    /// The I2C device a route from `source` streams from, if it sits on a
    /// bus the payload can use (on the RT1050-EVKB the FXOS8700CQ at 0x1F
    /// and the WM8960 at 0x1A on bus 0). While such a route runs the
    /// runtime answers the payload's `i2c_transfer` to that address
    /// `Busy` without calling [`Device::i2c_transfer`], and refuses a second
    /// route on the same device (`Busy`). `None` (the default) for a source
    /// that owns no payload-visible device.
    fn sample_device(&self, source: &SampleSource) -> Option<I2cDevice> {
        let _ = source;
        None
    }
    /// Starts a sink route (ABI 4): messages on `key` are consumed by the
    /// route (not passed to [`Payload::deliver`]) and run through a
    /// [`middleware::stream::Sink`] with `cfg.frame_len`; on
    /// `StartFrame` the device sends `cfg.preamble` on SPI `bus` (each
    /// segment with `cfg.dc_pin` at its level), then DMAs each accepted
    /// chunk's data with D/C high. The device owns the bus (its
    /// `spi_transfer` is `Busy`) and the D/C pin until the route stops.
    /// `Busy` if the bus is not open; `Unsupported` for a D/C pin the board
    /// reserves.
    fn route_sink(
        &mut self,
        key: middleware::Key<'_>,
        bus: u8,
        cfg: &SinkConfig,
    ) -> Result<u8, AbiError> {
        let _ = (key, bus, cfg);
        Err(AbiError::Unsupported)
    }
    /// Stops a route (ABI 4), releases its key, bus and pin, and purges its
    /// pending events; `Busy` if the handle is not running.
    fn route_stop(&mut self, handle: u8) -> Result<(), AbiError> {
        let _ = handle;
        Err(AbiError::Unsupported)
    }
    /// One counter of a running route (ABI 4); `Busy` if the handle is not
    /// running. Larger values reach the payload saturated to `i32::MAX`.
    fn route_stat(&mut self, handle: u8, stat: RouteStat) -> Result<u32, AbiError> {
        let _ = (handle, stat);
        Err(AbiError::Unsupported)
    }

    /// The payload serves requests on `key` (ABI 5; a non-`sys` key it has
    /// not served before). A device that routes requests to the payload
    /// (through [`Payload::request`]) records the key and returns `Ok`;
    /// the default, for a board that cannot, is `Unsupported`, which the
    /// payload's `serve` returns.
    fn serve(&mut self, key: middleware::Key<'_>) -> Result<(), AbiError> {
        let _ = key;
        Err(AbiError::Unsupported)
    }
    /// The device's node id (ABI 5), the [`SenderId`] its transports
    /// report and `sys/info` names; `Unsupported` by default.
    fn node_id(&mut self) -> Result<SenderId, AbiError> {
        Err(AbiError::Unsupported)
    }
    /// The node's manifest (ABI 10), encoded into `out`, which is at least
    /// [`middleware::loader::manifest::MANIFEST_MAX`] bytes: the bytes
    /// `sys/manifest` replies with, node id included. Returns their length
    /// (at most `out.len()`). `Unsupported` by default, for a device without
    /// a manifest.
    fn manifest(&mut self, out: &mut [u8]) -> Result<usize, AbiError> {
        let _ = out;
        Err(AbiError::Unsupported)
    }

    /// Installs a bridge rule (ABI 6, [`wasm_abi::bridge`]): publications
    /// arriving on link `bridge.src` whose key matches `bridge.filter` are
    /// re-emitted on link `bridge.dst`. The links are distinct and the key
    /// is already validated (not under `sys`). Returns the rule's handle
    /// (`BRIDGE_BASE..BRIDGE_BASE + MAX_BRIDGES`; any other value reaches
    /// the payload as `NoResources`), the same handle for an identical
    /// rule; `route_stop` and `route_stat` then take it. `Unsupported` for a
    /// link the device lacks, and by default (a single-link device).
    fn route_bridge(&mut self, bridge: &wasm_abi::bridge::Bridge<'_>) -> Result<u8, AbiError> {
        let _ = bridge;
        Err(AbiError::Unsupported)
    }

    /// Opens a WS2812-style LED strip of `spec.count` LEDs on `spec.pin`
    /// (ABI 7) and returns its handle (`0..MAX_LEDSTRIPS`; any other value
    /// reaches the payload as `NoResources`). The pin is checked as for
    /// [`Device::gpio_output`]; a pin another peripheral holds is
    /// `InvalidArgument`. Opening the pin of an open strip changes its
    /// length and returns the same handle. `NoResources` when the board
    /// drives no further strip, `Unsupported` for a chain longer than it
    /// can send. Sends nothing.
    fn ledstrip_open(&mut self, spec: LedStripSpec) -> Result<u8, AbiError> {
        let _ = spec;
        Err(AbiError::Unsupported)
    }
    /// Sends one frame (ABI 7): `grb` holds 3 bytes per LED (G, R, B), a
    /// non-zero multiple of 3 of at most
    /// [`ledstrip::MAX_FRAME`](wasm_abi::ledstrip::MAX_FRAME) bytes, already
    /// checked by the runtime. The device refuses a frame longer than the
    /// strip ([`LedStripSpec::check_frame`], `InvalidArgument`) and returns
    /// once it is on the wire. `Busy` if the handle is not open,
    /// `Transport` if transmission failed.
    fn ledstrip_write(&mut self, handle: u8, grb: &[u8]) -> Result<(), AbiError> {
        let _ = (handle, grb);
        Err(AbiError::Unsupported)
    }
    /// Closes a strip and returns its pin to idle GPIO (ABI 7); `Busy` if
    /// the handle is not open.
    fn ledstrip_close(&mut self, handle: u8) -> Result<(), AbiError> {
        let _ = handle;
        Err(AbiError::Unsupported)
    }
}

/// Default [`Limits::max_stack_bytes`]: 4 KiB of wasmi value stack.
///
/// Every example payload, the JPEG decoder of `cam_jpeg_view` included,
/// runs with 1 KiB (Rust payloads keep their own stack in linear memory;
/// wasmi's holds only the registers of active frames), so this leaves 4x
/// headroom. A call that needs more traps with a stack overflow.
pub const DEFAULT_STACK_BYTES: u32 = 4 * 1024;

/// Default [`Limits::max_call_depth`]: 32 nested WebAssembly calls.
///
/// Every example payload runs with 12. A deeper call traps with a stack
/// overflow.
pub const DEFAULT_CALL_DEPTH: u32 = 32;

/// Resource limits for one payload.
///
/// Build with [`Limits::pages`] or [`Limits::bytes`] and adjust the fuel
/// with [`Limits::with_fuel`] and the interpreter stacks with
/// [`Limits::with_stack`]; the fields are public for direct use.
/// [`Limits::load_heap_needed`] bounds the heap a payload loaded under
/// these limits can ever use.
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
    /// Size of wasmi's value stack (locals and temporaries of every active
    /// frame), in bytes. It is allocated in full on the first call into the
    /// payload and never grows: a call that needs more traps with a stack
    /// overflow instead of allocating. Default [`DEFAULT_STACK_BYTES`].
    pub max_stack_bytes: u32,
    /// Deepest WebAssembly call nesting; one deeper call traps with a stack
    /// overflow. wasmi's frame list grows on demand up to this many
    /// entries. Default [`DEFAULT_CALL_DEPTH`].
    pub max_call_depth: u32,
    /// The most keys the payload may subscribe to, its bound input channels
    /// included. Default [`wasm_abi::MAX_SUBSCRIPTIONS`]; a host with room
    /// gives more ([`Limits::with_subscriptions`]), up to
    /// [`wasm_abi::SLOT_MAX`].
    pub max_subscriptions: u8,
}

impl Limits {
    /// A cap of `n` 64 KiB pages (saturating at `u32::MAX` bytes) and the
    /// default fuel and stacks.
    pub const fn pages(n: u32) -> Self {
        let bytes = (n as u64).saturating_mul(abi::PAGE_BYTES as u64);
        Limits::bytes(if bytes > u32::MAX as u64 {
            u32::MAX
        } else {
            bytes as u32
        })
    }

    /// A cap of `n` bytes and the default fuel and stacks.
    pub const fn bytes(n: u32) -> Self {
        Limits {
            max_memory_bytes: n,
            fuel_per_call: wasm_abi::FUEL_PER_CALL,
            max_stack_bytes: DEFAULT_STACK_BYTES,
            max_call_depth: DEFAULT_CALL_DEPTH,
            max_subscriptions: wasm_abi::MAX_SUBSCRIPTIONS as u8,
        }
    }

    /// The same limits with up to `n` subscriptions (at most
    /// [`wasm_abi::SLOT_MAX`]).
    pub const fn with_subscriptions(self, n: u8) -> Self {
        let n = if (n as usize) > wasm_abi::SLOT_MAX {
            wasm_abi::SLOT_MAX as u8
        } else {
            n
        };
        Limits {
            max_subscriptions: n,
            ..self
        }
    }

    /// The same limits with `fuel` per call.
    pub const fn with_fuel(self, fuel: u64) -> Self {
        Limits {
            fuel_per_call: fuel,
            ..self
        }
    }

    /// The same limits with a value stack of `bytes` and at most `depth`
    /// nested calls.
    pub const fn with_stack(self, bytes: u32, depth: u32) -> Self {
        Limits {
            max_stack_bytes: bytes,
            max_call_depth: depth,
            ..self
        }
    }

    /// Upper bound, in bytes, on the heap a payload image of `image_len`
    /// bytes can use at any moment under these limits, from compiling it
    /// ([`Compiled::new`], with the caller's image buffer) through
    /// instantiating it and every later call ([`Payload::instantiate`],
    /// with the image freed), saturating at `u32::MAX` (requirement W10).
    ///
    /// Translation is eager, so all bytecode is allocated while compiling;
    /// later calls allocate only the value stack (once,
    /// [`Limits::max_stack_bytes`]), call frames (up to
    /// [`Limits::max_call_depth`]), linear memory growth (up to
    /// [`Limits::max_memory_bytes`]), subscription keys and `init`'s log,
    /// all counted here. See [`heap`] for the model and its measurements.
    /// A caller that keeps the image during [`Payload::load`] needs
    /// `image_len` more.
    ///
    /// A host that starts a payload only when this much heap is free (plus
    /// its allocator's own overhead) cannot run out of heap in the payload
    /// later, as long as nothing else allocates meanwhile.
    ///
    /// This is the worst case over every image of that length: it charges
    /// linear memory at the whole cap. With the image at hand,
    /// [`Limits::heap_needed`] charges only what the module can reach.
    pub const fn load_heap_needed(&self, image_len: u32) -> u32 {
        heap::needed(
            image_len,
            self.max_memory_bytes,
            self.max_stack_bytes,
            self.max_call_depth,
        )
    }

    /// [`Limits::load_heap_needed`] for this image: the same bound, with
    /// linear memory charged at [`abi::memory_reach`], the smaller of the
    /// cap and the module's declared maximum (requirement W10). A module
    /// built with 1-byte pages and initial = maximum (see
    /// `payloads/scripts/build-byte-pages.sh`) is charged exactly its
    /// memory; one without a maximum, or a malformed one, the cap.
    ///
    /// The declared maximum is a hard limit: wasmi's `memory.grow` never
    /// passes it, whatever the cap.
    pub fn heap_needed(&self, wasm: &[u8]) -> u32 {
        heap::needed_for(
            wasm,
            abi::memory_reach(wasm, self.max_memory_bytes),
            self.max_stack_bytes,
            self.max_call_depth,
        )
    }
}

impl Default for Limits {
    /// Two 64 KiB pages (128 KiB), [`wasm_abi::FUEL_PER_CALL`],
    /// [`DEFAULT_STACK_BYTES`] and [`DEFAULT_CALL_DEPTH`].
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

/// Carries out [`channels::plan`] on a fresh payload's state: starts the
/// local routes, subscribes the bound input keys, records the outputs.
fn resolve<D: Device>(
    st: &mut State<D>,
    decls: fixed::PerChannel<channels::Decl>,
    binding: Option<&middleware::loader::binding::Binding<'_>>,
) -> Result<(), LoadError> {
    use channels::{BindError, Resolved, Step};
    if decls.is_empty() {
        return Ok(());
    }
    let free = st.max_subscriptions.saturating_sub(st.subscriptions.len());
    let steps =
        channels::plan(&decls, binding, st.exports.messages, free).map_err(LoadError::Bind)?;
    let mut resolved = fixed::PerChannel::new();
    let full = |d: &channels::Decl| LoadError::Bind(BindError::NoResources(d.name.clone()));
    for (d, step) in decls.iter().zip(steps) {
        let step = match step {
            Step::Unbound => Resolved::Unbound,
            Step::Output { key } => Resolved::Output { key },
            Step::Input {
                key,
                route,
                from,
                adapter,
            } => {
                if let Some(source) = route {
                    let k = middleware::Key::new(&key)
                        .map_err(|_| LoadError::Bind(BindError::Mismatch(d.name.clone())))?;
                    link::start_route(st, source, k)
                        .map_err(|e| LoadError::Bind(BindError::Route(d.name.clone(), e)))?;
                }
                let slot = st.subscriptions.len();
                st.subscriptions.push(key).map_err(|_| full(d))?;
                Resolved::Input {
                    slot,
                    from,
                    adapter: adapter.map(middleware::stream::adapt::AdapterState::new),
                }
            }
        };
        resolved.push(step).map_err(|_| full(d))?;
    }
    st.channels = channels::ChannelTable { decls, resolved };
    Ok(())
}

/// A loaded, running payload bound to a device.
///
/// `D: 'static` is required because wasmi stores host closures and the
/// store's resource limiter as `'static` values parameterised by `D`.
pub struct Payload<D: Device + 'static> {
    inner: backend::Instance<D>,
    fuel: u64,
    fuel_stats: FuelStats,
    poisoned: bool,
}

/// Checks imports and exports of a compiled module; returns the optional
/// exports it provides.
fn validate(wasm: &[u8], module: &backend::Module, limits: &Limits) -> Result<Exports, LoadError> {
    module.imports(&mut |module, name, kind| abi::check_import(module, name, kind))?;
    let mut exports = Exports::default();
    let (mut alloc, mut on_message, mut on_request, mut memory) = (false, false, false, false);
    module.exports(&mut |name, export| {
        match export {
            backend::Export::Func(shape) => {
                abi::check_export(name, shape)?;
                match name {
                    wasm_abi::exports::INIT => exports.init = true,
                    wasm_abi::exports::TICK => exports.tick = true,
                    wasm_abi::exports::ALLOC => alloc = true,
                    wasm_abi::exports::ON_MESSAGE => on_message = true,
                    wasm_abi::exports::ON_IRQ => exports.irq = true,
                    wasm_abi::exports::ON_REQUEST => on_request = true,
                    _ => {}
                }
            }
            backend::Export::Memory if name == wasm_abi::exports::MEMORY => memory = true,
            _ => {}
        }
        Ok(())
    })?;
    if !memory {
        return Err(LoadError::MissingMemory);
    }
    let initial = abi::initial_memory_bytes(wasm).unwrap_or(u64::MAX);
    abi::check_memory(initial, limits.max_memory_bytes)?;
    exports.messages = alloc && on_message;
    exports.requests = alloc && on_request;
    Ok(exports)
}

/// A validated module, translated to wasmi bytecode and ready to
/// instantiate, independent of the image bytes it came from.
///
/// Loading in two steps lets the caller free the image between them, so
/// the image and linear memory are never allocated at the same time (see
/// [`heap`]).
pub struct Compiled {
    module: backend::Module,
    exports: Exports,
    limits: Limits,
    channels: fixed::PerChannel<channels::Decl>,
}

impl Compiled {
    /// Validates `wasm` against the ABI (W1, W5) and translates every
    /// function eagerly, so no translation (and no allocation for it)
    /// happens on a later call.
    pub fn new(wasm: &[u8], limits: Limits) -> Result<Self, LoadError> {
        let module = backend::Module::new(wasm, &limits)?;
        let exports = validate(wasm, &module, &limits)?;
        let declared =
            wasm_abi::channels::Channels::from_module(wasm).map_err(LoadError::Channels)?;
        let mut channels = fixed::PerChannel::new();
        for c in declared.iter() {
            // The section holds at most `MAX_CHANNELS`.
            channels
                .push(channels::Decl::from_channel(c))
                .map_err(|_| LoadError::OutOfMemory)?;
        }
        Ok(Compiled {
            module,
            exports,
            limits,
            channels,
        })
    }

    /// The limits the module was compiled under.
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// The channels the module declares (its `mw.channels` section, ABI 8),
    /// in declaration order.
    pub fn channels(&self) -> &[channels::Decl] {
        &self.channels
    }
}

impl<D: Device + 'static> Payload<D> {
    /// Validates and instantiates `wasm`, then calls `init` if exported:
    /// [`Compiled::new`] followed by [`Payload::instantiate`].
    pub fn load(wasm: &[u8], device: D, limits: Limits) -> Result<Self, LoadError> {
        Self::instantiate(Compiled::new(wasm, limits)?, device)
    }

    /// Instantiates a compiled module on `device`, then calls `init` if
    /// exported.
    ///
    /// While `init` runs, its log lines are forwarded to [`Device::log`] and
    /// also kept in a bounded [`InitLog`] (the last [`INIT_LOG_LINES`]). If
    /// `init` traps, the device is dropped and the error is
    /// [`LoadError::Init`] carrying the trap and that log, so the caller can
    /// show why `init` failed. On success the log is discarded.
    pub fn instantiate(compiled: Compiled, device: D) -> Result<Self, LoadError> {
        Self::instantiate_bound(compiled, device, None)
    }

    /// [`Payload::instantiate`] with the module's channels resolved
    /// against `binding` before `init` (ABI 8, [`channels`]): local sources
    /// are started on their internal keys through the device, bound input
    /// keys become subscriptions, outputs remember their keys. A required
    /// channel left unbound, or a binding that does not fit the
    /// declarations, fails with [`LoadError::Bind`] naming the channel
    /// (with no binding, every channel is unbound). A module without
    /// channels ignores the binding.
    pub fn instantiate_bound(
        compiled: Compiled,
        device: D,
        binding: Option<&middleware::loader::binding::Binding<'_>>,
    ) -> Result<Self, LoadError> {
        let Compiled {
            module,
            exports,
            limits,
            channels: decls,
        } = compiled;
        let state = State::new(
            device,
            exports,
            usize::from(limits.max_subscriptions),
            limits.max_memory_bytes,
        );
        let mut inner = backend::Instance::new(module, state, &limits)?;
        resolve(inner.state_mut(), decls, binding)?;
        let mut payload = Payload {
            inner,
            fuel: limits.fuel_per_call,
            fuel_stats: FuelStats::default(),
            poisoned: false,
        };
        if exports.init {
            payload.inner.state_mut().init_log = Some(InitLog::new());
            let result = payload.call(wasm_abi::exports::INIT, &[], false);
            let log = payload.inner.state_mut().init_log.take();
            if let Err(trap) = result {
                // The failed payload goes first, so its heap is free again
                // before the log is boxed for the error.
                drop(payload);
                let log = alloc::boxed::Box::new(log.unwrap_or_default());
                return Err(LoadError::Init { trap, log });
            }
        }
        Ok(payload)
    }

    /// Refuels and calls the export `name` (`i32` arguments, one `i32`
    /// result when `returns`, else 0), records the fuel it used in
    /// [`Payload::fuel_stats`], and poisons the payload on a trap.
    fn call(&mut self, name: &str, args: &[i32], returns: bool) -> Result<i32, Trap> {
        if self.poisoned {
            return Err(Trap::Poisoned);
        }
        let result = self.inner.call(name, args, returns, self.fuel);
        // A call that ran dry counts as its whole budget: an interpreter
        // may trap when the next instruction costs more than is left, so
        // a few units may remain.
        let remaining = match result {
            Err(Trap::OutOfFuel) => None,
            _ => self.inner.fuel_left(),
        };
        let used = abi::fuel_used(self.fuel, remaining);
        self.fuel_stats = self.fuel_stats.record(used);
        self.poisoned = result.is_err();
        result
    }

    /// Fuel used so far by every export call into this payload, `init`
    /// included (see [`FuelStats`]).
    pub fn fuel_stats(&self) -> FuelStats {
        self.fuel_stats
    }

    /// Keys the payload subscribed to, in slot order.
    pub fn subscriptions(&self) -> impl Iterator<Item = &str> {
        self.inner.state().subscriptions.iter().map(|s| s.as_str())
    }

    /// Absolute `now_ms` at which the next `tick` is due; `None` when ticking
    /// is disabled or the payload is poisoned.
    pub fn next_tick(&self) -> Option<u32> {
        if self.poisoned {
            return None;
        }
        self.inner.state().schedule.map(|s| s.due)
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
        let state = self.inner.state_mut();
        match abi::tick_decision(state.exports.tick, state.schedule, now_ms) {
            (Ticked::Ran, next) => state.schedule = next,
            (other, _) => return Ok(other),
        }
        self.call(wasm_abi::exports::TICK, &[now_ms as i32], false)
            .map(|_| Ticked::Ran)
    }

    /// Delivers a message. Returns `Ok(true)` if `key` exactly matches a
    /// subscription and `on_message` ran. Returns `Ok(false)` (not
    /// delivered, payload still healthy) if nothing is subscribed to `key`,
    /// `data` exceeds [`wasm_abi::MAX_MESSAGE`], or `mw_alloc` refused the
    /// message by returning 0 or a buffer outside linear memory.
    pub fn deliver(&mut self, key: middleware::Key<'_>, data: &[u8]) -> Result<bool, Trap> {
        self.deliver_from(key, data, None)
    }

    /// [`Payload::deliver`] of a message from `sender` (as the link
    /// reported it; `None` when unknown). A channel bound to a remote node
    /// (ABI 8, `Target::Remote`) takes messages from that node only: from
    /// anyone else, or an unknown sender, this is `Ok(false)` without
    /// calling the payload. The sender is what the link reports (over
    /// Zenoh, the publisher's own attachment): this routes a channel to a
    /// node, it does not authenticate one.
    pub fn deliver_from(
        &mut self,
        key: middleware::Key<'_>,
        data: &[u8],
        sender: Option<SenderId>,
    ) -> Result<bool, Trap> {
        if self.poisoned {
            return Err(Trap::Poisoned);
        }
        let Some(slot) = abi::slot_for(&self.inner.state().subscriptions, key.as_str()) else {
            return Ok(false);
        };
        if !self.inner.state().channels.admits(slot, sender) {
            return Ok(false);
        }
        // An adapted channel (ABI 8) gets the adapter's output, or nothing
        // when this message completed none.
        let mut adapted = [0u8; MAX_MESSAGE];
        let data = match self.inner.state_mut().channels.adapter_mut(slot) {
            Some(a) => match a.feed(data, &mut adapted) {
                Some(n) => &adapted[..n],
                None => return Ok(false),
            },
            None => data,
        };
        if data.len() > MAX_MESSAGE {
            return Ok(false);
        }
        // Both fit in i32: slot < SLOT_MAX, len <= MAX_MESSAGE.
        let (slot, len) = (slot as i32, data.len() as i32);
        let ptr = self.call(wasm_abi::exports::ALLOC, &[len], true)?;
        let mem = self.inner.memory_mut();
        let written = abi::alloc_range(ptr, len, mem.len())
            .and_then(|range| mem.get_mut(range.start..)?.get_mut(..data.len()))
            .map(|dst| dst.copy_from_slice(data));
        if written.is_none() {
            // The guest refused the message (0) or returned an unusable buffer.
            return Ok(false);
        }
        self.call(wasm_abi::exports::ON_MESSAGE, &[slot, ptr, len], false)?;
        Ok(true)
    }

    /// The channel table: declarations and what each resolved to (ABI 8).
    pub fn channels(&self) -> &channels::ChannelTable {
        &self.inner.state().channels
    }

    /// Keys the payload serves (ABI 5), in slot order.
    pub fn serves(&self) -> impl Iterator<Item = &str> {
        self.inner.state().serves.iter().map(|s| s.as_str())
    }

    /// Whether the module exports `mw_alloc` and `on_request` (ABI 5).
    pub fn handles_requests(&self) -> bool {
        self.inner.state().exports.requests
    }

    /// Hands one request to the payload (ABI 5) and copies its reply into
    /// `reply`; `Ok(Ok(len))` is the reply length, `Ok(Err(e))` what the
    /// device answers the caller instead:
    ///
    /// * `NotFound` when nothing serves `key` (exact match), without
    ///   calling the payload;
    /// * `InvalidRequest` for a body over
    ///   [`wasm_abi::serve::MAX_REQUEST`], without calling the payload;
    /// * `Unavailable` when `mw_alloc` declined the buffer (0, or a range
    ///   outside linear memory);
    /// * the payload's own `HandlerError`, or `Internal` for a result
    ///   that is neither a length within the reply capacity nor a known
    ///   code ([`wasm_abi::serve::reply_result`]).
    ///
    /// The reply capacity offered is `reply.len()`, at most
    /// [`wasm_abi::serve::MAX_REPLY`]. The meta block carries `meta`'s
    /// sender and deadline ([`wasm_abi::serve::encode_meta`]). `mw_alloc`
    /// and `on_request` each run on a fresh fuel budget; a trap (fuel
    /// included) poisons the payload and is the `Err`, which a device
    /// answers as `Internal`. Nothing is allocated.
    pub fn request(
        &mut self,
        key: middleware::Key<'_>,
        body: &[u8],
        meta: &RequestMeta<'_>,
        reply: &mut [u8],
    ) -> Result<Result<usize, HandlerError>, Trap> {
        use wasm_abi::serve;
        if self.poisoned {
            return Err(Trap::Poisoned);
        }
        let Some(slot) = abi::slot_for(&self.inner.state().serves, key.as_str()) else {
            return Ok(Err(HandlerError::NotFound));
        };
        let layout = match serve::layout(body.len(), reply.len()) {
            Ok(l) => l,
            Err(e) => return Ok(Err(e)),
        };
        // Fits i32: at most MAX_REQUEST_BUFFER.
        let alloc_len = layout.alloc_len as i32;
        let ptr = self.call(wasm_abi::exports::ALLOC, &[alloc_len], true)?;
        let mem = self.inner.memory_mut();
        let placed =
            abi::request_ranges(ptr, &layout, mem.len()).and_then(|r| Some((r.args()?, r)));
        let Some(((req_ptr, req_len, meta_ptr, reply_ptr, reply_cap), ranges)) = placed else {
            return Ok(Err(HandlerError::Unavailable));
        };
        let encoded = serve::encode_meta(meta);
        let put = |mem: &mut [u8], at: usize, src: &[u8]| {
            let dst = mem.get_mut(at..)?.get_mut(..src.len())?;
            dst.copy_from_slice(src);
            Some(())
        };
        if put(mem, ranges.meta.start, &encoded)
            .and_then(|()| put(mem, ranges.request.start, body))
            .is_none()
        {
            return Ok(Err(HandlerError::Unavailable));
        }
        // slot < MAX_SERVES.
        let args = [
            slot as i32,
            req_ptr,
            req_len,
            meta_ptr,
            reply_ptr,
            reply_cap,
        ];
        let code = self.call(wasm_abi::exports::ON_REQUEST, &args, true)?;
        let n = match serve::reply_result(code, layout.reply_cap) {
            Ok(n) => n,
            Err(e) => return Ok(Err(e)),
        };
        // Linear memory never shrinks, so the checked range is still valid.
        let src = self
            .inner
            .memory()
            .get(ranges.reply.start..)
            .and_then(|m| m.get(..n));
        match (reply.get_mut(..n), src) {
            (Some(out), Some(src)) => {
                out.copy_from_slice(src);
                Ok(Ok(n))
            }
            _ => Ok(Err(HandlerError::Internal)),
        }
    }

    /// Whether the module exports `on_irq` (ABI 2).
    pub fn handles_irq(&self) -> bool {
        self.inner.state().exports.irq
    }

    /// Delivers one interrupt event by calling
    /// `on_irq(kind, source, value, time_us)` with fresh fuel. Returns
    /// `Ok(false)` (event dropped, payload healthy) when `on_irq` is not
    /// exported, and [`Trap::Poisoned`] after an earlier trap.
    pub fn dispatch(&mut self, event: Event) -> Result<bool, Trap> {
        if self.poisoned {
            return Err(Trap::Poisoned);
        }
        // A route the device stopped by itself gives its I2C device back,
        // whether or not the payload hears of it.
        if event.kind == EventKind::RouteStopped {
            let st = self.inner.state_mut();
            if let Ok(handle) = route::route_handle(event.source) {
                st.i2c_owners = st.i2c_owners.release(handle);
            }
        }
        if !self.handles_irq() {
            return Ok(false);
        }
        let [kind, source, value, time] = event.to_args();
        self.call(
            wasm_abi::exports::ON_IRQ,
            &[kind, source, value, time],
            false,
        )?;
        Ok(true)
    }

    /// Whether an earlier call trapped (see [`Trap::Poisoned`]).
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// The payload's linear memory, e.g. for differential tests against
    /// another runtime (`wasm-hdl-ref`).
    pub fn memory(&self) -> &[u8] {
        self.inner.memory()
    }

    /// The device, e.g. to inspect a simulator's state.
    pub fn device(&mut self) -> &mut D {
        &mut self.inner.state_mut().device
    }

    /// Drops the interpreter and returns the device.
    pub fn into_device(self) -> D {
        self.inner.into_state().device
    }
}
