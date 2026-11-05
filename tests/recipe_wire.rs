//! The S88 phase command contract (`recipe_core::wire`) on the middleware
//! 0.5 traits, over the in-process backend: the control-recipe engine's
//! commands go out with `Requester::request_with` to
//! `unit/<node>/phase/<em>.<phase>` with a timeout and `Target::Node`,
//! each unit answers through a `Handler` built on `wire::serve` (and sees
//! the deadline in `RequestMeta`), and state reports come back over
//! pub/sub on `.../state`. The worked example "Blink then Measure" runs to
//! Complete; a command to the wrong node fails and holds the batch.

use std::cell::Cell;
use std::time::Duration;

use middleware::key::{Key, KeyFilter};
use middleware::transport::memory::{
    MemoryBus, MemoryError, MemoryNode, MemoryResponder, MemorySubscriber,
};
use middleware::transport::{
    Publisher, RequestOptions, Requester, Responder, Subscriber, Target, handler_fn,
};
use middleware::{HandlerError, SenderId};
use recipe_core::alloc::{PhaseRef, Unit, UnitPhase, allocate};
use recipe_core::engine::{Ack, AckError, Config, Effect, Engine, Input};
use recipe_core::phase::{self, PhaseState};
use recipe_core::recipe::{Arg, ParamDef, PhaseClass, Recipe, Step, Transition, UnitProc, compile};
use recipe_core::{Sfc, wire};

const COORD: SenderId = SenderId([0xC0; 8]);
const NODES: [SenderId; 2] = [
    SenderId([0x24, 0x6f, 0x28, 1, 2, 3, 0, 0]),
    SenderId([0xe6, 0x61, 0, 0, 0, 9, 0, 0]),
];
/// Unit 0 has LED.Blink, unit 1 ADC.Measure.
const PHASES: [(&str, &str); 2] = [("LED", "Blink"), ("ADC", "Measure")];
const RANGES: [&[(i32, i32)]; 2] = [&[(50, 1_000), (1, 10)], &[(1, 8)]];

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

fn units() -> [Unit<'static>; 2] {
    const LED: [UnitPhase<'static>; 1] = [UnitPhase {
        em: "LED",
        name: "Blink",
        class: "Blink",
        ranges: &[(50, 1_000), (1, 10)],
    }];
    const ADC: [UnitPhase<'static>; 1] = [UnitPhase {
        em: "ADC",
        name: "Measure",
        class: "Measure",
        ranges: &[(1, 8)],
    }];
    [
        Unit {
            id: "pico-led",
            class: "LedUnit",
            online: true,
            phases: &LED,
        },
        Unit {
            id: "pico-sense",
            class: "SensorUnit",
            online: true,
            phases: &ADC,
        },
    ]
}

/// One unit: its node, its phase's responder and state.
struct UnitNode {
    node: MemoryNode,
    responder: MemoryResponder,
    state: Cell<(PhaseState, u32)>,
    deadline_seen: Cell<Option<u32>>,
}

fn key_for(unit: usize, state: bool) -> String {
    let mut buf = [0u8; wire::MAX_KEY];
    let (em, ph) = PHASES[unit];
    let k = if state {
        wire::state_key(NODES[unit].0, em, ph, &mut buf)
    } else {
        wire::command_key(NODES[unit].0, em, ph, &mut buf)
    };
    k.unwrap().to_string()
}

fn unit_node(bus: &MemoryBus, unit: usize) -> UnitNode {
    let node = bus.node(NODES[unit]);
    let key = key_for(unit, false);
    let responder = node
        .responder(KeyFilter::Exact(Key::new(&key).unwrap()))
        .unwrap();
    UnitNode {
        node,
        responder,
        state: Cell::new((PhaseState::Idle, 0)),
        deadline_seen: Cell::new(None),
    }
}

/// The unit side of one request: `wire::serve` in a `Handler`.
async fn serve(u: &mut UnitNode, unit: usize) {
    let state = &u.state;
    let seen = &u.deadline_seen;
    let mut h = handler_fn(|req, out| {
        seen.set(req.meta.deadline_ms);
        let (now, _) = state.get();
        let s =
            wire::serve(now, RANGES[unit], req.body).map_err(|_| HandlerError::InvalidRequest)?;
        if s.accepted {
            state.set((s.state, s.command.id));
        }
        let ack = wire::encode_ack(s.accepted, s.state, s.command.id, out)
            .map_err(|_| HandlerError::BufferTooSmall)?;
        Ok(ack.len())
    });
    let mut scratch = [0u8; 64];
    u.responder.serve_one(&mut h, &mut scratch).await.unwrap();
}

/// The unit finishes its acting state and publishes a report.
async fn finish(u: &mut UnitNode, unit: usize) {
    let (s, id) = u.state.get();
    let (to, _) = phase::step(s, phase::Input::Done);
    u.state.set((to, id));
    let mut buf = [0u8; wire::MAX_BODY];
    let body = wire::encode_report(to, id, &[], &mut buf).unwrap();
    let key = key_for(unit, true);
    u.node.publish(Key::new(&key).unwrap(), body).await.unwrap();
}

/// The report the coordinator receives, as an engine input.
fn report(sub: &mut MemorySubscriber) -> Option<Input> {
    let mut buf = [0u8; 128];
    let sample = sub.try_recv(&mut buf).unwrap()?;
    let k = wire::parse_key(sample.key.as_str()).unwrap();
    assert!(k.state);
    let unit = NODES.iter().position(|n| n.0 == k.node).unwrap() as u8;
    let r = wire::decode_report(sample.payload).unwrap();
    Some(Input::Report {
        phase: PhaseRef { unit, phase: 0 },
        state: r.state,
        last_id: r.last_id,
    })
}

fn ack_of(result: Result<&[u8], MemoryError>) -> Ack {
    match result {
        Ok(body) => wire::decode_ack(body).unwrap().1,
        Err(MemoryError::Handler(HandlerError::NotFound)) => Ack::Failed(AckError::NotFound),
        Err(MemoryError::Handler(HandlerError::InvalidRequest)) => {
            Ack::Failed(AckError::InvalidRequest)
        }
        Err(MemoryError::Handler(HandlerError::Unavailable)) => Ack::Failed(AckError::Unavailable),
        Err(MemoryError::Handler(HandlerError::DeadlineExceeded)) => {
            Ack::Failed(AckError::DeadlineExceeded)
        }
        Err(_) => Ack::Failed(AckError::Transport),
    }
}

fn command_body<'b>(
    sfc: &Sfc,
    step: u8,
    command: recipe_core::Command,
    id: u32,
    buf: &'b mut [u8],
) -> &'b [u8] {
    let s = usize::from(step);
    let (class, _) = sfc.step_of(s).unwrap();
    let n = CLASSES[class].params.len();
    let args: &[i32] = if command == recipe_core::Command::Start {
        &sfc.args(s).unwrap()[..n]
    } else {
        &[]
    };
    wire::encode_command(command, id, args, buf).unwrap()
}

/// Sends every `Send` effect over the bus (served by its unit) and feeds
/// the replies back; returns the engine and the `Send`s it made.
async fn pump(
    mut e: Engine,
    sfc: &Sfc,
    coord: &mut MemoryNode,
    unit_nodes: &mut [UnitNode; 2],
    first: Vec<Effect>,
    target_override: Option<SenderId>,
) -> (Engine, Vec<Effect>) {
    let mut todo = first;
    let mut sent = Vec::new();
    for _ in 0..16 {
        let Some(fx) = todo.pop() else { break };
        let Effect::Send {
            step,
            phase,
            command,
            id,
            timeout_ms,
        } = fx
        else {
            continue;
        };
        sent.push(fx);
        let unit = usize::from(phase.unit);
        let key = key_for(unit, false);
        let mut body_buf = [0u8; wire::MAX_BODY];
        let body = command_body(sfc, step, command, id, &mut body_buf);
        let node = target_override.unwrap_or(NODES[unit]);
        let opts = RequestOptions::DEFAULT
            .with_timeout(Duration::from_millis(u64::from(timeout_ms)))
            .with_target(Target::Node(node));
        let mut resp = [0u8; wire::ACK_LEN];
        let result = if target_override.is_some() {
            ack_of(
                coord
                    .request_with(Key::new(&key).unwrap(), body, &mut resp, &opts)
                    .await,
            )
        } else {
            let (got, ()) = tokio::join!(
                coord.request_with(Key::new(&key).unwrap(), body, &mut resp, &opts),
                serve(&mut unit_nodes[unit], unit)
            );
            ack_of(got)
        };
        let (next, out) = e.on(1, Input::Ack { id, result });
        e = next;
        todo.extend(out.iter().copied());
    }
    (e, sent)
}

fn plan() -> (Sfc, Engine) {
    let sfc = compile(&RECIPE, &CLASSES).unwrap();
    let ups = [UPS[0].unit_class, UPS[1].unit_class];
    let a = allocate(&sfc, &CLASSES, &ups, &units()).unwrap();
    (sfc, Engine::new(sfc, &a, 2, Config::DEFAULT).unwrap())
}

#[tokio::test]
async fn the_worked_example_runs_to_complete_over_request_with_and_pub_sub() {
    let bus = MemoryBus::new();
    let mut coord = bus.node(COORD);
    let mut sub = coord
        .subscriber(KeyFilter::Prefix(Key::from_static("unit")))
        .unwrap();
    let mut u = [unit_node(&bus, 0), unit_node(&bus, 1)];
    let (sfc, e) = plan();

    let (e, fx) = e.on(0, Input::Operator(recipe_core::Command::Start));
    let (mut e, sent) = pump(
        e,
        &sfc,
        &mut coord,
        &mut u,
        fx.iter().copied().collect(),
        None,
    )
    .await;
    assert_eq!(sent.len(), 1);
    assert_eq!(u[0].state.get(), (PhaseState::Running, 1));
    // The unit saw the engine's timeout as its deadline.
    assert!(
        u[0].deadline_seen
            .get()
            .is_some_and(|d| d > 0 && d <= 1_000)
    );

    // Each unit finishes what it is doing until the batch completes;
    // every report goes back through the engine and its commands out
    // through the bus.
    for _ in 0..8 {
        if e.state() == PhaseState::Complete {
            break;
        }
        for (i, unit) in u.iter_mut().enumerate() {
            if unit.state.get().0.is_acting() {
                finish(unit, i).await;
            }
        }
        while let Some(input) = report(&mut sub) {
            let (next, fx) = e.on(2, input);
            let (next, _) = pump(
                next,
                &sfc,
                &mut coord,
                &mut u,
                fx.iter().copied().collect(),
                None,
            )
            .await;
            e = next;
        }
    }
    assert_eq!(e.state(), PhaseState::Complete);
    assert_eq!(u[0].state.get().0, PhaseState::Idle);
    assert_eq!(u[1].state.get().0, PhaseState::Idle);
}

#[tokio::test]
async fn a_command_addressed_to_another_node_fails_and_holds_the_batch() {
    let bus = MemoryBus::new();
    let mut coord = bus.node(COORD);
    let mut u = [unit_node(&bus, 0), unit_node(&bus, 1)];
    let (sfc, e) = plan();
    let (e, fx) = e.on(0, Input::Operator(recipe_core::Command::Start));
    // Target::Node(other) on the LED unit's key: no responder there.
    let (e, _) = pump(
        e,
        &sfc,
        &mut coord,
        &mut u,
        fx.iter().copied().collect(),
        Some(NODES[1]),
    )
    .await;
    assert_eq!(e.state(), PhaseState::Held);
    assert_eq!(u[0].state.get().0, PhaseState::Idle);
}

#[tokio::test]
async fn a_start_outside_the_phase_range_is_an_invalid_request_that_aborts() {
    let bus = MemoryBus::new();
    let mut coord = bus.node(COORD);
    let mut u = [unit_node(&bus, 0), unit_node(&bus, 1)];
    let key = key_for(0, false);
    let mut buf = [0u8; wire::MAX_BODY];
    let body = wire::encode_command(recipe_core::Command::Start, 1, &[20, 3], &mut buf).unwrap();
    let opts = RequestOptions::DEFAULT.with_target(Target::Node(NODES[0]));
    let mut resp = [0u8; wire::ACK_LEN];
    let (got, ()) = tokio::join!(
        coord.request_with(Key::new(&key).unwrap(), body, &mut resp, &opts),
        serve(&mut u[0], 0)
    );
    assert_eq!(ack_of(got), Ack::Failed(AckError::InvalidRequest));
    assert_eq!(u[0].state.get().0, PhaseState::Idle);
    let (_, e) = plan();
    let (e, _) = e.on(0, Input::Operator(recipe_core::Command::Start));
    let (e, _) = e.on(
        1,
        Input::Ack {
            id: 1,
            result: Ack::Failed(AckError::InvalidRequest),
        },
    );
    assert_eq!(e.state(), PhaseState::Aborted);
}
