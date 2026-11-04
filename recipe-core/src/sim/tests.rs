//! The worked example against simulated units (to Complete, held mid-run,
//! a unit lost and found, a unit lost and the batch aborted), and the
//! termination property over random event interleavings.

use super::*;
use crate::engine::{Config, Failure, Refusal};
use crate::fixture::{LED, SENSE, allocation, sfc};
use std::vec::Vec;

const PHASES: [usize; 2] = [1, 1];
const BLINK: PhaseRef = PhaseRef {
    unit: LED,
    phase: 0,
};

fn units() -> SimUnits {
    let mut u = SimUnits::new();
    u.phases[usize::from(LED)][0] = SimPhase::idle(300);
    u.phases[usize::from(SENSE)][0] = SimPhase::idle(100);
    u
}

fn at(at_ms: u64, event: Event) -> Scripted {
    Scripted { at_ms, event }
}

fn go(cfg: Config, script: &[Scripted], until: u64) -> (Outcome, Vec<(u64, Effect)>) {
    let e = Engine::new(sfc(), &allocation(), 2, cfg).unwrap();
    let mut trace = Vec::new();
    let out = run(e, units(), &PHASES, script, until, 10, &mut |t, fx| {
        trace.push((t, *fx))
    });
    assert_eq!((out.overflow, out.dropped), (0, 0));
    (out, trace)
}

fn recipe_states(trace: &[(u64, Effect)]) -> Vec<PhaseState> {
    trace
        .iter()
        .filter_map(|(_, e)| match e {
            Effect::Recipe { to, .. } => Some(*to),
            _ => None,
        })
        .collect()
}

#[test]
fn the_worked_example_runs_to_complete() {
    let (out, trace) = go(
        Config::DEFAULT,
        &[at(0, Event::Operator(Command::Start))],
        1_000,
    );
    assert_eq!(out.engine.state(), PhaseState::Complete);
    assert_eq!(
        recipe_states(&trace),
        [PhaseState::Running, PhaseState::Complete]
    );
    let starts: Vec<_> = trace
        .iter()
        .filter_map(|(t, e)| match e {
            Effect::Send {
                phase,
                command: Command::Start,
                ..
            } => Some((*t, phase.unit)),
            _ => None,
        })
        .collect();
    // Blink first; Measure once Blink's 300 ms run completed.
    assert_eq!(starts, [(0, LED), (300, SENSE)]);
    assert_eq!(
        out.units.phases[usize::from(LED)][0].state,
        PhaseState::Idle
    );
}

#[test]
fn a_hold_mid_run_holds_blink_and_a_restart_finishes_the_batch() {
    let script = [
        at(0, Event::Operator(Command::Start)),
        at(100, Event::Operator(Command::Hold)),
        at(400, Event::Operator(Command::Restart)),
    ];
    let (out, trace) = go(Config::DEFAULT, &script, 2_000);
    assert_eq!(
        recipe_states(&trace),
        [
            PhaseState::Running,
            PhaseState::Holding,
            PhaseState::Held,
            PhaseState::Restarting,
            PhaseState::Running,
            PhaseState::Complete
        ]
    );
    // Held 20 ms after the hold (the simulated settle time), and Measure
    // never started while held.
    let held_at = trace
        .iter()
        .find(|(_, e)| {
            matches!(
                e,
                Effect::Recipe {
                    to: PhaseState::Held,
                    ..
                }
            )
        })
        .unwrap()
        .0;
    assert_eq!(held_at, 120);
    let measure_start = trace
        .iter()
        .find(|(_, e)| matches!(e, Effect::Send { phase, command: Command::Start, .. } if phase.unit == SENSE))
        .unwrap()
        .0;
    assert!(measure_start > 400);
    assert_eq!(out.engine.state(), PhaseState::Complete);
}

#[test]
fn losing_the_led_unit_holds_the_batch_and_abort_ends_it_aborted() {
    let script = [
        at(0, Event::Operator(Command::Start)),
        at(100, Event::Lose(LED)),
        at(200, Event::Operator(Command::Restart)),
        at(300, Event::Operator(Command::Abort)),
    ];
    let (out, trace) = go(Config::DEFAULT, &script, 1_000);
    assert!(trace.contains(&(
        100,
        Effect::Failed {
            phase: None,
            failure: Failure::UnitLost(LED)
        }
    )));
    assert!(trace.contains(&(
        200,
        Effect::Refused {
            command: Command::Restart,
            reason: Refusal::UnitOffline(LED)
        }
    )));
    assert_eq!(
        recipe_states(&trace),
        [
            PhaseState::Running,
            PhaseState::Holding,
            PhaseState::Held,
            PhaseState::Aborting,
            PhaseState::Aborted
        ]
    );
    // Nothing was ever sent to the lost unit after it went.
    assert!(!trace.iter().any(|(t, e)| *t >= 100 && matches!(e, Effect::Send { phase, .. } if phase.unit == LED)));
    assert_eq!(out.engine.state(), PhaseState::Aborted);
}

#[test]
fn a_lost_unit_that_comes_back_is_held_and_the_batch_restarts_to_complete() {
    let script = [
        at(0, Event::Operator(Command::Start)),
        at(100, Event::Lose(LED)),
        at(200, Event::Find(LED)),
        at(300, Event::Operator(Command::Restart)),
    ];
    let (out, trace) = go(Config::DEFAULT, &script, 3_000);
    // On its heartbeat the engine holds the still-running Blink.
    assert!(trace.contains(&(
        200,
        Effect::Send {
            step: 0,
            phase: BLINK,
            command: Command::Hold,
            id: 2,
            timeout_ms: 1_000
        }
    )));
    assert_eq!(out.engine.state(), PhaseState::Complete);
}

#[test]
fn a_phase_stuck_holding_times_out_and_the_batch_is_held_with_it_unknown() {
    let cfg = Config {
        settle_timeout_ms: 200,
        ..Config::DEFAULT
    };
    let script = [
        at(0, Event::Operator(Command::Start)),
        at(50, Event::Stick(BLINK)),
        at(100, Event::Operator(Command::Hold)),
    ];
    let (out, trace) = go(cfg, &script, 1_000);
    assert!(trace.contains(&(
        300,
        Effect::Failed {
            phase: Some(BLINK),
            failure: Failure::SettleTimeout
        }
    )));
    assert_eq!(out.engine.state(), PhaseState::Held);
    assert_eq!(out.engine.phase_state(BLINK), None);
}

/// xorshift64*, for reproducible random scripts without a dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn random_event(rng: &mut Rng) -> Event {
    let unit = rng.below(2) as u8;
    match rng.below(10) {
        0..=3 => Event::Operator(Command::ALL[rng.below(6) as usize]),
        4 | 5 => Event::Lose(unit),
        6 | 7 => Event::Find(unit),
        _ => Event::Stick(PhaseRef { unit, phase: 0 }),
    }
}

#[test]
fn every_run_ends_complete_stopped_or_aborted_after_abort_and_two_timeouts() {
    // Any interleaving of operator commands, unit losses and returns, and
    // stuck phases in the first 600 ms; then Abort at 700. Within the ack
    // and settle timeouts (plus a tick each) the batch is in a final state
    // (or Idle, if it never started or was reset), no input overflowed its
    // effects, and no command went to a unit the engine had been told was
    // offline. (A unit may drop before the directory says so; commands
    // sent then are lost and time out, which the property covers.)
    let cfg = Config {
        ack_timeout_ms: 100,
        settle_timeout_ms: 200,
        max_commands: 16,
    };
    let horizon = 700 + 100 + 200 + 30;
    let mut ended = [0u32; crate::phase::STATES];
    let mut held_runs = 0u32;
    for seed in 1..=3_000u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let n = 1 + rng.below(12) as usize;
        let mut script: Vec<Scripted> = (0..n)
            .map(|_| at(rng.below(600), random_event(&mut rng)))
            .collect();
        script.insert(0, at(rng.below(50), Event::Operator(Command::Start)));
        script.push(at(700, Event::Operator(Command::Abort)));
        let e = Engine::new(sfc(), &allocation(), 2, cfg).unwrap();
        let mut offline_sends = 0;
        // The engine's view: offline from its UnitLost effect until a
        // Find of that unit takes effect (at the tick its time falls in).
        let mut lost_at: [Option<u64>; 2] = [None, None];
        let found_by = |u: u8, from: u64, to: u64| {
            script.iter().any(|s| {
                let tick = s.at_ms.div_ceil(10) * 10;
                matches!(s.event, Event::Find(f) if f == u) && tick >= from && tick <= to
            })
        };
        let mut sink = |t: u64, fx: &Effect| match fx {
            Effect::Failed {
                failure: Failure::UnitLost(u),
                ..
            } => lost_at[usize::from(*u)] = Some(t),
            Effect::Send { phase, .. } => {
                if let Some(since) = lost_at[usize::from(phase.unit)] {
                    if found_by(phase.unit, since, t) {
                        lost_at[usize::from(phase.unit)] = None;
                    } else {
                        offline_sends += 1;
                    }
                }
            }
            _ => {}
        };
        let mut was_held = false;
        let mut both = |t: u64, fx: &Effect| {
            was_held |= matches!(
                fx,
                Effect::Recipe {
                    to: PhaseState::Held,
                    ..
                }
            );
            sink(t, fx);
        };
        let out = run(e, units(), &PHASES, &script, horizon, 10, &mut both);
        let s = out.engine.state();
        ended[usize::from(s.code())] += 1;
        held_runs += u32::from(was_held);
        assert!(
            matches!(
                s,
                PhaseState::Complete | PhaseState::Stopped | PhaseState::Aborted | PhaseState::Idle
            ),
            "seed {seed}: ended {} with script {script:?}",
            s.name()
        );
        assert_eq!(
            (out.overflow, out.dropped, offline_sends),
            (0, 0, 0),
            "seed {seed}: {script:?}"
        );
        assert!(out.inputs < 2_000, "seed {seed}: {} inputs", out.inputs);
    }
    // The scripts exercise every ending and the hold path.
    for st in [
        PhaseState::Complete,
        PhaseState::Stopped,
        PhaseState::Aborted,
        PhaseState::Idle,
    ] {
        assert!(
            ended[usize::from(st.code())] > 30,
            "{}: {ended:?}",
            st.name()
        );
    }
    assert!(held_runs > 300, "{held_runs}");
}

#[test]
fn without_failures_every_ordering_of_unit_speeds_completes() {
    for led_ms in [10, 50, 300, 1_000] {
        for sense_ms in [10, 100, 700] {
            let mut u = units();
            u.phases[usize::from(LED)][0].run_ms = led_ms;
            u.phases[usize::from(SENSE)][0].run_ms = sense_ms;
            let e = Engine::new(sfc(), &allocation(), 2, Config::DEFAULT).unwrap();
            let script = [at(0, Event::Operator(Command::Start))];
            let out = run(e, u, &PHASES, &script, 3_000, 10, &mut |_, _| {});
            assert_eq!(
                out.engine.state(),
                PhaseState::Complete,
                "{led_ms} {sense_ms}"
            );
        }
    }
}
