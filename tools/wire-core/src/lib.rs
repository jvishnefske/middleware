//! Signal wiring for payloads, `no_std` with `alloc`.
//!
//! A wiring connects a board's pins through named channels, each carrying
//! an `i32` (a boolean is 0 or 1; a boolean function reads any nonzero
//! value as true):
//!
//! * a **source** (a digital input, an analog input, a constant) drives
//!   one or more **channels**;
//! * a **function** reads channels and drives channels: boolean (`and`,
//!   `or`, `xor`, `not`, `nand`, `nor`, `xnor`), arithmetic and comparison
//!   (`add`, `sub`, `mul`, `min`, `max`, `abs`, `gt` ... `ne`, `select`,
//!   `scale`, `clamp`) and stateful signal blocks (`delay`, `lowpass`,
//!   `mean`, `diff`, `hyst`, `debounce`, `rising`, `falling`, `count`,
//!   `latch`, `ton`), and two model-based blocks whose models are written
//!   as expressions ([`expr`]) and computed in `f64` ([`real`]): an
//!   unscented Kalman filter `ukf` and a control barrier function safety
//!   filter `cbf` ([`model`]), each driving several channels in order;
//! * a channel drives **sinks**: digital outputs and PWM outputs.
//!
//! Those are the only connections. [`spec`] is the one table of kinds
//! (class, arity, settings, statefulness) everything else reads. A wiring
//! has two spellings that convert both ways:
//!
//! * [`graph::Graph`], JSON with nodes, edges and a canvas position per
//!   node (what the web page's canvas edits; the shape is `graph-core`'s);
//! * [`text`] statements, one per command-line argument (what `mwctl wire`
//!   takes): `button=gpio15,pullup,invert`, `lit=and(button,armed)`,
//!   `level=lowpass(raw,shift=4)@100ms`, `gpio25=lit`.
//!
//! The wiring is multi-rate: its period is the base tick and every source,
//! function and sink may update on a multiple of it. All nodes update in
//! one topological order in which a `delay` counts as a source, so a
//! feedback loop is allowed exactly when it passes through a `delay`
//! ([`plan`] has the rules).
//!
//! [`check::check`] reports every problem at once in plain language, each
//! naming the node or edge it is about, optionally against a chip's pin map
//! from `board-pins` (the firmwares' own rules). [`plan::Plan`] is a checked
//! wiring in update order; [`plan::Sim`] is the reference semantics and
//! [`wasm::compile`] emits a payload for the middleware ABI that does the
//! same once per base tick.
//!
//! This crate has no I/O and no `unsafe`.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod check;
pub mod expr;
pub mod graph;
pub mod model;
pub mod plan;
pub mod real;
pub mod spec;
pub mod text;
pub mod wasm;

pub use check::{Issue, Severity, check, has_errors};
pub use graph::{Config, Graph, Kind, Node, Pull};
pub use plan::{Io, Plan, Sim};

use alloc::vec::Vec;
use board_pins::PinMap;

/// Checks `graph` (against `pins` when the chip is known) and compiles it.
/// Errors stop the build and come back with the warnings; a build that
/// succeeds returns the payload and the warnings.
pub fn build(graph: &Graph, pins: Option<&PinMap>) -> Result<(Vec<u8>, Vec<Issue>), Vec<Issue>> {
    let issues = check(graph, pins);
    if has_errors(&issues) {
        return Err(issues);
    }
    match Plan::new(graph) {
        Ok(plan) => Ok((wasm::compile(&plan), issues)),
        Err(more) => Err(more),
    }
}
