//! Tests of the recipe checks, allocation and simulation as the page sees
//! them: the worked example "Blink then Measure" and the messages.

use super::*;

fn classes() -> Vec<ClassIn> {
    example_classes()
}

fn recipe() -> RecipeIn {
    example_recipe()
}

fn units() -> Vec<UnitIn> {
    example_units()
}

pub(super) fn example_classes() -> Vec<ClassIn> {
    let p = |name: &str, min, max| ParamIn {
        name: name.into(),
        min,
        max,
    };
    vec![
        ClassIn {
            name: "Blink".into(),
            params: vec![p("period_ms", 10, 10_000), p("times", 1, 100)],
        },
        ClassIn {
            name: "Measure".into(),
            params: vec![p("samples", 1, 16)],
        },
    ]
}

pub(super) fn example_recipe() -> RecipeIn {
    RecipeIn {
        id: "BlinkThenMeasure".into(),
        unit_procs: vec![
            UnitProcIn {
                id: "Signal".into(),
                unit_class: None,
            },
            UnitProcIn {
                id: "Sense".into(),
                unit_class: Some("SensorUnit".into()),
            },
        ],
        steps: vec![
            StepIn {
                id: "BlinkLed".into(),
                unit_proc: "Signal".into(),
                class: "Blink".into(),
                args: vec![("period_ms".into(), 250), ("times".into(), 3)],
            },
            StepIn {
                id: "TakeReading".into(),
                unit_proc: "Sense".into(),
                class: "Measure".into(),
                args: vec![("samples".into(), 4)],
            },
        ],
        transitions: vec![TransitionIn {
            from: vec!["BlinkLed".into()],
            to: vec!["TakeReading".into()],
        }],
    }
}

pub(super) fn example_units() -> Vec<UnitIn> {
    vec![
        UnitIn {
            id: "pico-led".into(),
            class: "LedUnit".into(),
            online: true,
            phases: vec![UnitPhaseIn {
                em: "LED".into(),
                name: "Blink".into(),
                class: "Blink".into(),
                ranges: vec![(50, 1_000), (1, 10)],
                run_ms: 300,
            }],
        },
        UnitIn {
            id: "pico-sense".into(),
            class: "SensorUnit".into(),
            online: true,
            phases: vec![UnitPhaseIn {
                em: "ADC".into(),
                name: "Measure".into(),
                class: "Measure".into(),
                ranges: vec![(1, 8)],
                run_ms: 100,
            }],
        },
    ]
}

#[test]
fn the_worked_example_checks_clean_and_binds_each_step() {
    assert_eq!(check(&recipe(), &classes()), Vec::new());
    let b = allocate(&recipe(), &classes(), &units()).unwrap();
    assert_eq!(
        b,
        [
            Binding {
                step: "BlinkLed".into(),
                unit: "pico-led".into(),
                em: "LED".into(),
                phase: "Blink".into()
            },
            Binding {
                step: "TakeReading".into(),
                unit: "pico-sense".into(),
                em: "ADC".into(),
                phase: "Measure".into()
            },
        ]
    );
}

#[test]
fn check_messages_name_the_step_and_the_rule() {
    let mut r = recipe();
    r.steps[0].args[0].1 = 5;
    r.steps[1].class = "Spin".into();
    r.transitions[0].to = vec!["Ghost".into()];
    let lines: Vec<String> = check(&r, &classes())
        .into_iter()
        .map(|i| format!("{} {:?} {}: {}", i.at, i.index, i.severity, i.message))
        .collect();
    assert_eq!(
        lines,
        [
            "step Some(0) error: 5 is outside the phase class's range 10..10000",
            "step Some(1) error: step \"TakeReading\" runs phase class \"Spin\", which does not exist",
            "transition Some(0) error: there is no step \"Ghost\"",
        ]
    );
}

#[test]
fn q18_issues_carry_one_click_fixes() {
    let mut r = recipe();
    r.steps[0].args = vec![("period_ms".into(), 5), ("speed".into(), 1)];
    r.transitions.push(TransitionIn {
        from: vec!["TakeReading".into()],
        to: vec!["BlinkLed".into()],
    });
    r.transitions.push(TransitionIn {
        from: vec!["BlinkLed".into()],
        to: vec!["TakeReading".into()],
    });
    r.unit_procs.push(UnitProcIn {
        id: "Spare".into(),
        unit_class: None,
    });
    let fixes: Vec<(String, Option<RecipeFix>)> = check(&r, &classes())
        .into_iter()
        .map(|i| (i.message, i.fix))
        .collect();
    let has = |f: RecipeFix| fixes.iter().any(|(_, x)| x.as_ref() == Some(&f));
    assert!(
        has(RecipeFix::SetArg {
            step: 0,
            param: "period_ms".into(),
            value: 10
        }),
        "{fixes:#?}"
    );
    assert!(has(RecipeFix::SetArg {
        step: 0,
        param: "times".into(),
        value: 1
    }));
    assert!(has(RecipeFix::RemoveArg {
        step: 0,
        param: "speed".into()
    }));
    assert!(has(RecipeFix::RemoveFromTransition {
        transition: 2,
        side: "from",
        step: "BlinkLed".into()
    }));
    assert!(has(RecipeFix::RemoveFromTransition {
        transition: 2,
        side: "to",
        step: "TakeReading".into()
    }));
    assert!(has(RecipeFix::RemoveTransition(1)));
    assert!(has(RecipeFix::RemoveUnitProc(2)));
}

#[test]
fn allocation_failures_say_per_unit_why() {
    let mut us = units();
    us[1].online = false;
    let f = allocate(&recipe(), &classes(), &us).unwrap_err();
    assert_eq!(f.unit_proc.as_deref(), Some("Sense"));
    assert_eq!(
        f.message,
        "no unit can run unit procedure \"Sense\" (pico-led: not of the unit class the procedure asks for; pico-sense: offline (discovery does not see it))"
    );
    let mut r = recipe();
    r.steps[0].args[0].1 = 5_000;
    let f = allocate(&r, &classes(), &units()).unwrap_err();
    assert!(f.message.contains("pico-led: its Blink phase takes period_ms 50..1000, but step \"BlinkLed\" asks for 5000"), "{}", f.message);
    assert!(
        f.message
            .contains("pico-sense: has no Blink phase for step \"BlinkLed\""),
        "{}",
        f.message
    );
}

#[test]
fn a_simulated_run_completes_and_a_hold_and_a_unit_loss_show_in_the_trace() {
    let t = simulate(
        &recipe(),
        &classes(),
        &units(),
        &vec![(0, ScriptEventIn::Operator(Command::Start))],
        2_000,
    )
    .unwrap();
    assert_eq!(t.state, "Complete");
    assert_eq!(t.lines[0].text, "recipe Idle -> Running");
    assert!(t.lines.iter().any(
        |l| l.t == 300 && l.text == "Start -> pico-sense/ADC.Measure (#3, step \"TakeReading\")"
    ));

    let script = vec![
        (0, ScriptEventIn::Operator(Command::Start)),
        (100, ScriptEventIn::Operator(Command::Hold)),
        (400, ScriptEventIn::Operator(Command::Restart)),
    ];
    let t = simulate(&recipe(), &classes(), &units(), &script, 3_000).unwrap();
    assert_eq!(t.state, "Complete");
    assert!(
        t.lines
            .iter()
            .any(|l| l.t == 120 && l.text == "recipe Holding -> Held")
    );

    let script = vec![
        (0, ScriptEventIn::Operator(Command::Start)),
        (100, ScriptEventIn::Lose("pico-led".into())),
        (300, ScriptEventIn::Operator(Command::Abort)),
    ];
    let t = simulate(&recipe(), &classes(), &units(), &script, 1_000).unwrap();
    assert_eq!(t.state, "Aborted");
    assert!(t.lines.iter().any(
        |l| l.kind == "failed" && l.text == "unit pico-led left the network (resumable: hold)"
    ));
}

#[test]
fn the_phase_model_lists_states_commands_and_the_matrix() {
    let (states, commands, next) = phase_model();
    assert_eq!(states.len(), 11);
    assert_eq!(
        commands,
        ["Start", "Hold", "Restart", "Stop", "Abort", "Reset"]
    );
    assert_eq!(
        next[0],
        [Some("Running"), None, None, None, None, None, None]
    );
    assert_eq!(next[1][6], Some("Complete"));
    assert_eq!(command_named("hold"), Some(Command::Hold));
}
