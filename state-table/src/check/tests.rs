//! Tests of [`super::check`]: one per rule, plus a clean table.

use super::*;
use crate::{CmpOp, Event, Node, Row, State};
use std::vec::Vec;

const PINS: [Pin<'static>; 4] = [
    Pin {
        label: "LED",
        func: PinUse::Output,
    },
    Pin {
        label: "BUTTON",
        func: PinUse::Input,
    },
    Pin {
        label: "DOOR",
        func: PinUse::Input,
    },
    Pin {
        label: "POT",
        func: PinUse::Other,
    },
];

const OFF_ON: [State<'static>; 2] = [
    State {
        name: "Off",
        entry: &[],
    },
    State {
        name: "On",
        entry: &[],
    },
];
const PRESS: [Event<'static>; 1] = [Event { name: "PRESS" }];

/// `label = v`.
const fn eq(label: &str, v: i32) -> [Node<'_>; 3] {
    [
        Node::Pin(label),
        Node::Const(v),
        Node::Cmp {
            op: CmpOp::Eq,
            a: 0,
            b: 1,
        },
    ]
}

const BUTTON_1: [Node<'static>; 3] = eq("BUTTON", 1);
const BUTTON_0: [Node<'static>; 3] = eq("BUTTON", 0);
const DOOR_1: [Node<'static>; 3] = eq("DOOR", 1);

fn keys(k: &str) -> Result<(), KeyProblem> {
    if k.is_empty() {
        Err(KeyProblem::Empty)
    } else if k.starts_with('/') || k.ends_with('/') || k.contains("//") {
        Err(KeyProblem::EmptySegment)
    } else if let Some(i) = k.find(' ') {
        Err(KeyProblem::InvalidByte(i))
    } else {
        Ok(())
    }
}

fn row<'a>(
    state: &'a str,
    event: &'a str,
    guard: Option<&'a [Node<'a>]>,
    next: &'a str,
    actions: &'a [Action<'a>],
) -> Row<'a> {
    Row {
        state,
        event,
        guard: guard.map(|nodes| Guard { nodes }),
        next,
        actions,
    }
}

fn machine<'a>(rows: &'a [Row<'a>]) -> Machine<'a> {
    Machine {
        id: "Mode",
        initial: "Off",
        states: &OFF_ON,
        events: &PRESS,
        rows,
    }
}

fn issues<'a>(machines: &[Machine<'a>]) -> Vec<Issue<'a>> {
    let mut out = Vec::new();
    check(machines, &PINS, keys, &mut |i| out.push(i));
    out
}

fn kinds<'a>(machines: &[Machine<'a>]) -> Vec<IssueKind<'a>> {
    issues(machines).into_iter().map(|i| i.kind).collect()
}

const TOGGLE: [Action<'static>; 1] = [Action::Toggle { label: "LED" }];

fn valid_rows() -> [Row<'static>; 3] {
    [
        row("Off", "PRESS", Some(&BUTTON_1), "On", &TOGGLE),
        row("Off", "PRESS", Some(&BUTTON_0), "Off", &[]),
        row("On", "PRESS", None, "Off", &TOGGLE),
    ]
}

#[test]
fn a_valid_machine_has_no_issues() {
    let rows = valid_rows();
    assert_eq!(issues(&[machine(&rows)]), []);
}

#[test]
fn machine_ids_must_be_identifiers_unique_ignoring_case() {
    let rows = valid_rows();
    let a = machine(&rows);
    let b = Machine { id: "mode", ..a };
    let c = Machine { id: "2nd", ..a };
    let got = issues(&[a, b, c]);
    let at: Vec<_> = got
        .iter()
        .map(|i| (i.machine, i.list, i.index, i.field, i.severity))
        .collect();
    assert_eq!(
        at,
        [
            (0, List::Machine, None, Field::Id, Severity::Error),
            (1, List::Machine, None, Field::Id, Severity::Error),
            (2, List::Machine, None, Field::Id, Severity::Error),
        ]
    );
    assert_eq!(
        got[0].kind,
        IssueKind::IdDuplicate {
            other: 1,
            id: "Mode"
        }
    );
    assert_eq!(
        got[1].kind,
        IssueKind::IdDuplicate {
            other: 0,
            id: "mode"
        }
    );
    assert_eq!(got[2].kind, IssueKind::IdInvalid { id: "2nd" });
}

#[test]
fn a_machine_needs_at_least_one_state() {
    let m = Machine {
        states: &[],
        events: &[],
        initial: "",
        ..machine(&[])
    };
    assert_eq!(kinds(&[m]), [IssueKind::NoStates]);
}

#[test]
fn oversized_machines_report_the_size_and_skip_the_rest() {
    let states = [State {
        name: "bad name",
        entry: &[],
    }; MAX_STATES + 1];
    let events = [Event { name: "E" }; MAX_EVENTS + 1];
    let rows = [row("x", "y", None, "z", &[]); MAX_ROWS + 1];
    let m = Machine {
        id: "Big",
        initial: "nope",
        states: &states,
        events: &events,
        rows: &rows,
    };
    let got = issues(&[m]);
    assert_eq!(
        got.iter().map(|i| i.kind).collect::<Vec<_>>(),
        [
            IssueKind::TooManyStates { count: 17 },
            IssueKind::TooManyEvents { count: 17 },
            IssueKind::TooManyRows { count: 65 },
        ]
    );
    assert!(
        got.iter()
            .all(|i| i.field == Field::Size && i.severity == Severity::Error)
    );
    let ok_states = [State {
        name: "S",
        entry: &[],
    }; MAX_STATES];
    let m = Machine {
        states: &ok_states,
        initial: "S",
        ..machine(&[])
    };
    assert!(
        !kinds(&[m])
            .iter()
            .any(|k| matches!(k, IssueKind::TooManyStates { .. }))
    );
}

#[test]
fn state_and_event_names_must_be_identifiers_unique_ignoring_case() {
    let states = [
        State {
            name: "Off",
            entry: &[],
        },
        State {
            name: "OFF",
            entry: &[],
        },
        State {
            name: "",
            entry: &[],
        },
    ];
    let events = [Event { name: "PRESS" }, Event { name: "go now" }];
    let rows = [row("Off", "PRESS", None, "Off", &[])];
    let m = Machine {
        states: &states,
        events: &events,
        ..machine(&rows)
    };
    let got: Vec<_> = issues(&[m])
        .into_iter()
        .filter(|i| i.field == Field::Name && i.severity == Severity::Error)
        .map(|i| (i.list, i.index, i.kind))
        .collect();
    assert_eq!(
        got,
        [
            (
                List::States,
                Some(0),
                IssueKind::NameDuplicate {
                    other: 1,
                    name: "Off"
                }
            ),
            (
                List::States,
                Some(1),
                IssueKind::NameDuplicate {
                    other: 0,
                    name: "OFF"
                }
            ),
            (List::States, Some(2), IssueKind::NameInvalid { name: "" }),
            (
                List::Events,
                Some(1),
                IssueKind::NameInvalid { name: "go now" }
            ),
        ]
    );
}

#[test]
fn initial_must_name_a_state_with_a_fix_to_the_first() {
    let rows = valid_rows();
    let m = Machine {
        initial: "Idle",
        ..machine(&rows)
    };
    let got = issues(&[m]);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].field, Field::Initial);
    assert_eq!(
        got[0].kind,
        IssueKind::InitialUnknown {
            name: "Idle",
            first: "Off"
        }
    );
    assert_eq!(got[0].fix, Some(Fix::SetInitial("Off")));
}

#[test]
fn rows_must_name_known_states_and_events_with_add_fixes() {
    let rows = [
        row("Off", "PRESS", None, "On", &[]),
        row("On", "PRESS", None, "Off", &[]),
        row("Busy", "PRESS", None, "on", &[]),
        row("On", "HOLD", None, "bad name", &[]),
    ];
    let got: Vec<_> = issues(&[machine(&rows)])
        .into_iter()
        .filter(|i| i.severity == Severity::Error)
        .map(|i| (i.index, i.field, i.kind, i.fix))
        .collect();
    assert_eq!(
        got,
        [
            (
                Some(2),
                Field::State,
                IssueKind::UnknownState {
                    name: "Busy",
                    near: None
                },
                Some(Fix::AddState("Busy"))
            ),
            (
                Some(2),
                Field::Next,
                IssueKind::UnknownState {
                    name: "on",
                    near: Some("On")
                },
                None
            ),
            (
                Some(3),
                Field::Next,
                IssueKind::UnknownState {
                    name: "bad name",
                    near: None
                },
                None
            ),
            (
                Some(3),
                Field::Event,
                IssueKind::UnknownEvent {
                    name: "HOLD",
                    near: None
                },
                Some(Fix::AddEvent("HOLD"))
            ),
        ]
    );
}

#[test]
fn guard_pins_must_exist_and_be_digital() {
    let guard = [
        Node::Pin("LED"),
        Node::Pin("POT"),
        Node::And(0, 1),
        Node::Pin("SENSOR"),
        Node::Or(2, 3),
        Node::Pin("button"),
        Node::Or(4, 5),
    ];
    let rows = [
        row("Off", "PRESS", Some(&guard), "On", &[]),
        row("On", "PRESS", None, "Off", &[]),
    ];
    let got: Vec<_> = issues(&[machine(&rows)])
        .into_iter()
        .filter(|i| i.severity == Severity::Error)
        .map(|i| (i.field, i.kind, i.fix))
        .collect();
    assert_eq!(
        got,
        [
            (
                Field::Guard,
                IssueKind::WrongPinUse {
                    action: None,
                    label: "POT",
                    has: PinUse::Other
                },
                None
            ),
            (
                Field::Guard,
                IssueKind::UnknownPin {
                    action: None,
                    label: "SENSOR",
                    near: None
                },
                Some(Fix::AddPin {
                    label: "SENSOR",
                    func: PinUse::Input
                })
            ),
            (
                Field::Guard,
                IssueKind::UnknownPin {
                    action: None,
                    label: "button",
                    near: Some("BUTTON")
                },
                None
            ),
        ]
    );
}

#[test]
fn guards_deeper_than_four_or_malformed_are_errors() {
    let deep = [
        Node::Pin("BUTTON"),
        Node::Not(0),
        Node::Not(1),
        Node::Not(2),
        Node::Not(3),
    ];
    let bad = [Node::Not(1), Node::Pin("BUTTON")];
    let rows = [
        row("Off", "PRESS", Some(&deep), "On", &[]),
        row("On", "PRESS", Some(&bad), "Off", &[]),
        row("On", "PRESS", Some(&[]), "Off", &[]),
    ];
    let got: Vec<_> = issues(&[machine(&rows)])
        .into_iter()
        .filter(|i| i.severity == Severity::Error)
        .map(|i| (i.index, i.kind))
        .collect();
    assert_eq!(
        got,
        [
            (
                Some(0),
                IssueKind::GuardMalformed(GuardError::TooDeep { depth: 5 })
            ),
            (
                Some(1),
                IssueKind::GuardMalformed(GuardError::BadChild { node: 0 })
            ),
            (Some(2), IssueKind::GuardMalformed(GuardError::Empty)),
        ]
    );
}

#[test]
fn write_and_toggle_need_an_output_pin() {
    let acts = [
        Action::Write {
            label: "LED",
            level: true,
        },
        Action::Toggle { label: "BUTTON" },
        Action::Write {
            label: "BUZZER",
            level: false,
        },
        Action::Toggle { label: "led" },
    ];
    let rows = [
        row("Off", "PRESS", None, "On", &acts),
        row("On", "PRESS", None, "Off", &[]),
    ];
    let got: Vec<_> = issues(&[machine(&rows)])
        .into_iter()
        .map(|i| (i.index, i.field, i.kind, i.fix))
        .collect();
    assert_eq!(
        got,
        [
            (
                Some(0),
                Field::Actions,
                IssueKind::WrongPinUse {
                    action: Some(1),
                    label: "BUTTON",
                    has: PinUse::Input
                },
                None
            ),
            (
                Some(0),
                Field::Actions,
                IssueKind::UnknownPin {
                    action: Some(2),
                    label: "BUZZER",
                    near: None
                },
                Some(Fix::AddPin {
                    label: "BUZZER",
                    func: PinUse::Output
                })
            ),
            (
                Some(0),
                Field::Actions,
                IssueKind::UnknownPin {
                    action: Some(3),
                    label: "led",
                    near: Some("LED")
                },
                None
            ),
        ]
    );
}

#[test]
fn publish_keys_follow_the_key_rule_and_avoid_sys() {
    let long = [b'x'; MAX_TEXT + 1];
    let long = core::str::from_utf8(&long).unwrap_or("");
    let acts = [
        Action::Publish {
            key: "led/state",
            text: "on",
        },
        Action::Publish {
            key: "sys/reboot",
            text: "",
        },
        Action::Publish {
            key: "a//b",
            text: long,
        },
        Action::Publish {
            key: "sys",
            text: "",
        },
    ];
    let more = [
        Action::Publish { key: "", text: "" },
        Action::Log { text: long },
    ];
    let rows = [
        row("Off", "PRESS", None, "On", &acts),
        row("On", "PRESS", None, "Off", &more),
    ];
    let got: Vec<_> = issues(&[machine(&rows)])
        .into_iter()
        .map(|i| (i.index, i.kind))
        .collect();
    assert_eq!(
        got,
        [
            (
                Some(0),
                IssueKind::BadKey {
                    action: 1,
                    key: "sys/reboot",
                    problem: KeyProblem::System
                }
            ),
            (
                Some(0),
                IssueKind::BadKey {
                    action: 2,
                    key: "a//b",
                    problem: KeyProblem::EmptySegment
                }
            ),
            (
                Some(0),
                IssueKind::TextTooLong {
                    action: 2,
                    len: 513
                }
            ),
            (
                Some(0),
                IssueKind::BadKey {
                    action: 3,
                    key: "sys",
                    problem: KeyProblem::System
                }
            ),
            (
                Some(1),
                IssueKind::BadKey {
                    action: 0,
                    key: "",
                    problem: KeyProblem::Empty
                }
            ),
            (
                Some(1),
                IssueKind::TextTooLong {
                    action: 1,
                    len: 513
                }
            ),
        ]
    );
}

#[test]
fn raise_must_name_an_event_of_the_same_machine() {
    let acts = [
        Action::Raise { event: "PRESS" },
        Action::Raise { event: "TICK" },
        Action::Raise { event: "press" },
    ];
    let rows = [
        row("Off", "PRESS", None, "On", &acts),
        row("On", "PRESS", None, "Off", &[]),
    ];
    let got: Vec<_> = issues(&[machine(&rows)])
        .into_iter()
        .map(|i| (i.kind, i.fix))
        .collect();
    assert_eq!(
        got,
        [
            (
                IssueKind::RaiseUnknown {
                    action: 1,
                    event: "TICK",
                    near: None
                },
                Some(Fix::AddEvent("TICK"))
            ),
            (
                IssueKind::RaiseUnknown {
                    action: 2,
                    event: "press",
                    near: Some("PRESS")
                },
                None
            ),
        ]
    );
}

#[test]
fn rows_and_entries_hold_at_most_four_actions() {
    let five = [Action::Log { text: "x" }; MAX_ACTIONS + 1];
    let states = [
        State {
            name: "Off",
            entry: &five,
        },
        State {
            name: "On",
            entry: &[],
        },
    ];
    let rows = [
        row("Off", "PRESS", None, "On", &five),
        row("On", "PRESS", None, "Off", &[]),
    ];
    let m = Machine {
        states: &states,
        ..machine(&rows)
    };
    let got: Vec<_> = issues(&[m])
        .into_iter()
        .map(|i| (i.list, i.index, i.field, i.kind))
        .collect();
    assert_eq!(
        got,
        [
            (
                List::States,
                Some(0),
                Field::Entry,
                IssueKind::TooManyActions { count: 5 }
            ),
            (
                List::Rows,
                Some(0),
                Field::Actions,
                IssueKind::TooManyActions { count: 5 }
            ),
        ]
    );
}

#[test]
fn entry_actions_are_checked_like_row_actions() {
    let entry = [Action::Write {
        label: "NOPE",
        level: true,
    }];
    let states = [
        State {
            name: "Off",
            entry: &entry,
        },
        State {
            name: "On",
            entry: &[],
        },
    ];
    let rows = valid_rows();
    let m = Machine {
        states: &states,
        ..machine(&rows)
    };
    let got = issues(&[m]);
    assert_eq!(got.len(), 1);
    assert_eq!(
        (got[0].list, got[0].index, got[0].field),
        (List::States, Some(0), Field::Entry)
    );
    assert!(matches!(
        got[0].kind,
        IssueKind::UnknownPin { label: "NOPE", .. }
    ));
}

#[test]
fn a_cell_has_at_most_one_unguarded_row() {
    let always = [Node::Const(1)];
    let rows = [
        row("Off", "PRESS", None, "On", &[]),
        row("Off", "PRESS", None, "Off", &[]),
        row("Off", "PRESS", Some(&always), "On", &[]),
        row("On", "PRESS", None, "Off", &[]),
    ];
    let got: Vec<_> = issues(&[machine(&rows)])
        .into_iter()
        .map(|i| (i.index, i.field, i.severity, i.kind))
        .collect();
    assert_eq!(
        got,
        [
            (
                Some(1),
                Field::Guard,
                Severity::Error,
                IssueKind::DuplicateUnguarded { first: 0 }
            ),
            (
                Some(2),
                Field::Guard,
                Severity::Error,
                IssueKind::DuplicateUnguarded { first: 0 }
            ),
        ]
    );
}

#[test]
fn an_always_true_guard_counts_as_the_unguarded_row() {
    // BUTTON or not BUTTON: always true, though it reads a pin.
    let always = [Node::Pin("BUTTON"), Node::Not(0), Node::Or(0, 1)];
    let rows = [
        row("Off", "PRESS", Some(&always), "On", &[]),
        row("Off", "PRESS", None, "Off", &[]),
        row("On", "PRESS", None, "Off", &[]),
    ];
    assert_eq!(
        kinds(&[machine(&rows)]),
        [IssueKind::DuplicateUnguarded { first: 0 }]
    );
}

#[test]
fn an_always_true_guard_overlaps_every_other_guarded_row() {
    let always = [Node::Const(1)];
    let rows = [
        row("Off", "PRESS", Some(&BUTTON_1), "On", &[]),
        row("Off", "PRESS", Some(&always), "Off", &[]),
        row("On", "PRESS", None, "Off", &[]),
    ];
    let got = kinds(&[machine(&rows)]);
    assert_eq!(got.len(), 1);
    let IssueKind::GuardOverlap { other, witness } = got[0] else {
        panic!("{got:?}")
    };
    assert_eq!(other, 0);
    assert_eq!(witness.levels().collect::<Vec<_>>(), [("BUTTON", true)]);
}

#[test]
fn overlapping_guards_report_a_witness_assignment() {
    let rows = [
        row("Off", "PRESS", Some(&BUTTON_1), "On", &[]),
        row("Off", "PRESS", Some(&DOOR_1), "Off", &[]),
        row("On", "PRESS", None, "Off", &[]),
    ];
    let got = issues(&[machine(&rows)]);
    assert_eq!(got.len(), 1);
    assert_eq!(
        (got[0].index, got[0].field, got[0].severity),
        (Some(1), Field::Guard, Severity::Error)
    );
    let IssueKind::GuardOverlap { other, witness } = got[0].kind else {
        panic!("{got:?}")
    };
    assert_eq!(other, 0);
    assert_eq!(witness.len(), 2);
    assert_eq!(
        witness.levels().collect::<Vec<_>>(),
        [("BUTTON", true), ("DOOR", true)]
    );
}

#[test]
fn exclusive_guards_in_one_cell_do_not_overlap() {
    // BUTTON = 1 and DOOR = 0, versus DOOR = 1: never both.
    let a = [
        Node::Pin("BUTTON"),
        Node::Const(1),
        Node::Cmp {
            op: CmpOp::Eq,
            a: 0,
            b: 1,
        },
        Node::Pin("DOOR"),
        Node::Const(0),
        Node::Cmp {
            op: CmpOp::Eq,
            a: 3,
            b: 4,
        },
        Node::And(2, 5),
    ];
    let rows = [
        row("Off", "PRESS", Some(&a), "On", &[]),
        row("Off", "PRESS", Some(&DOOR_1), "Off", &[]),
        row("Off", "PRESS", None, "Off", &[]),
        row("On", "PRESS", None, "Off", &[]),
    ];
    assert_eq!(issues(&[machine(&rows)]), []);
}

#[test]
fn rows_in_different_cells_never_conflict() {
    let rows = [
        row("Off", "PRESS", Some(&BUTTON_1), "On", &[]),
        row("On", "PRESS", Some(&BUTTON_1), "Off", &[]),
        row("Off", "PRESS", None, "Off", &[]),
        row("On", "PRESS", None, "Off", &[]),
    ];
    assert_eq!(issues(&[machine(&rows)]), []);
}

#[test]
fn overlap_above_twelve_pins_is_a_warning_not_checked() {
    // Two guards of 7 distinct pins each (a depth-4 or of 8 leaves would
    // do too): 14 together.
    const fn ors<'a>(p: [&'a str; 7]) -> [Node<'a>; 13] {
        [
            Node::Pin(p[0]),
            Node::Pin(p[1]),
            Node::Or(0, 1),
            Node::Pin(p[2]),
            Node::Pin(p[3]),
            Node::Or(3, 4),
            Node::Or(2, 5),
            Node::Pin(p[4]),
            Node::Pin(p[5]),
            Node::Or(7, 8),
            Node::Pin(p[6]),
            Node::Or(9, 10),
            Node::Or(6, 11),
        ]
    }
    let a = ors(["A1", "A2", "A3", "A4", "A5", "A6", "A7"]);
    let b = ors(["B1", "B2", "B3", "B4", "B5", "B6", "B7"]);
    let rows = [
        row("Off", "PRESS", Some(&a), "On", &[]),
        row("Off", "PRESS", Some(&b), "Off", &[]),
        row("On", "PRESS", None, "Off", &[]),
    ];
    let got: Vec<_> = issues(&[machine(&rows)])
        .into_iter()
        .filter(|i| i.severity == Severity::Warning)
        .map(|i| (i.index, i.kind))
        .collect();
    assert_eq!(
        got,
        [(Some(1), IssueKind::OverlapUnchecked { other: 0, pins: 14 })]
    );
}

#[test]
fn a_guard_that_is_never_true_is_a_warning() {
    let never = [Node::Pin("BUTTON"), Node::Not(0), Node::And(0, 1)];
    let rows = [
        row("Off", "PRESS", Some(&never), "On", &[]),
        row("Off", "PRESS", None, "On", &[]),
        row("On", "PRESS", None, "Off", &[]),
    ];
    let got = issues(&[machine(&rows)]);
    assert_eq!(got.len(), 1);
    assert_eq!(
        (got[0].index, got[0].field, got[0].severity),
        (Some(0), Field::Guard, Severity::Warning)
    );
    assert_eq!(got[0].kind, IssueKind::GuardNeverTrue);
}

#[test]
fn unreachable_states_are_warnings() {
    let states = [
        State {
            name: "Off",
            entry: &[],
        },
        State {
            name: "On",
            entry: &[],
        },
        State {
            name: "Far",
            entry: &[],
        },
        State {
            name: "Island",
            entry: &[],
        },
    ];
    let rows = [
        row("Off", "PRESS", None, "On", &[]),
        row("On", "PRESS", None, "Far", &[]),
        row("Island", "PRESS", None, "Off", &[]),
    ];
    let m = Machine {
        states: &states,
        ..machine(&rows)
    };
    let got = issues(&[m]);
    assert_eq!(got.len(), 1);
    assert_eq!(
        (got[0].list, got[0].index, got[0].field, got[0].severity),
        (List::States, Some(3), Field::Name, Severity::Warning)
    );
    assert_eq!(got[0].kind, IssueKind::Unreachable { name: "Island" });
}

#[test]
fn reachability_follows_long_chains_within_the_state_bound() {
    const NAMES: [&str; MAX_STATES] = [
        "S0", "S1", "S2", "S3", "S4", "S5", "S6", "S7", "S8", "S9", "S10", "S11", "S12", "S13",
        "S14", "S15",
    ];
    let states = NAMES.map(|name| State { name, entry: &[] });
    // Rows listed backwards, so each pass reaches one more state.
    let rows: [Row<'_>; MAX_STATES - 1] = core::array::from_fn(|k| {
        let i = MAX_STATES - 2 - k;
        row(NAMES[i], "PRESS", None, NAMES[i + 1], &[])
    });
    let m = Machine {
        initial: "S0",
        states: &states,
        ..machine(&rows)
    };
    assert_eq!(issues(&[m]), []);
}

#[test]
fn events_no_row_handles_are_dead_warnings() {
    let events = [Event { name: "PRESS" }, Event { name: "HOLD" }];
    let rows = valid_rows();
    let m = Machine {
        events: &events,
        ..machine(&rows)
    };
    let got = issues(&[m]);
    assert_eq!(got.len(), 1);
    assert_eq!(
        (got[0].list, got[0].index, got[0].field, got[0].severity),
        (List::Events, Some(1), Field::Name, Severity::Warning)
    );
    assert_eq!(got[0].kind, IssueKind::DeadEvent { name: "HOLD" });
}

#[test]
fn add_fixes_are_withheld_when_the_list_is_full() {
    let names = [
        "E0", "E1", "E2", "E3", "E4", "E5", "E6", "E7", "E8", "E9", "E10", "E11", "E12", "E13",
        "E14", "E15",
    ];
    let events = names.map(|name| Event { name });
    let rows = [row("Off", "TICK", None, "On", &[])];
    let m = Machine {
        events: &events,
        ..machine(&rows)
    };
    let unknown = issues(&[m]).into_iter().find(|i| i.field == Field::Event);
    assert_eq!(unknown.map(|i| i.fix), Some(None));
}
