//! The wiring as command-line statements, one per argument.
//!
//! | statement                       | meaning                                        |
//! |---------------------------------|------------------------------------------------|
//! | `button=gpio15,pullup,invert`   | digital input GPIO 15 drives channel `button`  |
//! | `raw=adc26`                     | analog input GPIO 26 drives channel `raw`      |
//! | `limit=1200`, `offset=-7`       | a constant drives a channel                    |
//! | `lit=and(button,armed)`         | a function of channels drives channel `lit`    |
//! | `level=lowpass(raw,shift=4)`    | a function with settings                       |
//! | `gpio25=lit,invert`             | channel `lit` drives digital output GPIO 25    |
//! | `pwm0.1=level,freq=2000`        | channel `level` drives PWM timer 0's output 1  |
//! | `pos,vel=ukf(z,n=2,f="x0+dt*x1; x1",h="x0",q="1e-6;1e-4",r="0.01")` | a model-based block drives several channels, in order |
//!
//! A `ukf` or `cbf` drives one channel per output, listed before the `=`
//! (its node is `<first channel>.fn`). A setting that holds expressions or
//! a list of numbers is written in double quotes; inside them commas,
//! parentheses, `=`, `;`, `@` and spaces are just text ([`crate::expr`]).
//! A real setting is written plainly: `alpha=2.5`.
//!
//! Any statement may end in `@<N>ms`, the period its node updates at (a
//! multiple of the wiring's period, which is the default):
//! `raw=adc26@10ms`, `slow=mean(raw,n=10)@100ms`, `gpio25=lit@1000ms`.
//!
//! Input options: `pullup`, `pulldown`, `invert` (1 when the pin is low).
//! Output option: `invert` (low when the channel is nonzero). PWM option:
//! `freq=<Hz>` (default 1000). A function takes its channels first, in
//! argument order, then `name=value` settings; [`crate::spec`] lists each
//! function's channels and settings ([`functions`] names them all). An
//! argument is a channel name, never another function: name the inner
//! result as a channel of its own.
//!
//! Tracing a channel is not a statement but an option of the wiring
//! (`--trace` in `mwctl wire`): [`set_trace`].
//!
//! [`parse`] builds the [`Graph`] (pin nodes are called `gpio<N>`,
//! `adc<N>` and `pwm<T>.<C>`, a constant `<channel>.const`, a statement's
//! function `<channel>.fn`); [`format`] writes a graph back as statements
//! that parse to the same wiring.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::graph::{
    Class, Config, FUNCTIONS, Graph, Kind, Node, Pull, adc_pin, gpio_pin, is_channel_name,
    pwm_output,
};
use crate::model::is_model;
use crate::spec::{KINDS, ParamType};

/// The function names, in palette order, separated by `, `.
pub fn functions() -> String {
    let names: Vec<&str> =
        KINDS.iter().filter(|k| k.class() == Class::Function).map(|k| k.name()).collect();
    names.join(", ")
}

fn channel(name: &str, statement: &str) -> Result<(), String> {
    if is_channel_name(name) {
        return Ok(());
    }
    Err(format!(
        "'{statement}': '{name}' is not a channel name (a lower-case letter, then lower-case \
         letters, digits or _; not a pin such as gpio15 and not a function name)"
    ))
}

fn ensure_channel(graph: &mut Graph, name: &str) {
    if graph.node(name).is_none() {
        graph.nodes.push(Node::new(name, Kind::Channel, Config::default()));
    }
}

/// The node for a pin a statement names; a pin named twice must be the
/// same node both times.
fn pin_node(
    graph: &mut Graph,
    statement: &str,
    id: &str,
    pin: u8,
    kind: Kind,
    config: Config,
) -> Result<(), String> {
    match graph.node(id) {
        None => graph.nodes.push(Node::new(id, kind, config)),
        Some(old) if old.kind != kind => {
            return Err(format!("'{statement}': GPIO {pin} is already a digital {}", old.kind.name()));
        }
        Some(old) if old.config != config => {
            return Err(format!("'{statement}': GPIO {pin} is used earlier with other options"));
        }
        Some(_) => {}
    }
    Ok(())
}

fn number(statement: &str, text: &str) -> Result<i64, String> {
    text.parse::<i64>().map_err(|_| format!("'{statement}': '{text}' is not a whole number"))
}

/// The byte offsets of every `wanted` outside double quotes.
fn outside_quotes(text: &str, wanted: char) -> Vec<usize> {
    let mut quoted = false;
    let mut found = Vec::new();
    for (i, c) in text.char_indices() {
        if c == '"' {
            quoted = !quoted;
        } else if c == wanted && !quoted {
            found.push(i);
        }
    }
    found
}

/// `text` split at every `separator` outside double quotes.
fn split_outside(text: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    for at in outside_quotes(text, separator) {
        parts.push(&text[start..at]);
        start = at + separator.len_utf8();
    }
    parts.push(&text[start..]);
    parts
}

/// Splits `@<N>ms` off the end of a statement's right side.
fn rate<'a>(statement: &str, right: &'a str) -> Result<(&'a str, Option<u32>), String> {
    let Some(&at) = outside_quotes(right, '@').last() else { return Ok((right, None)) };
    let (body, period) = (&right[..at], &right[at + 1..]);
    let ms = period
        .trim()
        .strip_suffix("ms")
        .filter(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|d| d.parse::<u32>().ok())
        .ok_or_else(|| {
            format!("'{statement}': after '@' comes the update period in ms, such as @100ms")
        })?;
    Ok((body.trim(), Some(ms)))
}

fn statement(graph: &mut Graph, s: &str) -> Result<(), String> {
    let (left, right) = s.split_once('=').ok_or_else(|| {
        format!(
            "'{s}' is not a wiring statement; write CHANNEL=gpioN[,pullup|pulldown][,invert], \
             CHANNEL=adcN, CHANNEL=NUMBER, CHANNEL[,CHANNEL]=FUNCTION(CHANNEL,...[,NAME=VALUE]), \
             gpioN=CHANNEL[,invert] or pwmT.C=CHANNEL[,freq=HZ], each optionally ending in @Nms"
        )
    })?;
    let (right, period) = rate(s, right.trim())?;
    let left = left.trim();
    // The wiring's own period is the default and is not stored.
    let period_ms = period.filter(|&p| p != graph.period_ms);
    let base = Config { period_ms, ..Config::default() };

    // gpioN=CHANNEL[,invert]: a digital output.
    if let Some(pin) = gpio_pin(left) {
        let mut words = right.split(',').map(str::trim);
        let name = words.next().unwrap_or("");
        channel(name, s)?;
        let mut config = Config { pin: Some(pin), ..base };
        for word in words {
            match word {
                "invert" => config.invert = true,
                other => return Err(format!("'{s}': an output takes only 'invert', not '{other}'")),
            }
        }
        let id = format!("gpio{pin}");
        pin_node(graph, s, &id, pin, Kind::Output, config)?;
        if graph.sources(&id).next().is_some() {
            return Err(format!("'{s}': GPIO {pin} already follows a channel; an output follows one"));
        }
        ensure_channel(graph, name);
        graph.edges.push((name.into(), id));
        return Ok(());
    }

    // pwmT.C=CHANNEL[,freq=HZ]: a PWM output.
    if let Some((timer, output)) = pwm_output(left) {
        let mut words = right.split(',').map(str::trim);
        let name = words.next().unwrap_or("");
        channel(name, s)?;
        let mut config = Config { timer: Some(timer), channel: Some(output), ..base };
        for word in words {
            let Some(hz) = word.strip_prefix("freq=") else {
                return Err(format!("'{s}': a PWM output takes only freq=<Hz>, not '{word}'"));
            };
            if !config.set("freq_hz", number(s, hz.trim())?) {
                return Err(format!("'{s}': freq={hz} is out of range"));
            }
        }
        let id = format!("pwm{timer}.{output}");
        if graph.node(&id).is_some() {
            return Err(format!(
                "'{s}': PWM output {timer}.{output} already follows a channel; an output follows one"
            ));
        }
        ensure_channel(graph, name);
        graph.nodes.push(Node::new(&id, Kind::Pwm, config));
        graph.edges.push((name.into(), id));
        return Ok(());
    }

    // The channels driven: one, or for a model-based block one per output.
    let lefts: Vec<&str> = left.split(',').map(str::trim).collect();
    for (k, name) in lefts.iter().enumerate() {
        channel(name, s)?;
        if graph.sources(name).next().is_some() {
            return Err(format!("'{s}': channel {name} already has a source; a channel has one"));
        }
        if lefts[..k].contains(name) {
            return Err(format!("'{s}': channel {name} is driven twice; each output has its own channel"));
        }
    }
    let left = lefts[0];
    let several = || {
        format!("'{s}': only a ukf or a cbf drives several channels; give each other source its own statement")
    };

    // CHANNEL=gpioN[,options]: a digital input.
    let mut words = right.split(',').map(str::trim);
    let first = words.next().unwrap_or("");
    if let Some(pin) = gpio_pin(first) {
        if lefts.len() > 1 {
            return Err(several());
        }
        let mut config = Config { pin: Some(pin), ..base };
        for word in words {
            match word {
                "pullup" if config.pull == Pull::None => config.pull = Pull::Up,
                "pulldown" if config.pull == Pull::None => config.pull = Pull::Down,
                "pullup" | "pulldown" => return Err(format!("'{s}': give one of pullup and pulldown")),
                "invert" => config.invert = true,
                other => {
                    return Err(format!("'{s}': an input takes pullup, pulldown and invert, not '{other}'"));
                }
            }
        }
        let id = format!("gpio{pin}");
        pin_node(graph, s, &id, pin, Kind::Input, config)?;
        ensure_channel(graph, left);
        graph.edges.push((id, left.into()));
        return Ok(());
    }

    // CHANNEL=adcN: an analog input.
    if let Some(pin) = adc_pin(first) {
        if lefts.len() > 1 {
            return Err(several());
        }
        if let Some(other) = words.next() {
            return Err(format!("'{s}': an analog input takes no options, not '{other}'"));
        }
        let id = format!("adc{pin}");
        pin_node(graph, s, &id, pin, Kind::Analog, Config { pin: Some(pin), ..base })?;
        ensure_channel(graph, left);
        graph.edges.push((id, left.into()));
        return Ok(());
    }

    // CHANNEL=NUMBER: a constant.
    if right.starts_with(|c: char| c.is_ascii_digit() || c == '-' || c == '+') {
        if lefts.len() > 1 {
            return Err(several());
        }
        let value = right.parse::<i32>().map_err(|_| {
            format!("'{s}': '{right}' is not a whole number from {} to {}", i32::MIN, i32::MAX)
        })?;
        let id = format!("{left}.const");
        ensure_channel(graph, left);
        graph.nodes.push(Node::new(&id, Kind::Const, Config { value: Some(value), ..base }));
        graph.edges.push((id, left.into()));
        return Ok(());
    }

    // CHANNEL[,CHANNEL]=FUNCTION(CHANNEL,...[,NAME=VALUE]).
    let call = right.strip_suffix(')').and_then(|r| r.split_once('('));
    let Some((name, args)) = call else {
        return Err(format!(
            "'{s}': after '=' comes a pin (gpio15, adc26), a number, or a function of channels \
             such as and(a,b) or not(a)"
        ));
    };
    let name = name.trim();
    let kind = Kind::function(name)
        .ok_or_else(|| format!("'{s}': '{name}' is not a function ({})", functions()))?;
    if lefts.len() > 1 && !is_model(kind) {
        return Err(several());
    }
    let spec = kind.spec();
    let mut config = base;
    let mut args_in: Vec<&str> = Vec::new();
    let mut setting: Option<&str> = None;
    for arg in split_outside(args, ',').into_iter().map(str::trim) {
        let named = arg.split_once('=').filter(|(key, _)| !key.contains(['"', '(']));
        if let Some((key, value)) = named {
            let (key, value) = (key.trim(), value.trim());
            setting = Some(arg);
            let takes: Vec<&str> =
                spec.params.iter().map(|p| p.name).filter(|n| *n != "period_ms").collect();
            let Some(param) = spec.param(key).filter(|p| p.name != "period_ms") else {
                return Err(if takes.is_empty() {
                    format!("'{s}': {name} takes no settings, so not '{key}'")
                } else {
                    format!("'{s}': {name} takes {}, not '{key}'", takes.join(", "))
                });
            };
            if config.has(key) {
                return Err(format!("'{s}': {key} is given twice"));
            }
            let quoted = value.len() >= 2 && value.starts_with('"') && value.ends_with('"');
            match param.ty {
                ParamType::Text => {
                    let inner = if quoted { &value[1..value.len() - 1] } else { "\"" };
                    if inner.contains('"') {
                        return Err(format!(
                            "'{s}': {key} holds expressions or numbers; write it in double quotes, {key}=\"{}\"",
                            param.hint
                        ));
                    }
                    config.set_text(key, inner);
                }
                ParamType::Real => {
                    let parsed = value.parse::<f64>().ok().filter(|v| v.is_finite());
                    if !parsed.is_some_and(|v| config.set_real(key, v)) {
                        return Err(format!("'{s}': '{value}' is not a number"));
                    }
                }
                _ => {
                    if !config.set(key, number(s, value)?) {
                        return Err(format!("'{s}': {key}={value} is out of range"));
                    }
                }
            }
            continue;
        }
        if arg.contains('(') {
            return Err(format!(
                "'{s}': a function reads channels; give '{arg}' a channel name of its own and use that"
            ));
        }
        if let Some(setting) = setting {
            return Err(format!(
                "'{s}': channels come before settings; move '{arg}' before '{setting}'"
            ));
        }
        channel(arg, s)?;
        if lefts.contains(&arg) && kind != Kind::Delay {
            return Err(format!(
                "'{s}': channel {arg} cannot be its own input (a loop needs a delay in it)"
            ));
        }
        if args_in.contains(&arg) {
            // A boolean function of the same channel twice reads it once;
            // anywhere else the second argument would silently vanish.
            if FUNCTIONS.contains(&kind) {
                continue;
            }
            return Err(format!("'{s}': {arg} is given twice; {name} reads a channel once"));
        }
        args_in.push(arg);
    }
    let id = format!("{left}.fn");
    if graph.node(&id).is_some() {
        return Err(format!("'{s}': channel {left} already has a source; a channel has one"));
    }
    for arg in &args_in {
        ensure_channel(graph, arg);
    }
    for name in &lefts {
        ensure_channel(graph, name);
    }
    graph.nodes.push(Node::new(&id, kind, config));
    for arg in &args_in {
        graph.edges.push(((*arg).into(), id.clone()));
    }
    for name in &lefts {
        graph.edges.push((id.clone(), (*name).into()));
    }
    Ok(())
}

/// Builds the wiring the statements describe. Only the statements' own
/// form is checked here; [`crate::check`] checks the wiring.
pub fn parse<S: AsRef<str>>(name: &str, period_ms: u32, statements: &[S]) -> Result<Graph, String> {
    let mut graph = Graph::new(name);
    graph.period_ms = period_ms;
    for s in statements {
        statement(&mut graph, s.as_ref())?;
    }
    Ok(graph)
}

/// Marks channels as traced: the ones named, or every channel when
/// `channels` is empty. An unknown name is an error.
pub fn set_trace<S: AsRef<str>>(graph: &mut Graph, channels: &[S]) -> Result<(), String> {
    for name in channels {
        let name = name.as_ref();
        match graph.nodes.iter_mut().find(|n| n.id == name && n.kind == Kind::Channel) {
            Some(node) => node.config.trace = true,
            None => return Err(format!("--trace: the wiring has no channel '{name}'")),
        }
    }
    if channels.is_empty() {
        for node in graph.nodes.iter_mut().filter(|n| n.kind == Kind::Channel) {
            node.config.trace = true;
        }
    }
    Ok(())
}

/// The statements for `graph`: sources, then functions, then sinks, each
/// in node order. Connections the statements cannot express (the ones
/// [`crate::check`] reports as errors) are left out.
pub fn format(graph: &Graph) -> Vec<String> {
    let mut out = Vec::new();
    let classed = |class: Class| graph.nodes.iter().filter(move |n| n.kind.class() == class);
    let is_channel = |id: &str| graph.node(id).is_some_and(|n| n.kind == Kind::Channel);
    let rate = |node: &Node| match node.config.period_ms {
        Some(p) if p != graph.period_ms => format!("@{p}ms"),
        _ => String::new(),
    };
    for node in classed(Class::Source) {
        let right = match node.kind {
            Kind::Const => match node.config.value {
                Some(value) => format!("{value}"),
                None => continue,
            },
            Kind::Analog => match node.config.pin {
                Some(pin) => format!("adc{pin}"),
                None => continue,
            },
            _ => {
                let Some(pin) = node.config.pin else { continue };
                let mut right = format!("gpio{pin}");
                match node.config.pull {
                    Pull::None => {}
                    Pull::Up => right.push_str(",pullup"),
                    Pull::Down => right.push_str(",pulldown"),
                }
                if node.config.invert {
                    right.push_str(",invert");
                }
                right
            }
        };
        for target in graph.targets(&node.id).filter(|t| is_channel(t)) {
            out.push(format!("{target}={right}{}", rate(node)));
        }
    }
    for node in classed(Class::Function) {
        let spec = node.kind.spec();
        let mut args: Vec<String> =
            graph.sources(&node.id).filter(|a| is_channel(a)).map(ToString::to_string).collect();
        if args.is_empty() {
            continue;
        }
        for p in spec.params.iter().filter(|p| p.name != "period_ms") {
            match p.ty {
                ParamType::Text => {
                    // A quote cannot be written inside quotes; the check
                    // rejects such a text anyway.
                    if let Some(text) = node.config.get_text(p.name) {
                        args.push(format!("{}=\"{}\"", p.name, text.replace('"', "")));
                    }
                }
                ParamType::Real => {
                    if let Some(v) = node.config.get_real(p.name).filter(|v| Some(*v) != p.real.2) {
                        args.push(format!("{}={v}", p.name));
                    }
                }
                _ => {
                    if let Some(v) = node.config.get(p.name).filter(|v| Some(*v) != p.default) {
                        args.push(format!("{}={v}", p.name));
                    }
                }
            }
        }
        let right = format!("{}({}){}", spec.name, args.join(","), rate(node));
        let targets: Vec<&str> = graph.targets(&node.id).filter(|t| is_channel(t)).collect();
        if is_model(node.kind) {
            // Ordered outputs: one statement drives them all.
            if !targets.is_empty() {
                out.push(format!("{}={right}", targets.join(",")));
            }
            continue;
        }
        for target in targets {
            out.push(format!("{target}={right}"));
        }
    }
    for node in classed(Class::Sink) {
        let Some(source) = graph.sources(&node.id).find(|a| is_channel(a)) else { continue };
        if node.kind == Kind::Pwm {
            let (Some(timer), Some(output)) = (node.config.timer, node.config.channel) else { continue };
            let freq = match node.config.freq_hz {
                Some(hz) if hz != crate::graph::DEFAULT_PWM_HZ => format!(",freq={hz}"),
                _ => String::new(),
            };
            out.push(format!("pwm{timer}.{output}={source}{freq}{}", rate(node)));
        } else if let Some(pin) = node.config.pin {
            let invert = if node.config.invert { ",invert" } else { "" };
            out.push(format!("gpio{pin}={source}{invert}{}", rate(node)));
        }
    }
    out
}

/// The `mwctl wire` command line for `graph` (statements quoted where a
/// shell needs it): `--name`, `--period`, `--trace` (every channel) or
/// `--trace a,b`, then the statements.
pub fn command_line(graph: &Graph) -> String {
    let mut line = String::from("mwctl wire");
    if graph.name != "wire" {
        line.push_str(&format!(" --name {}", graph.name));
    }
    if graph.period_ms != crate::graph::MIN_PERIOD_MS {
        line.push_str(&format!(" --period {}", graph.period_ms));
    }
    let channels = || graph.nodes.iter().filter(|n| n.kind == Kind::Channel);
    let traced: Vec<&str> = channels().filter(|n| n.config.trace).map(|n| n.id.as_str()).collect();
    if !traced.is_empty() {
        line.push_str(" --trace");
        if traced.len() < channels().count() {
            line.push(' ');
            line.push_str(&traced.join(","));
        }
    }
    for s in format(graph) {
        line.push(' ');
        line.push_str(&shell_word(&s));
    }
    line
}

/// `word` as one shell argument: as it is when every character is plain,
/// else in single quotes (a single quote inside as `'\''`).
pub fn shell_word(word: &str) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || "_=,.@:/+-".contains(c);
    if !word.is_empty() && word.chars().all(plain) {
        return word.to_string();
    }
    format!("'{}'", word.replace('\'', "'\\''"))
}
