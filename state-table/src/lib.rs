//! State-machine tables for payloads: what a table may contain, and what
//! it does when an event is raised.
//!
//! The web editor (`web/src/model/machines.ts`) owns a table's shape and
//! turns it into AssemblyScript (`web/src/blocks/`); this crate owns the
//! rules. `web/protocol-wasm` converts the editor's JS objects into the
//! borrowed views below, runs [`check()`] and renders each [`Issue`] in
//! plain language. The crate is `no_std` with no heap and no dependencies,
//! so a firmware can run the same tables.
//!
//! * [`guard`] — guard expressions as a flat arena evaluated in one
//!   forward pass ([`Guard`], [`Node`]).
//! * [`check`] — [`check()`]: every rule below, emitted as structured
//!   [`Issue`]s (indices and borrowed names, no strings built).
//! * [`step`] — the reference dispatch ([`step()`]) and the bounded raise
//!   queue ([`RaiseQueue`], [`dispatch`]).
//!
//! # Semantics (what the generated code implements)
//!
//! A machine is in one of its states. When event `e` is raised on a
//! machine in state `s`, the rows of the *cell* (`s`, `e`) are considered
//! in table order: the first **guarded** row whose guard is true fires; if
//! none is, the cell's **unguarded** row fires; otherwise nothing happens.
//! A row whose `next` names no state never fires. When a row fires, its
//! actions run in order; then, if `next` differs from `s`, the machine
//! moves to `next` and runs `next`'s entry actions. A self-transition
//! (`next == s`) runs only the row's actions. The initial state's entry
//! actions run once, in `init`.
//!
//! Actions are a closed set: write a pin high or low, toggle a pin,
//! publish constant text on a key, log constant text, raise an event of
//! the same machine.
//!
//! Raised events are bounded. A raise while any machine is stepping (from
//! a row's or an entry action, or from a "when machine enters state"
//! block) is queued; each machine has one pending slot, so a second raise
//! for a machine that already has one pending is dropped (and logged).
//! After the outermost step the pending slots are drained once, in machine
//! order; a raise during the drain is dropped. So one external raise runs
//! at most `1 + machines` steps ([`dispatch`] models this and its tests
//! check the bound).
//!
//! # Rules ([`check()`])
//!
//! Errors unless marked as a warning:
//!
//! * machine `id`: an identifier ([`ident_ok`]: `[A-Za-z][A-Za-z0-9_]*`, at
//!   most [`MAX_IDENT`] bytes), unique across machines ignoring case;
//! * at least one state; at most [`MAX_STATES`] states, [`MAX_EVENTS`]
//!   events and [`MAX_ROWS`] rows (over a limit, the size is reported and
//!   the rest of that machine is not checked);
//! * state and event names: identifiers, unique ignoring case within the
//!   machine;
//! * `initial` names a state (fix: the first state);
//! * per row: `state`, `event` and `next` name a state / event / state of
//!   the machine (fix: add it, when the name is an identifier with no
//!   case-only match); the guard is a well-formed arena of at most
//!   [`MAX_GUARD_NODES`] nodes and depth [`MAX_GUARD_DEPTH`]; its pins are
//!   in the pin table (fix: add a `gpio-in` row) and are digital
//!   (`gpio-in` or `gpio-out`);
//! * per action (rows and state entries), at most [`MAX_ACTIONS`]: a
//!   write / toggle label is in the pin table (fix: add a `gpio-out` row)
//!   and is a `gpio-out`; a publish key passes the key rule given to
//!   [`check()`] and is not under `sys/`; publish and log text is at most
//!   [`MAX_TEXT`] bytes; a raise names an event of the same machine (fix:
//!   add it);
//! * per cell: at most one unguarded row (a guard that is always true
//!   counts as unguarded), reported on each later row naming the first;
//!   two guarded rows overlap when some 0/1 assignment of their pins makes
//!   both true, reported on the later row with that assignment (decided
//!   by trying every assignment of their distinct pins; above
//!   [`MAX_OVERLAP_PINS`] pins, a warning that overlap was not checked);
//! * warnings: a guard that is never true, a state unreachable from
//!   `initial`, an event no row handles.
//!
//! A table with no errors dispatches the same whatever its row order:
//! guarded rows of a cell are pairwise exclusive and there is at most one
//! default.
//!
//! Every loop is bounded by an input slice's length or a constant; the
//! only nested enumeration (overlap) is capped at `2^12` assignments per
//! pair of rows.
#![no_std]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod check;
pub mod guard;
pub mod step;

pub use check::{
    Field, Fix, Issue, IssueKind, KeyProblem, List, MAX_OVERLAP_PINS, Severity, Witness, check,
};
pub use guard::{CmpOp, Guard, GuardError, MAX_GUARD_DEPTH, MAX_GUARD_NODES, Node};
pub use step::{Fired, MAX_RAISES_PER_STEP, RaiseQueue, Raised, Raises, dispatch, step};

/// Longest machine id, state name or event name, in bytes (the pin
/// table's label limit).
pub const MAX_IDENT: usize = 24;
/// Most states per machine.
pub const MAX_STATES: usize = 16;
/// Most events per machine.
pub const MAX_EVENTS: usize = 16;
/// Most transition rows per machine.
pub const MAX_ROWS: usize = 64;
/// Most actions per row or per state entry.
pub const MAX_ACTIONS: usize = 4;
/// Longest publish or log text, in bytes (a message's size).
pub const MAX_TEXT: usize = 512;

/// What a pin-table row does, as far as machines care.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinUse {
    /// `gpio-out`: written by write / toggle, readable by guards.
    Output,
    /// `gpio-in`: read by guards.
    Input,
    /// Any other function (ADC, PWM, a bus pin): not usable here.
    Other,
}

/// A pin-table row: its label and what it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pin<'a> {
    /// The label blocks and machines use.
    pub label: &'a str,
    /// Its function.
    pub func: PinUse,
}

/// One action of a row or a state's entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action<'a> {
    /// Drive an output pin.
    Write {
        /// The pin's label.
        label: &'a str,
        /// High (`true`) or low.
        level: bool,
    },
    /// Invert an output pin.
    Toggle {
        /// The pin's label.
        label: &'a str,
    },
    /// Publish constant text on a key.
    Publish {
        /// The key.
        key: &'a str,
        /// The message text.
        text: &'a str,
    },
    /// Log constant text.
    Log {
        /// The line.
        text: &'a str,
    },
    /// Raise an event of the same machine (queued; see the crate docs).
    Raise {
        /// The event's name.
        event: &'a str,
    },
}

/// A state: its name and entry actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct State<'a> {
    /// The name rows and blocks use.
    pub name: &'a str,
    /// Run on entering the state (the initial state's, in init).
    pub entry: &'a [Action<'a>],
}

/// An event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Event<'a> {
    /// The name rows, raises and blocks use.
    pub name: &'a str,
}

/// A transition row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row<'a> {
    /// The state it fires in.
    pub state: &'a str,
    /// The event it fires on.
    pub event: &'a str,
    /// The condition, or `None` for the cell's default.
    pub guard: Option<Guard<'a>>,
    /// The state it moves to.
    pub next: &'a str,
    /// What it does.
    pub actions: &'a [Action<'a>],
}

/// A state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Machine<'a> {
    /// Identifier naming the generated code.
    pub id: &'a str,
    /// The state `init` enters.
    pub initial: &'a str,
    /// The states.
    pub states: &'a [State<'a>],
    /// The events.
    pub events: &'a [Event<'a>],
    /// The transition rows.
    pub rows: &'a [Row<'a>],
}

impl<'a> Machine<'a> {
    /// The index of the state named exactly `name` (the first, if
    /// duplicated).
    pub fn state_index(&self, name: &str) -> Option<usize> {
        self.states.iter().position(|s| s.name == name)
    }

    /// The index of the event named exactly `name` (the first, if
    /// duplicated).
    pub fn event_index(&self, name: &str) -> Option<usize> {
        self.events.iter().position(|e| e.name == name)
    }
}

/// Whether `s` is an identifier: `[A-Za-z][A-Za-z0-9_]*`, at most
/// [`MAX_IDENT`] bytes (the pin table's label rule).
pub fn ident_ok(s: &str) -> bool {
    let mut chars = s.chars();
    s.len() <= MAX_IDENT
        && chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_start_with_a_letter_and_fit_24_bytes() {
        assert!(ident_ok("Mode"));
        assert!(ident_ok("a_1"));
        assert!(ident_ok("A23456789012345678901234"));
        assert!(!ident_ok("A234567890123456789012345"));
        assert!(!ident_ok(""));
        assert!(!ident_ok("1st"));
        assert!(!ident_ok("_x"));
        assert!(!ident_ok("has space"));
        assert!(!ident_ok("é"));
    }

    #[test]
    fn lookups_match_names_exactly() {
        let states = [
            State {
                name: "Off",
                entry: &[],
            },
            State {
                name: "On",
                entry: &[],
            },
        ];
        let events = [Event { name: "PRESS" }];
        let m = Machine {
            id: "M",
            initial: "Off",
            states: &states,
            events: &events,
            rows: &[],
        };
        assert_eq!(m.state_index("On"), Some(1));
        assert_eq!(m.state_index("on"), None);
        assert_eq!(m.event_index("PRESS"), Some(0));
        assert_eq!(m.event_index("press"), None);
    }
}
