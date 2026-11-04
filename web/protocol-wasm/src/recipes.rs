//! S88 master recipes for the web editor: checks, allocation to units and
//! a simulated run, over the `recipe-core` crate, with plain-language
//! messages.
//!
//! The web model (`web/src/model/recipe.ts`, `procedural.ts`, `plant.ts`)
//! owns the shapes; `recipe-core` owns the rules. This module converts
//! owned values ([`RecipeIn`], [`ClassIn`], [`UnitIn`]) into that crate's
//! borrowed views and renders its structured results:
//!
//! * [`check`] — [`recipe_core::recipe::check_classes`] and
//!   [`recipe_core::recipe::check`] as [`RecipeIssue`]s naming the step,
//!   transition, class or unit procedure;
//! * [`allocate`] — [`recipe_core::alloc::allocate`]: the [`Binding`] of
//!   every step, or an [`AllocFailure`] saying, per unit, why it cannot
//!   host the procedure ([`recipe_core::alloc::explain`]);
//! * [`simulate`] — allocation, then [`recipe_core::sim::run`] against
//!   simulated units following a [`ScriptIn`] of operator commands and
//!   unit losses, as a [`Trace`] of timed lines.
//!
//! Everything here is pure; `crate::bindings` converts JS objects.

use recipe_core::alloc::{self, AllocError, Reject, Unit, UnitPhase};
use recipe_core::engine::{AckError, Config, Effect, Engine, Failure, Ignored, Refusal};
use recipe_core::recipe::{self, At, Issue, IssueKind, Severity};
use recipe_core::sim::{self, Event, Scripted, SimPhase, SimUnits};
use recipe_core::{
    Arg, Command, ParamDef, PhaseClass, PhaseState, Recipe, Step, Transition, UnitProc,
};

/// A phase class parameter as the page sends it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamIn {
    /// Name.
    pub name: String,
    /// Low end.
    pub min: i32,
    /// High end.
    pub max: i32,
}

/// A phase class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassIn {
    /// Name.
    pub name: String,
    /// Parameters.
    pub params: Vec<ParamIn>,
}

/// A unit procedure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitProcIn {
    /// Id.
    pub id: String,
    /// Required unit class, if any.
    pub unit_class: Option<String>,
}

/// A step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepIn {
    /// Id.
    pub id: String,
    /// Its unit procedure's id.
    pub unit_proc: String,
    /// Its phase class.
    pub class: String,
    /// Argument name and value pairs.
    pub args: Vec<(String, i32)>,
}

/// A transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionIn {
    /// Step ids that must be done.
    pub from: Vec<String>,
    /// Step ids that start.
    pub to: Vec<String>,
}

/// A master recipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipeIn {
    /// Id.
    pub id: String,
    /// Unit procedures.
    pub unit_procs: Vec<UnitProcIn>,
    /// Steps.
    pub steps: Vec<StepIn>,
    /// Transitions.
    pub transitions: Vec<TransitionIn>,
}

/// An equipment phase a unit offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitPhaseIn {
    /// Equipment module id.
    pub em: String,
    /// Phase name.
    pub name: String,
    /// Phase class.
    pub class: String,
    /// `(min, max)` per class parameter.
    pub ranges: Vec<(i32, i32)>,
    /// How long Running lasts in a simulation (ms).
    pub run_ms: u32,
}

/// A unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitIn {
    /// Id.
    pub id: String,
    /// Unit class.
    pub class: String,
    /// Online in discovery.
    pub online: bool,
    /// Its equipment phases.
    pub phases: Vec<UnitPhaseIn>,
}

/// One check result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipeIssue {
    /// `"recipe"`, `"class"`, `"unitProc"`, `"step"` or `"transition"`.
    pub at: &'static str,
    /// Index into that list (`None` for the recipe).
    pub index: Option<usize>,
    /// Parameter or argument index within it, if any.
    pub sub: Option<usize>,
    /// `"error"` or `"warning"`.
    pub severity: &'static str,
    /// What is wrong, in plain language.
    pub message: String,
}

/// Where a step runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// The step's id.
    pub step: String,
    /// The unit's id.
    pub unit: String,
    /// The equipment module.
    pub em: String,
    /// The phase.
    pub phase: String,
}

/// Why allocation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllocFailure {
    /// The whole reason.
    pub message: String,
    /// The unit procedure no unit can host, when that is the reason.
    pub unit_proc: Option<String>,
    /// Per unit: its id and why it cannot (only units with a reason).
    pub reasons: Vec<(String, String)>,
}

/// A scripted simulation event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptEventIn {
    /// An operator command.
    Operator(Command),
    /// The unit with this id drops off.
    Lose(String),
    /// The unit with this id comes back.
    Find(String),
}

/// A script line: time and event.
pub type ScriptIn = Vec<(u64, ScriptEventIn)>;

/// One line of a simulated run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceLine {
    /// Time in ms.
    pub t: u64,
    /// `"recipe"`, `"step"`, `"send"`, `"refused"`, `"failed"` or
    /// `"ignored"`.
    pub kind: &'static str,
    /// What happened.
    pub text: String,
}

/// A simulated run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trace {
    /// What happened, in order.
    pub lines: Vec<TraceLine>,
    /// The procedure's state at the end.
    pub state: &'static str,
    /// Where each step ran.
    pub bindings: Vec<Binding>,
}

fn class_views(classes: &[ClassIn]) -> Vec<Vec<ParamDef<'_>>> {
    classes
        .iter()
        .map(|c| {
            c.params
                .iter()
                .map(|p| ParamDef {
                    name: &p.name,
                    min: p.min,
                    max: p.max,
                })
                .collect()
        })
        .collect()
}

/// Runs `f` with borrowed views of the recipe and classes.
fn with_recipe<R>(
    r: &RecipeIn,
    classes: &[ClassIn],
    f: impl FnOnce(&Recipe<'_>, &[PhaseClass<'_>]) -> R,
) -> R {
    let params = class_views(classes);
    let class_list: Vec<PhaseClass<'_>> = classes
        .iter()
        .zip(&params)
        .map(|(c, p)| PhaseClass {
            name: &c.name,
            params: p,
        })
        .collect();
    let ups: Vec<UnitProc<'_>> = r
        .unit_procs
        .iter()
        .map(|u| UnitProc {
            id: &u.id,
            unit_class: u.unit_class.as_deref(),
        })
        .collect();
    let args: Vec<Vec<Arg<'_>>> = r
        .steps
        .iter()
        .map(|s| {
            s.args
                .iter()
                .map(|(n, v)| Arg { name: n, value: *v })
                .collect()
        })
        .collect();
    let steps: Vec<Step<'_>> = r
        .steps
        .iter()
        .zip(&args)
        .map(|(s, a)| Step {
            id: &s.id,
            unit_proc: &s.unit_proc,
            class: &s.class,
            args: a,
        })
        .collect();
    let sides: Vec<(Vec<&str>, Vec<&str>)> = r
        .transitions
        .iter()
        .map(|t| {
            (
                t.from.iter().map(String::as_str).collect(),
                t.to.iter().map(String::as_str).collect(),
            )
        })
        .collect();
    let transitions: Vec<Transition<'_>> = sides
        .iter()
        .map(|(f, t)| Transition { from: f, to: t })
        .collect();
    let view = Recipe {
        id: &r.id,
        unit_procs: &ups,
        steps: &steps,
        transitions: &transitions,
    };
    f(&view, &class_list)
}

fn step_name(r: &RecipeIn, i: usize) -> String {
    r.steps.get(i).map_or_else(
        || format!("step {}", i + 1),
        |s| format!("step \"{}\"", s.id),
    )
}

fn message(r: &RecipeIn, classes: &[ClassIn], i: &Issue<'_>) -> String {
    let limit_of = |what: &str, limit: usize| format!("at most {limit} {what}");
    match i.kind {
        IssueKind::BadIdent => match i.at {
            At::Recipe => format!(
                "recipe id \"{}\" must start with a letter and use only letters, digits and _ (at most 24)",
                r.id
            ),
            At::UnitProc(u)
                if r.unit_procs
                    .get(u)
                    .is_some_and(|x| recipe_core::ident_ok(&x.id)) =>
            {
                "the unit class must start with a letter and use only letters, digits and _"
                    .to_string()
            }
            _ => {
                "the name must start with a letter and use only letters, digits and _ (at most 24)"
                    .to_string()
            }
        },
        IssueKind::Duplicate { first } => format!(
            "the same name as entry {} (names are compared ignoring case)",
            first + 1
        ),
        IssueKind::TooMany { limit } => match i.at {
            At::Class(_) => limit_of("parameters per phase class", limit),
            _ => format!(
                "too big: at most {} unit procedures, {} steps, {} transitions, {} phase classes and {} parameters per step",
                recipe_core::MAX_UNIT_PROCS,
                recipe_core::MAX_STEPS,
                recipe_core::MAX_TRANSITIONS,
                recipe_core::MAX_CLASSES,
                recipe_core::MAX_PARAMS
            ),
        },
        IssueKind::EmptyRange => "the parameter's minimum is above its maximum".to_string(),
        IssueKind::NoSteps => "the recipe has no steps".to_string(),
        IssueKind::UnknownUnitProc(u) => format!(
            "{} names unit procedure \"{u}\", which the recipe does not have",
            step_of(r, i)
        ),
        IssueKind::UnknownClass(c) => format!(
            "{} runs phase class \"{c}\", which does not exist",
            step_of(r, i)
        ),
        IssueKind::UnknownParam(p) => {
            let class = match i.at {
                At::Arg(s, _) => r.steps.get(s).map(|s| s.class.as_str()).unwrap_or(""),
                _ => "",
            };
            let known: Vec<&str> = classes
                .iter()
                .find(|c| c.name == class)
                .map(|c| c.params.iter().map(|p| p.name.as_str()).collect())
                .unwrap_or_default();
            format!(
                "{class} has no parameter \"{p}\" (it has: {})",
                if known.is_empty() {
                    "none".into()
                } else {
                    known.join(", ")
                }
            )
        }
        IssueKind::DuplicateArg { first } => format!(
            "the parameter is given twice (first as argument {})",
            first + 1
        ),
        IssueKind::MissingParam(p) => format!("{} needs a value for \"{p}\"", step_of(r, i)),
        IssueKind::OutOfRange { value, min, max } => {
            format!("{value} is outside the phase class's range {min}..{max}")
        }
        IssueKind::EmptyTransition => {
            "a transition needs at least one step before it and one after it".to_string()
        }
        IssueKind::UnknownStep(s) => format!("there is no step \"{s}\""),
        IssueKind::RepeatedStep(s) => format!("\"{s}\" is named twice on one side"),
        IssueKind::TwoOutgoing { step, first } => format!(
            "{} already leads to transition {}; choosing between branches is not supported yet",
            step_name(r, step),
            first + 1
        ),
        IssueKind::TwoIncoming { step, first } => {
            format!(
                "{} is already started by transition {}",
                step_name(r, step),
                first + 1
            )
        }
        IssueKind::Cycle => {
            "this transition is part of a loop; a recipe runs each step once".to_string()
        }
        IssueKind::UnusedUnitProc => "no step runs in this unit procedure".to_string(),
    }
}

fn step_of(r: &RecipeIn, i: &Issue<'_>) -> String {
    match i.at {
        At::Step(s) | At::Arg(s, _) => step_name(r, s),
        _ => "the step".to_string(),
    }
}

fn place(at: At) -> (&'static str, Option<usize>, Option<usize>) {
    match at {
        At::Recipe => ("recipe", None, None),
        At::Class(c) => ("class", Some(c), None),
        At::ClassParam(c, p) => ("class", Some(c), Some(p)),
        At::UnitProc(u) => ("unitProc", Some(u), None),
        At::Step(s) => ("step", Some(s), None),
        At::Arg(s, a) => ("step", Some(s), Some(a)),
        At::Transition(t) => ("transition", Some(t), None),
    }
}

/// Checks the classes and the recipe.
pub fn check(r: &RecipeIn, classes: &[ClassIn]) -> Vec<RecipeIssue> {
    let issues: Vec<(At, Severity, String)> = with_recipe(r, classes, |view, cls| {
        let mut out = Vec::new();
        let mut push = |i: Issue<'_>| out.push((i.at, i.severity, message(r, classes, &i)));
        recipe::check_classes(cls, &mut push);
        recipe::check(view, cls, &mut push);
        out
    });
    issues
        .into_iter()
        .map(|(at, severity, message)| {
            let (at, index, sub) = place(at);
            RecipeIssue {
                at,
                index,
                sub,
                severity: match severity {
                    Severity::Error => "error",
                    Severity::Warning => "warning",
                },
                message,
            }
        })
        .collect()
}

fn reject_message(r: &RecipeIn, classes: &[ClassIn], reject: Reject) -> String {
    let class_of = |step: usize| {
        r.steps
            .get(step)
            .map(|s| s.class.clone())
            .unwrap_or_default()
    };
    match reject {
        Reject::Offline => "offline (discovery does not see it)".to_string(),
        Reject::UnitClass => "not of the unit class the procedure asks for".to_string(),
        Reject::MissingClass { step } => {
            format!("has no {} phase for {}", class_of(step), step_name(r, step))
        }
        Reject::OutOfRange {
            step,
            param,
            value,
            min,
            max,
        } => {
            let pname = classes
                .iter()
                .find(|c| c.name == class_of(step))
                .and_then(|c| c.params.get(param))
                .map(|p| p.name.clone())
                .unwrap_or_default();
            format!(
                "its {} phase takes {pname} {min}..{max}, but {} asks for {value}",
                class_of(step),
                step_name(r, step)
            )
        }
        Reject::PhaseBusy { step } => format!(
            "its {} phases are all taken by steps that run at the same time as {}",
            class_of(step),
            step_name(r, step)
        ),
    }
}

/// Runs `f` with the units as allocation views.
fn with_units<R>(units: &[UnitIn], f: impl FnOnce(&[Unit<'_>]) -> R) -> R {
    let phases: Vec<Vec<UnitPhase<'_>>> = units
        .iter()
        .map(|u| {
            u.phases
                .iter()
                .map(|p| UnitPhase {
                    em: &p.em,
                    name: &p.name,
                    class: &p.class,
                    ranges: &p.ranges,
                })
                .collect()
        })
        .collect();
    let views: Vec<Unit<'_>> = units
        .iter()
        .zip(&phases)
        .map(|(u, p)| Unit {
            id: &u.id,
            class: &u.class,
            online: u.online,
            phases: p,
        })
        .collect();
    f(&views)
}

type Planned = (recipe_core::Sfc, alloc::Allocation);

fn plan(r: &RecipeIn, classes: &[ClassIn], units: &[UnitIn]) -> Result<Planned, AllocFailure> {
    with_recipe(r, classes, |view, cls| {
        let sfc = recipe::compile(view, cls).map_err(|_| AllocFailure {
            message: "the recipe has errors; fix them first".to_string(),
            unit_proc: None,
            reasons: Vec::new(),
        })?;
        let up_class: Vec<Option<&str>> = r
            .unit_procs
            .iter()
            .map(|u| u.unit_class.as_deref())
            .collect();
        with_units(units, |us| {
            match alloc::allocate(&sfc, cls, &up_class, us) {
            Ok(a) => Ok((sfc, a)),
            Err(AllocError::TooMany) => Err(AllocFailure {
                message: format!(
                    "at most {} units with at most {} phases each",
                    recipe_core::MAX_UNITS,
                    recipe_core::MAX_UNIT_PHASES
                ),
                unit_proc: None,
                reasons: Vec::new(),
            }),
            Err(AllocError::NoUnit { up }) => {
                let why = alloc::explain(&sfc, cls, &up_class, us, up);
                let name = r.unit_procs.get(up).map(|u| u.id.clone()).unwrap_or_default();
                let reasons: Vec<(String, String)> = units
                    .iter()
                    .zip(why)
                    .filter_map(|(u, w)| w.map(|w| (u.id.clone(), reject_message(r, classes, w))))
                    .collect();
                let detail: Vec<String> = reasons.iter().map(|(u, w)| format!("{u}: {w}")).collect();
                Err(AllocFailure {
                    message: if detail.is_empty() {
                        format!("no unit can run unit procedure \"{name}\": there are no units")
                    } else {
                        format!("no unit can run unit procedure \"{name}\" ({})", detail.join("; "))
                    },
                    unit_proc: Some(name),
                    reasons,
                })
            }
            Err(AllocError::Contention { .. }) => Err(AllocFailure {
                message: "each unit runs one unit procedure at a time, and there are not enough suitable units for all of them".to_string(),
                unit_proc: None,
                reasons: Vec::new(),
            }),
        }
        })
    })
}

fn bindings(r: &RecipeIn, units: &[UnitIn], a: &alloc::Allocation) -> Vec<Binding> {
    r.steps
        .iter()
        .zip(a.step_phase.iter())
        .map(|(s, p)| {
            let unit = units.get(usize::from(p.unit));
            let ph = unit.and_then(|u| u.phases.get(usize::from(p.phase)));
            Binding {
                step: s.id.clone(),
                unit: unit.map(|u| u.id.clone()).unwrap_or_default(),
                em: ph.map(|p| p.em.clone()).unwrap_or_default(),
                phase: ph.map(|p| p.name.clone()).unwrap_or_default(),
            }
        })
        .collect()
}

/// Binds every step to a unit's equipment phase.
pub fn allocate(
    r: &RecipeIn,
    classes: &[ClassIn],
    units: &[UnitIn],
) -> Result<Vec<Binding>, AllocFailure> {
    let (_, a) = plan(r, classes, units)?;
    Ok(bindings(r, units, &a))
}

fn phase_name(units: &[UnitIn], p: alloc::PhaseRef) -> String {
    units
        .get(usize::from(p.unit))
        .and_then(|u| {
            u.phases
                .get(usize::from(p.phase))
                .map(|ph| format!("{}/{}.{}", u.id, ph.em, ph.name))
        })
        .unwrap_or_else(|| "?".to_string())
}

fn unit_name(units: &[UnitIn], u: u8) -> String {
    units
        .get(usize::from(u))
        .map_or_else(|| "?".to_string(), |u| u.id.clone())
}

fn effect_line(r: &RecipeIn, units: &[UnitIn], e: &Effect) -> (&'static str, String) {
    match *e {
        Effect::Send {
            step,
            phase,
            command,
            id,
            ..
        } => (
            "send",
            format!(
                "{} -> {} (#{id}, {})",
                command.name(),
                phase_name(units, phase),
                step_name(r, usize::from(step))
            ),
        ),
        Effect::Recipe { from, to } => {
            ("recipe", format!("recipe {} -> {}", from.name(), to.name()))
        }
        Effect::StepActive(s) => ("step", format!("{} active", step_name(r, usize::from(s)))),
        Effect::StepDone(s) => ("step", format!("{} done", step_name(r, usize::from(s)))),
        Effect::Refused { command, reason } => (
            "refused",
            match reason {
                Refusal::InvalidInState(s) => {
                    format!("{} refused: not valid while {}", command.name(), s.name())
                }
                Refusal::UnitOffline(u) => format!(
                    "{} refused: unit {} is offline",
                    command.name(),
                    unit_name(units, u)
                ),
                Refusal::PhaseUnknown(p) => format!(
                    "{} refused: the state of {} is unknown",
                    command.name(),
                    phase_name(units, p)
                ),
            },
        ),
        Effect::Failed { phase, failure } => {
            let on = phase
                .map(|p| format!(" on {}", phase_name(units, p)))
                .unwrap_or_default();
            let what = match failure {
                Failure::AckTimeout => "no acknowledgement in time".to_string(),
                Failure::SettleTimeout => "a transient state did not settle in time".to_string(),
                Failure::Request(e) => format!(
                    "the request failed ({})",
                    match e {
                        AckError::NotFound => "no such phase",
                        AckError::InvalidRequest => "invalid request",
                        AckError::Unavailable => "unavailable",
                        AckError::DeadlineExceeded => "deadline exceeded",
                        AckError::Transport => "transport error",
                    }
                ),
                Failure::CommandRejected(s) => {
                    format!("command rejected; the phase is {}", s.name())
                }
                Failure::SelfHeld => "the phase held itself".to_string(),
                Failure::Unexpected(s) => format!("the phase is {} without being told", s.name()),
                Failure::UnitLost(u) => format!("unit {} left the network", unit_name(units, u)),
                Failure::Budget => "the phase used up its command budget".to_string(),
            };
            let policy = match failure.resumable() {
                Some(true) => " (resumable: hold)",
                Some(false) => " (unresumable: abort)",
                None => "",
            };
            ("failed", format!("{what}{on}{policy}"))
        }
        Effect::Ignored(i) => (
            "ignored",
            match i {
                Ignored::StaleReport => "a stale report",
                Ignored::UnknownPhase => "a report for a phase the recipe does not use",
                Ignored::UnknownAck => "a reply to no outstanding command",
                Ignored::UnitOffline => "a message from an offline unit",
            }
            .to_string(),
        ),
    }
}

/// Allocates and runs the recipe against simulated units (each phase
/// Running for its `run_ms`, 20 ms per other acting state, acknowledgements
/// due in 500 ms, transient states in 2 s) from 0 to `until_ms` in 10 ms
/// ticks, following `script`.
pub fn simulate(
    r: &RecipeIn,
    classes: &[ClassIn],
    units: &[UnitIn],
    script: &ScriptIn,
    until_ms: u64,
) -> Result<Trace, AllocFailure> {
    let (sfc, a) = plan(r, classes, units)?;
    let cfg = Config {
        ack_timeout_ms: 500,
        settle_timeout_ms: 2_000,
        max_commands: 16,
    };
    let engine = Engine::new(sfc, &a, units.len(), cfg).map_err(|_| AllocFailure {
        message: "too many units".to_string(),
        unit_proc: None,
        reasons: Vec::new(),
    })?;
    let mut sims = SimUnits::new();
    for (u, unit) in units.iter().enumerate().take(recipe_core::MAX_UNITS) {
        for (p, ph) in unit
            .phases
            .iter()
            .enumerate()
            .take(recipe_core::MAX_UNIT_PHASES)
        {
            sims.phases[u][p] = SimPhase::idle(ph.run_ms);
        }
    }
    let index = |id: &str| units.iter().position(|u| u.id == id).map(|i| i as u8);
    let scripted: Vec<Scripted> = script
        .iter()
        .filter_map(|(at, e)| {
            let event = match e {
                ScriptEventIn::Operator(c) => Event::Operator(*c),
                ScriptEventIn::Lose(id) => Event::Lose(index(id)?),
                ScriptEventIn::Find(id) => Event::Find(index(id)?),
            };
            Some(Scripted { at_ms: *at, event })
        })
        .collect();
    let counts: Vec<usize> = units.iter().map(|u| u.phases.len()).collect();
    let mut lines = Vec::new();
    let out = sim::run(
        engine,
        sims,
        &counts,
        &scripted,
        until_ms,
        10,
        &mut |t, e| {
            let (kind, text) = effect_line(r, units, e);
            lines.push(TraceLine { t, kind, text });
        },
    );
    Ok(Trace {
        lines,
        state: out.engine.state().name(),
        bindings: bindings(r, units, &a),
    })
}

/// The command named `name` (`"start"`, `"hold"`, ... lower case).
pub fn command_named(name: &str) -> Option<Command> {
    Command::ALL
        .into_iter()
        .find(|c| c.name().eq_ignore_ascii_case(name))
}

/// State names, command names, and `next[state][input]`.
pub type PhaseModel = (
    Vec<&'static str>,
    Vec<&'static str>,
    Vec<Vec<Option<&'static str>>>,
);

/// The phase state model for the page: state names in code order, command
/// names in code order, and for each state the state each command (then
/// Done, last) leads to, `None` where it is rejected.
pub fn phase_model() -> PhaseModel {
    use recipe_core::phase::{Input, next};
    let inputs: Vec<Input> = Command::ALL
        .iter()
        .map(|c| Input::Command(*c))
        .chain([Input::Done])
        .collect();
    (
        PhaseState::ALL.iter().map(|s| s.name()).collect(),
        Command::ALL.iter().map(|c| c.name()).collect(),
        PhaseState::ALL
            .iter()
            .map(|s| {
                inputs
                    .iter()
                    .map(|i| next(*s, *i).map(PhaseState::name))
                    .collect()
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests;
