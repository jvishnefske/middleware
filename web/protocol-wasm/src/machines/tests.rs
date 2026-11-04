//! Message tests for [`super::check`]: each rule's wording, place and fix.

use super::*;

fn pins() -> Vec<PinIn> {
    [
        ("LED", Func::GpioOut),
        ("BUTTON", Func::GpioIn),
        ("DOOR", Func::GpioIn),
        ("POT", Func::Adc),
    ]
    .into_iter()
    .map(|(l, f)| PinIn {
        label: l.into(),
        func: Ok(f),
    })
    .collect()
}

fn pin(label: &str) -> Expr {
    Expr::Pin(label.into())
}

fn eq(label: &str, v: i32) -> Expr {
    Expr::Cmp(CmpOp::Eq, Box::new(pin(label)), Box::new(Expr::Const(v)))
}

fn row(state: &str, event: &str, guard: Option<Expr>, next: &str, actions: Vec<ActionIn>) -> RowIn {
    RowIn {
        state: state.into(),
        event: event.into(),
        guard: guard.as_ref().map(flatten),
        next: next.into(),
        actions,
    }
}

fn st(name: &str) -> StateIn {
    StateIn {
        name: name.into(),
        entry: vec![],
    }
}

fn toggle(label: &str) -> ActionIn {
    ActionIn::Toggle {
        label: label.into(),
    }
}

/// Off/On with PRESS, toggling LED on BUTTON = 1 and returning when On.
fn mode() -> MachineIn {
    MachineIn {
        id: "Mode".into(),
        initial: "Off".into(),
        states: vec![st("Off"), st("On")],
        events: vec!["PRESS".into()],
        rows: vec![
            row(
                "Off",
                "PRESS",
                Some(eq("BUTTON", 1)),
                "On",
                vec![toggle("LED")],
            ),
            row("On", "PRESS", None, "Off", vec![toggle("LED")]),
        ],
    }
}

/// `"<list> <row|-> <field> <severity>: <message>"` per issue.
fn lines(ms: &[MachineIn]) -> Vec<String> {
    check(ms, &pins())
        .iter()
        .map(|i| {
            let row = i.row.map_or("-".into(), |r| r.to_string());
            format!("{} {row} {} {}: {}", i.list, i.field, i.severity, i.message)
        })
        .collect()
}

#[test]
fn a_valid_machine_produces_no_messages() {
    assert_eq!(lines(&[mode()]), Vec::<String>::new());
}

#[test]
fn flatten_puts_children_first_and_the_root_last() {
    let e = Expr::And(
        Box::new(eq("A", 1)),
        Box::new(Expr::Not(Box::new(pin("B")))),
    );
    assert_eq!(
        flatten(&e),
        [
            NodeIn::Pin("A".into()),
            NodeIn::Const(1),
            NodeIn::Cmp(CmpOp::Eq, 0, 1),
            NodeIn::Pin("B".into()),
            NodeIn::Not(3),
            NodeIn::And(2, 4),
        ]
    );
}

#[test]
fn flatten_cuts_deep_expressions_at_one_past_the_limit() {
    let mut e = pin("A");
    for _ in 0..50 {
        e = Expr::Not(Box::new(e));
    }
    let nodes = flatten(&e);
    assert_eq!(nodes.len(), 5);
    assert_eq!(nodes[0], NodeIn::Const(0));
    let mut m = mode();
    m.rows[0].guard = Some(nodes);
    assert_eq!(
        lines(&[m]),
        ["rows 0 guard error: This condition is nested more than 4 levels deep; simplify it."]
    );
}

#[test]
fn machine_id_messages_name_the_other_machine() {
    let mut b = mode();
    b.id = "MODE".into();
    let mut c = mode();
    c.id = String::new();
    let mut d = mode();
    d.id = "1x".into();
    assert_eq!(
        lines(&[mode(), b, c, d]),
        [
            "machine - id error: The id Mode is also used by machine 2; machine ids must be unique (ignoring case).",
            "machine - id error: The id MODE is also used by machine 1; machine ids must be unique (ignoring case).",
            "machine - id error: Give this machine an id, for example Mode.",
            "machine - id error: A machine id starts with a letter and has only letters, digits and _ (at most 24).",
        ]
    );
}

#[test]
fn size_messages_give_the_count_and_the_limit() {
    let mut m = mode();
    m.states = (0..17).map(|k| st(&format!("S{k}"))).collect();
    assert_eq!(
        lines(&[m]),
        [
            "machine - size error: This machine has 17 states; at most 16 are allowed. Remove some or split the machine."
        ]
    );
    let mut m = mode();
    m.states.clear();
    let got = check(&[m], &pins());
    assert_eq!(got[0].message, "Add a state; a machine needs at least one.");
}

#[test]
fn name_messages_say_what_a_name_looks_like() {
    let mut m = mode();
    m.states.push(st("off"));
    m.states.push(st(""));
    m.events.push("bad event".into());
    let got: Vec<String> = lines(&[m])
        .into_iter()
        .filter(|l| l.contains(" name error"))
        .collect();
    assert_eq!(
        got,
        [
            "states 0 name error: The name Off is also used by state 3; state names must be unique (ignoring case).",
            "states 2 name error: The name off is also used by state 1; state names must be unique (ignoring case).",
            "states 3 name error: Give this state a name, for example Idle.",
            "events 1 name error: An event name starts with a letter and has only letters, digits and _ (at most 24).",
        ]
    );
}

#[test]
fn unknown_initial_offers_the_first_state() {
    let mut m = mode();
    m.initial = "Idle".into();
    let got = check(&[m], &pins());
    assert_eq!(got.len(), 1);
    assert_eq!(
        got[0].message,
        "The initial state Idle is not a state of this machine; choose one, for example Off."
    );
    assert_eq!(got[0].fix, Some(MachineFix::SetInitial("Off".into())));
}

#[test]
fn unknown_state_and_event_messages_offer_to_add_them() {
    let mut m = mode();
    m.rows.push(row("Busy", "HOLD", None, "on", vec![]));
    m.rows.push(row("", "", None, "", vec![]));
    let got = check(&[m], &pins());
    let msgs: Vec<(&str, &str, Option<MachineFix>)> = got
        .iter()
        .filter(|i| i.severity == "error")
        .map(|i| (i.field, i.message.as_str(), i.fix.clone()))
        .collect();
    assert_eq!(
        msgs,
        [
            (
                "state",
                "There is no state Busy; add it or choose another.",
                Some(MachineFix::AddState("Busy".into()))
            ),
            ("next", "There is no state on. Did you mean On?", None),
            (
                "event",
                "There is no event HOLD; add it or choose another.",
                Some(MachineFix::AddEvent("HOLD".into()))
            ),
            ("state", "Choose the state this row fires in.", None),
            ("next", "Choose the state this row moves to.", None),
            ("event", "Choose the event this row fires on.", None),
        ]
    );
}

#[test]
fn guard_pin_messages_name_the_label_and_its_function() {
    let mut m = mode();
    let g = Expr::Or(
        Box::new(Expr::And(Box::new(eq("POT", 1)), Box::new(eq("SENSOR", 1)))),
        Box::new(eq("door", 0)),
    );
    m.rows[0].guard = Some(flatten(&g));
    let got = check(&[m], &pins());
    let msgs: Vec<(&str, Option<MachineFix>)> = got
        .iter()
        .map(|i| (i.message.as_str(), i.fix.clone()))
        .collect();
    assert_eq!(
        msgs,
        [
            (
                "The condition reads POT, which is an analog input; a condition can read only GPIO inputs and outputs.",
                None
            ),
            (
                "The condition reads SENSOR, which is not in the pin table; add it as a GPIO input.",
                Some(MachineFix::AddPin {
                    label: "SENSOR".into(),
                    func: Func::GpioIn
                })
            ),
            ("There is no pin door. Did you mean DOOR?", None),
        ]
    );
}

#[test]
fn action_pin_messages_number_the_action() {
    let mut m = mode();
    m.rows[0].actions = vec![
        toggle("BUTTON"),
        ActionIn::Write {
            label: "BUZZER".into(),
            level: true,
        },
    ];
    let got = check(&[m], &pins());
    assert_eq!(
        got.iter().map(|i| i.message.as_str()).collect::<Vec<_>>(),
        [
            "Action 1 drives BUTTON, which is a GPIO input; only a GPIO output can be written or toggled.",
            "Action 2 drives BUZZER, which is not in the pin table; add it as a GPIO output.",
        ]
    );
    assert_eq!(
        got[1].fix,
        Some(MachineFix::AddPin {
            label: "BUZZER".into(),
            func: Func::GpioOut
        })
    );
}

#[test]
fn publish_messages_use_the_middleware_key_rule() {
    let publish = |key: &str| ActionIn::Publish {
        key: key.into(),
        text: "x".into(),
    };
    let mut m = mode();
    m.rows[0].actions = vec![
        publish("sys/reboot"),
        publish("a//b"),
        publish("led state"),
        publish(""),
    ];
    m.rows[1].actions = vec![
        publish(&"k".repeat(129)),
        ActionIn::Log {
            text: "y".repeat(513),
        },
    ];
    assert_eq!(
        lines(&[m]),
        [
            "rows 0 actions error: Action 1 publishes on sys/reboot, but keys under sys/ belong to the firmware; choose another key.",
            "rows 0 actions error: Action 2 publishes on a//b, which has an empty part (a leading, trailing or doubled /).",
            "rows 0 actions error: Action 3 publishes on led state, which has ' '; keys use only letters, digits, _, -, . and /.",
            "rows 0 actions error: Action 4 publishes on no key; give it one, for example led/state.",
            "rows 1 actions error: Action 1 publishes on a key longer than 128 bytes; shorten it.",
            "rows 1 actions error: Action 2's text is 513 bytes; at most 512 fit in a message.",
        ]
    );
}

#[test]
fn raise_and_action_count_messages() {
    let mut m = mode();
    m.states[1].entry = vec![
        ActionIn::Raise {
            event: "TICK".into(),
        },
        ActionIn::Raise {
            event: "press".into(),
        },
        ActionIn::Log { text: "a".into() },
        ActionIn::Log { text: "b".into() },
        ActionIn::Log { text: "c".into() },
    ];
    let got = check(&[m], &pins());
    assert_eq!(
        got.iter()
            .map(|i| (i.list, i.row, i.field, i.message.as_str()))
            .collect::<Vec<_>>(),
        [
            (
                "states",
                Some(1),
                "entry",
                "This state's entry has 5 actions; at most 4 are allowed."
            ),
            (
                "states",
                Some(1),
                "entry",
                "Action 1 raises TICK, which is not an event of this machine; add it or choose another."
            ),
            (
                "states",
                Some(1),
                "entry",
                "Action 2 raises press, which is not an event. Did you mean PRESS?"
            ),
        ]
    );
    assert_eq!(got[1].fix, Some(MachineFix::AddEvent("TICK".into())));
}

#[test]
fn cell_messages_name_the_earlier_row() {
    let mut m = mode();
    m.rows
        .push(row("Off", "PRESS", Some(eq("DOOR", 1)), "Off", vec![]));
    m.rows.push(row("On", "PRESS", None, "On", vec![]));
    assert_eq!(
        lines(&[m]),
        [
            "rows 2 guard error: This condition and row 1's can both be true (when BUTTON = 1 and DOOR = 1), so which row fires depends on their order; make them exclusive.",
            "rows 3 guard error: Row 2 already handles PRESS in On without a condition; give this row a condition or remove one of them.",
        ]
    );
}

#[test]
fn warning_messages_for_never_true_unreachable_and_dead() {
    let mut m = mode();
    m.states.push(st("Island"));
    m.events.push("HOLD".into());
    m.rows[0].guard = Some(flatten(&Expr::And(
        Box::new(eq("BUTTON", 1)),
        Box::new(eq("BUTTON", 0)),
    )));
    assert_eq!(
        lines(&[m]),
        [
            "rows 0 guard warning: This condition is never true, so the row never fires.",
            "states 2 name warning: No row leads from the initial state Off to Island, so the machine never enters it.",
            "events 1 name warning: No row handles HOLD, so raising it does nothing.",
        ]
    );
}

#[test]
fn unknown_pin_functions_are_not_digital() {
    let mut ps = pins();
    ps.push(PinIn {
        label: "X".into(),
        func: Err("laser".into()),
    });
    let mut m = mode();
    m.rows[0].guard = Some(flatten(&eq("X", 1)));
    let got = check(&[m], &ps);
    assert_eq!(
        got[0].message,
        "The condition reads X, which is a pin of unknown function; a condition can read only GPIO inputs and outputs."
    );
}
