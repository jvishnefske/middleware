//! The worked example the tests share: master recipe "Blink then Measure"
//! with phase classes Blink and Measure, and two units, one with an LED
//! equipment module and one with a sensor equipment module.

use crate::alloc::{Allocation, Unit, UnitPhase, allocate};
use crate::engine::{Config, Engine};
use crate::recipe::{Arg, ParamDef, PhaseClass, Recipe, Sfc, Step, Transition, UnitProc, compile};

pub const BLINK_PARAMS: [ParamDef<'static>; 2] = [
    ParamDef {
        name: "period_ms",
        min: 10,
        max: 10_000,
    },
    ParamDef {
        name: "times",
        min: 1,
        max: 100,
    },
];
pub const MEASURE_PARAMS: [ParamDef<'static>; 1] = [ParamDef {
    name: "samples",
    min: 1,
    max: 16,
}];
pub const CLASSES: [PhaseClass<'static>; 2] = [
    PhaseClass {
        name: "Blink",
        params: &BLINK_PARAMS,
    },
    PhaseClass {
        name: "Measure",
        params: &MEASURE_PARAMS,
    },
];

pub const UPS: [UnitProc<'static>; 2] = [
    UnitProc {
        id: "Signal",
        unit_class: None,
    },
    UnitProc {
        id: "Sense",
        unit_class: Some("SensorUnit"),
    },
];
pub const BLINK_ARGS: [Arg<'static>; 2] = [
    Arg {
        name: "period_ms",
        value: 250,
    },
    Arg {
        name: "times",
        value: 3,
    },
];
pub const MEASURE_ARGS: [Arg<'static>; 1] = [Arg {
    name: "samples",
    value: 4,
}];
pub const STEPS: [Step<'static>; 2] = [
    Step {
        id: "BlinkLed",
        unit_proc: "Signal",
        class: "Blink",
        args: &BLINK_ARGS,
    },
    Step {
        id: "TakeReading",
        unit_proc: "Sense",
        class: "Measure",
        args: &MEASURE_ARGS,
    },
];
pub const TRANSITIONS: [Transition<'static>; 1] = [Transition {
    from: &["BlinkLed"],
    to: &["TakeReading"],
}];
pub const RECIPE: Recipe<'static> = Recipe {
    id: "BlinkThenMeasure",
    unit_procs: &UPS,
    steps: &STEPS,
    transitions: &TRANSITIONS,
};

pub const LED_PHASES: [UnitPhase<'static>; 1] = [UnitPhase {
    em: "LED",
    name: "Blink",
    class: "Blink",
    ranges: &[(50, 1_000), (1, 10)],
}];
pub const SENSOR_PHASES: [UnitPhase<'static>; 1] = [UnitPhase {
    em: "ADC",
    name: "Measure",
    class: "Measure",
    ranges: &[(1, 8)],
}];
/// Listed sensor first: allocation must not depend on the order.
pub const UNITS: [Unit<'static>; 2] = [
    Unit {
        id: "esp-sense",
        class: "SensorUnit",
        online: true,
        phases: &SENSOR_PHASES,
    },
    Unit {
        id: "pico-led",
        class: "LedUnit",
        online: true,
        phases: &LED_PHASES,
    },
];
/// Unit indices in [`UNITS`].
pub const SENSE: u8 = 0;
pub const LED: u8 = 1;

pub fn sfc() -> Sfc {
    compile(&RECIPE, &CLASSES).expect("the example compiles")
}

pub fn up_classes() -> [Option<&'static str>; 2] {
    [UPS[0].unit_class, UPS[1].unit_class]
}

pub fn allocation() -> Allocation {
    allocate(&sfc(), &CLASSES, &up_classes(), &UNITS).expect("the example allocates")
}

pub fn engine() -> Engine {
    Engine::new(sfc(), &allocation(), UNITS.len(), Config::DEFAULT).expect("engine")
}
