//! The wiring end to end: statements and JSON to a graph, the checks, and
//! the compiled payload run in the device runtime (`wasm-host`) against a
//! fake board, compared with the reference evaluation.

use std::collections::BTreeMap;

use middleware::Key;
use wasm_host::{AbiError, Device, Limits, Payload, Pull as HostPull};
use wire_core::graph::{Config, FUNCTIONS, Graph, Kind, Node, Pull};
use wire_core::{Plan, Severity, build, check, has_errors, text};

const LAMP: [&str; 4] =
    ["button=gpio15,pullup,invert", "armed=gpio14,pulldown", "lit=and(button,armed)", "gpio25=lit"];

fn lamp() -> Graph {
    text::parse("lamp", 10, &LAMP).expect("the lamp statements parse")
}

fn errors(graph: &Graph, chip: Option<&str>) -> Vec<String> {
    let pins = chip.map(|c| board_pins::boards::by_chip(c).expect("a known chip"));
    check(graph, pins)
        .into_iter()
        .filter(|i| i.severity == Severity::Error)
        .map(|i| i.message)
        .collect()
}

/// A board that records what the payload does to its pins.
#[derive(Default)]
struct Board {
    levels: BTreeMap<u8, bool>,
    refused: Vec<u8>,
    logs: Vec<String>,
    inputs: Vec<(u8, HostPull)>,
    outputs: Vec<u8>,
    writes: Vec<(u8, bool)>,
}

impl Device for Board {
    fn log(&mut self, line: &str) {
        self.logs.push(line.to_string());
    }
    fn publish(&mut self, _key: Key<'_>, _data: &[u8]) -> Result<(), AbiError> {
        Err(AbiError::Unsupported)
    }
    fn now_ms(&mut self) -> u32 {
        0
    }
    fn gpio_output(&mut self, pin: u8) -> Result<(), AbiError> {
        if self.refused.contains(&pin) {
            return Err(AbiError::Unsupported);
        }
        self.outputs.push(pin);
        Ok(())
    }
    fn gpio_input(&mut self, pin: u8, pull: HostPull) -> Result<(), AbiError> {
        if self.refused.contains(&pin) {
            return Err(AbiError::Unsupported);
        }
        self.inputs.push((pin, pull));
        Ok(())
    }
    fn gpio_write(&mut self, pin: u8, high: bool) -> Result<(), AbiError> {
        self.writes.push((pin, high));
        Ok(())
    }
    fn gpio_read(&mut self, pin: u8) -> Result<bool, AbiError> {
        if self.refused.contains(&pin) {
            return Err(AbiError::Unsupported);
        }
        Ok(self.levels.get(&pin).copied().unwrap_or(false))
    }
}

fn run(graph: &Graph, board: Board) -> Payload<Board> {
    let (wasm, _) = build(graph, None).expect("the wiring builds");
    Payload::load(&wasm, board, Limits::default()).expect("the runtime loads the payload")
}

/// The level each output pin was last written.
fn driven(board: &Board) -> BTreeMap<u8, bool> {
    board.writes.iter().copied().collect()
}

#[test]
fn wr1_statements_build_the_graph_and_format_back() {
    let g = lamp();
    let kinds: Vec<(&str, Kind)> = g.nodes.iter().map(|n| (n.id.as_str(), n.kind)).collect();
    assert_eq!(
        kinds,
        [
            ("gpio15", Kind::Input),
            ("button", Kind::Channel),
            ("gpio14", Kind::Input),
            ("armed", Kind::Channel),
            ("lit", Kind::Channel),
            ("lit.fn", Kind::And),
            ("gpio25", Kind::Output),
        ]
    );
    assert_eq!(g.node("gpio15").unwrap().config, Config { pin: Some(15), pull: Pull::Up, invert: true, ..Config::default() });
    assert!(errors(&g, Some("rp2350")).is_empty());
    assert_eq!(text::format(&g), LAMP);
    assert_eq!(text::parse("lamp", 10, &text::format(&g)).unwrap(), g);
    assert_eq!(
        text::command_line(&g),
        "mwctl wire --name lamp button=gpio15,pullup,invert armed=gpio14,pulldown 'lit=and(button,armed)' gpio25=lit"
    );
}

#[test]
fn wr1_statements_say_what_is_wrong_with_them() {
    let bad = |s: &[&str]| text::parse("w", 10, s).unwrap_err();
    assert!(bad(&["button"]).contains("is not a wiring statement"));
    assert!(bad(&["Button=gpio15"]).contains("is not a channel name"));
    assert!(bad(&["gpio3=gpio4"]).contains("is not a channel name"));
    assert!(bad(&["a=gpio15,pullup,pulldown"]).contains("one of pullup and pulldown"));
    assert!(bad(&["a=gpio15,fast"]).contains("not 'fast'"));
    assert!(bad(&["gpio25=a,pullup"]).contains("only 'invert'"));
    assert!(bad(&["c=maybe(a,b)"]).contains("is not a function"));
    assert!(bad(&["c=and(a,not(b))"]).contains("a channel name of its own"));
    assert!(bad(&["c=and(a,c)"]).contains("cannot be its own input"));
    assert!(bad(&["a=gpio1", "a=gpio2"]).contains("already has a source"));
    assert!(bad(&["a=gpio1", "gpio1=a"]).contains("already a digital input"));
    assert!(bad(&["a=gpio1", "b=gpio1,invert"]).contains("with other options"));
    assert!(bad(&["a=gpio1", "b=gpio2", "gpio5=a", "gpio5=b"]).contains("an output follows one"));
    // One pin may drive several channels when its options agree.
    let g = text::parse("w", 10, &["a=gpio1,pullup", "b=gpio1,pullup"]).unwrap();
    assert_eq!(g.targets("gpio1").collect::<Vec<_>>(), ["a", "b"]);
}

#[test]
fn wr2_the_json_form_round_trips_and_is_strict() {
    let mut g = lamp();
    g.layout();
    let back = Graph::from_json(&g.to_json()).unwrap();
    assert_eq!(back, g);
    // graph-core's shape, with defaults: no period, no config, no position.
    let small = r#"{"name":"w","nodes":[{"id":"gpio2","kind":"input","config":{"pin":2}},
        {"id":"a","kind":"channel"},{"id":"gpio3","kind":"output","config":{"pin":3}}],
        "edges":[["gpio2","a"],["a","gpio3"]]}"#;
    let g = Graph::from_json(small).unwrap();
    assert_eq!(g.period_ms, 10);
    assert!(check(&g, None).is_empty());
    for bad in [
        r#"{"name":"w","nodes":[{"id":"a","kind":"flipflop"}],"edges":[]}"#,
        r#"{"name":"w","nodes":[{"id":"a","kind":"channel","colour":"red"}],"edges":[]}"#,
        r#"{"name":"w","nodes":[{"id":"a","kind":"input","config":{"pin":300}}],"edges":[]}"#,
        r#"{"name":"w","nodes":[],"edges":[],"extra":1}"#,
    ] {
        assert!(Graph::from_json(bad).unwrap_err().starts_with("wiring json:"), "{bad}");
    }
}

#[test]
fn wr2_layout_puts_stages_in_columns_and_keeps_set_positions() {
    let mut g = lamp();
    g.nodes[0].x = Some(7);
    g.nodes[0].y = Some(9);
    g.layout();
    let at = |id: &str| g.node(id).map(|n| (n.x.unwrap(), n.y.unwrap())).unwrap();
    assert_eq!(at("gpio15"), (7, 9));
    assert!(at("gpio14").0 < at("armed").0);
    assert!(at("armed").0 < at("lit.fn").0);
    assert!(at("lit.fn").0 < at("lit").0);
    assert!(at("lit").0 < at("gpio25").0);
    assert_ne!(at("button"), at("armed"));
}

#[test]
fn wr3_connections_follow_the_three_rules() {
    let node = |id: &str, kind: Kind, pin: Option<u8>| {
        Node::new(id, kind, Config { pin, ..Config::default() })
    };
    let mut g = Graph::new("w");
    g.nodes = vec![
        node("gpio1", Kind::Input, Some(1)),
        node("gpio2", Kind::Output, Some(2)),
        node("a", Kind::Channel, None),
        node("b", Kind::Channel, None),
        node("f", Kind::And, None),
        node("n", Kind::Not, None),
    ];
    let edge = |a: &str, b: &str| (a.to_string(), b.to_string());
    g.edges = vec![
        edge("gpio1", "gpio2"),
        edge("gpio1", "f"),
        edge("a", "b"),
        edge("f", "n"),
        edge("gpio2", "a"),
        edge("a", "gpio1"),
        edge("a", "ghost"),
    ];
    let e = errors(&g, None);
    let has = |needle: &str| assert!(e.iter().any(|m| m.contains(needle)), "{needle}: {e:#?}");
    has("gpio1 is a digital input and gpio2 is a digital output: an input drives a channel");
    has("gpio1 is a digital input and f is a function: an input drives a channel");
    has("a is a channel and b is a channel: a channel is driven by an input or a function");
    has("f is a function and n is a function: a function drives a channel");
    has("nothing connects out of a digital output");
    has("nothing connects into a digital input");
    has("names ghost, which does not exist");
    has("nothing drives channel a");
    has("f (and) reads 0 channel(s); it needs 2 to 8 channels");
    has("n (not) reads 0 channel(s); it needs exactly one channel");
    has("nothing drives gpio2");
}

#[test]
fn wr3_sources_pins_loops_and_names_are_checked() {
    let mut g = lamp();
    g.edges.push(("gpio14".into(), "button".into()));
    assert!(errors(&g, None)[0].contains("channel button is driven by gpio15 and gpio14; a channel has one source"));

    let mut g = lamp();
    g.nodes.push(Node::new("x.fn", Kind::Not, Config::default()));
    g.edges.push(("lit".into(), "x.fn".into()));
    g.edges.push(("x.fn".into(), "armed".into()));
    g.edges.retain(|(f, _)| f != "gpio14");
    let e = errors(&g, None);
    assert!(e.iter().any(|m| m.contains("armed feeds back into itself")), "{e:#?}");
    assert!(Plan::new(&g).is_err());

    let mut g = lamp();
    g.nodes[6].config.pin = Some(15);
    assert!(errors(&g, None)[0].contains("GPIO 15 is already used by gpio15"));
    g.nodes[6].config.pin = None;
    assert!(errors(&g, None)[0].contains("has no pin"));

    let mut g = lamp();
    g.name = String::new();
    g.period_ms = 5;
    g.nodes[1].id = "Button".into();
    let e = errors(&g, None);
    assert!(e[0].contains("name must be 1 to 24"));
    assert!(e[1].contains("scan period is 5 ms; it must be 10 to 60000"));
    assert!(e.iter().any(|m| m.contains("a channel name is a lower-case letter")));

    assert!(errors(&Graph::new("w"), None)[0].contains("the wiring is empty"));

    // Warnings do not stop a build.
    let g = text::parse("w", 10, &["a=gpio1", "b=gpio2"]).unwrap();
    let issues = check(&g, None);
    assert!(!has_errors(&issues));
    assert!(issues.iter().any(|i| i.message.contains("channel a is not used")));
    assert!(issues.iter().any(|i| i.message.contains("no digital output")));
    assert!(build(&g, None).is_ok());
}

#[test]
fn wr4_pins_are_checked_against_the_chip() {
    let g = lamp();
    assert!(errors(&g, Some("rp2350")).is_empty());
    // The Pico has GPIO 0..29; the ESP32's GPIO 34..39 are input-only and
    // its flash pins are the board's.
    let g = text::parse("w", 10, &["a=gpio40", "gpio25=a"]).unwrap();
    assert!(errors(&g, Some("rp2040"))[0].contains("this board has no GPIO 40"));
    let g = text::parse("w", 10, &["a=gpio4", "gpio34=a"]).unwrap();
    assert!(errors(&g, Some("esp32"))[0].contains("GPIO 34 is input-only"));
    let g = text::parse("w", 10, &["a=gpio6", "gpio4=a"]).unwrap();
    assert!(errors(&g, Some("esp32"))[0].contains("GPIO 6 is taken by the board"));
    // Without a chip only the wiring's own rules apply.
    assert!(errors(&g, None).is_empty());
}

#[test]
fn wr5_the_payload_sets_up_the_pins_and_follows_the_inputs() {
    let mut p = run(&lamp(), Board::default());
    {
        let b = p.device();
        assert_eq!(b.inputs, [(15, HostPull::Up), (14, HostPull::Down)]);
        assert_eq!(b.outputs, [25]);
        assert_eq!(b.logs, ["wire lamp: 2 in, 3 ch, 1 out"]);
        // The first scan ran in init: button low is pressed, armed is low.
        assert_eq!(b.writes, [(25, false)]);
    }
    assert_eq!(p.next_tick(), Some(10));
    p.tick(10).unwrap();
    assert_eq!(p.device().writes.len(), 1, "an unchanged output is not written again");
    p.device().levels.insert(14, true);
    p.tick(20).unwrap();
    assert_eq!(p.device().writes, [(25, false), (25, true)]);
    p.device().levels.insert(15, true); // released
    p.tick(30).unwrap();
    assert_eq!(p.device().writes.last(), Some(&(25, false)));
}

#[test]
fn wr5_a_refused_pin_is_logged_and_reads_false() {
    let board = Board { refused: vec![14], ..Board::default() };
    let mut p = run(&lamp(), board);
    assert_eq!(p.device().logs, ["wire: input gpio 14 refused", "wire lamp: 2 in, 3 ch, 1 out"]);
    p.device().levels.insert(14, true);
    p.tick(10).unwrap();
    assert!(!driven(p.device())[&25]);
}

#[test]
fn wr6_every_function_matches_the_reference_on_every_input() {
    for kind in FUNCTIONS {
        let width = if kind == Kind::Not { 1 } else { 3 };
        let mut statements: Vec<String> = (0..width).map(|i| format!("c{i}=gpio{i}")).collect();
        let args: Vec<String> = (0..width).map(|i| format!("c{i}")).collect();
        statements.push(format!("y={}({})", kind.name(), args.join(",")));
        statements.push("gpio10=y".into());
        statements.push("gpio11=y,invert".into());
        let g = text::parse("f", 10, &statements).unwrap();
        let plan = Plan::new(&g).unwrap();
        let mut p = run(&g, Board::default());
        for bits in 0..(1u8 << width) {
            for pin in 0..width {
                p.device().levels.insert(pin, bits >> pin & 1 == 1);
            }
            p.tick(u32::from(bits) * 10).unwrap();
            let want: BTreeMap<u8, bool> = plan.eval(|pin| bits >> pin & 1 == 1).into_iter().collect();
            assert_eq!(driven(p.device()), want, "{} of {bits:03b}", kind.name());
            assert_ne!(want[&10], want[&11]);
        }
    }
    // The truth tables themselves.
    let t = |kind: Kind, v: &[bool]| kind.apply(v.iter().copied());
    assert!(t(Kind::And, &[true, true]) && !t(Kind::And, &[true, false]));
    assert!(t(Kind::Or, &[false, true]) && !t(Kind::Or, &[false, false]));
    assert!(t(Kind::Xor, &[true, false]) && !t(Kind::Xor, &[true, true]));
    assert!(t(Kind::Not, &[false]) && !t(Kind::Not, &[true]));
    assert!(t(Kind::Nand, &[true, false]) && !t(Kind::Nand, &[true, true]));
    assert!(t(Kind::Nor, &[false, false]) && !t(Kind::Nor, &[false, true]));
    assert!(t(Kind::Xnor, &[true, true]) && !t(Kind::Xnor, &[true, false]));
}

#[test]
fn wr6_functions_chain_through_channels_in_any_statement_order() {
    // `alarm` is stated before the channels it reads.
    let statements = [
        "gpio20=alarm",
        "alarm=or(forced,both)",
        "both=and(door,window)",
        "forced=not(quiet)",
        "door=gpio1",
        "window=gpio2",
        "quiet=gpio3",
        "gpio21=door",
    ];
    let g = text::parse("alarm", 50, &statements).unwrap();
    let plan = Plan::new(&g).unwrap();
    let mut p = run(&g, Board::default());
    assert_eq!(p.next_tick(), Some(50));
    for bits in 0..8u8 {
        for pin in 1..=3 {
            p.device().levels.insert(pin, bits >> (pin - 1) & 1 == 1);
        }
        p.tick(u32::from(bits) * 50).unwrap();
        let read = |pin: u8| bits >> (pin - 1) & 1 == 1;
        let want: BTreeMap<u8, bool> = plan.eval(read).into_iter().collect();
        assert_eq!(want[&20], !read(3) || (read(1) && read(2)));
        assert_eq!(want[&21], read(1));
        assert_eq!(driven(p.device()), want, "{bits:03b}");
    }
}

#[test]
fn wr6_the_largest_function_and_a_wiring_without_outputs_load() {
    let mut statements: Vec<String> = (0..8).map(|i| format!("c{i}=gpio{i}")).collect();
    let args: Vec<String> = (0..8).map(|i| format!("c{i}")).collect();
    statements.push(format!("all=and({})", args.join(",")));
    let g = text::parse("wide", 10, &statements).unwrap();
    let p = run(&g, Board::default());
    assert!(!p.is_poisoned());
    statements.push(format!("more=or({},all)", args.join(",")));
    let g = text::parse("wide", 10, &statements).unwrap();
    assert!(errors(&g, None)[0].contains("reads 9 channel(s); it needs 2 to 8"));
}
