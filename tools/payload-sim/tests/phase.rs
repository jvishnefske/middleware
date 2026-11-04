//! The generated S88 phase server (web `blocks/phaseGen.ts`; fixtures in
//! `tests/fixtures/phase/`, rebuilt and checked by
//! `web/test/phase.gen.test.ts`) against recipe-core, the reference:
//!
//! * every one of the 11 × 7 cells of the phase state model: the
//!   payload-sim binary drives the `matrix` unit (one phase `X.P` whose
//!   acting states all have bodies that never finish) from Idle to each
//!   state and applies each input, a command by `!req` and Done by a
//!   message that runs `phase-done`; the acknowledgement and the state
//!   reports must be `recipe_core::phase::next`'s, byte for byte the
//!   bodies `recipe_core::wire` encodes;
//! * Start's argument checks and malformed bodies: `InvalidRequest`;
//! * the worked example "Blink then Measure" end to end: the two units'
//!   payloads (`pico-led`, `esp-sense`) run in `wasm-host` and
//!   recipe-core's engine drives them through `Payload::request` and
//!   their published reports, to Complete; a Hold mid-run and a Restart;
//!   losing the LED unit, a refused Restart and an Abort to Aborted.

use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command as Proc, Stdio};

use middleware::{HandlerError, Key, RequestMeta, SenderId};
use recipe_core::alloc::{PhaseRef, Unit, UnitPhase, allocate};
use recipe_core::engine::{Ack, AckError, Config, Effect, Engine, Failure, Input, Refusal};
use recipe_core::phase::{self, Command, PhaseState};
use recipe_core::recipe::{Arg, ParamDef, PhaseClass, Recipe, Step, Transition, UnitProc, compile};
use recipe_core::{Sfc, wire};
use wasm_host::{AbiError, Device, Limits, Payload, Pull};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/phase/{name}.wasm"))
}

/// The default node of payload-sim's first board.
const SIM_NODE: [u8; 8] = [0, 0, 0, 0, 0, 0, 0, 1];

fn key(node: [u8; 8], em: &str, ph: &str, state: bool) -> String {
    let mut buf = [0u8; wire::MAX_KEY];
    let k = if state {
        wire::state_key(node, em, ph, &mut buf)
    } else {
        wire::command_key(node, em, ph, &mut buf)
    };
    k.expect("key").to_string()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
        .collect()
}

/// One input of the phase model: a command or Done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum In {
    Cmd(Command),
    Done,
}

const INPUTS: [In; 7] = [
    In::Cmd(Command::Start),
    In::Cmd(Command::Hold),
    In::Cmd(Command::Restart),
    In::Cmd(Command::Stop),
    In::Cmd(Command::Abort),
    In::Cmd(Command::Reset),
    In::Done,
];

fn next(s: PhaseState, i: In) -> Option<PhaseState> {
    phase::next(
        s,
        match i {
            In::Cmd(c) => phase::Input::Command(c),
            In::Done => phase::Input::Done,
        },
    )
}

/// The shortest inputs from Idle to `target` (breadth first over the
/// table, so the paths come from recipe-core too).
fn path_to(target: PhaseState) -> Vec<In> {
    let mut seen = vec![PhaseState::Idle];
    let mut queue = VecDeque::from([(PhaseState::Idle, Vec::new())]);
    while let Some((s, path)) = queue.pop_front() {
        if s == target {
            return path;
        }
        for i in INPUTS {
            if let Some(to) = next(s, i)
                && !seen.contains(&to)
            {
                seen.push(to);
                let mut p = path.clone();
                p.push(i);
                queue.push_back((to, p));
            }
        }
    }
    panic!("{target:?} unreachable");
}

/// The stdin line for input `i` as command id `id`.
fn line(i: In, id: u32) -> String {
    match i {
        In::Done => "t/done x".to_string(),
        In::Cmd(c) => {
            let args: &[i32] = if c == Command::Start { &[5] } else { &[] };
            let mut buf = [0u8; wire::MAX_BODY];
            let body = wire::encode_command(c, id, args, &mut buf).expect("body");
            format!("!req {} {}", key(SIM_NODE, "X", "P", false), hex(body))
        }
    }
}

/// What a payload-sim run printed: the replies and the state reports.
struct Run {
    replies: Vec<Result<Vec<u8>, String>>,
    reports: Vec<Vec<u8>>,
}

fn sim(stdin: &str) -> Run {
    let mut child = Proc::new(env!("CARGO_BIN_EXE_payload-sim"))
        .arg(fixture("matrix"))
        .arg("--virtual")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin.as_bytes())
        .expect("stdin");
    let out = child.wait_with_output().expect("runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).expect("utf8");
    let state_key = key(SIM_NODE, "X", "P", true);
    let cmd_key = key(SIM_NODE, "X", "P", false);
    let mut run = Run {
        replies: Vec::new(),
        reports: Vec::new(),
    };
    for l in text.lines() {
        let Some((_, rest)) = l.split_once("] ") else {
            continue;
        };
        if let Some(r) = rest.strip_prefix(&format!("reply {cmd_key} ")) {
            run.replies.push(match r.strip_prefix("error ") {
                Some(e) => Err(e.to_string()),
                None => Ok(unhex(r)),
            });
        } else if let Some(r) = rest.strip_prefix(&format!("publish {state_key} 0x")) {
            run.reports.push(unhex(r));
        }
    }
    run
}

#[test]
fn p1_every_cell_of_the_phase_model_matches_recipe_core() {
    let mut cells = 0;
    for target in PhaseState::ALL {
        let path = path_to(target);
        for input in INPUTS {
            let mut stdin = String::new();
            let mut id = 0;
            let mut last_applied = 0;
            let mut state = PhaseState::Idle;
            for &step in &path {
                if let In::Cmd(_) = step {
                    id += 1;
                    last_applied = id;
                }
                stdin.push_str(&line(step, id));
                stdin.push('\n');
                state = next(state, step).expect("on the path");
            }
            assert_eq!(state, target);
            let before = sim(&stdin);
            // The reports so far end in the target state.
            let r = wire::decode_report(before.reports.last().expect("a report")).expect("report");
            assert_eq!((r.state, r.last_id), (target, last_applied), "{path:?}");

            if let In::Cmd(_) = input {
                id += 1;
            }
            stdin.push_str(&line(input, id));
            stdin.push('\n');
            let after = sim(&stdin);
            let expected = next(target, input);
            let new_reports = &after.reports[before.reports.len()..];
            match (input, expected) {
                (In::Cmd(_), _) => {
                    let ack = after.replies.last().expect("a reply").clone().expect("ack");
                    let want = match expected {
                        Some(to) => (id, Ack::Accepted(to)),
                        None => (id, Ack::Rejected(target)),
                    };
                    assert_eq!(wire::decode_ack(&ack), Ok(want), "{target:?} {input:?}");
                    // Byte for byte recipe-core's acknowledgement.
                    let mut buf = [0u8; wire::ACK_LEN];
                    let (acc, st) = match want.1 {
                        Ack::Accepted(s) => (true, s),
                        Ack::Rejected(s) => (false, s),
                        Ack::Failed(_) => unreachable!(),
                    };
                    assert_eq!(ack, wire::encode_ack(acc, st, id, &mut buf).unwrap());
                }
                (In::Done, _) => {}
            }
            match expected {
                Some(to) => {
                    let last_id = if matches!(input, In::Cmd(_)) {
                        id
                    } else {
                        last_applied
                    };
                    let mut buf = [0u8; wire::MAX_BODY];
                    let want = wire::encode_report(to, last_id, &[], &mut buf).unwrap();
                    assert_eq!(new_reports, [want.to_vec()], "{target:?} {input:?}");
                }
                None => assert_eq!(new_reports, Vec::<Vec<u8>>::new(), "{target:?} {input:?}"),
            }
            cells += 1;
        }
    }
    assert_eq!(cells, 77);
}

#[test]
fn p2_start_arguments_and_malformed_bodies_are_invalid_requests() {
    let k = key(SIM_NODE, "X", "P", false);
    let mut buf = [0u8; wire::MAX_BODY];
    let out_of_range = hex(wire::encode_command(Command::Start, 1, &[11], &mut buf).unwrap());
    let mut buf = [0u8; wire::MAX_BODY];
    let two_args = hex(wire::encode_command(Command::Start, 2, &[1, 2], &mut buf).unwrap());
    let mut buf = [0u8; wire::MAX_BODY];
    let no_args = hex(wire::encode_command(Command::Start, 3, &[], &mut buf).unwrap());
    let hold_with_arg = "01020400000001ff000000";
    let version2 = "02010500000000";
    let short = "010106";
    let unknown_code = "01070700000000";
    let mut buf = [0u8; wire::MAX_BODY];
    let ok = hex(wire::encode_command(Command::Start, 8, &[0], &mut buf).unwrap());
    let stdin: String = [
        &out_of_range,
        &two_args,
        &no_args,
        hold_with_arg,
        version2,
        short,
        unknown_code,
        &ok,
    ]
    .iter()
    .map(|b| format!("!req {k} {b}\n"))
    .collect();
    let run = sim(&stdin);
    let invalid = Err("InvalidRequest".to_string());
    assert_eq!(&run.replies[..7], vec![invalid; 7].as_slice());
    assert_eq!(
        wire::decode_ack(run.replies[7].as_ref().unwrap()),
        Ok((8, Ack::Accepted(PhaseState::Running)))
    );
    // Nothing changed before the valid Start: Idle, then Running.
    let states: Vec<_> = run
        .reports
        .iter()
        .map(|r| wire::decode_report(r).unwrap().state)
        .collect();
    assert_eq!(states, [PhaseState::Idle, PhaseState::Running]);
}

// ---- the worked example end to end

/// A board for one unit payload: a clock, an analog reading, and the
/// publications and LED levels it saw.
struct Board {
    node: SenderId,
    now: u32,
    adc: u16,
    outbox: Vec<(String, Vec<u8>)>,
    led: Vec<bool>,
    logs: Vec<String>,
}

impl Device for Board {
    fn log(&mut self, line: &str) {
        self.logs.push(line.to_string());
    }
    fn publish(&mut self, key: Key<'_>, data: &[u8]) -> Result<(), AbiError> {
        self.outbox.push((key.as_str().to_string(), data.to_vec()));
        Ok(())
    }
    fn now_ms(&mut self) -> u32 {
        self.now
    }
    fn gpio_output(&mut self, _: u8) -> Result<(), AbiError> {
        Ok(())
    }
    fn gpio_input(&mut self, _: u8, _: Pull) -> Result<(), AbiError> {
        Ok(())
    }
    fn gpio_write(&mut self, _: u8, high: bool) -> Result<(), AbiError> {
        self.led.push(high);
        Ok(())
    }
    fn gpio_read(&mut self, _: u8) -> Result<bool, AbiError> {
        Ok(false)
    }
    fn adc_read(&mut self, _: u8) -> Result<u16, AbiError> {
        Ok(self.adc)
    }
    fn serve(&mut self, _: Key<'_>) -> Result<(), AbiError> {
        Ok(())
    }
    fn node_id(&mut self) -> Result<SenderId, AbiError> {
        Ok(self.node)
    }
}

const NODES: [[u8; 8]; 2] = [
    [0x24, 0x6f, 0x28, 1, 2, 3, 0, 0],
    [0xe6, 0x61, 0, 0, 0, 9, 0, 0],
];
const PHASES: [(&str, &str); 2] = [("LED", "Blink"), ("ADC", "Measure")];

const BLINK_P: [ParamDef<'static>; 2] = [
    ParamDef {
        name: "period_ms",
        min: 10,
        max: 10_000,
    },
    ParamDef {
        name: "times",
        min: 1,
        max: 100,
    },
];
const MEASURE_P: [ParamDef<'static>; 1] = [ParamDef {
    name: "samples",
    min: 1,
    max: 16,
}];
const CLASSES: [PhaseClass<'static>; 2] = [
    PhaseClass {
        name: "Blink",
        params: &BLINK_P,
    },
    PhaseClass {
        name: "Measure",
        params: &MEASURE_P,
    },
];
const UPS: [UnitProc<'static>; 2] = [
    UnitProc {
        id: "Signal",
        unit_class: None,
    },
    UnitProc {
        id: "Sense",
        unit_class: Some("SensorUnit"),
    },
];
const B_ARGS: [Arg<'static>; 2] = [
    Arg {
        name: "period_ms",
        value: 250,
    },
    Arg {
        name: "times",
        value: 3,
    },
];
const M_ARGS: [Arg<'static>; 1] = [Arg {
    name: "samples",
    value: 4,
}];
const STEPS: [Step<'static>; 2] = [
    Step {
        id: "BlinkLed",
        unit_proc: "Signal",
        class: "Blink",
        args: &B_ARGS,
    },
    Step {
        id: "TakeReading",
        unit_proc: "Sense",
        class: "Measure",
        args: &M_ARGS,
    },
];
const TRANS: [Transition<'static>; 1] = [Transition {
    from: &["BlinkLed"],
    to: &["TakeReading"],
}];
const RECIPE: Recipe<'static> = Recipe {
    id: "BlinkThenMeasure",
    unit_procs: &UPS,
    steps: &STEPS,
    transitions: &TRANS,
};
const LED_PHASES: [UnitPhase<'static>; 1] = [UnitPhase {
    em: "LED",
    name: "Blink",
    class: "Blink",
    ranges: &[(50, 1_000), (1, 10)],
}];
const ADC_PHASES: [UnitPhase<'static>; 1] = [UnitPhase {
    em: "ADC",
    name: "Measure",
    class: "Measure",
    ranges: &[(1, 8)],
}];

/// The plant: both unit payloads, the engine, the clock and what
/// happened.
struct Plant {
    units: Vec<Payload<Board>>,
    online: [bool; 2],
    sfc: Sfc,
    engine: Option<Engine>,
    now: u64,
    /// Every engine effect with its time.
    trace: Vec<(u64, Effect)>,
    /// Requests the units answered (unit, command, reply).
    served: Vec<(usize, Command, Result<usize, HandlerError>)>,
    /// The latest report values per unit.
    values: [Vec<i32>; 2],
}

impl Plant {
    fn new() -> Plant {
        let units = ["pico-led", "esp-sense"]
            .iter()
            .zip(NODES)
            .map(|(name, node)| {
                let wasm = std::fs::read(fixture(name)).expect("fixture");
                let board = Board {
                    node: SenderId(node),
                    now: 0,
                    adc: 2048,
                    outbox: Vec::new(),
                    led: Vec::new(),
                    logs: Vec::new(),
                };
                Payload::load(&wasm, board, Limits::default()).expect("loads")
            })
            .collect();
        let sfc = compile(&RECIPE, &CLASSES).expect("compiles");
        let alloc_units = [
            Unit {
                id: "pico-led",
                class: "LedUnit",
                online: true,
                phases: &LED_PHASES,
            },
            Unit {
                id: "esp-sense",
                class: "SensorUnit",
                online: true,
                phases: &ADC_PHASES,
            },
        ];
        let ups = [UPS[0].unit_class, UPS[1].unit_class];
        let a = allocate(&sfc, &CLASSES, &ups, &alloc_units).expect("allocates");
        let engine = Engine::new(sfc, &a, 2, Config::DEFAULT).expect("engine");
        let mut p = Plant {
            units,
            online: [true, true],
            sfc,
            engine: Some(engine),
            now: 0,
            trace: Vec::new(),
            served: Vec::new(),
            values: [Vec::new(), Vec::new()],
        };
        p.reports();
        p
    }

    fn state(&self) -> PhaseState {
        self.engine.as_ref().expect("engine").state()
    }

    /// Feeds one input and handles its effects (sending commands), up to
    /// a bound of follow-up inputs.
    fn input(&mut self, input: Input) {
        let mut todo = VecDeque::from([input]);
        for _ in 0..64 {
            let Some(i) = todo.pop_front() else { return };
            let (e, fx) = self.engine.take().expect("engine").on(self.now, i);
            self.engine = Some(e);
            for f in fx.iter().copied() {
                self.trace.push((self.now, f));
                if let Effect::Send {
                    step,
                    phase,
                    command,
                    id,
                    timeout_ms,
                } = f
                    && let Some(result) = self.send(step, phase, command, id, timeout_ms)
                {
                    todo.push_back(Input::Ack { id, result });
                }
            }
            for r in self.collect_reports() {
                todo.push_back(r);
            }
        }
        panic!("inputs did not settle");
    }

    /// Sends a command to its unit's payload; `None` when the unit is
    /// offline (the request is lost; the engine's deadline notices).
    fn send(
        &mut self,
        step: u8,
        p: PhaseRef,
        command: Command,
        id: u32,
        timeout_ms: u32,
    ) -> Option<Ack> {
        let u = usize::from(p.unit);
        if !self.online[u] {
            return None;
        }
        let s = usize::from(step);
        let n = CLASSES[self.sfc.step_of(s).expect("step").0].params.len();
        let args: &[i32] = if command == Command::Start {
            &self.sfc.args(s).expect("args")[..n]
        } else {
            &[]
        };
        let mut buf = [0u8; wire::MAX_BODY];
        let body = wire::encode_command(command, id, args, &mut buf).expect("body");
        let (em, ph) = PHASES[u];
        let k = key(NODES[u], em, ph, false);
        let meta = RequestMeta::NONE.with_deadline_ms(timeout_ms);
        let mut reply = [0u8; 64];
        let r = self.units[u]
            .request(Key::new(&k).expect("key"), body, &meta, &mut reply)
            .expect("no trap");
        self.served.push((u, command, r));
        Some(match r {
            Ok(n) => wire::decode_ack(&reply[..n]).expect("ack").1,
            Err(HandlerError::NotFound) => Ack::Failed(AckError::NotFound),
            Err(HandlerError::InvalidRequest) => Ack::Failed(AckError::InvalidRequest),
            Err(HandlerError::Unavailable) => Ack::Failed(AckError::Unavailable),
            Err(HandlerError::DeadlineExceeded) => Ack::Failed(AckError::DeadlineExceeded),
            Err(_) => Ack::Failed(AckError::Transport),
        })
    }

    /// The units' published reports as engine inputs (an offline unit's
    /// are lost).
    fn collect_reports(&mut self) -> Vec<Input> {
        let mut out = Vec::new();
        for (u, node) in NODES.iter().enumerate() {
            let outbox = std::mem::take(&mut self.units[u].device().outbox);
            if !self.online[u] {
                continue;
            }
            for (k, body) in outbox {
                let pk = wire::parse_key(&k).expect("a phase key");
                assert!(pk.state && pk.node == *node, "{k}");
                let r = wire::decode_report(&body).expect("report");
                self.values[u] = r.values[..r.n].to_vec();
                out.push(Input::Report {
                    phase: PhaseRef {
                        unit: u as u8,
                        phase: 0,
                    },
                    state: r.state,
                    last_id: r.last_id,
                });
            }
        }
        out
    }

    fn reports(&mut self) {
        for i in self.collect_reports() {
            self.input(i);
        }
    }

    /// Runs the clock to `until` in 10 ms steps: each unit's due tick,
    /// then the engine's tick and the reports.
    fn run_to(&mut self, until: u64) {
        while self.now < until {
            self.now += 10;
            for u in &mut self.units {
                u.device().now = self.now as u32;
                if u.next_tick().is_some_and(|due| due <= self.now as u32) {
                    u.tick(self.now as u32).expect("no trap");
                }
            }
            self.input(Input::Tick);
            self.reports();
        }
    }

    fn recipe_states(&self) -> Vec<PhaseState> {
        self.trace
            .iter()
            .filter_map(|(_, e)| match e {
                Effect::Recipe { to, .. } => Some(*to),
                _ => None,
            })
            .collect()
    }
}

#[test]
fn p3_blink_then_measure_runs_to_complete_on_the_generated_units() {
    let mut p = Plant::new();
    assert_eq!(p.state(), PhaseState::Idle);
    p.input(Input::Operator(Command::Start));
    p.run_to(3_000);
    assert_eq!(p.state(), PhaseState::Complete, "{:#?}", p.trace);
    assert_eq!(
        p.recipe_states(),
        [PhaseState::Running, PhaseState::Complete]
    );
    // Blink toggled its LED three times (250 ms apart) and was reset to
    // off; Measure took four samples of the potentiometer.
    let led = &p.units[0].device().led;
    assert_eq!(led.as_slice(), [false, true, false, true, false]);
    assert_eq!(p.values[1], [2048, 4]);
    // Commands per unit: Start then Reset, all accepted.
    let cmds: Vec<_> = p
        .served
        .iter()
        .map(|(u, c, r)| (*u, *c, r.is_ok()))
        .collect();
    assert_eq!(
        cmds,
        [
            (0, Command::Start, true),
            (0, Command::Reset, true),
            (1, Command::Start, true),
            (1, Command::Reset, true),
        ]
    );
    // Measure started only after Blink completed.
    let start_of = |u: u8| {
        p.trace
            .iter()
            .find(|(_, e)| matches!(e, Effect::Send { phase, command: Command::Start, .. } if phase.unit == u))
            .map(|(t, _)| *t)
            .expect("started")
    };
    assert!(start_of(1) >= 500, "Measure at {}", start_of(1));
    assert!(
        p.units[1]
            .device()
            .logs
            .iter()
            .any(|l| l == "phase ADC.Measure: Complete")
    );
}

#[test]
fn p4_a_hold_mid_blink_holds_the_batch_and_restart_finishes_it() {
    let mut p = Plant::new();
    p.input(Input::Operator(Command::Start));
    p.run_to(300);
    p.input(Input::Operator(Command::Hold));
    p.run_to(1_000);
    assert_eq!(p.state(), PhaseState::Held);
    // Nothing new started while held.
    assert!(!p.served.iter().any(|(u, _, _)| *u == 1));
    p.input(Input::Operator(Command::Restart));
    p.run_to(4_000);
    assert_eq!(p.state(), PhaseState::Complete, "{:#?}", p.trace);
    assert_eq!(
        p.recipe_states(),
        [
            PhaseState::Running,
            PhaseState::Holding,
            PhaseState::Held,
            PhaseState::Restarting,
            PhaseState::Running,
            PhaseState::Complete
        ]
    );
    let blink: Vec<_> = p
        .served
        .iter()
        .filter(|(u, _, _)| *u == 0)
        .map(|(_, c, _)| *c)
        .collect();
    assert_eq!(
        blink,
        [
            Command::Start,
            Command::Hold,
            Command::Restart,
            Command::Reset
        ]
    );
}

#[test]
fn p5_losing_the_led_unit_holds_restart_is_refused_and_abort_ends_aborted() {
    let mut p = Plant::new();
    p.input(Input::Operator(Command::Start));
    p.run_to(100);
    p.online[0] = false;
    p.input(Input::UnitLost(0));
    p.run_to(200);
    p.input(Input::Operator(Command::Restart));
    p.run_to(300);
    p.input(Input::Operator(Command::Abort));
    p.run_to(2_000);
    assert!(p.trace.iter().any(|(_, e)| *e
        == Effect::Failed {
            phase: None,
            failure: Failure::UnitLost(0)
        }));
    assert!(p.trace.iter().any(|(_, e)| *e
        == Effect::Refused {
            command: Command::Restart,
            reason: Refusal::UnitOffline(0)
        }));
    assert_eq!(p.state(), PhaseState::Aborted, "{:#?}", p.trace);
    // The lost unit was sent nothing after it went.
    assert!(
        !p.trace
            .iter()
            .any(|(t, e)| *t >= 100 && matches!(e, Effect::Send { phase, .. } if phase.unit == 0))
    );
    // Its payload still runs Blink by itself: the batch is held, the
    // equipment is not.
    assert!(
        p.units[0]
            .device()
            .logs
            .iter()
            .any(|l| l == "phase LED.Blink: Complete")
    );
}
