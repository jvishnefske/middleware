//! ISA-88 (S88) batch control for payloads, as pure functions.
//!
//! The web editor models a plant in three S88 models plus the procedural
//! control model (`design.md`, "S88 recipe model"):
//!
//! * the **physical model** — units (one board each), their equipment
//!   modules and the control modules (pin-table rows) those own;
//! * the **procedural control model** — the equipment phases each
//!   equipment module offers, every one a [`phase`] state machine whose
//!   acting states run payload logic on the unit;
//! * the **master recipe** — an equipment-independent procedure of steps
//!   naming phase classes and parameters ([`recipe`]);
//! * the **control recipe** — the master recipe bound to units by
//!   [`alloc`], executed by the [`engine`].
//!
//! This crate owns the rules; the editor owns the shapes. It is `no_std`
//! with no heap and no recursion, so a unit can host the same phase table
//! and a coordinator can run the engine on a microcontroller.
//!
//! * [`phase`] — the phase state machine: [`PhaseState`], [`Command`],
//!   [`phase::step`] as a pure `(state, input) -> (state, effect)`.
//! * [`recipe`] — master recipe views ([`Recipe`], [`Step`],
//!   [`Transition`], [`PhaseClass`]), [`recipe::check`] and the compiled
//!   sequential function chart ([`Sfc`]).
//! * [`alloc`] — matching phase classes to units' advertised phases:
//!   [`alloc::allocate`] with deterministic tie-breaking and structured
//!   rejection reasons.
//! * [`engine`] — the control-recipe engine: a pure state machine over
//!   [`engine::Input`]s (operator commands, acknowledgements, phase state
//!   reports, unit loss, time) emitting [`engine::Effect`]s.
//! * [`wire`] — the phase command contract over middleware request /
//!   response and pub/sub: keys and fixed binary bodies.
//! * [`sim`] — simulated units answering the engine, and a bounded driver
//!   that runs a recipe against them.
//!
//! Every loop is bounded by a slice length or one of the constants below.
#![no_std]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod alloc;
pub mod engine;
#[cfg(test)]
mod fixture;
pub mod phase;
pub mod recipe;
pub mod sim;
pub mod wire;

pub use phase::{Command, PhaseState};
pub use recipe::{Arg, ParamDef, PhaseClass, Recipe, Sfc, Step, Transition, UnitProc};
pub use state_table::{MAX_IDENT, ident_ok};

/// Most phase classes.
pub const MAX_CLASSES: usize = 16;
/// Most parameters per phase class.
pub const MAX_PARAMS: usize = 4;
/// Most unit procedures per recipe (and units one recipe can bind).
pub const MAX_UNIT_PROCS: usize = 4;
/// Most steps per recipe.
pub const MAX_STEPS: usize = 16;
/// Most transitions per recipe.
pub const MAX_TRANSITIONS: usize = 16;
/// Most units allocation considers.
pub const MAX_UNITS: usize = 8;
/// Most equipment phases per unit.
pub const MAX_UNIT_PHASES: usize = 16;

/// A set of steps (bit `i` = step `i`); [`MAX_STEPS`] fits.
pub type StepMask = u32;

/// Bit `i` of a [`StepMask`].
pub const fn bit(i: usize) -> StepMask {
    1 << i
}
