//! The one table of node kinds: what each connects to, how many channels
//! it reads, which settings it takes and whether it keeps state.
//!
//! [`crate::check`], [`crate::text`], [`crate::plan`] and [`crate::wasm`]
//! all read [`Kind::spec`]; [`kinds_json`] is the same table for a page's
//! palette and property editor, so nothing restates it.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::graph::{
    Class, Config, DEFAULT_PWM_HZ, Kind, MAX_FUNCTION_INPUTS, MAX_NODE_PERIOD_MS, MIN_PERIOD_MS,
    Pull,
};

/// How a setting is edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamType {
    /// A whole number between `min` and `max`.
    Int,
    /// `true` or `false` (1 or 0).
    Bool,
    /// A pull resistor: `none`, `up`, `down` (0, 1, 2).
    Pull,
    /// A real number (`f64`, a JSON number).
    Real,
    /// Text: expressions or numbers separated by `;` ([`crate::expr`]).
    Text,
}

/// One setting of a node kind: a field of [`Config`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Param {
    /// The [`Config`] field, and the name in a function statement.
    pub name: &'static str,
    /// How it is edited.
    pub ty: ParamType,
    /// Smallest value.
    pub min: i64,
    /// Largest value.
    pub max: i64,
    /// The value when the field is absent; `None` when there is no fixed
    /// one (a required setting, or `period_ms`, which follows the wiring).
    pub default: Option<i64>,
    /// Whether the node is an error without it.
    pub required: bool,
    /// For [`ParamType::Real`]: the smallest and largest value (infinite
    /// when there is no limit) and the value when absent.
    pub real: (f64, f64, Option<f64>),
    /// For [`ParamType::Text`]: a short example.
    pub hint: &'static str,
}

/// What the table says about one kind.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spec {
    /// The kind.
    pub kind: Kind,
    /// Its JSON and command-line name.
    pub name: &'static str,
    /// Its connection class.
    pub class: Class,
    /// How many channels it reads: `(fewest, most)`.
    pub inputs: (usize, usize),
    /// How many different values it drives: `(fewest, most)`. `(1, 1)` is
    /// one value, written to every channel the node connects to; more
    /// than one means ordered outputs, one channel each (`ukf`, `cbf`);
    /// `(0, 0)` is a sink.
    pub outputs: (usize, usize),
    /// Whether it keeps a value between updates.
    pub stateful: bool,
    /// The settings it takes.
    pub params: &'static [Param],
}

impl Spec {
    /// The setting called `name`, if this kind takes it.
    pub fn param(&self, name: &str) -> Option<&'static Param> {
        self.params.iter().find(|p| p.name == name)
    }
}

const I32_MIN: i64 = i32::MIN as i64;
const I32_MAX: i64 = i32::MAX as i64;

const NO_REAL: (f64, f64, Option<f64>) = (f64::NEG_INFINITY, f64::INFINITY, None);

const fn required(name: &'static str, min: i64, max: i64) -> Param {
    Param { name, ty: ParamType::Int, min, max, default: None, required: true, real: NO_REAL, hint: "" }
}

const fn optional(name: &'static str, min: i64, max: i64, default: i64) -> Param {
    Param { default: Some(default), required: false, ..required(name, min, max) }
}

const fn flag(name: &'static str) -> Param {
    Param { ty: ParamType::Bool, ..optional(name, 0, 1, 0) }
}

/// A real setting; an infinite limit is no limit.
const fn real(name: &'static str, min: f64, max: f64, default: Option<f64>) -> Param {
    Param { ty: ParamType::Real, real: (min, max, default), default: None, ..optional(name, 0, 0, 0) }
}

const fn text(name: &'static str, required: bool, hint: &'static str) -> Param {
    Param { ty: ParamType::Text, required, hint, default: None, ..optional(name, 0, 0, 0) }
}

const PIN: Param = required("pin", 0, 255);
const PULL: Param = Param { ty: ParamType::Pull, ..optional("pull", 0, 2, 0) };
const INVERT: Param = flag("invert");
/// Every source, function and sink may update slower than the wiring.
const PERIOD: Param =
    Param { default: None, ..optional("period_ms", MIN_PERIOD_MS as i64, MAX_NODE_PERIOD_MS as i64, 0) };
/// Channel units per 1.0 inside a model-based block.
const SCALE_UNITS: Param = optional("scale", 1, 1_000_000, crate::model::DEFAULT_SCALE as i64);
const LO: Param = required("lo", I32_MIN, I32_MAX);
const HI: Param = required("hi", I32_MIN, I32_MAX);

const RATE_ONLY: &[Param] = &[PERIOD];
const INPUT: &[Param] = &[PIN, PULL, INVERT, PERIOD];
const ANALOG: &[Param] = &[PIN, PERIOD];
const CONST: &[Param] = &[required("value", I32_MIN, I32_MAX), PERIOD];
const CHANNEL: &[Param] = &[flag("trace")];
const OUTPUT: &[Param] = &[PIN, INVERT, PERIOD];
const PWM: &[Param] = &[
    required("timer", 0, 3),
    required("channel", 0, 1),
    optional("freq_hz", 1, 40_000_000, DEFAULT_PWM_HZ as i64),
    PERIOD,
];
const SCALE: &[Param] =
    &[optional("mul", I32_MIN, I32_MAX, 1), optional("div", I32_MIN, I32_MAX, 1), PERIOD];
const RANGE: &[Param] = &[LO, HI, PERIOD];
const LOWPASS: &[Param] = &[required("shift", 1, 16), PERIOD];
const MEAN: &[Param] = &[required("n", 2, 4096), PERIOD];
const DEBOUNCE: &[Param] = &[required("n", 1, 60_000), PERIOD];
const TON: &[Param] = &[required("ms", 1, MAX_NODE_PERIOD_MS as i64), PERIOD];
const UKF: &[Param] = &[
    required("n", 1, 4),
    text("f", true, "x0+dt*x1; x1"),
    text("h", true, "x0"),
    text("q", true, "1e-6; 1e-4"),
    text("r", true, "0.01"),
    text("x0", false, "0; 0"),
    text("p0", false, "1; 1"),
    real("alpha", 1e-3, 1.0, Some(1.0)),
    real("beta", f64::NEG_INFINITY, f64::INFINITY, Some(2.0)),
    real("kappa", f64::NEG_INFINITY, f64::INFINITY, Some(0.0)),
    SCALE_UNITS,
    PERIOD,
];
const CBF: &[Param] = &[
    required("n", 1, 4),
    text("h", true, "2-x0"),
    text("f", true, "0"),
    text("g", true, "1"),
    real("alpha", 1e-9, 1e9, Some(1.0)),
    real("umin", f64::NEG_INFINITY, f64::INFINITY, None),
    real("umax", f64::NEG_INFINITY, f64::INFINITY, None),
    SCALE_UNITS,
    PERIOD,
];

const MANY: (usize, usize) = (2, MAX_FUNCTION_INPUTS);

const fn function(
    kind: Kind,
    name: &'static str,
    inputs: (usize, usize),
    stateful: bool,
    params: &'static [Param],
) -> Spec {
    Spec { kind, name, class: Class::Function, inputs, outputs: (1, 1), stateful, params }
}

const fn source(kind: Kind, name: &'static str, params: &'static [Param]) -> Spec {
    Spec { kind, name, class: Class::Source, inputs: (0, 0), outputs: (1, 1), stateful: false, params }
}

const fn sink(kind: Kind, name: &'static str, params: &'static [Param]) -> Spec {
    Spec { kind, name, class: Class::Sink, inputs: (1, 1), outputs: (0, 0), stateful: false, params }
}

/// Every kind, in the order palettes list them: sources, the channel,
/// boolean functions, arithmetic, comparisons, stateful blocks, the
/// model-based blocks, sinks.
pub const KINDS: [Kind; 41] = [
    Kind::Input,
    Kind::Analog,
    Kind::Const,
    Kind::Channel,
    Kind::And,
    Kind::Or,
    Kind::Not,
    Kind::Xor,
    Kind::Nand,
    Kind::Nor,
    Kind::Xnor,
    Kind::Add,
    Kind::Sub,
    Kind::Mul,
    Kind::Min,
    Kind::Max,
    Kind::Abs,
    Kind::Gt,
    Kind::Lt,
    Kind::Ge,
    Kind::Le,
    Kind::Eq,
    Kind::Ne,
    Kind::Select,
    Kind::Scale,
    Kind::Clamp,
    Kind::Delay,
    Kind::Lowpass,
    Kind::Mean,
    Kind::Diff,
    Kind::Hyst,
    Kind::Debounce,
    Kind::Rising,
    Kind::Falling,
    Kind::Count,
    Kind::Latch,
    Kind::Ton,
    Kind::Ukf,
    Kind::Cbf,
    Kind::Output,
    Kind::Pwm,
];

impl Kind {
    /// The table row for this kind.
    pub const fn spec(self) -> Spec {
        match self {
            Kind::Input => source(self, "input", INPUT),
            Kind::Analog => source(self, "analog", ANALOG),
            Kind::Const => source(self, "const", CONST),
            Kind::Channel => Spec {
                kind: self,
                name: "channel",
                class: Class::Channel,
                inputs: (1, 1),
                outputs: (1, 1),
                stateful: false,
                params: CHANNEL,
            },
            Kind::And => function(self, "and", MANY, false, RATE_ONLY),
            Kind::Or => function(self, "or", MANY, false, RATE_ONLY),
            Kind::Xor => function(self, "xor", MANY, false, RATE_ONLY),
            Kind::Not => function(self, "not", (1, 1), false, RATE_ONLY),
            Kind::Nand => function(self, "nand", MANY, false, RATE_ONLY),
            Kind::Nor => function(self, "nor", MANY, false, RATE_ONLY),
            Kind::Xnor => function(self, "xnor", MANY, false, RATE_ONLY),
            Kind::Add => function(self, "add", MANY, false, RATE_ONLY),
            Kind::Sub => function(self, "sub", (2, 2), false, RATE_ONLY),
            Kind::Mul => function(self, "mul", (2, 2), false, RATE_ONLY),
            Kind::Min => function(self, "min", MANY, false, RATE_ONLY),
            Kind::Max => function(self, "max", MANY, false, RATE_ONLY),
            Kind::Abs => function(self, "abs", (1, 1), false, RATE_ONLY),
            Kind::Gt => function(self, "gt", (2, 2), false, RATE_ONLY),
            Kind::Lt => function(self, "lt", (2, 2), false, RATE_ONLY),
            Kind::Ge => function(self, "ge", (2, 2), false, RATE_ONLY),
            Kind::Le => function(self, "le", (2, 2), false, RATE_ONLY),
            Kind::Eq => function(self, "eq", (2, 2), false, RATE_ONLY),
            Kind::Ne => function(self, "ne", (2, 2), false, RATE_ONLY),
            Kind::Select => function(self, "select", (3, 3), false, RATE_ONLY),
            Kind::Scale => function(self, "scale", (1, 1), false, SCALE),
            Kind::Clamp => function(self, "clamp", (1, 1), false, RANGE),
            Kind::Delay => function(self, "delay", (1, 1), true, RATE_ONLY),
            Kind::Lowpass => function(self, "lowpass", (1, 1), true, LOWPASS),
            Kind::Mean => function(self, "mean", (1, 1), true, MEAN),
            Kind::Diff => function(self, "diff", (1, 1), true, RATE_ONLY),
            Kind::Hyst => function(self, "hyst", (1, 1), true, RANGE),
            Kind::Debounce => function(self, "debounce", (1, 1), true, DEBOUNCE),
            Kind::Rising => function(self, "rising", (1, 1), true, RATE_ONLY),
            Kind::Falling => function(self, "falling", (1, 1), true, RATE_ONLY),
            Kind::Count => function(self, "count", (1, 2), true, RATE_ONLY),
            Kind::Latch => function(self, "latch", (2, 2), true, RATE_ONLY),
            Kind::Ton => function(self, "ton", (1, 1), true, TON),
            Kind::Ukf => Spec { outputs: (1, 8), ..function(self, "ukf", (1, 8), true, UKF) },
            Kind::Cbf => Spec { outputs: (1, 3), ..function(self, "cbf", (2, 6), false, CBF) },
            Kind::Output => sink(self, "output", OUTPUT),
            Kind::Pwm => sink(self, "pwm", PWM),
        }
    }
}

/// Every [`Config`] field, in JSON order.
pub const FIELDS: [&str; 29] = [
    "pin",
    "pull",
    "invert",
    "value",
    "timer",
    "channel",
    "freq_hz",
    "mul",
    "div",
    "lo",
    "hi",
    "shift",
    "n",
    "ms",
    "f",
    "h",
    "g",
    "q",
    "r",
    "x0",
    "p0",
    "alpha",
    "beta",
    "kappa",
    "umin",
    "umax",
    "scale",
    "period_ms",
    "trace",
];

impl Config {
    /// Whether the field called `name` is set (for `pull`, `invert` and
    /// `trace`: not at its default).
    pub fn has(&self, name: &str) -> bool {
        self.get(name).is_some() || self.get_real(name).is_some() || self.get_text(name).is_some()
    }

    /// The real field called `name`.
    pub fn get_real(&self, name: &str) -> Option<f64> {
        match name {
            "alpha" => self.alpha,
            "beta" => self.beta,
            "kappa" => self.kappa,
            "umin" => self.umin,
            "umax" => self.umax,
            _ => None,
        }
    }

    /// Sets the real field called `name`; `false` when there is none or
    /// `value` is not finite.
    pub fn set_real(&mut self, name: &str, value: f64) -> bool {
        let slot = match name {
            "alpha" => &mut self.alpha,
            "beta" => &mut self.beta,
            "kappa" => &mut self.kappa,
            "umin" => &mut self.umin,
            "umax" => &mut self.umax,
            _ => return false,
        };
        if !value.is_finite() {
            return false;
        }
        *slot = Some(value);
        true
    }

    /// The text field called `name`.
    pub fn get_text(&self, name: &str) -> Option<&str> {
        match name {
            "f" => self.f.as_deref(),
            "h" => self.h.as_deref(),
            "g" => self.g.as_deref(),
            "q" => self.q.as_deref(),
            "r" => self.r.as_deref(),
            "x0" => self.x0.as_deref(),
            "p0" => self.p0.as_deref(),
            _ => None,
        }
    }

    /// Sets the text field called `name`; `false` when there is none.
    pub fn set_text(&mut self, name: &str, value: &str) -> bool {
        let slot = match name {
            "f" => &mut self.f,
            "h" => &mut self.h,
            "g" => &mut self.g,
            "q" => &mut self.q,
            "r" => &mut self.r,
            "x0" => &mut self.x0,
            "p0" => &mut self.p0,
            _ => return false,
        };
        *slot = Some(value.into());
        true
    }

    /// The field called `name` as a number, `None` when it is absent (or,
    /// for `pull`, `invert` and `trace`, at its default).
    pub fn get(&self, name: &str) -> Option<i64> {
        match name {
            "pin" => self.pin.map(i64::from),
            "pull" => (self.pull != Pull::None).then(|| i64::from(self.pull.code())),
            "invert" => self.invert.then_some(1),
            "value" => self.value.map(i64::from),
            "timer" => self.timer.map(i64::from),
            "channel" => self.channel.map(i64::from),
            "freq_hz" => self.freq_hz.map(i64::from),
            "mul" => self.mul.map(i64::from),
            "div" => self.div.map(i64::from),
            "lo" => self.lo.map(i64::from),
            "hi" => self.hi.map(i64::from),
            "shift" => self.shift.map(i64::from),
            "n" => self.n.map(i64::from),
            "ms" => self.ms.map(i64::from),
            "scale" => self.scale.map(i64::from),
            "period_ms" => self.period_ms.map(i64::from),
            "trace" => self.trace.then_some(1),
            _ => None,
        }
    }

    /// Sets the field called `name`; `false` when there is no such field
    /// or `value` does not fit it.
    pub fn set(&mut self, name: &str, value: i64) -> bool {
        fn fit<T: TryFrom<i64>>(slot: &mut Option<T>, value: i64) -> bool {
            match T::try_from(value) {
                Ok(v) => {
                    *slot = Some(v);
                    true
                }
                Err(_) => false,
            }
        }
        match name {
            "pin" => fit(&mut self.pin, value),
            "pull" => {
                self.pull = match value {
                    0 => Pull::None,
                    1 => Pull::Up,
                    2 => Pull::Down,
                    _ => return false,
                };
                true
            }
            "invert" | "trace" if value != 0 && value != 1 => false,
            "invert" => {
                self.invert = value == 1;
                true
            }
            "trace" => {
                self.trace = value == 1;
                true
            }
            "value" => fit(&mut self.value, value),
            "timer" => fit(&mut self.timer, value),
            "channel" => fit(&mut self.channel, value),
            "freq_hz" => fit(&mut self.freq_hz, value),
            "mul" => fit(&mut self.mul, value),
            "div" => fit(&mut self.div, value),
            "lo" => fit(&mut self.lo, value),
            "hi" => fit(&mut self.hi, value),
            "shift" => fit(&mut self.shift, value),
            "n" => fit(&mut self.n, value),
            "ms" => fit(&mut self.ms, value),
            "scale" => fit(&mut self.scale, value),
            "period_ms" => fit(&mut self.period_ms, value),
            _ => false,
        }
    }
}

/// The table as JSON, one object per kind in [`KINDS`] order:
///
/// ```json
/// {"kind":"lowpass","class":"function","inputs":[1,1],"outputs":[1,1],"stateful":true,
///  "params":[{"name":"shift","type":"int","min":1,"max":16,"default":null,"required":true},
///            {"name":"period_ms","type":"int","min":10,"max":3600000,"default":null,"required":false}]}
/// ```
///
/// `class` is `source`, `function`, `sink` or `channel`. `outputs` is how
/// many different values the node drives: `[1,1]` is one value for every
/// channel it connects to, more than one means ordered outputs with one
/// channel each, `[0,0]` is a sink. `type` is `int`, `bool` (0 or 1 here,
/// `true`/`false` in the wiring's JSON), `pull` (0, 1, 2 here, `"none"`,
/// `"up"`, `"down"` in the wiring's JSON, listed in `options`), `real` (a
/// JSON number; `min` or `max` is `null` where there is no limit) or
/// `text` (a JSON string of expressions or numbers separated by `;`;
/// `min` and `max` are `null` and `hint` is a short example). A `default`
/// of `null` means there is no fixed one.
pub fn kinds_json() -> String {
    let number = |v: Option<f64>| match v {
        Some(v) if v.is_finite() => format!("{v}"),
        _ => String::from("null"),
    };
    let kinds: Vec<String> = KINDS
        .iter()
        .map(|kind| {
            let spec = kind.spec();
            let params: Vec<String> = spec
                .params
                .iter()
                .map(|p| {
                    let (ty, extra) = match p.ty {
                        ParamType::Int => ("int", String::new()),
                        ParamType::Bool => ("bool", String::new()),
                        ParamType::Pull => ("pull", String::from(",\"options\":[\"none\",\"up\",\"down\"]")),
                        ParamType::Real => ("real", String::new()),
                        ParamType::Text => ("text", format!(",\"hint\":\"{}\"", p.hint)),
                    };
                    let (min, max, default) = match p.ty {
                        ParamType::Real => (number(Some(p.real.0)), number(Some(p.real.1)), number(p.real.2)),
                        ParamType::Text => (number(None), number(None), number(None)),
                        _ => (
                            format!("{}", p.min),
                            format!("{}", p.max),
                            p.default.map_or_else(|| String::from("null"), |d| format!("{d}")),
                        ),
                    };
                    format!(
                        "{{\"name\":\"{}\",\"type\":\"{ty}\",\"min\":{min},\"max\":{max},\"default\":{default},\"required\":{}{extra}}}",
                        p.name, p.required
                    )
                })
                .collect();
            format!(
                "{{\"kind\":\"{}\",\"class\":\"{}\",\"inputs\":[{},{}],\"outputs\":[{},{}],\"stateful\":{},\"params\":[{}]}}",
                spec.name,
                spec.class.name(),
                spec.inputs.0,
                spec.inputs.1,
                spec.outputs.0,
                spec.outputs.1,
                spec.stateful,
                params.join(",")
            )
        })
        .collect();
    format!("[{}]", kinds.join(","))
}
