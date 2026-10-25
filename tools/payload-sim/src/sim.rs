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

pub mod periph;

use std::fmt::Write as _;

use middleware::Key;
use middleware::wasm_abi::events::{EventQueue, MAX_EVENTS};
use wasm_host::{
    AbiError, Device, DmaChannel, Edge, Event, EventKind, Limits, Pull, SpiConfig, TimerSpec,
    UartConfig,
};

use periph::{Spi, Timers, Uart};

/// Parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// Path of the `.wasm` payload.
    pub file: String,
    /// Stop after this many ticks; `None` runs until stdin closes.
    pub ticks: Option<u32>,
    /// Runtime limits (`--fuel`, `--pages`).
    pub limits: Limits,
    /// Use virtual time: stdin is processed first, then the clock jumps
    /// straight to each due tick or timer.
    pub virtual_time: bool,
    /// Stop once the clock would pass this many milliseconds.
    pub until_ms: Option<u64>,
    /// Bytes the SPI device returns instead of looping back (`--spi-pattern`).
    pub spi_pattern: Option<Vec<u8>>,
}

/// Usage text printed on argument errors.
pub const USAGE: &str = "usage: payload-sim <file.wasm> [--ticks N] [--fuel F] [--pages P] [--virtual]\n\
                         [--until-ms T] [--spi-pattern HEX]\n\
stdin lines `<key> <text>` are delivered to the payload;\n\
`!gpio <pin> <0|1>` sets an input level, `!uart <text>` types at the UART1 terminal";

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
    };
    let mut it = args.iter().map(AsRef::as_ref);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match arg {
            "--ticks" => opts.ticks = Some(number(value("--ticks")?)?),
            "--fuel" => opts.limits.fuel_per_call = number(value("--fuel")?)?,
            "--pages" => opts.limits.max_memory_pages = number(value("--pages")?)?,
            "--virtual" => opts.virtual_time = true,
            "--until-ms" => opts.until_ms = Some(number(value("--until-ms")?)?),
            "--spi-pattern" => opts.spi_pattern = Some(hex(value("--spi-pattern")?)?),
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
pub const PINS: usize = 48;

/// In-memory board. The shell moves the clock with
/// [`SimDevice::advance_to`] before each call and drains
/// [`SimDevice::events`] after it.
pub struct SimDevice {
    /// Event lines produced since the last drain.
    pub events: Vec<String>,
    now_us: u64,
    pins: [Mode; PINS],
    /// Level driven from outside (`!gpio`); overrides the pull when set.
    external: [Option<bool>; PINS],
    edges: [Edge; PINS],
    queue: EventQueue<MAX_EVENTS>,
    timers: Timers,
    spi: Spi,
    uart: Uart,
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
            now_us: 0,
            pins: [Mode::Unused; PINS],
            external: [None; PINS],
            edges: [Edge::Off; PINS],
            queue: EventQueue::new(),
            timers: Timers::default(),
            spi: Spi::new(spi_pattern),
            uart: Uart::default(),
        }
    }

    /// Moves the clock to `now_us` (never backwards) and queues every timer
    /// event due by then.
    pub fn advance_to(&mut self, now_us: u64) {
        self.now_us = self.now_us.max(now_us);
        for event in self.timers.fire(self.now_us) {
            self.queue.push(event);
        }
    }

    /// When the next timer is due, in microseconds.
    pub fn next_timer_us(&self) -> Option<u64> {
        self.timers.next_due()
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
    /// received bytes and raises a `UartRx` event.
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

    /// Records a printable event line stamped with the current time.
    pub fn note(&mut self, line: String) {
        self.events
            .push(format!("[{:>6} ms] {line}", self.now_us / 1000));
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
        Ok(())
    }

    fn now_ms(&mut self) -> u32 {
        (self.now_us / 1000) as u32
    }

    fn gpio_output(&mut self, pin: u8) -> Result<(), AbiError> {
        *self.pin(pin)? = Mode::Output(false);
        self.set_edge(pin, Edge::Off);
        self.event(format!("gpio {pin} output"));
        Ok(())
    }

    fn gpio_input(&mut self, pin: u8, pull: Pull) -> Result<(), AbiError> {
        *self.pin(pin)? = Mode::Input(pull);
        self.event(format!("gpio {pin} input {pull:?}"));
        Ok(())
    }

    fn gpio_write(&mut self, pin: u8, high: bool) -> Result<(), AbiError> {
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

    fn spi_transfer(&mut self, bus: u8, tx: &[u8], len: usize) -> Result<(), AbiError> {
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
        assert_eq!(
            o.limits,
            Limits {
                max_memory_pages: 4,
                fuel_per_call: 99
            }
        );
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
}
