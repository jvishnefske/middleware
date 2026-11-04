//! Simulated units and a bounded driver that runs an [`Engine`] against
//! them: the same loop the browser and the tests use before real units
//! exist.
//!
//! A [`SimUnits`] holds every unit's equipment phases as [`phase`] state
//! machines. A command ([`Effect::Send`]) to an online unit is applied by
//! the phase table and answered at once ([`Ack::Accepted`] or
//! [`Ack::Rejected`] with the current state); to an offline unit it is
//! lost (the engine's acknowledgement deadline then fires). An acting
//! state ends by itself — Running after the phase's run time, every other
//! acting state after the settle time — and the unit then publishes a
//! state report ([`Input::Report`]). A phase can be made to stick (never
//! finish its acting state) to exercise settle timeouts.
//!
//! [`run`] advances time from 0 in fixed ticks up to a horizon, applying a
//! [`Scripted`] list of operator commands and unit losses at their times,
//! and hands every effect to a sink. It is bounded: at most
//! [`MAX_SIM_TICKS`] ticks, and per tick at most [`MAX_QUEUE`] queued
//! inputs, each producing at most [`crate::engine::MAX_EFFECTS`] effects.

use crate::alloc::PhaseRef;
use crate::engine::{Ack, Effect, Effects, Engine, Input};
use crate::phase::{self, Command, PhaseState};
use crate::{MAX_UNIT_PHASES, MAX_UNITS};

/// Most ticks [`run`] simulates.
pub const MAX_SIM_TICKS: u64 = 100_000;
/// Most inputs waiting for the engine within one tick.
pub const MAX_QUEUE: usize = 64;

/// One simulated equipment phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimPhase {
    /// Its state.
    pub state: PhaseState,
    /// When its acting state ends (ignored when quiescent).
    pub busy_until: u64,
    /// The id of the last command it applied.
    pub last_id: u32,
    /// How long Running lasts.
    pub run_ms: u32,
    /// Never ends an acting state (to exercise timeouts).
    pub stuck: bool,
}

impl SimPhase {
    /// An Idle phase that runs for `run_ms`.
    pub const fn idle(run_ms: u32) -> SimPhase {
        SimPhase {
            state: PhaseState::Idle,
            busy_until: 0,
            last_id: 0,
            run_ms,
            stuck: false,
        }
    }
}

/// Simulated units: `phases[unit][phase]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimUnits {
    /// Each unit's phases.
    pub phases: [[SimPhase; MAX_UNIT_PHASES]; MAX_UNITS],
    /// Bit per unit: answering.
    pub online: u32,
    /// How long every acting state other than Running lasts.
    pub settle_ms: u32,
}

impl SimUnits {
    /// Every unit online, every phase Idle with a 100 ms run, 20 ms
    /// settling.
    pub const fn new() -> SimUnits {
        SimUnits {
            phases: [[SimPhase::idle(100); MAX_UNIT_PHASES]; MAX_UNITS],
            online: u32::MAX,
            settle_ms: 20,
        }
    }

    fn get(&mut self, p: PhaseRef) -> Option<&mut SimPhase> {
        self.phases
            .get_mut(usize::from(p.unit))?
            .get_mut(usize::from(p.phase))
    }

    fn duration(&self, sp: &SimPhase, state: PhaseState) -> u64 {
        if state == PhaseState::Running {
            u64::from(sp.run_ms)
        } else {
            u64::from(self.settle_ms)
        }
    }

    /// The reply to a command, or `None` when the unit is offline (or the
    /// phase does not exist: the engine sees no reply either way).
    pub fn command(&mut self, now: u64, p: PhaseRef, command: Command, id: u32) -> Option<Ack> {
        if self.online & (1u32 << p.unit) == 0 {
            return None;
        }
        let settle = self.settle_ms;
        let sp = *self.get(p)?;
        let (to, effect) = phase::step(sp.state, phase::Input::Command(command));
        if effect == phase::Effect::Rejected {
            return Some(Ack::Rejected(sp.state));
        }
        let busy = if to == PhaseState::Running {
            u64::from(sp.run_ms)
        } else {
            u64::from(settle)
        };
        let slot = self.get(p)?;
        slot.state = to;
        slot.last_id = id;
        slot.busy_until = now + busy;
        Some(Ack::Accepted(to))
    }

    /// Ends every acting state whose time is up at `now`, pushing a report
    /// for each change into `out` (at most one change per phase per call).
    pub fn tick(&mut self, now: u64, out: &mut Queue) {
        for u in 0..MAX_UNITS {
            if self.online & (1u32 << u) == 0 {
                continue;
            }
            for ph in 0..MAX_UNIT_PHASES {
                let sp = self.phases[u][ph];
                if !sp.state.is_acting() || sp.stuck || sp.busy_until > now {
                    continue;
                }
                let (to, _) = phase::step(sp.state, phase::Input::Done);
                let busy = self.duration(&sp, to);
                let slot = &mut self.phases[u][ph];
                slot.state = to;
                slot.busy_until = now + busy;
                out.push(Input::Report {
                    phase: PhaseRef {
                        unit: u as u8,
                        phase: ph as u8,
                    },
                    state: to,
                    last_id: slot.last_id,
                });
            }
        }
    }

    /// Pushes a report of every phase of `unit` (what a unit does when it
    /// comes back: a full heartbeat).
    pub fn announce(&self, unit: u8, phases: usize, out: &mut Queue) {
        let Some(list) = self.phases.get(usize::from(unit)) else {
            return;
        };
        for (ph, sp) in list.iter().take(phases).enumerate() {
            out.push(Input::Report {
                phase: PhaseRef {
                    unit,
                    phase: ph as u8,
                },
                state: sp.state,
                last_id: sp.last_id,
            });
        }
    }
}

impl Default for SimUnits {
    fn default() -> Self {
        Self::new()
    }
}

/// A bounded FIFO of inputs for the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Queue {
    items: [Option<Input>; MAX_QUEUE],
    head: usize,
    len: usize,
    /// Inputs that did not fit.
    pub dropped: u16,
}

impl Queue {
    /// Empty.
    pub const fn new() -> Queue {
        Queue {
            items: [None; MAX_QUEUE],
            head: 0,
            len: 0,
            dropped: 0,
        }
    }

    /// Adds at the back (or counts it as dropped when full).
    pub fn push(&mut self, input: Input) {
        if self.len == MAX_QUEUE {
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        self.items[(self.head + self.len) % MAX_QUEUE] = Some(input);
        self.len += 1;
    }

    /// Takes from the front.
    pub fn pop(&mut self) -> Option<Input> {
        if self.len == 0 {
            return None;
        }
        let item = self.items[self.head].take();
        self.head = (self.head + 1) % MAX_QUEUE;
        self.len -= 1;
        item
    }
}

impl Default for Queue {
    fn default() -> Self {
        Self::new()
    }
}

/// Something that happens at a set time in a [`run`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// The operator commands the procedure.
    Operator(Command),
    /// The unit drops off the network (and out of the directory).
    Lose(u8),
    /// The unit comes back and reports every phase.
    Find(u8),
    /// The phase stops finishing its acting states.
    Stick(PhaseRef),
}

/// An [`Event`] at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scripted {
    /// When, in ms from the start.
    pub at_ms: u64,
    /// What.
    pub event: Event,
}

/// What a [`run`] ended with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    /// The engine at the horizon.
    pub engine: Engine,
    /// The units at the horizon.
    pub units: SimUnits,
    /// Engine inputs processed.
    pub inputs: u32,
    /// Effects that overflowed an input's [`Effects`], summed.
    pub overflow: u32,
    /// Inputs dropped from a full [`Queue`], summed.
    pub dropped: u32,
}

/// Runs `engine` against `units` from 0 to `until_ms` in ticks of
/// `tick_ms` (at least 1; at most [`MAX_SIM_TICKS`] ticks), applying
/// `script` (in any order; events at the same time apply in list order),
/// with `phases[u]` the number of phases unit `u` has (for its heartbeat
/// on [`Event::Find`]). Every effect goes to `sink` with its time.
pub fn run(
    engine: Engine,
    units: SimUnits,
    phases: &[usize],
    script: &[Scripted],
    until_ms: u64,
    tick_ms: u64,
    sink: &mut impl FnMut(u64, &Effect),
) -> Outcome {
    let tick = tick_ms.max(1);
    let ticks = (until_ms / tick).min(MAX_SIM_TICKS);
    let mut out = Outcome {
        engine,
        units,
        inputs: 0,
        overflow: 0,
        dropped: 0,
    };
    for n in 0..=ticks {
        let now = n * tick;
        let since = now.saturating_sub(tick);
        let mut queue = Queue::new();
        for s in script {
            let due = if n == 0 {
                s.at_ms == 0
            } else {
                s.at_ms > since && s.at_ms <= now
            };
            if !due {
                continue;
            }
            match s.event {
                Event::Operator(c) => queue.push(Input::Operator(c)),
                Event::Lose(u) => {
                    out.units.online &= !(1u32 << u);
                    queue.push(Input::UnitLost(u));
                }
                Event::Find(u) => {
                    out.units.online |= 1u32 << u;
                    queue.push(Input::UnitFound(u));
                    let count = phases.get(usize::from(u)).copied().unwrap_or(0);
                    out.units.announce(u, count, &mut queue);
                }
                Event::Stick(p) => {
                    if let Some(sp) = out.units.get(p) {
                        sp.stuck = true;
                    }
                }
            }
        }
        out.units.tick(now, &mut queue);
        queue.push(Input::Tick);
        for _ in 0..MAX_QUEUE * 4 {
            let Some(input) = queue.pop() else { break };
            let (e, fx) = out.engine.on(now, input);
            out.engine = e;
            out.inputs += 1;
            out.overflow += u32::from(fx.overflow);
            deliver(&fx, now, &mut out.units, &mut queue, sink);
        }
        out.dropped += u32::from(queue.dropped);
    }
    out
}

fn deliver(
    fx: &Effects,
    now: u64,
    units: &mut SimUnits,
    queue: &mut Queue,
    sink: &mut impl FnMut(u64, &Effect),
) {
    for e in fx.iter() {
        sink(now, e);
        if let Effect::Send {
            phase, command, id, ..
        } = *e
            && let Some(result) = units.command(now, phase, command, id)
        {
            queue.push(Input::Ack { id, result });
        }
    }
}

#[cfg(test)]
mod tests;
