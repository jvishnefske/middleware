//! Master recipes: an equipment-independent procedure of steps, each
//! naming a phase class and its parameter values, joined by transitions
//! into a sequential function chart (SFC, IEC 60848 / IEC 61131-3 style).
//!
//! The ISA-88 procedure hierarchy is kept as data: a recipe holds
//! **unit procedures** ([`UnitProc`], each optionally constrained to a unit
//! class; allocation binds each to one unit), and every [`Step`] belongs to
//! one unit procedure and runs one phase class there. Operations are a
//! grouping label the editor keeps for the reader; they carry no
//! semantics here.
//!
//! # The chart
//!
//! A [`Transition`] has a non-empty `from` and `to` list of steps. It fires
//! once every `from` step is done (its phase reached Complete): those steps
//! end and every `to` step starts. A `to` list of several steps is a
//! simultaneous (parallel) divergence; a `from` list of several steps a
//! simultaneous convergence. The **initial** steps are those no transition
//! leads to; they start together with the recipe. The recipe is complete
//! when every step is done.
//!
//! Conditions are always "the `from` phases are complete": there is no
//! selection (alternative) divergence, so the chart is a marked graph and
//! every run activates every step exactly once. [`check`] makes that so:
//!
//! * a step appears in at most one transition's `from` (a second is a
//!   choice, not supported) and in at most one transition's `to`;
//! * the chart is acyclic (found by peeling steps with no incoming edge,
//!   at most one pass per step).
//!
//! Every step is then reachable: in topological order, a step is initial
//! or started by its one incoming transition, whose `from` steps come
//! earlier and so (by induction) start and, once their phases complete,
//! finish. Together these give the engine its termination argument: each
//! transition fires at most once and each step runs exactly once in a run
//! whose phases all complete.
//!
//! # Rules
//!
//! Errors unless marked: phase class names, parameter names, the recipe id,
//! unit procedure ids and step ids are identifiers ([`ident_ok`]) unique
//! ignoring case in their list; a class has at most [`MAX_PARAMS`]
//! parameters each with `min <= max`; at most [`MAX_CLASSES`] classes,
//! [`MAX_UNIT_PROCS`] unit procedures, [`MAX_STEPS`] steps and
//! [`MAX_TRANSITIONS`] transitions; at least one step; a step's unit
//! procedure and class exist; its arguments name parameters of the class,
//! each at most once, every parameter is given, and each value lies in the
//! class's range; a transition names existing steps, none twice on one
//! side, with non-empty sides; the chart rules above; a unit procedure no
//! step uses is a warning.

use crate::{
    MAX_CLASSES, MAX_PARAMS, MAX_STEPS, MAX_TRANSITIONS, MAX_UNIT_PROCS, StepMask, bit, ident_ok,
};

/// A phase class parameter: an `i32` with an inclusive range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParamDef<'a> {
    /// The name recipes give values for.
    pub name: &'a str,
    /// Smallest allowed value.
    pub min: i32,
    /// Largest allowed value.
    pub max: i32,
}

/// A phase class: what recipes ask for and units implement, e.g.
/// `Blink(period_ms, times)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhaseClass<'a> {
    /// Identifier, unique ignoring case.
    pub name: &'a str,
    /// The parameters, in wire order.
    pub params: &'a [ParamDef<'a>],
}

/// A unit procedure: the part of the recipe that runs on one unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnitProc<'a> {
    /// Identifier, unique ignoring case.
    pub id: &'a str,
    /// The unit class a bound unit must have, if constrained.
    pub unit_class: Option<&'a str>,
}

/// A step's value for one parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arg<'a> {
    /// The parameter's name.
    pub name: &'a str,
    /// Its value.
    pub value: i32,
}

/// A step: run one phase class within a unit procedure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step<'a> {
    /// Identifier, unique ignoring case.
    pub id: &'a str,
    /// The unit procedure's id.
    pub unit_proc: &'a str,
    /// The phase class's name.
    pub class: &'a str,
    /// One value per class parameter.
    pub args: &'a [Arg<'a>],
}

/// A transition: when every `from` step is done, start every `to` step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transition<'a> {
    /// Step ids that must be done.
    pub from: &'a [&'a str],
    /// Step ids that start.
    pub to: &'a [&'a str],
}

/// A master recipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recipe<'a> {
    /// Identifier.
    pub id: &'a str,
    /// The unit procedures.
    pub unit_procs: &'a [UnitProc<'a>],
    /// The steps.
    pub steps: &'a [Step<'a>],
    /// The transitions.
    pub transitions: &'a [Transition<'a>],
}

/// Where an [`Issue`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum At {
    /// The recipe as a whole.
    Recipe,
    /// A phase class.
    Class(usize),
    /// A class's parameter: class, parameter.
    ClassParam(usize, usize),
    /// A unit procedure.
    UnitProc(usize),
    /// A step.
    Step(usize),
    /// A step's argument: step, argument.
    Arg(usize, usize),
    /// A transition.
    Transition(usize),
}

/// How bad an [`Issue`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// The recipe cannot be compiled.
    Error,
    /// Worth fixing; does not stop compilation.
    Warning,
}

/// What is wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueKind<'a> {
    /// Not an identifier.
    BadIdent,
    /// Same name (ignoring case) as the entry at this index of the list.
    Duplicate {
        /// The earlier entry.
        first: usize,
    },
    /// The list is longer than this; the rest of the recipe is not checked.
    TooMany {
        /// The limit.
        limit: usize,
    },
    /// A parameter's `min` is above its `max`.
    EmptyRange,
    /// A recipe with no steps.
    NoSteps,
    /// No unit procedure has this id.
    UnknownUnitProc(&'a str),
    /// No phase class has this name.
    UnknownClass(&'a str),
    /// The class has no parameter of this name.
    UnknownParam(&'a str),
    /// The parameter is given again (first at this argument index).
    DuplicateArg {
        /// The earlier argument.
        first: usize,
    },
    /// This class parameter has no value.
    MissingParam(&'a str),
    /// The value is outside the class's range.
    OutOfRange {
        /// The value.
        value: i32,
        /// The range's low end.
        min: i32,
        /// The range's high end.
        max: i32,
    },
    /// A transition with an empty `from` or `to` list.
    EmptyTransition,
    /// No step has this id.
    UnknownStep(&'a str),
    /// The step is named twice on one side of the transition.
    RepeatedStep(&'a str),
    /// The step already ends at an earlier transition (a choice; not
    /// supported).
    TwoOutgoing {
        /// The step.
        step: usize,
        /// The earlier transition.
        first: usize,
    },
    /// The step is already started by an earlier transition.
    TwoIncoming {
        /// The step.
        step: usize,
        /// The earlier transition.
        first: usize,
    },
    /// The transition is part of a cycle.
    Cycle,
    /// No step uses the unit procedure (warning).
    UnusedUnitProc,
}

/// One problem [`check`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Issue<'a> {
    /// Where.
    pub at: At,
    /// How bad.
    pub severity: Severity,
    /// What.
    pub kind: IssueKind<'a>,
}

/// A recipe compiled against its classes: indices and masks, `Copy`, no
/// borrows. Only [`compile`] makes one (the fields are private), so it
/// always satisfies the rules and its counts are within the limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sfc {
    /// Number of steps.
    pub(crate) steps: usize,
    /// Number of unit procedures.
    pub(crate) unit_procs: usize,
    /// Each step's class index.
    pub(crate) step_class: [u8; MAX_STEPS],
    /// Each step's unit procedure index.
    pub(crate) step_up: [u8; MAX_STEPS],
    /// Each step's arguments in the class's parameter order (the rest 0).
    pub(crate) args: [[i32; MAX_PARAMS]; MAX_STEPS],
    /// The steps no transition leads to.
    pub(crate) initial: StepMask,
    /// Number of transitions.
    pub(crate) transitions: usize,
    /// Each transition's `from` steps.
    pub(crate) from: [StepMask; MAX_TRANSITIONS],
    /// Each transition's `to` steps.
    pub(crate) to: [StepMask; MAX_TRANSITIONS],
    /// For each step, every step that is done before it starts.
    pub(crate) before: [StepMask; MAX_STEPS],
}

impl Sfc {
    /// Number of steps.
    pub const fn steps(&self) -> usize {
        self.steps
    }

    /// Number of unit procedures.
    pub const fn unit_procs(&self) -> usize {
        self.unit_procs
    }

    /// Number of transitions.
    pub const fn transitions(&self) -> usize {
        self.transitions
    }

    /// Step `i`'s class index and unit procedure index.
    pub fn step_of(&self, i: usize) -> Option<(usize, usize)> {
        let c = *self.step_class.get(i)?;
        let u = *self.step_up.get(i)?;
        (i < self.steps).then_some((usize::from(c), usize::from(u)))
    }

    /// Step `i`'s arguments in its class's parameter order (unused
    /// positions 0).
    pub fn args(&self, i: usize) -> Option<&[i32; MAX_PARAMS]> {
        self.args.get(i).filter(|_| i < self.steps)
    }

    /// The initial steps.
    pub const fn initial(&self) -> StepMask {
        self.initial
    }

    /// Transition `t`'s `from` and `to` steps.
    pub fn transition(&self, t: usize) -> Option<(StepMask, StepMask)> {
        let f = *self.from.get(t)?;
        let g = *self.to.get(t)?;
        (t < self.transitions).then_some((f, g))
    }

    /// Every step that is done before step `i` starts.
    pub fn before(&self, i: usize) -> StepMask {
        self.before.get(i).copied().unwrap_or(0)
    }

    /// Every step.
    pub const fn all(&self) -> StepMask {
        if self.steps >= 32 {
            StepMask::MAX
        } else {
            (1 << self.steps) - 1
        }
    }

    /// Whether steps `a` and `b` may be active at the same time.
    pub const fn concurrent(&self, a: usize, b: usize) -> bool {
        a != b && self.before[a] & bit(b) == 0 && self.before[b] & bit(a) == 0
    }
}

/// Why [`compile`] failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompileError {
    /// [`check`] found at least one error.
    Invalid,
}

fn position_ignoring_case<'a, T>(
    list: &[T],
    upto: usize,
    name: &str,
    key: impl Fn(&T) -> &'a str,
) -> Option<usize> {
    list.iter()
        .take(upto)
        .position(|x| key(x).eq_ignore_ascii_case(name))
}

fn exact<T>(list: &[T], name: &str, key: impl Fn(&T) -> &str) -> Option<usize> {
    list.iter().position(|x| key(x) == name)
}

fn names<'a, T>(
    list: &[T],
    key: impl Fn(&T) -> &'a str,
    at: impl Fn(usize) -> At,
    sink: &mut impl FnMut(Issue<'a>),
) {
    for (i, x) in list.iter().enumerate() {
        let name = key(x);
        if !ident_ok(name) {
            sink(err(at(i), IssueKind::BadIdent));
        } else if let Some(first) = position_ignoring_case(list, i, name, &key) {
            sink(err(at(i), IssueKind::Duplicate { first }));
        }
    }
}

const fn err(at: At, kind: IssueKind<'_>) -> Issue<'_> {
    Issue {
        at,
        severity: Severity::Error,
        kind,
    }
}

/// Checks the phase classes on their own (names, sizes, ranges).
pub fn check_classes<'a>(classes: &[PhaseClass<'a>], sink: &mut impl FnMut(Issue<'a>)) {
    if classes.len() > MAX_CLASSES {
        sink(err(At::Recipe, IssueKind::TooMany { limit: MAX_CLASSES }));
        return;
    }
    names(classes, |c| c.name, At::Class, sink);
    for (ci, c) in classes.iter().enumerate() {
        if c.params.len() > MAX_PARAMS {
            sink(err(At::Class(ci), IssueKind::TooMany { limit: MAX_PARAMS }));
            continue;
        }
        names(c.params, |p| p.name, |pi| At::ClassParam(ci, pi), sink);
        for (pi, p) in c.params.iter().enumerate() {
            if p.min > p.max {
                sink(err(At::ClassParam(ci, pi), IssueKind::EmptyRange));
            }
        }
    }
}

/// Checks `recipe` against `classes` (module docs), reporting every issue
/// to `sink`. The classes' own rules are [`check_classes`]; a recipe step
/// naming a class resolves it by exact name.
pub fn check<'a>(
    recipe: &Recipe<'a>,
    classes: &[PhaseClass<'a>],
    sink: &mut impl FnMut(Issue<'a>),
) {
    if !ident_ok(recipe.id) {
        sink(err(At::Recipe, IssueKind::BadIdent));
    }
    let limits = [
        (recipe.unit_procs.len(), MAX_UNIT_PROCS),
        (recipe.steps.len(), MAX_STEPS),
        (recipe.transitions.len(), MAX_TRANSITIONS),
        (classes.len(), MAX_CLASSES),
    ];
    if let Some(&(_, limit)) = limits.iter().find(|(n, limit)| n > limit) {
        sink(err(At::Recipe, IssueKind::TooMany { limit }));
        return;
    }
    if recipe.steps.is_empty() {
        sink(err(At::Recipe, IssueKind::NoSteps));
    }
    names(recipe.unit_procs, |u| u.id, At::UnitProc, sink);
    for (ui, u) in recipe.unit_procs.iter().enumerate() {
        if u.unit_class.is_some_and(|c| !ident_ok(c)) {
            sink(err(At::UnitProc(ui), IssueKind::BadIdent));
        }
        if !recipe.steps.iter().any(|s| s.unit_proc == u.id) {
            sink(Issue {
                at: At::UnitProc(ui),
                severity: Severity::Warning,
                kind: IssueKind::UnusedUnitProc,
            });
        }
    }
    names(recipe.steps, |s| s.id, At::Step, sink);
    for (si, s) in recipe.steps.iter().enumerate() {
        step_issues(recipe, classes, si, s, sink);
    }
    chart_issues(recipe, sink);
}

fn step_issues<'a>(
    recipe: &Recipe<'a>,
    classes: &[PhaseClass<'a>],
    si: usize,
    s: &Step<'a>,
    sink: &mut impl FnMut(Issue<'a>),
) {
    if exact(recipe.unit_procs, s.unit_proc, |u| u.id).is_none() {
        sink(err(At::Step(si), IssueKind::UnknownUnitProc(s.unit_proc)));
    }
    let Some(ci) = exact(classes, s.class, |c| c.name) else {
        sink(err(At::Step(si), IssueKind::UnknownClass(s.class)));
        return;
    };
    let class = &classes[ci];
    if s.args.len() > MAX_PARAMS {
        sink(err(At::Step(si), IssueKind::TooMany { limit: MAX_PARAMS }));
        return;
    }
    for (ai, a) in s.args.iter().enumerate() {
        match exact(class.params, a.name, |p| p.name) {
            None => sink(err(At::Arg(si, ai), IssueKind::UnknownParam(a.name))),
            Some(_) if exact(&s.args[..ai], a.name, |b| b.name).is_some() => {
                let first = exact(s.args, a.name, |b| b.name).unwrap_or(0);
                sink(err(At::Arg(si, ai), IssueKind::DuplicateArg { first }));
            }
            Some(pi) => {
                let p = &class.params[pi];
                if a.value < p.min || a.value > p.max {
                    sink(err(
                        At::Arg(si, ai),
                        IssueKind::OutOfRange {
                            value: a.value,
                            min: p.min,
                            max: p.max,
                        },
                    ));
                }
            }
        }
    }
    for p in class.params {
        if exact(s.args, p.name, |a| a.name).is_none() {
            sink(err(At::Step(si), IssueKind::MissingParam(p.name)));
        }
    }
}

/// The steps a side of a transition names, reporting unknown and repeated
/// names. `None` if any name failed.
fn side<'a>(
    recipe: &Recipe<'a>,
    ti: usize,
    list: &[&'a str],
    sink: &mut impl FnMut(Issue<'a>),
) -> Option<StepMask> {
    let mut mask: StepMask = 0;
    let mut ok = true;
    for (k, name) in list.iter().enumerate() {
        match exact(recipe.steps, name, |s| s.id) {
            None => {
                sink(err(At::Transition(ti), IssueKind::UnknownStep(name)));
                ok = false;
            }
            Some(_) if list[..k].contains(name) => {
                sink(err(At::Transition(ti), IssueKind::RepeatedStep(name)));
                ok = false;
            }
            Some(i) => mask |= bit(i),
        }
    }
    ok.then_some(mask)
}

/// The transitions as masks, or `None` (issues reported) when a side is
/// empty or names an unknown step.
fn masks<'a>(
    recipe: &Recipe<'a>,
    sink: &mut impl FnMut(Issue<'a>),
) -> Option<([StepMask; MAX_TRANSITIONS], [StepMask; MAX_TRANSITIONS])> {
    let mut from = [0; MAX_TRANSITIONS];
    let mut to = [0; MAX_TRANSITIONS];
    let mut ok = true;
    for (ti, t) in recipe.transitions.iter().enumerate() {
        if t.from.is_empty() || t.to.is_empty() {
            sink(err(At::Transition(ti), IssueKind::EmptyTransition));
            ok = false;
        }
        match (side(recipe, ti, t.from, sink), side(recipe, ti, t.to, sink)) {
            (Some(f), Some(g)) => {
                from[ti] = f;
                to[ti] = g;
            }
            _ => ok = false,
        }
    }
    ok.then_some((from, to))
}

fn chart_issues<'a>(recipe: &Recipe<'a>, sink: &mut impl FnMut(Issue<'a>)) {
    let Some((from, to)) = masks(recipe, sink) else {
        return;
    };
    let n = recipe.transitions.len();
    let steps = recipe.steps.len();
    for ti in 0..n {
        for si in 0..steps {
            if from[ti] & bit(si) != 0
                && let Some(first) = (0..ti).find(|&e| from[e] & bit(si) != 0)
            {
                sink(err(
                    At::Transition(ti),
                    IssueKind::TwoOutgoing { step: si, first },
                ));
            }
            if to[ti] & bit(si) != 0
                && let Some(first) = (0..ti).find(|&e| to[e] & bit(si) != 0)
            {
                sink(err(
                    At::Transition(ti),
                    IssueKind::TwoIncoming { step: si, first },
                ));
            }
        }
    }
    // Cycles: peel steps with no incoming edge from a remaining step.
    let all: StepMask = if steps >= 32 {
        StepMask::MAX
    } else {
        (1 << steps) - 1
    };
    let mut left = all;
    for _ in 0..steps {
        let mut peeled = false;
        for si in 0..steps {
            let incoming = (0..n).any(|t| to[t] & bit(si) != 0 && from[t] & left != 0);
            if left & bit(si) != 0 && !incoming {
                left &= !bit(si);
                peeled = true;
            }
        }
        if !peeled {
            break;
        }
    }
    for ti in 0..n {
        if from[ti] & left != 0 && to[ti] & left != 0 {
            sink(err(At::Transition(ti), IssueKind::Cycle));
        }
    }
}

fn initial(to: &[StepMask], all: StepMask) -> StepMask {
    all & !to.iter().fold(0, |m, t| m | t)
}

/// Compiles a recipe that [`check`] (with [`check_classes`]) finds no
/// error in.
pub fn compile(recipe: &Recipe<'_>, classes: &[PhaseClass<'_>]) -> Result<Sfc, CompileError> {
    let mut errors = 0usize;
    let mut count = |i: Issue<'_>| {
        if i.severity == Severity::Error {
            errors += 1;
        }
    };
    check_classes(classes, &mut count);
    check(recipe, classes, &mut count);
    if errors > 0 {
        return Err(CompileError::Invalid);
    }
    let mut sfc = Sfc {
        steps: recipe.steps.len(),
        unit_procs: recipe.unit_procs.len(),
        step_class: [0; MAX_STEPS],
        step_up: [0; MAX_STEPS],
        args: [[0; MAX_PARAMS]; MAX_STEPS],
        initial: 0,
        transitions: recipe.transitions.len(),
        from: [0; MAX_TRANSITIONS],
        to: [0; MAX_TRANSITIONS],
        before: [0; MAX_STEPS],
    };
    for (si, s) in recipe.steps.iter().enumerate() {
        let ci = exact(classes, s.class, |c| c.name).ok_or(CompileError::Invalid)?;
        let ui = exact(recipe.unit_procs, s.unit_proc, |u| u.id).ok_or(CompileError::Invalid)?;
        sfc.step_class[si] = u8::try_from(ci).map_err(|_| CompileError::Invalid)?;
        sfc.step_up[si] = u8::try_from(ui).map_err(|_| CompileError::Invalid)?;
        for (pi, p) in classes[ci].params.iter().enumerate() {
            let a = exact(s.args, p.name, |a| a.name).ok_or(CompileError::Invalid)?;
            sfc.args[si][pi] = s.args[a].value;
        }
    }
    let (from, to) = masks(recipe, &mut |_| {}).ok_or(CompileError::Invalid)?;
    sfc.from = from;
    sfc.to = to;
    sfc.initial = initial(&to[..sfc.transitions], sfc.all());
    // `before`: a fixpoint over the transitions, one pass per step.
    for _ in 0..sfc.steps {
        for t in 0..sfc.transitions {
            let mut done_first = from[t];
            for s in 0..sfc.steps {
                if from[t] & bit(s) != 0 {
                    done_first |= sfc.before[s];
                }
            }
            for s in 0..sfc.steps {
                if to[t] & bit(s) != 0 {
                    sfc.before[s] |= done_first;
                }
            }
        }
    }
    Ok(sfc)
}

#[cfg(test)]
mod tests;
