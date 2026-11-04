//! Flow graphs: what a flow may contain, checked like the tables.
//!
//! A flow (`web/src/model/flows.ts`) is a small control-flow graph drawn
//! on the web editor's canvas: one **entry** node (run in `init`, every
//! N ms, on a message, on a pin edge, or when a machine enters a state),
//! then **action** nodes (the tables' [`Action`]s, a raise naming its
//! machine), **branch** nodes (a [`Guard`], the tables' condition grammar
//! with state tests), **repeat** nodes (a constant count of at most
//! [`MAX_REPEAT`]) and **sequence** nodes (2 to [`MAX_SEQUENCE`] steps run
//! in order). Wires join an output port of one node to the `in` port of
//! another.
//!
//! Every output port starts a chain that runs to its end and then returns
//! to the node it left: `next` continues after an action, a branch (after
//! `then` or `else`) or a repeat (after `body` ran its count of times); a
//! sequence runs its steps `1`, `2`, ... in order. A repeat's body loops
//! by itself, so a flow has **no** back wires: the graph must be acyclic.
//! With each input wired at most once the graph is a tree under its entry,
//! which is what makes lowering it to straight-line code (an `if` per
//! branch, a bounded `for` per repeat) direct and bounded.
//!
//! [`check_flows`] reports, as [`FlowIssue`]s (flow index, node or wire
//! index, severity, [`FlowKind`], fix):
//!
//! * at most [`MAX_FLOWS`] flows; a flow id is an identifier, unique
//!   ignoring case; at most [`MAX_FLOW_NODES`] nodes and
//!   [`MAX_FLOW_WIRES`] wires (over a limit, the size is reported and the
//!   rest of the flow is not checked); node ids are identifiers, unique;
//! * exactly one entry (none: an error on the flow; more: an error on
//!   each later one);
//! * an `every` period from [`MIN_TICK_MS`] to `i32::MAX` ms; a message
//!   key by the key rule and not under `sys/`; a pin-edge label that is a
//!   `gpio-in` row (fix: add it); an `entered` state of an existing
//!   machine; repeat counts 1..=[`MAX_REPEAT`]; sequence steps
//!   2..=[`MAX_SEQUENCE`];
//! * actions and branch conditions by the tables' own rules
//!   ([`crate::check::action_issues`], [`crate::check::guard_issues`]),
//!   except that a raise must name its machine;
//! * a wire names existing nodes, an output port the source node has and
//!   the `in` port of a node that is not an entry; each output port and
//!   each input is wired at most once (reported on the later wire);
//! * a cycle is an error on each of its wires;
//! * warnings: a node the entry does not lead to never runs; a repeat with
//!   no body and a branch with neither `then` nor `else` do nothing.
//!
//! No heap and no recursion: node and wire indices live in fixed arrays of
//! [`MAX_FLOW_NODES`] and [`MAX_FLOW_WIRES`] entries, cycles are found by
//! peeling nodes with no incoming wire (at most one pass per node) and
//! reachability by relaxing wires (at most one pass per node).

use crate::check::{Fix, IssueKind, KeyProblem, Severity, action_issues, guard_issues, key_issue};
use crate::{Action, Guard, Machine, Pin, PinUse, ident_ok};

/// Most flows in a payload.
pub const MAX_FLOWS: usize = 8;
/// Most nodes per flow.
pub const MAX_FLOW_NODES: usize = 64;
/// Most wires per flow.
pub const MAX_FLOW_WIRES: usize = 128;
/// Most iterations of a repeat node (the generator's loop bound).
pub const MAX_REPEAT: i64 = 10_000;
/// Most steps of a sequence node.
pub const MAX_SEQUENCE: i64 = 4;
/// Shortest `every` period (wasm_abi::MIN_TICK_MS).
pub const MIN_TICK_MS: i64 = 10;

/// Which pin edges a pin-edge entry runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    /// Low to high.
    Rise,
    /// High to low.
    Fall,
    /// Either.
    Both,
}

/// What starts a flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry<'a> {
    /// Once, in `init`.
    Init,
    /// Every `ms` milliseconds.
    Every {
        /// The period.
        ms: i64,
    },
    /// On each message on `key`.
    Message {
        /// The subscribed key.
        key: &'a str,
    },
    /// On an edge of the pin with this label.
    PinEdge {
        /// The pin-table label (a `gpio-in` row).
        label: &'a str,
        /// Which edges.
        edge: Edge,
    },
    /// When machine `machine` enters `state` (after its entry actions).
    Entered {
        /// The machine's id.
        machine: &'a str,
        /// The state's name.
        state: &'a str,
    },
}

/// What a node does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind<'a> {
    /// The flow's start.
    Entry(Entry<'a>),
    /// One action.
    Action(Action<'a>),
    /// `then` when the condition is non-zero, else `else`; then `next`.
    Branch(Guard<'a>),
    /// `body` `times` times; then `next`.
    Repeat {
        /// The count.
        times: i64,
    },
    /// Steps `1`, `2`, ... in order.
    Sequence {
        /// How many steps.
        steps: i64,
    },
}

const SEQ_PORTS: [&str; MAX_SEQUENCE as usize] = ["1", "2", "3", "4"];

impl NodeKind<'_> {
    /// The output ports, in the order they run.
    pub fn outputs(&self) -> &'static [&'static str] {
        match *self {
            NodeKind::Entry(_) | NodeKind::Action(_) => &["next"],
            NodeKind::Branch(_) => &["then", "else", "next"],
            NodeKind::Repeat { .. } => &["body", "next"],
            NodeKind::Sequence { steps } => {
                let n = usize::try_from(steps.clamp(2, MAX_SEQUENCE)).unwrap_or(2);
                SEQ_PORTS.get(..n).unwrap_or(&SEQ_PORTS)
            }
        }
    }

    /// Whether the node has an `in` port (every node but the entry).
    pub const fn has_input(&self) -> bool {
        !matches!(self, NodeKind::Entry(_))
    }
}

/// The input port's name.
pub const IN_PORT: &str = "in";

/// A node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlowNode<'a> {
    /// Identifier, unique in the flow (names it in the `flow` log line).
    pub id: &'a str,
    /// What it does.
    pub kind: NodeKind<'a>,
}

/// A wire from an output port to an input port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wire<'a> {
    /// Source node id.
    pub from: &'a str,
    /// Source port.
    pub from_port: &'a str,
    /// Target node id.
    pub to: &'a str,
    /// Target port (`in`).
    pub to_port: &'a str,
}

/// A flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flow<'a> {
    /// Identifier, unique ignoring case.
    pub id: &'a str,
    /// The nodes.
    pub nodes: &'a [FlowNode<'a>],
    /// The wires.
    pub wires: &'a [Wire<'a>],
}

/// Where in a flow an issue is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum At {
    /// The flow as a whole.
    Flow,
    /// A node, by index.
    Node(usize),
    /// A wire, by index.
    Wire(usize),
}

/// Which end of a wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// The source.
    From,
    /// The target.
    To,
}

/// What is wrong with a flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowKind<'a> {
    /// More than [`MAX_FLOWS`] flows (on each flow past the limit).
    TooManyFlows {
        /// How many.
        count: usize,
    },
    /// The flow id is not an identifier.
    IdInvalid {
        /// The id.
        id: &'a str,
    },
    /// Another flow has the same id ignoring case.
    IdDuplicate {
        /// That flow's index.
        other: usize,
        /// The id.
        id: &'a str,
    },
    /// More than [`MAX_FLOW_NODES`] nodes.
    TooManyNodes {
        /// How many.
        count: usize,
    },
    /// More than [`MAX_FLOW_WIRES`] wires.
    TooManyWires {
        /// How many.
        count: usize,
    },
    /// A node id is not an identifier.
    NodeIdInvalid {
        /// The id.
        id: &'a str,
    },
    /// Another node has the same id.
    NodeIdDuplicate {
        /// That node's index.
        other: usize,
    },
    /// The flow has no entry node.
    NoEntry,
    /// A second entry node.
    ExtraEntry {
        /// The first entry's index.
        first: usize,
    },
    /// An `every` period outside [`MIN_TICK_MS`]..=`i32::MAX`.
    BadPeriod {
        /// The period.
        ms: i64,
    },
    /// A message entry's key is refused.
    BadSubscribeKey {
        /// The key.
        key: &'a str,
        /// Why.
        problem: KeyProblem,
    },
    /// A pin-edge label is not in the pin table.
    EdgePinUnknown {
        /// The label.
        label: &'a str,
        /// A pin whose label differs only in case.
        near: Option<&'a str>,
    },
    /// A pin-edge label is not a GPIO input.
    EdgePinNotInput {
        /// The label.
        label: &'a str,
        /// What it is.
        has: PinUse,
    },
    /// An `entered` entry names no machine.
    EnteredMachineUnknown {
        /// The id given.
        machine: &'a str,
        /// A machine whose id differs only in case.
        near: Option<&'a str>,
    },
    /// An `entered` entry names no state of its machine.
    EnteredStateUnknown {
        /// The machine's id.
        machine: &'a str,
        /// The name given.
        state: &'a str,
        /// A state whose name differs only in case.
        near: Option<&'a str>,
    },
    /// An action's or a branch condition's problem, by the tables' rules
    /// (the action index in it is always 0).
    Table(IssueKind<'a>),
    /// A repeat count outside 1..=[`MAX_REPEAT`].
    BadRepeat {
        /// The count.
        times: i64,
    },
    /// A sequence step count outside 2..=[`MAX_SEQUENCE`].
    BadSequence {
        /// The count.
        steps: i64,
    },
    /// A wire end names no node.
    WireNodeUnknown {
        /// Which end.
        end: End,
        /// The id given.
        id: &'a str,
    },
    /// A wire leaves from a port its node does not have.
    WireBadOutput {
        /// The port given.
        port: &'a str,
    },
    /// A wire arrives at a port other than `in`.
    WireBadInput {
        /// The port given.
        port: &'a str,
    },
    /// A wire arrives at the entry, which has no input.
    WireIntoEntry,
    /// The output port is already wired by an earlier wire.
    OutputWiredTwice {
        /// The earlier wire.
        other: usize,
    },
    /// The input is already wired by an earlier wire.
    InputWiredTwice {
        /// The earlier wire.
        other: usize,
    },
    /// The wire is part of a cycle.
    Cycle,
    /// Warning: nothing leads from the entry to this node.
    Unreachable,
    /// Warning: the repeat's body is not wired.
    RepeatEmpty,
    /// Warning: neither `then` nor `else` is wired.
    BranchEmpty,
}

/// One problem with a flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlowIssue<'a> {
    /// Index into the flows.
    pub flow: usize,
    /// Where.
    pub at: At,
    /// Error or warning.
    pub severity: Severity,
    /// What is wrong.
    pub kind: FlowKind<'a>,
    /// A one-click fix, when there is an obvious one.
    pub fix: Option<Fix<'a>>,
}

/// Checks `flows` against the machines and the pin table, calling `sink`
/// for each issue in order (flow by flow). `keys` validates key syntax
/// like [`crate::check()`]'s.
pub fn check_flows<'a>(
    flows: &[Flow<'a>],
    machines: &[Machine<'a>],
    pins: &[Pin<'a>],
    keys: impl Fn(&str) -> Result<(), KeyProblem>,
    sink: &mut impl FnMut(FlowIssue<'a>),
) {
    for (fi, f) in flows.iter().enumerate() {
        let mut out = |at: At, severity: Severity, kind: FlowKind<'a>, fix: Option<Fix<'a>>| {
            sink(FlowIssue {
                flow: fi,
                at,
                severity,
                kind,
                fix,
            });
        };
        if fi >= MAX_FLOWS {
            out(
                At::Flow,
                Severity::Error,
                FlowKind::TooManyFlows { count: flows.len() },
                None,
            );
            continue;
        }
        check_flow(flows, fi, f, machines, pins, &keys, &mut out);
    }
}

fn near<'a, T>(items: &[T], name: &str, of: impl Fn(&T) -> &'a str) -> Option<&'a str> {
    items.iter().map(of).find(|n| n.eq_ignore_ascii_case(name))
}

fn check_flow<'a>(
    flows: &[Flow<'a>],
    fi: usize,
    f: &Flow<'a>,
    machines: &[Machine<'a>],
    pins: &[Pin<'a>],
    keys: &impl Fn(&str) -> Result<(), KeyProblem>,
    out: &mut impl FnMut(At, Severity, FlowKind<'a>, Option<Fix<'a>>),
) {
    use Severity::{Error, Warning};
    if !ident_ok(f.id) {
        out(At::Flow, Error, FlowKind::IdInvalid { id: f.id }, None);
    } else if let Some(other) = flows
        .iter()
        .take(MAX_FLOWS)
        .enumerate()
        .position(|(j, o)| j != fi && o.id.eq_ignore_ascii_case(f.id))
    {
        out(
            At::Flow,
            Error,
            FlowKind::IdDuplicate { other, id: f.id },
            None,
        );
    }
    let mut too_big = false;
    if f.nodes.len() > MAX_FLOW_NODES {
        out(
            At::Flow,
            Error,
            FlowKind::TooManyNodes {
                count: f.nodes.len(),
            },
            None,
        );
        too_big = true;
    }
    if f.wires.len() > MAX_FLOW_WIRES {
        out(
            At::Flow,
            Error,
            FlowKind::TooManyWires {
                count: f.wires.len(),
            },
            None,
        );
        too_big = true;
    }
    if too_big {
        return;
    }

    // Nodes: ids, one entry, properties.
    let mut entry: Option<usize> = None;
    for (ni, n) in f.nodes.iter().enumerate() {
        let at = At::Node(ni);
        if !ident_ok(n.id) {
            out(at, Error, FlowKind::NodeIdInvalid { id: n.id }, None);
        } else if let Some(other) = f.nodes.iter().take(ni).position(|o| o.id == n.id) {
            out(at, Error, FlowKind::NodeIdDuplicate { other }, None);
        }
        match n.kind {
            NodeKind::Entry(e) => {
                if let Some(first) = entry {
                    out(at, Error, FlowKind::ExtraEntry { first }, None);
                } else {
                    entry = Some(ni);
                }
                check_entry(e, machines, pins, keys, &mut |kind, fix| {
                    out(at, Error, kind, fix)
                });
            }
            NodeKind::Action(a) => {
                action_issues(machines, None, 0, &a, pins, keys, &mut |kind, fix| {
                    out(at, Error, FlowKind::Table(kind), fix);
                });
            }
            NodeKind::Branch(g) => guard_issues(machines, &g, pins, &mut |kind, fix| {
                out(at, Error, FlowKind::Table(kind), fix);
            }),
            NodeKind::Repeat { times } => {
                if !(1..=MAX_REPEAT).contains(&times) {
                    out(at, Error, FlowKind::BadRepeat { times }, None);
                }
            }
            NodeKind::Sequence { steps } => {
                if !(2..=MAX_SEQUENCE).contains(&steps) {
                    out(at, Error, FlowKind::BadSequence { steps }, None);
                }
            }
        }
    }
    if entry.is_none() {
        out(At::Flow, Error, FlowKind::NoEntry, None);
    }

    // Wires: ends, ports, at most one wire per output port and per input.
    let index = |id: &str| f.nodes.iter().position(|n| n.id == id);
    // The valid wires as (from node, from port index, to node).
    let mut valid: [Option<(usize, usize, usize)>; MAX_FLOW_WIRES] = [None; MAX_FLOW_WIRES];
    let mut out_wire: [[Option<usize>; MAX_SEQUENCE as usize]; MAX_FLOW_NODES] =
        [[None; MAX_SEQUENCE as usize]; MAX_FLOW_NODES];
    let mut in_wire: [Option<usize>; MAX_FLOW_NODES] = [None; MAX_FLOW_NODES];
    for (wi, w) in f.wires.iter().enumerate() {
        let at = At::Wire(wi);
        let (from, to) = (index(w.from), index(w.to));
        for (end, id, found) in [(End::From, w.from, from), (End::To, w.to, to)] {
            if found.is_none() {
                out(at, Error, FlowKind::WireNodeUnknown { end, id }, None);
            }
        }
        let (Some(from), Some(to)) = (from, to) else {
            continue;
        };
        let (Some(src), Some(dst)) = (f.nodes.get(from), f.nodes.get(to)) else {
            continue;
        };
        let Some(port) = src.kind.outputs().iter().position(|p| *p == w.from_port) else {
            out(
                at,
                Error,
                FlowKind::WireBadOutput { port: w.from_port },
                None,
            );
            continue;
        };
        if !dst.kind.has_input() {
            out(at, Error, FlowKind::WireIntoEntry, None);
            continue;
        }
        if w.to_port != IN_PORT {
            out(at, Error, FlowKind::WireBadInput { port: w.to_port }, None);
            continue;
        }
        let out_slot = out_wire.get_mut(from).and_then(|ports| ports.get_mut(port));
        if let Some(Some(other)) = out_slot.as_deref() {
            out(
                at,
                Error,
                FlowKind::OutputWiredTwice { other: *other },
                None,
            );
            continue;
        }
        if let Some(Some(other)) = in_wire.get(to) {
            out(at, Error, FlowKind::InputWiredTwice { other: *other }, None);
            continue;
        }
        if let Some(slot) = out_slot {
            *slot = Some(wi);
        }
        if let Some(slot) = in_wire.get_mut(to) {
            *slot = Some(wi);
        }
        if let Some(slot) = valid.get_mut(wi) {
            *slot = Some((from, port, to));
        }
    }
    let n = f.nodes.len();
    let edges = || valid.iter().take(f.wires.len()).flatten().copied();

    // Cycles: peel nodes with no remaining incoming wire; what is left is
    // on a cycle or after one. Each pass peels at least one node or stops.
    let mut peeled: u64 = 0;
    for _ in 0..n {
        let before = peeled;
        for v in 0..n {
            let bit = 1u64 << v;
            if peeled & bit != 0 {
                continue;
            }
            let fed = edges().any(|(a, _, b)| b == v && peeled & (1u64 << a) == 0);
            if !fed {
                peeled |= bit;
            }
        }
        if peeled == before {
            break;
        }
    }
    // Each node left has exactly one incoming valid wire (inputs are
    // wired at most once); following them back n steps lands on a cycle,
    // whose members are then marked by walking it once more.
    let pred = |v: usize| -> Option<usize> {
        in_wire
            .get(v)
            .copied()
            .flatten()
            .and_then(|w| valid.get(w).copied().flatten())
            .map(|(a, _, _)| a)
    };
    let mut on_cycle: u64 = 0;
    for v in 0..n {
        if peeled & (1u64 << v) != 0 || on_cycle & (1u64 << v) != 0 {
            continue;
        }
        let mut u = v;
        for _ in 0..n {
            u = pred(u).unwrap_or(u);
        }
        let mut w = u;
        for _ in 0..n {
            on_cycle |= 1u64 << w;
            w = pred(w).unwrap_or(u);
            if w == u {
                break;
            }
        }
    }
    for (wi, e) in valid.iter().take(f.wires.len()).enumerate() {
        if let Some((a, _, b)) = *e
            && on_cycle & (1u64 << a) != 0
            && on_cycle & (1u64 << b) != 0
        {
            out(At::Wire(wi), Error, FlowKind::Cycle, None);
        }
    }

    // Reachability from the entry over valid wires.
    let Some(start) = entry else { return };
    let mut reached: u64 = 1u64 << start;
    for _ in 0..n {
        let before = reached;
        for (a, _, b) in edges() {
            if reached & (1u64 << a) != 0 {
                reached |= 1u64 << b;
            }
        }
        if reached == before {
            break;
        }
    }
    for (ni, node) in f.nodes.iter().enumerate() {
        let at = At::Node(ni);
        let wired = |port: usize| {
            out_wire
                .get(ni)
                .and_then(|p| p.get(port))
                .copied()
                .flatten()
                .is_some()
        };
        if reached & (1u64 << ni) == 0 && !matches!(node.kind, NodeKind::Entry(_)) {
            out(at, Warning, FlowKind::Unreachable, None);
            continue;
        }
        match node.kind {
            NodeKind::Repeat { .. } if !wired(0) => out(at, Warning, FlowKind::RepeatEmpty, None),
            NodeKind::Branch(_) if !wired(0) && !wired(1) => {
                out(at, Warning, FlowKind::BranchEmpty, None);
            }
            _ => {}
        }
    }
}

fn check_entry<'a>(
    e: Entry<'a>,
    machines: &[Machine<'a>],
    pins: &[Pin<'a>],
    keys: &impl Fn(&str) -> Result<(), KeyProblem>,
    emit: &mut impl FnMut(FlowKind<'a>, Option<Fix<'a>>),
) {
    match e {
        Entry::Init => {}
        Entry::Every { ms } => {
            if !(MIN_TICK_MS..=i64::from(i32::MAX)).contains(&ms) {
                emit(FlowKind::BadPeriod { ms }, None);
            }
        }
        Entry::Message { key } => {
            if let Some(problem) = key_issue(key, keys) {
                emit(FlowKind::BadSubscribeKey { key, problem }, None);
            }
        }
        Entry::PinEdge { label, .. } => match pins.iter().find(|p| p.label == label) {
            Some(p) if p.func != PinUse::Input => {
                emit(FlowKind::EdgePinNotInput { label, has: p.func }, None);
            }
            Some(_) => {}
            None => {
                let near = near(pins, label, |p| p.label);
                let fix = (near.is_none() && ident_ok(label)).then_some(Fix::AddPin {
                    label,
                    func: PinUse::Input,
                });
                emit(FlowKind::EdgePinUnknown { label, near }, fix);
            }
        },
        Entry::Entered { machine, state } => match machines.iter().find(|m| m.id == machine) {
            None => emit(
                FlowKind::EnteredMachineUnknown {
                    machine,
                    near: near(machines, machine, |m| m.id),
                },
                None,
            ),
            Some(m) if m.state_index(state).is_none() => emit(
                FlowKind::EnteredStateUnknown {
                    machine,
                    state,
                    near: near(m.states, state, |s| s.name),
                },
                None,
            ),
            Some(_) => {}
        },
    }
}

#[cfg(test)]
mod tests;
