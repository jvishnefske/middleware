//! Desktop payload simulator (requirements W7, D10, X7 and R10).
//!
//! `payload-sim <file.wasm> [--ticks N] [--fuel F] [--pages P | --memory-bytes B] [--virtual]
//! [--until-ms T] [--spi-pattern HEX] [--peer <other.wasm>] [--loss PERMILLE] [--seed S]
//! [--dump-panel <file.ppm>] [--panel-dc <pin>]` loads a payload with the same runtime as the
//! firmware (`wasm-host`) and runs it against [`sim::SimDevice`], printing
//! every log line, publish, GPIO change and peripheral transfer to stdout
//! as `[   time ms] event`.
//!
//! Each stdin line `<key> <text>` is delivered to the payload; lines whose
//! key matches no subscription print `drop <key>`. Lines starting with `!`
//! drive the virtual peripherals (ABI 2):
//!
//! * `!gpio <pin> <0|1>` sets an input pin's level and raises a GPIO event
//!   if the transition matches the pin's `gpio_irq` edge.
//! * `!uart <text>` buffers the text as bytes received on UART1 and raises a
//!   (coalesced) `UartRx` event.
//! * `!adc <pin> <0..4095>` sets the value `adc_read(pin)` returns (ABI 3;
//!   pins read 0 until set).
//!
//! The ABI 3 PWM and I2C imports print `pwm<t> open <f> Hz`,
//! `pwm<t>.<ch> duty <d>`, `pwm<t> stop`, `i2c<bus> open <f> Hz` and
//! `i2c<bus> <addr> w <bytes> r <bytes>` (or the error). Each I2C bus has a
//! 256-byte register device at address 0x50; other addresses do not
//! acknowledge.
//!
//! Hardware timers run on the simulator clock. `spi_transfer` loops the
//! transmitted bytes back (or returns `--spi-pattern`, repeated) and
//! `uart_write` prints `uart1 tx <text>`; both complete with a `DmaDone`
//! event on the next loop iteration. Every iteration drains at most
//! [`MAX_EVENTS`] queued events into `Payload::dispatch` (`on_irq`).
//! A transfer prints `spi<bus> tx <bytes> (<len> bytes)`; transmit buffers
//! longer than 32 bytes show only their first 16 bytes, in hex, followed by
//! `…`, as in `spi0 tx 0x000102030405060708090a0b0c0d0e0f… (3840 bytes)`.
//!
//! ABI 4 DMA routes (R10) run on the same clock: a test-pattern source
//! route publishes each frame as chunks and prints one line per frame
//! (`route<h> frame <n> -> <key> (<c> chunks)`); a sink route draws the
//! chunks arriving on its key on a virtual 240×240 GC9A01-like panel and
//! prints `route<h> frame <seq> drawn` or `... dropped`. When the run ends,
//! each running route prints its counters, and `--dump-panel` writes the
//! panel framebuffer as a binary PPM (P6) image.
//!
//! * `--panel-dc <pin>` also puts the panel on the payload's own SPI bus 0
//!   with `<pin>` as its D/C line, so a payload driving a GC9A01 directly
//!   (CASET / RASET / RAMWR with D/C low for commands, high for data) draws
//!   on it too. Each accepted `spi_transfer` on bus 0 feeds the panel the
//!   bytes clocked out (`tx` padded with `0x00` to the transfer length) at
//!   the pin's current output level; while the pin is not configured as a
//!   GPIO output the panel ignores the transfer. In a `--peer` run it
//!   applies to both boards.
//!
//! When the run ends, each board also prints its payload's fuel use
//! (every export call, `init` included) as
//! `fuel calls <C> max <M> total <T>`: the number of calls, the most fuel
//! one call used, and the saturating total (labelled in `--peer` runs).
//!
//! * `--peer <other.wasm>` runs a second payload on its own board in the
//!   same process. Each board's publications (a payload's own and its
//!   source routes' chunks) are delivered to the other board: to a sink
//!   route on the key, else to a matching subscription (unmatched messages
//!   are ignored silently). Output lines are labelled with each file's
//!   name; stdin lines go to the first payload. A delivery may cause a
//!   reply; at most [`MAX_EXCHANGE_ROUNDS`] rounds of replies run per step.
//!   The run ends with `bus: delivered D lost L`. `--dump-panel` writes the
//!   first board's panel if anything was drawn on it, else the peer's.
//! * `--loss P` loses each message on that bus with probability `P / 1000`,
//!   decided by a generator seeded with `--seed S` (default 1), so lossy
//!   runs are reproducible.
//!
//! Timing:
//!
//! * Real time (default): ticks, timers and route frames fire on the wall
//!   clock while stdin is read on a helper thread. The run ends when stdin
//!   closes, unless `--ticks N` (keep going until every payload with a tick
//!   has had `N` ticks) or `--until-ms T` (keep going until `T` ms) is given.
//! * `--virtual`: all stdin lines are processed at time 0, then the clock
//!   jumps from one due tick, timer or route frame to the next. Each payload
//!   ticks at most `--ticks N` times (none without it); the run ends when
//!   nothing is due or queued, or when the next due time is past
//!   `--until-ms` (a source route never stops by itself, so give one).
//!   Output is deterministic.
//!
//! If `init` traps, the lines it logged before the trap are printed as
//! `[     0 ms] log <line>` (from the runtime's bounded
//! [`InitLog`](wasm_host::InitLog)), followed by `load failed: ...` on
//! stderr.
//!
//! Exit status: 0 on success, 1 on bad arguments or a rejected module
//! (including a trapping `init`), 2 if a payload trapped later.
//!
//! [`sim`] holds the pure parts; this file is the I/O shell.

mod sim;

use std::io::{BufRead, Write};
use std::process::ExitCode;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use middleware::Key;
use middleware::wasm_abi::events::MAX_EVENTS;
use wasm_host::{LoadError, Payload, Trap};

use sim::bus::Bus;
use sim::{Options, SimDevice};

type Sim = Payload<SimDevice>;

/// Upper bound on virtual-time loop iterations, so a periodic timer that is
/// never stopped (and no `--until-ms`) still ends the run.
const MAX_VIRTUAL_STEPS: u32 = 1_000_000;

/// Upper bound on delivery rounds between two boards per step, so two
/// payloads echoing each other cannot loop forever.
pub const MAX_EXCHANGE_ROUNDS: u32 = 64;

/// A trap, with the label of the board whose payload trapped.
struct Fault {
    node: Option<String>,
    trap: Trap,
}

/// One simulated board running one payload.
struct Node {
    payload: Sim,
    /// Label in a `--peer` run.
    label: Option<String>,
    /// Ticks fired so far.
    ticks: u32,
}

impl Node {
    fn device(&mut self) -> &mut SimDevice {
        self.payload.device()
    }

    fn fault<T>(&self, r: Result<T, Trap>) -> Result<T, Fault> {
        r.map_err(|trap| Fault {
            node: self.label.clone(),
            trap,
        })
    }
}

/// The boards and the bus between them.
struct Bench {
    nodes: Vec<Node>,
    bus: Bus,
}

/// Prints and clears the device's pending events.
fn flush(p: &mut Sim) {
    let mut out = std::io::stdout().lock();
    for line in p.device().events.drain(..) {
        let _ = writeln!(out, "{line}");
    }
    let _ = out.flush();
}

/// Completes deferred DMA, then dispatches at most [`MAX_EVENTS`] queued
/// events to `on_irq`.
fn pump(p: &mut Sim) -> Result<(), Trap> {
    p.device().complete_dma();
    let result = drain(p);
    flush(p);
    result
}

fn drain(p: &mut Sim) -> Result<(), Trap> {
    for _ in 0..MAX_EVENTS {
        let Some(event) = p.device().pop_event() else {
            break;
        };
        p.dispatch(event)?;
    }
    Ok(())
}

/// Offers a message to a board: its sink routes first, then its
/// subscriptions. Returns whether anything took it.
fn offer(p: &mut Sim, key: Key<'_>, data: &[u8]) -> Result<bool, Trap> {
    if p.device().route_receive(key.as_str(), data) {
        return Ok(true);
    }
    p.deliver(key, data)
}

impl Bench {
    fn busy(&mut self) -> bool {
        self.nodes.iter_mut().any(|n| n.device().busy())
    }

    fn advance_to(&mut self, now_us: u64) {
        for n in &mut self.nodes {
            n.device().advance_to(now_us);
        }
    }

    /// Hands each board's publications to the other board over the bus
    /// (or discards them when there is no peer), for at most
    /// [`MAX_EXCHANGE_ROUNDS`] rounds.
    fn exchange(&mut self) -> Result<(), Fault> {
        for _ in 0..MAX_EXCHANGE_ROUNDS {
            let mut moved = false;
            for from in 0..self.nodes.len() {
                let Some(sender) = self.nodes.get_mut(from) else {
                    continue;
                };
                let outbox = sender.device().take_outbox();
                let to = (from + 1) % self.nodes.len();
                if to == from {
                    continue;
                }
                let Some(receiver) = self.nodes.get_mut(to) else {
                    continue;
                };
                for (key, data) in outbox {
                    moved = true;
                    if !self.bus.pass() {
                        continue;
                    }
                    let Ok(key) = Key::new(&key) else { continue };
                    let r = offer(&mut receiver.payload, key, &data);
                    receiver.fault(r)?;
                }
                flush(&mut receiver.payload);
            }
            if !moved {
                return Ok(());
            }
        }
        eprintln!("stopped exchanging after {MAX_EXCHANGE_ROUNDS} rounds");
        for n in &mut self.nodes {
            n.device().take_outbox();
        }
        Ok(())
    }

    /// Pumps every board, then exchanges their publications.
    fn pump(&mut self) -> Result<(), Fault> {
        for n in &mut self.nodes {
            let r = pump(&mut n.payload);
            n.fault(r)?;
        }
        self.exchange()
    }

    /// Fires the tick of board `i` due at `now_us`. The callers only call
    /// it when a tick is due, so whether it ran ([`wasm_host::Ticked`]) is
    /// not reported.
    fn tick(&mut self, i: usize, now_us: u64) -> Result<(), Fault> {
        let Some(n) = self.nodes.get_mut(i) else {
            return Ok(());
        };
        n.device().advance_to(now_us);
        let r = n.payload.tick((now_us / 1000) as u32).map(|_| ());
        flush(&mut n.payload);
        n.ticks += 1;
        n.fault(r)?;
        self.exchange()
    }

    /// Handles one stdin line on the first board at the current time: a `!`
    /// command for the virtual peripherals, or a message for the payload.
    fn handle_line(&mut self, line: &str) -> Result<(), Fault> {
        let Some(n) = self.nodes.first_mut() else {
            return Ok(());
        };
        match sim::parse_command(line) {
            Ok(Some(command)) => {
                if let Err(e) = n.device().apply(command) {
                    eprintln!("{e}");
                }
                self.pump()
            }
            Ok(None) => {
                let r = deliver(&mut n.payload, line);
                n.fault(r)?;
                self.exchange()
            }
            Err(e) => {
                eprintln!("{e}");
                Ok(())
            }
        }
    }

    /// Prints the route counters, fuel use and bus summary, and dumps the
    /// panel.
    fn finish(&mut self, opts: &Options) {
        for n in &mut self.nodes {
            for line in n.device().route_summary() {
                n.device().note(line);
            }
            let fuel = n.payload.fuel_stats();
            n.device().note(format!(
                "fuel calls {} max {} total {}",
                fuel.calls, fuel.max_call, fuel.total
            ));
            flush(&mut n.payload);
        }
        if self.nodes.len() > 1 {
            println!(
                "bus: delivered {} lost {}",
                self.bus.delivered, self.bus.lost
            );
        }
        if let Some(path) = &opts.dump_panel {
            let drawn = self
                .nodes
                .iter_mut()
                .position(|n| n.device().panel().pixel_writes() > 0)
                .unwrap_or(0);
            if let Some(n) = self.nodes.get_mut(drawn) {
                let ppm = n.device().panel().to_ppm();
                if let Err(e) = std::fs::write(path, ppm) {
                    eprintln!("{path}: {e}");
                }
            }
        }
    }
}

/// Delivers one stdin message line.
fn deliver(p: &mut Sim, line: &str) -> Result<(), Trap> {
    let result = match sim::parse_line(line) {
        Ok(Some((key, data))) => match offer(p, key, data) {
            Ok(true) => Ok(()),
            Ok(false) => {
                p.device().note(format!("drop {key}"));
                Ok(())
            }
            Err(t) => Err(t),
        },
        Ok(None) => Ok(()),
        Err(e) => {
            eprintln!("{e}");
            Ok(())
        }
    };
    flush(p);
    result
}

/// Reports a rejected module: for a trapping `init`, first the lines it
/// logged (in the device's `log` format, at time 0), then the error.
fn report_load_error(e: &LoadError) {
    let mut out = std::io::stdout().lock();
    if let LoadError::Init { trap, log } = e {
        if log.dropped() > 0 {
            let _ = writeln!(
                out,
                "[{:>6} ms] ({} earlier log lines dropped)",
                0,
                log.dropped()
            );
        }
        for line in log.lines() {
            let _ = writeln!(out, "[{:>6} ms] log {line}", 0);
        }
        let _ = out.flush();
        eprintln!("load failed: init failed: {trap}");
    } else {
        eprintln!("load failed: {e}");
    }
}

/// Signed milliseconds from `now` until `due` on the wrapping clock.
fn until(due: u32, now: u32) -> i64 {
    i64::from(due.wrapping_sub(now) as i32)
}

fn run_virtual(b: &mut Bench, opts: &Options) -> Result<(), Fault> {
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        b.handle_line(&line)?;
    }
    let max_ticks = opts.ticks.unwrap_or(0);
    let limit_us = opts.until_ms.map(|ms| ms.saturating_mul(1000));
    for _ in 0..MAX_VIRTUAL_STEPS {
        b.pump()?;
        if b.busy() {
            continue;
        }
        let ticks: Vec<Option<u64>> = b
            .nodes
            .iter()
            .map(|n| {
                (n.ticks < max_ticks)
                    .then(|| n.payload.next_tick())
                    .flatten()
                    .map(|ms| u64::from(ms) * 1000)
            })
            .collect();
        let next = ticks
            .iter()
            .copied()
            .chain(b.nodes.iter_mut().map(|n| n.device().next_due_us()))
            .flatten()
            .min();
        let Some(next) = next else { return Ok(()) };
        if limit_us.is_some_and(|limit| next > limit) {
            return Ok(());
        }
        b.advance_to(next);
        for (i, due) in ticks.iter().enumerate() {
            if *due == Some(next) {
                b.tick(i, next)?;
            }
        }
    }
    eprintln!("stopped after {MAX_VIRTUAL_STEPS} virtual steps");
    Ok(())
}

fn run_real(b: &mut Bench, opts: &Options) -> Result<(), Fault> {
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let start = Instant::now();
    let now_us = || u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX);
    let limit_us = opts.until_ms.map(|ms| ms.saturating_mul(1000));
    let max_ticks = opts.ticks.unwrap_or(u32::MAX);
    let mut stdin_open = true;
    loop {
        let now = now_us();
        if limit_us.is_some_and(|limit| now >= limit) {
            return Ok(());
        }
        b.advance_to(now);
        b.pump()?;
        let ticking: Vec<Option<i64>> = b
            .nodes
            .iter()
            .map(|n| {
                (n.ticks < max_ticks)
                    .then(|| n.payload.next_tick())
                    .flatten()
                    .map(|due| until(due, (now / 1000) as u32).saturating_mul(1000))
            })
            .collect();
        if opts.ticks.is_some() && ticking.iter().all(Option::is_none) {
            return Ok(());
        }
        if let Some(i) = ticking.iter().position(|w| w.is_some_and(|w| w <= 0)) {
            b.tick(i, now)?;
            continue;
        }
        if b.busy() {
            continue;
        }
        let tick_wait = ticking.iter().copied().flatten().min();
        let due_wait = b
            .nodes
            .iter_mut()
            .filter_map(|n| n.device().next_due_us())
            .min()
            .map(|due| i64::try_from(due.saturating_sub(now)).unwrap_or(i64::MAX));
        let limit_wait =
            limit_us.map(|limit| i64::try_from(limit.saturating_sub(now)).unwrap_or(i64::MAX));
        let wait = [tick_wait, due_wait, limit_wait]
            .into_iter()
            .flatten()
            .min()
            .map_or(Duration::from_secs(3600), |w| {
                Duration::from_micros(w.max(0).unsigned_abs())
            });
        if !stdin_open {
            let ticking = opts.ticks.is_some() && tick_wait.is_some();
            if !ticking && limit_us.is_none() {
                return Ok(());
            }
            std::thread::sleep(wait);
            continue;
        }
        match rx.recv_timeout(wait) {
            Ok(line) => {
                b.advance_to(now_us());
                b.handle_line(&line)?;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => stdin_open = false,
        }
    }
}

/// The label of `path` in a `--peer` run: its file stem.
fn label(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .map_or_else(|| path.to_string(), |s| s.to_string_lossy().into_owned())
}

/// Loads one payload onto a fresh board.
fn load(path: &str, label: Option<String>, opts: &Options) -> Result<Node, ExitCode> {
    let wasm = std::fs::read(path).map_err(|e| {
        eprintln!("{path}: {e}");
        ExitCode::from(1)
    })?;
    let device = SimDevice::new(opts.spi_pattern.clone());
    let device = match &label {
        Some(l) => device.with_label(l),
        None => device,
    };
    let device = match opts.panel_dc {
        Some(pin) => device.with_panel_dc(pin),
        None => device,
    };
    let mut payload = Payload::load(&wasm, device, opts.limits).map_err(|e| {
        report_load_error(&e);
        ExitCode::from(1)
    })?;
    flush(&mut payload);
    let subs: Vec<&str> = payload.subscriptions().collect();
    println!(
        "loaded {path} (subscriptions: [{}], next tick: {:?})",
        subs.join(", "),
        payload.next_tick()
    );
    Ok(Node {
        payload,
        label,
        ticks: 0,
    })
}

fn run(opts: &Options) -> Result<(), ExitCode> {
    let labels = match &opts.peer {
        Some(peer) => {
            let (a, mut b) = (label(&opts.file), label(peer));
            if a == b {
                b.push_str("-peer");
            }
            vec![(opts.file.as_str(), Some(a)), (peer.as_str(), Some(b))]
        }
        None => vec![(opts.file.as_str(), None)],
    };
    let nodes = labels
        .into_iter()
        .map(|(path, label)| load(path, label, opts))
        .collect::<Result<Vec<_>, _>>()?;
    let mut bench = Bench {
        nodes,
        bus: Bus::new(opts.loss, opts.seed),
    };
    let result = bench.exchange().and_then(|()| {
        if opts.virtual_time {
            run_virtual(&mut bench, opts)
        } else {
            run_real(&mut bench, opts)
        }
    });
    bench.finish(opts);
    result.map_err(|f| {
        match f.node {
            Some(node) => println!("trap: {node}: {}", f.trap),
            None => println!("trap: {}", f.trap),
        }
        ExitCode::from(2)
    })
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match sim::parse_args(&args) {
        Ok(opts) => match run(&opts) {
            Ok(()) => ExitCode::SUCCESS,
            Err(code) => code,
        },
        Err(e) => {
            eprintln!("{e}\n{}", sim::USAGE);
            ExitCode::from(1)
        }
    }
}
