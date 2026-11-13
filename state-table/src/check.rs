//! The table checks: [`check`] applies every rule in the crate docs and
//! emits each problem as a structured [`Issue`] through a sink.
//!
//! An issue names *where* (machine index, [`List`], index into the list,
//! [`Field`]), *how bad* ([`Severity`]), *what* ([`IssueKind`], carrying
//! indices, numbers and borrowed names, never formatted text) and, where
//! one exists, a one-click [`Fix`]. `web/protocol-wasm` renders the
//! message; nothing here allocates.
//!
//! Names are looked up exactly. When a lookup fails but a name differing
//! only in case exists, the issue carries it as `near` and no fix is
//! offered (adding the name would collide, since names are unique
//! ignoring case).
//!
//! Publish keys are checked by the `keys` function the caller passes (the
//! middleware crate's `Key::new` in protocol-wasm, so this crate keeps no
//! copy of the key grammar), then refused under `sys/`, which belongs to
//! the firmware.
//!
//! Guard overlap is decided exactly: every leaf is a 0/1 pin, a 0/1
//! state test or a constant, so two guards overlap if and only if some
//! assignment of their distinct leaves makes both true. [`check`] tries
//! all `2^n` assignments for `n <=` [`MAX_OVERLAP_PINS`] (skipping those
//! with one machine in two states, [`crate::guard::consistent`]) and
//! reports the first one found as a [`Witness`]; above the cap it warns
//! instead. The same enumeration over one guard's leaves finds guards that
//! are always true (they count as the cell's unguarded row) and never true
//! (a warning). Treating a machine's state tests as independent apart from
//! that exclusion can only over-report: an assignment where none of a
//! machine's tested states holds is tried even if those are all its
//! states.
//!
//! The per-action and per-guard rules ([`action_issues`], [`guard_issues`])
//! are shared with the flow checks ([`crate::flow`]).

use crate::guard::{GuardError, Leaf, LeafSet, consistent, holds_under};
use crate::{
    Action, Guard, MAX_ACTIONS, MAX_EVENTS, MAX_ROWS, MAX_STATES, MAX_TEXT, Machine, Pin, PinUse,
    ident_ok,
};

/// Most distinct leaves (pins and state tests) two guards may read
/// together for the overlap check.
pub const MAX_OVERLAP_PINS: usize = 12;

/// How bad an issue is: errors stop the build, warnings do not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// The table cannot be built.
    Error,
    /// Allowed, but probably not what was meant.
    Warning,
}

/// Which list an issue's index refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum List {
    /// The machine itself (index is `None`).
    Machine,
    /// Its states.
    States,
    /// Its events.
    Events,
    /// Its transition rows.
    Rows,
}

/// The part of the machine, state, event or row an issue is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    /// The machine id.
    Id,
    /// A state's or event's name.
    Name,
    /// The initial state.
    Initial,
    /// A row's state.
    State,
    /// A row's event.
    Event,
    /// A row's guard.
    Guard,
    /// A row's next state.
    Next,
    /// A row's actions.
    Actions,
    /// A state's entry actions.
    Entry,
    /// A size limit.
    Size,
}

/// Why a publish key is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyProblem {
    /// Empty.
    Empty,
    /// Longer than the key limit.
    TooLong,
    /// Starts or ends with `/`, or has `//`.
    EmptySegment,
    /// A byte outside the key alphabet at this offset.
    InvalidByte(usize),
    /// `sys` or under `sys/`: the firmware's keys.
    System,
}

/// A change that resolves an issue, offered as a button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fix<'a> {
    /// Add a state with this name.
    AddState(&'a str),
    /// Add an event with this name.
    AddEvent(&'a str),
    /// Add a pin-table row with this label and function.
    AddPin {
        /// The label.
        label: &'a str,
        /// [`PinUse::Input`] for a guard, [`PinUse::Output`] for an action.
        func: PinUse,
    },
    /// Make this state the initial one.
    SetInitial(&'a str),
    /// Add an event with this name to another machine (by index).
    AddEventTo {
        /// The machine's index.
        machine: usize,
        /// The event's name.
        name: &'a str,
    },
}

/// A leaf assignment under which two guards are both true: the two
/// guards and the assignment's bits over their joint leaves (kept as
/// references, so an issue stays small; [`Witness::levels`] lists them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Witness<'a> {
    a: Guard<'a>,
    b: Guard<'a>,
    high: u32,
}

impl<'a> Witness<'a> {
    fn new(a: Guard<'a>, b: Guard<'a>, high: u32) -> Self {
        Witness { a, b, high }
    }

    fn set(&self) -> LeafSet<'a> {
        self.a.leaves().with(&self.b)
    }

    /// Each leaf and its value (`true` = 1), in first-seen order.
    pub fn levels(&self) -> impl Iterator<Item = (Leaf<'a>, bool)> + use<'a> {
        let set = self.set();
        let high = self.high;
        (0..set.len().min(MAX_OVERLAP_PINS))
            .filter_map(move |k| set.as_slice().get(k).map(|l| (*l, (high >> k) & 1 == 1)))
    }

    /// How many leaves the assignment names (0 when both guards are
    /// constant).
    pub fn len(&self) -> usize {
        self.set().len().min(MAX_OVERLAP_PINS)
    }

    /// Whether it names no leaves.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// What is wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueKind<'a> {
    /// The machine id is not an identifier.
    IdInvalid {
        /// The id.
        id: &'a str,
    },
    /// Another machine has the same id ignoring case.
    IdDuplicate {
        /// That machine's index.
        other: usize,
        /// The id.
        id: &'a str,
    },
    /// The machine has no states.
    NoStates,
    /// More than [`MAX_STATES`] states.
    TooManyStates {
        /// How many.
        count: usize,
    },
    /// More than [`MAX_EVENTS`] events.
    TooManyEvents {
        /// How many.
        count: usize,
    },
    /// More than [`MAX_ROWS`] rows.
    TooManyRows {
        /// How many.
        count: usize,
    },
    /// A state or event name is not an identifier.
    NameInvalid {
        /// The name.
        name: &'a str,
    },
    /// Another state (or event) has the same name ignoring case.
    NameDuplicate {
        /// Its index in the same list.
        other: usize,
        /// The name.
        name: &'a str,
    },
    /// `initial` names no state.
    InitialUnknown {
        /// The name given.
        name: &'a str,
        /// The first state (the fix).
        first: &'a str,
    },
    /// A row's `state` or `next` (by [`Field`]) names no state.
    UnknownState {
        /// The name given.
        name: &'a str,
        /// A state whose name differs only in case.
        near: Option<&'a str>,
    },
    /// A row's `event` names no event.
    UnknownEvent {
        /// The name given.
        name: &'a str,
        /// An event whose name differs only in case.
        near: Option<&'a str>,
    },
    /// The guard's arena is malformed, too large or too deep.
    GuardMalformed(GuardError),
    /// A state test names no machine.
    GuardMachineUnknown {
        /// The id given.
        machine: &'a str,
        /// A machine whose id differs only in case.
        near: Option<&'a str>,
    },
    /// A state test names no state of its machine.
    GuardStateUnknown {
        /// The machine's id.
        machine: &'a str,
        /// The name given.
        state: &'a str,
        /// A state whose name differs only in case.
        near: Option<&'a str>,
    },
    /// A pin label the guard (`action: None`) or an action reads or drives
    /// is not in the pin table.
    UnknownPin {
        /// The action's index, or `None` for the guard.
        action: Option<usize>,
        /// The label.
        label: &'a str,
        /// A pin whose label differs only in case.
        near: Option<&'a str>,
    },
    /// The pin is there but has the wrong function: a guard needs a
    /// digital pin (`gpio-in` or `gpio-out`), write / toggle a `gpio-out`.
    WrongPinUse {
        /// The action's index, or `None` for the guard.
        action: Option<usize>,
        /// The label.
        label: &'a str,
        /// What the pin is.
        has: PinUse,
    },
    /// More than [`MAX_ACTIONS`] actions.
    TooManyActions {
        /// How many.
        count: usize,
    },
    /// A publish key is refused.
    BadKey {
        /// The action's index.
        action: usize,
        /// The key.
        key: &'a str,
        /// Why.
        problem: KeyProblem,
    },
    /// Publish or log text longer than [`MAX_TEXT`] bytes.
    TextTooLong {
        /// The action's index.
        action: usize,
        /// Its length in bytes.
        len: usize,
    },
    /// A raise names no event of this machine.
    RaiseUnknown {
        /// The action's index.
        action: usize,
        /// The name given.
        event: &'a str,
        /// An event whose name differs only in case.
        near: Option<&'a str>,
    },
    /// A raise names no machine, where one is required (flows).
    RaiseNoMachine {
        /// The action's index.
        action: usize,
    },
    /// A raise names a machine there is none of.
    RaiseMachineUnknown {
        /// The action's index.
        action: usize,
        /// The id given.
        machine: &'a str,
        /// A machine whose id differs only in case.
        near: Option<&'a str>,
    },
    /// A raise names no event of another machine.
    RaiseUnknownIn {
        /// The action's index.
        action: usize,
        /// The machine's id.
        machine: &'a str,
        /// The name given.
        event: &'a str,
        /// An event whose name differs only in case.
        near: Option<&'a str>,
    },
    /// A second unguarded (or always-true) row in a cell.
    DuplicateUnguarded {
        /// The first unguarded row of the cell.
        first: usize,
    },
    /// This row's guard and an earlier one in the same cell can both be
    /// true.
    GuardOverlap {
        /// The earlier row.
        other: usize,
        /// Pin levels making both true.
        witness: Witness<'a>,
    },
    /// Warning: overlap with an earlier row was not checked (too many
    /// pins).
    OverlapUnchecked {
        /// The earlier row.
        other: usize,
        /// Distinct pins the two guards read.
        pins: usize,
    },
    /// Warning: the guard is never true, so the row never fires.
    GuardNeverTrue,
    /// Warning: no path of rows leads from the initial state here.
    Unreachable {
        /// The state's name.
        name: &'a str,
    },
    /// Warning: no row handles this event.
    DeadEvent {
        /// The event's name.
        name: &'a str,
    },
}

/// One problem with a machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Issue<'a> {
    /// Index into the machines.
    pub machine: usize,
    /// Which list `index` refers to.
    pub list: List,
    /// Index into `list`, or `None` for the machine as a whole.
    pub index: Option<usize>,
    /// What part is wrong.
    pub field: Field,
    /// Error or warning.
    pub severity: Severity,
    /// What is wrong.
    pub kind: IssueKind<'a>,
    /// A one-click fix, when there is an obvious one.
    pub fix: Option<Fix<'a>>,
}

/// What enumerating a guard's own pins says about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Truth {
    Always,
    Never,
    Depends,
    /// Too many pins to enumerate.
    Unknown,
}

fn truth(g: &Guard<'_>) -> Truth {
    let set = g.leaves();
    if set.len() > MAX_OVERLAP_PINS {
        return Truth::Unknown;
    }
    let (mut any, mut all) = (false, true);
    for bits in (0u32..(1 << set.len())).filter(|&b| consistent(&set, b)) {
        if holds_under(g, &set, bits) {
            any = true;
        } else {
            all = false;
        }
    }
    match (any, all) {
        (true, true) => Truth::Always,
        (false, _) => Truth::Never,
        _ => Truth::Depends,
    }
}

/// How two guards relate, from enumerating their joint pins.
enum Overlap<'a> {
    Disjoint,
    Both(Witness<'a>),
    Unchecked(usize),
}

fn overlap<'a>(a: &Guard<'a>, b: &Guard<'a>) -> Overlap<'a> {
    let set = a.leaves().with(b);
    if set.len() > MAX_OVERLAP_PINS {
        return Overlap::Unchecked(set.len());
    }
    for bits in (0u32..(1 << set.len())).filter(|&b| consistent(&set, b)) {
        if holds_under(a, &set, bits) && holds_under(b, &set, bits) {
            return Overlap::Both(Witness::new(*a, *b, bits));
        }
    }
    Overlap::Disjoint
}

/// Where issues of one machine go.
struct Out<'s, 'a, S: FnMut(Issue<'a>)> {
    machine: usize,
    sink: &'s mut S,
    marker: core::marker::PhantomData<&'a ()>,
}

impl<'a, S: FnMut(Issue<'a>)> Out<'_, 'a, S> {
    fn push(
        &mut self,
        at: (List, Option<usize>, Field),
        severity: Severity,
        kind: IssueKind<'a>,
        fix: Option<Fix<'a>>,
    ) {
        (self.sink)(Issue {
            machine: self.machine,
            list: at.0,
            index: at.1,
            field: at.2,
            severity,
            kind,
            fix,
        });
    }

    fn error(
        &mut self,
        at: (List, Option<usize>, Field),
        kind: IssueKind<'a>,
        fix: Option<Fix<'a>>,
    ) {
        self.push(at, Severity::Error, kind, fix);
    }

    fn warning(&mut self, at: (List, Option<usize>, Field), kind: IssueKind<'a>) {
        self.push(at, Severity::Warning, kind, None);
    }
}

fn near<'a, T>(items: &[T], name: &str, of: impl Fn(&T) -> &'a str) -> Option<&'a str> {
    items.iter().map(of).find(|n| n.eq_ignore_ascii_case(name))
}

/// Checks `machines` against the pin table `pins`, calling `sink` for each
/// issue in order (machine by machine). `keys` validates a publish key's
/// syntax; see the module docs.
pub fn check<'a>(
    machines: &[Machine<'a>],
    pins: &[Pin<'a>],
    keys: impl Fn(&str) -> Result<(), KeyProblem>,
    sink: &mut impl FnMut(Issue<'a>),
) {
    for (mi, m) in machines.iter().enumerate() {
        let mut out = Out {
            machine: mi,
            sink: &mut *sink,
            marker: core::marker::PhantomData,
        };
        check_machine(machines, mi, m, pins, &keys, &mut out);
    }
}

fn check_machine<'a, S: FnMut(Issue<'a>)>(
    machines: &[Machine<'a>],
    mi: usize,
    m: &Machine<'a>,
    pins: &[Pin<'a>],
    keys: &impl Fn(&str) -> Result<(), KeyProblem>,
    out: &mut Out<'_, 'a, S>,
) {
    let whole = |field| (List::Machine, None, field);
    // Id.
    if !ident_ok(m.id) {
        out.error(whole(Field::Id), IssueKind::IdInvalid { id: m.id }, None);
    } else if let Some(other) = machines
        .iter()
        .enumerate()
        .position(|(j, o)| j != mi && o.id.eq_ignore_ascii_case(m.id))
    {
        out.error(
            whole(Field::Id),
            IssueKind::IdDuplicate { other, id: m.id },
            None,
        );
    }
    // Sizes.
    if m.states.is_empty() {
        out.error(whole(Field::Size), IssueKind::NoStates, None);
    }
    let over = [
        (
            m.states.len() > MAX_STATES,
            IssueKind::TooManyStates {
                count: m.states.len(),
            },
        ),
        (
            m.events.len() > MAX_EVENTS,
            IssueKind::TooManyEvents {
                count: m.events.len(),
            },
        ),
        (
            m.rows.len() > MAX_ROWS,
            IssueKind::TooManyRows {
                count: m.rows.len(),
            },
        ),
    ];
    let mut too_big = false;
    for (is_over, kind) in over {
        if is_over {
            out.error(whole(Field::Size), kind, None);
            too_big = true;
        }
    }
    if too_big {
        return;
    }
    // Names.
    let state_names = |i: usize| m.states.get(i).map_or("", |s| s.name);
    let event_names = |i: usize| m.events.get(i).map_or("", |e| e.name);
    check_names(List::States, m.states.len(), state_names, out);
    check_names(List::Events, m.events.len(), event_names, out);
    // Initial.
    if let Some(first) = m.states.first()
        && m.state_index(m.initial).is_none()
    {
        out.error(
            whole(Field::Initial),
            IssueKind::InitialUnknown {
                name: m.initial,
                first: first.name,
            },
            Some(Fix::SetInitial(first.name)),
        );
    }
    // State entries.
    for (si, s) in m.states.iter().enumerate() {
        check_actions(
            machines,
            mi,
            s.entry,
            (List::States, Some(si), Field::Entry),
            pins,
            keys,
            out,
        );
    }
    // Rows.
    for (ri, row) in m.rows.iter().enumerate() {
        let at = |field| (List::Rows, Some(ri), field);
        for (field, name) in [(Field::State, row.state), (Field::Next, row.next)] {
            if m.state_index(name).is_none() {
                let near = near(m.states, name, |s| s.name);
                let fix = (near.is_none() && ident_ok(name) && m.states.len() < MAX_STATES)
                    .then_some(Fix::AddState(name));
                out.error(at(field), IssueKind::UnknownState { name, near }, fix);
            }
        }
        if m.event_index(row.event).is_none() {
            let near = near(m.events, row.event, |e| e.name);
            let fix = (near.is_none() && ident_ok(row.event) && m.events.len() < MAX_EVENTS)
                .then_some(Fix::AddEvent(row.event));
            out.error(
                at(Field::Event),
                IssueKind::UnknownEvent {
                    name: row.event,
                    near,
                },
                fix,
            );
        }
        if let Some(g) = &row.guard {
            guard_issues(machines, g, pins, &mut |kind, fix| {
                out.error(at(Field::Guard), kind, fix);
            });
        }
        check_actions(
            machines,
            mi,
            row.actions,
            at(Field::Actions),
            pins,
            keys,
            out,
        );
    }
    check_cells(m, out);
    check_reach(m, out);
}

fn check_names<'a, S: FnMut(Issue<'a>)>(
    list: List,
    len: usize,
    name_of: impl Fn(usize) -> &'a str,
    out: &mut Out<'_, 'a, S>,
) {
    for i in 0..len {
        let name = name_of(i);
        let at = (list, Some(i), Field::Name);
        if !ident_ok(name) {
            out.error(at, IssueKind::NameInvalid { name }, None);
        } else if let Some(other) =
            (0..len).find(|&j| j != i && name_of(j).eq_ignore_ascii_case(name))
        {
            out.error(at, IssueKind::NameDuplicate { other, name }, None);
        }
    }
}

/// The issue (and fix) for a label that is not in the pin table; `func`
/// is what the fix adds.
fn unknown_pin<'a>(
    action: Option<usize>,
    label: &'a str,
    func: PinUse,
    pins: &[Pin<'a>],
) -> (IssueKind<'a>, Option<Fix<'a>>) {
    let near = near(pins, label, |p| p.label);
    let fix = (near.is_none() && ident_ok(label)).then_some(Fix::AddPin { label, func });
    (
        IssueKind::UnknownPin {
            action,
            label,
            near,
        },
        fix,
    )
}

/// Every problem with guard `g`: a malformed arena, pins missing from the
/// table or not digital, state tests naming no machine or state.
pub fn guard_issues<'a>(
    machines: &[Machine<'a>],
    g: &Guard<'a>,
    pins: &[Pin<'a>],
    emit: &mut impl FnMut(IssueKind<'a>, Option<Fix<'a>>),
) {
    if let Err(e) = g.shape() {
        emit(IssueKind::GuardMalformed(e), None);
    }
    for &leaf in g.leaves().as_slice() {
        match leaf {
            Leaf::Pin(label) => match pins.iter().find(|p| p.label == label) {
                Some(p) if p.func == PinUse::Other => emit(
                    IssueKind::WrongPinUse {
                        action: None,
                        label,
                        has: p.func,
                    },
                    None,
                ),
                Some(_) => {}
                None => {
                    let (kind, fix) = unknown_pin(None, label, PinUse::Input, pins);
                    emit(kind, fix);
                }
            },
            Leaf::State { machine, state } => match machines.iter().find(|m| m.id == machine) {
                None => emit(
                    IssueKind::GuardMachineUnknown {
                        machine,
                        near: near(machines, machine, |m| m.id),
                    },
                    None,
                ),
                Some(m) if m.state_index(state).is_none() => emit(
                    IssueKind::GuardStateUnknown {
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
}

/// Every problem with action `a` (index `k` in its list). `own` is the
/// machine a raise without a machine names (a table's own), or `None`
/// where a raise must name one (flows).
#[allow(clippy::too_many_arguments)]
pub fn action_issues<'a>(
    machines: &[Machine<'a>],
    own: Option<usize>,
    k: usize,
    a: &Action<'a>,
    pins: &[Pin<'a>],
    keys: &impl Fn(&str) -> Result<(), KeyProblem>,
    emit: &mut impl FnMut(IssueKind<'a>, Option<Fix<'a>>),
) {
    match *a {
        Action::Write { label, .. } | Action::Toggle { label } => {
            match pins.iter().find(|p| p.label == label) {
                Some(p) if p.func != PinUse::Output => emit(
                    IssueKind::WrongPinUse {
                        action: Some(k),
                        label,
                        has: p.func,
                    },
                    None,
                ),
                Some(_) => {}
                None => {
                    let (kind, fix) = unknown_pin(Some(k), label, PinUse::Output, pins);
                    emit(kind, fix);
                }
            }
        }
        Action::Publish { key, text } => {
            if let Some(problem) = key_issue(key, keys) {
                emit(
                    IssueKind::BadKey {
                        action: k,
                        key,
                        problem,
                    },
                    None,
                );
            }
            text_len(k, text, emit);
        }
        Action::Log { text } => text_len(k, text, emit),
        Action::Raise { machine, event } => {
            let own_m = own.and_then(|i| machines.get(i).map(|m| (i, m)));
            let target = match (machine, own_m) {
                (None, Some(o)) => o,
                (None, None) => return emit(IssueKind::RaiseNoMachine { action: k }, None),
                (Some(id), _) => match machines.iter().enumerate().find(|(_, m)| m.id == id) {
                    Some(t) => t,
                    None => {
                        return emit(
                            IssueKind::RaiseMachineUnknown {
                                action: k,
                                machine: id,
                                near: near(machines, id, |m| m.id),
                            },
                            None,
                        );
                    }
                },
            };
            let (ti, t) = target;
            if t.event_index(event).is_some() {
                return;
            }
            let near = near(t.events, event, |e| e.name);
            let can_add = near.is_none() && ident_ok(event) && t.events.len() < MAX_EVENTS;
            if Some(ti) == own {
                emit(
                    IssueKind::RaiseUnknown {
                        action: k,
                        event,
                        near,
                    },
                    can_add.then_some(Fix::AddEvent(event)),
                );
            } else {
                emit(
                    IssueKind::RaiseUnknownIn {
                        action: k,
                        machine: t.id,
                        event,
                        near,
                    },
                    can_add.then_some(Fix::AddEventTo {
                        machine: ti,
                        name: event,
                    }),
                );
            }
        }
    }
}

/// Why `key` cannot be published or subscribed to: the key rule, or
/// `sys/`, which belongs to the firmware.
pub fn key_issue(key: &str, keys: &impl Fn(&str) -> Result<(), KeyProblem>) -> Option<KeyProblem> {
    match keys(key) {
        Err(p) => Some(p),
        Ok(()) if key == "sys" || key.starts_with("sys/") => Some(KeyProblem::System),
        Ok(()) => None,
    }
}

fn check_actions<'a, S: FnMut(Issue<'a>)>(
    machines: &[Machine<'a>],
    mi: usize,
    actions: &[Action<'a>],
    at: (List, Option<usize>, Field),
    pins: &[Pin<'a>],
    keys: &impl Fn(&str) -> Result<(), KeyProblem>,
    out: &mut Out<'_, 'a, S>,
) {
    if actions.len() > MAX_ACTIONS {
        out.error(
            at,
            IssueKind::TooManyActions {
                count: actions.len(),
            },
            None,
        );
    }
    for (k, a) in actions.iter().enumerate() {
        action_issues(machines, Some(mi), k, a, pins, keys, &mut |kind, fix| {
            out.error(at, kind, fix);
        });
    }
}

fn text_len<'a>(action: usize, text: &str, emit: &mut impl FnMut(IssueKind<'a>, Option<Fix<'a>>)) {
    if text.len() > MAX_TEXT {
        emit(
            IssueKind::TextTooLong {
                action,
                len: text.len(),
            },
            None,
        );
    }
}

/// A row's part in its cell.
#[derive(Clone, Copy)]
enum Role<'a> {
    /// Unguarded, or a guard that is always true.
    Default,
    /// A guard that can be true.
    Guarded(Guard<'a>),
    /// Not part of the cell analysis (unknown state or event, a
    /// malformed guard, or one that is never true).
    Skip,
}

fn role<'a>(m: &Machine<'a>, ri: usize) -> (Role<'a>, Option<Truth>) {
    let Some(row) = m.rows.get(ri) else {
        return (Role::Skip, None);
    };
    if m.state_index(row.state).is_none() || m.event_index(row.event).is_none() {
        return (Role::Skip, None);
    }
    match row.guard {
        None => (Role::Default, None),
        Some(g) if g.shape().is_err() => (Role::Skip, None),
        Some(g) => {
            let t = truth(&g);
            let r = match t {
                Truth::Never => Role::Skip,
                _ => Role::Guarded(g),
            };
            (r, Some(t))
        }
    }
}

fn check_cells<'a, S: FnMut(Issue<'a>)>(m: &Machine<'a>, out: &mut Out<'_, 'a, S>) {
    let same_cell = |i: usize, j: usize| match (m.rows.get(i), m.rows.get(j)) {
        (Some(a), Some(b)) => a.state == b.state && a.event == b.event,
        _ => false,
    };
    for ri in 0..m.rows.len() {
        let at = (List::Rows, Some(ri), Field::Guard);
        let (r, t) = role(m, ri);
        if t == Some(Truth::Never) {
            out.warning(at, IssueKind::GuardNeverTrue);
        }
        let is_default = matches!(r, Role::Default) || t == Some(Truth::Always);
        if is_default
            && let Some(first) = (0..ri).find(|&j| {
                same_cell(ri, j)
                    && match role(m, j) {
                        (Role::Default, _) => true,
                        (_, t) => t == Some(Truth::Always),
                    }
            })
        {
            out.error(at, IssueKind::DuplicateUnguarded { first }, None);
        }
        let Role::Guarded(g) = r else { continue };
        for j in (0..ri).filter(|&j| same_cell(ri, j)) {
            let Role::Guarded(h) = role(m, j).0 else {
                continue;
            };
            // Two always-true guards are already a duplicate default.
            if t == Some(Truth::Always) && truth(&h) == Truth::Always {
                continue;
            }
            match overlap(&h, &g) {
                Overlap::Disjoint => {}
                Overlap::Unchecked(pins) => {
                    out.warning(at, IssueKind::OverlapUnchecked { other: j, pins });
                }
                Overlap::Both(witness) => {
                    out.error(at, IssueKind::GuardOverlap { other: j, witness }, None);
                    break;
                }
            }
        }
    }
}

fn check_reach<'a, S: FnMut(Issue<'a>)>(m: &Machine<'a>, out: &mut Out<'_, 'a, S>) {
    if let Some(init) = m.state_index(m.initial) {
        let mut reached: u32 = 1 << init;
        // Each pass reaches at least one new state or stops; there are at
        // most MAX_STATES states.
        for _ in 0..MAX_STATES {
            let before = reached;
            for row in m.rows {
                if let (Some(s), Some(n)) = (m.state_index(row.state), m.state_index(row.next))
                    && reached & (1 << s) != 0
                {
                    reached |= 1 << n;
                }
            }
            if reached == before {
                break;
            }
        }
        for (si, s) in m.states.iter().enumerate() {
            if reached & (1 << si) == 0 {
                out.warning(
                    (List::States, Some(si), Field::Name),
                    IssueKind::Unreachable { name: s.name },
                );
            }
        }
    }
    for (ei, e) in m.events.iter().enumerate() {
        if !m.rows.iter().any(|r| m.event_index(r.event) == Some(ei)) {
            out.warning(
                (List::Events, Some(ei), Field::Name),
                IssueKind::DeadEvent { name: e.name },
            );
        }
    }
}

#[cfg(test)]
mod tests;
