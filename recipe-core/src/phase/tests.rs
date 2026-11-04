//! Tests of the phase state machine: the whole command matrix cell by cell
//! against the table in the module docs, and the properties it implies.

use super::*;

/// The module docs' table, written out independently: one row per state
/// in code order, columns Start, Hold, Restart, Stop, Abort, Reset, Done
/// (`-` = rejected).
const MATRIX: [[&str; 7]; STATES] = [
    ["Running", "-", "-", "-", "-", "-", "-"],
    ["-", "Holding", "-", "Stopping", "Aborting", "-", "Complete"],
    ["-", "-", "-", "-", "-", "Resetting", "-"],
    ["-", "-", "-", "Stopping", "Aborting", "-", "Held"],
    ["-", "-", "Restarting", "Stopping", "Aborting", "-", "-"],
    ["-", "Holding", "-", "Stopping", "Aborting", "-", "Running"],
    ["-", "-", "-", "-", "Aborting", "-", "Stopped"],
    ["-", "-", "-", "-", "-", "Resetting", "-"],
    ["-", "-", "-", "-", "-", "-", "Aborted"],
    ["-", "-", "-", "-", "-", "Resetting", "-"],
    ["-", "-", "-", "Stopping", "Aborting", "-", "Idle"],
];

fn inputs() -> [Input; 7] {
    [
        Input::Command(Command::Start),
        Input::Command(Command::Hold),
        Input::Command(Command::Restart),
        Input::Command(Command::Stop),
        Input::Command(Command::Abort),
        Input::Command(Command::Reset),
        Input::Done,
    ]
}

#[test]
fn every_cell_of_the_command_matrix_matches_the_table() {
    for (si, s) in PhaseState::ALL.iter().enumerate() {
        for (ii, input) in inputs().iter().enumerate() {
            let (to, effect) = step(*s, *input);
            match MATRIX[si][ii] {
                "-" => {
                    assert_eq!(effect, Effect::Rejected, "{} {:?}", s.name(), input);
                    assert_eq!(to, *s, "a rejected input keeps the state");
                }
                name => {
                    assert_eq!(effect, Effect::Enter(to), "{} {:?}", s.name(), input);
                    assert_eq!(to.name(), name, "{} {:?}", s.name(), input);
                }
            }
        }
    }
}

#[test]
fn codes_round_trip_and_unknown_codes_are_none() {
    for s in PhaseState::ALL {
        assert_eq!(PhaseState::from_code(s.code()), Some(s));
    }
    for c in Command::ALL {
        assert_eq!(Command::from_code(c.code()), Some(c));
    }
    assert_eq!(PhaseState::from_code(11), None);
    assert_eq!(Command::from_code(0), None);
    assert_eq!(Command::from_code(7), None);
}

#[test]
fn acting_states_and_only_they_accept_done() {
    for s in PhaseState::ALL {
        assert_eq!(
            next(s, Input::Done).is_some(),
            s.is_acting(),
            "{}",
            s.name()
        );
    }
    assert!(!PhaseState::Running.is_transient());
    assert!(PhaseState::Holding.is_transient());
    assert!(!PhaseState::Held.is_acting());
}

#[test]
fn abort_is_accepted_wherever_something_is_still_happening() {
    for s in PhaseState::ALL {
        let expected = !matches!(s, PhaseState::Idle | PhaseState::Aborting) && !s.is_final();
        assert_eq!(accepts(s, Command::Abort), expected, "{}", s.name());
    }
}

#[test]
fn final_states_reset_to_idle_through_resetting() {
    for s in [
        PhaseState::Complete,
        PhaseState::Stopped,
        PhaseState::Aborted,
    ] {
        assert!(s.is_final());
        let (r, _) = step(s, Input::Command(Command::Reset));
        assert_eq!(r, PhaseState::Resetting);
        assert_eq!(step(r, Input::Done).0, PhaseState::Idle);
    }
}

#[test]
fn every_state_is_reachable_from_idle_and_every_state_reaches_idle() {
    // Breadth-first over the 11 states, bounded by STATES passes.
    let reach = |from: PhaseState| {
        let mut seen = [false; STATES];
        seen[usize::from(from.code())] = true;
        for _ in 0..STATES {
            for s in PhaseState::ALL {
                if !seen[usize::from(s.code())] {
                    continue;
                }
                for i in inputs() {
                    if let Some(t) = next(s, i) {
                        seen[usize::from(t.code())] = true;
                    }
                }
            }
        }
        seen
    };
    assert_eq!(reach(PhaseState::Idle), [true; STATES]);
    for s in PhaseState::ALL {
        assert!(reach(s)[0], "{} cannot get back to Idle", s.name());
    }
}

#[test]
fn done_alone_from_any_acting_state_ends_in_a_quiescent_state_within_three_steps() {
    for s in PhaseState::ALL {
        let mut cur = s;
        for _ in 0..3 {
            if !cur.is_acting() {
                break;
            }
            cur = step(cur, Input::Done).0;
        }
        assert!(!cur.is_acting(), "{} keeps acting", s.name());
    }
}
