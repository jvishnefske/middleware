//! Message tests for [`super::check`]: wording, place and fix.

use super::*;
use crate::machines::{Expr, StateIn, flatten};
use crate::pins::Func;

fn pins() -> Vec<PinIn> {
    [("LED", Func::GpioOut), ("BUTTON", Func::GpioIn)]
        .into_iter()
        .map(|(l, f)| PinIn {
            label: l.into(),
            func: Ok(f),
        })
        .collect()
}

fn mode() -> MachineIn {
    let st = |n: &str| StateIn {
        name: n.into(),
        entry: vec![],
    };
    MachineIn {
        id: "Mode".into(),
        initial: "Off".into(),
        states: vec![st("Off"), st("On"), st("Blink")],
        events: vec!["PRESS".into(), "TICK".into()],
        rows: vec![],
    }
}

fn node(id: &str, kind: KindIn) -> NodeOwned {
    NodeOwned {
        id: id.into(),
        kind,
    }
}

fn wire(from: &str, port: &str, to: &str) -> WireIn {
    WireIn {
        from: from.into(),
        from_port: port.into(),
        to: to.into(),
        to_port: "in".into(),
    }
}

fn raise(machine: Option<&str>, event: &str) -> KindIn {
    KindIn::Action(ActionIn::Raise {
        machine: machine.map(Into::into),
        event: event.into(),
    })
}

fn lines(flows: &[FlowIn]) -> Vec<String> {
    check(flows, &[mode()], &pins())
        .into_iter()
        .map(|i| {
            let at = match (i.node, i.wire) {
                (Some(n), _) => format!("node {n}"),
                (_, Some(w)) => format!("wire {w}"),
                _ => "flow".into(),
            };
            format!("{} {at} {}: {}", i.flow, i.severity, i.message)
        })
        .collect()
}

#[test]
fn the_mode_example_flows_are_clean() {
    let (m, s) = split_ref("Mode.Blink");
    let flows = [
        FlowIn {
            id: "Press".into(),
            nodes: vec![
                node(
                    "n1",
                    KindIn::Entry(EntryIn::PinEdge {
                        label: "BUTTON".into(),
                        edge: Edge::Fall,
                    }),
                ),
                node("n2", raise(Some("Mode"), "PRESS")),
            ],
            wires: vec![wire("n1", "next", "n2")],
        },
        FlowIn {
            id: "Blinking".into(),
            nodes: vec![
                node(
                    "n1",
                    KindIn::Entry(EntryIn::Entered {
                        machine: m,
                        state: s,
                    }),
                ),
                node(
                    "n2",
                    KindIn::Action(ActionIn::Log {
                        text: "mode: blinking".into(),
                    }),
                ),
            ],
            wires: vec![wire("n1", "next", "n2")],
        },
    ];
    assert_eq!(lines(&flows), Vec::<String>::new());
    assert_eq!(split_ref("Mode"), ("Mode".into(), String::new()));
}

#[test]
fn graph_messages_name_the_other_wire_and_say_what_to_do() {
    let cond = flatten(&Expr::State("Mode".into(), "On".into()));
    let flows = [FlowIn {
        id: "Merge".into(),
        nodes: vec![
            node("e", KindIn::Entry(EntryIn::Init)),
            node("b", KindIn::Branch(cond)),
            node(
                "x",
                KindIn::Action(ActionIn::Toggle {
                    label: "LED".into(),
                }),
            ),
            node("lost", KindIn::Repeat { times: 3 }),
        ],
        wires: vec![
            wire("e", "next", "b"),
            wire("b", "then", "x"),
            wire("b", "else", "x"),
            wire("x", "next", "e"),
        ],
    }];
    assert_eq!(
        lines(&flows),
        [
            "0 wire 2 error: This node is already reached by the wire b.then -> x; a node runs after one thing. For what follows both sides of a branch, use the branch's next port.",
            "0 wire 3 error: This wire arrives at the entry, which has no input; remove it.",
            "0 node 3 warning: Nothing leads from the entry to this node, so it never runs.",
        ]
    );
}

#[test]
fn entry_and_action_messages_use_the_tables_wording_with_fixes() {
    let flows = [FlowIn {
        id: "Bad".into(),
        nodes: vec![
            node(
                "e",
                KindIn::Entry(EntryIn::PinEdge {
                    label: "SWITCH".into(),
                    edge: Edge::Rise,
                }),
            ),
            node("a", raise(None, "PRESS")),
            node("b", raise(Some("Mode"), "HOLD")),
            node(
                "c",
                KindIn::Action(ActionIn::Write {
                    label: "BUTTON".into(),
                    level: true,
                }),
            ),
        ],
        wires: vec![
            wire("e", "next", "a"),
            wire("a", "next", "b"),
            wire("b", "next", "c"),
        ],
    }];
    let got = check(&flows, &[mode()], &pins());
    let msgs: Vec<_> = got
        .iter()
        .map(|i| (i.node, i.message.as_str(), i.fix.clone()))
        .collect();
    assert_eq!(
        msgs,
        [
            (
                Some(0),
                "This entry watches SWITCH, which is not in the pin table; add it as a GPIO input.",
                Some(MachineFix::AddPin {
                    label: "SWITCH".into(),
                    func: Func::GpioIn
                })
            ),
            (
                Some(1),
                "This action: choose the machine whose event to raise.",
                None
            ),
            (
                Some(2),
                "This action raises HOLD, which is not an event of Mode; add it or choose another.",
                Some(MachineFix::AddEventTo {
                    machine: 0,
                    name: "HOLD".into()
                })
            ),
            (
                Some(3),
                "This action drives BUTTON, which is a GPIO input; only a GPIO output can be written or toggled.",
                None
            ),
        ]
    );
}

#[test]
fn a_flow_without_an_entry_and_a_loop_are_errors() {
    let flows = [FlowIn {
        id: "Loop".into(),
        nodes: vec![
            node("a", KindIn::Action(ActionIn::Log { text: "a".into() })),
            node("b", KindIn::Action(ActionIn::Log { text: "b".into() })),
        ],
        wires: vec![wire("a", "next", "b"), wire("b", "next", "a")],
    }];
    assert_eq!(
        lines(&flows),
        [
            "0 flow error: This flow has no entry, so it never runs; add one (on start, every, on message, on pin edge or when entered).",
            "0 wire 0 error: This wire closes a loop, so the flow would never end; remove it (a repeat runs its body by itself).",
            "0 wire 1 error: This wire closes a loop, so the flow would never end; remove it (a repeat runs its body by itself).",
        ]
    );
}
