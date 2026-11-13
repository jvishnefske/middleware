//! Tests of [`super::check_flows`]: one per rule, plus clean flows.

use super::*;
use crate::{CmpOp, Event, Node, State};
use std::vec;
use std::vec::Vec;

const PINS: [Pin<'static>; 3] = [
    Pin {
        label: "LED",
        func: PinUse::Output,
    },
    Pin {
        label: "BUTTON",
        func: PinUse::Input,
    },
    Pin {
        label: "POT",
        func: PinUse::Other,
    },
];

const STATES: [State<'static>; 3] = [
    State {
        name: "Off",
        entry: &[],
    },
    State {
        name: "On",
        entry: &[],
    },
    State {
        name: "Blink",
        entry: &[],
    },
];
const EVENTS: [Event<'static>; 2] = [Event { name: "PRESS" }, Event { name: "TICK" }];
const MODE: [Machine<'static>; 1] = [Machine {
    id: "Mode",
    initial: "Off",
    states: &STATES,
    events: &EVENTS,
    rows: &[],
}];

fn keys(k: &str) -> Result<(), KeyProblem> {
    if k.is_empty() {
        Err(KeyProblem::Empty)
    } else if k.contains("//") {
        Err(KeyProblem::EmptySegment)
    } else {
        Ok(())
    }
}

fn node<'a>(id: &'a str, kind: NodeKind<'a>) -> FlowNode<'a> {
    FlowNode { id, kind }
}
fn wire<'a>(from: &'a str, from_port: &'a str, to: &'a str) -> Wire<'a> {
    Wire {
        from,
        from_port,
        to,
        to_port: IN_PORT,
    }
}
fn flow<'a>(id: &'a str, nodes: &'a [FlowNode<'a>], wires: &'a [Wire<'a>]) -> Flow<'a> {
    Flow { id, nodes, wires }
}
const fn raise(event: &'static str) -> NodeKind<'static> {
    NodeKind::Action(Action::Raise {
        machine: Some("Mode"),
        event,
    })
}
const LOG: NodeKind<'static> = NodeKind::Action(Action::Log { text: "x" });
const INIT: NodeKind<'static> = NodeKind::Entry(Entry::Init);

fn issues<'a>(flows: &[Flow<'a>]) -> Vec<FlowIssue<'a>> {
    let mut out = Vec::new();
    check_flows(flows, &MODE, &PINS, keys, &mut |i| out.push(i));
    out
}
fn found<'a>(flows: &[Flow<'a>]) -> Vec<(usize, At, FlowKind<'a>)> {
    issues(flows)
        .into_iter()
        .map(|i| (i.flow, i.at, i.kind))
        .collect()
}

#[test]
fn the_mode_example_as_flows_has_no_issues() {
    let press = [
        node(
            "n1",
            NodeKind::Entry(Entry::PinEdge {
                label: "BUTTON",
                edge: Edge::Fall,
            }),
        ),
        node("n2", raise("PRESS")),
    ];
    let tick = [
        node("n1", NodeKind::Entry(Entry::Every { ms: 250 })),
        node("n2", raise("TICK")),
    ];
    let blink = [
        node(
            "n1",
            NodeKind::Entry(Entry::Entered {
                machine: "Mode",
                state: "Blink",
            }),
        ),
        node("n2", LOG),
    ];
    let w = [wire("n1", "next", "n2")];
    let flows = [
        flow("Press", &press, &w),
        flow("Tick", &tick, &w),
        flow("Blinking", &blink, &w),
    ];
    assert_eq!(found(&flows), []);
}

#[test]
fn every_node_kind_and_port_wired_into_a_tree_is_clean() {
    let cond = [
        Node::Pin("BUTTON"),
        Node::Const(0),
        Node::Cmp {
            op: CmpOp::Eq,
            a: 0,
            b: 1,
        },
    ];
    let nodes = [
        node("e", INIT),
        node("s", NodeKind::Sequence { steps: 3 }),
        node("b", NodeKind::Branch(Guard { nodes: &cond })),
        node("t", NodeKind::Action(Action::Toggle { label: "LED" })),
        node(
            "f",
            NodeKind::Action(Action::Write {
                label: "LED",
                level: false,
            }),
        ),
        node("r", NodeKind::Repeat { times: 3 }),
        node("body", LOG),
        node("after", LOG),
        node(
            "p",
            NodeKind::Action(Action::Publish {
                key: "app/led",
                text: "on",
            }),
        ),
    ];
    let wires = [
        wire("e", "next", "s"),
        wire("s", "1", "b"),
        wire("b", "then", "t"),
        wire("b", "else", "f"),
        wire("s", "2", "r"),
        wire("r", "body", "body"),
        wire("r", "next", "after"),
        wire("s", "3", "p"),
    ];
    assert_eq!(found(&[flow("All", &nodes, &wires)]), []);
    assert_eq!(nodes[1].kind.outputs(), ["1", "2", "3"]);
    assert_eq!(nodes[2].kind.outputs(), ["then", "else", "next"]);
    assert!(!nodes[0].kind.has_input());
}

#[test]
fn a_flow_has_exactly_one_entry() {
    let none = [node("a", LOG)];
    let two = [node("a", INIT), node("b", INIT)];
    assert_eq!(
        found(&[flow("A", &none, &[]), flow("B", &two, &[])]),
        [
            (0, At::Flow, FlowKind::NoEntry),
            (1, At::Node(1), FlowKind::ExtraEntry { first: 0 }),
        ]
    );
}

#[test]
fn flow_and_node_ids_are_identifiers_and_unique() {
    let nodes = [node("e", INIT), node("e", LOG), node("1x", LOG)];
    let wires = [wire("e", "next", "1x")];
    let got = found(&[
        flow("Main", &nodes, &wires),
        flow("main", &[], &[]),
        flow("", &[], &[]),
    ]);
    assert!(got.contains(&(0, At::Node(1), FlowKind::NodeIdDuplicate { other: 0 })));
    assert!(got.contains(&(0, At::Node(2), FlowKind::NodeIdInvalid { id: "1x" })));
    assert!(got.contains(&(
        1,
        At::Flow,
        FlowKind::IdDuplicate {
            other: 0,
            id: "main"
        }
    )));
    assert!(got.contains(&(2, At::Flow, FlowKind::IdInvalid { id: "" })));
}

#[test]
fn sizes_are_bounded_and_an_oversized_flow_is_not_checked_further() {
    let many = [node("x", LOG); MAX_FLOW_NODES + 1];
    let wires = [wire("x", "next", "y"); MAX_FLOW_WIRES + 1];
    assert_eq!(
        found(&[flow("Big", &many, &wires)]),
        [
            (0, At::Flow, FlowKind::TooManyNodes { count: 65 }),
            (0, At::Flow, FlowKind::TooManyWires { count: 129 }),
        ]
    );
    let one = [node("e", INIT)];
    let flows = [flow("F", &one, &[]); MAX_FLOWS + 1];
    let got = found(&flows);
    // Same ids: duplicates for 1..8, then the ninth is over the limit.
    assert_eq!(
        got.last(),
        Some(&(8, At::Flow, FlowKind::TooManyFlows { count: 9 }))
    );
    assert!(!got.iter().any(|(f, at, _)| *f == 8 && *at != At::Flow));
}

#[test]
fn wires_need_known_nodes_real_ports_and_no_entry_target() {
    let nodes = [node("e", INIT), node("a", LOG), node("b", LOG)];
    let wires = [
        wire("e", "next", "a"),
        wire("ghost", "next", "b"),
        wire("a", "then", "b"),
        wire("a", "next", "e"),
        Wire {
            to_port: "out",
            ..wire("a", "next", "b")
        },
    ];
    let got = found(&[flow("W", &nodes, &wires)]);
    assert_eq!(
        got,
        [
            (
                0,
                At::Wire(1),
                FlowKind::WireNodeUnknown {
                    end: End::From,
                    id: "ghost"
                }
            ),
            (0, At::Wire(2), FlowKind::WireBadOutput { port: "then" }),
            (0, At::Wire(3), FlowKind::WireIntoEntry),
            (0, At::Wire(4), FlowKind::WireBadInput { port: "out" }),
            (0, At::Node(2), FlowKind::Unreachable),
        ]
    );
}

#[test]
fn each_output_port_and_each_input_is_wired_at_most_once() {
    let nodes = [
        node("e", INIT),
        node("a", LOG),
        node("b", LOG),
        node("c", LOG),
    ];
    let wires = [
        wire("e", "next", "a"),
        wire("e", "next", "b"),
        wire("a", "next", "c"),
        wire("b", "next", "c"),
    ];
    let got = found(&[flow("W", &nodes, &wires)]);
    assert!(got.contains(&(0, At::Wire(1), FlowKind::OutputWiredTwice { other: 0 })));
    assert!(got.contains(&(0, At::Wire(3), FlowKind::InputWiredTwice { other: 2 })));
    // b is left without its wire from the entry, so it never runs.
    assert!(got.contains(&(0, At::Node(2), FlowKind::Unreachable)));
}

#[test]
fn a_cycle_is_an_error_on_each_of_its_wires_even_through_a_repeat_body() {
    let nodes = [
        node("e", INIT),
        node("r", NodeKind::Repeat { times: 2 }),
        node("a", LOG),
        node("b", LOG),
        node("tail", LOG),
    ];
    // e -> r -body-> a -> b -> r is not allowed: the body loops by itself.
    let wires = [
        wire("e", "next", "r"),
        wire("r", "body", "a"),
        wire("a", "next", "b"),
        wire("b", "next", "r"),
    ];
    let got = found(&[flow("C", &nodes, &wires)]);
    assert!(got.contains(&(0, At::Wire(3), FlowKind::InputWiredTwice { other: 0 })));
    // Without the entry's wire, the loop r -> a -> b -> r closes.
    let wires = [
        wire("r", "body", "a"),
        wire("a", "next", "b"),
        wire("b", "next", "r"),
        wire("r", "next", "tail"),
    ];
    let got = found(&[flow("C", &nodes, &wires)]);
    let cycles: Vec<_> = got
        .iter()
        .filter(|(_, _, k)| *k == FlowKind::Cycle)
        .map(|(_, at, _)| *at)
        .collect();
    assert_eq!(cycles, [At::Wire(0), At::Wire(1), At::Wire(2)]);
    // tail hangs off the cycle: it never runs either.
    assert!(got.contains(&(0, At::Node(4), FlowKind::Unreachable)));
}

#[test]
fn nodes_the_entry_does_not_lead_to_and_empty_repeats_and_branches_are_warnings() {
    let t = [Node::Const(1)];
    let nodes = [
        node("e", INIT),
        node("r", NodeKind::Repeat { times: 2 }),
        node("b", NodeKind::Branch(Guard { nodes: &t })),
        node("lost", LOG),
    ];
    let wires = [wire("e", "next", "r"), wire("r", "next", "b")];
    let all = issues(&[flow("U", &nodes, &wires)]);
    assert!(all.iter().all(|i| i.severity == Severity::Warning));
    assert_eq!(
        all.iter().map(|i| (i.at, i.kind)).collect::<Vec<_>>(),
        [
            (At::Node(1), FlowKind::RepeatEmpty),
            (At::Node(2), FlowKind::BranchEmpty),
            (At::Node(3), FlowKind::Unreachable),
        ]
    );
}

#[test]
fn entries_check_their_period_key_pin_and_state() {
    let entries = [
        Entry::Every { ms: 5 },
        Entry::Every {
            ms: i64::from(i32::MAX) + 1,
        },
        Entry::Message { key: "sys/info" },
        Entry::Message { key: "a//b" },
        Entry::PinEdge {
            label: "SWITCH",
            edge: Edge::Rise,
        },
        Entry::PinEdge {
            label: "LED",
            edge: Edge::Both,
        },
        Entry::Entered {
            machine: "mode",
            state: "On",
        },
        Entry::Entered {
            machine: "Mode",
            state: "on",
        },
    ];
    let want = [
        (FlowKind::BadPeriod { ms: 5 }, None),
        (FlowKind::BadPeriod { ms: 2147483648 }, None),
        (
            FlowKind::BadSubscribeKey {
                key: "sys/info",
                problem: KeyProblem::System,
            },
            None,
        ),
        (
            FlowKind::BadSubscribeKey {
                key: "a//b",
                problem: KeyProblem::EmptySegment,
            },
            None,
        ),
        (
            FlowKind::EdgePinUnknown {
                label: "SWITCH",
                near: None,
            },
            Some(Fix::AddPin {
                label: "SWITCH",
                func: PinUse::Input,
            }),
        ),
        (
            FlowKind::EdgePinNotInput {
                label: "LED",
                has: PinUse::Output,
            },
            None,
        ),
        (
            FlowKind::EnteredMachineUnknown {
                machine: "mode",
                near: Some("Mode"),
            },
            None,
        ),
        (
            FlowKind::EnteredStateUnknown {
                machine: "Mode",
                state: "on",
                near: Some("On"),
            },
            None,
        ),
    ];
    for (e, want) in entries.iter().zip(want) {
        let nodes = [node("e", NodeKind::Entry(*e))];
        let f = [flow("E", &nodes, &[])];
        let got: Vec<_> = issues(&f).into_iter().map(|i| (i.kind, i.fix)).collect();
        assert_eq!(got, [want], "{e:?}");
    }
}

#[test]
fn actions_and_conditions_follow_the_table_rules_and_raises_name_their_machine() {
    let bad_state = [Node::State {
        machine: "Mode",
        state: "Dim",
    }];
    let pot = [Node::Pin("POT")];
    let kinds = [
        NodeKind::Action(Action::Raise {
            machine: None,
            event: "PRESS",
        }),
        NodeKind::Action(Action::Raise {
            machine: Some("Lamp"),
            event: "PRESS",
        }),
        raise("HOLD"),
        NodeKind::Action(Action::Toggle { label: "BUTTON" }),
        NodeKind::Branch(Guard { nodes: &bad_state }),
        NodeKind::Branch(Guard { nodes: &pot }),
        NodeKind::Branch(Guard { nodes: &[] }),
    ];
    use crate::IssueKind as K;
    let want = [
        vec![(FlowKind::Table(K::RaiseNoMachine { action: 0 }), None)],
        vec![(
            FlowKind::Table(K::RaiseMachineUnknown {
                action: 0,
                machine: "Lamp",
                near: None,
            }),
            None,
        )],
        vec![(
            FlowKind::Table(K::RaiseUnknownIn {
                action: 0,
                machine: "Mode",
                event: "HOLD",
                near: None,
            }),
            Some(Fix::AddEventTo {
                machine: 0,
                name: "HOLD",
            }),
        )],
        vec![(
            FlowKind::Table(K::WrongPinUse {
                action: Some(0),
                label: "BUTTON",
                has: PinUse::Input,
            }),
            None,
        )],
        vec![(
            FlowKind::Table(K::GuardStateUnknown {
                machine: "Mode",
                state: "Dim",
                near: None,
            }),
            None,
        )],
        vec![(
            FlowKind::Table(K::WrongPinUse {
                action: None,
                label: "POT",
                has: PinUse::Other,
            }),
            None,
        )],
        vec![(
            FlowKind::Table(K::GuardMalformed(crate::GuardError::Empty)),
            None,
        )],
    ];
    for (k, want) in kinds.iter().zip(want) {
        let nodes = [node("e", INIT), node("x", *k), node("y", LOG)];
        let wires = [
            wire("e", "next", "x"),
            wire("x", k.outputs().first().copied().unwrap_or("next"), "y"),
        ];
        let f = [flow("A", &nodes, &wires)];
        let all = issues(&f);
        assert!(all.iter().all(|i| i.at == At::Node(1)), "{all:?}");
        let got: Vec<_> = all.into_iter().map(|i| (i.kind, i.fix)).collect();
        assert_eq!(got, want, "{k:?}");
    }
}

#[test]
fn repeat_counts_and_sequence_steps_are_bounded() {
    let kinds = [
        (
            NodeKind::Repeat { times: 0 },
            FlowKind::BadRepeat { times: 0 },
        ),
        (
            NodeKind::Repeat {
                times: MAX_REPEAT + 1,
            },
            FlowKind::BadRepeat { times: 10_001 },
        ),
        (
            NodeKind::Sequence { steps: 1 },
            FlowKind::BadSequence { steps: 1 },
        ),
        (
            NodeKind::Sequence { steps: 5 },
            FlowKind::BadSequence { steps: 5 },
        ),
    ];
    for (k, want) in kinds {
        let nodes = [node("e", INIT), node("x", k), node("y", LOG)];
        let wires = [
            wire("e", "next", "x"),
            wire("x", k.outputs().first().copied().unwrap_or("next"), "y"),
        ];
        let f = [flow("R", &nodes, &wires)];
        assert_eq!(found(&f), [(0, At::Node(1), want)]);
    }
    // Out-of-range step counts still name a bounded set of ports.
    assert_eq!(NodeKind::Sequence { steps: 9 }.outputs().len(), 4);
    assert_eq!(NodeKind::Sequence { steps: -1 }.outputs().len(), 2);
}

const BLINK_PARAMS: [&str; 2] = ["period_ms", "times"];
const PHASES: [PhaseDecl<'static>; 2] = [
    PhaseDecl {
        em: "LED",
        phase: "Blink",
        params: &BLINK_PARAMS,
    },
    PhaseDecl {
        em: "ADC",
        phase: "Measure",
        params: &["samples"],
    },
];

fn phase_found<'a>(flows: &[Flow<'a>]) -> Vec<(usize, At, FlowKind<'a>)> {
    let mut out = Vec::new();
    check_flows_in(flows, &MODE, &PINS, &PHASES, keys, &mut |i| {
        out.push((i.flow, i.at, i.kind));
    });
    out
}

const fn phase_entry(
    em: &'static str,
    phase: &'static str,
    state: &'static str,
    every: Option<i64>,
) -> NodeKind<'static> {
    NodeKind::Entry(Entry::Phase {
        em,
        phase,
        state,
        every,
    })
}

const fn done(after: After<'static>) -> NodeKind<'static> {
    NodeKind::Phase(PhaseAction::Done {
        em: "LED",
        phase: "Blink",
        after,
    })
}

#[test]
fn a_blink_body_with_done_after_a_parameter_and_a_report_is_clean() {
    let values = [
        PhaseValue::Runs,
        PhaseValue::Param("times"),
        PhaseValue::Pin("POT"),
        PhaseValue::Const(-1),
    ];
    let nodes = [
        node("e", phase_entry("LED", "Blink", "running", Some(250))),
        node("t", NodeKind::Action(Action::Toggle { label: "LED" })),
        node(
            "r",
            NodeKind::Phase(PhaseAction::Report {
                em: "LED",
                phase: "Blink",
                values: &values,
            }),
        ),
        node("d", done(After::Param("times"))),
    ];
    let wires = [
        wire("e", "next", "t"),
        wire("t", "next", "r"),
        wire("r", "next", "d"),
    ];
    // Another flow (a button) may finish the phase at once.
    let button = [
        node(
            "e",
            NodeKind::Entry(Entry::PinEdge {
                label: "BUTTON",
                edge: Edge::Fall,
            }),
        ),
        node("d", done(After::Now)),
    ];
    let hold = [
        node("e", phase_entry("LED", "Blink", "holding", None)),
        node("d", done(After::Count(1))),
    ];
    let w = [wire("e", "next", "d")];
    let flows = [
        flow("BlinkRun", &nodes, &wires),
        flow("Button", &button, &w),
        flow("BlinkHold", &hold, &w),
    ];
    assert_eq!(phase_found(&flows), []);
    // A phase action node runs its next port.
    assert_eq!(done(After::Now).outputs(), ["next"]);
    assert!(done(After::Now).has_input());
}

#[test]
fn phase_entries_name_a_phase_of_the_unit_and_an_acting_state() {
    let unknown = [node("e", phase_entry("led", "blink", "running", None))];
    let idle = [node("e", phase_entry("LED", "Blink", "idle", None))];
    let fast = [node("e", phase_entry("ADC", "Measure", "running", Some(5)))];
    let flows = [
        flow("A", &unknown, &[]),
        flow("B", &idle, &[]),
        flow("C", &fast, &[]),
    ];
    assert_eq!(
        phase_found(&flows),
        [
            (
                0,
                At::Node(0),
                FlowKind::PhaseUnknown {
                    em: "led",
                    phase: "blink",
                    near: Some(("LED", "Blink"))
                }
            ),
            (
                1,
                At::Node(0),
                FlowKind::PhaseStateNotActing { state: "idle" }
            ),
            (2, At::Node(0), FlowKind::BadPeriod { ms: 5 }),
        ]
    );
    // Without phases (check_flows), every phase entry is unknown.
    let ok = [node("e", phase_entry("LED", "Blink", "running", None))];
    assert_eq!(
        found(&[flow("A", &ok, &[])]),
        [(
            0,
            At::Node(0),
            FlowKind::PhaseUnknown {
                em: "LED",
                phase: "Blink",
                near: None
            }
        )]
    );
}

#[test]
fn a_phase_state_has_one_body_flow() {
    let body = [node("e", phase_entry("LED", "Blink", "stopping", None))];
    let other = [node("e", phase_entry("LED", "Blink", "aborting", None))];
    let flows = [
        flow("A", &body, &[]),
        flow("B", &other, &[]),
        flow("C", &body, &[]),
    ];
    assert_eq!(
        phase_found(&flows),
        [(2, At::Node(0), FlowKind::PhaseBodyTaken { other: 0 })]
    );
}

#[test]
fn phase_actions_name_a_phase_and_its_parameters_and_count_only_in_its_body() {
    let values = [
        PhaseValue::Param("nope"),
        PhaseValue::Pin("LED"),
        PhaseValue::Pin("led2"),
        PhaseValue::Runs,
    ];
    let nodes = [
        node("e", INIT),
        node(
            "u",
            NodeKind::Phase(PhaseAction::Done {
                em: "LED",
                phase: "Fade",
                after: After::Now,
            }),
        ),
        node("c", done(After::Count(0))),
        node("p", done(After::Param("samples"))),
        node(
            "r",
            NodeKind::Phase(PhaseAction::Report {
                em: "LED",
                phase: "Blink",
                values: &values,
            }),
        ),
    ];
    let wires = [
        wire("e", "next", "u"),
        wire("u", "next", "c"),
        wire("c", "next", "p"),
        wire("p", "next", "r"),
    ];
    let flows = [flow("Init", &nodes, &wires)];
    assert_eq!(
        phase_found(&flows),
        [
            (
                0,
                At::Node(1),
                FlowKind::PhaseUnknown {
                    em: "LED",
                    phase: "Fade",
                    near: None
                }
            ),
            (0, At::Node(2), FlowKind::BadAfter { count: 0 }),
            (0, At::Node(2), FlowKind::RunsOutsideBody),
            (
                0,
                At::Node(3),
                FlowKind::PhaseParamUnknown { param: "samples" }
            ),
            (0, At::Node(3), FlowKind::RunsOutsideBody),
            (
                0,
                At::Node(4),
                FlowKind::PhaseParamUnknown { param: "nope" }
            ),
            (0, At::Node(4), FlowKind::ValuePinOutput { label: "LED" }),
            (
                0,
                At::Node(4),
                FlowKind::ValuePinUnknown {
                    label: "led2",
                    near: None
                }
            ),
            (0, At::Node(4), FlowKind::RunsOutsideBody),
        ]
    );
}

#[test]
fn a_report_carries_at_most_four_values() {
    let values = [PhaseValue::Const(1); MAX_REPORT_VALUES + 1];
    let nodes = [
        node("e", phase_entry("LED", "Blink", "running", None)),
        node(
            "r",
            NodeKind::Phase(PhaseAction::Report {
                em: "LED",
                phase: "Blink",
                values: &values,
            }),
        ),
    ];
    let w = [wire("e", "next", "r")];
    let flows = [flow("A", &nodes, &w)];
    assert_eq!(
        phase_found(&flows),
        [(0, At::Node(1), FlowKind::TooManyValues { count: 5 })]
    );
}
