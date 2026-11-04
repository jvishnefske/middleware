//! The reference dispatch and the bounded raise queue.
//!
//! [`step()`] decides which row of a cell fires: the first guarded row (in
//! table order) whose guard is true, else the first unguarded row, else
//! none; a row whose `state`, `event` or `next` names nothing never fires
//! and a malformed guard is never true. It does not run actions; it is the
//! definition the generated `switch` must agree with.
//!
//! [`RaiseQueue`] is the raise queue as pure state transitions (each takes
//! the queue and returns a new one), and [`dispatch`] drives it for one
//! external raise: the step it starts may queue at most one event per
//! machine, those are drained once in machine order, and raises during
//! the drain are dropped. It returns how many steps ran, which is at most
//! `1 + M` for `M` machines.

use crate::{Leaf, MAX_ACTIONS, Machine};

/// Which row fired, and the state the machine is in after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fired {
    /// Index into the machine's rows.
    pub row: usize,
    /// Index into its states.
    pub next: usize,
}

/// The row that fires when `event` (an index into `machine.events`) is
/// raised in `state` (an index into `machine.states`), reading guard
/// leaves (pins, machines' states) through `read`; see the module docs.
pub fn step(
    machine: &Machine<'_>,
    read: impl Fn(Leaf<'_>) -> bool,
    state: usize,
    event: usize,
) -> Option<Fired> {
    let mut default = None;
    for (i, row) in machine.rows.iter().enumerate() {
        if machine.state_index(row.state) != Some(state)
            || machine.event_index(row.event) != Some(event)
        {
            continue;
        }
        let Some(next) = machine.state_index(row.next) else {
            continue;
        };
        match &row.guard {
            Some(g) => {
                if g.holds(&read) {
                    return Some(Fired { row: i, next });
                }
            }
            None => {
                if default.is_none() {
                    default = Some(Fired { row: i, next });
                }
            }
        }
    }
    default
}

/// Most raises one step makes: a row's actions and the next state's entry
/// actions.
pub const MAX_RAISES_PER_STEP: usize = 2 * MAX_ACTIONS;

/// The raises one step made, as `(machine, event)` in order.
pub type Raises = [Option<(usize, usize)>; MAX_RAISES_PER_STEP];

/// What happened to a raise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Raised {
    /// Nothing was stepping: run this step now (then call
    /// [`RaiseQueue::end_step`]).
    Step,
    /// Queued in the machine's pending slot.
    Queued,
    /// Dropped: the machine already had one pending, the queue is
    /// draining, or the machine index is out of range.
    Dropped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Stepping,
    Draining,
}

/// The raise queue for `M` machines: one pending event per machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RaiseQueue<const M: usize> {
    phase: Phase,
    pending: [Option<usize>; M],
}

impl<const M: usize> RaiseQueue<M> {
    /// Nothing stepping, nothing pending.
    pub const fn new() -> Self {
        RaiseQueue {
            phase: Phase::Idle,
            pending: [None; M],
        }
    }

    /// A raise of `event` on `machine`.
    pub fn raise(self, machine: usize, event: usize) -> (Self, Raised) {
        match self.phase {
            Phase::Idle if machine < M => (
                RaiseQueue {
                    phase: Phase::Stepping,
                    ..self
                },
                Raised::Step,
            ),
            Phase::Stepping => {
                let mut next = self;
                match next.pending.get_mut(machine) {
                    Some(slot @ None) => {
                        *slot = Some(event);
                        (next, Raised::Queued)
                    }
                    _ => (self, Raised::Dropped),
                }
            }
            _ => (self, Raised::Dropped),
        }
    }

    /// The outermost step finished: start draining. Returns the pending
    /// event of each machine (in machine order) and clears the slots.
    pub fn end_step(self) -> (Self, [Option<usize>; M]) {
        if self.phase != Phase::Stepping {
            return (self, [None; M]);
        }
        (
            RaiseQueue {
                phase: Phase::Draining,
                pending: [None; M],
            },
            self.pending,
        )
    }

    /// The drain finished: back to idle.
    pub fn end_drain(self) -> Self {
        RaiseQueue {
            phase: Phase::Idle,
            pending: [None; M],
        }
    }
}

impl<const M: usize> Default for RaiseQueue<M> {
    fn default() -> Self {
        Self::new()
    }
}

/// Runs one external raise of `event` on `machine` through the queue,
/// calling `run(machine, event)` for each step and feeding back the
/// raises it reports. Returns the number of steps run (at most `1 + M`).
pub fn dispatch<const M: usize>(
    queue: RaiseQueue<M>,
    machine: usize,
    event: usize,
    mut run: impl FnMut(usize, usize) -> Raises,
) -> (RaiseQueue<M>, usize) {
    let (mut q, r) = queue.raise(machine, event);
    if r != Raised::Step {
        return (q, 0);
    }
    let mut steps = 1;
    for (m, e) in run(machine, event).into_iter().flatten() {
        q = q.raise(m, e).0;
    }
    let (mut q, pending) = q.end_step();
    for (m, e) in pending.into_iter().enumerate() {
        let Some(e) = e else { continue };
        steps += 1;
        for (m2, e2) in run(m, e).into_iter().flatten() {
            q = q.raise(m2, e2).0;
        }
    }
    (q.end_drain(), steps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Action, CmpOp, Event, Guard, Node, Row, State};
    use std::vec::Vec;

    const STATES: [State<'static>; 3] = [
        State {
            name: "Off",
            entry: &[],
        },
        State {
            name: "On",
            entry: &[],
        },
        State {
            name: "Locked",
            entry: &[],
        },
    ];
    const EVENTS: [Event<'static>; 2] = [Event { name: "PRESS" }, Event { name: "LOCK" }];
    const DOOR_OPEN: [Node<'static>; 3] = [
        Node::Pin("DOOR"),
        Node::Const(1),
        Node::Cmp {
            op: CmpOp::Eq,
            a: 0,
            b: 1,
        },
    ];
    const BUTTON: [Node<'static>; 1] = [Node::Pin("BUTTON")];
    const NOTHING: [Action<'static>; 0] = [];

    fn row<'a>(
        state: &'a str,
        event: &'a str,
        guard: Option<&'a [Node<'a>]>,
        next: &'a str,
    ) -> Row<'a> {
        Row {
            state,
            event,
            guard: guard.map(|nodes| Guard { nodes }),
            next,
            actions: &NOTHING,
        }
    }

    fn machine<'a>(rows: &'a [Row<'a>]) -> Machine<'a> {
        Machine {
            id: "Mode",
            initial: "Off",
            states: &STATES,
            events: &EVENTS,
            rows,
        }
    }

    #[test]
    fn step_fires_the_first_true_guarded_row_before_the_default() {
        let rows = [
            row("Off", "PRESS", None, "On"),
            row("Off", "PRESS", Some(&DOOR_OPEN), "Locked"),
            row("Off", "PRESS", Some(&BUTTON), "Off"),
        ];
        let m = machine(&rows);
        let read = |door: bool, button: bool| {
            move |l: Leaf<'_>| match l {
                Leaf::Pin("DOOR") => door,
                Leaf::Pin("BUTTON") => button,
                _ => false,
            }
        };
        assert_eq!(
            step(&m, read(true, true), 0, 0),
            Some(Fired { row: 1, next: 2 })
        );
        assert_eq!(
            step(&m, read(false, true), 0, 0),
            Some(Fired { row: 2, next: 0 })
        );
        assert_eq!(
            step(&m, read(false, false), 0, 0),
            Some(Fired { row: 0, next: 1 })
        );
    }

    #[test]
    fn step_without_a_matching_row_fires_nothing() {
        let rows = [
            row("Off", "PRESS", Some(&DOOR_OPEN), "On"),
            row("On", "PRESS", None, "Off"),
        ];
        let m = machine(&rows);
        assert_eq!(step(&m, |_| false, 0, 0), None);
        assert_eq!(step(&m, |_| false, 0, 1), None);
        assert_eq!(step(&m, |_| false, 2, 0), None);
        assert_eq!(step(&m, |_| false, 1, 0), Some(Fired { row: 1, next: 0 }));
        assert_eq!(step(&m, |_| false, 9, 9), None);
    }

    #[test]
    fn step_uses_the_first_unguarded_row_and_skips_rows_naming_nothing() {
        let rows = [
            row("Off", "PRESS", None, "Nowhere"),
            row("Off", "PRESS", None, "On"),
            row("Off", "PRESS", None, "Locked"),
            row("Off", "PRESS", Some(&[Node::Not(0)]), "Locked"),
        ];
        let m = machine(&rows);
        assert_eq!(step(&m, |_| true, 0, 0), Some(Fired { row: 1, next: 1 }));
    }

    #[test]
    fn step_self_transition_reports_the_same_state() {
        let rows = [row("On", "PRESS", None, "On")];
        assert_eq!(
            step(&machine(&rows), |_| false, 1, 0),
            Some(Fired { row: 0, next: 1 })
        );
    }

    #[test]
    fn raise_queue_holds_one_pending_event_per_machine() {
        let q = RaiseQueue::<2>::new();
        let (q, r) = q.raise(0, 5);
        assert_eq!(r, Raised::Step);
        let (q, r) = q.raise(1, 1);
        assert_eq!(r, Raised::Queued);
        let (q, r) = q.raise(1, 2);
        assert_eq!(r, Raised::Dropped);
        let (q, r) = q.raise(7, 0);
        assert_eq!(r, Raised::Dropped);
        let (q, r) = q.raise(0, 3);
        assert_eq!(r, Raised::Queued);
        let (q, pending) = q.end_step();
        assert_eq!(pending, [Some(3), Some(1)]);
        let (q, r) = q.raise(0, 4);
        assert_eq!(r, Raised::Dropped, "a raise during the drain is dropped");
        let q = q.end_drain();
        assert_eq!(q, RaiseQueue::new());
        assert_eq!(RaiseQueue::<1>::new().raise(3, 0).1, Raised::Dropped);
        assert_eq!(RaiseQueue::<1>::new().end_step().1, [None]);
    }

    #[test]
    fn dispatch_drains_pending_raises_once_in_machine_order() {
        let mut log = Vec::new();
        let (q, steps) = dispatch(RaiseQueue::<3>::new(), 1, 0, |m, e| {
            log.push((m, e));
            let mut out: Raises = [None; MAX_RAISES_PER_STEP];
            if (m, e) == (1, 0) {
                out[0] = Some((2, 7));
                out[1] = Some((0, 8));
                out[2] = Some((0, 9));
            } else {
                out[0] = Some((1, 1));
            }
            out
        });
        assert_eq!(log, [(1, 0), (0, 8), (2, 7)]);
        assert_eq!(steps, 3);
        assert_eq!(q, RaiseQueue::new());
    }

    /// Exhaustive over every reaction table for three machines (each
    /// machine's step raises an event on any subset of the machines, each
    /// twice, and on an out-of-range machine): one external raise runs at
    /// most 1 + machines steps, and that bound is reached.
    #[test]
    fn dispatch_runs_at_most_one_plus_machines_steps() {
        const M: usize = 3;
        let mut worst = 0;
        for table in 0u32..(1 << (M * M)) {
            for start in 0..M {
                let (q, steps) = dispatch(RaiseQueue::<M>::new(), start, 0, |m, _| {
                    let mut out: Raises = [None; MAX_RAISES_PER_STEP];
                    let targets = (table >> (m * M)) & 0b111;
                    let mut k = 0;
                    for t in 0..M {
                        if targets & (1 << t) != 0 {
                            out[k] = Some((t, 1));
                            out[k + 1] = Some((t, 2));
                            k += 2;
                        }
                    }
                    out[k] = Some((M + 1, 0));
                    out
                });
                assert!(
                    steps <= 1 + M,
                    "table {table:b} from {start}: {steps} steps"
                );
                assert_eq!(q, RaiseQueue::new());
                worst = worst.max(steps);
            }
        }
        assert_eq!(worst, 1 + M, "the bound is reached");
    }

    #[test]
    fn dispatch_while_stepping_queues_instead_of_running() {
        let (q, _) = RaiseQueue::<1>::new().raise(0, 0);
        let (q, steps) = dispatch(q, 0, 1, |_, _| [None; MAX_RAISES_PER_STEP]);
        assert_eq!(steps, 0);
        assert_eq!(q.end_step().1, [Some(1)]);
    }
}
