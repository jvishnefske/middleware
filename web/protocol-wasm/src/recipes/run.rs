//! A control recipe run against real or simulated payloads: recipe-core's
//! [`Engine`] with the wire contract around it, for the browser.
//!
//! [`super::simulate`] runs the engine against recipe-core's own simulated
//! units. A [`Run`] instead leaves the units outside: every
//! [`Effect::Send`] becomes a [`RunEffect::Send`] carrying the command key
//! (`unit/<node>/phase/<em>.<phase>`, [`wire::command_key`]) and body
//! ([`wire::encode_command`], with the step's arguments for a Start), which
//! the JS shell hands to the unit serving it (the browser simulator's
//! `request`, later a board over Web Serial) and feeds back as
//! [`RunInput::Reply`]; every publication on a phase's `/state` key is a
//! [`RunInput::Report`]. The other effects become timed [`TraceLine`]s
//! worded like [`super::simulate`]'s.
//!
//! [`Run::on`] is a pure transition: it takes the run by value with the
//! time and one input and returns the next run and its effects. Replies
//! decode with [`wire::decode_ack`] (a `HandlerError` name maps to the
//! engine's [`AckError`]; an undecodable reply is a transport failure);
//! reports with [`wire::parse_key`] and [`wire::decode_report`], matched
//! to a bound phase by the unit's node id and the phase's `em.phase`
//! (anything else is an `ignored` line). The last values each phase
//! reported are kept for the page ([`PhaseView::values`]).

use recipe_core::alloc::{Allocation, PhaseRef};
use recipe_core::engine::{Ack, AckError, Config, Effect, Engine, Input, StepStatus};
use recipe_core::{Command, Sfc, wire};

use super::{AllocFailure, Binding, ClassIn, RecipeIn, TraceLine, UnitIn};

/// One input from the shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunInput {
    /// An operator command for the procedure.
    Operator(Command),
    /// The reply to command `id`: the reply body, or the `HandlerError`
    /// name the unit answered (`"NotFound"`, ...; anything unknown is a
    /// transport failure).
    Reply {
        /// The command's id.
        id: u32,
        /// Body or error name.
        result: Result<Vec<u8>, String>,
    },
    /// A publication (a phase's `/state` report, or anything else, which
    /// is ignored).
    Report {
        /// Its key.
        key: String,
        /// Its body.
        body: Vec<u8>,
    },
    /// The unit with this id left the network.
    Lost(String),
    /// The unit with this id is back.
    Found(String),
    /// Time passed.
    Tick,
}

/// A command for the shell to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendOut {
    /// Correlation id (the reply's [`RunInput::Reply`] id).
    pub id: u32,
    /// The unit's id.
    pub unit: String,
    /// The command key.
    pub key: String,
    /// The command body.
    pub body: Vec<u8>,
    /// The request's deadline.
    pub timeout_ms: u32,
    /// The command's name.
    pub command: &'static str,
    /// The step's id.
    pub step: String,
}

/// What the shell does or shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunEffect {
    /// Send a command (also traced as a `send` line before it).
    Send(SendOut),
    /// A trace line.
    Line(TraceLine),
}

/// A bound phase as the page shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseView {
    /// The unit's id.
    pub unit: String,
    /// Equipment module.
    pub em: String,
    /// Phase name.
    pub phase: String,
    /// Last known state, `None` when unknown (unreachable or lost).
    pub state: Option<&'static str>,
    /// The values of its last report.
    pub values: Vec<i32>,
}

/// A step as the page shows it: `"inactive"`, `"active"`, `"started"` or
/// `"done"`.
pub fn step_status_name(s: StepStatus) -> &'static str {
    match s {
        StepStatus::Inactive => "inactive",
        StepStatus::Active { started: false } => "active",
        StepStatus::Active { started: true } => "started",
        StepStatus::Done => "done",
    }
}

/// Why a run could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunError {
    /// Allocation failed (or the recipe has errors).
    Alloc(AllocFailure),
    /// `nodes` does not give one well-formed node id per unit.
    Nodes,
}

/// A control recipe run (module docs).
#[derive(Debug, Clone)]
pub struct Run {
    recipe: RecipeIn,
    classes: Vec<ClassIn>,
    units: Vec<UnitIn>,
    nodes: Vec<[u8; 8]>,
    sfc: Sfc,
    alloc: Allocation,
    engine: Engine,
    /// Per distinct bound phase: its ref and last report values.
    phases: Vec<(PhaseRef, Vec<i32>)>,
}

impl Run {
    /// Compiles and allocates `recipe` on `units` (with node ids `nodes`,
    /// one per unit) and builds an Idle engine with `cfg`.
    pub fn new(
        recipe: RecipeIn,
        classes: Vec<ClassIn>,
        units: Vec<UnitIn>,
        nodes: Vec<[u8; 8]>,
        cfg: Config,
    ) -> Result<Run, RunError> {
        if nodes.len() != units.len() {
            return Err(RunError::Nodes);
        }
        let (sfc, alloc) = super::plan(&recipe, &classes, &units).map_err(RunError::Alloc)?;
        let engine = Engine::new(sfc, &alloc, units.len(), cfg).map_err(|_| {
            RunError::Alloc(AllocFailure {
                message: "too many units".to_string(),
                unit_proc: None,
                reasons: Vec::new(),
            })
        })?;
        let mut phases: Vec<(PhaseRef, Vec<i32>)> = Vec::new();
        for p in alloc.step_phase.iter().take(sfc.steps()) {
            if !phases.iter().any(|(q, _)| q == p) {
                phases.push((*p, Vec::new()));
            }
        }
        Ok(Run {
            recipe,
            classes,
            units,
            nodes,
            sfc,
            alloc,
            engine,
            phases,
        })
    }

    /// Where each step runs.
    pub fn bindings(&self) -> Vec<Binding> {
        super::bindings(&self.recipe, &self.units, &self.alloc)
    }

    /// The procedure's state name.
    pub fn state(&self) -> &'static str {
        self.engine.state().name()
    }

    /// Each step's id and status name.
    pub fn steps(&self) -> Vec<(String, &'static str)> {
        self.recipe
            .steps
            .iter()
            .enumerate()
            .map(|(i, s)| (s.id.clone(), step_status_name(self.engine.step(i))))
            .collect()
    }

    /// The bound phases, in first-use order.
    pub fn phases(&self) -> Vec<PhaseView> {
        self.phases
            .iter()
            .map(|(p, values)| {
                let (unit, em, phase) = self.names(*p);
                PhaseView {
                    unit,
                    em,
                    phase,
                    state: self.engine.phase_state(*p).map(|s| s.name()),
                    values: values.clone(),
                }
            })
            .collect()
    }

    /// When the engine next needs a [`RunInput::Tick`], if ever.
    pub fn next_deadline(&self) -> Option<u64> {
        self.engine.next_deadline()
    }

    fn names(&self, p: PhaseRef) -> (String, String, String) {
        let unit = self.units.get(usize::from(p.unit));
        let ph = unit.and_then(|u| u.phases.get(usize::from(p.phase)));
        (
            unit.map(|u| u.id.clone()).unwrap_or_default(),
            ph.map(|x| x.em.clone()).unwrap_or_default(),
            ph.map(|x| x.name.clone()).unwrap_or_default(),
        )
    }

    fn unit_index(&self, id: &str) -> Option<u8> {
        self.units
            .iter()
            .position(|u| u.id == id)
            .and_then(|i| u8::try_from(i).ok())
    }

    fn line(t: u64, kind: &'static str, text: String) -> RunEffect {
        RunEffect::Line(TraceLine { t, kind, text })
    }

    /// The engine input for a reply.
    fn ack(result: &Result<Vec<u8>, String>) -> Ack {
        match result {
            Ok(body) => wire::decode_ack(body)
                .map(|(_, a)| a)
                .unwrap_or(Ack::Failed(AckError::Transport)),
            Err(name) => Ack::Failed(match name.as_str() {
                "NotFound" => AckError::NotFound,
                "InvalidRequest" => AckError::InvalidRequest,
                "Unavailable" => AckError::Unavailable,
                "DeadlineExceeded" => AckError::DeadlineExceeded,
                _ => AckError::Transport,
            }),
        }
    }

    /// A report's engine input, recording its values; `Err` with why it
    /// is not one.
    fn report(&mut self, key: &str, body: &[u8]) -> Result<Input, &'static str> {
        let k = wire::parse_key(key).map_err(|_| "a publication that is not a phase report")?;
        if !k.state {
            return Err("a publication on a phase command key");
        }
        let found = self.phases.iter_mut().find(|(p, _)| {
            let unit = self.units.get(usize::from(p.unit));
            let ph = unit.and_then(|u| u.phases.get(usize::from(p.phase)));
            self.nodes.get(usize::from(p.unit)) == Some(&k.node)
                && ph.is_some_and(|x| x.em == k.em && x.name == k.phase)
        });
        let Some((phase, values)) = found else {
            return Err("a report for a phase the recipe does not use");
        };
        let r = wire::decode_report(body).map_err(|_| "a malformed phase report")?;
        *values = r.values.get(..r.n).unwrap_or_default().to_vec();
        Ok(Input::Report {
            phase: *phase,
            state: r.state,
            last_id: r.last_id,
        })
    }

    /// One input at `now_ms` (module docs).
    pub fn on(mut self, now_ms: u64, input: RunInput) -> (Run, Vec<RunEffect>) {
        let mut out = Vec::new();
        let engine_input = match input {
            RunInput::Operator(c) => Input::Operator(c),
            RunInput::Reply { id, result } => Input::Ack {
                id,
                result: Self::ack(&result),
            },
            RunInput::Report { key, body } => match self.report(&key, &body) {
                Ok(i) => i,
                Err(why) => {
                    out.push(Self::line(now_ms, "ignored", why.to_string()));
                    return (self, out);
                }
            },
            RunInput::Lost(u) | RunInput::Found(u) if self.unit_index(&u).is_none() => {
                out.push(Self::line(now_ms, "ignored", format!("no unit \"{u}\"")));
                return (self, out);
            }
            RunInput::Lost(u) => Input::UnitLost(self.unit_index(&u).unwrap_or_default()),
            RunInput::Found(u) => Input::UnitFound(self.unit_index(&u).unwrap_or_default()),
            RunInput::Tick => Input::Tick,
        };
        let (engine, fx) = self.engine.on(now_ms, engine_input);
        self.engine = engine;
        for e in fx.iter() {
            let (kind, text) = super::effect_line(&self.recipe, &self.units, e);
            out.push(Self::line(now_ms, kind, text));
            if let Effect::Send {
                step,
                phase,
                command,
                id,
                timeout_ms,
            } = *e
            {
                match self.send(step, phase, command, id, timeout_ms) {
                    Some(s) => out.push(RunEffect::Send(s)),
                    None => out.push(Self::line(
                        now_ms,
                        "failed",
                        format!("could not encode {} #{id}", command.name()),
                    )),
                }
            }
        }
        (self, out)
    }

    fn send(
        &self,
        step: u8,
        phase: PhaseRef,
        command: Command,
        id: u32,
        timeout_ms: u32,
    ) -> Option<SendOut> {
        let s = usize::from(step);
        let (class, _) = self.sfc.step_of(s)?;
        let n = self.classes.get(class)?.params.len();
        let args: &[i32] = if command == Command::Start {
            self.sfc.args(s)?.get(..n)?
        } else {
            &[]
        };
        let mut body = [0u8; wire::MAX_BODY];
        let body = wire::encode_command(command, id, args, &mut body).ok()?;
        let (unit, em, ph) = self.names(phase);
        let node = *self.nodes.get(usize::from(phase.unit))?;
        let mut key = [0u8; wire::MAX_KEY];
        let key = wire::command_key(node, &em, &ph, &mut key).ok()?;
        Some(SendOut {
            id,
            unit,
            key: key.to_string(),
            body: body.to_vec(),
            timeout_ms,
            command: command.name(),
            step: self.recipe.steps.get(s)?.id.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recipes::tests::{example_classes, example_recipe, example_units};
    use recipe_core::PhaseState;
    use recipe_core::engine::Ack;
    use recipe_core::engine::Config;

    const NODES: [[u8; 8]; 2] = [
        [0x24, 0x6f, 0x28, 1, 2, 3, 0, 0],
        [0xe6, 0x61, 0, 0, 0, 9, 0, 0],
    ];

    fn run() -> Run {
        Run::new(
            example_recipe(),
            example_classes(),
            example_units(),
            NODES.to_vec(),
            Config::DEFAULT,
        )
        .unwrap()
    }

    fn sends(fx: &[RunEffect]) -> Vec<SendOut> {
        fx.iter()
            .filter_map(|e| match e {
                RunEffect::Send(s) => Some(s.clone()),
                RunEffect::Line(_) => None,
            })
            .collect()
    }

    fn report(
        node: [u8; 8],
        em: &str,
        ph: &str,
        state: PhaseState,
        id: u32,
        values: &[i32],
    ) -> RunInput {
        let mut kb = [0u8; wire::MAX_KEY];
        let mut bb = [0u8; wire::MAX_BODY];
        RunInput::Report {
            key: wire::state_key(node, em, ph, &mut kb).unwrap().to_string(),
            body: wire::encode_report(state, id, values, &mut bb)
                .unwrap()
                .to_vec(),
        }
    }

    fn ack(id: u32, state: PhaseState) -> RunInput {
        let mut b = [0u8; wire::ACK_LEN];
        RunInput::Reply {
            id,
            result: Ok(wire::encode_ack(true, state, id, &mut b).unwrap().to_vec()),
        }
    }

    #[test]
    fn q19_start_sends_the_step_arguments_to_the_bound_phase_key() {
        let (r, fx) = run().on(0, RunInput::Operator(Command::Start));
        let s = sends(&fx);
        assert_eq!(s.len(), 1, "{fx:?}");
        assert_eq!(s[0].key, "unit/246f280102030000/phase/LED.Blink");
        assert_eq!(s[0].unit, "pico-led");
        assert_eq!(s[0].command, "Start");
        let body = wire::decode_command(&s[0].body).unwrap();
        assert_eq!(body.command, Command::Start);
        assert_eq!(&body.args[..body.n], &[250, 3]);
        assert_eq!(r.state(), "Running");
        assert_eq!(r.steps()[0], ("BlinkLed".to_string(), "active"));
        // The accepted reply starts the step; the report carries values.
        let (r, _) = r.on(5, ack(s[0].id, PhaseState::Running));
        assert_eq!(r.steps()[0].1, "started");
        let (r, fx) = r.on(
            10,
            report(
                NODES[0],
                "LED",
                "Blink",
                PhaseState::Complete,
                s[0].id,
                &[3],
            ),
        );
        let s = sends(&fx);
        assert_eq!(
            s.iter().map(|s| s.command).collect::<Vec<_>>(),
            ["Reset", "Start"],
            "{fx:?}"
        );
        assert_eq!(s[1].key, "unit/e661000000090000/phase/ADC.Measure");
        assert_eq!(r.phases()[0].values, [3]);
        assert_eq!(r.phases()[0].state, Some("Complete"));
    }

    #[test]
    fn foreign_reports_errors_and_unknown_units_are_ignored_lines() {
        let r = run();
        let (r, fx) = r.on(0, report([9; 8], "LED", "Blink", PhaseState::Idle, 0, &[]));
        assert!(matches!(&fx[..], [RunEffect::Line(l)] if l.kind == "ignored"));
        let (r, fx) = r.on(
            0,
            RunInput::Report {
                key: "app/x".into(),
                body: vec![],
            },
        );
        assert!(matches!(&fx[..], [RunEffect::Line(l)] if l.kind == "ignored"));
        let (r, fx) = r.on(0, RunInput::Lost("nobody".into()));
        assert!(matches!(&fx[..], [RunEffect::Line(l)] if l.text.contains("nobody")));
        let (r, fx) = r.on(0, RunInput::Operator(Command::Start));
        let id = sends(&fx)[0].id;
        let (r, fx) = r.on(
            1,
            RunInput::Reply {
                id,
                result: Err("NotFound".into()),
            },
        );
        assert!(fx.iter().any(|e| matches!(e, RunEffect::Line(l) if l.kind == "failed" && l.text.contains("no such phase"))), "{fx:?}");
        // Nothing was started, so the abort finishes at once.
        assert_eq!(r.state(), "Aborted");
        assert_eq!(Run::ack(&Ok(vec![1, 2])), Ack::Failed(AckError::Transport));
        assert_eq!(
            Run::ack(&Err("Denied".into())),
            Ack::Failed(AckError::Transport)
        );
    }

    #[test]
    fn nodes_must_match_units() {
        let e = Run::new(
            example_recipe(),
            example_classes(),
            example_units(),
            vec![NODES[0]],
            Config::DEFAULT,
        );
        assert!(matches!(e, Err(RunError::Nodes)));
    }
}
