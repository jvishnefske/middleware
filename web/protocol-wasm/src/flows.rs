//! The flow graphs' checks for the web editor: owned flows as the page
//! sends them, checked by [`state_table::flow::check_flows`] against the
//! machines and the pin table, with every issue rendered in plain
//! language.
//!
//! The web model (`web/src/model/flows.ts`) owns a flow's shape; the
//! `state-table` crate owns what is allowed in it. This module converts
//! owned values ([`FlowIn`]) into that crate's borrowed views (branch
//! conditions flattened like the tables' guards, [`crate::machines::flatten`]),
//! and turns each structured issue into a [`FlowIssueOut`]: the flow, the
//! node or wire it is about (by index), how bad, a message naming nodes by
//! id and saying what to do, and the same [`MachineFix`]es as the tables.
//! Action and condition problems are worded by the tables' own
//! [`crate::machines::part_message`].
//!
//! Everything here is pure; `crate::bindings` converts JS objects to
//! [`FlowIn`] and issues back.

use crate::machines::{
    ActionIn, MachineFix, MachineIn, NodeIn, PinIn, did_you_mean, fix, func_text, key_problem,
    part_message, severity_name, with_views,
};
use state_table::flow::{
    After, At, Edge, End, Entry, Flow, FlowIssue, FlowKind, FlowNode, MAX_FLOW_NODES,
    MAX_FLOW_WIRES, MAX_FLOWS, MAX_REPEAT, MAX_REPORT_VALUES, MAX_SEQUENCE, MIN_TICK_MS, NodeKind,
    PhaseAction, PhaseDecl, PhaseValue, Wire, check_flows_in,
};
use state_table::{Action, Guard, KeyProblem, Node};

/// An entry as the page sends it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryIn {
    /// `{on: "init"}`.
    Init,
    /// `{on: "every", ms}`.
    Every {
        /// Period.
        ms: i64,
    },
    /// `{on: "message", key}`.
    Message {
        /// Key.
        key: String,
    },
    /// `{on: "pin-edge", label, edge}`.
    PinEdge {
        /// Label.
        label: String,
        /// Edge.
        edge: Edge,
    },
    /// `{on: "entered", state: "<machine>.<state>"}`, split at the first
    /// dot (no dot: the whole text is the machine and the state is empty).
    Entered {
        /// Machine id.
        machine: String,
        /// State name.
        state: String,
    },
    /// `{on: "phase", phase: "<em>.<phase>", state, every?}`, the phase
    /// split at the first dot.
    Phase {
        /// Equipment module.
        em: String,
        /// Phase.
        phase: String,
        /// Acting state (lower case).
        state: String,
        /// Repeat period.
        every: Option<i64>,
    },
}

/// When a `phase-done` node completes its phase, as the page sends it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AfterIn {
    /// No `after`.
    Now,
    /// `after: <count>`.
    Count(i64),
    /// `after: "<param>"`.
    Param(String),
}

/// A report value as the page sends it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueIn {
    /// `{kind: "const", value}`.
    Const(i64),
    /// `{kind: "pin", label}`.
    Pin(String),
    /// `{kind: "param", param}`.
    Param(String),
    /// `{kind: "runs"}`.
    Runs,
}

/// A phase action node as the page sends it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhaseIn {
    /// `{kind: "phase-done", phase, after?}`.
    Done {
        /// Equipment module.
        em: String,
        /// Phase.
        phase: String,
        /// When.
        after: AfterIn,
    },
    /// `{kind: "phase-report", phase, values}`.
    Report {
        /// Equipment module.
        em: String,
        /// Phase.
        phase: String,
        /// Values.
        values: Vec<ValueIn>,
    },
}

/// An equipment phase of the unit, as the page sends it: `{em, phase,
/// params: [ids]}`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PhaseDeclIn {
    /// Equipment module.
    pub em: String,
    /// Phase.
    pub phase: String,
    /// The class's parameter ids, in order.
    pub params: Vec<String>,
}

/// A node's kind and properties as the page sends them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KindIn {
    /// An entry.
    Entry(EntryIn),
    /// An action.
    Action(ActionIn),
    /// A branch on a flattened condition.
    Branch(Vec<NodeIn>),
    /// A repeat.
    Repeat {
        /// Count.
        times: i64,
    },
    /// A sequence.
    Sequence {
        /// Steps.
        steps: i64,
    },
    /// A phase action.
    Phase(PhaseIn),
}

/// A node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeOwned {
    /// Id.
    pub id: String,
    /// Kind and properties.
    pub kind: KindIn,
}

/// A wire.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WireIn {
    /// Source node id.
    pub from: String,
    /// Source port.
    pub from_port: String,
    /// Target node id.
    pub to: String,
    /// Target port.
    pub to_port: String,
}

/// A flow as the page sends it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FlowIn {
    /// Identifier.
    pub id: String,
    /// Nodes.
    pub nodes: Vec<NodeOwned>,
    /// Wires.
    pub wires: Vec<WireIn>,
}

/// One issue, as the page shows it (`FlowIssue` in flows.ts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowIssueOut {
    /// Index into the flows.
    pub flow: usize,
    /// The node it is about, by index.
    pub node: Option<usize>,
    /// The wire it is about, by index.
    pub wire: Option<usize>,
    /// `"error"` or `"warning"`.
    pub severity: &'static str,
    /// What is wrong and what to do.
    pub message: String,
    /// A one-click fix.
    pub fix: Option<MachineFix>,
}

/// `"<machine>.<state>"` split at the first dot.
pub fn split_ref(v: &str) -> (String, String) {
    match v.split_once('.') {
        Some((m, s)) => (m.into(), s.into()),
        None => (v.into(), String::new()),
    }
}

fn key_text(key: &str, problem: KeyProblem) -> String {
    match problem {
        KeyProblem::Empty => {
            "This entry listens on no key; give it one, for example app/in.".into()
        }
        KeyProblem::TooLong => format!(
            "This entry listens on a key longer than {} bytes; shorten it.",
            middleware::key::MAX_KEY_LEN
        ),
        KeyProblem::EmptySegment => format!(
            "This entry listens on {key}, which has an empty part (a leading, trailing or doubled /)."
        ),
        KeyProblem::InvalidByte(at) => {
            let c = key.get(at..).and_then(|s| s.chars().next()).unwrap_or('?');
            format!(
                "This entry listens on {key}, which has {c:?}; keys use only letters, digits, _, -, . and /."
            )
        }
        KeyProblem::System => format!(
            "This entry listens on {key}, but keys under sys/ belong to the firmware; choose another key."
        ),
    }
}

/// The plain-language message for an issue of flow `f`.
fn message(f: &Flow<'_>, pins: &[PinIn], i: &FlowIssue<'_>) -> String {
    let node_id = |k: usize| f.nodes.get(k).map_or("?", |n| n.id);
    let wire_text = |k: usize| {
        f.wires.get(k).map_or_else(
            || format!("wire {}", k + 1),
            |w| format!("the wire {}.{} -> {}", w.from, w.from_port, w.to),
        )
    };
    match i.kind {
        FlowKind::TooManyFlows { count } => {
            format!("This payload has {count} flows; at most {MAX_FLOWS} are allowed. Remove this one or merge flows.")
        }
        FlowKind::IdInvalid { id: "" } => "Give this flow a name, for example Press.".into(),
        FlowKind::IdInvalid { .. } => "A flow name starts with a letter and has only letters, digits and _ (at most 24).".into(),
        FlowKind::IdDuplicate { other, id } => format!(
            "The name {id} is also used by flow {}; flow names must be unique (ignoring case).",
            other + 1
        ),
        FlowKind::TooManyNodes { count } => format!(
            "This flow has {count} nodes; at most {MAX_FLOW_NODES} are allowed. Split it into two flows."
        ),
        FlowKind::TooManyWires { count } => format!(
            "This flow has {count} wires; at most {MAX_FLOW_WIRES} are allowed. Split it into two flows."
        ),
        FlowKind::NodeIdInvalid { id } => format!(
            "The node id {id:?} must start with a letter and have only letters, digits and _."
        ),
        FlowKind::NodeIdDuplicate { other } => format!(
            "Node {} has the same id; node ids must be unique.",
            other + 1
        ),
        FlowKind::NoEntry => {
            "This flow has no entry, so it never runs; add one (on start, every, on message, on pin edge, when entered or a phase state).".into()
        }
        FlowKind::ExtraEntry { first } => format!(
            "This flow already starts at {}; a flow has one entry. Move this one to a flow of its own.",
            node_id(first)
        ),
        FlowKind::BadPeriod { ms } => format!(
            "Every {ms} ms is out of range; use {MIN_TICK_MS} ms or more (at most {} ms).",
            i32::MAX
        ),
        FlowKind::BadSubscribeKey { key, problem } => key_text(key, problem),
        FlowKind::EdgePinUnknown { label, near } => {
            if label.is_empty() {
                "Choose the input pin this entry watches.".into()
            } else if near.is_some() {
                format!("There is no pin {label}.{}", did_you_mean(near))
            } else {
                format!("This entry watches {label}, which is not in the pin table; add it as a GPIO input.")
            }
        }
        FlowKind::EdgePinNotInput { label, .. } => format!(
            "This entry watches {label}, which is {}; edges come only from GPIO inputs.",
            func_text(pins, label)
        ),
        FlowKind::EnteredMachineUnknown { machine, near } => {
            if machine.is_empty() {
                "Choose the machine and state this entry waits for.".into()
            } else if near.is_some() {
                format!("There is no state machine {machine}.{}", did_you_mean(near))
            } else {
                format!("This entry waits for {machine}, which is not a state machine; add it under States.")
            }
        }
        FlowKind::EnteredStateUnknown { machine, state, near } => {
            if state.is_empty() {
                format!("Choose the state of {machine} this entry waits for.")
            } else if near.is_some() {
                format!("{machine} has no state {state}.{}", did_you_mean(near))
            } else {
                format!("{machine} has no state {state}; add it under States or choose another.")
            }
        }
        FlowKind::Table(ref k) => {
            part_message(pins, k, |_| "This action".into()).unwrap_or_else(|| "This node has a problem.".into())
        }
        FlowKind::BadRepeat { times } => {
            format!("Repeat {times} times is out of range; use 1 to {MAX_REPEAT}.")
        }
        FlowKind::BadSequence { steps } => {
            format!("A sequence has 2 to {MAX_SEQUENCE} steps, not {steps}.")
        }
        FlowKind::WireNodeUnknown { end, id } => {
            let which = if end == End::From { "starts at" } else { "ends at" };
            format!("This wire {which} {id:?}, which is not a node of this flow; remove it.")
        }
        FlowKind::WireBadOutput { port } => {
            format!("This wire leaves from {port:?}, which its node does not have; remove it.")
        }
        FlowKind::WireBadInput { port } => {
            format!("This wire arrives at {port:?}; wires arrive at a node's input. Remove it.")
        }
        FlowKind::WireIntoEntry => "This wire arrives at the entry, which has no input; remove it.".into(),
        FlowKind::OutputWiredTwice { other } => format!(
            "This port already leads to {}; a port runs one thing. Use a sequence to run several in order.",
            wire_text(other)
        ),
        FlowKind::InputWiredTwice { other } => format!(
            "This node is already reached by {}; a node runs after one thing. For what follows both sides of a branch, use the branch's next port.",
            wire_text(other)
        ),
        FlowKind::Cycle => "This wire closes a loop, so the flow would never end; remove it (a repeat runs its body by itself).".into(),
        FlowKind::Unreachable => "Nothing leads from the entry to this node, so it never runs.".into(),
        FlowKind::RepeatEmpty => "Nothing is wired to this repeat's body, so it does nothing.".into(),
        FlowKind::BranchEmpty => "Nothing is wired to this branch's then or else, so it does nothing.".into(),
        FlowKind::PhaseUnknown { em, phase, near } => {
            if em.is_empty() || phase.is_empty() {
                "Choose the equipment phase (module.phase) this node is about.".into()
            } else if let Some((e, p)) = near {
                format!("This unit has no phase {em}.{phase}. Did you mean {e}.{p}?")
            } else {
                format!("This unit has no phase {em}.{phase}; add it to an equipment module's phases or choose another.")
            }
        }
        FlowKind::PhaseStateNotActing { state } => format!(
            "A phase body runs in an acting state (running, holding, restarting, stopping, aborting or resetting), not {state:?}."
        ),
        FlowKind::PhaseBodyTaken { other } => format!(
            "Flow {} already runs in this phase state; a phase state has one body flow. Change the state or merge the flows.",
            other + 1
        ),
        FlowKind::PhaseParamUnknown { param } => format!(
            "The phase's class has no parameter {param}; use one of its parameters or a number."
        ),
        FlowKind::BadAfter { count } => format!(
            "Done after {count} runs is out of range; use 1 to {MAX_REPEAT}."
        ),
        FlowKind::RunsOutsideBody => "Counting runs only works in the phase's own body flow (the one whose entry is this phase); finish at once here, or move this node there.".into(),
        FlowKind::TooManyValues { count } => format!(
            "This report has {count} values; a phase report carries at most {MAX_REPORT_VALUES}."
        ),
        FlowKind::ValuePinUnknown { label, near } => {
            if label.is_empty() {
                "Choose the pin this value reads.".into()
            } else if near.is_some() {
                format!("There is no pin {label}.{}", did_you_mean(near))
            } else {
                format!("This value reads {label}, which is not in the pin table; add it as an input.")
            }
        }
        FlowKind::ValuePinOutput { label } => format!(
            "This value reads {label}, which is an output; report an input or an analog pin."
        ),
    }
}

fn after_view(a: &AfterIn) -> After<'_> {
    match a {
        AfterIn::Now => After::Now,
        AfterIn::Count(n) => After::Count(*n),
        AfterIn::Param(p) => After::Param(p),
    }
}

fn value_view(v: &ValueIn) -> PhaseValue<'_> {
    match v {
        ValueIn::Const(c) => PhaseValue::Const(*c),
        ValueIn::Pin(l) => PhaseValue::Pin(l),
        ValueIn::Param(p) => PhaseValue::Param(p),
        ValueIn::Runs => PhaseValue::Runs,
    }
}

/// A flow's views: its nodes (with their conditions and report values)
/// and wires.
fn node_view<'a>(
    n: &'a NodeOwned,
    guard: &'a [Node<'a>],
    values: &'a [PhaseValue<'a>],
) -> FlowNode<'a> {
    let kind = match &n.kind {
        KindIn::Entry(e) => NodeKind::Entry(match e {
            EntryIn::Init => Entry::Init,
            EntryIn::Every { ms } => Entry::Every { ms: *ms },
            EntryIn::Message { key } => Entry::Message { key },
            EntryIn::PinEdge { label, edge } => Entry::PinEdge { label, edge: *edge },
            EntryIn::Entered { machine, state } => Entry::Entered { machine, state },
            EntryIn::Phase {
                em,
                phase,
                state,
                every,
            } => Entry::Phase {
                em,
                phase,
                state,
                every: *every,
            },
        }),
        KindIn::Action(a) => NodeKind::Action(action_view(a)),
        KindIn::Branch(_) => NodeKind::Branch(Guard { nodes: guard }),
        KindIn::Repeat { times } => NodeKind::Repeat { times: *times },
        KindIn::Sequence { steps } => NodeKind::Sequence { steps: *steps },
        KindIn::Phase(PhaseIn::Done { em, phase, after }) => NodeKind::Phase(PhaseAction::Done {
            em,
            phase,
            after: after_view(after),
        }),
        KindIn::Phase(PhaseIn::Report { em, phase, .. }) => {
            NodeKind::Phase(PhaseAction::Report { em, phase, values })
        }
    };
    FlowNode { id: &n.id, kind }
}

fn action_view(a: &ActionIn) -> Action<'_> {
    match a {
        ActionIn::Write { label, level } => Action::Write {
            label,
            level: *level,
        },
        ActionIn::Toggle { label } => Action::Toggle { label },
        ActionIn::Publish { key, text } => Action::Publish { key, text },
        ActionIn::Log { text } => Action::Log { text },
        ActionIn::Raise { machine, event } => Action::Raise {
            machine: machine.as_deref(),
            event,
        },
    }
}

/// Checks `flows` against the machines and the pin table, for a unit
/// without phases; see the module docs.
pub fn check(flows: &[FlowIn], machines: &[MachineIn], pins: &[PinIn]) -> Vec<FlowIssueOut> {
    check_in(flows, machines, pins, &[])
}

/// [`check`] for a unit with equipment phases `phases` (phase entries and
/// actions name them).
pub fn check_in(
    flows: &[FlowIn],
    machines: &[MachineIn],
    pins: &[PinIn],
    phases: &[PhaseDeclIn],
) -> Vec<FlowIssueOut> {
    // Innermost views first: conditions, then nodes and wires, then flows.
    let guards: Vec<Vec<Vec<Node<'_>>>> = flows
        .iter()
        .map(|f| {
            f.nodes
                .iter()
                .map(|n| match &n.kind {
                    KindIn::Branch(g) => g.iter().map(NodeIn::view).collect(),
                    _ => Vec::new(),
                })
                .collect()
        })
        .collect();
    let values: Vec<Vec<Vec<PhaseValue<'_>>>> = flows
        .iter()
        .map(|f| {
            f.nodes
                .iter()
                .map(|n| match &n.kind {
                    KindIn::Phase(PhaseIn::Report { values, .. }) => {
                        values.iter().map(value_view).collect()
                    }
                    _ => Vec::new(),
                })
                .collect()
        })
        .collect();
    let nodes: Vec<Vec<FlowNode<'_>>> = flows
        .iter()
        .zip(&guards)
        .zip(&values)
        .map(|((f, gs), vs)| {
            f.nodes
                .iter()
                .zip(gs)
                .zip(vs)
                .map(|((n, g), v)| node_view(n, g, v))
                .collect()
        })
        .collect();
    let params: Vec<Vec<&str>> = phases
        .iter()
        .map(|p| p.params.iter().map(String::as_str).collect())
        .collect();
    let decls: Vec<PhaseDecl<'_>> = phases
        .iter()
        .zip(&params)
        .map(|(p, ps)| PhaseDecl {
            em: &p.em,
            phase: &p.phase,
            params: ps,
        })
        .collect();
    let wires: Vec<Vec<Wire<'_>>> = flows
        .iter()
        .map(|f| {
            f.wires
                .iter()
                .map(|w| Wire {
                    from: &w.from,
                    from_port: &w.from_port,
                    to: &w.to,
                    to_port: &w.to_port,
                })
                .collect()
        })
        .collect();
    let views: Vec<Flow<'_>> = flows
        .iter()
        .enumerate()
        .map(|(i, f)| Flow {
            id: &f.id,
            nodes: nodes.get(i).map_or(&[], Vec::as_slice),
            wires: wires.get(i).map_or(&[], Vec::as_slice),
        })
        .collect();
    with_views(machines, pins, |ms, ps| {
        let mut out = Vec::new();
        check_flows_in(&views, ms, ps, &decls, key_problem, &mut |i| {
            let Some(f) = views.get(i.flow) else { return };
            let (node, wire) = match i.at {
                At::Flow => (None, None),
                At::Node(n) => (Some(n), None),
                At::Wire(w) => (None, Some(w)),
            };
            out.push(FlowIssueOut {
                flow: i.flow,
                node,
                wire,
                severity: severity_name(i.severity),
                message: message(f, pins, &i),
                fix: i.fix.map(fix),
            });
        });
        out
    })
}

#[cfg(test)]
mod tests;
