//! The S88 phase state machine: the states every equipment phase (and the
//! recipe procedure itself) moves through, the commands that move it, and
//! the one internal event, [`Input::Done`], by which an acting state's
//! logic reports that it has finished.
//!
//! The states and commands follow the example procedural-element state
//! model of ISA-88.00.01 (IEC 61512-1) §5.2 / Figure 18, without its
//! optional Pausing / Paused pair (a phase here has no breakpoints; Hold
//! covers an operator stop-and-resume). **Resetting** comes from the PackML
//! state model (ISA-TR88.00.02): ISA-88 resets straight to Idle, which
//! gives the phase no state in which to put its equipment back (an LED off,
//! a valve closed), so Reset here enters Resetting and the phase reports
//! Done when it is back to Idle.
//!
//! Every state is either **acting** (`-ing`: its logic is running and ends
//! with [`Input::Done`]) or **quiescent** (waiting for a command). Complete,
//! Stopped and Aborted are the **final** states of a run; Reset from any of
//! them starts the next one.
//!
//! | state      | Start   | Hold    | Restart    | Stop     | Abort    | Reset     | Done      |
//! |------------|---------|---------|------------|----------|----------|-----------|-----------|
//! | Idle       | Running |         |            |          |          |           |           |
//! | Running    |         | Holding |            | Stopping | Aborting |           | Complete  |
//! | Complete   |         |         |            |          |          | Resetting |           |
//! | Holding    |         |         |            | Stopping | Aborting |           | Held      |
//! | Held       |         |         | Restarting | Stopping | Aborting |           |           |
//! | Restarting |         | Holding |            | Stopping | Aborting |           | Running   |
//! | Stopping   |         |         |            |          | Aborting |           | Stopped   |
//! | Stopped    |         |         |            |          |          | Resetting |           |
//! | Aborting   |         |         |            |          |          |           | Aborted   |
//! | Aborted    |         |         |            |          |          | Resetting |           |
//! | Resetting  |         |         |            | Stopping | Aborting |           | Idle      |
//!
//! An empty cell is [`Effect::Rejected`]: the state does not change and the
//! command's sender is told so (on the wire, an acknowledgement with the
//! current state; see [`crate::wire`]). Abort is accepted in every state
//! that is not Idle, final or already Aborting; Stop in every state that is
//! still doing something it could stop.

/// Number of states.
pub const STATES: usize = 11;
/// Number of commands.
pub const COMMANDS: usize = 6;

/// A phase's (or the recipe procedure's) state. The discriminant is the
/// wire code ([`PhaseState::code`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PhaseState {
    /// Ready for Start.
    Idle = 0,
    /// Executing its normal logic.
    Running = 1,
    /// Its normal logic finished (final).
    Complete = 2,
    /// Executing its hold logic.
    Holding = 3,
    /// Held: paused in a safe state, waiting for Restart, Stop or Abort.
    Held = 4,
    /// Executing its restart logic, on its way back to Running.
    Restarting = 5,
    /// Executing its stop logic (an orderly end).
    Stopping = 6,
    /// Stopped (final).
    Stopped = 7,
    /// Executing its abort logic (a quick end).
    Aborting = 8,
    /// Aborted (final).
    Aborted = 9,
    /// Executing its reset logic, on its way back to Idle (PackML).
    Resetting = 10,
}

/// A command a phase (or the recipe procedure) accepts. The discriminant is
/// the wire code ([`Command::code`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Command {
    /// Begin the normal logic (Idle only).
    Start = 1,
    /// Pause in a safe state.
    Hold = 2,
    /// Resume from Held.
    Restart = 3,
    /// End in an orderly way.
    Stop = 4,
    /// End quickly.
    Abort = 5,
    /// Return to Idle from a final state.
    Reset = 6,
}

/// What moves a phase: a command from outside, or the end of the current
/// acting state's logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    /// A command.
    Command(Command),
    /// The acting state's logic finished (PackML "state complete").
    Done,
}

/// What a step does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// The phase moved to this state: run its logic if it is acting.
    Enter(PhaseState),
    /// The input is not valid in this state; nothing changed.
    Rejected,
}

impl PhaseState {
    /// Every state, in code order.
    pub const ALL: [PhaseState; STATES] = [
        PhaseState::Idle,
        PhaseState::Running,
        PhaseState::Complete,
        PhaseState::Holding,
        PhaseState::Held,
        PhaseState::Restarting,
        PhaseState::Stopping,
        PhaseState::Stopped,
        PhaseState::Aborting,
        PhaseState::Aborted,
        PhaseState::Resetting,
    ];

    /// The wire code (0..=10).
    pub const fn code(self) -> u8 {
        self as u8
    }

    /// The state with wire code `code`, if any.
    pub fn from_code(code: u8) -> Option<PhaseState> {
        PhaseState::ALL.get(usize::from(code)).copied()
    }

    /// The state's name as ISA-88 / PackML write it.
    pub const fn name(self) -> &'static str {
        match self {
            PhaseState::Idle => "Idle",
            PhaseState::Running => "Running",
            PhaseState::Complete => "Complete",
            PhaseState::Holding => "Holding",
            PhaseState::Held => "Held",
            PhaseState::Restarting => "Restarting",
            PhaseState::Stopping => "Stopping",
            PhaseState::Stopped => "Stopped",
            PhaseState::Aborting => "Aborting",
            PhaseState::Aborted => "Aborted",
            PhaseState::Resetting => "Resetting",
        }
    }

    /// True for the states whose logic runs and ends with [`Input::Done`]
    /// (Running and every `-ing` state).
    pub const fn is_acting(self) -> bool {
        matches!(
            self,
            PhaseState::Running
                | PhaseState::Holding
                | PhaseState::Restarting
                | PhaseState::Stopping
                | PhaseState::Aborting
                | PhaseState::Resetting
        )
    }

    /// True for Complete, Stopped and Aborted: a run has ended.
    pub const fn is_final(self) -> bool {
        matches!(
            self,
            PhaseState::Complete | PhaseState::Stopped | PhaseState::Aborted
        )
    }

    /// True for the acting states other than Running: transitional states
    /// that should finish within a settle time.
    pub const fn is_transient(self) -> bool {
        self.is_acting() && !matches!(self, PhaseState::Running)
    }
}

impl Command {
    /// Every command, in code order.
    pub const ALL: [Command; COMMANDS] = [
        Command::Start,
        Command::Hold,
        Command::Restart,
        Command::Stop,
        Command::Abort,
        Command::Reset,
    ];

    /// The wire code (1..=6).
    pub const fn code(self) -> u8 {
        self as u8
    }

    /// The command with wire code `code`, if any.
    pub fn from_code(code: u8) -> Option<Command> {
        Command::ALL.get(usize::from(code).wrapping_sub(1)).copied()
    }

    /// The command's name.
    pub const fn name(self) -> &'static str {
        match self {
            Command::Start => "Start",
            Command::Hold => "Hold",
            Command::Restart => "Restart",
            Command::Stop => "Stop",
            Command::Abort => "Abort",
            Command::Reset => "Reset",
        }
    }
}

/// The state `input` moves `state` to, if the table (module docs) has a
/// cell for it.
pub const fn next(state: PhaseState, input: Input) -> Option<PhaseState> {
    use Command::*;
    use PhaseState::*;
    match (state, input) {
        (Idle, Input::Command(Start)) => Some(Running),
        (Running, Input::Command(Hold)) => Some(Holding),
        (Running, Input::Done) => Some(Complete),
        (Holding, Input::Done) => Some(Held),
        (Held, Input::Command(Restart)) => Some(Restarting),
        (Restarting, Input::Command(Hold)) => Some(Holding),
        (Restarting, Input::Done) => Some(Running),
        (Running | Holding | Held | Restarting | Resetting, Input::Command(Stop)) => Some(Stopping),
        (Stopping, Input::Done) => Some(Stopped),
        (Running | Holding | Held | Restarting | Stopping | Resetting, Input::Command(Abort)) => {
            Some(Aborting)
        }
        (Aborting, Input::Done) => Some(Aborted),
        (Complete | Stopped | Aborted, Input::Command(Reset)) => Some(Resetting),
        (Resetting, Input::Done) => Some(Idle),
        _ => None,
    }
}

/// Whether `state` accepts `command`.
pub const fn accepts(state: PhaseState, command: Command) -> bool {
    next(state, Input::Command(command)).is_some()
}

/// One step of the phase state machine: the new state and what happened.
/// A rejected input leaves the state as it was.
pub const fn step(state: PhaseState, input: Input) -> (PhaseState, Effect) {
    match next(state, input) {
        Some(to) => (to, Effect::Enter(to)),
        None => (state, Effect::Rejected),
    }
}

#[cfg(test)]
mod tests;
