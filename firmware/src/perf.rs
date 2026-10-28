//! Run-time accounting of where the payload loop's time goes (pure).
//!
//! The controller's payload session (`src/shell/controller.rs`) alternates
//! between calls into the payload, waiting for the next input, one yield to
//! the other tasks per iteration, and its own host work (requests, sink
//! routes, the interrupt queue). With [`ENABLED`] it times each of these
//! with the microsecond clock and folds the measurements into a [`Window`];
//! the radio task adds the time it spends handling received frames to a
//! [`RadioTime`]. Every [`REPORT_INTERVAL_US`] the controller logs the
//! window's [`Summary`] as two `sys/log` lines and starts a new one:
//!
//! ```text
//! perf: 5000 ms: wasm 4210 ms (84.2%), 61 calls, 6104k fuel, 1449k fuel/s busy; max op 12345 us 260k fuel
//! perf: msg 580/1100k/900 ms, irq 70/4900k/3200 ms, tick 2/100k/1 ms; wait 500 ms, yield 200 ms, host 90 ms; radio 120 ms, 1450 rx
//! ```
//!
//! * **wasm** — wall time inside payload operations (a delivery is two
//!   export calls, `mw_alloc` then `on_message`), its share of the window,
//!   the export calls and fuel they used (`wasm_host::FuelStats` before and
//!   after), and the fuel rate while busy: the interpreter's speed on this
//!   board, including anything that preempts it (Wi-Fi interrupts and
//!   driver threads).
//! * **max op** — the longest single operation, in microseconds, and the
//!   most fuel one operation used.
//! * **msg / irq / tick** — export calls / fuel / wall time per kind of
//!   operation: deliveries (`on_message`, from the serial links or
//!   ESP-NOW), interrupt events (`on_irq`: timers, `DmaDone`, GPIO, UART)
//!   and `tick`.
//! * **wait** — time the controller waited for its next input, during which
//!   the executor ran other tasks or the CPU idled; **yield** — time the
//!   once-per-iteration yield gave the other tasks; **host** — the rest of
//!   the window: the controller's own work outside payload calls.
//! * **radio** — time the radio task spent handling received frames
//!   (transport decode and the queue to the controller; not esp-radio's
//!   driver threads or interrupts), and how many frames it handled.
//!
//! Everything is an integer (no FPU on some targets); the counts saturate.

use core::fmt;

/// Whether the controller measures and logs its time (on for now: it is
/// cheap, two clock reads per operation, and tells where a slow payload's
/// time goes).
pub const ENABLED: bool = true;

/// Length of one reporting window, in microseconds.
pub const REPORT_INTERVAL_US: u64 = 5_000_000;

/// What a payload operation was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallKind {
    /// A delivered publication (`mw_alloc` and `on_message`).
    Message,
    /// An interrupt event (`on_irq`).
    Irq,
    /// A due `tick`.
    Tick,
}

impl CallKind {
    const fn index(self) -> usize {
        match self {
            CallKind::Message => 0,
            CallKind::Irq => 1,
            CallKind::Tick => 2,
        }
    }
}

/// What one payload operation cost.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cost {
    /// Export calls made.
    pub calls: u64,
    /// Fuel those calls used.
    pub fuel: u64,
    /// Wall time, in microseconds.
    pub us: u64,
}

impl Cost {
    /// Nothing ran.
    pub const ZERO: Cost = Cost { calls: 0, fuel: 0, us: 0 };

    /// The cost between two readings of a payload's cumulative
    /// `(calls, fuel)` counters and the clock (`start_us`, `end_us`).
    /// Counters or a clock that went backwards count as zero.
    pub const fn between(before: (u64, u64), after: (u64, u64), start_us: u64, end_us: u64) -> Cost {
        Cost {
            calls: after.0.saturating_sub(before.0),
            fuel: after.1.saturating_sub(before.1),
            us: end_us.saturating_sub(start_us),
        }
    }
}

/// Export calls, fuel and wall time of one kind of operation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KindStats {
    /// Export calls.
    pub calls: u64,
    /// Fuel used.
    pub fuel: u64,
    /// Wall time, in microseconds.
    pub us: u64,
}

impl KindStats {
    const ZERO: KindStats = KindStats { calls: 0, fuel: 0, us: 0 };

    const fn add(self, c: Cost) -> KindStats {
        KindStats {
            calls: self.calls.saturating_add(c.calls),
            fuel: self.fuel.saturating_add(c.fuel),
            us: self.us.saturating_add(c.us),
        }
    }
}

/// Time the radio task spent on received frames, and how many.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RadioTime {
    /// Wall time, in microseconds.
    pub us: u64,
    /// Frames handled.
    pub frames: u32,
}

impl RadioTime {
    /// None yet.
    pub const ZERO: RadioTime = RadioTime { us: 0, frames: 0 };

    /// `frames` more frames handled in `us` microseconds.
    pub const fn add(self, us: u64, frames: u32) -> RadioTime {
        RadioTime { us: self.us.saturating_add(us), frames: self.frames.saturating_add(frames) }
    }
}

/// One reporting window of the controller's payload loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// When the window started (microsecond clock).
    pub start_us: u64,
    /// Per [`CallKind`]: message, irq, tick.
    pub kinds: [KindStats; 3],
    /// The longest single operation, in microseconds.
    pub max_op_us: u64,
    /// The most fuel a single operation used.
    pub max_op_fuel: u64,
    /// Time spent waiting for the next input.
    pub wait_us: u64,
    /// Time spent in the once-per-iteration yield.
    pub yield_us: u64,
}

impl Window {
    /// An empty window starting at `now_us`.
    pub const fn new(now_us: u64) -> Window {
        Window { start_us: now_us, kinds: [KindStats::ZERO; 3], max_op_us: 0, max_op_fuel: 0, wait_us: 0, yield_us: 0 }
    }

    /// The window with one more operation of `kind` that cost `cost`.
    #[must_use]
    pub const fn op(self, kind: CallKind, cost: Cost) -> Window {
        let mut kinds = self.kinds;
        let i = kind.index();
        kinds[i] = kinds[i].add(cost);
        Window {
            kinds,
            max_op_us: if cost.us > self.max_op_us { cost.us } else { self.max_op_us },
            max_op_fuel: if cost.fuel > self.max_op_fuel { cost.fuel } else { self.max_op_fuel },
            ..self
        }
    }

    /// The window with `us` more microseconds of waiting for input.
    #[must_use]
    pub const fn waited(self, us: u64) -> Window {
        Window { wait_us: self.wait_us.saturating_add(us), ..self }
    }

    /// The window with `us` more microseconds of yielding.
    #[must_use]
    pub const fn yielded(self, us: u64) -> Window {
        Window { yield_us: self.yield_us.saturating_add(us), ..self }
    }

    /// Whether the window is [`REPORT_INTERVAL_US`] long at `now_us`.
    pub const fn due(&self, now_us: u64) -> bool {
        now_us.saturating_sub(self.start_us) >= REPORT_INTERVAL_US
    }

    /// All operations together.
    pub const fn wasm(&self) -> KindStats {
        self.kinds[0].add(as_cost(self.kinds[1])).add(as_cost(self.kinds[2]))
    }

    /// The window's report at `now_us`, with the radio task's time.
    pub const fn summary(&self, now_us: u64, radio: RadioTime) -> Summary {
        let wall_us = now_us.saturating_sub(self.start_us);
        let wasm = self.wasm();
        let host_us = wall_us.saturating_sub(wasm.us).saturating_sub(self.wait_us).saturating_sub(self.yield_us);
        Summary {
            wall_us,
            wasm,
            busy_permille: ratio(wasm.us, 1000, wall_us),
            fuel_per_s: ratio(wasm.fuel, 1_000_000, wasm.us),
            kinds: self.kinds,
            max_op_us: self.max_op_us,
            max_op_fuel: self.max_op_fuel,
            wait_us: self.wait_us,
            yield_us: self.yield_us,
            host_us,
            radio,
        }
    }
}

const fn as_cost(k: KindStats) -> Cost {
    Cost { calls: k.calls, fuel: k.fuel, us: k.us }
}

/// `n * scale / d`, 0 when `d` is 0, saturating.
pub const fn ratio(n: u64, scale: u64, d: u64) -> u64 {
    if d == 0 {
        return 0;
    }
    let q = (n as u128 * scale as u128) / d as u128;
    if q > u64::MAX as u128 { u64::MAX } else { q as u64 }
}

/// A finished window, ready to log ([`Summary::wasm_line`],
/// [`Summary::host_line`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    /// Length of the window, in microseconds.
    pub wall_us: u64,
    /// All payload operations together.
    pub wasm: KindStats,
    /// Share of the window spent in payload operations, in ‰.
    pub busy_permille: u64,
    /// Fuel per second while in payload operations.
    pub fuel_per_s: u64,
    /// Per [`CallKind`]: message, irq, tick.
    pub kinds: [KindStats; 3],
    /// The longest single operation, in microseconds.
    pub max_op_us: u64,
    /// The most fuel a single operation used.
    pub max_op_fuel: u64,
    /// Time spent waiting for the next input.
    pub wait_us: u64,
    /// Time spent yielding.
    pub yield_us: u64,
    /// The rest: the controller's own work.
    pub host_us: u64,
    /// The radio task's time on received frames.
    pub radio: RadioTime,
}

impl Summary {
    /// The first log line: totals and the interpreter's speed.
    pub const fn wasm_line(&self) -> WasmLine<'_> {
        WasmLine(self)
    }

    /// The second log line: the split per kind and the host's time.
    pub const fn host_line(&self) -> HostLine<'_> {
        HostLine(self)
    }
}

/// [`Summary::wasm_line`].
pub struct WasmLine<'a>(&'a Summary);

/// [`Summary::host_line`].
pub struct HostLine<'a>(&'a Summary);

impl fmt::Display for WasmLine<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = self.0;
        write!(
            f,
            "perf: {} ms: wasm {} ms ({}.{}%), {} calls, {}k fuel, {}k fuel/s busy; max op {} us {}k fuel",
            s.wall_us / 1000,
            s.wasm.us / 1000,
            s.busy_permille / 10,
            s.busy_permille % 10,
            s.wasm.calls,
            s.wasm.fuel / 1000,
            s.fuel_per_s / 1000,
            s.max_op_us,
            s.max_op_fuel / 1000,
        )
    }
}

impl fmt::Display for HostLine<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = self.0;
        f.write_str("perf:")?;
        for (name, k) in [("msg", s.kinds[0]), ("irq", s.kinds[1]), ("tick", s.kinds[2])] {
            let sep = if name == "msg" { " " } else { ", " };
            write!(f, "{sep}{name} {}/{}k/{} ms", k.calls, k.fuel / 1000, k.us / 1000)?;
        }
        write!(
            f,
            "; wait {} ms, yield {} ms, host {} ms; radio {} ms, {} rx",
            s.wait_us / 1000,
            s.yield_us / 1000,
            s.host_us / 1000,
            s.radio.us / 1000,
            s.radio.frames,
        )
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::format;

    fn cost(calls: u64, fuel: u64, us: u64) -> Cost {
        Cost { calls, fuel, us }
    }

    #[test]
    fn cost_is_the_difference_of_the_counters_and_saturates() {
        assert_eq!(Cost::between((3, 100), (5, 350), 10, 40), cost(2, 250, 30));
        assert_eq!(Cost::between((5, 350), (3, 100), 40, 10), Cost::ZERO);
    }

    #[test]
    fn window_folds_operations_per_kind_and_tracks_the_largest() {
        let w = Window::new(1_000)
            .op(CallKind::Message, cost(2, 3_000, 400))
            .op(CallKind::Irq, cost(1, 250_000, 12_000))
            .op(CallKind::Irq, cost(1, 100_000, 15_000))
            .op(CallKind::Tick, cost(1, 5_000, 300))
            .waited(7_000)
            .yielded(2_000);
        assert_eq!(w.kinds[0], KindStats { calls: 2, fuel: 3_000, us: 400 });
        assert_eq!(w.kinds[1], KindStats { calls: 2, fuel: 350_000, us: 27_000 });
        assert_eq!(w.kinds[2], KindStats { calls: 1, fuel: 5_000, us: 300 });
        assert_eq!((w.max_op_us, w.max_op_fuel), (15_000, 250_000));
        assert_eq!(w.wasm(), KindStats { calls: 5, fuel: 358_000, us: 27_700 });
    }

    #[test]
    fn due_after_one_interval() {
        let w = Window::new(10);
        assert!(!w.due(10 + REPORT_INTERVAL_US - 1));
        assert!(w.due(10 + REPORT_INTERVAL_US));
        assert!(!w.due(0), "a clock behind the start is not due");
    }

    #[test]
    fn summary_splits_the_wall_time_and_rates_the_fuel() {
        // 5 s window: 4 s in wasm running 6 M fuel, 0.5 s waiting, 0.2 s
        // yielding, so 0.3 s of host work.
        let w = Window::new(0)
            .op(CallKind::Irq, cost(60, 6_000_000, 4_000_000))
            .waited(500_000)
            .yielded(200_000);
        let s = w.summary(5_000_000, RadioTime::ZERO.add(120_000, 1_450));
        assert_eq!(s.wall_us, 5_000_000);
        assert_eq!(s.busy_permille, 800);
        assert_eq!(s.fuel_per_s, 1_500_000);
        assert_eq!(s.host_us, 300_000);
        assert_eq!(s.radio, RadioTime { us: 120_000, frames: 1_450 });
    }

    #[test]
    fn empty_window_reports_zeros_without_dividing_by_zero() {
        let s = Window::new(5).summary(5, RadioTime::ZERO);
        assert_eq!((s.busy_permille, s.fuel_per_s, s.host_us), (0, 0, 0));
        assert_eq!(ratio(u64::MAX, u64::MAX, 1), u64::MAX);
    }

    #[test]
    fn log_lines_fit_a_sys_log_line() {
        let w = Window::new(0)
            .op(CallKind::Message, cost(580, 1_100_000, 900_000))
            .op(CallKind::Irq, cost(70, 4_900_000, 3_200_000))
            .op(CallKind::Tick, cost(2, 100_000, 1_000))
            .waited(500_000)
            .yielded(200_000);
        let s = w.summary(5_000_000, RadioTime::ZERO.add(120_000, 1_450));
        let wasm = format!("{}", s.wasm_line());
        let host = format!("{}", s.host_line());
        assert_eq!(
            wasm,
            "perf: 5000 ms: wasm 4101 ms (82.0%), 652 calls, 6100k fuel, 1487k fuel/s busy; max op 3200000 us 4900k fuel"
        );
        assert_eq!(
            host,
            "perf: msg 580/1100k/900 ms, irq 70/4900k/3200 ms, tick 2/100k/1 ms; wait 500 ms, yield 200 ms, host 199 ms; radio 120 ms, 1450 rx"
        );
        // Every count ten digits wide (far beyond a 5 s window) still fits
        // `MAX_LOG`; the bus truncates anything longer.
        const W: u64 = u32::MAX as u64;
        let big = KindStats { calls: W, fuel: W, us: W };
        let s = Summary {
            wall_us: W,
            wasm: big,
            busy_permille: 1000,
            fuel_per_s: W,
            kinds: [big; 3],
            max_op_us: W,
            max_op_fuel: W,
            wait_us: W,
            yield_us: W,
            host_us: W,
            radio: RadioTime { us: W, frames: u32::MAX },
        };
        assert!(format!("{}", s.wasm_line()).len() <= middleware::wasm_abi::MAX_LOG);
        assert!(format!("{}", s.host_line()).len() <= middleware::wasm_abi::MAX_LOG);
    }
}
