//! Tests of [`super::allocate`]: the worked example, each rejection
//! reason, deterministic tie-breaking and exclusive units.

use super::*;
use crate::fixture::{
    CLASSES, LED, LED_PHASES, RECIPE, SENSE, SENSOR_PHASES, UNITS, sfc, up_classes,
};
use crate::recipe::{Arg, Recipe, Step, Transition, UnitProc, compile};

#[test]
fn the_worked_example_binds_blink_to_the_led_unit_and_measure_to_the_sensor() {
    let a = allocate(&sfc(), &CLASSES, &up_classes(), &UNITS).unwrap();
    assert_eq!(a.up_unit[..2], [Some(LED), Some(SENSE)]);
    assert_eq!(
        a.step_phase[0],
        PhaseRef {
            unit: LED,
            phase: 0
        }
    );
    assert_eq!(
        a.step_phase[1],
        PhaseRef {
            unit: SENSE,
            phase: 0
        }
    );
}

#[test]
fn the_result_does_not_depend_on_the_order_units_are_listed_in() {
    let reversed = [UNITS[1], UNITS[0]];
    let a = allocate(&sfc(), &CLASSES, &up_classes(), &reversed).unwrap();
    // Same units, now at swapped indices.
    assert_eq!(a.up_unit[..2], [Some(0), Some(1)]);
}

#[test]
fn an_offline_or_wrong_class_unit_is_rejected_with_its_reason() {
    let units = [
        Unit {
            online: false,
            ..UNITS[0]
        },
        Unit {
            class: "Other",
            ..UNITS[1]
        },
    ];
    // Signal (no unit class asked) still fits the LED unit; Sense needs a
    // SensorUnit, and the only one is offline.
    assert_eq!(
        allocate(&sfc(), &CLASSES, &up_classes(), &units),
        Err(AllocError::NoUnit { up: 1 })
    );
    let reasons = explain(&sfc(), &CLASSES, &up_classes(), &units, 1);
    assert_eq!(
        reasons[..2],
        [Some(Reject::Offline), Some(Reject::UnitClass)]
    );
    assert_eq!(explain(&sfc(), &CLASSES, &up_classes(), &units, 0)[1], None);
}

#[test]
fn a_unit_without_the_class_or_with_a_narrower_range_is_rejected() {
    let args = [
        Arg {
            name: "period_ms",
            value: 5_000,
        },
        Arg {
            name: "times",
            value: 3,
        },
    ];
    let steps = [
        Step {
            args: &args,
            ..RECIPE.steps[0]
        },
        RECIPE.steps[1],
    ];
    let r = Recipe {
        steps: &steps,
        ..RECIPE
    };
    let s = compile(&r, &CLASSES).unwrap();
    assert_eq!(
        allocate(&s, &CLASSES, &up_classes(), &UNITS),
        Err(AllocError::NoUnit { up: 0 })
    );
    let reasons = explain(&s, &CLASSES, &up_classes(), &UNITS, 0);
    assert_eq!(
        reasons[usize::from(SENSE)],
        Some(Reject::MissingClass { step: 0 })
    );
    assert_eq!(
        reasons[usize::from(LED)],
        Some(Reject::OutOfRange {
            step: 0,
            param: 0,
            value: 5_000,
            min: 50,
            max: 1_000
        })
    );
}

#[test]
fn two_procedures_cannot_share_one_unit() {
    // Both procedures need a Measure phase; only one unit has one.
    let ups = [
        UnitProc {
            id: "Signal",
            unit_class: None,
        },
        UnitProc {
            id: "Sense",
            unit_class: None,
        },
    ];
    let a1 = [Arg {
        name: "samples",
        value: 1,
    }];
    let steps = [
        Step {
            id: "M1",
            unit_proc: "Signal",
            class: "Measure",
            args: &a1,
        },
        Step {
            id: "M2",
            unit_proc: "Sense",
            class: "Measure",
            args: &a1,
        },
    ];
    let r = Recipe {
        unit_procs: &ups,
        steps: &steps,
        transitions: &[],
        ..RECIPE
    };
    let s = compile(&r, &CLASSES).unwrap();
    assert_eq!(
        allocate(&s, &CLASSES, &[None, None], &UNITS),
        Err(AllocError::Contention { ups: 0b11 })
    );
}

#[test]
fn the_search_backtracks_to_find_an_exclusive_assignment() {
    // P0 can use A or B, P1 only A: the first choice (A for P0) must be
    // undone. Units A and B both offer Measure; only A offers Blink.
    let both: [UnitPhase<'static>; 2] = [LED_PHASES[0], SENSOR_PHASES[0]];
    let units = [
        Unit {
            id: "a",
            class: "U",
            online: true,
            phases: &both,
        },
        Unit {
            id: "b",
            class: "U",
            online: true,
            phases: &SENSOR_PHASES,
        },
    ];
    let ups = [
        UnitProc {
            id: "P0",
            unit_class: None,
        },
        UnitProc {
            id: "P1",
            unit_class: None,
        },
    ];
    let m = [Arg {
        name: "samples",
        value: 1,
    }];
    let b = [
        Arg {
            name: "period_ms",
            value: 100,
        },
        Arg {
            name: "times",
            value: 1,
        },
    ];
    let steps = [
        Step {
            id: "M",
            unit_proc: "P0",
            class: "Measure",
            args: &m,
        },
        Step {
            id: "B",
            unit_proc: "P1",
            class: "Blink",
            args: &b,
        },
    ];
    let r = Recipe {
        unit_procs: &ups,
        steps: &steps,
        transitions: &[],
        ..RECIPE
    };
    let s = compile(&r, &CLASSES).unwrap();
    let a = allocate(&s, &CLASSES, &[None, None], &units).unwrap();
    assert_eq!(a.up_unit[..2], [Some(1), Some(0)]);
}

#[test]
fn ties_go_to_the_smallest_unit_id_and_the_smallest_module_and_phase() {
    let two: [UnitPhase<'static>; 2] = [
        UnitPhase {
            em: "Z",
            ..SENSOR_PHASES[0]
        },
        UnitPhase {
            em: "A",
            ..SENSOR_PHASES[0]
        },
    ];
    let units = [
        Unit {
            id: "zeta",
            class: "SensorUnit",
            online: true,
            phases: &two,
        },
        Unit {
            id: "alpha",
            class: "SensorUnit",
            online: true,
            phases: &two,
        },
    ];
    let ups = [UnitProc {
        id: "Sense",
        unit_class: None,
    }];
    let m = [Arg {
        name: "samples",
        value: 1,
    }];
    let steps = [Step {
        id: "M",
        unit_proc: "Sense",
        class: "Measure",
        args: &m,
    }];
    let r = Recipe {
        unit_procs: &ups,
        steps: &steps,
        transitions: &[],
        ..RECIPE
    };
    let s = compile(&r, &CLASSES).unwrap();
    let a = allocate(&s, &CLASSES, &[None], &units).unwrap();
    assert_eq!(a.step_phase[0], PhaseRef { unit: 1, phase: 1 });
}

#[test]
fn concurrent_steps_take_different_phases_and_sequential_ones_may_share() {
    let two: [UnitPhase<'static>; 2] = [
        UnitPhase {
            em: "A",
            ..SENSOR_PHASES[0]
        },
        UnitPhase {
            em: "B",
            ..SENSOR_PHASES[0]
        },
    ];
    let units = [Unit {
        id: "u",
        class: "SensorUnit",
        online: true,
        phases: &two,
    }];
    let ups = [UnitProc {
        id: "Sense",
        unit_class: None,
    }];
    let m = [Arg {
        name: "samples",
        value: 1,
    }];
    let steps = [
        Step {
            id: "S",
            unit_proc: "Sense",
            class: "Measure",
            args: &m,
        },
        Step {
            id: "P",
            unit_proc: "Sense",
            class: "Measure",
            args: &m,
        },
        Step {
            id: "Q",
            unit_proc: "Sense",
            class: "Measure",
            args: &m,
        },
        Step {
            id: "T",
            unit_proc: "Sense",
            class: "Measure",
            args: &m,
        },
    ];
    let t = [
        Transition {
            from: &["S"],
            to: &["P", "Q"],
        },
        Transition {
            from: &["P", "Q"],
            to: &["T"],
        },
    ];
    let r = Recipe {
        unit_procs: &ups,
        steps: &steps,
        transitions: &t,
        ..RECIPE
    };
    let s = compile(&r, &CLASSES).unwrap();
    let a = allocate(&s, &CLASSES, &[None], &units).unwrap();
    let phases: [u8; 4] = core::array::from_fn(|i| a.step_phase[i].phase);
    assert_eq!(phases, [0, 0, 1, 0]);

    let one = [Unit {
        phases: &two[..1],
        ..units[0]
    }];
    assert_eq!(
        allocate(&s, &CLASSES, &[None], &one),
        Err(AllocError::NoUnit { up: 0 })
    );
    assert_eq!(
        explain(&s, &CLASSES, &[None], &one, 0)[0],
        Some(Reject::PhaseBusy { step: 2 })
    );
}

#[test]
fn too_many_units_is_refused() {
    let many = [UNITS[0]; MAX_UNITS + 1];
    assert_eq!(
        allocate(&sfc(), &CLASSES, &up_classes(), &many),
        Err(AllocError::TooMany)
    );
}
