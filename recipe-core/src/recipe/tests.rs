//! Tests of [`super::check`] and [`super::compile`]: one per rule, and the
//! compiled chart of the worked example.

use super::*;
use crate::fixture::{CLASSES, RECIPE, STEPS, UPS};
use std::vec::Vec;

fn issues<'a>(r: &Recipe<'a>, classes: &[PhaseClass<'a>]) -> Vec<Issue<'a>> {
    let mut out = Vec::new();
    check_classes(classes, &mut |i| out.push(i));
    check(r, classes, &mut |i| out.push(i));
    out
}

fn kinds<'a>(r: &Recipe<'a>) -> Vec<(At, IssueKind<'a>)> {
    issues(r, &CLASSES)
        .into_iter()
        .map(|i| (i.at, i.kind))
        .collect()
}

fn step<'a>(id: &'a str, up: &'a str, class: &'a str, args: &'a [Arg<'a>]) -> Step<'a> {
    Step {
        id,
        unit_proc: up,
        class,
        args,
    }
}

const NO_ARGS: [Arg<'static>; 0] = [];
const SAMPLES: [Arg<'static>; 1] = [Arg {
    name: "samples",
    value: 2,
}];

#[test]
fn the_worked_example_has_no_issues_and_compiles_to_a_two_step_chain() {
    assert_eq!(issues(&RECIPE, &CLASSES), Vec::new());
    let sfc = compile(&RECIPE, &CLASSES).unwrap();
    assert_eq!((sfc.steps, sfc.unit_procs, sfc.transitions), (2, 2, 1));
    assert_eq!(sfc.initial, 0b01);
    assert_eq!((sfc.from[0], sfc.to[0]), (0b01, 0b10));
    assert_eq!(sfc.args[0][..2], [250, 3]);
    assert_eq!(sfc.args[1][0], 4);
    assert_eq!((sfc.step_class[1], sfc.step_up[1]), (1, 1));
    assert_eq!(sfc.before[1], 0b01);
    assert!(!sfc.concurrent(0, 1));
}

#[test]
fn ids_must_be_identifiers_unique_ignoring_case() {
    let steps = [
        STEPS[0],
        Step {
            id: "blinkled",
            ..STEPS[1]
        },
        Step {
            id: "2nd",
            ..STEPS[1]
        },
    ];
    let r = Recipe {
        id: "has space",
        steps: &steps,
        transitions: &[],
        ..RECIPE
    };
    let k = kinds(&r);
    assert!(k.contains(&(At::Recipe, IssueKind::BadIdent)));
    assert!(k.contains(&(At::Step(1), IssueKind::Duplicate { first: 0 })));
    assert!(k.contains(&(At::Step(2), IssueKind::BadIdent)));
}

#[test]
fn class_rules_cover_names_parameter_count_and_ranges() {
    let params = [
        ParamDef {
            name: "a",
            min: 2,
            max: 1,
        },
        ParamDef {
            name: "A",
            min: 0,
            max: 0,
        },
    ];
    let five = [ParamDef {
        name: "p",
        min: 0,
        max: 0,
    }; 5];
    let classes = [
        PhaseClass {
            name: "X",
            params: &params,
        },
        PhaseClass {
            name: "x",
            params: &[],
        },
        PhaseClass {
            name: "Y",
            params: &five,
        },
    ];
    let mut out = Vec::new();
    check_classes(&classes, &mut |i| out.push((i.at, i.kind)));
    assert_eq!(
        out,
        [
            (At::Class(1), IssueKind::Duplicate { first: 0 }),
            (At::ClassParam(0, 1), IssueKind::Duplicate { first: 0 }),
            (At::ClassParam(0, 0), IssueKind::EmptyRange),
            (At::Class(2), IssueKind::TooMany { limit: MAX_PARAMS }),
        ]
    );
}

#[test]
fn a_step_needs_a_known_unit_procedure_and_class() {
    let steps = [
        step("A", "Nowhere", "Blink", &NO_ARGS),
        step("B", "Sense", "Spin", &NO_ARGS),
    ];
    let r = Recipe {
        steps: &steps,
        transitions: &[],
        ..RECIPE
    };
    let k = kinds(&r);
    assert!(k.contains(&(At::Step(0), IssueKind::UnknownUnitProc("Nowhere"))));
    assert!(k.contains(&(At::Step(1), IssueKind::UnknownClass("Spin"))));
}

#[test]
fn arguments_name_class_parameters_once_cover_all_and_lie_in_range() {
    let args = [
        Arg {
            name: "period_ms",
            value: 5,
        },
        Arg {
            name: "speed",
            value: 1,
        },
        Arg {
            name: "period_ms",
            value: 20,
        },
    ];
    let steps = [step("A", "Signal", "Blink", &args)];
    let r = Recipe {
        steps: &steps,
        transitions: &[],
        ..RECIPE
    };
    assert_eq!(
        kinds(&r)
            .into_iter()
            .filter(|(at, _)| matches!(at, At::Arg(..) | At::Step(_)))
            .collect::<Vec<_>>(),
        [
            (
                At::Arg(0, 0),
                IssueKind::OutOfRange {
                    value: 5,
                    min: 10,
                    max: 10_000
                }
            ),
            (At::Arg(0, 1), IssueKind::UnknownParam("speed")),
            (At::Arg(0, 2), IssueKind::DuplicateArg { first: 0 }),
            (At::Step(0), IssueKind::MissingParam("times")),
        ]
    );
}

#[test]
fn transitions_need_both_sides_known_steps_and_no_repeats() {
    let t = [
        Transition {
            from: &[],
            to: &["TakeReading"],
        },
        Transition {
            from: &["Ghost"],
            to: &["TakeReading"],
        },
        Transition {
            from: &["BlinkLed", "BlinkLed"],
            to: &["TakeReading"],
        },
    ];
    let r = Recipe {
        transitions: &t,
        ..RECIPE
    };
    let k = kinds(&r);
    assert!(k.contains(&(At::Transition(0), IssueKind::EmptyTransition)));
    assert!(k.contains(&(At::Transition(1), IssueKind::UnknownStep("Ghost"))));
    assert!(k.contains(&(At::Transition(2), IssueKind::RepeatedStep("BlinkLed"))));
}

#[test]
fn a_step_ends_at_most_one_transition_and_starts_from_at_most_one() {
    let steps = [
        step("A", "Sense", "Measure", &SAMPLES),
        step("B", "Sense", "Measure", &SAMPLES),
        step("C", "Sense", "Measure", &SAMPLES),
    ];
    let t = [
        Transition {
            from: &["A"],
            to: &["B"],
        },
        Transition {
            from: &["A"],
            to: &["C"],
        },
        Transition {
            from: &["C"],
            to: &["B"],
        },
    ];
    let r = Recipe {
        steps: &steps,
        transitions: &t,
        ..RECIPE
    };
    let k = kinds(&r);
    assert!(k.contains(&(
        At::Transition(1),
        IssueKind::TwoOutgoing { step: 0, first: 0 }
    )));
    assert!(k.contains(&(
        At::Transition(2),
        IssueKind::TwoIncoming { step: 1, first: 0 }
    )));
}

#[test]
fn cycles_are_errors_on_each_of_their_transitions() {
    let steps = [
        step("A", "Sense", "Measure", &SAMPLES),
        step("B", "Sense", "Measure", &SAMPLES),
        step("C", "Sense", "Measure", &SAMPLES),
    ];
    let t = [
        Transition {
            from: &["B"],
            to: &["C"],
        },
        Transition {
            from: &["C"],
            to: &["B"],
        },
    ];
    let r = Recipe {
        steps: &steps,
        transitions: &t,
        ..RECIPE
    };
    let k: Vec<_> = kinds(&r)
        .into_iter()
        .filter(|(_, k)| *k == IssueKind::Cycle)
        .collect();
    assert_eq!(
        k,
        [
            (At::Transition(0), IssueKind::Cycle),
            (At::Transition(1), IssueKind::Cycle)
        ]
    );
}

#[test]
fn a_convergence_waiting_on_its_own_successor_is_a_cycle() {
    // B is started only by {A, C} -> B, and C only by B -> C.
    let steps = [
        step("A", "Sense", "Measure", &SAMPLES),
        step("B", "Sense", "Measure", &SAMPLES),
        step("C", "Sense", "Measure", &SAMPLES),
    ];
    let t = [
        Transition {
            from: &["A", "C"],
            to: &["B"],
        },
        Transition {
            from: &["B"],
            to: &["C"],
        },
    ];
    let r = Recipe {
        steps: &steps,
        transitions: &t,
        ..RECIPE
    };
    let k = kinds(&r);
    assert!(k.contains(&(At::Transition(0), IssueKind::Cycle)));
    assert!(k.contains(&(At::Transition(1), IssueKind::Cycle)));
}

#[test]
fn parallel_divergence_and_convergence_compile_with_concurrency() {
    let steps = [
        step("A", "Sense", "Measure", &SAMPLES),
        step("B", "Sense", "Measure", &SAMPLES),
        step("C", "Sense", "Measure", &SAMPLES),
        step("D", "Sense", "Measure", &SAMPLES),
    ];
    let t = [
        Transition {
            from: &["A"],
            to: &["B", "C"],
        },
        Transition {
            from: &["B", "C"],
            to: &["D"],
        },
    ];
    let r = Recipe {
        steps: &steps,
        transitions: &t,
        ..RECIPE
    };
    let k: Vec<_> = kinds(&r)
        .into_iter()
        .filter(|(at, _)| !matches!(at, At::UnitProc(_)))
        .collect();
    assert_eq!(k, Vec::new());
    let sfc = compile(&r, &CLASSES).unwrap();
    assert_eq!(sfc.initial, 0b0001);
    assert!(sfc.concurrent(1, 2));
    assert!(!sfc.concurrent(0, 1));
    assert_eq!(sfc.before[3], 0b0111);
}

#[test]
fn a_recipe_needs_steps_and_respects_its_limits() {
    let r = Recipe {
        steps: &[],
        transitions: &[],
        ..RECIPE
    };
    assert!(kinds(&r).contains(&(At::Recipe, IssueKind::NoSteps)));
    let many = [STEPS[0]; MAX_STEPS + 1];
    let r = Recipe {
        steps: &many,
        transitions: &[],
        ..RECIPE
    };
    assert_eq!(
        kinds(&r),
        [(At::Recipe, IssueKind::TooMany { limit: MAX_STEPS })]
    );
}

#[test]
fn an_unused_unit_procedure_is_only_a_warning() {
    let ups = [
        UPS[0],
        UPS[1],
        UnitProc {
            id: "Spare",
            unit_class: None,
        },
    ];
    let r = Recipe {
        unit_procs: &ups,
        ..RECIPE
    };
    let all = issues(&r, &CLASSES);
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].severity, Severity::Warning);
    assert_eq!(all[0].kind, IssueKind::UnusedUnitProc);
    assert!(compile(&r, &CLASSES).is_ok());
}

#[test]
fn compile_refuses_a_recipe_with_errors() {
    let r = Recipe { id: "", ..RECIPE };
    assert_eq!(compile(&r, &CLASSES), Err(CompileError::Invalid));
}
