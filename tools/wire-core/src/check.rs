//! Every problem of a wiring, at once and in plain language.
//!
//! Each [`Issue`] names the node or the edge it is about, so a canvas can
//! mark it. Errors stop a build; warnings do not.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use board_pins::{AdcError, Direction, PinError, PinMap};
use serde::Serialize;

use crate::graph::{
    Class, DEFAULT_PWM_HZ, Graph, Kind, MAX_EDGES, MAX_ID, MAX_NODES, MAX_NODE_PERIOD_MS,
    MAX_PERIOD_MS, MIN_PERIOD_MS, adc_pin, gpio_pin, is_channel_name, is_id,
};
use crate::model::{Shape, is_model};
use crate::spec::{FIELDS, ParamType};

/// Whether an issue stops the build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// The wiring cannot be built.
    Error,
    /// The wiring builds; something in it does nothing.
    Warning,
}

/// One problem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Issue {
    /// Whether it stops the build.
    pub severity: Severity,
    /// The node it is about.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// The edge it is about.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edge: Option<(String, String)>,
    /// What is wrong and what to do.
    pub message: String,
}

/// Whether any issue is an error.
pub fn has_errors(issues: &[Issue]) -> bool {
    issues.iter().any(|i| i.severity == Severity::Error)
}

struct Report(Vec<Issue>);

impl Report {
    fn graph(&mut self, message: String) {
        self.0.push(Issue { severity: Severity::Error, node: None, edge: None, message });
    }
    fn node(&mut self, id: &str, message: String) {
        self.0.push(Issue { severity: Severity::Error, node: Some(id.into()), edge: None, message });
    }
    fn warn(&mut self, id: &str, message: String) {
        self.0.push(Issue {
            severity: Severity::Warning,
            node: Some(id.into()),
            edge: None,
            message,
        });
    }
    fn edge(&mut self, from: &str, to: &str, message: String) {
        self.0.push(Issue {
            severity: Severity::Error,
            node: None,
            edge: Some((from.into(), to.into())),
            message,
        });
    }
}

fn list(ids: &[&str]) -> String {
    match ids {
        [] => String::new(),
        [one] => (*one).to_string(),
        [head @ .., last] => format!("{} and {last}", head.join(", ")),
    }
}

/// Checks `graph`, against `pins` when the chip is known.
pub fn check(graph: &Graph, pins: Option<&PinMap>) -> Vec<Issue> {
    let mut r = Report(Vec::new());
    let n = graph.nodes.len();

    if graph.name.is_empty() || graph.name.len() > MAX_ID {
        r.graph(format!("the wiring's name must be 1 to {MAX_ID} characters"));
    } else if !graph.name.bytes().all(|b| b.is_ascii_graphic()) {
        r.graph("the wiring's name must be printable ASCII without spaces".into());
    }
    if !(MIN_PERIOD_MS..=MAX_PERIOD_MS).contains(&graph.period_ms) {
        r.graph(format!(
            "the scan period is {} ms; it must be {MIN_PERIOD_MS} to {MAX_PERIOD_MS} ms",
            graph.period_ms
        ));
    }
    if n > MAX_NODES || graph.edges.len() > MAX_EDGES {
        r.graph(format!(
            "the wiring has {n} nodes and {} connections; the most is {MAX_NODES} and {MAX_EDGES}",
            graph.edges.len()
        ));
        return r.0;
    }
    if n == 0 {
        r.graph("the wiring is empty: add a digital input, a channel and a digital output".into());
        return r.0;
    }

    // Ids.
    let mut index: BTreeMap<&str, usize> = BTreeMap::new();
    for (i, node) in graph.nodes.iter().enumerate() {
        let id = node.id.as_str();
        if index.insert(id, i).is_some() {
            r.node(id, format!("two nodes are called {id}; rename one"));
            continue;
        }
        match node.kind.class() {
            Class::Channel if !is_channel_name(id) => r.node(
                id,
                if gpio_pin(id).is_some() || adc_pin(id).is_some() || Kind::function(id).is_some() {
                    format!("{id} cannot name a channel: it reads as a pin or a function")
                } else {
                    format!(
                        "a channel name is a lower-case letter, then lower-case letters, digits \
                         or _, at most {MAX_ID} characters; '{id}' is not"
                    )
                },
            ),
            _ if !is_id(id) => r.node(
                id,
                format!("a node id is 1 to {MAX_ID} letters, digits, _ or .; '{id}' is not"),
            ),
            _ => {}
        }
    }

    // Settings: each kind takes the ones its table row lists.
    let base = graph.period_ms;
    let base_ok = (MIN_PERIOD_MS..=MAX_PERIOD_MS).contains(&base);
    for node in &graph.nodes {
        let id = node.id.as_str();
        let spec = node.kind.spec();
        let name = spec.name;
        let a = node.kind.a();
        let mut pin_settings = false;
        for field in FIELDS {
            if !node.config.has(field) || spec.param(field).is_some() {
                continue;
            }
            match field {
                "pull" if node.kind == Kind::Output => {
                    r.node(id, format!("{id} is an output; a pull resistor applies to inputs only"));
                }
                "pin" | "pull" | "invert" if matches!(spec.class, Class::Channel | Class::Function) => {
                    if !core::mem::replace(&mut pin_settings, true) {
                        r.node(id, format!("{id} is {a} and takes no pin settings"));
                    }
                }
                "period_ms" => r.node(
                    id,
                    format!("{id} is a channel and has no period: it holds the last value written"),
                ),
                _ => r.node(id, format!("{id} is {a} ({name}) and takes no '{field}' setting")),
            }
        }
        for p in spec.params {
            match p.ty {
                ParamType::Real => {
                    let (min, max, _) = p.real;
                    match node.config.get_real(p.name) {
                        Some(v) if !(v >= min && v <= max) => r.node(
                            id,
                            match (min.is_finite(), max.is_finite()) {
                                (true, true) => format!("{id} ({name}) has {} {v}; it must be {min} to {max}", p.name),
                                (true, false) => format!("{id} ({name}) has {} {v}; it must be at least {min}", p.name),
                                (false, true) => format!("{id} ({name}) has {} {v}; it must be at most {max}", p.name),
                                (false, false) => format!("{id} ({name}) has {} {v}; it must be a finite number", p.name),
                            },
                        ),
                        None if p.required => r.node(id, format!("{id} ({name}) has no {}: set it", p.name)),
                        _ => {}
                    }
                    continue;
                }
                ParamType::Text => {
                    if p.required && node.config.get_text(p.name).is_none() {
                        r.node(
                            id,
                            format!("{id} ({name}) has no {}: write it, such as {}=\"{}\"", p.name, p.name, p.hint),
                        );
                    }
                    continue;
                }
                _ => {}
            }
            match node.config.get(p.name) {
                None if p.required && p.name == "pin" => {
                    r.node(id, format!("{id} has no pin: choose its GPIO number"));
                }
                None if p.required => {
                    r.node(id, format!("{id} ({name}) has no {}: set it ({} to {})", p.name, p.min, p.max));
                }
                None => {}
                Some(period) if p.name == "period_ms" => {
                    if base_ok
                        && (period <= 0
                            || period % i64::from(base) != 0
                            || period > i64::from(MAX_NODE_PERIOD_MS))
                    {
                        r.node(
                            id,
                            format!(
                                "{id} updates every {period} ms; that must be a multiple of the scan \
                                 period ({base} ms), at most {MAX_NODE_PERIOD_MS} ms"
                            ),
                        );
                    }
                }
                Some(v) if v < p.min || v > p.max => r.node(
                    id,
                    format!("{id} ({name}) has {} {v}; it must be {} to {}", p.name, p.min, p.max),
                ),
                Some(_) => {}
            }
        }
        if node.kind == Kind::Scale && node.config.div == Some(0) {
            r.node(id, format!("{id} (scale) divides by 0; div must not be 0"));
        }
        if let (Some(lo), Some(hi)) = (node.config.lo, node.config.hi) {
            match node.kind {
                Kind::Clamp if lo > hi => {
                    r.node(id, format!("{id} (clamp) has lo {lo} above hi {hi}; lo must not exceed hi"));
                }
                Kind::Hyst if lo >= hi => {
                    r.node(id, format!("{id} (hyst) has lo {lo} and hi {hi}; lo must be below hi"));
                }
                _ => {}
            }
        }
    }

    // Pins: one node per GPIO, and each must suit the board.
    let mut used: BTreeMap<u8, &str> = BTreeMap::new();
    for node in &graph.nodes {
        let id = node.id.as_str();
        let dir = match node.kind {
            Kind::Input | Kind::Analog => Direction::Input,
            Kind::Output => Direction::Output,
            _ => continue,
        };
        let Some(pin) = node.config.pin else { continue };
        if let Some(other) = used.insert(pin, id) {
            r.node(id, format!("GPIO {pin} is already used by {other}; one node per pin"));
            continue;
        }
        let Some(map) = pins else { continue };
        if node.kind == Kind::Analog {
            // The board's ADC table decides, not the digital rules: a pin
            // kept for the ADC is exactly the one an analog input wants.
            match map.adc_channel(pin) {
                Ok(_) => {}
                Err(AdcError::NoSuchPin) => r.node(id, format!("this board has no GPIO {pin}")),
                Err(AdcError::NotAnalog) => {
                    let analog: Vec<String> = map.adc_pins().iter().map(|p| format!("{p}")).collect();
                    r.node(
                        id,
                        if analog.is_empty() {
                            format!("GPIO {pin} has no analog input: this board gives payloads none")
                        } else {
                            format!(
                                "GPIO {pin} has no analog input on this board (analog pins: {})",
                                analog.join(", ")
                            )
                        },
                    );
                }
            }
            continue;
        }
        match map.check(pin, dir) {
            Ok(()) => {}
            Err(PinError::NoSuchPin) => {
                r.node(id, format!("this board has no GPIO {pin}"));
            }
            Err(PinError::Reserved) => r.node(
                id,
                match map.reason(pin) {
                    Some(why) => format!("GPIO {pin} is taken by the board: {why}"),
                    None => format!("GPIO {pin} is taken by the board"),
                },
            ),
            Err(PinError::InputOnly) => {
                r.node(id, format!("GPIO {pin} is input-only on this board; it cannot be an output"));
            }
        }
    }

    // PWM: one node per timer output, one frequency per timer, and the
    // board must wire the output to a pin nothing else uses.
    let mut pwm: Vec<(u8, u8, u32, &str)> = Vec::new();
    for node in graph.nodes.iter().filter(|n| n.kind == Kind::Pwm) {
        let id = node.id.as_str();
        let (Some(timer), Some(output)) = (node.config.timer, node.config.channel) else { continue };
        let freq = node.config.freq_hz.unwrap_or(DEFAULT_PWM_HZ);
        if let Some(other) = pwm.iter().find(|p| (p.0, p.1) == (timer, output)) {
            r.node(
                id,
                format!("PWM output {timer}.{output} is already used by {}; one node per output", other.3),
            );
            continue;
        }
        if let Some(other) = pwm.iter().find(|p| p.0 == timer && p.2 != freq) {
            r.node(
                id,
                format!(
                    "{id} runs PWM timer {timer} at {freq} Hz but {} runs it at {} Hz; a timer has \
                     one frequency",
                    other.3, other.2
                ),
            );
        }
        pwm.push((timer, output, freq, id));
        let Some(map) = pins else { continue };
        match map.pwm_pin(timer, output) {
            None => r.node(id, format!("this board has no PWM output {timer}.{output}")),
            Some(pin) => {
                if let Some(other) = used.get(&pin) {
                    r.node(
                        id,
                        format!(
                            "PWM output {timer}.{output} is GPIO {pin} on this board, which {other} \
                             already uses"
                        ),
                    );
                }
            }
        }
    }

    // Edges.
    let mut into: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut out: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (k, (from, to)) in graph.edges.iter().enumerate() {
        let (Some(&f), Some(&t)) = (index.get(from.as_str()), index.get(to.as_str())) else {
            let missing = if index.contains_key(from.as_str()) { to } else { from };
            r.edge(from, to, format!("the connection {from} -> {to} names {missing}, which does not exist"));
            continue;
        };
        if graph.edges[..k].contains(&(from.clone(), to.clone())) {
            r.edge(from, to, format!("{from} -> {to} is connected twice; remove one"));
            continue;
        }
        let (a, b) = (graph.nodes[f].kind.class(), graph.nodes[t].kind.class());
        if !a.connects_to(b) {
            let (from_kind, to_kind) = (graph.nodes[f].kind, graph.nodes[t].kind);
            let hint = match (a, b) {
                (Class::Source, Class::Function) | (Class::Source, Class::Sink) => {
                    if from_kind == Kind::Const {
                        "a constant drives a channel; put a channel between them".to_string()
                    } else {
                        "an input drives a channel; put a channel between them".to_string()
                    }
                }
                (Class::Function, Class::Function) | (Class::Function, Class::Sink) => {
                    "a function drives a channel; put a channel between them".to_string()
                }
                (Class::Channel, Class::Channel) => {
                    "a channel is driven by an input or a function, not by another channel".to_string()
                }
                (_, Class::Source) => format!("nothing connects into {}", to_kind.a()),
                (Class::Sink, _) => format!("nothing connects out of {}", from_kind.a()),
                _ => "that connection is not allowed".to_string(),
            };
            r.edge(from, to, format!("{from} is {} and {to} is {}: {hint}", from_kind.a(), to_kind.a()));
            continue;
        }
        into[t].push(f);
        out[f].push(t);
    }

    // Per node: who drives it and what it drives.
    for (i, node) in graph.nodes.iter().enumerate() {
        let id = node.id.as_str();
        let sources: Vec<&str> = into[i].iter().map(|&f| graph.nodes[f].id.as_str()).collect();
        match node.kind.class() {
            Class::Source => {
                if out[i].is_empty() {
                    r.warn(id, format!("{id} drives no channel; connect it to one"));
                }
            }
            Class::Channel => {
                match sources.len() {
                    0 => r.node(
                        id,
                        format!("nothing drives channel {id}: connect a digital input or a function to it"),
                    ),
                    1 => {}
                    _ => r.node(
                        id,
                        format!(
                            "channel {id} is driven by {}; a channel has one source (combine them with a function)",
                            list(&sources)
                        ),
                    ),
                }
                if out[i].is_empty() && !node.config.trace {
                    r.warn(id, format!("channel {id} is not used by a function or an output"));
                }
            }
            Class::Function if is_model(node.kind) => {
                // The model says how many channels it reads and drives.
                let shape = Shape {
                    inputs: sources.len(),
                    outputs: out[i].len(),
                    period_ms: node.config.period_ms.unwrap_or(graph.period_ms),
                };
                if let Err(problems) = crate::model::program(node.kind, &node.config, &shape) {
                    for problem in problems {
                        r.node(id, format!("{id} ({}) {problem}", node.kind.name()));
                    }
                }
            }
            Class::Function => {
                let (fewest, most) = node.kind.arity();
                let name = node.kind.name();
                if sources.len() < fewest || sources.len() > most {
                    let want = match (fewest, most) {
                        (1, 1) => "exactly one channel".to_string(),
                        (f, m) if f == m => format!("exactly {f} channels"),
                        (f, m) => format!("{f} to {m} channels"),
                    };
                    r.node(id, format!("{id} ({name}) reads {} channel(s); it needs {want}", sources.len()));
                }
                if out[i].is_empty() {
                    r.warn(id, format!("{id} ({name}) drives no channel; connect it to one"));
                }
            }
            Class::Sink => match sources.len() {
                0 => r.node(id, format!("nothing drives {id}: connect a channel to it")),
                1 => {}
                _ => r.node(
                    id,
                    format!(
                        "{id} is driven by {}; an output follows one channel (combine them with a function)",
                        list(&sources)
                    ),
                ),
            },
        }
    }

    // Feedback: the update order ignores the connections into a delay, so
    // a loop through one is fine; what cannot be ordered sits on a loop
    // without a delay, or behind one.
    let kinds: Vec<Kind> = graph.nodes.iter().map(|n| n.kind).collect();
    let (_, pending) = update_order(&kinds, &out);
    if pending.iter().any(|&p| p > 0) {
        let follows = |i: usize| out[i].iter().copied().filter(|&t| kinds[t] != Kind::Delay);
        // Of the leftovers, the ones that can reach themselves are the loop.
        for start in (0..n).filter(|&i| pending[i] > 0) {
            let mut seen = vec![false; n];
            let mut stack: Vec<usize> = follows(start).collect();
            let mut loops = false;
            while let Some(i) = stack.pop() {
                if i == start {
                    loops = true;
                    break;
                }
                if !core::mem::replace(&mut seen[i], true) {
                    stack.extend(follows(i));
                }
            }
            if loops {
                let id = graph.nodes[start].id.as_str();
                r.node(id, format!("{id} feeds back into itself; a loop needs a delay in it"));
            }
        }
    }

    let acts = graph
        .nodes
        .iter()
        .any(|n| n.kind.class() == Class::Sink || (n.kind == Kind::Channel && n.config.trace));
    if !acts {
        r.0.push(Issue {
            severity: Severity::Warning,
            node: None,
            edge: None,
            message: "the wiring has no digital output, so it changes nothing on the board".into(),
        });
    }
    r.0
}

/// The one update order of a wiring: every node after the nodes that feed
/// it, where the connections into a `delay` do not count (a delay emits
/// what it stored, so for ordering it is a source). Among the nodes that
/// are ready, the earliest in node order goes first.
///
/// `out[i]` lists the nodes `i` connects to. Returns the order and, per
/// node, how many feeders were never placed: nonzero for the nodes on a
/// loop without a delay, or behind one (they are left out of the order).
pub(crate) fn update_order(kinds: &[Kind], out: &[Vec<usize>]) -> (Vec<usize>, Vec<usize>) {
    let n = kinds.len();
    let mut pending = vec![0usize; n];
    for targets in out {
        for &t in targets {
            if kinds[t] != Kind::Delay {
                pending[t] += 1;
            }
        }
    }
    let mut placed = vec![false; n];
    let mut order = Vec::with_capacity(n);
    while let Some(i) = (0..n).find(|&i| !placed[i] && pending[i] == 0) {
        placed[i] = true;
        order.push(i);
        for &t in &out[i] {
            if kinds[t] != Kind::Delay {
                pending[t] -= 1;
            }
        }
    }
    (order, pending)
}
