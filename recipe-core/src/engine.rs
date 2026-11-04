//! The control-recipe engine: a compiled master recipe ([`Sfc`]) bound to
//! units ([`Allocation`]) and executed by commanding equipment phases.
//!
//! The engine is a pure value. [`Engine::on`] takes it by value with the
//! current time and one [`Input`] and returns the next engine and the
//! [`Effects`] to carry out; the imperative shell sends each
//! [`Effect::Send`] as a request (see [`crate::wire`]), feeds back the
//! reply as [`Input::Ack`], every state report as [`Input::Report`], the
//! discovery directory's changes as [`Input::UnitLost`] /
//! [`Input::UnitFound`], operator buttons as [`Input::Operator`], and the
//! clock as [`Input::Tick`].
//!
//! # The procedure's own state
//!
//! The recipe procedure follows the same [`phase`] state model as its
//! phases: Start, Hold, Restart, Stop, Abort and Reset from the operator
//! move it by the same table, and the engine raises Done when the phases
//! have caught up (Holding → Held once every involved phase is held, and
//! so on). A Restart while a unit with unfinished steps is offline, or an
//! involved phase's state is unknown, is refused.
//!
//! # Steps
//!
//! A step is inactive, active or done. Start activates the initial steps;
//! a transition whose `from` steps are all done fires once, activating its
//! `to` steps. An active step's phase gets Start (with the step's
//! arguments) once the phase is Idle; it is *started* when that command is
//! acknowledged, and done when its phase reports Complete. A done step's
//! phase is then Reset to release it. The recipe is Complete when every
//! step is done and every phase is back to Idle (or unreachable).
//!
//! # Commands
//!
//! Commands are level-triggered: after every input the engine compares
//! each phase's last known state with what the procedure's state asks for
//! and sends at most one command per phase, only when none is outstanding,
//! the unit is online, the state is known and the phase table
//! ([`phase::accepts`]) allows it:
//!
//! | procedure  | phase state                             | command  |
//! |------------|-----------------------------------------|----------|
//! | any acting | Complete                                | Reset    |
//! | Running    | Idle, its step active and not started   | Start    |
//! | Holding, Held | Running, Restarting                  | Hold     |
//! | Restarting | Held                                    | Restart  |
//! | Stopping   | Running, Holding, Held, Restarting      | Stop     |
//! | Aborting   | Running, Holding, Held, Restarting, Stopping | Abort |
//! | Resetting  | Stopped, Aborted                        | Reset    |
//!
//! (Hold, Restart, Stop and Abort go only to the phases of *involved*
//! steps: active and started, or with a Start outstanding.) A command is
//! acknowledged by its reply ([`Input::Ack`]) or by a state report naming
//! its id. Each phase gets at most [`Config::max_commands`] commands per
//! run.
//!
//! # Failures
//!
//! A failure is **resumable** when the phase can still be held and
//! restarted: no acknowledgement within [`Config::ack_timeout_ms`], a
//! transient state that does not settle within
//! [`Config::settle_timeout_ms`], a reply of `Unavailable`,
//! `DeadlineExceeded` or a transport error, a phase that holds itself, a
//! unit with unfinished steps leaving the directory. It is
//! **unresumable** when it cannot: a reply of `NotFound` or
//! `InvalidRequest`, a started phase that is found Stopping, Stopped,
//! Aborting, Aborted, Resetting or Idle without being told to, or a phase
//! exceeding its command budget. A resumable failure while the procedure
//! is Running or Restarting holds it; an unresumable one while it is
//! Running, Restarting, Holding or Held aborts it; otherwise a failure is
//! only reported. A timed-out or lost phase becomes *unreachable* (state
//! unknown): it no longer blocks Holding, Stopping or Aborting from
//! finishing, but it blocks Restart until a report makes it known again.
//!
//! # Bounds
//!
//! One input runs at most [`MAX_PASSES`] passes; each transition fires at
//! most once per run and each step runs at most once. Reports older than
//! the last applied command are ignored (ids are increasing). Every
//! waiting state is ended by a report, an acknowledgement, a deadline or
//! the operator, so after Abort and two timeouts every run is Aborted (the
//! termination property the tests check).

use crate::alloc::{Allocation, PhaseRef};
use crate::phase::{self, Command, PhaseState};
use crate::recipe::Sfc;
use crate::{MAX_STEPS, MAX_UNITS, StepMask, bit};

/// Most effects one input produces before [`Effects::overflow`] counts.
pub const MAX_EFFECTS: usize = 96;
/// Most settle passes per input.
pub const MAX_PASSES: usize = MAX_STEPS + crate::MAX_TRANSITIONS + 8;

/// Timing and budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// How long a command may wait for its acknowledgement (also the
    /// request's timeout, which transports carry as
    /// `RequestMeta::deadline_ms`).
    pub ack_timeout_ms: u32,
    /// How long a phase may stay in a transient state (Holding,
    /// Restarting, Stopping, Aborting, Resetting).
    pub settle_timeout_ms: u32,
    /// Most commands sent to one phase per run.
    pub max_commands: u8,
}

impl Config {
    /// 1 s acknowledgements, 5 s settling, 16 commands per phase.
    pub const DEFAULT: Config = Config {
        ack_timeout_ms: 1_000,
        settle_timeout_ms: 5_000,
        max_commands: 16,
    };
}

/// A phase's reply to a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ack {
    /// Applied; the phase is now in this state.
    Accepted(PhaseState),
    /// Not valid in the phase's current state, which is this.
    Rejected(PhaseState),
    /// The request failed.
    Failed(AckError),
}

/// Why a command's request failed (the middleware `HandlerError`s a unit
/// may answer, plus the transport's own failure).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckError {
    /// The unit has no such phase.
    NotFound,
    /// The body was malformed or an argument out of the phase's range.
    InvalidRequest,
    /// The unit cannot serve now.
    Unavailable,
    /// The request's deadline passed at the unit.
    DeadlineExceeded,
    /// The transport failed (no route, link down, ...).
    Transport,
}

/// One input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    /// An operator command for the procedure.
    Operator(Command),
    /// The reply to command `id`.
    Ack {
        /// The command's id ([`Effect::Send`]).
        id: u32,
        /// The reply.
        result: Ack,
    },
    /// A phase published its state; `last_id` is the last command it
    /// applied (0 for none).
    Report {
        /// The phase.
        phase: PhaseRef,
        /// Its state.
        state: PhaseState,
        /// The id of the last command it applied.
        last_id: u32,
    },
    /// The directory lost this unit (goodbye or expiry).
    UnitLost(u8),
    /// The directory sees this unit again.
    UnitFound(u8),
    /// Time passed.
    Tick,
}

/// Why an operator command was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The procedure's state does not accept it.
    InvalidInState(PhaseState),
    /// A unit with unfinished steps is offline.
    UnitOffline(u8),
    /// An involved phase's state is unknown.
    PhaseUnknown(PhaseRef),
}

/// A failure the engine noticed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// No acknowledgement in time (resumable).
    AckTimeout,
    /// A transient state did not settle in time (resumable).
    SettleTimeout,
    /// The request failed (resumable unless `NotFound` or
    /// `InvalidRequest`).
    Request(AckError),
    /// The phase rejected a command; its state is now known (no policy).
    CommandRejected(PhaseState),
    /// A started phase held itself (resumable).
    SelfHeld,
    /// A started phase is in this state without being told (unresumable).
    Unexpected(PhaseState),
    /// A unit left the directory (resumable when it had unfinished steps).
    UnitLost(u8),
    /// A phase used up its command budget (unresumable).
    Budget,
}

impl Failure {
    /// What the policy does with it: `Some(true)` resumable, `Some(false)`
    /// unresumable, `None` report only.
    pub const fn resumable(self) -> Option<bool> {
        match self {
            Failure::AckTimeout
            | Failure::SettleTimeout
            | Failure::SelfHeld
            | Failure::UnitLost(_)
            | Failure::Request(
                AckError::Unavailable | AckError::DeadlineExceeded | AckError::Transport,
            ) => Some(true),
            Failure::Request(AckError::NotFound | AckError::InvalidRequest)
            | Failure::Unexpected(_)
            | Failure::Budget => Some(false),
            Failure::CommandRejected(_) => None,
        }
    }
}

/// Why an input was ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ignored {
    /// A report older than the last applied command.
    StaleReport,
    /// A report for a phase the recipe does not use.
    UnknownPhase,
    /// A reply for no outstanding command.
    UnknownAck,
    /// A report from a unit the directory does not see.
    UnitOffline,
}

/// What the shell should do or log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Send `command` to `phase` (for `step`, whose arguments a Start
    /// carries) as request `id` with this timeout.
    Send {
        /// The step it is for.
        step: u8,
        /// The phase.
        phase: PhaseRef,
        /// The command.
        command: Command,
        /// Correlation id (increasing per engine).
        id: u32,
        /// The request's timeout.
        timeout_ms: u32,
    },
    /// The procedure changed state.
    Recipe {
        /// Before.
        from: PhaseState,
        /// After.
        to: PhaseState,
    },
    /// A step became active.
    StepActive(u8),
    /// A step is done.
    StepDone(u8),
    /// An operator command was refused.
    Refused {
        /// The command.
        command: Command,
        /// Why.
        reason: Refusal,
    },
    /// A failure (and, by [`Failure::resumable`], what the policy did).
    Failed {
        /// The phase, when the failure is about one.
        phase: Option<PhaseRef>,
        /// What.
        failure: Failure,
    },
    /// An input changed nothing.
    Ignored(Ignored),
}

/// The effects of one input, in order. Fixed capacity; anything past
/// [`MAX_EFFECTS`] is counted in `overflow` (the tests check it stays 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Effects {
    items: [Option<Effect>; MAX_EFFECTS],
    len: usize,
    /// Effects that did not fit.
    pub overflow: u16,
}

impl Effects {
    const EMPTY: Effects = Effects {
        items: [None; MAX_EFFECTS],
        len: 0,
        overflow: 0,
    };

    fn push(&mut self, e: Effect) {
        match self.items.get_mut(self.len) {
            Some(slot) => {
                *slot = Some(e);
                self.len += 1;
            }
            None => self.overflow = self.overflow.saturating_add(1),
        }
    }

    /// The effects in order.
    pub fn iter(&self) -> impl Iterator<Item = &Effect> {
        self.items.iter().take(self.len).flatten()
    }

    /// Number of effects held.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// True when there are none.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// A step's progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    /// Not yet reached.
    Inactive,
    /// Reached; `started` once its Start was acknowledged.
    Active {
        /// Start acknowledged.
        started: bool,
    },
    /// Its phase completed.
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pending {
    command: Command,
    id: u32,
    deadline: u64,
}

/// What the engine knows about one equipment phase the recipe uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Slot {
    phase: PhaseRef,
    known: Option<PhaseState>,
    last_id: u32,
    pending: Option<Pending>,
    settle_deadline: Option<u64>,
    sent: u8,
}

const NO_SLOT: Slot = Slot {
    phase: PhaseRef { unit: 0, phase: 0 },
    known: None,
    last_id: 0,
    pending: None,
    settle_deadline: None,
    sent: 0,
};

/// Why [`Engine::new`] failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineError {
    /// More than [`MAX_UNITS`] units, or a step bound to a unit index not
    /// below `units`.
    BadAllocation,
}

/// The control-recipe engine (module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Engine {
    sfc: Sfc,
    cfg: Config,
    slot_of: [u8; MAX_STEPS],
    slots: [Slot; MAX_STEPS],
    n_slots: usize,
    online: u32,
    recipe: PhaseState,
    steps: [StepStatus; MAX_STEPS],
    fired: u32,
    now: u64,
    next_id: u32,
}

impl Engine {
    /// An Idle engine for `sfc` bound by `alloc` over `units` units, all
    /// online, every bound phase assumed Idle (allocation's precondition).
    pub fn new(
        sfc: Sfc,
        alloc: &Allocation,
        units: usize,
        cfg: Config,
    ) -> Result<Engine, EngineError> {
        if units > MAX_UNITS {
            return Err(EngineError::BadAllocation);
        }
        let mut e = Engine {
            sfc,
            cfg,
            slot_of: [0; MAX_STEPS],
            slots: [NO_SLOT; MAX_STEPS],
            n_slots: 0,
            online: (1u32 << units) - 1,
            recipe: PhaseState::Idle,
            steps: [StepStatus::Inactive; MAX_STEPS],
            fired: 0,
            now: 0,
            next_id: 1,
        };
        for s in 0..sfc.steps {
            let p = alloc.step_phase[s];
            if usize::from(p.unit) >= units {
                return Err(EngineError::BadAllocation);
            }
            let found = e.slots[..e.n_slots].iter().position(|x| x.phase == p);
            let i = match found {
                Some(i) => i,
                None => {
                    let i = e.n_slots;
                    e.slots[i] = Slot {
                        phase: p,
                        known: Some(PhaseState::Idle),
                        ..NO_SLOT
                    };
                    e.n_slots += 1;
                    i
                }
            };
            e.slot_of[s] = i as u8;
        }
        Ok(e)
    }

    /// The procedure's state.
    pub const fn state(&self) -> PhaseState {
        self.recipe
    }

    /// Step `i`'s progress (Inactive past the last step).
    pub fn step(&self, i: usize) -> StepStatus {
        self.steps.get(i).copied().unwrap_or(StepStatus::Inactive)
    }

    /// The last known state of `phase`, `None` when unknown or unused.
    pub fn phase_state(&self, phase: PhaseRef) -> Option<PhaseState> {
        self.slots[..self.n_slots]
            .iter()
            .find(|s| s.phase == phase)
            .and_then(|s| s.known)
    }

    /// The time of the last input.
    pub const fn now(&self) -> u64 {
        self.now
    }

    /// True while a command to some phase awaits its acknowledgement.
    pub fn awaiting(&self) -> bool {
        self.slots[..self.n_slots]
            .iter()
            .any(|s| s.pending.is_some())
    }

    /// The earliest deadline after which a [`Input::Tick`] changes
    /// something, if any.
    pub fn next_deadline(&self) -> Option<u64> {
        self.slots[..self.n_slots]
            .iter()
            .flat_map(|s| [s.pending.map(|p| p.deadline), s.settle_deadline])
            .flatten()
            .min()
    }

    /// Applies `input` at `now_ms` (never earlier than the last input's
    /// time) and returns the next engine and its effects.
    pub fn on(self, now_ms: u64, input: Input) -> (Engine, Effects) {
        let mut e = self;
        let mut fx = Effects::EMPTY;
        e.now = e.now.max(now_ms);
        e.apply(input, &mut fx);
        e.expire(&mut fx);
        // Issuing can fail a phase (its budget), which moves the procedure;
        // settle once more so that move is followed through.
        for _ in 0..2 {
            let before = e.recipe;
            e.settle(&mut fx);
            e.issue(&mut fx);
            if e.recipe == before {
                break;
            }
        }
        (e, fx)
    }

    fn online(&self, unit: u8) -> bool {
        self.online & (1u32 << unit) != 0
    }

    fn slot_index(&self, phase: PhaseRef) -> Option<usize> {
        self.slots[..self.n_slots]
            .iter()
            .position(|s| s.phase == phase)
    }

    fn move_recipe(&mut self, input: phase::Input, fx: &mut Effects) -> bool {
        let from = self.recipe;
        let (to, effect) = phase::step(from, input);
        if effect == phase::Effect::Rejected {
            return false;
        }
        self.recipe = to;
        fx.push(Effect::Recipe { from, to });
        match (from, to) {
            (PhaseState::Idle, PhaseState::Running) => self.begin(fx),
            (PhaseState::Resetting, PhaseState::Idle) => {
                self.steps = [StepStatus::Inactive; MAX_STEPS];
                self.fired = 0;
            }
            _ => {}
        }
        true
    }

    fn begin(&mut self, fx: &mut Effects) {
        self.steps = [StepStatus::Inactive; MAX_STEPS];
        self.fired = 0;
        for slot in self.slots[..self.n_slots].iter_mut() {
            slot.sent = 0;
        }
        for s in 0..self.sfc.steps {
            if self.sfc.initial & bit(s) != 0 {
                self.steps[s] = StepStatus::Active { started: false };
                fx.push(Effect::StepActive(s as u8));
            }
        }
    }

    fn fail(&mut self, phase: Option<PhaseRef>, failure: Failure, fx: &mut Effects) {
        fx.push(Effect::Failed { phase, failure });
        let command = match (self.recipe, failure.resumable()) {
            (PhaseState::Running | PhaseState::Restarting, Some(true)) => Command::Hold,
            (
                PhaseState::Running
                | PhaseState::Restarting
                | PhaseState::Holding
                | PhaseState::Held,
                Some(false),
            ) => Command::Abort,
            _ => return,
        };
        self.move_recipe(phase::Input::Command(command), fx);
    }

    fn operator(&mut self, command: Command, fx: &mut Effects) {
        if command == Command::Restart && self.recipe == PhaseState::Held {
            for s in 0..self.sfc.steps {
                if self.steps[s] == StepStatus::Done {
                    continue;
                }
                let slot = &self.slots[usize::from(self.slot_of[s])];
                if !self.online(slot.phase.unit) {
                    let reason = Refusal::UnitOffline(slot.phase.unit);
                    fx.push(Effect::Refused { command, reason });
                    return;
                }
                if self.involved(s) && slot.known.is_none() {
                    let reason = Refusal::PhaseUnknown(slot.phase);
                    fx.push(Effect::Refused { command, reason });
                    return;
                }
            }
        }
        if !self.move_recipe(phase::Input::Command(command), fx) {
            let reason = Refusal::InvalidInState(self.recipe);
            fx.push(Effect::Refused { command, reason });
        }
    }

    /// Records that `command` took effect on slot `i`.
    fn applied(&mut self, i: usize, command: Command) {
        if command != Command::Start {
            return;
        }
        for s in 0..self.sfc.steps {
            if usize::from(self.slot_of[s]) == i
                && self.steps[s] == (StepStatus::Active { started: false })
            {
                self.steps[s] = StepStatus::Active { started: true };
                return;
            }
        }
    }

    fn set_known(&mut self, i: usize, state: PhaseState) {
        let now = self.now;
        let settle = u64::from(self.cfg.settle_timeout_ms);
        let slot = &mut self.slots[i];
        let changed = slot.known != Some(state);
        slot.known = Some(state);
        if !state.is_transient() {
            slot.settle_deadline = None;
        } else if changed || slot.settle_deadline.is_none() {
            slot.settle_deadline = Some(now + settle);
        }
    }

    fn apply(&mut self, input: Input, fx: &mut Effects) {
        match input {
            Input::Operator(c) => self.operator(c, fx),
            Input::Ack { id, result } => {
                let found = self.slots[..self.n_slots]
                    .iter()
                    .position(|s| s.pending.is_some_and(|p| p.id == id));
                let Some(i) = found else {
                    fx.push(Effect::Ignored(Ignored::UnknownAck));
                    return;
                };
                let command = self.slots[i].pending.map(|p| p.command);
                self.slots[i].pending = None;
                let phase = self.slots[i].phase;
                match result {
                    Ack::Accepted(state) => {
                        self.slots[i].last_id = id;
                        self.set_known(i, state);
                        if let Some(c) = command {
                            self.applied(i, c);
                        }
                    }
                    Ack::Rejected(state) => {
                        self.slots[i].last_id = id;
                        self.set_known(i, state);
                        self.fail(Some(phase), Failure::CommandRejected(state), fx);
                    }
                    Ack::Failed(err) => {
                        self.slots[i].known = None;
                        self.slots[i].settle_deadline = None;
                        self.fail(Some(phase), Failure::Request(err), fx);
                    }
                }
            }
            Input::Report {
                phase,
                state,
                last_id,
            } => {
                let Some(i) = self.slot_index(phase) else {
                    fx.push(Effect::Ignored(Ignored::UnknownPhase));
                    return;
                };
                if !self.online(phase.unit) {
                    fx.push(Effect::Ignored(Ignored::UnitOffline));
                    return;
                }
                if last_id < self.slots[i].last_id {
                    fx.push(Effect::Ignored(Ignored::StaleReport));
                    return;
                }
                self.slots[i].last_id = last_id;
                if let Some(p) = self.slots[i].pending
                    && p.id == last_id
                {
                    self.slots[i].pending = None;
                    self.applied(i, p.command);
                }
                self.set_known(i, state);
            }
            Input::UnitLost(unit) => {
                if usize::from(unit) >= MAX_UNITS || !self.online(unit) {
                    fx.push(Effect::Ignored(Ignored::UnitOffline));
                    return;
                }
                self.online &= !(1u32 << unit);
                for slot in self.slots[..self.n_slots].iter_mut() {
                    if slot.phase.unit == unit {
                        slot.known = None;
                        slot.pending = None;
                        slot.settle_deadline = None;
                    }
                }
                let unfinished = (0..self.sfc.steps).any(|s| {
                    self.slots[usize::from(self.slot_of[s])].phase.unit == unit
                        && self.steps[s] != StepStatus::Done
                });
                let failure = Failure::UnitLost(unit);
                if unfinished && self.recipe != PhaseState::Idle {
                    self.fail(None, failure, fx);
                } else {
                    fx.push(Effect::Failed {
                        phase: None,
                        failure,
                    });
                }
            }
            Input::UnitFound(unit) => {
                if usize::from(unit) < MAX_UNITS {
                    self.online |= 1u32 << unit;
                }
            }
            Input::Tick => {}
        }
    }

    fn expire(&mut self, fx: &mut Effects) {
        for i in 0..self.n_slots {
            let phase = self.slots[i].phase;
            if self.slots[i]
                .pending
                .is_some_and(|p| p.deadline <= self.now)
            {
                self.slots[i].pending = None;
                self.slots[i].known = None;
                self.slots[i].settle_deadline = None;
                self.fail(Some(phase), Failure::AckTimeout, fx);
            }
            if self.slots[i].settle_deadline.is_some_and(|d| d <= self.now) {
                self.slots[i].settle_deadline = None;
                if self.slots[i].known.is_some_and(PhaseState::is_transient) {
                    self.slots[i].known = None;
                    self.fail(Some(phase), Failure::SettleTimeout, fx);
                }
            }
        }
    }

    /// Active and started, or with a Start outstanding.
    fn involved(&self, s: usize) -> bool {
        let slot = &self.slots[usize::from(self.slot_of[s])];
        match self.steps[s] {
            StepStatus::Active { started: true } => true,
            StepStatus::Active { started: false } => {
                slot.pending.is_some_and(|p| p.command == Command::Start)
            }
            _ => false,
        }
    }

    /// Every involved step's phase is unreachable or idle-quiet in a state
    /// `ok` accepts.
    fn involved_quiet(&self, ok: fn(PhaseState) -> bool) -> bool {
        (0..self.sfc.steps).filter(|&s| self.involved(s)).all(|s| {
            let slot = &self.slots[usize::from(self.slot_of[s])];
            match slot.known {
                None => true,
                Some(k) => slot.pending.is_none() && ok(k),
            }
        })
    }

    fn all_released(&self) -> bool {
        self.slots[..self.n_slots].iter().all(|s| match s.known {
            None => true,
            Some(k) => s.pending.is_none() && k == PhaseState::Idle,
        })
    }

    fn caught_up(&self) -> bool {
        use PhaseState::*;
        match self.recipe {
            Running => {
                (0..self.sfc.steps).all(|s| self.steps[s] == StepStatus::Done)
                    && self.all_released()
            }
            Holding => self.involved_quiet(|k| !matches!(k, Running | Restarting | Holding)),
            Restarting => (0..self.sfc.steps).filter(|&s| self.involved(s)).all(|s| {
                let slot = &self.slots[usize::from(self.slot_of[s])];
                slot.pending.is_none() && matches!(slot.known, Some(Running | Complete))
            }),
            Stopping => self.involved_quiet(|k| matches!(k, Stopped | Aborted | Complete | Idle)),
            Aborting => self.involved_quiet(|k| matches!(k, Aborted | Stopped | Complete | Idle)),
            Resetting => self.all_released(),
            Idle | Complete | Held | Stopped | Aborted => false,
        }
    }

    /// A started phase in a state the procedure did not ask for.
    fn surprise(&self, s: usize) -> Option<Failure> {
        use PhaseState::*;
        if self.steps[s] != (StepStatus::Active { started: true }) {
            return None;
        }
        let slot = &self.slots[usize::from(self.slot_of[s])];
        let k = slot.known?;
        if slot.pending.is_some() {
            return None;
        }
        let unresumable = matches!(
            k,
            Stopping | Stopped | Aborting | Aborted | Resetting | Idle
        );
        match self.recipe {
            Running | Restarting
                if matches!(k, Holding | Held) && !(self.recipe == Restarting && k == Held) =>
            {
                Some(Failure::SelfHeld)
            }
            Running | Restarting | Holding | Held if unresumable => Some(Failure::Unexpected(k)),
            _ => None,
        }
    }

    fn settle(&mut self, fx: &mut Effects) {
        for _ in 0..MAX_PASSES {
            let mut changed = false;
            for s in 0..self.sfc.steps {
                let slot = &self.slots[usize::from(self.slot_of[s])];
                if self.steps[s] == (StepStatus::Active { started: true })
                    && slot.pending.is_none()
                    && slot.known == Some(PhaseState::Complete)
                {
                    self.steps[s] = StepStatus::Done;
                    fx.push(Effect::StepDone(s as u8));
                    changed = true;
                }
            }
            for s in 0..self.sfc.steps {
                if let Some(f) = self.surprise(s) {
                    let before = self.recipe;
                    let phase = self.slots[usize::from(self.slot_of[s])].phase;
                    self.fail(Some(phase), f, fx);
                    changed |= self.recipe != before;
                }
            }
            if self.recipe == PhaseState::Running {
                let done: StepMask = (0..self.sfc.steps)
                    .filter(|&s| self.steps[s] == StepStatus::Done)
                    .fold(0, |m, s| m | bit(s));
                for t in 0..self.sfc.transitions {
                    if self.fired & bit(t) == 0 && self.sfc.from[t] & !done == 0 {
                        self.fired |= bit(t);
                        changed = true;
                        for s in 0..self.sfc.steps {
                            if self.sfc.to[t] & bit(s) != 0 {
                                self.steps[s] = StepStatus::Active { started: false };
                                fx.push(Effect::StepActive(s as u8));
                            }
                        }
                    }
                }
            }
            if self.caught_up() {
                changed |= self.move_recipe(phase::Input::Done, fx);
            }
            if !changed {
                break;
            }
        }
    }

    /// The command slot `i` should get now, if any (module docs' table).
    fn desired(&self, i: usize) -> Option<(usize, Command)> {
        use PhaseState::*;
        let slot = &self.slots[i];
        let k = slot.known?;
        let owner = (0..self.sfc.steps).find(|&s| {
            usize::from(self.slot_of[s]) == i && matches!(self.steps[s], StepStatus::Active { .. })
        });
        let any_step = (0..self.sfc.steps).find(|&s| usize::from(self.slot_of[s]) == i)?;
        let involved = owner.is_some_and(|s| self.involved(s));
        let command = match (self.recipe, k) {
            (Idle | Complete | Stopped | Aborted, _) => return None,
            (Held, Complete) => return None,
            (_, Complete) => Command::Reset,
            (Running, Idle)
                if owner
                    .is_some_and(|s| self.steps[s] == (StepStatus::Active { started: false })) =>
            {
                Command::Start
            }
            (Holding | Held, Running | Restarting) if involved => Command::Hold,
            (Restarting, Held) if involved => Command::Restart,
            (Stopping, Running | Holding | Held | Restarting) if involved => Command::Stop,
            (Aborting, Running | Holding | Held | Restarting | Stopping) if involved => {
                Command::Abort
            }
            (Resetting, Stopped | Aborted) => Command::Reset,
            _ => return None,
        };
        Some((owner.unwrap_or(any_step), command))
    }

    fn issue(&mut self, fx: &mut Effects) {
        for i in 0..self.n_slots {
            let slot = self.slots[i];
            if slot.pending.is_some() || !self.online(slot.phase.unit) {
                continue;
            }
            let Some((step, command)) = self.desired(i) else {
                continue;
            };
            let Some(k) = slot.known else { continue };
            if !phase::accepts(k, command) {
                continue;
            }
            if slot.sent >= self.cfg.max_commands {
                self.slots[i].known = None;
                self.fail(Some(slot.phase), Failure::Budget, fx);
                continue;
            }
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1).max(1);
            self.slots[i].pending = Some(Pending {
                command,
                id,
                deadline: self.now + u64::from(self.cfg.ack_timeout_ms),
            });
            self.slots[i].sent += 1;
            fx.push(Effect::Send {
                step: step as u8,
                phase: slot.phase,
                command,
                id,
                timeout_ms: self.cfg.ack_timeout_ms,
            });
        }
    }
}

#[cfg(test)]
mod tests;
