//! The sensor bring-up as a pure state machine: [`Bringup::action`] says
//! what to do next, the shell does it (over an [`Sccb`] bus, or by starting
//! the camera route) and feeds the [`Outcome`] to [`Bringup::advance`],
//! which returns the next state and how long to [`Wait`] before its action.
//!
//! The order, each step one call into the payload:
//!
//! 1. ([`Bringup::start`]) wait [`POWER_UP_MS`] after power and XCLK.
//! 2. [`Phase::Identify`]: select the sensor bank, read `PID` and `VER`
//!    ([`identify`]). A part that is not an OV2640 is a failure.
//! 3. [`Phase::Reset`]: [`RESET_REGS`] (sensor bank, `COM7` = soft reset),
//!    then wait [`RESET_MS`].
//! 4. [`Phase::Table`]: [`TABLE`] in slices of at most [`WRITES_PER_STEP`]
//!    writes, [`STEP_GAP_MS`] apart; after the last one wait [`SETTLE_MS`].
//! 5. [`Phase::StartRoute`]: start the camera route. A failure retries on
//!    every stats tick ([`Wait::Tick`]), without limit.
//! 6. [`Phase::Streaming`]: nothing more until the route stops
//!    ([`Bringup::route_stopped`] goes back to step 5).
//!
//! A bus error or a wrong ID in steps 2–4 is a failure: the bring-up starts
//! again at step 2 after [`backoff_ms`] (the soft reset puts every register
//! back, so a half-written table does no harm). After [`MAX_FAILURES`]
//! failures in a row it gives up ([`Phase::GaveUp`]). A route that starts
//! clears the count.

use mw_payload::AbiError;

use crate::ov2640::{
    BANK_SEL, BANK_SEL_SENSOR, COM7, COM7_SRST, REG_PID, REG_VER, RegWrite, TABLE, TABLE_LEN,
    is_ov2640,
};

/// Wait after power-up and XCLK before the first SCCB access.
pub const POWER_UP_MS: u32 = 10;
/// Wait after the soft reset (Zephyr waits 300 ms; the sensor needs at
/// least a few ms before it acknowledges again).
pub const RESET_MS: u32 = 300;
/// Wait after the last table write before starting the route (Zephyr
/// waits 30 ms after enabling the DSP).
pub const SETTLE_MS: u32 = 30;
/// Gap between two table slices: a timer round trip that lets the host
/// run its own work.
pub const STEP_GAP_MS: u32 = 1;
/// Most register writes in one call. A full slice measured under 6 000
/// fuel in `payload-sim` (see the crate docs), far below the 1 000 000 a
/// call may use; the bound keeps each call short in wall-clock time too
/// (32 transfers of 3 bytes at 100 kHz, about 10 ms).
pub const WRITES_PER_STEP: usize = 32;
/// Consecutive bring-up failures before giving up.
pub const MAX_FAILURES: u8 = 5;
/// First back-off after a failure; it doubles with every further one.
pub const BACKOFF_BASE_MS: u32 = 100;

// The sensor's minimum delays (power-up ~10 ms, soft reset >= 50 ms,
// DSP settle ~30 ms), checked when building.
const _: () = assert!(POWER_UP_MS >= 10 && RESET_MS >= 50 && SETTLE_MS >= 30);

/// The soft reset: select the sensor bank, set `COM7`'s reset bit.
pub const RESET_REGS: [RegWrite; 2] = [(BANK_SEL, BANK_SEL_SENSOR), (COM7, COM7_SRST)];

/// The wait before retrying after `failures` failures in a row (≥ 1):
/// [`BACKOFF_BASE_MS`] doubled per failure, 100, 200, 400, 800 ms.
pub const fn backoff_ms(failures: u8) -> u32 {
    let shift = if failures == 0 { 0 } else { failures - 1 };
    let shift = if shift > 4 { 4 } else { shift };
    BACKOFF_BASE_MS << shift
}

/// Where the bring-up is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Read and check the sensor ID.
    Identify,
    /// Soft-reset the sensor.
    Reset,
    /// Write [`TABLE`] from entry `next` on.
    Table {
        /// Index of the next entry to write.
        next: u16,
    },
    /// Start the camera route.
    StartRoute,
    /// The route runs.
    Streaming,
    /// Too many failures in a row; nothing more happens.
    GaveUp,
}

/// What the shell must do for the current phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// [`identify`]: select the sensor bank, read `PID` and `VER`.
    Identify,
    /// Write these registers in order ([`write_all`]).
    Write(&'static [RegWrite]),
    /// Start the camera route.
    StartRoute,
    /// Nothing ([`Phase::Streaming`], [`Phase::GaveUp`]).
    Nothing,
}

/// Why a step failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// An SCCB transfer failed (no acknowledge, bus error, ...).
    Bus(AbiError),
    /// The host refused the camera route.
    Route(AbiError),
}

/// The result of an [`Action`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// [`Action::Identify`] read these.
    Id {
        /// `PID`.
        pid: u8,
        /// `VER`.
        ver: u8,
    },
    /// [`Action::Write`] wrote every register.
    Written,
    /// [`Action::StartRoute`] started the route.
    Started,
    /// The action failed.
    Failed(Failure),
}

/// When to run the next [`Bringup::action`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// After this many milliseconds (a one-shot timer).
    Ms(u32),
    /// On the next stats tick.
    Tick,
    /// Not on its own: only an event (a route stop) moves on.
    Idle,
}

/// The bring-up state: the phase and the failures in a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bringup {
    phase: Phase,
    failures: u8,
}

/// A state together with when its action is due.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    /// The new state.
    pub state: Bringup,
    /// When to run its action.
    pub wait: Wait,
}

/// A bus to the sensor's registers (SCCB). The shell implements it over
/// the I2C import; tests implement it with a model.
pub trait Sccb {
    /// Writes `value` to `reg` in the selected bank.
    fn write(&mut self, reg: u8, value: u8) -> Result<(), AbiError>;
    /// Reads `reg` in the selected bank.
    fn read(&mut self, reg: u8) -> Result<u8, AbiError>;
}

/// [`Action::Identify`]: selects the sensor bank and reads `PID`, `VER`.
pub fn identify<B: Sccb>(bus: &mut B) -> Outcome {
    let id = bus
        .write(BANK_SEL, BANK_SEL_SENSOR)
        .and_then(|()| Ok((bus.read(REG_PID)?, bus.read(REG_VER)?)));
    match id {
        Ok((pid, ver)) => Outcome::Id { pid, ver },
        Err(e) => Outcome::Failed(Failure::Bus(e)),
    }
}

/// [`Action::Write`]: writes `regs` in order, stopping at the first error.
pub fn write_all<B: Sccb>(bus: &mut B, regs: &[RegWrite]) -> Outcome {
    for &(reg, value) in regs {
        if let Err(e) = bus.write(reg, value) {
            return Outcome::Failed(Failure::Bus(e));
        }
    }
    Outcome::Written
}

/// Runs a bus action ([`Action::Identify`] or [`Action::Write`]); `None`
/// for the others, which are not bus work.
pub fn run<B: Sccb>(action: Action, bus: &mut B) -> Option<Outcome> {
    match action {
        Action::Identify => Some(identify(bus)),
        Action::Write(regs) => Some(write_all(bus, regs)),
        Action::StartRoute | Action::Nothing => None,
    }
}

impl Bringup {
    /// The first state, [`Phase::Identify`], due [`POWER_UP_MS`] after the
    /// shell powered the sensor and started its clock.
    pub const fn start() -> Step {
        Step {
            state: Bringup {
                phase: Phase::Identify,
                failures: 0,
            },
            wait: Wait::Ms(POWER_UP_MS),
        }
    }

    /// A state that does nothing (the shell could not power the sensor).
    pub const fn gave_up() -> Bringup {
        Bringup {
            phase: Phase::GaveUp,
            failures: 0,
        }
    }

    /// The phase.
    pub const fn phase(&self) -> Phase {
        self.phase
    }

    /// Failures in a row so far.
    pub const fn failures(&self) -> u8 {
        self.failures
    }

    /// What the shell must do now.
    pub fn action(&self) -> Action {
        match self.phase {
            Phase::Identify => Action::Identify,
            Phase::Reset => Action::Write(&RESET_REGS),
            Phase::Table { next } => {
                let start = usize::from(next);
                let end = start.saturating_add(WRITES_PER_STEP).min(TABLE_LEN);
                match TABLE.get(start..end) {
                    Some(regs) if !regs.is_empty() => Action::Write(regs),
                    // Unreachable through `advance`; nothing left to write.
                    _ => Action::Nothing,
                }
            }
            Phase::StartRoute => Action::StartRoute,
            Phase::Streaming | Phase::GaveUp => Action::Nothing,
        }
    }

    /// The state after this phase's action ended with `outcome`. An outcome
    /// that does not belong to the phase leaves the state as it is.
    pub fn advance(self, outcome: Outcome) -> Step {
        let at = |phase: Phase, wait: Wait| Step {
            state: Bringup { phase, ..self },
            wait,
        };
        match (self.phase, outcome) {
            (Phase::Identify, Outcome::Id { pid, ver }) if is_ov2640(pid, ver) => {
                at(Phase::Reset, Wait::Ms(STEP_GAP_MS))
            }
            (Phase::Identify, Outcome::Id { .. }) => self.fail(),
            (Phase::Reset, Outcome::Written) => at(Phase::Table { next: 0 }, Wait::Ms(RESET_MS)),
            (Phase::Table { next }, Outcome::Written) => {
                let end = usize::from(next)
                    .saturating_add(WRITES_PER_STEP)
                    .min(TABLE_LEN);
                match u16::try_from(end) {
                    Ok(next) if end < TABLE_LEN => at(Phase::Table { next }, Wait::Ms(STEP_GAP_MS)),
                    _ => at(Phase::StartRoute, Wait::Ms(SETTLE_MS)),
                }
            }
            (Phase::Identify | Phase::Reset | Phase::Table { .. }, Outcome::Failed(_)) => {
                self.fail()
            }
            (Phase::StartRoute, Outcome::Started) => Step {
                state: Bringup {
                    phase: Phase::Streaming,
                    failures: 0,
                },
                wait: Wait::Idle,
            },
            (Phase::StartRoute, Outcome::Failed(_)) => at(Phase::StartRoute, Wait::Tick),
            _ => Step {
                state: self,
                wait: Wait::Idle,
            },
        }
    }

    /// The route stopped by itself: start it again (the sensor keeps its
    /// registers). Only meaningful while [`Phase::Streaming`].
    pub fn route_stopped(self) -> Bringup {
        match self.phase {
            Phase::Streaming => Bringup {
                phase: Phase::StartRoute,
                ..self
            },
            _ => self,
        }
    }

    /// One more failure: back to [`Phase::Identify`] after a back-off, or
    /// [`Phase::GaveUp`] at [`MAX_FAILURES`].
    fn fail(self) -> Step {
        let failures = self.failures.saturating_add(1);
        if failures >= MAX_FAILURES {
            Step {
                state: Bringup {
                    phase: Phase::GaveUp,
                    failures,
                },
                wait: Wait::Idle,
            }
        } else {
            Step {
                state: Bringup {
                    phase: Phase::Identify,
                    failures,
                },
                wait: Wait::Ms(backoff_ms(failures)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ov2640::{
        BANK_SEL_DSP, IMAGE_MODE, IMAGE_MODE_JPEG_EN, QS, QUALITY, ZMHH, ZMOH, ZMOW,
    };

    /// What a test bus saw.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Op {
        W(u8, u8),
        R(u8),
    }

    /// A banked register model of the OV2640 with fault injection.
    struct Model {
        ops: Vec<Op>,
        bank: u8,
        regs: [[u8; 256]; 2],
        /// Fail the n-th operation (0-based) with `Transport`.
        fail_at: Option<usize>,
    }

    impl Model {
        fn new(pid: u8, ver: u8) -> Self {
            let mut regs = [[0u8; 256]; 2];
            regs[1][usize::from(REG_PID)] = pid;
            regs[1][usize::from(REG_VER)] = ver;
            Model {
                ops: Vec::new(),
                bank: 0,
                regs,
                fail_at: None,
            }
        }

        fn ov2640() -> Self {
            Model::new(0x26, 0x42)
        }

        fn check_fault(&mut self) -> Result<(), AbiError> {
            if self.fail_at == Some(self.ops.len()) {
                self.fail_at = None;
                return Err(AbiError::Transport);
            }
            Ok(())
        }

        fn writes(&self) -> Vec<(u8, u8)> {
            self.ops
                .iter()
                .filter_map(|op| match *op {
                    Op::W(r, v) => Some((r, v)),
                    Op::R(_) => None,
                })
                .collect()
        }
    }

    impl Sccb for Model {
        fn write(&mut self, reg: u8, value: u8) -> Result<(), AbiError> {
            self.check_fault()?;
            self.ops.push(Op::W(reg, value));
            if reg == BANK_SEL {
                self.bank = value & 1;
            } else {
                self.regs[usize::from(self.bank)][usize::from(reg)] = value;
            }
            Ok(())
        }

        fn read(&mut self, reg: u8) -> Result<u8, AbiError> {
            self.check_fault()?;
            self.ops.push(Op::R(reg));
            Ok(self.regs[usize::from(self.bank)][usize::from(reg)])
        }
    }

    /// One shell step: runs the action against `bus` (the route start
    /// succeeds when `route_ok`), returns the next step and the action.
    fn step(state: Bringup, bus: &mut Model, route_ok: bool) -> (Step, Action) {
        let action = state.action();
        let outcome = match run(action, bus) {
            Some(o) => o,
            None if action == Action::StartRoute && route_ok => Outcome::Started,
            None if action == Action::StartRoute => {
                Outcome::Failed(Failure::Route(AbiError::Unsupported))
            }
            None => return (state.advance(Outcome::Written), action),
        };
        (state.advance(outcome), action)
    }

    /// Runs from `start` until the phase is `Streaming` or `GaveUp`, or
    /// `limit` steps; returns every (action, step) pair.
    fn drive(bus: &mut Model, route_ok: bool, limit: usize) -> Vec<(Action, Step)> {
        let mut s = Bringup::start().state;
        let mut out = Vec::new();
        for _ in 0..limit {
            let (next, action) = step(s, bus, route_ok);
            out.push((action, next));
            s = next.state;
            if matches!(s.phase(), Phase::Streaming | Phase::GaveUp) {
                break;
            }
        }
        out
    }

    #[test]
    fn starts_with_identify_after_the_power_up_delay() {
        let s = Bringup::start();
        assert_eq!(s.state.phase(), Phase::Identify);
        assert_eq!(s.wait, Wait::Ms(POWER_UP_MS));
        assert_eq!(s.state.action(), Action::Identify);
    }

    #[test]
    fn happy_path_order_id_then_reset_then_whole_table_then_route() {
        let mut bus = Model::ov2640();
        let steps = drive(&mut bus, true, 100);
        // Identify: bank select then the two ID reads, before any other
        // write.
        assert_eq!(
            &bus.ops[..3],
            &[
                Op::W(BANK_SEL, BANK_SEL_SENSOR),
                Op::R(REG_PID),
                Op::R(REG_VER)
            ]
        );
        // Then the soft reset, then every table entry exactly once, in
        // order, and nothing else.
        let writes = bus.writes();
        assert_eq!(&writes[1..3], &RESET_REGS);
        assert_eq!(&writes[3..], &TABLE[..]);
        assert_eq!(writes.len(), 1 + 2 + TABLE_LEN);
        // Delays: reset waits RESET_MS before the first slice; the last
        // slice waits SETTLE_MS before the route; the route is last.
        assert_eq!(steps[0].0, Action::Identify);
        assert_eq!(steps[0].1.wait, Wait::Ms(STEP_GAP_MS));
        assert_eq!(steps[1].0, Action::Write(&RESET_REGS));
        assert_eq!(steps[1].1.wait, Wait::Ms(RESET_MS));
        let (last_action, last) = steps[steps.len() - 1];
        assert_eq!(last_action, Action::StartRoute);
        assert_eq!(last.state.phase(), Phase::Streaming);
        assert_eq!(last.wait, Wait::Idle);
        let (before, settle) = steps[steps.len() - 2];
        assert!(matches!(before, Action::Write(_)));
        assert_eq!(settle.wait, Wait::Ms(SETTLE_MS));
        // Table slices: bounded, gap-separated.
        let slices: Vec<usize> = steps[2..steps.len() - 1]
            .iter()
            .map(|(a, _)| match a {
                Action::Write(r) => r.len(),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(slices.len(), TABLE_LEN.div_ceil(WRITES_PER_STEP));
        assert!(slices.iter().all(|&n| (1..=WRITES_PER_STEP).contains(&n)));
        assert_eq!(slices.iter().sum::<usize>(), TABLE_LEN);
        for (_, s) in &steps[2..steps.len() - 2] {
            assert_eq!(s.wait, Wait::Ms(STEP_GAP_MS));
        }
    }

    #[test]
    fn final_registers_select_240x240_jpeg() {
        let mut bus = Model::ov2640();
        drive(&mut bus, true, 100);
        let dsp = &bus.regs[usize::from(BANK_SEL_DSP)];
        assert_eq!(dsp[usize::from(IMAGE_MODE)], IMAGE_MODE_JPEG_EN);
        assert_eq!(dsp[usize::from(QS)], QUALITY);
        assert_eq!(dsp[usize::from(ZMOW)], 60);
        assert_eq!(dsp[usize::from(ZMOH)], 60);
        assert_eq!(dsp[usize::from(ZMHH)], 0);
        assert_eq!(dsp[0xE0], 0x00, "RESET released");
        assert_eq!(dsp[0x05], 0x00, "DSP enabled");
    }

    #[test]
    fn wrong_id_backs_off_without_resetting_and_gives_up() {
        let mut bus = Model::new(0x77, 0x21);
        let steps = drive(&mut bus, true, 100);
        assert_eq!(steps.len(), usize::from(MAX_FAILURES));
        for (i, (action, s)) in steps.iter().enumerate() {
            assert_eq!(*action, Action::Identify);
            let n = u8::try_from(i + 1).unwrap();
            assert_eq!(s.state.failures(), n);
            if n < MAX_FAILURES {
                assert_eq!(s.state.phase(), Phase::Identify);
                assert_eq!(s.wait, Wait::Ms(backoff_ms(n)));
            } else {
                assert_eq!(s.state.phase(), Phase::GaveUp);
                assert_eq!(s.wait, Wait::Idle);
                assert_eq!(s.state.action(), Action::Nothing);
            }
        }
        // Never reset, never wrote the table.
        assert!(
            bus.writes()
                .iter()
                .all(|&w| w == (BANK_SEL, BANK_SEL_SENSOR)),
            "{:?}",
            bus.writes()
        );
    }

    #[test]
    fn backoff_doubles_and_is_capped() {
        assert_eq!(
            [1, 2, 3, 4].map(backoff_ms),
            [100, 200, 400, 800],
            "the four retries before giving up"
        );
        assert_eq!(backoff_ms(0), 100);
        assert_eq!(backoff_ms(200), 1600);
    }

    #[test]
    fn a_bus_error_mid_table_restarts_from_identify_and_rewrites_everything() {
        // Fail the 100th bus operation (inside the table).
        let mut bus = Model::ov2640();
        bus.fail_at = Some(100);
        let steps = drive(&mut bus, true, 200);
        let failed = steps
            .iter()
            .position(|(_, s)| s.state.failures() == 1)
            .expect("a failure");
        assert!(matches!(steps[failed].0, Action::Write(_)));
        assert_eq!(steps[failed].1.state.phase(), Phase::Identify);
        assert_eq!(steps[failed].1.wait, Wait::Ms(backoff_ms(1)));
        // Then the whole bring-up again: identify, reset, the full table.
        assert_eq!(steps[failed + 1].0, Action::Identify);
        assert_eq!(steps[failed + 2].0, Action::Write(&RESET_REGS));
        let writes = bus.writes();
        assert_eq!(&writes[writes.len() - TABLE_LEN..], &TABLE[..]);
        let last = steps[steps.len() - 1].1;
        assert_eq!(last.state.phase(), Phase::Streaming);
        assert_eq!(last.state.failures(), 0, "a started route clears it");
    }

    #[test]
    fn bus_errors_in_identify_and_reset_count_as_failures() {
        let s = Bringup::start().state;
        let failed = Outcome::Failed(Failure::Bus(AbiError::Transport));
        let a = s.advance(failed);
        assert_eq!(a.state.phase(), Phase::Identify);
        assert_eq!(a.state.failures(), 1);
        let reset = a.state.advance(Outcome::Id {
            pid: 0x26,
            ver: 0x41,
        });
        assert_eq!(reset.state.phase(), Phase::Reset);
        let b = reset.state.advance(failed);
        assert_eq!(b.state.phase(), Phase::Identify);
        assert_eq!(b.state.failures(), 2, "failures in a row accumulate");
        assert_eq!(b.wait, Wait::Ms(200));
    }

    #[test]
    fn route_failures_retry_on_the_tick_forever_and_a_stop_restarts_it() {
        let mut bus = Model::ov2640();
        let steps = drive(&mut bus, false, 100);
        let last = steps[steps.len() - 1].1;
        assert_eq!(last.state.phase(), Phase::StartRoute);
        assert_eq!(last.wait, Wait::Tick);
        // Many more failed starts: still retrying, never giving up.
        let mut s = last.state;
        for _ in 0..1000 {
            let next = s.advance(Outcome::Failed(Failure::Route(AbiError::Unsupported)));
            assert_eq!(next.wait, Wait::Tick);
            s = next.state;
        }
        assert_eq!(s.phase(), Phase::StartRoute);
        let running = s.advance(Outcome::Started).state;
        assert_eq!(running.phase(), Phase::Streaming);
        assert_eq!(running.route_stopped().phase(), Phase::StartRoute);
        assert_eq!(running.route_stopped().action(), Action::StartRoute);
        // A stop outside Streaming changes nothing.
        let id = Bringup::start().state;
        assert_eq!(id.route_stopped(), id);
    }

    #[test]
    fn outcomes_of_other_phases_are_ignored() {
        let s = Bringup::start().state;
        assert_eq!(
            s.advance(Outcome::Written),
            Step {
                state: s,
                wait: Wait::Idle
            }
        );
        let gave_up = Bringup::gave_up();
        assert_eq!(gave_up.action(), Action::Nothing);
        assert_eq!(gave_up.advance(Outcome::Started).state, gave_up);
    }

    #[test]
    fn every_action_is_bounded() {
        // Every table slice the sequencer can produce holds at most
        // WRITES_PER_STEP writes.
        let mut next = 0u16;
        loop {
            let s = Bringup {
                phase: Phase::Table { next },
                failures: 0,
            };
            let Action::Write(regs) = s.action() else {
                panic!("table phase writes");
            };
            assert!(regs.len() <= WRITES_PER_STEP);
            let after = s.advance(Outcome::Written).state.phase();
            match after {
                Phase::Table { next: n } => next = n,
                Phase::StartRoute => break,
                other => panic!("{other:?}"),
            }
        }
        // An index past the table (not reachable) asks for nothing.
        let past = Bringup {
            phase: Phase::Table {
                next: TABLE_LEN as u16,
            },
            failures: 0,
        };
        assert_eq!(past.action(), Action::Nothing);
    }
}
