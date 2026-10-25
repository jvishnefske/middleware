//! Desktop payload simulator (requirements W7 and D10).
//!
//! `payload-sim <file.wasm> [--ticks N] [--fuel F] [--pages P | --memory-bytes B] [--virtual]
//! [--until-ms T] [--spi-pattern HEX]` loads a payload with the same
//! runtime as the firmware (`wasm-host`) and runs it against
//! [`sim::SimDevice`], printing every log line, publish, GPIO change and
//! peripheral transfer to stdout as `[   time ms] event`.
//!
//! Each stdin line `<key> <text>` is delivered to the payload; lines whose
//! key matches no subscription print `drop <key>`. Lines starting with `!`
//! drive the virtual peripherals (ABI 2):
//!
//! * `!gpio <pin> <0|1>` sets an input pin's level and raises a GPIO event
//!   if the transition matches the pin's `gpio_irq` edge.
//! * `!uart <text>` buffers the text as bytes received on UART1 and raises a
//!   (coalesced) `UartRx` event.
//!
//! Hardware timers run on the simulator clock. `spi_transfer` loops the
//! transmitted bytes back (or returns `--spi-pattern`, repeated) and
//! `uart_write` prints `uart1 tx <text>`; both complete with a `DmaDone`
//! event on the next loop iteration. Every iteration drains at most
//! [`MAX_EVENTS`] queued events into `Payload::dispatch` (`on_irq`).
//!
//! * Real time (default): ticks and timers fire on the wall clock while
//!   stdin is read on a helper thread. The run ends when stdin closes,
//!   unless `--ticks N` (keep going until `N` ticks have fired) or
//!   `--until-ms T` (keep going until `T` ms) is given.
//! * `--virtual`: all stdin lines are processed at time 0, then the clock
//!   jumps from one due tick or timer to the next. Ticks fire only up to
//!   `--ticks N` (none without it); the run ends when nothing is due or
//!   queued, or when the next due time is past `--until-ms`. Output is
//!   deterministic.
//!
//! Exit status: 0 on success, 1 on bad arguments or a rejected module, 2 if
//! the payload trapped.
//!
//! [`sim`] holds the pure parts; this file is the I/O shell.

mod sim;

use std::io::{BufRead, Write};
use std::process::ExitCode;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use middleware::wasm_abi::events::MAX_EVENTS;
use wasm_host::{Payload, Trap};

use sim::{Options, SimDevice};

type Sim = Payload<SimDevice>;

/// Upper bound on virtual-time loop iterations, so a periodic timer that is
/// never stopped (and no `--until-ms`) still ends the run.
const MAX_VIRTUAL_STEPS: u32 = 1_000_000;

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

/// Handles one stdin line at the current simulator time: a `!` command for
/// the virtual peripherals, or a message for the payload.
fn handle_line(p: &mut Sim, line: &str) -> Result<(), Trap> {
    match sim::parse_command(line) {
        Ok(Some(command)) => {
            if let Err(e) = p.device().apply(command) {
                eprintln!("{e}");
            }
            pump(p)
        }
        Ok(None) => deliver(p, line),
        Err(e) => {
            eprintln!("{e}");
            Ok(())
        }
    }
}

/// Delivers one stdin message line.
fn deliver(p: &mut Sim, line: &str) -> Result<(), Trap> {
    let result = match sim::parse_line(line) {
        Ok(Some((key, data))) => match p.deliver(key, data) {
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

/// Fires the tick due at `now_us`.
fn tick(p: &mut Sim, now_us: u64) -> Result<(), Trap> {
    p.device().advance_to(now_us);
    let result = p.tick((now_us / 1000) as u32);
    flush(p);
    result
}

/// Signed milliseconds from `now` until `due` on the wrapping clock.
fn until(due: u32, now: u32) -> i64 {
    i64::from(due.wrapping_sub(now) as i32)
}

fn run_virtual(p: &mut Sim, opts: &Options) -> Result<(), Trap> {
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        handle_line(p, &line)?;
    }
    let mut ticks_left = opts.ticks.unwrap_or(0);
    let limit_us = opts.until_ms.map(|ms| ms.saturating_mul(1000));
    for _ in 0..MAX_VIRTUAL_STEPS {
        pump(p)?;
        if p.device().busy() {
            continue;
        }
        let tick_us = if ticks_left > 0 {
            p.next_tick().map(|ms| u64::from(ms) * 1000)
        } else {
            None
        };
        let next = [tick_us, p.device().next_timer_us()]
            .into_iter()
            .flatten()
            .min();
        let Some(next) = next else { return Ok(()) };
        if limit_us.is_some_and(|limit| next > limit) {
            return Ok(());
        }
        p.device().advance_to(next);
        if tick_us == Some(next) {
            tick(p, next)?;
            ticks_left -= 1;
        }
    }
    eprintln!("stopped after {MAX_VIRTUAL_STEPS} virtual steps");
    Ok(())
}

fn run_real(p: &mut Sim, opts: &Options) -> Result<(), Trap> {
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
    let mut fired = 0u32;
    let mut stdin_open = true;
    loop {
        let now = now_us();
        if limit_us.is_some_and(|limit| now >= limit) {
            return Ok(());
        }
        p.device().advance_to(now);
        pump(p)?;
        if opts.ticks.is_some_and(|max| fired >= max) {
            return Ok(());
        }
        let tick_wait = p
            .next_tick()
            .map(|due| until(due, (now / 1000) as u32).saturating_mul(1000));
        if tick_wait.is_some_and(|w| w <= 0) {
            tick(p, now)?;
            fired += 1;
            continue;
        }
        if p.device().busy() {
            continue;
        }
        let timer_wait = p
            .device()
            .next_timer_us()
            .map(|due| i64::try_from(due.saturating_sub(now)).unwrap_or(i64::MAX));
        let limit_wait =
            limit_us.map(|limit| i64::try_from(limit.saturating_sub(now)).unwrap_or(i64::MAX));
        let wait = [tick_wait, timer_wait, limit_wait]
            .into_iter()
            .flatten()
            .min()
            .map_or(Duration::from_secs(3600), |w| {
                Duration::from_micros(w.max(0).unsigned_abs())
            });
        if !stdin_open {
            let ticking = opts.ticks.is_some() && p.next_tick().is_some();
            if !ticking && limit_us.is_none() {
                return Ok(());
            }
            std::thread::sleep(wait);
            continue;
        }
        match rx.recv_timeout(wait) {
            Ok(line) => {
                p.device().advance_to(now_us());
                handle_line(p, &line)?;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => stdin_open = false,
        }
    }
}

fn run(opts: &Options) -> Result<(), ExitCode> {
    let wasm = std::fs::read(&opts.file).map_err(|e| {
        eprintln!("{}: {e}", opts.file);
        ExitCode::from(1)
    })?;
    let device = SimDevice::new(opts.spi_pattern.clone());
    let mut p = Payload::load(&wasm, device, opts.limits).map_err(|e| {
        eprintln!("load failed: {e}");
        ExitCode::from(1)
    })?;
    flush(&mut p);
    let subs: Vec<&str> = p.subscriptions().collect();
    println!(
        "loaded {} (subscriptions: [{}], next tick: {:?})",
        opts.file,
        subs.join(", "),
        p.next_tick()
    );
    let result = if opts.virtual_time {
        run_virtual(&mut p, opts)
    } else {
        run_real(&mut p, opts)
    };
    result.map_err(|t| {
        println!("trap: {t}");
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
