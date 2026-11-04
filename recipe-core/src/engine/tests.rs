//! Tests of the engine driven input by input: the worked example's happy
//! path with every command it sends, acknowledgement by report, stale
//! reports, and each failure's policy.

use super::*;
use crate::fixture::{LED, SENSE, engine};
use std::vec::Vec;

const BLINK: PhaseRef = PhaseRef {
    unit: LED,
    phase: 0,
};
const MEASURE: PhaseRef = PhaseRef {
    unit: SENSE,
    phase: 0,
};

fn list(fx: &Effects) -> Vec<Effect> {
    assert_eq!(fx.overflow, 0);
    fx.iter().copied().collect()
}

fn sends(fx: &Effects) -> Vec<(PhaseRef, Command, u32)> {
    fx.iter()
        .filter_map(|e| match *e {
            Effect::Send {
                phase, command, id, ..
            } => Some((phase, command, id)),
            _ => None,
        })
        .collect()
}

fn ack(id: u32, s: PhaseState) -> Input {
    Input::Ack {
        id,
        result: Ack::Accepted(s),
    }
}

fn report(phase: PhaseRef, state: PhaseState, last_id: u32) -> Input {
    Input::Report {
        phase,
        state,
        last_id,
    }
}

/// Started, Blink acknowledged Running.
fn running() -> Engine {
    let (e, _) = engine().on(0, Input::Operator(Command::Start));
    let (e, _) = e.on(1, ack(1, PhaseState::Running));
    e
}

#[test]
fn the_worked_example_runs_blink_then_measure_and_releases_both_phases() {
    let (e, fx) = engine().on(0, Input::Operator(Command::Start));
    assert_eq!(
        list(&fx),
        [
            Effect::Recipe {
                from: PhaseState::Idle,
                to: PhaseState::Running
            },
            Effect::StepActive(0),
            Effect::Send {
                step: 0,
                phase: BLINK,
                command: Command::Start,
                id: 1,
                timeout_ms: 1_000
            },
        ]
    );
    let (e, fx) = e.on(5, ack(1, PhaseState::Running));
    assert!(fx.is_empty());
    assert_eq!(e.step(0), StepStatus::Active { started: true });

    let (e, fx) = e.on(800, report(BLINK, PhaseState::Complete, 1));
    assert_eq!(
        list(&fx),
        [
            Effect::StepDone(0),
            Effect::StepActive(1),
            Effect::Send {
                step: 0,
                phase: BLINK,
                command: Command::Reset,
                id: 2,
                timeout_ms: 1_000
            },
            Effect::Send {
                step: 1,
                phase: MEASURE,
                command: Command::Start,
                id: 3,
                timeout_ms: 1_000
            },
        ]
    );
    let (e, _) = e.on(801, ack(2, PhaseState::Resetting));
    let (e, _) = e.on(802, ack(3, PhaseState::Running));
    let (e, _) = e.on(820, report(BLINK, PhaseState::Idle, 2));
    let (e, fx) = e.on(900, report(MEASURE, PhaseState::Complete, 3));
    assert_eq!(
        list(&fx),
        [
            Effect::StepDone(1),
            Effect::Send {
                step: 1,
                phase: MEASURE,
                command: Command::Reset,
                id: 4,
                timeout_ms: 1_000
            },
        ]
    );
    let (e, _) = e.on(901, ack(4, PhaseState::Resetting));
    assert_eq!(e.state(), PhaseState::Running);
    let (e, fx) = e.on(920, report(MEASURE, PhaseState::Idle, 4));
    assert_eq!(
        list(&fx),
        [Effect::Recipe {
            from: PhaseState::Running,
            to: PhaseState::Complete
        }]
    );
    assert_eq!(e.state(), PhaseState::Complete);
    assert!(!e.awaiting());
}

#[test]
fn a_report_naming_the_command_acknowledges_it() {
    let (e, _) = engine().on(0, Input::Operator(Command::Start));
    // A unit that only publishes: its report carries id 1.
    let (e, fx) = e.on(5, report(BLINK, PhaseState::Running, 1));
    assert!(fx.is_empty());
    assert_eq!(e.step(0), StepStatus::Active { started: true });
    assert!(!e.awaiting());
}

#[test]
fn reports_older_than_the_last_applied_command_are_ignored() {
    let e = running();
    let (e, fx) = e.on(10, report(BLINK, PhaseState::Idle, 0));
    assert_eq!(list(&fx), [Effect::Ignored(Ignored::StaleReport)]);
    assert_eq!(e.phase_state(BLINK), Some(PhaseState::Running));
    let (_, fx) = e.on(
        11,
        report(PhaseRef { unit: 7, phase: 0 }, PhaseState::Idle, 9),
    );
    assert_eq!(list(&fx), [Effect::Ignored(Ignored::UnknownPhase)]);
}

#[test]
fn operator_commands_follow_the_phase_table() {
    let (e, fx) = engine().on(0, Input::Operator(Command::Hold));
    assert_eq!(
        list(&fx),
        [Effect::Refused {
            command: Command::Hold,
            reason: Refusal::InvalidInState(PhaseState::Idle)
        }]
    );
    let (_, fx) = running().on(2, Input::Operator(Command::Restart));
    assert_eq!(
        list(&fx),
        [Effect::Refused {
            command: Command::Restart,
            reason: Refusal::InvalidInState(PhaseState::Running)
        }]
    );
    let _ = e;
}

#[test]
fn hold_holds_the_running_phase_and_restart_resumes_it() {
    let (e, fx) = running().on(10, Input::Operator(Command::Hold));
    assert_eq!(sends(&fx), [(BLINK, Command::Hold, 2)]);
    assert_eq!(e.state(), PhaseState::Holding);
    let (e, _) = e.on(11, ack(2, PhaseState::Holding));
    assert_eq!(e.state(), PhaseState::Holding);
    let (e, fx) = e.on(30, report(BLINK, PhaseState::Held, 2));
    assert_eq!(
        list(&fx),
        [Effect::Recipe {
            from: PhaseState::Holding,
            to: PhaseState::Held
        }]
    );
    let (e, fx) = e.on(40, Input::Operator(Command::Restart));
    assert_eq!(sends(&fx), [(BLINK, Command::Restart, 3)]);
    let (e, _) = e.on(41, ack(3, PhaseState::Restarting));
    let (e, fx) = e.on(60, report(BLINK, PhaseState::Running, 3));
    assert_eq!(
        list(&fx),
        [Effect::Recipe {
            from: PhaseState::Restarting,
            to: PhaseState::Running
        }]
    );
    assert_eq!(e.step(0), StepStatus::Active { started: true });
}

#[test]
fn stop_and_abort_end_the_run_in_stopped_and_aborted() {
    let (e, fx) = running().on(10, Input::Operator(Command::Stop));
    assert_eq!(sends(&fx), [(BLINK, Command::Stop, 2)]);
    let (e, _) = e.on(11, ack(2, PhaseState::Stopping));
    let (e, _) = e.on(20, report(BLINK, PhaseState::Stopped, 2));
    assert_eq!(e.state(), PhaseState::Stopped);
    // Reset releases the stopped phase and returns to Idle.
    let (e, fx) = e.on(30, Input::Operator(Command::Reset));
    assert_eq!(sends(&fx), [(BLINK, Command::Reset, 3)]);
    let (e, _) = e.on(31, ack(3, PhaseState::Resetting));
    let (e, _) = e.on(40, report(BLINK, PhaseState::Idle, 3));
    assert_eq!(e.state(), PhaseState::Idle);
    assert_eq!(e.step(0), StepStatus::Inactive);

    let (e, fx) = running().on(10, Input::Operator(Command::Abort));
    assert_eq!(sends(&fx), [(BLINK, Command::Abort, 2)]);
    let (e, _) = e.on(11, ack(2, PhaseState::Aborting));
    let (e, _) = e.on(20, report(BLINK, PhaseState::Aborted, 2));
    assert_eq!(e.state(), PhaseState::Aborted);
}

#[test]
fn a_missing_acknowledgement_holds_the_recipe_and_makes_the_phase_unknown() {
    let (e, _) = engine().on(0, Input::Operator(Command::Start));
    assert_eq!(e.next_deadline(), Some(1_000));
    let (e, fx) = e.on(1_000, Input::Tick);
    let l = list(&fx);
    assert_eq!(
        l[0],
        Effect::Failed {
            phase: Some(BLINK),
            failure: Failure::AckTimeout
        }
    );
    assert_eq!(
        l[1],
        Effect::Recipe {
            from: PhaseState::Running,
            to: PhaseState::Holding
        }
    );
    // Nothing involved is reachable, so Holding finishes at once.
    assert_eq!(e.state(), PhaseState::Held);
    assert_eq!(e.phase_state(BLINK), None);
}

#[test]
fn not_found_and_invalid_request_abort_while_unavailable_holds() {
    let (e, _) = engine().on(0, Input::Operator(Command::Start));
    let (e2, _) = e.on(
        1,
        Input::Ack {
            id: 1,
            result: Ack::Failed(AckError::NotFound),
        },
    );
    assert_eq!(e2.state(), PhaseState::Aborted);
    let (e3, _) = e.on(
        1,
        Input::Ack {
            id: 1,
            result: Ack::Failed(AckError::Unavailable),
        },
    );
    assert_eq!(e3.state(), PhaseState::Held);
    let (_, fx) = e.on(
        1,
        Input::Ack {
            id: 99,
            result: Ack::Accepted(PhaseState::Running),
        },
    );
    assert_eq!(list(&fx), [Effect::Ignored(Ignored::UnknownAck)]);
}

#[test]
fn a_phase_that_holds_itself_holds_the_recipe_and_one_that_aborts_aborts_it() {
    let (e, fx) = running().on(10, report(BLINK, PhaseState::Held, 1));
    assert_eq!(
        fx.iter().next(),
        Some(&Effect::Failed {
            phase: Some(BLINK),
            failure: Failure::SelfHeld
        })
    );
    assert_eq!(e.state(), PhaseState::Held);
    let (e, _) = running().on(10, report(BLINK, PhaseState::Aborted, 1));
    assert_eq!(e.state(), PhaseState::Aborted);
}

#[test]
fn a_rejected_command_updates_the_known_state_and_is_retried() {
    let (e, _) = engine().on(0, Input::Operator(Command::Start));
    // The phase was still resetting from an earlier run.
    let (e, fx) = e.on(
        1,
        Input::Ack {
            id: 1,
            result: Ack::Rejected(PhaseState::Resetting),
        },
    );
    assert_eq!(
        list(&fx),
        [Effect::Failed {
            phase: Some(BLINK),
            failure: Failure::CommandRejected(PhaseState::Resetting)
        }]
    );
    assert_eq!(e.state(), PhaseState::Running);
    let (_, fx) = e.on(10, report(BLINK, PhaseState::Idle, 1));
    assert_eq!(sends(&fx), [(BLINK, Command::Start, 2)]);
}

#[test]
fn a_phase_that_keeps_rejecting_exhausts_its_budget_and_aborts() {
    let (mut e, _) = engine().on(0, Input::Operator(Command::Start));
    for id in 1..=u32::from(Config::DEFAULT.max_commands) {
        let (n, _) = e.on(
            u64::from(id),
            Input::Ack {
                id,
                result: Ack::Rejected(PhaseState::Idle),
            },
        );
        e = n;
    }
    assert_eq!(e.state(), PhaseState::Aborted);
}

#[test]
fn losing_a_unit_with_unfinished_steps_holds_and_restart_waits_for_it() {
    let (e, fx) = running().on(10, Input::UnitLost(LED));
    let l = list(&fx);
    assert_eq!(
        l[0],
        Effect::Failed {
            phase: None,
            failure: Failure::UnitLost(LED)
        }
    );
    assert_eq!(e.state(), PhaseState::Held);
    let (e, fx) = e.on(20, Input::Operator(Command::Restart));
    assert_eq!(
        list(&fx),
        [Effect::Refused {
            command: Command::Restart,
            reason: Refusal::UnitOffline(LED)
        }]
    );
    let (e, _) = e.on(30, Input::UnitFound(LED));
    let (e, fx) = e.on(31, Input::Operator(Command::Restart));
    assert_eq!(
        list(&fx),
        [Effect::Refused {
            command: Command::Restart,
            reason: Refusal::PhaseUnknown(BLINK)
        }]
    );
    // Its heartbeat: still running (it never saw the hold) -> held now.
    let (e, fx) = e.on(32, report(BLINK, PhaseState::Running, 1));
    assert_eq!(sends(&fx), [(BLINK, Command::Hold, 2)]);
    let (e, _) = e.on(33, ack(2, PhaseState::Holding));
    let (e, _) = e.on(40, report(BLINK, PhaseState::Held, 2));
    let (e, fx) = e.on(41, Input::Operator(Command::Restart));
    assert_eq!(sends(&fx), [(BLINK, Command::Restart, 3)]);
    assert_eq!(e.state(), PhaseState::Restarting);
}

#[test]
fn losing_a_unit_whose_steps_are_done_is_only_reported() {
    let (e, _) = running().on(10, report(BLINK, PhaseState::Complete, 1));
    let (e, _) = e.on(11, ack(2, PhaseState::Resetting));
    let (e, _) = e.on(12, ack(3, PhaseState::Running));
    let (e, fx) = e.on(13, Input::UnitLost(LED));
    assert_eq!(
        list(&fx),
        [Effect::Failed {
            phase: None,
            failure: Failure::UnitLost(LED)
        }]
    );
    assert_eq!(e.state(), PhaseState::Running);
    // Its phase is unreachable, so it no longer blocks completion.
    let (e, _) = e.on(20, report(MEASURE, PhaseState::Complete, 3));
    let (e, _) = e.on(21, ack(4, PhaseState::Resetting));
    let (e, _) = e.on(30, report(MEASURE, PhaseState::Idle, 4));
    assert_eq!(e.state(), PhaseState::Complete);
}

#[test]
fn a_transient_state_that_never_settles_times_out() {
    let (e, _) = running().on(10, Input::Operator(Command::Hold));
    let (e, _) = e.on(11, ack(2, PhaseState::Holding));
    assert_eq!(e.next_deadline(), Some(11 + 5_000));
    let (e, fx) = e.on(5_011, Input::Tick);
    assert_eq!(
        fx.iter().next(),
        Some(&Effect::Failed {
            phase: Some(BLINK),
            failure: Failure::SettleTimeout
        })
    );
    assert_eq!(e.state(), PhaseState::Held);
}
