//! The state-machine tables' checks for the web editor: owned machines as
//! the page sends them, checked by [`state_table::check`] against the pin
//! table, with every issue rendered in plain language.
//!
//! The web model (`web/src/model/machines.ts`) owns a table's shape; the
//! `state-table` crate owns what is allowed in it. This module converts
//! owned values ([`MachineIn`], [`PinIn`]) into that crate's borrowed
//! views, flattens nested guard expressions ([`Expr`]) into its arena form
//! ([`flatten`], bounded by [`MAX_GUARD_DEPTH`]: anything deeper becomes a
//! leaf at depth 5, which the check reports), validates publish keys with
//! the middleware crate's [`Key::new`], and turns each structured issue
//! into a [`MachineIssue`]: where (machine, list, index, field), how bad,
//! a message naming the state, event, label or row (rows numbered from 1)
//! and saying what to do, and a [`MachineFix`] where one exists.
//!
//! Everything here is pure; `crate::bindings` converts JS objects to
//! [`MachineIn`] and [`MachineIssue`]s back.

use crate::pins::{Func, MAX_LABEL};
use middleware::{Key, KeyError};
use state_table::{
    Action, CmpOp, Event, Field, Fix, Guard, GuardError, Issue, IssueKind, KeyProblem, List,
    MAX_ACTIONS, MAX_EVENTS, MAX_GUARD_DEPTH, MAX_GUARD_NODES, MAX_OVERLAP_PINS, MAX_ROWS,
    MAX_STATES, MAX_TEXT, Machine, Node, Pin, PinUse, Row, Severity, State,
};

/// A guard expression as the page nests it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    /// A digital pin read by label.
    Pin(String),
    /// A constant.
    Const(i32),
    /// A comparison.
    Cmp(CmpOp, Box<Expr>, Box<Expr>),
    /// Both.
    And(Box<Expr>, Box<Expr>),
    /// Either.
    Or(Box<Expr>, Box<Expr>),
    /// Not.
    Not(Box<Expr>),
}

/// An arena node with an owned label (see [`state_table::Node`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeIn {
    /// A pin read.
    Pin(String),
    /// A constant.
    Const(i32),
    /// A comparison of two earlier nodes.
    Cmp(CmpOp, usize, usize),
    /// Both earlier nodes.
    And(usize, usize),
    /// Either earlier node.
    Or(usize, usize),
    /// Not an earlier node.
    Not(usize),
}

impl NodeIn {
    fn view(&self) -> Node<'_> {
        match *self {
            NodeIn::Pin(ref l) => Node::Pin(l),
            NodeIn::Const(c) => Node::Const(c),
            NodeIn::Cmp(op, a, b) => Node::Cmp { op, a, b },
            NodeIn::And(a, b) => Node::And(a, b),
            NodeIn::Or(a, b) => Node::Or(a, b),
            NodeIn::Not(a) => Node::Not(a),
        }
    }
}

/// `e` as an arena (children first, root last). Recursion stops at depth
/// `MAX_GUARD_DEPTH + 1`: a deeper subtree becomes a `Const(0)` leaf
/// there, so the arena is exactly as deep as the limit plus one and the
/// check reports it.
pub fn flatten(e: &Expr) -> Vec<NodeIn> {
    fn go(e: &Expr, depth: u8, out: &mut Vec<NodeIn>) -> usize {
        let node = if depth > MAX_GUARD_DEPTH {
            NodeIn::Const(0)
        } else {
            match e {
                Expr::Pin(l) => NodeIn::Pin(l.clone()),
                Expr::Const(c) => NodeIn::Const(*c),
                Expr::Cmp(op, a, b) => {
                    let (a, b) = (go(a, depth + 1, out), go(b, depth + 1, out));
                    NodeIn::Cmp(*op, a, b)
                }
                Expr::And(a, b) => {
                    let (a, b) = (go(a, depth + 1, out), go(b, depth + 1, out));
                    NodeIn::And(a, b)
                }
                Expr::Or(a, b) => {
                    let (a, b) = (go(a, depth + 1, out), go(b, depth + 1, out));
                    NodeIn::Or(a, b)
                }
                Expr::Not(a) => NodeIn::Not(go(a, depth + 1, out)),
            }
        };
        out.push(node);
        out.len() - 1
    }
    let mut out = Vec::new();
    go(e, 1, &mut out);
    out
}

/// An action as the page sends it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionIn {
    /// Write a pin.
    Write {
        /// Pin label.
        label: String,
        /// High or low.
        level: bool,
    },
    /// Toggle a pin.
    Toggle {
        /// Pin label.
        label: String,
    },
    /// Publish text.
    Publish {
        /// Key.
        key: String,
        /// Text.
        text: String,
    },
    /// Log text.
    Log {
        /// Text.
        text: String,
    },
    /// Raise an event.
    Raise {
        /// Event name.
        event: String,
    },
}

impl ActionIn {
    fn view(&self) -> Action<'_> {
        match self {
            ActionIn::Write { label, level } => Action::Write {
                label,
                level: *level,
            },
            ActionIn::Toggle { label } => Action::Toggle { label },
            ActionIn::Publish { key, text } => Action::Publish { key, text },
            ActionIn::Log { text } => Action::Log { text },
            ActionIn::Raise { event } => Action::Raise { event },
        }
    }
}

/// A state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StateIn {
    /// Name.
    pub name: String,
    /// Entry actions.
    pub entry: Vec<ActionIn>,
}

/// A transition row.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RowIn {
    /// State it fires in.
    pub state: String,
    /// Event it fires on.
    pub event: String,
    /// Guard arena ([`flatten`]), or `None`.
    pub guard: Option<Vec<NodeIn>>,
    /// State it moves to.
    pub next: String,
    /// Actions.
    pub actions: Vec<ActionIn>,
}

/// A machine as the page sends it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MachineIn {
    /// Identifier.
    pub id: String,
    /// Initial state's name.
    pub initial: String,
    /// States.
    pub states: Vec<StateIn>,
    /// Event names.
    pub events: Vec<String>,
    /// Rows.
    pub rows: Vec<RowIn>,
}

/// A pin-table row (label and function are what the check reads).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinIn {
    /// Label.
    pub label: String,
    /// Function, or `Err(name)` for a name `pins` does not know.
    pub func: Result<Func, String>,
}

/// A fix the page offers as a button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MachineFix {
    /// `{kind: "add-state", name}`.
    AddState(String),
    /// `{kind: "add-event", name}`.
    AddEvent(String),
    /// `{kind: "add-pin", label, func}`.
    AddPin {
        /// Label.
        label: String,
        /// `gpio-in` or `gpio-out`.
        func: Func,
    },
    /// `{kind: "set-initial", name}`.
    SetInitial(String),
}

/// One issue, as the page shows it (`MachineIssue` in machines.ts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineIssue {
    /// Index into the machines.
    pub machine: usize,
    /// `"machine"`, `"states"`, `"events"` or `"rows"`.
    pub list: &'static str,
    /// Index into the list, or `None` for the machine.
    pub row: Option<usize>,
    /// The field's model name.
    pub field: &'static str,
    /// `"error"` or `"warning"`.
    pub severity: &'static str,
    /// What is wrong and what to do.
    pub message: String,
    /// A one-click fix.
    pub fix: Option<MachineFix>,
}

fn list_name(l: List) -> &'static str {
    match l {
        List::Machine => "machine",
        List::States => "states",
        List::Events => "events",
        List::Rows => "rows",
    }
}

fn field_name(f: Field) -> &'static str {
    match f {
        Field::Id => "id",
        Field::Name => "name",
        Field::Initial => "initial",
        Field::State => "state",
        Field::Event => "event",
        Field::Guard => "guard",
        Field::Next => "next",
        Field::Actions => "actions",
        Field::Entry => "entry",
        Field::Size => "size",
    }
}

fn severity_name(s: Severity) -> &'static str {
    match s {
        Severity::Error => "error",
        Severity::Warning => "warning",
    }
}

fn pin_use(f: &Result<Func, String>) -> PinUse {
    match f {
        Ok(Func::GpioOut) => PinUse::Output,
        Ok(Func::GpioIn) => PinUse::Input,
        _ => PinUse::Other,
    }
}

/// "a GPIO input", "an analog input", ... for the pin with this label.
fn func_text(pins: &[PinIn], label: &str) -> &'static str {
    match pins.iter().find(|p| p.label == label).map(|p| &p.func) {
        Some(Ok(Func::GpioOut)) => "a GPIO output",
        Some(Ok(Func::GpioIn)) => "a GPIO input",
        Some(Ok(Func::Adc)) => "an analog input",
        Some(Ok(Func::Pwm)) => "a PWM output",
        Some(Ok(Func::I2cSda | Func::I2cScl)) => "an I2C pin",
        Some(Ok(Func::SpiSck | Func::SpiMosi | Func::SpiMiso | Func::SpiCs)) => "an SPI pin",
        _ => "a pin of unknown function",
    }
}

fn key_problem(key: &str) -> Result<(), KeyProblem> {
    Key::new(key).map(|_| ()).map_err(|e| match e {
        KeyError::Empty => KeyProblem::Empty,
        KeyError::TooLong => KeyProblem::TooLong,
        KeyError::EmptySegment => KeyProblem::EmptySegment,
        KeyError::InvalidByte(i) => KeyProblem::InvalidByte(i),
    })
}

fn did_you_mean(near: Option<&str>) -> String {
    near.map(|n| format!(" Did you mean {n}?"))
        .unwrap_or_default()
}

/// "Action 2 " for an action of a row or an entry.
fn action_text(k: usize) -> String {
    format!("Action {}", k + 1)
}

fn ident_rule(what: &str) -> String {
    format!("{what} starts with a letter and has only letters, digits and _ (at most {MAX_LABEL}).")
}

/// The plain-language message for an issue of machine `m`.
fn message(m: &Machine<'_>, pins: &[PinIn], i: &Issue<'_>) -> String {
    let row_cell = |r: usize| {
        m.rows
            .get(r)
            .map(|row| (row.state, row.event))
            .unwrap_or_default()
    };
    let in_entry = i.list == List::States;
    match i.kind {
        IssueKind::IdInvalid { id: "" } => "Give this machine an id, for example Mode.".into(),
        IssueKind::IdInvalid { .. } => ident_rule("A machine id"),
        IssueKind::IdDuplicate { other, id } => format!(
            "The id {id} is also used by machine {}; machine ids must be unique (ignoring case).",
            other + 1
        ),
        IssueKind::NoStates => "Add a state; a machine needs at least one.".into(),
        IssueKind::TooManyStates { count } => format!(
            "This machine has {count} states; at most {MAX_STATES} are allowed. Remove some or split the machine."
        ),
        IssueKind::TooManyEvents { count } => format!(
            "This machine has {count} events; at most {MAX_EVENTS} are allowed. Remove some or split the machine."
        ),
        IssueKind::TooManyRows { count } => format!(
            "This machine has {count} rows; at most {MAX_ROWS} are allowed. Remove some or split the machine."
        ),
        IssueKind::NameInvalid { name } => {
            let (what, article, example) = if i.list == List::Events {
                ("event", "An", "PRESS")
            } else {
                ("state", "A", "Idle")
            };
            if name.is_empty() {
                format!("Give this {what} a name, for example {example}.")
            } else {
                ident_rule(&format!("{article} {what} name"))
            }
        }
        IssueKind::NameDuplicate { other, name } => {
            let what = if i.list == List::Events {
                "event"
            } else {
                "state"
            };
            format!(
                "The name {name} is also used by {what} {}; {what} names must be unique (ignoring case).",
                other + 1
            )
        }
        IssueKind::InitialUnknown { name: "", first } => {
            format!("Choose the initial state, for example {first}.")
        }
        IssueKind::InitialUnknown { name, first } => format!(
            "The initial state {name} is not a state of this machine; choose one, for example {first}."
        ),
        IssueKind::UnknownState { name, near } => {
            let role = if i.field == Field::Next {
                "moves to"
            } else {
                "fires in"
            };
            if name.is_empty() {
                format!("Choose the state this row {role}.")
            } else if near.is_some() {
                format!("There is no state {name}.{}", did_you_mean(near))
            } else {
                format!("There is no state {name}; add it or choose another.")
            }
        }
        IssueKind::UnknownEvent { name, near } => {
            if name.is_empty() {
                "Choose the event this row fires on.".into()
            } else if near.is_some() {
                format!("There is no event {name}.{}", did_you_mean(near))
            } else {
                format!("There is no event {name}; add it or choose another.")
            }
        }
        IssueKind::GuardMalformed(GuardError::TooDeep { .. }) => format!(
            "This condition is nested more than {MAX_GUARD_DEPTH} levels deep; simplify it."
        ),
        IssueKind::GuardMalformed(GuardError::TooLarge { nodes }) => format!(
            "This condition has {nodes} parts; at most {MAX_GUARD_NODES} are allowed. Simplify it."
        ),
        IssueKind::GuardMalformed(GuardError::Empty | GuardError::BadChild { .. }) => {
            "This condition is malformed; replace it.".into()
        }
        IssueKind::UnknownPin {
            action: None,
            label,
            near,
        } => {
            if label.is_empty() {
                "Choose a pin for this condition.".into()
            } else if near.is_some() {
                format!("There is no pin {label}.{}", did_you_mean(near))
            } else {
                format!(
                    "The condition reads {label}, which is not in the pin table; add it as a GPIO input."
                )
            }
        }
        IssueKind::UnknownPin {
            action: Some(k),
            label,
            near,
        } => {
            let a = action_text(k);
            if label.is_empty() {
                format!("{a}: choose a pin.")
            } else if near.is_some() {
                format!("{a}: there is no pin {label}.{}", did_you_mean(near))
            } else {
                format!(
                    "{a} drives {label}, which is not in the pin table; add it as a GPIO output."
                )
            }
        }
        IssueKind::WrongPinUse {
            action: None,
            label,
            ..
        } => format!(
            "The condition reads {label}, which is {}; a condition can read only GPIO inputs and outputs.",
            func_text(pins, label)
        ),
        IssueKind::WrongPinUse {
            action: Some(k),
            label,
            ..
        } => format!(
            "{} drives {label}, which is {}; only a GPIO output can be written or toggled.",
            action_text(k),
            func_text(pins, label)
        ),
        IssueKind::TooManyActions { count } => {
            let whose = if in_entry {
                "This state's entry"
            } else {
                "This row"
            };
            format!("{whose} has {count} actions; at most {MAX_ACTIONS} are allowed.")
        }
        IssueKind::BadKey {
            action,
            key,
            problem,
        } => {
            let a = action_text(action);
            match problem {
                KeyProblem::Empty => {
                    format!("{a} publishes on no key; give it one, for example led/state.")
                }
                KeyProblem::TooLong => format!(
                    "{a} publishes on a key longer than {} bytes; shorten it.",
                    middleware::key::MAX_KEY_LEN
                ),
                KeyProblem::EmptySegment => format!(
                    "{a} publishes on {key}, which has an empty part (a leading, trailing or doubled /)."
                ),
                KeyProblem::InvalidByte(at) => {
                    let c = key.get(at..).and_then(|s| s.chars().next()).unwrap_or('?');
                    format!(
                        "{a} publishes on {key}, which has {c:?}; keys use only letters, digits, _, -, . and /."
                    )
                }
                KeyProblem::System => format!(
                    "{a} publishes on {key}, but keys under sys/ belong to the firmware; choose another key."
                ),
            }
        }
        IssueKind::TextTooLong { action, len } => format!(
            "{}'s text is {len} bytes; at most {MAX_TEXT} fit in a message.",
            action_text(action)
        ),
        IssueKind::RaiseUnknown {
            action,
            event,
            near,
        } => {
            let a = action_text(action);
            if event.is_empty() {
                format!("{a}: choose the event to raise.")
            } else if near.is_some() {
                format!(
                    "{a} raises {event}, which is not an event.{}",
                    did_you_mean(near)
                )
            } else {
                format!(
                    "{a} raises {event}, which is not an event of this machine; add it or choose another."
                )
            }
        }
        IssueKind::DuplicateUnguarded { first } => {
            let (st, ev) = row_cell(first);
            format!(
                "Row {} already handles {ev} in {st} without a condition; give this row a condition or remove one of them.",
                first + 1
            )
        }
        IssueKind::GuardOverlap { other, witness } => {
            let when: Vec<String> = witness
                .levels()
                .map(|(l, high)| format!("{l} = {}", u8::from(high)))
                .collect();
            let when = if when.is_empty() {
                String::new()
            } else {
                format!(" (when {})", when.join(" and "))
            };
            format!(
                "This condition and row {}'s can both be true{when}, so which row fires depends on their order; make them exclusive.",
                other + 1
            )
        }
        IssueKind::OverlapUnchecked { other, pins } => format!(
            "This condition and row {}'s read {pins} pins together, too many (over {MAX_OVERLAP_PINS}) to check that they cannot both be true.",
            other + 1
        ),
        IssueKind::GuardNeverTrue => "This condition is never true, so the row never fires.".into(),
        IssueKind::Unreachable { name } => format!(
            "No row leads from the initial state {} to {name}, so the machine never enters it.",
            m.initial
        ),
        IssueKind::DeadEvent { name } => {
            format!("No row handles {name}, so raising it does nothing.")
        }
    }
}

fn fix(f: Fix<'_>) -> MachineFix {
    match f {
        Fix::AddState(n) => MachineFix::AddState(n.into()),
        Fix::AddEvent(n) => MachineFix::AddEvent(n.into()),
        Fix::AddPin { label, func } => MachineFix::AddPin {
            label: label.into(),
            func: if func == PinUse::Output {
                Func::GpioOut
            } else {
                Func::GpioIn
            },
        },
        Fix::SetInitial(n) => MachineFix::SetInitial(n.into()),
    }
}

/// A row's actions and guard nodes, viewed.
type RowParts<'a> = (Vec<Action<'a>>, Option<Vec<Node<'a>>>);

/// Checks `machines` against the pin table `pins`; see the module docs.
pub fn check(machines: &[MachineIn], pins: &[PinIn]) -> Vec<MachineIssue> {
    let pin_views: Vec<Pin<'_>> = pins
        .iter()
        .map(|p| Pin {
            label: &p.label,
            func: pin_use(&p.func),
        })
        .collect();
    fn acts(a: &[ActionIn]) -> Vec<Action<'_>> {
        a.iter().map(ActionIn::view).collect()
    }
    // Innermost views first: actions and guard nodes, then states and
    // rows, then machines, each borrowing the previous.
    let entries: Vec<Vec<Vec<Action<'_>>>> = machines
        .iter()
        .map(|m| m.states.iter().map(|s| acts(&s.entry)).collect())
        .collect();
    let row_parts: Vec<Vec<RowParts<'_>>> = machines
        .iter()
        .map(|m| {
            m.rows
                .iter()
                .map(|r| {
                    let guard = r
                        .guard
                        .as_ref()
                        .map(|g| g.iter().map(NodeIn::view).collect());
                    (acts(&r.actions), guard)
                })
                .collect()
        })
        .collect();
    let states: Vec<Vec<State<'_>>> = machines
        .iter()
        .zip(&entries)
        .map(|(m, e)| {
            m.states
                .iter()
                .zip(e)
                .map(|(s, entry)| State {
                    name: &s.name,
                    entry,
                })
                .collect()
        })
        .collect();
    let events: Vec<Vec<Event<'_>>> = machines
        .iter()
        .map(|m| m.events.iter().map(|name| Event { name }).collect())
        .collect();
    let rows: Vec<Vec<Row<'_>>> = machines
        .iter()
        .zip(&row_parts)
        .map(|(m, parts)| {
            m.rows
                .iter()
                .zip(parts)
                .map(|(r, (actions, guard))| Row {
                    state: &r.state,
                    event: &r.event,
                    guard: guard.as_ref().map(|nodes| Guard { nodes }),
                    next: &r.next,
                    actions,
                })
                .collect()
        })
        .collect();
    let views: Vec<Machine<'_>> = machines
        .iter()
        .enumerate()
        .map(|(i, m)| Machine {
            id: &m.id,
            initial: &m.initial,
            states: states.get(i).map_or(&[], Vec::as_slice),
            events: events.get(i).map_or(&[], Vec::as_slice),
            rows: rows.get(i).map_or(&[], Vec::as_slice),
        })
        .collect();
    let mut out = Vec::new();
    state_table::check(&views, &pin_views, key_problem, &mut |i| {
        let Some(m) = views.get(i.machine) else {
            return;
        };
        out.push(MachineIssue {
            machine: i.machine,
            list: list_name(i.list),
            row: i.index,
            field: field_name(i.field),
            severity: severity_name(i.severity),
            message: message(m, pins, &i),
            fix: i.fix.map(fix),
        });
    });
    out
}

#[cfg(test)]
mod tests;
