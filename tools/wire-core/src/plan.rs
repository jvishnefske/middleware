//! A checked wiring in update order, and the reference for what it computes.
//!
//! [`Plan::new`] puts every source, function and sink in the one update
//! order ([`Step`]): each after the nodes that feed it, where the
//! connections into a `delay` do not count. [`Sim`] is the reference
//! semantics; the payload [`crate::wasm::compile`] emits must do exactly
//! what it does (the tests run both).
//!
//! # One tick
//!
//! The wiring's period is the base tick, numbered from 0 (tick 0 runs in
//! the payload's `init`). A step whose period is `every` base ticks is
//! *due* on tick `k` when `k % every == 0`. On each tick, in order:
//!
//! 1. every due step updates: a source reads its pin, a function computes
//!    from the channels' current values, and both write their channels; a
//!    sink writes its pin if the level or duty changed. A due `delay`
//!    writes the value it stored;
//! 2. then every due `delay` stores its input channel's current value.
//!
//! A channel holds the last value written, so a slower reader samples it
//! and a faster reader sees it held. Channels and state start at 0. The
//! tick number is a `u32` that wraps.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::check::{Issue, check, has_errors, update_order};
use crate::graph::{Class, Config, DEFAULT_PWM_HZ, Graph, Kind};
use crate::model::{self, Shape, is_model};
use crate::real::{Program, round_sat, sin_program};

/// A channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Channel {
    /// Its name.
    pub name: String,
    /// Whether its value is published on `wire/<name>` when it changes.
    pub trace: bool,
}

/// One source, function or sink, at its place in the update order.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    /// The node's id.
    pub id: String,
    /// The node's index in the graph.
    pub node: usize,
    /// What it is.
    pub kind: Kind,
    /// Its settings.
    pub config: Config,
    /// It updates on the base ticks divisible by this (at least 1).
    pub every: u32,
    /// Its own update period in ms (`every` base periods).
    pub period_ms: u32,
    /// Indices of the channels it reads, in argument order.
    pub args: Vec<usize>,
    /// Indices of the channels it drives; for a `ukf` or `cbf`, output
    /// `k` goes to `outs[k]`.
    pub outs: Vec<usize>,
    /// What a `ukf` or `cbf` computes ([`crate::model`]).
    pub program: Option<Program>,
}

impl Step {
    /// A numeric setting, or the table's default when the node has none.
    pub fn param(&self, name: &str) -> i32 {
        let default = || self.kind.spec().param(name).and_then(|p| p.default).unwrap_or(0);
        self.config.get(name).unwrap_or_else(default) as i32
    }

    /// How many consecutive updates a `ton` waits: its `ms` in whole
    /// update periods, rounded up.
    pub fn ton_updates(&self) -> i32 {
        self.config.ms.unwrap_or(0).div_ceil(self.period_ms.max(1)) as i32
    }
}

/// A wiring ready to simulate or compile.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// The wiring's name.
    pub name: String,
    /// The base tick.
    pub period_ms: u32,
    /// The channels, in node order.
    pub channels: Vec<Channel>,
    /// Sources, functions and sinks in update order.
    pub steps: Vec<Step>,
}

impl Plan {
    /// Orders a wiring that has no errors; otherwise returns its issues.
    pub fn new(graph: &Graph) -> Result<Plan, Vec<Issue>> {
        let issues = check(graph, None);
        if has_errors(&issues) {
            return Err(issues);
        }
        let n = graph.nodes.len();
        let index: BTreeMap<&str, usize> =
            graph.nodes.iter().enumerate().map(|(i, node)| (node.id.as_str(), i)).collect();
        let mut into: Vec<Vec<usize>> = vec![Vec::new(); n];
        let mut out: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (from, to) in &graph.edges {
            if let (Some(&f), Some(&t)) = (index.get(from.as_str()), index.get(to.as_str())) {
                into[t].push(f);
                out[f].push(t);
            }
        }
        let mut channel_of: Vec<usize> = vec![0; n];
        let mut channels = Vec::new();
        for (i, node) in graph.nodes.iter().enumerate() {
            if node.kind == Kind::Channel {
                channel_of[i] = channels.len();
                channels.push(Channel { name: node.id.clone(), trace: node.config.trace });
            }
        }
        let kinds: Vec<Kind> = graph.nodes.iter().map(|node| node.kind).collect();
        let (order, _) = update_order(&kinds, &out);
        let base = graph.period_ms.max(1);
        let steps = order
            .into_iter()
            .filter(|&i| kinds[i] != Kind::Channel)
            .map(|i| {
                let node = &graph.nodes[i];
                let period_ms = node.config.period_ms.unwrap_or(base);
                let shape = Shape { inputs: into[i].len(), outputs: out[i].len(), period_ms };
                // The check built the same program, so it has no problems.
                let program =
                    is_model(node.kind).then(|| model::program(node.kind, &node.config, &shape).ok()).flatten();
                Step {
                    id: node.id.clone(),
                    node: i,
                    kind: node.kind,
                    config: node.config.clone(),
                    every: (period_ms / base).max(1),
                    period_ms,
                    args: into[i].iter().map(|&f| channel_of[f]).collect(),
                    outs: out[i].iter().map(|&t| channel_of[t]).collect(),
                    program,
                }
            })
            .collect();
        Ok(Plan { name: graph.name.clone(), period_ms: graph.period_ms, channels, steps })
    }

    /// The steps in node order (the order `init` configures pins in).
    pub fn steps_by_node(&self) -> Vec<&Step> {
        let mut steps: Vec<&Step> = self.steps.iter().collect();
        steps.sort_by_key(|s| s.node);
        steps
    }

    /// The index of the channel called `name`.
    pub fn channel(&self, name: &str) -> Option<usize> {
        self.channels.iter().position(|c| c.name == name)
    }

    /// Every channel's value after the first tick, in [`Plan::channels`]
    /// order, for the pin levels `read` reports (analog pins read 0).
    pub fn channel_values(&self, read: impl FnMut(u8) -> bool) -> Vec<i32> {
        let mut io = FirstScan { read, writes: Vec::new() };
        let mut sim = Sim::new(self);
        sim.step(&mut io);
        sim.channels().to_vec()
    }

    /// The first tick: the level written to every digital output pin, in
    /// update order, for the pin levels `read` reports. For a wiring
    /// without state or rates this is what every tick computes.
    pub fn eval(&self, read: impl FnMut(u8) -> bool) -> Vec<(u8, bool)> {
        let mut io = FirstScan { read, writes: Vec::new() };
        Sim::new(self).step(&mut io);
        io.writes
    }
}

struct FirstScan<F> {
    read: F,
    writes: Vec<(u8, bool)>,
}

impl<F: FnMut(u8) -> bool> Io for FirstScan<F> {
    fn gpio_read(&mut self, pin: u8) -> bool {
        (self.read)(pin)
    }
    fn gpio_write(&mut self, pin: u8, high: bool) {
        self.writes.push((pin, high));
    }
}

/// The board as the reference sees it. Every method has a default (pins
/// read low and 0, writes go nowhere), so a test implements what it uses.
pub trait Io {
    /// The level of a digital input pin.
    fn gpio_read(&mut self, pin: u8) -> bool {
        let _ = pin;
        false
    }
    /// The raw reading of an analog pin, `0..=4095`; negative is an error
    /// and reads as 0.
    fn adc_read(&mut self, pin: u8) -> i32 {
        let _ = pin;
        0
    }
    /// A digital output changed level (or is written for the first time).
    fn gpio_write(&mut self, pin: u8, high: bool) {
        let _ = (pin, high);
    }
    /// A PWM duty changed (or is set for the first time).
    fn pwm_set(&mut self, timer: u8, channel: u8, duty: u16) {
        let _ = (timer, channel, duty);
    }
    /// A traced channel's value changed (or is published for the first
    /// time): the payload publishes `value` little-endian on
    /// `wire/<channel>`.
    fn publish(&mut self, channel: &str, value: i32) {
        let _ = (channel, value);
    }
}

/// The largest PWM duty.
pub const PWM_FULL: i32 = 65_535;

/// The reference simulator: a [`Plan`] and its state between ticks.
#[derive(Debug, Clone, PartialEq)]
pub struct Sim<'a> {
    plan: &'a Plan,
    tick: u32,
    channels: Vec<i32>,
    /// Two words of state per step; what each kind keeps is in [`Sim::update`].
    state: Vec<[i32; 2]>,
    /// The level or duty a sink last wrote; -1 before the first.
    written: Vec<i32>,
    /// What a traced channel last published.
    published: Vec<Option<i32>>,
    /// The real state of each `ukf` step ([`Program::init`]).
    real: Vec<Vec<f64>>,
    /// The one sine ([`sin_program`]).
    sin: Program,
}

impl<'a> Sim<'a> {
    /// A wiring before its first tick: every channel and all state at 0.
    pub fn new(plan: &'a Plan) -> Self {
        Sim {
            plan,
            tick: 0,
            channels: vec![0; plan.channels.len()],
            state: vec![[0; 2]; plan.steps.len()],
            written: vec![-1; plan.steps.len()],
            published: vec![None; plan.channels.len()],
            real: plan
                .steps
                .iter()
                .map(|s| s.program.as_ref().map_or_else(Vec::new, |p| p.init.clone()))
                .collect(),
            sin: sin_program(),
        }
    }

    /// The number of the next tick (0 before the first [`Sim::step`]).
    pub fn tick(&self) -> u32 {
        self.tick
    }

    /// Every channel's current value, in [`Plan::channels`] order.
    pub fn channels(&self) -> &[i32] {
        &self.channels
    }

    /// The current value of the channel called `name`.
    pub fn channel(&self, name: &str) -> Option<i32> {
        self.plan.channel(name).map(|i| self.channels[i])
    }

    /// The real state of the `ukf` node called `id`: the `n` estimates,
    /// the lower triangle of `P` by rows, then the fault count.
    pub fn reals(&self, id: &str) -> Option<&[f64]> {
        let i = self.plan.steps.iter().position(|s| s.id == id && s.program.is_some())?;
        Some(&self.real[i])
    }

    /// How many updates of the `ukf` node called `id` were unsound and
    /// reset it ([`crate::model`]).
    pub fn faults(&self, id: &str) -> Option<u32> {
        let i = self.plan.steps.iter().position(|s| s.id == id)?;
        let word = self.plan.steps[i].program.as_ref()?.faults?;
        self.real[i].get(word as usize).map(|&count| count as u32)
    }

    /// Runs one base tick against `io`.
    pub fn step(&mut self, io: &mut impl Io) {
        let plan = self.plan;
        let tick = self.tick;
        for (i, step) in plan.steps.iter().enumerate() {
            if !tick.is_multiple_of(step.every) {
                continue;
            }
            let arg = |k: usize| step.args.get(k).map_or(0, |&c| self.channels[c]);
            match step.kind {
                Kind::Output => {
                    let level = (arg(0) != 0) != step.config.invert;
                    if self.written[i] != i32::from(level) {
                        self.written[i] = i32::from(level);
                        io.gpio_write(step.config.pin.unwrap_or(0), level);
                    }
                }
                Kind::Pwm => {
                    let duty = arg(0).clamp(0, PWM_FULL);
                    if self.written[i] != duty {
                        self.written[i] = duty;
                        io.pwm_set(
                            step.config.timer.unwrap_or(0),
                            step.config.channel.unwrap_or(0),
                            duty as u16,
                        );
                    }
                }
                _ => {
                    if let Some(program) = &step.program {
                        // All of it is computed from the old state and the
                        // channels as they are, then written.
                        let channels = &self.channels;
                        let chan = |k: u32| step.args.get(k as usize).map_or(0, |&c| channels[c]);
                        let values = program.eval(&chan, &[], &self.real[i], &self.sin);
                        for &(word, v) in &program.stores {
                            self.real[i][word as usize] = values[v as usize];
                        }
                        for (&c, &v) in step.outs.iter().zip(&program.outputs) {
                            self.channels[c] = round_sat(values[v as usize]);
                        }
                    } else if let Some(value) = self.update(i, step, io) {
                        for &c in &step.outs {
                            self.channels[c] = value;
                        }
                    }
                    for &c in &step.outs {
                        let value = self.channels[c];
                        if plan.channels[c].trace && self.published[c] != Some(value) {
                            self.published[c] = Some(value);
                            io.publish(&plan.channels[c].name, value);
                        }
                    }
                }
            }
        }
        // The delays capture last, so every reader this tick saw the old value.
        for (i, step) in plan.steps.iter().enumerate() {
            if step.kind == Kind::Delay && tick.is_multiple_of(step.every) {
                self.state[i][0] = step.args.first().map_or(0, |&c| self.channels[c]);
            }
        }
        self.tick = tick.wrapping_add(1);
    }

    /// One update of a source or function: the value it writes to its
    /// channels, or `None` when they hold (a `mean` between dumps).
    fn update(&mut self, i: usize, step: &Step, io: &mut impl Io) -> Option<i32> {
        let channels = &self.channels;
        let arg = |k: usize| step.args.get(k).map_or(0, |&c| channels[c]);
        let args = || step.args.iter().map(|&c| channels[c]);
        let truth = |k: usize| i32::from(arg(k) != 0);
        let [s0, s1] = &mut self.state[i];
        let x = arg(0);
        Some(match step.kind {
            Kind::Input => {
                i32::from(io.gpio_read(step.config.pin.unwrap_or(0)) != step.config.invert)
            }
            Kind::Analog => io.adc_read(step.config.pin.unwrap_or(0)).max(0),
            Kind::Const => step.param("value"),
            Kind::And | Kind::Or | Kind::Xor | Kind::Not | Kind::Nand | Kind::Nor | Kind::Xnor => {
                i32::from(step.kind.apply(args().map(|v| v != 0)))
            }
            Kind::Add => args().fold(0, i32::wrapping_add),
            Kind::Sub => x.wrapping_sub(arg(1)),
            Kind::Mul => x.wrapping_mul(arg(1)),
            Kind::Min => args().min().unwrap_or(0),
            Kind::Max => args().max().unwrap_or(0),
            Kind::Abs => x.wrapping_abs(),
            Kind::Gt => i32::from(x > arg(1)),
            Kind::Lt => i32::from(x < arg(1)),
            Kind::Ge => i32::from(x >= arg(1)),
            Kind::Le => i32::from(x <= arg(1)),
            Kind::Eq => i32::from(x == arg(1)),
            Kind::Ne => i32::from(x != arg(1)),
            Kind::Select => {
                if x != 0 {
                    arg(1)
                } else {
                    arg(2)
                }
            }
            Kind::Scale => {
                let div = i64::from(step.param("div"));
                if div == 0 {
                    0
                } else {
                    (i64::from(x) * i64::from(step.param("mul")) / div) as i32
                }
            }
            Kind::Clamp => x.min(step.param("hi")).max(step.param("lo")),
            // s0: the input at the previous update.
            Kind::Delay => *s0,
            // s0: the accumulator, the output times 2^shift.
            Kind::Lowpass => {
                let shift = step.param("shift").clamp(1, 16);
                *s0 = s0.wrapping_add(x.wrapping_sub(*s0 >> shift));
                *s0 >> shift
            }
            // s0: the sum so far, s1: how many inputs are in it.
            Kind::Mean => {
                *s0 = s0.wrapping_add(x);
                *s1 += 1;
                let n = step.param("n").max(1);
                if *s1 < n {
                    return None;
                }
                let mean = *s0 / n;
                *s0 = 0;
                *s1 = 0;
                mean
            }
            // s0: the input at the previous update.
            Kind::Diff => {
                let y = x.wrapping_sub(*s0);
                *s0 = x;
                y
            }
            // s0: the output.
            Kind::Hyst => {
                if x >= step.param("hi") {
                    *s0 = 1;
                } else if x <= step.param("lo") {
                    *s0 = 0;
                }
                *s0
            }
            // s0: updates the input has disagreed with the output, s1: the output.
            Kind::Debounce => {
                let t = truth(0);
                if t == *s1 {
                    *s0 = 0;
                } else {
                    *s0 += 1;
                    if *s0 >= step.param("n") {
                        *s1 = t;
                        *s0 = 0;
                    }
                }
                *s1
            }
            // s0: whether the input was nonzero at the previous update.
            Kind::Rising => {
                let t = truth(0);
                let y = t & (*s0 ^ 1);
                *s0 = t;
                y
            }
            Kind::Falling => {
                let t = truth(0);
                let y = (t ^ 1) & *s0;
                *s0 = t;
                y
            }
            // s0: whether the clock was nonzero at the previous update, s1: the count.
            Kind::Count => {
                let t = truth(0);
                if arg(1) != 0 {
                    *s1 = 0;
                } else {
                    *s1 = s1.wrapping_add(t & (*s0 ^ 1));
                }
                *s0 = t;
                *s1
            }
            // s0: the output.
            Kind::Latch => {
                if arg(1) != 0 {
                    *s0 = 0;
                } else if x != 0 {
                    *s0 = 1;
                }
                *s0
            }
            // s0: consecutive updates the input has been nonzero, up to the wait.
            Kind::Ton => {
                let wait = step.ton_updates();
                if x == 0 {
                    *s0 = 0;
                } else if *s0 < wait {
                    *s0 += 1;
                }
                i32::from(*s0 >= wait)
            }
            Kind::Channel | Kind::Output | Kind::Pwm | Kind::Ukf | Kind::Cbf => 0,
        })
    }
}

/// How many of a plan's steps are pin sources and how many are sinks: the
/// `N in` and `K out` of the payload's log line.
pub fn pin_counts(plan: &Plan) -> (usize, usize) {
    let pins = plan.steps.iter().filter(|s| matches!(s.kind, Kind::Input | Kind::Analog)).count();
    let sinks = plan.steps.iter().filter(|s| s.kind.class() == Class::Sink).count();
    (pins, sinks)
}

/// The frequency a `pwm` step opens its timer at.
pub fn pwm_freq(step: &Step) -> u32 {
    step.config.freq_hz.unwrap_or(DEFAULT_PWM_HZ)
}
