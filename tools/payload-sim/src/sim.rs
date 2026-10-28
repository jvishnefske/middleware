//! Functional core of the simulator: command-line parsing, stdin line
//! parsing, and [`SimDevice`], an in-memory board that records every host
//! call as a printable event line instead of touching hardware.
//!
//! [`SimDevice`] also provides the ABI 2 peripherals (requirement D10) as
//! virtual hardware, modelled in [`periph`]: timers driven by the
//! simulator clock ([`SimDevice::advance_to`]), GPIO edge interrupts raised
//! by `!gpio` commands, an SPI loopback device and a UART terminal fed by
//! `!uart` commands. Like the firmware's ISRs, these only push [`Event`]s
//! into a bounded [`EventQueue`]; the shell pops them with
//! [`SimDevice::pop_event`] and hands them to `Payload::dispatch`. DMA
//! completions are deferred: a transfer started during one loop iteration
//! raises its `DmaDone` when the shell calls [`SimDevice::complete_dma`] at
//! the start of the next.
//!
//! The ABI 3 peripherals (requirement X7) are synchronous and raise no
//! events: PWM records each channel's duty (printed as an event line), ADC
//! pins read values set by
//! `!adc` commands, and each I2C bus carries a register device at
//! [`periph::I2C_DEVICE_ADDR`].
//!
//! The ABI 4 DMA routes (requirement R10) run in [`route`]: a test-pattern
//! source route publishes each due frame as chunks (one printed summary
//! line per frame), and a sink route consumes the messages on its key,
//! steps a [`middleware::stream::Sink`], and sends the preamble and the
//! forwarded chunk data to a virtual GC9A01-like [`panel::Panel`] on its
//! SPI bus. Route progress raises `RouteFrames` / `RouteDropped` events
//! through the same queue as the other peripherals. Every publication, the
//! payload's own and a source route's chunks, is also queued in an outbox
//! ([`SimDevice::take_outbox`]) that the shell hands to a `--peer` payload
//! over a [`bus::Bus`].

pub mod bus;
pub mod panel;
pub mod periph;
pub mod route;

use std::fmt::Write as _;

use middleware::Key;
use middleware::stream::{Action, Sink};
use middleware::wasm_abi::events::{EventQueue, MAX_EVENTS};
use wasm_host::{
    AbiError, Device, DmaChannel, Edge, Event, EventKind, I2cConfig, Limits, Pull, PwmSpec,
    RouteStat, SinkConfig, SourceConfig, SourceKind, SpiConfig, TimerSpec, UartConfig,
};

use panel::Panel;
use periph::{Adc, I2c, Pwm, Spi, Timers, Uart};
use route::{Route, Routes, SinkRoute, SourceRoute};

/// Parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// Path of the `.wasm` payload.
    pub file: String,
    /// Stop after this many ticks; `None` runs until stdin closes.
    pub ticks: Option<u32>,
    /// Runtime limits (`--fuel`, `--pages`, `--memory-bytes`).
    pub limits: Limits,
    /// Use virtual time: stdin is processed first, then the clock jumps
    /// straight to each due tick or timer.
    pub virtual_time: bool,
    /// Stop once the clock would pass this many milliseconds.
    pub until_ms: Option<u64>,
    /// Bytes the SPI device returns instead of looping back (`--spi-pattern`).
    pub spi_pattern: Option<Vec<u8>>,
    /// A second payload sharing an in-memory bus with the first (`--peer`).
    pub peer: Option<String>,
    /// Per-mille message loss on that bus (`--loss`, `0..=1000`).
    pub loss: u16,
    /// Seed of the loss generator (`--seed`).
    pub seed: u64,
    /// Where to write the panel framebuffer as a PPM image at the end
    /// (`--dump-panel`).
    pub dump_panel: Option<String>,
}

/// Usage text printed on argument errors.
pub const USAGE: &str = "usage: payload-sim <file.wasm> [--ticks N] [--fuel F] [--pages P | --memory-bytes B] [--virtual]\n\
                         [--until-ms T] [--spi-pattern HEX] [--peer <other.wasm>] [--loss PERMILLE]\n\
                         [--seed S] [--dump-panel <file.ppm>]\n\
stdin lines `<key> <text>` are delivered to the payload;\n\
`!gpio <pin> <0|1>` sets an input level, `!uart <text>` types at the UART1 terminal,\n\
`!adc <pin> <0..4095>` sets an analog input";

/// Parses the arguments after the program name.
pub fn parse_args<S: AsRef<str>>(args: &[S]) -> Result<Options, String> {
    let mut file = None;
    let mut opts = Options {
        file: String::new(),
        ticks: None,
        limits: Limits::default(),
        virtual_time: false,
        until_ms: None,
        spi_pattern: None,
        peer: None,
        loss: 0,
        seed: 1,
        dump_panel: None,
    };
    let mut it = args.iter().map(AsRef::as_ref);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match arg {
            "--ticks" => opts.ticks = Some(number(value("--ticks")?)?),
            "--fuel" => opts.limits.fuel_per_call = number(value("--fuel")?)?,
            "--pages" => {
                opts.limits.max_memory_bytes =
                    Limits::pages(number(value("--pages")?)?).max_memory_bytes
            }
            "--memory-bytes" => opts.limits.max_memory_bytes = number(value("--memory-bytes")?)?,
            "--virtual" => opts.virtual_time = true,
            "--until-ms" => opts.until_ms = Some(number(value("--until-ms")?)?),
            "--spi-pattern" => opts.spi_pattern = Some(hex(value("--spi-pattern")?)?),
            "--peer" => opts.peer = Some(value("--peer")?.to_string()),
            "--loss" => {
                opts.loss = number(value("--loss")?)?;
                if opts.loss > bus::MAX_LOSS {
                    return Err(format!("--loss is per mille, at most {}", bus::MAX_LOSS));
                }
            }
            "--seed" => opts.seed = number(value("--seed")?)?,
            "--dump-panel" => opts.dump_panel = Some(value("--dump-panel")?.to_string()),
            flag if flag.starts_with("--") => return Err(format!("unknown option {flag}")),
            path if file.is_none() => file = Some(path.to_string()),
            extra => return Err(format!("unexpected argument {extra}")),
        }
    }
    opts.file = file.ok_or("missing <file.wasm>")?;
    Ok(opts)
}

fn number<T: std::str::FromStr>(s: &str) -> Result<T, String> {
    s.parse().map_err(|_| format!("not a number: {s}"))
}

/// Parses a non-empty hex byte string (optional `0x` prefix).
pub fn hex(s: &str) -> Result<Vec<u8>, String> {
    let digits = s.strip_prefix("0x").unwrap_or(s);
    if digits.is_empty() || !digits.len().is_multiple_of(2) {
        return Err(format!("not a hex byte string: {s}"));
    }
    (0..digits.len())
        .step_by(2)
        .map(|i| {
            digits
                .get(i..i + 2)
                .and_then(|d| u8::from_str_radix(d, 16).ok())
                .ok_or_else(|| format!("not a hex byte string: {s}"))
        })
        .collect()
}

/// A simulator command typed on stdin (lines starting with `!`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `!gpio <pin> <0|1>`: drive an input pin from outside.
    Gpio {
        /// Pin number.
        pin: u8,
        /// New level.
        high: bool,
    },
    /// `!uart <text>`: bytes arriving on UART1.
    Uart(Vec<u8>),
    /// `!adc <pin> <value>`: the value an analog pin reads.
    Adc {
        /// Pin number.
        pin: u8,
        /// Raw reading, `0..=ADC_MAX`.
        value: u16,
    },
}

/// Parses a command line; `Ok(None)` when the line is not a command.
pub fn parse_command(line: &str) -> Result<Option<Command>, String> {
    let line = line.trim_end_matches(['\r', '\n']);
    let Some(rest) = line.strip_prefix('!') else {
        return Ok(None);
    };
    let (name, args) = rest.split_once(' ').unwrap_or((rest, ""));
    match name {
        "gpio" => {
            let mut it = args.split_whitespace();
            let pin = it.next().ok_or("!gpio needs <pin> <0|1>")?;
            let level = it.next().ok_or("!gpio needs <pin> <0|1>")?;
            if it.next().is_some() {
                return Err("!gpio takes <pin> <0|1>".to_string());
            }
            let high = match level {
                "0" => false,
                "1" => true,
                _ => return Err(format!("bad level {level:?}, expected 0 or 1")),
            };
            Ok(Some(Command::Gpio {
                pin: number(pin)?,
                high,
            }))
        }
        "adc" => {
            let usage = "!adc needs <pin> <0..4095>";
            let mut it = args.split_whitespace();
            let (pin, value) = (it.next().ok_or(usage)?, it.next().ok_or(usage)?);
            if it.next().is_some() {
                return Err(usage.to_string());
            }
            Ok(Some(Command::Adc {
                pin: number(pin)?,
                value: number(value)?,
            }))
        }
        "uart" if !args.is_empty() => Ok(Some(Command::Uart(args.as_bytes().to_vec()))),
        "uart" => Err("!uart needs <text>".to_string()),
        other => Err(format!("unknown command !{other}")),
    }
}

/// Splits a stdin line into a key and its text payload. Blank lines yield
/// `Ok(None)`; the payload is everything after the first space (may be empty).
pub fn parse_line(line: &str) -> Result<Option<(Key<'_>, &[u8])>, String> {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.trim().is_empty() {
        return Ok(None);
    }
    let (key, text) = line.split_once(' ').unwrap_or((line, ""));
    let key = Key::new(key).map_err(|e| format!("bad key {key:?}: {e:?}"))?;
    Ok(Some((key, text.as_bytes())))
}

/// Renders message bytes: as text when printable UTF-8, otherwise hex.
pub fn render(data: &[u8]) -> String {
    match std::str::from_utf8(data) {
        Ok(s) if !s.chars().any(char::is_control) => s.to_string(),
        _ => data.iter().fold(String::from("0x"), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        }),
    }
}

/// How a simulated pin is configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Unused,
    Output(bool),
    Input(Pull),
}

/// Number of simulated GPIO pins; higher pins are `Unsupported`.
///
/// 144 covers every board the payloads target: the ESP32 parts (up to 48
/// GPIOs) and STM32F4 boards numbered `port * 16 + n` for ports A–I
/// (PE0 = 64, PI15 = 143).
pub const PINS: usize = 144;

/// In-memory board. The shell moves the clock with
/// [`SimDevice::advance_to`] before each call and drains
/// [`SimDevice::events`] after it.
pub struct SimDevice {
    /// Event lines produced since the last drain.
    pub events: Vec<String>,
    /// Name printed after the timestamp (`--peer` runs label both boards).
    label: Option<String>,
    /// Publications not yet handed to the shell.
    outbox: Vec<(String, Vec<u8>)>,
    routes: Routes,
    panel: Panel,
    now_us: u64,
    pins: [Mode; PINS],
    /// Level driven from outside (`!gpio`); overrides the pull when set.
    external: [Option<bool>; PINS],
    edges: [Edge; PINS],
    queue: EventQueue<MAX_EVENTS>,
    timers: Timers,
    spi: Spi,
    uart: Uart,
    pwm: Pwm,
    adc: Adc,
    i2c: I2c,
}

impl Default for SimDevice {
    fn default() -> Self {
        SimDevice::new(None)
    }
}

impl SimDevice {
    /// A board whose SPI device returns `spi_pattern` (repeated) instead of
    /// looping back.
    pub fn new(spi_pattern: Option<Vec<u8>>) -> Self {
        SimDevice {
            events: Vec::new(),
            label: None,
            outbox: Vec::new(),
            routes: Routes::default(),
            panel: Panel::new(),
            now_us: 0,
            pins: [Mode::Unused; PINS],
            external: [None; PINS],
            edges: [Edge::Off; PINS],
            queue: EventQueue::new(),
            timers: Timers::default(),
            spi: Spi::new(spi_pattern),
            uart: Uart::default(),
            pwm: Pwm::default(),
            adc: Adc::default(),
            i2c: I2c::default(),
        }
    }

    /// The same board with event lines labelled `label`.
    pub fn with_label(self, label: &str) -> Self {
        SimDevice {
            label: Some(label.to_string()),
            ..self
        }
    }

    /// Moves the clock to `now_us` (never backwards), queues every timer
    /// event due by then, and publishes the source-route frames due by
    /// then.
    pub fn advance_to(&mut self, now_us: u64) {
        self.now_us = self.now_us.max(now_us);
        for event in self.timers.fire(self.now_us) {
            self.queue.push(event);
        }
        for frame in self.routes.due_frames(self.now_us) {
            let skipped = if frame.skipped > 0 {
                format!(", {} skipped", frame.skipped)
            } else {
                String::new()
            };
            self.event(format!(
                "route{} frame {} -> {} ({} chunks{skipped})",
                frame.handle,
                frame.number,
                frame.key,
                frame.chunks.len()
            ));
            for chunk in frame.chunks {
                self.outbox.push((frame.key.clone(), chunk));
            }
            self.route_event(EventKind::RouteFrames, frame.handle, 1);
            if frame.skipped > 0 {
                let n = i32::try_from(frame.skipped).unwrap_or(i32::MAX);
                self.route_event(EventKind::RouteDropped, frame.handle, n);
            }
        }
    }

    /// When the next timer is due, in microseconds.
    #[cfg(test)]
    pub fn next_timer_us(&self) -> Option<u64> {
        self.timers.next_due()
    }

    /// When the next timer or source-route frame is due, in microseconds.
    pub fn next_due_us(&self) -> Option<u64> {
        [self.timers.next_due(), self.routes.next_due()]
            .into_iter()
            .flatten()
            .min()
    }

    /// The publications (key, data) made since the last call, oldest first.
    pub fn take_outbox(&mut self) -> Vec<(String, Vec<u8>)> {
        std::mem::take(&mut self.outbox)
    }

    /// The virtual panel sink routes draw on.
    pub fn panel(&self) -> &Panel {
        &self.panel
    }

    /// One line per running route with its counters.
    pub fn route_summary(&self) -> Vec<String> {
        self.routes
            .iter()
            .map(|(h, r)| {
                let (kind, key) = match r {
                    Route::Source(s) => ("source", &s.key),
                    Route::Sink(s) => ("sink", &s.key),
                };
                let s = self.routes.stats(h).unwrap_or_default();
                format!(
                    "route{h} {kind} {key}: frames {} dropped {} chunks {} chunks dropped {}",
                    s.frames, s.frames_dropped, s.chunks, s.chunks_dropped
                )
            })
            .collect()
    }

    /// Offers a received message to the sink routes. Returns `true` if a
    /// sink route consumed it (then it must not reach `on_message`):
    /// a frame start sends the preamble, then forwarded data goes to the
    /// panel with D/C high; completed and abandoned frames raise route
    /// events and print one line each.
    pub fn route_receive(&mut self, key: &str, data: &[u8]) -> bool {
        let Some((handle, sink)) = self.routes.sink_for(key) else {
            return false;
        };
        let before = sink.sink;
        let preamble = sink.cfg.preamble;
        let (step, forwarded) = sink.receive(data);
        let after = sink.sink;
        if step.action == Action::StartFrame {
            for segment in preamble.segments() {
                self.panel.write(segment.dc, segment.bytes);
            }
        }
        self.panel.write(true, forwarded);
        if step.aborted {
            if let Sink::Receiving { seq, .. } = before {
                self.event(format!("route{handle} frame {seq} dropped"));
            }
            self.route_event(EventKind::RouteDropped, handle, 1);
        }
        if step.completed {
            if let Sink::Idle { last: Some(seq) } = after {
                self.event(format!("route{handle} frame {seq} drawn"));
            }
            self.route_event(EventKind::RouteFrames, handle, 1);
        }
        true
    }

    fn route_event(&mut self, kind: EventKind, handle: u8, value: i32) {
        self.queue.push(Event {
            kind,
            source: i32::from(handle),
            value,
            time_us: self.now_us as u32,
        });
    }

    /// Completes the SPI and UART transfers started earlier, queueing their
    /// `DmaDone` events.
    pub fn complete_dma(&mut self) {
        let now = self.now_us as u32;
        for event in [self.spi.complete(now), self.uart.complete(now)]
            .into_iter()
            .flatten()
        {
            self.queue.push(event);
        }
    }

    /// The oldest queued event.
    pub fn pop_event(&mut self) -> Option<Event> {
        self.queue.pop()
    }

    /// Whether events are queued or a transfer awaits completion.
    pub fn busy(&self) -> bool {
        !self.queue.is_empty() || self.spi.in_flight() || self.uart.in_flight()
    }

    /// Applies a stdin command: `!gpio` drives an input and raises an edge
    /// event when it matches the pin's interrupt setting; `!uart` buffers
    /// received bytes and raises a `UartRx` event; `!adc` sets the value an
    /// analog pin reads.
    pub fn apply(&mut self, command: Command) -> Result<(), String> {
        match command {
            Command::Gpio { pin, high } => {
                let i = usize::from(pin);
                let before = self.input_level(pin);
                let slot = self
                    .external
                    .get_mut(i)
                    .ok_or_else(|| format!("no pin {pin}"))?;
                *slot = Some(high);
                self.event(format!("gpio {pin} <- {}", if high { 1 } else { 0 }));
                let edge = self.edges.get(i).copied().unwrap_or(Edge::Off);
                if before != Some(high) && edge.matches(high) {
                    self.queue.push(Event {
                        kind: EventKind::Gpio,
                        source: i32::from(pin),
                        value: i32::from(high),
                        time_us: self.now_us as u32,
                    });
                }
                Ok(())
            }
            Command::Uart(bytes) => {
                let event = self.uart.receive(&bytes, self.now_us as u32)?;
                self.event(format!("uart1 rx {}", render(&bytes)));
                self.queue.push(event);
                Ok(())
            }
            Command::Adc { pin, value } => {
                self.adc.set(pin, value)?;
                self.event(format!("adc {pin} <- {value}"));
                Ok(())
            }
        }
    }

    /// The level an input pin reads: the external level, else its pull.
    fn input_level(&self, pin: u8) -> Option<bool> {
        let i = usize::from(pin);
        match self.pins.get(i)? {
            Mode::Input(pull) => Some(
                self.external
                    .get(i)
                    .copied()
                    .flatten()
                    .unwrap_or(*pull == Pull::Up),
            ),
            _ => None,
        }
    }

    fn pin(&mut self, pin: u8) -> Result<&mut Mode, AbiError> {
        self.pins
            .get_mut(usize::from(pin))
            .ok_or(AbiError::Unsupported)
    }

    fn set_edge(&mut self, pin: u8, edge: Edge) {
        if let Some(e) = self.edges.get_mut(usize::from(pin)) {
            *e = edge;
        }
        if edge == Edge::Off {
            self.queue.purge(EventKind::Gpio, i32::from(pin));
        }
    }

    /// Records a printable event line stamped with the current time (and
    /// the board's label, if any).
    pub fn note(&mut self, line: String) {
        let ms = self.now_us / 1000;
        self.events.push(match &self.label {
            Some(label) => format!("[{ms:>6} ms] {label}: {line}"),
            None => format!("[{ms:>6} ms] {line}"),
        });
    }

    /// `Busy` if a sink route owns `pin` as its D/C pin.
    fn pin_free(&self, pin: u8) -> Result<(), AbiError> {
        if self.routes.owns_pin(pin) {
            Err(AbiError::Busy)
        } else {
            Ok(())
        }
    }

    fn event(&mut self, line: String) {
        self.note(line);
    }
}

impl Device for SimDevice {
    fn log(&mut self, line: &str) {
        self.event(format!("log {line}"));
    }

    fn publish(&mut self, key: Key<'_>, data: &[u8]) -> Result<(), AbiError> {
        self.event(format!("publish {key} {}", render(data)));
        self.outbox.push((key.as_str().to_string(), data.to_vec()));
        Ok(())
    }

    fn now_ms(&mut self) -> u32 {
        (self.now_us / 1000) as u32
    }

    fn gpio_output(&mut self, pin: u8) -> Result<(), AbiError> {
        self.pin_free(pin)?;
        *self.pin(pin)? = Mode::Output(false);
        self.set_edge(pin, Edge::Off);
        self.event(format!("gpio {pin} output"));
        Ok(())
    }

    fn gpio_input(&mut self, pin: u8, pull: Pull) -> Result<(), AbiError> {
        self.pin_free(pin)?;
        *self.pin(pin)? = Mode::Input(pull);
        self.event(format!("gpio {pin} input {pull:?}"));
        Ok(())
    }

    fn gpio_write(&mut self, pin: u8, high: bool) -> Result<(), AbiError> {
        self.pin_free(pin)?;
        let mode = self.pin(pin)?;
        if !matches!(mode, Mode::Output(_)) {
            return Err(AbiError::InvalidArgument);
        }
        *mode = Mode::Output(high);
        self.event(format!(
            "gpio {pin} = {}",
            if high { "high" } else { "low" }
        ));
        Ok(())
    }

    /// Outputs read back their level; inputs read the level set by `!gpio`,
    /// or their pull (up = high) until one is set.
    fn gpio_read(&mut self, pin: u8) -> Result<bool, AbiError> {
        match *self.pin(pin)? {
            Mode::Output(level) => Ok(level),
            Mode::Input(_) => self.input_level(pin).ok_or(AbiError::InvalidArgument),
            Mode::Unused => Err(AbiError::InvalidArgument),
        }
    }

    fn now_us(&mut self) -> u32 {
        self.now_us as u32
    }

    fn irq_overflow(&mut self) -> u32 {
        self.queue.take_overflow()
    }

    /// Only input pins take interrupts; [`Edge::Off`] purges pending edges.
    fn gpio_irq(&mut self, pin: u8, edge: Edge) -> Result<(), AbiError> {
        if !matches!(*self.pin(pin)?, Mode::Input(_)) {
            return Err(AbiError::InvalidArgument);
        }
        self.set_edge(pin, edge);
        self.event(format!("gpio {pin} irq {edge:?}"));
        Ok(())
    }

    /// (Re)starting a timer purges its pending events.
    fn timer_start(&mut self, spec: TimerSpec) -> Result<(), AbiError> {
        self.timers.start(spec, self.now_us)?;
        self.queue.purge(EventKind::Timer, i32::from(spec.id));
        Ok(())
    }

    fn timer_stop(&mut self, id: u8) -> Result<(), AbiError> {
        self.timers.stop(id)?;
        self.queue.purge(EventKind::Timer, i32::from(id));
        Ok(())
    }

    fn spi_open(&mut self, cfg: SpiConfig) -> Result<(), AbiError> {
        self.spi.open(cfg);
        self.event(format!(
            "spi{} open {} kHz mode {}",
            cfg.bus, cfg.freq_khz, cfg.mode
        ));
        Ok(())
    }

    /// `Busy` while a sink route owns the bus.
    fn spi_transfer(&mut self, bus: u8, tx: &[u8], len: usize) -> Result<(), AbiError> {
        if self.routes.owns_bus(bus) {
            return Err(AbiError::Busy);
        }
        self.spi.transfer(bus, tx, len)?;
        self.event(format!("spi{bus} tx {} ({len} bytes)", render(tx)));
        Ok(())
    }

    /// SPI drains the last completed transfer; UART TX has no receive data.
    fn dma_read(&mut self, channel: DmaChannel, out: &mut [u8]) -> Result<usize, AbiError> {
        match channel {
            DmaChannel::Spi0 => self.spi.read(out),
            DmaChannel::Uart1Tx => Ok(0),
        }
    }

    fn uart_open(&mut self, cfg: UartConfig) -> Result<(), AbiError> {
        self.uart.open(cfg);
        self.event(format!("uart{} open {} baud", cfg.port, cfg.baud));
        Ok(())
    }

    fn uart_write(&mut self, port: u8, data: &[u8]) -> Result<(), AbiError> {
        self.uart.write(port, data)?;
        self.event(format!("uart{port} tx {}", render(data)));
        Ok(())
    }

    fn uart_read(&mut self, port: u8, out: &mut [u8]) -> Result<usize, AbiError> {
        self.uart.read(port, out)
    }

    fn pwm_open(&mut self, spec: PwmSpec) -> Result<(), AbiError> {
        self.pwm.open(spec)?;
        self.event(format!("pwm{} open {} Hz", spec.timer, spec.freq_hz));
        Ok(())
    }

    fn pwm_set(&mut self, timer: u8, channel: u8, duty: u16) -> Result<(), AbiError> {
        self.pwm.set(timer, channel, duty)?;
        self.event(format!("pwm{timer}.{channel} duty {duty}"));
        Ok(())
    }

    fn pwm_stop(&mut self, timer: u8) -> Result<(), AbiError> {
        self.pwm.stop(timer)?;
        self.event(format!("pwm{timer} stop"));
        Ok(())
    }

    /// Reads are silent (a payload may poll every tick).
    fn adc_read(&mut self, pin: u8) -> Result<u16, AbiError> {
        self.adc.read(pin)
    }

    fn i2c_open(&mut self, cfg: I2cConfig) -> Result<(), AbiError> {
        self.i2c.open(cfg)?;
        self.event(format!("i2c{} open {} Hz", cfg.bus, cfg.freq_hz));
        Ok(())
    }

    fn i2c_transfer(
        &mut self,
        bus: u8,
        addr: u8,
        tx: &[u8],
        rx: &mut [u8],
    ) -> Result<(), AbiError> {
        let result = self.i2c.transfer(bus, addr, tx, rx);
        let shown = |b: &[u8]| {
            if b.is_empty() {
                "-".to_string()
            } else {
                render(b)
            }
        };
        let outcome = match result {
            Ok(()) => format!("r {}", shown(rx)),
            Err(e) => format!("{e:?}"),
        };
        self.event(format!("i2c{bus} {addr:#04x} w {} {outcome}", shown(tx)));
        result
    }

    /// Only the test pattern; the camera is `Unsupported`. The first frame
    /// is due one period from now.
    fn route_source(
        &mut self,
        source: SourceKind,
        key: Key<'_>,
        cfg: SourceConfig,
    ) -> Result<u8, AbiError> {
        if source != SourceKind::TestPattern {
            return Err(AbiError::Unsupported);
        }
        let handle = self.routes.add(Route::Source(SourceRoute {
            key: key.as_str().to_string(),
            cfg,
            frame: 0,
            due_us: self.now_us + u64::from(cfg.period_ms) * 1000,
            stats: Default::default(),
        }))?;
        self.event(format!(
            "route{handle} source {source:?} {}x{} every {} ms on {key} ({} B chunks)",
            cfg.width, cfg.height, cfg.period_ms, cfg.chunk_data
        ));
        Ok(handle)
    }

    /// `Busy` unless `bus` is open and no other sink owns it; `Unsupported`
    /// for a D/C pin past [`PINS`]. The D/C pin becomes an output.
    fn route_sink(&mut self, key: Key<'_>, bus: u8, cfg: &SinkConfig) -> Result<u8, AbiError> {
        if !self.spi.is_open(bus) || self.routes.owns_bus(bus) || self.spi.in_flight() {
            return Err(AbiError::Busy);
        }
        self.pin_free(cfg.dc_pin)?;
        *self.pin(cfg.dc_pin)? = Mode::Output(true);
        self.set_edge(cfg.dc_pin, Edge::Off);
        let handle = self.routes.add(Route::Sink(SinkRoute {
            key: key.as_str().to_string(),
            bus,
            cfg: *cfg,
            sink: Sink::new(),
            stats: Default::default(),
        }))?;
        self.event(format!(
            "route{handle} sink {key} -> spi{bus} dc {} ({} B frames, {} B preamble)",
            cfg.dc_pin,
            cfg.frame_len,
            cfg.preamble.byte_len()
        ));
        Ok(handle)
    }

    fn route_stop(&mut self, handle: u8) -> Result<(), AbiError> {
        self.routes.stop(handle)?;
        for kind in [
            EventKind::RouteFrames,
            EventKind::RouteDropped,
            EventKind::RouteStopped,
        ] {
            self.queue.purge(kind, i32::from(handle));
        }
        self.event(format!("route{handle} stop"));
        Ok(())
    }

    fn route_stat(&mut self, handle: u8, stat: RouteStat) -> Result<u32, AbiError> {
        Ok(stat.of(&self.routes.stats(handle)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_arguments() {
        let o = parse_args(&[
            "p.wasm",
            "--ticks",
            "3",
            "--fuel",
            "99",
            "--pages",
            "4",
            "--virtual",
        ])
        .unwrap();
        assert_eq!(o.file, "p.wasm");
        assert_eq!(o.ticks, Some(3));
        assert_eq!(o.limits, Limits::pages(4).with_fuel(99));
        assert!(o.virtual_time);
        assert!(parse_args::<&str>(&[]).is_err());
        assert!(parse_args(&["a", "b"]).is_err());
        assert!(parse_args(&["a", "--ticks"]).is_err());
        assert!(parse_args(&["a", "--ticks", "x"]).is_err());
        assert!(parse_args(&["a", "--bogus"]).is_err());
        let o = parse_args(&["a", "--until-ms", "50", "--spi-pattern", "0xa55a"]).unwrap();
        assert_eq!(o.until_ms, Some(50));
        assert_eq!(o.spi_pattern, Some(vec![0xA5, 0x5A]));
        assert!(parse_args(&["a", "--spi-pattern", "abc"]).is_err());
        assert!(parse_args(&["a", "--spi-pattern", "zz"]).is_err());
        let o = parse_args(&[
            "a",
            "--peer",
            "b.wasm",
            "--loss",
            "250",
            "--seed",
            "9",
            "--dump-panel",
            "p.ppm",
        ])
        .unwrap();
        assert_eq!(o.peer.as_deref(), Some("b.wasm"));
        assert_eq!((o.loss, o.seed), (250, 9));
        assert_eq!(o.dump_panel.as_deref(), Some("p.ppm"));
        assert!(parse_args(&["a", "--loss", "1001"]).is_err());
        assert!(parse_args(&["a", "--peer"]).is_err());
    }

    #[test]
    fn parses_commands() {
        assert_eq!(
            parse_command("!gpio 4 1\n"),
            Ok(Some(Command::Gpio { pin: 4, high: true }))
        );
        assert_eq!(
            parse_command("!uart hi there"),
            Ok(Some(Command::Uart(b"hi there".to_vec())))
        );
        assert_eq!(parse_command("in/a x"), Ok(None));
        assert!(parse_command("!gpio 4 2").is_err());
        assert!(parse_command("!gpio 4").is_err());
        assert!(parse_command("!gpio x 1").is_err());
        assert!(parse_command("!uart").is_err());
        assert_eq!(
            parse_command("!adc 3 1234"),
            Ok(Some(Command::Adc {
                pin: 3,
                value: 1234
            }))
        );
        assert!(parse_command("!adc 3").is_err());
        assert!(parse_command("!adc 3 4 5").is_err());
        assert!(parse_command("!adc 3 x").is_err());
        assert!(parse_command("!nope").is_err());
    }

    #[test]
    fn gpio_edges_raise_matching_events_only() {
        let mut d = SimDevice::default();
        d.gpio_input(4, Pull::Down).unwrap();
        assert_eq!(d.gpio_irq(9, Edge::Rising), Err(AbiError::InvalidArgument));
        assert_eq!(d.gpio_irq(200, Edge::Rising), Err(AbiError::Unsupported));
        d.gpio_irq(4, Edge::Rising).unwrap();
        d.apply(Command::Gpio { pin: 4, high: true }).unwrap();
        d.apply(Command::Gpio { pin: 4, high: true }).unwrap(); // no change
        d.apply(Command::Gpio {
            pin: 4,
            high: false,
        })
        .unwrap(); // falling
        assert_eq!(d.gpio_read(4), Ok(false));
        let e = d.pop_event().unwrap();
        assert_eq!((e.kind, e.source, e.value), (EventKind::Gpio, 4, 1));
        assert_eq!(d.pop_event(), None);
        d.apply(Command::Gpio { pin: 4, high: true }).unwrap();
        d.gpio_irq(4, Edge::Off).unwrap();
        assert_eq!(d.pop_event(), None, "disabling purges pending edges");
    }

    #[test]
    fn overflow_is_reported_and_reset() {
        let mut d = SimDevice::default();
        d.gpio_input(4, Pull::Down).unwrap();
        d.gpio_irq(4, Edge::Any).unwrap();
        for i in 0..MAX_EVENTS + 3 {
            d.apply(Command::Gpio {
                pin: 4,
                high: i % 2 == 0,
            })
            .unwrap();
        }
        assert_eq!(d.irq_overflow(), 3);
        assert_eq!(d.irq_overflow(), 0);
    }

    #[test]
    fn timers_follow_the_clock_and_dma_completes_later() {
        let mut d = SimDevice::default();
        d.timer_start(TimerSpec {
            id: 1,
            period_us: 500,
            periodic: true,
        })
        .unwrap();
        assert_eq!(d.next_timer_us(), Some(500));
        d.advance_to(1600);
        let e = d.pop_event().unwrap();
        assert_eq!(
            (e.kind, e.source, e.value, e.time_us),
            (EventKind::Timer, 1, 3, 1500)
        );
        d.advance_to(2000);
        d.timer_stop(1).unwrap();
        assert!(!d.busy(), "stopping purges the pending event");

        d.spi_open(SpiConfig {
            bus: 0,
            sck: 1,
            mosi: Some(2),
            miso: Some(3),
            cs: None,
            freq_khz: 1,
            mode: 0,
        })
        .unwrap();
        d.spi_transfer(0, b"ab", 2).unwrap();
        assert!(d.busy() && d.pop_event().is_none());
        d.complete_dma();
        let e = d.pop_event().unwrap();
        assert_eq!((e.kind, e.source, e.value), (EventKind::DmaDone, 0, 2));
        let mut out = [0; 4];
        assert_eq!(d.dma_read(DmaChannel::Spi0, &mut out), Ok(2));
        assert_eq!(&out[..2], b"ab");
    }

    #[test]
    fn parses_lines() {
        let (k, d) = parse_line("in/a hello world\n").unwrap().unwrap();
        assert_eq!((k.as_str(), d), ("in/a", &b"hello world"[..]));
        let (k, d) = parse_line("k").unwrap().unwrap();
        assert_eq!((k.as_str(), d), ("k", &b""[..]));
        assert_eq!(parse_line("  \n"), Ok(None));
        assert!(parse_line("bad//key x").is_err());
    }

    #[test]
    fn renders_text_or_hex() {
        assert_eq!(render(b"hi"), "hi");
        assert_eq!(render(&[0, 255]), "0x00ff");
    }

    #[test]
    fn gpio_simulation() {
        let mut d = SimDevice::default();
        assert_eq!(d.gpio_write(2, true), Err(AbiError::InvalidArgument));
        d.gpio_output(2).unwrap();
        d.gpio_write(2, true).unwrap();
        assert_eq!(d.gpio_read(2), Ok(true));
        d.gpio_input(3, Pull::Up).unwrap();
        assert_eq!(d.gpio_read(3), Ok(true));
        assert_eq!(d.gpio_output(200), Err(AbiError::Unsupported));
        assert_eq!(d.events.len(), 3);
        assert!(d.events[1].ends_with("gpio 2 = high"));
    }

    #[test]
    fn abi3_peripherals() {
        let mut d = SimDevice::default();
        d.pwm_open(PwmSpec {
            timer: 1,
            freq_hz: 1000,
        })
        .unwrap();
        d.pwm_set(1, 2, 32_768).unwrap();
        assert_eq!(d.pwm.duty(1, 2), Some(32_768));
        assert!(d.events.last().unwrap().ends_with("pwm1.2 duty 32768"));
        assert_eq!(d.adc_read(5), Ok(0));
        d.apply(Command::Adc { pin: 5, value: 777 }).unwrap();
        assert_eq!(d.adc_read(5), Ok(777));
        assert!(
            d.apply(Command::Adc {
                pin: 5,
                value: 4096
            })
            .is_err()
        );
        assert_eq!(d.adc_read(200), Err(AbiError::Unsupported));
        d.i2c_open(I2cConfig {
            bus: 0,
            freq_hz: 100_000,
        })
        .unwrap();
        d.i2c_transfer(0, 0x50, &[0, b'o', b'k'], &mut []).unwrap();
        let mut rx = [0; 2];
        d.i2c_transfer(0, 0x50, &[0], &mut rx).unwrap();
        assert_eq!(&rx, b"ok");
        assert!(d.events.last().unwrap().ends_with("i2c0 0x50 w 0x00 r ok"));
        assert_eq!(
            d.i2c_transfer(0, 0x10, &[0], &mut rx),
            Err(AbiError::Transport)
        );
        assert!(d.events.last().unwrap().ends_with("Transport"));
        d.pwm_stop(1).unwrap();
        assert_eq!(d.pwm.duty(1, 2), None);
    }

    use middleware::stream::pattern;
    use wasm_host::Preamble;

    const W: u16 = 20;
    const H: u16 = 10;
    /// Window top-left corner on the panel.
    const X0: u16 = 5;
    const Y0: u16 = 7;

    /// A camera board with a 20×10 test-pattern source on `cam` (64-byte
    /// chunks, 10 ms), and a display board with a sink drawing it at
    /// `(X0, Y0)`.
    fn camera_and_display() -> (SimDevice, SimDevice) {
        let key = Key::new("cam").unwrap();
        let cfg = SourceConfig::new(W, H, 10)
            .unwrap()
            .with_chunk_data(64)
            .unwrap();
        let mut cam = SimDevice::default();
        assert_eq!(
            cam.route_source(SourceKind::Camera, key, cfg),
            Err(AbiError::Unsupported)
        );
        assert_eq!(cam.route_source(SourceKind::TestPattern, key, cfg), Ok(0));
        let mut view = SimDevice::default();
        let window = Preamble::mipi_window(X0, Y0, X0 + W - 1, Y0 + H - 1).unwrap();
        let sink = SinkConfig::new(13, cfg.frame_len())
            .unwrap()
            .with_preamble(window);
        assert_eq!(
            view.route_sink(key, 0, &sink),
            Err(AbiError::Busy),
            "bus closed"
        );
        view.spi_open(SpiConfig {
            bus: 0,
            sck: 12,
            mosi: Some(11),
            miso: None,
            cs: Some(10),
            freq_khz: 40_000,
            mode: 0,
        })
        .unwrap();
        assert_eq!(view.route_sink(key, 0, &sink), Ok(0));
        assert_eq!(
            view.route_sink(key, 0, &sink),
            Err(AbiError::Busy),
            "bus taken"
        );
        (cam, view)
    }

    /// Runs `frames` frame periods, delivering each chunk when `pass` says
    /// so.
    fn stream(
        cam: &mut SimDevice,
        view: &mut SimDevice,
        frames: u64,
        mut pass: impl FnMut() -> bool,
    ) {
        for f in 1..=frames {
            cam.advance_to(f * 10_000);
            for (key, data) in cam.take_outbox() {
                if pass() {
                    assert!(view.route_receive(&key, &data));
                }
            }
        }
    }

    fn in_window(x: u16, y: u16) -> bool {
        (X0..X0 + W).contains(&x) && (Y0..Y0 + H).contains(&y)
    }

    #[test]
    fn r10_source_chunks_drive_the_sink_onto_the_panel() {
        let (mut cam, mut view) = camera_and_display();
        assert_eq!(view.spi_transfer(0, b"x", 1), Err(AbiError::Busy));
        assert_eq!(view.gpio_write(13, false), Err(AbiError::Busy));
        assert!(!view.route_receive("other", b"x"));
        stream(&mut cam, &mut view, 3, || true);
        let panel = view.panel();
        for y in 0..panel::HEIGHT {
            for x in 0..panel::WIDTH {
                if in_window(x, y) {
                    let want = pattern::pixel(2, W, H, x - X0, y - Y0);
                    assert_eq!(panel.pixel(x, y), Some(want), "({x}, {y})");
                } else {
                    assert!(!panel.written(x, y), "({x}, {y}) outside the window");
                }
            }
        }
        // 400 bytes in 64-byte chunks: 7 per frame.
        assert_eq!(cam.route_stat(0, RouteStat::Frames), Ok(3));
        assert_eq!(cam.route_stat(0, RouteStat::Chunks), Ok(21));
        assert_eq!(view.route_stat(0, RouteStat::Frames), Ok(3));
        assert_eq!(view.route_stat(0, RouteStat::Chunks), Ok(21));
        assert_eq!(view.route_stat(0, RouteStat::ChunksDropped), Ok(0));
        let e = view.pop_event().unwrap();
        assert_eq!((e.kind, e.source, e.value), (EventKind::RouteFrames, 0, 3));
        let e = cam.pop_event().unwrap();
        assert_eq!((e.kind, e.source, e.value), (EventKind::RouteFrames, 0, 3));
        assert!(
            view.events
                .iter()
                .any(|l| l.ends_with("route0 frame 2 drawn"))
        );
        assert!(
            cam.events
                .iter()
                .any(|l| l.ends_with("route0 frame 2 -> cam (7 chunks)"))
        );

        assert_eq!(view.route_stop(0), Ok(()));
        assert_eq!(view.route_stop(0), Err(AbiError::Busy));
        assert_eq!(view.route_stat(0, RouteStat::Frames), Err(AbiError::Busy));
        assert!(view.spi_transfer(0, b"x", 1).is_ok(), "bus released");
        assert!(!view.route_receive("cam", b"x"), "key released");
    }

    #[test]
    fn r10_lossy_delivery_never_misplaces_pixels() {
        let (mut cam, mut view) = camera_and_display();
        let mut bus = bus::Bus::new(150, 42);
        let frames = 40;
        stream(&mut cam, &mut view, frames, || bus.pass());
        assert!(bus.lost > 0);
        let panel = view.panel();
        for y in 0..panel::HEIGHT {
            for x in 0..panel::WIDTH {
                if !in_window(x, y) {
                    assert!(!panel.written(x, y), "({x}, {y}) outside the window");
                } else if panel.written(x, y) {
                    let got = panel.pixel(x, y);
                    assert!(
                        (0..frames as u32)
                            .any(|f| Some(pattern::pixel(f, W, H, x - X0, y - Y0)) == got),
                        "({x}, {y}) = {got:?} is in no frame"
                    );
                }
            }
        }
        let frames_drawn = view.route_stat(0, RouteStat::Frames).unwrap();
        let dropped = view.route_stat(0, RouteStat::FramesDropped).unwrap();
        assert!(
            frames_drawn > 0 && dropped > 0,
            "{frames_drawn} drawn, {dropped} dropped"
        );
        assert!(frames_drawn + dropped <= frames as u32);
        assert!(view.route_stat(0, RouteStat::ChunksDropped).unwrap() > 0);
    }

    #[test]
    fn r10_late_clock_skips_source_frames() {
        let (mut cam, _) = camera_and_display();
        cam.advance_to(35_000);
        assert_eq!(cam.take_outbox().len(), 7, "only the latest frame");
        assert_eq!(cam.route_stat(0, RouteStat::FramesDropped), Ok(2));
        assert_eq!(cam.next_due_us(), Some(40_000));
        let kinds: Vec<_> = std::iter::from_fn(|| cam.pop_event())
            .map(|e| (e.kind, e.value))
            .collect();
        assert_eq!(
            kinds,
            vec![(EventKind::RouteFrames, 1), (EventKind::RouteDropped, 2)]
        );
        assert!(
            cam.events
                .last()
                .unwrap()
                .ends_with("route0 frame 2 -> cam (7 chunks, 2 skipped)")
        );
    }
}
