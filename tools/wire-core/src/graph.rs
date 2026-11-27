//! The wiring as a graph: what the canvas edits and what JSON files hold.
//!
//! ```json
//! {"name": "lamp", "period_ms": 10,
//!  "nodes": [
//!    {"id": "gpio15", "kind": "input", "config": {"pin": 15, "pull": "up", "invert": true}, "x": 40, "y": 40},
//!    {"id": "button", "kind": "channel"},
//!    {"id": "gpio25", "kind": "output", "config": {"pin": 25, "period_ms": 100}}],
//!  "edges": [["gpio15", "button"], ["button", "gpio25"]]}
//! ```
//!
//! The kinds and the settings each takes are in [`crate::spec`]. The edges
//! into a function are its arguments, in `edges` order; the edges out of a
//! `ukf` or `cbf` are its outputs, in `edges` order, one channel each (any
//! other source or function writes the same value to every channel it
//! drives).
//!
//! The shape check is strict: an unknown field or kind is an error, not
//! something dropped on the next save.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

/// Shortest and default scan period: the ABI's shortest tick.
pub const MIN_PERIOD_MS: u32 = 10;
/// Longest scan period.
pub const MAX_PERIOD_MS: u32 = 60_000;
/// Longest update period of one node (a multiple of the scan period).
pub const MAX_NODE_PERIOD_MS: u32 = 3_600_000;
/// The PWM frequency when a `pwm` node gives none.
pub const DEFAULT_PWM_HZ: u32 = 1000;
/// Most nodes in one wiring.
pub const MAX_NODES: usize = 128;
/// Most edges in one wiring.
pub const MAX_EDGES: usize = 256;
/// Most channels a function reads.
pub const MAX_FUNCTION_INPUTS: usize = 8;
/// Longest node id and channel name, in bytes (the `mw.channels` limit).
pub const MAX_ID: usize = 24;
/// Largest canvas coordinate.
pub const MAX_COORD: i32 = 4000;

/// What a node is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A digital input pin: 1 when the pin is high (xor `invert`).
    Input,
    /// An analog input pin: the raw reading, 0 to 4095 (0 on an error).
    Analog,
    /// A constant.
    Const,
    /// A named channel carrying an `i32`.
    Channel,
    /// 1 when every input is nonzero.
    And,
    /// 1 when any input is nonzero.
    Or,
    /// 1 when an odd number of inputs is nonzero.
    Xor,
    /// 1 when its one input is 0.
    Not,
    /// 0 when every input is nonzero.
    Nand,
    /// 0 when any input is nonzero.
    Nor,
    /// 1 when an even number of inputs is nonzero.
    Xnor,
    /// The wrapping sum of its inputs.
    Add,
    /// First input minus the second, wrapping.
    Sub,
    /// The wrapping product of its two inputs.
    Mul,
    /// The smallest input.
    Min,
    /// The largest input.
    Max,
    /// The absolute value (wrapping: the most negative value stays).
    Abs,
    /// 1 when the first input is greater than the second.
    Gt,
    /// 1 when the first input is less than the second.
    Lt,
    /// 1 when the first input is greater than or equal to the second.
    Ge,
    /// 1 when the first input is less than or equal to the second.
    Le,
    /// 1 when both inputs are equal.
    Eq,
    /// 1 when the inputs differ.
    Ne,
    /// The second input when the first is nonzero, else the third.
    Select,
    /// `x * mul / div` computed in 64 bits, truncated to 32.
    Scale,
    /// The input limited to `lo..=hi`.
    Clamp,
    /// Unit delay: the value its input had at the delay's previous update.
    /// The only node that may sit on a feedback loop.
    Delay,
    /// First-order low-pass: `acc += x - (acc >> shift); y = acc >> shift`.
    Lowpass,
    /// Integrate and dump: every `n` updates, the mean of the last `n`
    /// inputs (a wrapping 32-bit sum, signed division); it holds between.
    Mean,
    /// The input minus the input at the previous update, wrapping.
    Diff,
    /// Schmitt trigger: 1 from `x >= hi` until `x <= lo`.
    Hyst,
    /// Follows whether the input is nonzero once that has been stable for
    /// `n` consecutive updates.
    Debounce,
    /// 1 for the one update where the input became nonzero.
    Rising,
    /// 1 for the one update where the input became 0.
    Falling,
    /// Counts rising edges of its first input; 0 while its optional second
    /// input (reset) is nonzero.
    Count,
    /// Set/reset latch (inputs: set, reset), reset dominant.
    Latch,
    /// On-delay timer: 1 once the input has been nonzero for `ms`.
    Ton,
    /// Unscented Kalman filter of a model written as expressions
    /// ([`crate::model`]); drives one channel per state.
    Ukf,
    /// Control barrier function safety filter ([`crate::model`]); drives
    /// one channel per control.
    Cbf,
    /// A digital output pin: high when its channel is nonzero (xor `invert`).
    Output,
    /// A PWM output: the duty is its channel limited to 0..=65535.
    Pwm,
}

/// The four classes the connection rules are written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Drives channels and reads none: a pin or a constant.
    Source,
    /// A channel.
    Channel,
    /// Reads channels and drives channels.
    Function,
    /// Follows one channel: a pin.
    Sink,
}

/// The boolean function kinds, in the order palettes list them. Every
/// kind, with its arity and settings, is in [`crate::spec::KINDS`].
pub const FUNCTIONS: [Kind; 7] =
    [Kind::And, Kind::Or, Kind::Not, Kind::Xor, Kind::Nand, Kind::Nor, Kind::Xnor];

impl Kind {
    /// The kind as JSON and the command line spell it.
    pub const fn name(self) -> &'static str {
        self.spec().name
    }

    /// The function kind called `name`.
    pub fn function(name: &str) -> Option<Kind> {
        crate::spec::KINDS
            .into_iter()
            .find(|k| k.class() == Class::Function && k.name() == name)
    }

    /// The node's class.
    pub const fn class(self) -> Class {
        self.spec().class
    }

    /// How many channels a function reads: `(fewest, most)`.
    pub const fn arity(self) -> (usize, usize) {
        self.spec().inputs
    }

    /// Whether the node keeps a value between updates.
    pub const fn stateful(self) -> bool {
        self.spec().stateful
    }

    /// The kind in a sentence, with its article.
    pub const fn a(self) -> &'static str {
        match self {
            Kind::Input => "a digital input",
            Kind::Analog => "an analog input",
            Kind::Const => "a constant",
            Kind::Channel => "a channel",
            Kind::Output => "a digital output",
            Kind::Pwm => "a PWM output",
            _ => "a function",
        }
    }

    /// A boolean function applied to `inputs`. Not one of [`FUNCTIONS`]:
    /// `false`.
    pub fn apply(self, inputs: impl Iterator<Item = bool>) -> bool {
        let (mut all, mut any, mut odd) = (true, false, false);
        for v in inputs {
            all &= v;
            any |= v;
            odd ^= v;
        }
        match self {
            Kind::And => all,
            Kind::Nand => !all,
            Kind::Or => any,
            Kind::Nor | Kind::Not => !any,
            Kind::Xor => odd,
            Kind::Xnor => !odd,
            _ => false,
        }
    }
}

impl Class {
    /// The class as [`crate::spec::kinds_json`] spells it.
    pub const fn name(self) -> &'static str {
        match self {
            Class::Source => "source",
            Class::Channel => "channel",
            Class::Function => "function",
            Class::Sink => "sink",
        }
    }

    /// Whether an edge may run from `self` to `to`.
    pub const fn connects_to(self, to: Class) -> bool {
        matches!(
            (self, to),
            (Class::Source, Class::Channel)
                | (Class::Function, Class::Channel)
                | (Class::Channel, Class::Function)
                | (Class::Channel, Class::Sink)
        )
    }
}

/// An input's pull resistor (the ABI's `gpio_input` argument).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Pull {
    /// Floating.
    #[default]
    None,
    /// Pulled up: an open switch reads high.
    Up,
    /// Pulled down: an open switch reads low.
    Down,
}

impl Pull {
    /// The ABI code.
    pub const fn code(self) -> i32 {
        match self {
            Pull::None => 0,
            Pull::Up => 1,
            Pull::Down => 2,
        }
    }

    fn is_none(&self) -> bool {
        *self == Pull::None
    }
}

fn is_false(v: &bool) -> bool {
    !*v
}

/// A node's settings. Every field is optional and belongs to the kinds
/// whose [`crate::spec::Spec::params`] list it; on any other kind it is an
/// error ([`crate::check`]).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The GPIO number (`input`, `analog`, `output`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<u8>,
    /// The pull resistor (`input`).
    #[serde(default, skip_serializing_if = "Pull::is_none")]
    pub pull: Pull,
    /// Active low: the channel is 1 when the pin is low (`input`), or the
    /// pin is driven low when the channel is nonzero (`output`).
    #[serde(default, skip_serializing_if = "is_false")]
    pub invert: bool,
    /// The constant (`const`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<i32>,
    /// The PWM timer, 0 to 3 (`pwm`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timer: Option<u8>,
    /// The timer's output, 0 or 1 (`pwm`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<u8>,
    /// The PWM frequency, default 1000 (`pwm`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freq_hz: Option<u32>,
    /// Multiplier, default 1 (`scale`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mul: Option<i32>,
    /// Divisor, not 0, default 1 (`scale`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub div: Option<i32>,
    /// Lower limit (`clamp`) or release threshold (`hyst`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lo: Option<i32>,
    /// Upper limit (`clamp`) or trigger threshold (`hyst`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hi: Option<i32>,
    /// Filter strength, 1 to 16 (`lowpass`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shift: Option<u8>,
    /// A number of updates (`mean`: 2 to 4096, `debounce`: 1 to 60000), or
    /// of states (`ukf`, `cbf`: 1 to 4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n: Option<u32>,
    /// A time in ms (`ton`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ms: Option<u32>,
    /// Expressions separated by `;` ([`crate::expr`]): the transition
    /// `x[k+1]` per state (`ukf`), or the drift `xdot` per state (`cbf`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub f: Option<String>,
    /// Expressions: the measurement model, one per measurement (`ukf`), or
    /// the one barrier function, safe where it is not negative (`cbf`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub h: Option<String>,
    /// Expressions: the input matrix by rows, one row per state with one
    /// entry per control (`cbf`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub g: Option<String>,
    /// Numbers separated by `;`: the process noise variance per state (`ukf`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub q: Option<String>,
    /// Numbers: the measurement noise variance per measurement (`ukf`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r: Option<String>,
    /// Numbers: the initial state, default all 0 (`ukf`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x0: Option<String>,
    /// Numbers: the initial variance per state, default all 1 (`ukf`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p0: Option<String>,
    /// Sigma point spread, 0.001 to 1, default 1 (`ukf`), or the barrier's
    /// rate, greater than 0, default 1 (`cbf`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alpha: Option<f64>,
    /// Weight of the centre sigma point's covariance, default 2 (`ukf`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub beta: Option<f64>,
    /// Secondary sigma point spread, default 0 (`ukf`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kappa: Option<f64>,
    /// Lower limit of every control (`cbf`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub umin: Option<f64>,
    /// Upper limit of every control (`cbf`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub umax: Option<f64>,
    /// Channel units per 1.0 inside the block, default 1000 (`ukf`, `cbf`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<u32>,
    /// How often the node updates: a multiple of the wiring's period,
    /// which is the default (sources, functions, sinks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period_ms: Option<u32>,
    /// Publish the value on `wire/<channel>` when it changes (`channel`).
    #[serde(default, skip_serializing_if = "is_false")]
    pub trace: bool,
}

impl Config {
    fn is_default(&self) -> bool {
        *self == Config::default()
    }
}

/// One node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Node {
    /// Unique id. A channel's id is its name.
    pub id: String,
    /// What it is.
    pub kind: Kind,
    /// Its settings.
    #[serde(default, skip_serializing_if = "Config::is_default")]
    pub config: Config,
    /// Canvas position, if it has one yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<i32>,
    /// Canvas position.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<i32>,
}

impl Node {
    /// A node without a position.
    pub fn new(id: &str, kind: Kind, config: Config) -> Self {
        Node { id: id.into(), kind, config, x: None, y: None }
    }
}

fn default_period() -> u32 {
    MIN_PERIOD_MS
}

/// A wiring.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Graph {
    /// Shown in the payload's log line.
    pub name: String,
    /// The base tick: how often the payload updates the nodes that are due.
    #[serde(default = "default_period")]
    pub period_ms: u32,
    /// The nodes.
    pub nodes: Vec<Node>,
    /// `[from, to]` node ids.
    pub edges: Vec<(String, String)>,
}

impl Graph {
    /// An empty wiring.
    pub fn new(name: &str) -> Self {
        Graph { name: name.into(), period_ms: MIN_PERIOD_MS, nodes: Vec::new(), edges: Vec::new() }
    }

    /// Parses the JSON form.
    pub fn from_json(s: &str) -> Result<Self, String> {
        serde_json::from_str(s).map_err(|e| format!("wiring json: {e}"))
    }

    /// The JSON form, one line.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// The index of the node called `id`.
    pub fn index(&self, id: &str) -> Option<usize> {
        self.nodes.iter().position(|n| n.id == id)
    }

    /// The node called `id`.
    pub fn node(&self, id: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// Ids of the nodes with an edge into `id`, in edge order.
    pub fn sources<'a>(&'a self, id: &'a str) -> impl Iterator<Item = &'a str> {
        self.edges.iter().filter(move |(_, t)| t == id).map(|(f, _)| f.as_str())
    }

    /// Ids of the nodes `id` has an edge to, in edge order.
    pub fn targets<'a>(&'a self, id: &'a str) -> impl Iterator<Item = &'a str> {
        self.edges.iter().filter(move |(f, _)| f == id).map(|(_, t)| t.as_str())
    }

    /// Gives every node without a position one: a column per stage (inputs,
    /// the channels they drive, functions, their channels, ..., outputs last),
    /// rows in node order. Positions already set are kept.
    pub fn layout(&mut self) {
        const COLUMN: i32 = 190;
        const ROW: i32 = 72;
        const MARGIN: i32 = 40;
        let n = self.nodes.len();
        let mut depth = alloc::vec![0usize; n];
        // Longest path from a node nothing feeds (a delay counts as one: it
        // is where a loop is cut); bounded so a cycle ends.
        for _ in 0..n {
            let mut changed = false;
            for (f, t) in &self.edges {
                if let (Some(f), Some(t)) = (self.index(f), self.index(t))
                    && self.nodes[t].kind != Kind::Delay
                    && depth[t] < depth[f] + 1
                    && depth[f] + 1 < n
                {
                    depth[t] = depth[f] + 1;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        let deepest = depth.iter().copied().max().unwrap_or(0);
        let mut rows = alloc::vec![0i32; n + 2];
        for (i, node) in self.nodes.iter_mut().enumerate() {
            let column = match node.kind.class() {
                Class::Sink => deepest.max(depth[i]).max(1),
                _ => depth[i],
            };
            let row = rows[column];
            rows[column] += 1;
            if node.x.is_none() || node.y.is_none() {
                node.x = Some((MARGIN + COLUMN * column as i32).min(MAX_COORD));
                node.y = Some((MARGIN + ROW * row).min(MAX_COORD));
            }
        }
    }
}

/// Whether `id` may name a node: 1 to [`MAX_ID`] of `A-Z a-z 0-9 _ .`.
pub fn is_id(id: &str) -> bool {
    (1..=MAX_ID).contains(&id.len())
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.')
}

fn pin_word(word: &str, prefix: &str) -> Option<u8> {
    let digits = word.strip_prefix(prefix)?;
    if digits.is_empty() || digits.len() > 3 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// The pin a `gpio<N>` word names.
pub fn gpio_pin(word: &str) -> Option<u8> {
    pin_word(word, "gpio")
}

/// The pin an `adc<N>` word names.
pub fn adc_pin(word: &str) -> Option<u8> {
    pin_word(word, "adc")
}

/// The `(timer, output)` a `pwm<T>.<C>` word names.
pub fn pwm_output(word: &str) -> Option<(u8, u8)> {
    let (timer, output) = word.strip_prefix("pwm")?.split_once('.')?;
    let number = |digits: &str| {
        ((1..=3).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit()))
            .then(|| digits.parse::<u8>().ok())
            .flatten()
    };
    Some((number(timer)?, number(output)?))
}

/// Whether `name` may name a channel: a lower-case letter, then lower-case
/// letters, digits and `_`, at most [`MAX_ID`] bytes, and not a pin word
/// (`gpio15`, `adc26`) or a function name, which the command line reads
/// differently.
pub fn is_channel_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b) if b.is_ascii_lowercase())
        && name.len() <= MAX_ID
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        && gpio_pin(name).is_none()
        && adc_pin(name).is_none()
        && Kind::function(name).is_none()
}
