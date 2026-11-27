//! Multi-rate signal processing: the signal blocks, node periods, the one
//! update order with `delay` as the only way round a loop, and traces.
//!
//! The reference is [`wire_core::Sim`]. The compiled payload runs in the
//! device runtime (`wasm-host`) against a fake board fed the same
//! pseudo-random pins, and must do exactly what the reference does: the
//! same pin writes, PWM duties and publications, in the same order, on
//! every tick. Tracing every channel makes every value visible that way.

use std::collections::BTreeMap;

use middleware::Key;
use wasm_host::{AbiError, Device, Limits, Payload, Pull as HostPull, PwmSpec, Ticked};
use wire_core::graph::{Class, Config, Graph, Kind, Node};
use wire_core::spec::{KINDS, kinds_json};
use wire_core::{Io, Plan, Severity, Sim, build, check, text};

/// What a wiring did to the board.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Event {
    Gpio(u8, bool),
    Pwm(u8, u8, u16),
    Publish(String, i32),
}

/// A board for both sides: the payload's [`Device`] and the reference's
/// [`Io`]. A negative analog value is a read error.
#[derive(Default)]
struct Board {
    levels: BTreeMap<u8, bool>,
    analog: BTreeMap<u8, i32>,
    refused: Vec<u8>,
    refused_timers: Vec<u8>,
    events: Vec<Event>,
    logs: Vec<String>,
    opened: Vec<(u8, u32)>,
    reads: Vec<u8>,
}

impl Device for Board {
    fn log(&mut self, line: &str) {
        self.logs.push(line.to_string());
    }
    fn publish(&mut self, key: Key<'_>, data: &[u8]) -> Result<(), AbiError> {
        let name = key.as_str().strip_prefix("wire/").expect("a trace key is wire/<channel>");
        let value = i32::from_le_bytes(data.try_into().expect("a trace is 4 bytes"));
        self.events.push(Event::Publish(name.to_string(), value));
        Ok(())
    }
    fn now_ms(&mut self) -> u32 {
        0
    }
    fn gpio_output(&mut self, pin: u8) -> Result<(), AbiError> {
        if self.refused.contains(&pin) { Err(AbiError::Unsupported) } else { Ok(()) }
    }
    fn gpio_input(&mut self, pin: u8, _pull: HostPull) -> Result<(), AbiError> {
        if self.refused.contains(&pin) { Err(AbiError::Unsupported) } else { Ok(()) }
    }
    fn gpio_write(&mut self, pin: u8, high: bool) -> Result<(), AbiError> {
        self.events.push(Event::Gpio(pin, high));
        Ok(())
    }
    fn gpio_read(&mut self, pin: u8) -> Result<bool, AbiError> {
        self.reads.push(pin);
        Ok(self.levels.get(&pin).copied().unwrap_or(false))
    }
    fn pwm_open(&mut self, spec: PwmSpec) -> Result<(), AbiError> {
        if self.refused_timers.contains(&spec.timer) {
            return Err(AbiError::Unsupported);
        }
        self.opened.push((spec.timer, spec.freq_hz));
        Ok(())
    }
    fn pwm_set(&mut self, timer: u8, channel: u8, duty: u16) -> Result<(), AbiError> {
        self.events.push(Event::Pwm(timer, channel, duty));
        Ok(())
    }
    fn adc_read(&mut self, pin: u8) -> Result<u16, AbiError> {
        self.reads.push(pin);
        match self.analog.get(&pin).copied().unwrap_or(0) {
            v if v < 0 => Err(AbiError::Unsupported),
            v => Ok(v as u16),
        }
    }
}

impl Io for Board {
    fn gpio_read(&mut self, pin: u8) -> bool {
        self.reads.push(pin);
        self.levels.get(&pin).copied().unwrap_or(false)
    }
    fn adc_read(&mut self, pin: u8) -> i32 {
        self.reads.push(pin);
        self.analog.get(&pin).copied().unwrap_or(0)
    }
    fn gpio_write(&mut self, pin: u8, high: bool) {
        self.events.push(Event::Gpio(pin, high));
    }
    fn pwm_set(&mut self, timer: u8, channel: u8, duty: u16) {
        self.events.push(Event::Pwm(timer, channel, duty));
    }
    fn publish(&mut self, channel: &str, value: i32) {
        self.events.push(Event::Publish(channel.to_string(), value));
    }
}

/// A small linear congruential generator (Numerical Recipes' constants).
struct Lcg(u32);

impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        self.0 >> 8
    }
}

fn wiring(period: u32, statements: &[&str]) -> Graph {
    text::parse("w", period, statements).unwrap_or_else(|e| panic!("{e}"))
}

fn errors(graph: &Graph, chip: Option<&str>) -> Vec<String> {
    let pins = chip.map(|c| board_pins::boards::by_chip(c).expect("a known chip"));
    check(graph, pins)
        .into_iter()
        .filter(|i| i.severity == Severity::Error)
        .map(|i| i.message)
        .collect()
}

fn load(graph: &Graph, board: Board) -> Payload<Board> {
    let (wasm, _) = build(graph, None).unwrap_or_else(|issues| panic!("{issues:#?}"));
    Payload::load(&wasm, board, Limits::default()).expect("the runtime loads the payload")
}

/// Runs the payload and the reference side by side for `ticks` base ticks
/// (tick 0 is the payload's `init`), setting both boards' pins with
/// `stimulus(tick, board)` before each, and compares what they did after
/// every tick. Returns everything the reference did, per tick.
fn compare(
    graph: &Graph,
    ticks: u32,
    mut stimulus: impl FnMut(u32, &mut Board),
) -> Vec<Vec<Event>> {
    let plan = Plan::new(graph).unwrap_or_else(|issues| panic!("{issues:#?}"));
    let mut reference = Board::default();
    let mut sim = Sim::new(&plan);
    let mut first = Board::default();
    stimulus(0, &mut first);
    let mut payload = load(graph, first);
    let mut history = Vec::new();
    for tick in 0..ticks {
        stimulus(tick, &mut reference);
        if tick > 0 {
            stimulus(tick, payload.device());
            let ran = payload.tick(tick * graph.period_ms).expect("the tick does not trap");
            assert_eq!(ran, Ticked::Ran, "tick {tick}");
        }
        sim.step(&mut reference);
        let want = std::mem::take(&mut reference.events);
        let got = std::mem::take(&mut payload.device().events);
        assert_eq!(got, want, "tick {tick} of {:?}", text::format(graph));
        assert_eq!(
            std::mem::take(&mut payload.device().reads),
            std::mem::take(&mut reference.reads),
            "pins read on tick {tick}"
        );
        history.push(want);
    }
    assert_eq!(sim.tick(), ticks);
    history
}

/// Pseudo-random pins: GPIO 1 to 3 as bits and ADC 26, 27 anywhere in the
/// 12-bit range, sometimes an extreme and sometimes a read error.
fn noise(seed: u32) -> impl FnMut(u32, &mut Board) {
    let mut lcg = Lcg(seed);
    let mut rows: Vec<[u32; 5]> = Vec::new();
    move |tick, board| {
        // Both boards ask for each tick, in order: draw it once.
        while rows.len() <= tick as usize {
            rows.push([lcg.next(), lcg.next(), lcg.next(), lcg.next(), lcg.next()]);
        }
        let row = rows[tick as usize];
        for pin in 1..=3u8 {
            board.levels.insert(pin, row[usize::from(pin) - 1] >> 23 == 1);
        }
        for (pin, r) in [(26u8, row[3]), (27, row[4])] {
            let value = match r >> 20 {
                0 => 0,
                1 => 4095,
                2 => -1,
                _ => ((r >> 4) % 4096) as i32,
            };
            board.analog.insert(pin, value);
        }
    }
}

/// Channels to feed a block under test: bits, 12-bit readings, numbers
/// across the whole `i32` range (a reading times a large odd constant,
/// truncated), small signed numbers, and a mix that jumps between the
/// most negative and the most positive value.
const POOL: [&str; 14] = [
    "b1=gpio1",
    "b2=gpio2",
    "b3=gpio3",
    "r1=adc26",
    "r2=adc27",
    "wide1=scale(r1,mul=1048583)",
    "wide2=scale(r2,mul=-2097169)",
    "half=2048",
    "small1=sub(r1,half)",
    "small2=sub(r2,half)",
    "lowest=-2147483648",
    "highest=2147483647",
    "extreme=select(b1,lowest,highest)",
    "mix=select(b2,extreme,wide1)",
];

/// The argument lists a function of `arity` channels is tried with.
fn arguments(arity: usize) -> Vec<String> {
    let sets: [[&str; 8]; 5] = [
        ["wide1", "wide2", "mix", "small1", "small2", "r1", "b1", "extreme"],
        ["small1", "small2", "r1", "r2", "b1", "b2", "b3", "half"],
        ["mix", "extreme", "wide2", "wide1", "highest", "lowest", "small1", "r2"],
        ["b1", "b2", "b3", "small1", "wide1", "r1", "r2", "mix"],
        ["b3", "small2", "small1", "b1", "b2", "mix", "wide2", "extreme"],
    ];
    sets.iter().map(|set| set[..arity].join(",")).collect()
}

/// The settings a kind is tried with (`""` for a kind without any).
fn settings(kind: Kind) -> Vec<&'static str> {
    match kind {
        Kind::Scale => vec![",mul=3,div=2", ",mul=-7", ",div=-3", ",mul=2147483647,div=-1", ",mul=-2147483648,div=7"],
        Kind::Clamp => vec![",lo=-100,hi=100", ",lo=0,hi=0", ",lo=-2147483648,hi=5", ",lo=1000,hi=2147483647"],
        Kind::Hyst => vec![",lo=-500,hi=500", ",lo=1000,hi=3000", ",lo=0,hi=1", ",lo=-2147483648,hi=2147483647"],
        Kind::Lowpass => vec![",shift=1", ",shift=4", ",shift=16"],
        Kind::Mean => vec![",n=2", ",n=7", ",n=64"],
        Kind::Debounce => vec![",n=1", ",n=2", ",n=5"],
        Kind::Ton => vec![",ms=1", ",ms=10", ",ms=25", ",ms=70"],
        _ => vec![""],
    }
}

#[test]
fn sp1_every_block_computes_what_the_reference_computes() {
    let mut cases = 0;
    // The model-based blocks have tests of their own (tests/model.rs).
    let blocks = KINDS.into_iter().filter(|k| k.class() == Class::Function && !wire_core::model::is_model(*k));
    for kind in blocks {
        let (fewest, most) = kind.arity();
        let mut arities = vec![fewest, most.min(fewest + 1), most];
        arities.dedup();
        for arity in arities {
            for (n, args) in arguments(arity).iter().enumerate() {
                for (m, setting) in settings(kind).iter().enumerate() {
                    // Both rates: the block at the base tick and slower than
                    // its inputs; its readers faster and slower than it.
                    for rate in ["", "@30ms"] {
                        let block = format!("y={}({args}{setting}){rate}", kind.name());
                        let mut statements: Vec<&str> = POOL.to_vec();
                        statements.extend([block.as_str(), "gpio10=y", "gpio11=y,invert@20ms", "pwm1.0=y"]);
                        let mut g = wiring(10, &statements);
                        text::set_trace::<&str>(&mut g, &[]).unwrap();
                        let seed = 0x5eed_0000 + (cases as u32) * 97 + (n * 7 + m) as u32;
                        compare(&g, 400, noise(seed));
                        cases += 1;
                    }
                }
            }
        }
    }
    assert!(cases > 700, "{cases} cases");
}

/// Feeds `xs` to a one-input block through a constant-free wiring: the
/// reference's value of `y` after each tick.
fn response(block: &str, xs: &[i32]) -> Vec<i32> {
    responses(block, &xs.iter().map(|&x| (x, 0)).collect::<Vec<_>>())
}

/// The same with two inputs `x` and `z`. The inputs are analog pins, so
/// values are 0..=4095; `x` and `z` are shifted down by 2048 first when
/// the block reads `sx` and `sz`.
fn responses(block: &str, inputs: &[(i32, i32)]) -> Vec<i32> {
    let statement = format!("y={block}");
    let g = wiring(
        10,
        &["x=adc1", "z=adc2", "half=2048", "sx=sub(x,half)", "sz=sub(z,half)", "u=gpio9", &statement, "gpio3=y", "gpio4=sx", "gpio5=sz", "gpio6=u", "gpio7=x", "gpio8=z"],
    );
    let plan = Plan::new(&g).unwrap_or_else(|issues| panic!("{issues:#?}"));
    let mut sim = Sim::new(&plan);
    let mut board = Board::default();
    inputs
        .iter()
        .map(|&(x, z)| {
            board.analog.insert(1, x);
            board.analog.insert(2, z);
            sim.step(&mut board);
            sim.channel("y").unwrap()
        })
        .collect()
}

#[test]
fn sp2_arithmetic_and_comparisons_are_signed_and_wrap() {
    // sx = x - 2048, sz = z - 2048.
    let at = |block: &str, x: i32, z: i32| responses(block, &[(x + 2048, z + 2048)])[0];
    assert_eq!(at("add(sx,sz)", -5, 3), -2);
    assert_eq!(at("add(sx,sz,x)", -5, 3), -2 + 2043);
    assert_eq!(at("sub(sx,sz)", -5, 3), -8);
    assert_eq!(at("mul(sx,sz)", -5, 3), -15);
    assert_eq!(at("min(sx,sz,half)", -5, 3), -5);
    assert_eq!(at("max(sx,sz,half)", -5, 3), 2048);
    assert_eq!(at("abs(sx)", -5, 0), 5);
    assert_eq!((at("gt(sx,sz)", -5, 3), at("gt(sx,sz)", 3, -5), at("gt(sx,sz)", 3, 3)), (0, 1, 0));
    assert_eq!((at("lt(sx,sz)", -5, 3), at("lt(sx,sz)", 3, -5), at("lt(sx,sz)", 3, 3)), (1, 0, 0));
    assert_eq!((at("ge(sx,sz)", -5, 3), at("ge(sx,sz)", 3, -5), at("ge(sx,sz)", 3, 3)), (0, 1, 1));
    assert_eq!((at("le(sx,sz)", -5, 3), at("le(sx,sz)", 3, -5), at("le(sx,sz)", 3, 3)), (1, 0, 1));
    assert_eq!((at("eq(sx,sz)", 3, 3), at("eq(sx,sz)", 3, 4)), (1, 0));
    assert_eq!((at("ne(sx,sz)", 3, 3), at("ne(sx,sz)", 3, 4)), (0, 1));
    // select(cond, a, b): a when cond is nonzero.
    assert_eq!((at("select(sx,sz,half)", -1, 7), at("select(sx,sz,half)", 0, 7)), (7, 2048));
    // scale computes in 64 bits and truncates toward zero.
    assert_eq!(at("scale(sx,mul=3,div=2)", 5, 0), 7);
    assert_eq!(at("scale(sx,mul=3,div=2)", -5, 0), -7);
    assert_eq!(at("scale(sx,mul=1000000,div=1000)", 2000, 0), 2_000_000);
    assert_eq!(at("scale(sx,div=-4)", 10, 0), -2);
    assert_eq!(at("scale(sx,mul=1000000)", 2047, 0), 2_047_000_000);
    assert_eq!(at("scale(sx,mul=2000000)", 2047, 0), (4_094_000_000i64 as u32) as i32, "truncated to 32 bits");
    assert_eq!((at("clamp(sx,lo=-10,hi=10)", -50, 0), at("clamp(sx,lo=-10,hi=10)", 4, 0), at("clamp(sx,lo=-10,hi=10)", 50, 0)), (-10, 4, 10));
    // A boolean function reads any nonzero value as true and gives 0 or 1.
    assert_eq!((at("and(sx,sz)", -7, 5), at("and(sx,sz)", -7, 0)), (1, 0));
    assert_eq!((at("xor(sx,sz)", 2, 4), at("xor(sx,sz)", 2, 0)), (0, 1));
    assert_eq!((at("not(sx)", 0, 0), at("not(sx)", -3, 0)), (1, 0));
    assert_eq!((at("nor(sx,sz)", 0, 0), at("or(sx,sz)", 0, -1)), (1, 1));
    // Wrapping, from constants.
    let g = wiring(10, &["big=2147483647", "one=1", "low=-2147483648", "a=add(big,one)", "big2=2147483647", "m=mul(big,big2)", "n=abs(low)", "d=sub(low,one)"]);
    let plan = Plan::new(&g).unwrap();
    let mut sim = Sim::new(&plan);
    sim.step(&mut Board::default());
    let v = |name: &str| sim.channel(name).unwrap();
    assert_eq!((v("a"), v("m"), v("n"), v("d")), (i32::MIN, 1, i32::MIN, i32::MAX));
    compare(&g, 3, |_, _| {});
}

#[test]
fn sp3_filters_keep_state_between_updates() {
    // lowpass: acc += x - (acc >> shift); y = acc >> shift.
    let step = response("lowpass(x,shift=2)", &[400; 6]);
    assert_eq!(step, [100, 175, 231, 273, 305, 329]);
    assert_eq!(*response("lowpass(x,shift=2)", &[400; 60]).last().unwrap(), 400, "no loss of precision");
    // Negative inputs shift arithmetically.
    assert_eq!(response("lowpass(sx,shift=1)", &[2048 - 100; 3]), [-50, -75, -88]);
    // mean: integrate and dump; the output holds between dumps.
    assert_eq!(response("mean(x,n=3)", &[3, 6, 9, 30, 30, 31, 1, 1]), [0, 0, 6, 6, 6, 30, 30, 30]);
    assert_eq!(response("mean(sx,n=2)", &[2048 - 3, 2048 - 4, 2048 + 3, 2048]), [0, -3, -3, 1], "signed, toward zero");
    // diff: x minus the previous x.
    assert_eq!(response("diff(x)", &[5, 5, 9, 2]), [5, 0, 4, -7]);
    // hyst: on at hi, off at lo, holding between.
    assert_eq!(response("hyst(x,lo=10,hi=20)", &[15, 20, 15, 11, 10, 19, 25, 0]), [0, 1, 1, 1, 0, 0, 1, 0]);
}

#[test]
fn sp4_edges_counters_latches_and_timers() {
    assert_eq!(response("rising(x)", &[0, 5, 9, 0, 0, 1]), [0, 1, 0, 0, 0, 1]);
    assert_eq!(response("rising(x)", &[7, 7]), [1, 0], "state starts at 0, so high at once is an edge");
    assert_eq!(response("falling(x)", &[0, 5, 9, 0, 0, 1, 0]), [0, 0, 0, 1, 0, 0, 1]);
    // debounce: follows once the input has been stable for n updates.
    assert_eq!(response("debounce(x,n=3)", &[1, 1, 0, 1, 1, 1, 1, 0, 0, 1, 0, 0, 0, 0]), [0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 0, 0]);
    assert_eq!(response("debounce(x,n=1)", &[1, 0, 1]), [1, 0, 1]);
    // count: rising edges of the clock; 0 while reset is nonzero.
    assert_eq!(response("count(x)", &[0, 1, 1, 0, 1, 0, 9]), [0, 1, 1, 1, 2, 2, 3]);
    let clocked = [(1, 0), (0, 0), (1, 0), (0, 1), (1, 1), (1, 0), (0, 0), (1, 0)];
    assert_eq!(responses("count(x,z)", &clocked), [1, 1, 2, 0, 0, 0, 0, 1], "an edge during reset is not counted");
    // latch(set, reset): reset wins.
    let pulses = [(0, 0), (1, 0), (0, 0), (0, 1), (1, 1), (1, 0)];
    assert_eq!(responses("latch(x,z)", &pulses), [0, 1, 1, 0, 0, 1]);
    // ton: on after ceil(ms / period) consecutive nonzero updates (10 ms each).
    assert_eq!(response("ton(x,ms=30)", &[1, 1, 1, 1, 0, 1, 1, 0, 1, 1, 1]), [0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 1]);
    assert_eq!(response("ton(x,ms=25)", &[1, 1, 1, 1]), [0, 0, 1, 1], "25 ms is three 10 ms updates");
    assert_eq!(response("ton(x,ms=10)", &[0, 1, 0, 1]), [0, 1, 0, 1]);
    // At its own period: 100 ms is two 50 ms updates, on base ticks 0, 5, 10.
    let y = response("ton(x,ms=100)@50ms", &[1; 11]);
    assert_eq!(y, [0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1]);
}

#[test]
fn sp5_analog_inputs_constants_and_pwm_outputs() {
    let g = wiring(10, &["level=adc26", "bias=-7", "sum=add(level,bias)", "pwm0.1=level,freq=2000", "pwm0.0=sum,freq=2000", "pwm2.0=bias"]);
    let mut readings = [0, 100, 100, 4095, -1, 3, 3, 3, 5].into_iter().cycle();
    let mut values: Vec<i32> = Vec::new();
    let history = compare(&g, 9, |tick, board| {
        if values.len() <= tick as usize {
            values.push(readings.next().unwrap());
        }
        board.analog.insert(26, values[tick as usize]);
    });
    // The first tick sets every duty; after that only a changed one. A
    // read error is 0 and a negative value is duty 0.
    assert_eq!(history[0], [Event::Pwm(0, 1, 0), Event::Pwm(0, 0, 0), Event::Pwm(2, 0, 0)]);
    assert_eq!(history[1], [Event::Pwm(0, 1, 100), Event::Pwm(0, 0, 93)]);
    assert_eq!(history[2], []);
    assert_eq!(history[3], [Event::Pwm(0, 1, 4095), Event::Pwm(0, 0, 4088)]);
    assert_eq!(history[4], [Event::Pwm(0, 1, 0), Event::Pwm(0, 0, 0)]);
    assert_eq!(history[5], [Event::Pwm(0, 1, 3)], "3 - 7 is still duty 0");
    assert_eq!(history[8], [Event::Pwm(0, 1, 5)]);

    // The duty is the channel limited to 0..=65535.
    let g = wiring(10, &["x=adc1", "y=scale(x,mul=100)", "pwm3.1=y"]);
    let history = compare(&g, 3, |tick, board| {
        board.analog.insert(1, [5, 700, 655][tick as usize]);
    });
    assert_eq!(history, [[Event::Pwm(3, 1, 500)], [Event::Pwm(3, 1, 65535)], [Event::Pwm(3, 1, 65500)]]);

    // init opens each timer once, at its frequency, and logs a refusal.
    let g = wiring(10, &["level=adc26", "pwm0.1=level,freq=2000", "pwm0.0=level,freq=2000", "pwm2.0=level", "b=gpio4", "gpio5=b"]);
    let mut p = load(&g, Board { refused_timers: vec![2], refused: vec![5], ..Board::default() });
    assert_eq!(p.device().opened, [(0, 2000)]);
    assert_eq!(p.device().logs, ["wire: output gpio 5 refused", "wire: pwm timer 2 refused", "wire w: 2 in, 2 ch, 4 out"]);
    assert_eq!(p.next_tick(), Some(10));
}

#[test]
fn sp6_a_payload_imports_only_what_it_calls() {
    let imports = |statements: &[&str], trace: bool| {
        let mut g = wiring(10, statements);
        if trace {
            text::set_trace::<&str>(&mut g, &[]).unwrap();
        }
        let (wasm, _) = build(&g, None).unwrap();
        let has = |name: &str| wasm.windows(name.len()).any(|w| w == name.as_bytes());
        ["log", "set_tick", "gpio_input", "gpio_read", "gpio_output", "gpio_write", "adc_read", "pwm_open", "pwm_set", "publish"]
            .into_iter()
            .filter(|n| has(n))
            .collect::<Vec<_>>()
    };
    // A wiring of digital pins still needs only ABI 1.
    assert_eq!(imports(&["a=gpio1", "y=rising(a)", "gpio2=y"], false), ["log", "set_tick", "gpio_input", "gpio_read", "gpio_output", "gpio_write"]);
    assert_eq!(imports(&["a=adc1", "pwm0.0=a"], false), ["log", "set_tick", "adc_read", "pwm_open", "pwm_set"]);
    assert_eq!(imports(&["a=5", "b=abs(a)"], true), ["log", "set_tick", "publish"]);
    assert_eq!(imports(&["a=5", "b=abs(a)"], false), ["log", "set_tick"]);
}

#[test]
fn sp7_settings_belong_to_their_kinds_and_have_ranges() {
    let e = |statements: &[&str]| errors(&wiring(10, statements), None);
    assert_eq!(e(&["x=adc1", "y=lowpass(x)"]), ["y.fn (lowpass) has no shift: set it (1 to 16)"]);
    assert_eq!(e(&["x=adc1", "y=lowpass(x,shift=17)"]), ["y.fn (lowpass) has shift 17; it must be 1 to 16"]);
    assert_eq!(e(&["x=adc1", "y=mean(x,n=1)"]), ["y.fn (mean) has n 1; it must be 2 to 4096"]);
    assert_eq!(e(&["x=adc1", "y=debounce(x,n=60001)"]), ["y.fn (debounce) has n 60001; it must be 1 to 60000"]);
    assert_eq!(e(&["x=adc1", "y=ton(x,ms=0)"]), ["y.fn (ton) has ms 0; it must be 1 to 3600000"]);
    assert_eq!(e(&["x=adc1", "y=scale(x,div=0)"]), ["y.fn (scale) divides by 0; div must not be 0"]);
    assert_eq!(e(&["x=adc1", "y=clamp(x,lo=5,hi=4)"]), ["y.fn (clamp) has lo 5 above hi 4; lo must not exceed hi"]);
    assert!(e(&["x=adc1", "y=clamp(x,lo=5,hi=5)"]).is_empty());
    assert_eq!(e(&["x=adc1", "y=hyst(x,lo=5,hi=5)"]), ["y.fn (hyst) has lo 5 and hi 5; lo must be below hi"]);
    assert_eq!(e(&["x=adc1", "y=hyst(x,hi=5)"]), ["y.fn (hyst) has no lo: set it (-2147483648 to 2147483647)"]);
    assert_eq!(e(&["x=adc1", "pwm4.0=x"]), ["pwm4.0 (pwm) has timer 4; it must be 0 to 3"]);
    assert_eq!(e(&["x=adc1", "pwm0.2=x"]), ["pwm0.2 (pwm) has channel 2; it must be 0 to 1"]);
    assert_eq!(e(&["x=adc1", "pwm0.0=x,freq=0"]), ["pwm0.0 (pwm) has freq_hz 0; it must be 1 to 40000000"]);
    assert_eq!(e(&["x=adc1", "pwm0.0=x", "pwm0.1=x,freq=50"]), ["pwm0.1 runs PWM timer 0 at 50 Hz but pwm0.0 runs it at 1000 Hz; a timer has one frequency"]);
    assert_eq!(e(&["x=adc1", "y=sub(x)"]), ["y.fn (sub) reads 1 channel(s); it needs exactly 2 channels"]);
    assert_eq!(e(&["x=adc1", "y=count(x,x2,x3)", "x2=1", "x3=2"]), ["y.fn (count) reads 3 channel(s); it needs 1 to 2 channels"]);
    assert_eq!(e(&["x=adc1", "y=gpio1"]), ["GPIO 1 is already used by adc1; one node per pin"]);

    // A setting on a kind that does not take it names the node.
    let mut g = wiring(10, &["x=adc1", "y=abs(x)", "gpio2=y"]);
    g.nodes.iter_mut().find(|n| n.id == "y.fn").unwrap().config.shift = Some(3);
    g.nodes.iter_mut().find(|n| n.id == "gpio2").unwrap().config.value = Some(1);
    g.nodes.iter_mut().find(|n| n.id == "adc1").unwrap().config.trace = true;
    g.nodes.iter_mut().find(|n| n.id == "x").unwrap().config.period_ms = Some(20);
    g.nodes.iter_mut().find(|n| n.id == "y").unwrap().config.pin = Some(3);
    let issues = check(&g, None);
    let about = |id: &str| issues.iter().find(|i| i.node.as_deref() == Some(id)).map(|i| i.message.as_str());
    assert_eq!(about("y.fn"), Some("y.fn is a function (abs) and takes no 'shift' setting"));
    assert_eq!(about("gpio2"), Some("gpio2 is a digital output (output) and takes no 'value' setting"));
    assert_eq!(about("adc1"), Some("adc1 is an analog input (analog) and takes no 'trace' setting"));
    assert_eq!(about("x"), Some("x is a channel and has no period: it holds the last value written"));
    assert_eq!(about("y"), Some("y is a channel and takes no pin settings"));

    // Against a chip: the pin must have an analog input, the PWM output a pin.
    let g = wiring(10, &["x=adc5", "gpio2=x"]);
    assert_eq!(errors(&g, Some("rp2350")), ["GPIO 5 has no analog input on this board (analog pins: 26, 27, 28)"]);
    // The Pico keeps GPIO 26 to 28 for the ADC: analog yes, digital no.
    assert!(errors(&wiring(10, &["x=adc26", "gpio2=x"]), Some("rp2350")).is_empty());
    assert_eq!(errors(&wiring(10, &["x=gpio26", "gpio2=x"]), Some("rp2350")), ["GPIO 26 is taken by the board: the ADC (analog input only)"]);
    assert_eq!(errors(&wiring(10, &["x=adc40", "gpio2=x"]), Some("rp2350")), ["this board has no GPIO 40"]);
    let g = wiring(10, &["x=adc34", "pwm0.1=x", "pwm1.1=x"]);
    assert!(errors(&g, Some("esp32")).is_empty(), "{:?}", errors(&g, Some("esp32")));
    assert_eq!(errors(&g, Some("rp2350")), ["this board has no GPIO 34", "this board has no PWM output 0.1", "this board has no PWM output 1.1"]);
    // The ESP32 wires PWM 0.1 to GPIO 4.
    let g = wiring(10, &["x=gpio4", "pwm0.1=x"]);
    assert_eq!(errors(&g, Some("esp32")), ["PWM output 0.1 is GPIO 4 on this board, which gpio4 already uses"]);
}

#[test]
fn sp8_statements_spell_every_kind_and_format_back() {
    let statements = [
        "button=gpio15,pullup,invert@20ms",
        "raw=adc26",
        "limit=1200",
        "offset=-7",
        "slow=mean(raw,n=10)@100ms",
        "level=lowpass(slow,shift=4)@100ms",
        "hot=hyst(level,lo=1000,hi=1200)",
        "trimmed=scale(raw,mul=3)",
        "halved=scale(raw,div=2)",
        "c=count(button,hot)",
        "both=and(button,hot)",
        "gpio25=both,invert@1000ms",
        "pwm0.1=level,freq=2000@100ms",
        "pwm1.0=trimmed",
    ];
    let g = wiring(10, &statements);
    assert_eq!(text::format(&g), statements);
    assert_eq!(text::parse("w", 10, &text::format(&g)).unwrap(), g);
    assert_eq!(Graph::from_json(&g.to_json()).unwrap(), g);
    let node = |id: &str| g.node(id).unwrap_or_else(|| panic!("no node {id}"));
    assert_eq!(node("adc26").kind, Kind::Analog);
    assert_eq!(node("limit.const").config, Config { value: Some(1200), ..Config::default() });
    assert_eq!(node("offset.const").config.value, Some(-7));
    assert_eq!(node("level.fn").config, Config { shift: Some(4), period_ms: Some(100), ..Config::default() });
    assert_eq!(node("hot.fn").config, Config { lo: Some(1000), hi: Some(1200), ..Config::default() });
    assert_eq!(node("pwm0.1").config, Config { timer: Some(0), channel: Some(1), freq_hz: Some(2000), period_ms: Some(100), ..Config::default() });
    assert_eq!(node("gpio15").config.period_ms, Some(20));
    // Arguments are ordered: the edges into a function, in order.
    assert_eq!(g.sources("c.fn").collect::<Vec<_>>(), ["button", "hot"]);
    // Only what is set is written, in JSON as in statements.
    let json = g.to_json();
    assert!(json.contains(r#"{"id":"level.fn","kind":"lowpass","config":{"shift":4,"period_ms":100}}"#), "{json}");
    assert!(json.contains(r#"{"id":"pwm1.0","kind":"pwm","config":{"timer":1,"channel":0}}"#), "{json}");
    assert!(json.contains(r#"{"id":"both.fn","kind":"and"}"#), "{json}");
    // The wiring's own period is not a setting, and a default is not written.
    let g = wiring(20, &["a=adc1@20ms", "y=scale(a,mul=1,div=1)", "pwm0.0=y,freq=1000"]);
    assert_eq!(g.node("adc1").unwrap().config.period_ms, None);
    assert_eq!(text::format(&g), ["a=adc1", "y=scale(a)", "pwm0.0=y"]);

    let bad = |s: &[&str]| text::parse("w", 10, s).unwrap_err();
    assert!(bad(&["y=lowpass(x,shift=four)"]).contains("'four' is not a whole number"));
    assert!(bad(&["y=lowpass(x,shift=999)"]).contains("shift=999 is out of range"));
    assert!(bad(&["y=lowpass(x,n=4)"]).contains("lowpass takes shift, not 'n'"));
    assert!(bad(&["y=abs(x,n=4)"]).contains("abs takes no settings"));
    assert!(bad(&["y=hyst(lo=1,x,hi=2)"]).contains("channels come before settings"));
    assert!(bad(&["y=hyst(x,lo=1,lo=2)"]).contains("lo is given twice"));
    assert!(bad(&["y=sub(x,x)"]).contains("x is given twice"));
    assert!(bad(&["y=99999999999"]).contains("is not a whole number from -2147483648 to 2147483647"));
    assert!(bad(&["y=adc26,pullup"]).contains("an analog input takes no options"));
    assert!(bad(&["pwm0.1=y,invert"]).contains("takes only freq=<Hz>"));
    assert!(bad(&["x=adc1", "pwm0.1=x", "pwm0.1=x"]).contains("PWM output 0.1 already follows a channel"));
    assert!(bad(&["y=adc1@fast"]).contains("after '@' comes the update period in ms"));
    assert!(bad(&["y=adc1@10"]).contains("after '@' comes the update period in ms"));
    assert!(bad(&["adc26=gpio1"]).contains("is not a channel name"));
    assert!(bad(&["count=gpio1"]).contains("is not a channel name"));
    assert!(bad(&["y=sum(a,b)"]).contains("is not a function (and, or, not, xor, nand, nor, xnor, add, sub, mul, min, max, abs, gt, lt, ge, le, eq, ne, select, scale, clamp, delay, lowpass, mean, diff, hyst, debounce, rising, falling, count, latch, ton, ukf, cbf)"));
}

#[test]
fn sp9_the_table_of_kinds_is_the_one_description() {
    // Every kind once, under the name JSON spells it with.
    let mut names: Vec<&str> = KINDS.iter().map(|k| k.name()).collect();
    for kind in KINDS {
        assert_eq!(serde_json::to_string(&kind).unwrap(), format!("\"{}\"", kind.name()));
        assert_eq!(kind.spec().kind, kind);
        for p in kind.spec().params {
            assert!(wire_core::spec::FIELDS.contains(&p.name), "{} is a Config field", p.name);
            assert!(p.min <= p.max);
            assert!(p.default.is_none_or(|d| (p.min..=p.max).contains(&d)));
            if matches!(p.ty, wire_core::spec::ParamType::Real | wire_core::spec::ParamType::Text) {
                continue;
            }
            let mut config = Config::default();
            assert!(config.set(p.name, p.max), "{} takes its largest value", p.name);
            assert_eq!(config.get(p.name), Some(p.max).filter(|_| p.max != 0));
        }
    }
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), KINDS.len());

    let json = kinds_json();
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    let kinds = parsed.as_array().unwrap();
    assert_eq!(kinds.len(), KINDS.len());
    let of = |name: &str| kinds.iter().find(|k| k["kind"] == name).unwrap_or_else(|| panic!("{name}"));
    assert!(json.contains(
        r#"{"kind":"lowpass","class":"function","inputs":[1,1],"outputs":[1,1],"stateful":true,"params":[{"name":"shift","type":"int","min":1,"max":16,"default":null,"required":true},{"name":"period_ms","type":"int","min":10,"max":3600000,"default":null,"required":false}]}"#
    ));
    assert!(json.contains(r#"{"kind":"channel","class":"channel","inputs":[1,1],"outputs":[1,1],"stateful":false,"params":[{"name":"trace","type":"bool","min":0,"max":1,"default":0,"required":false}]}"#));
    assert_eq!(of("input")["class"], "source");
    assert_eq!(of("input")["inputs"].to_string(), "[0,0]");
    assert!(json.contains(r#"{"kind":"input","class":"source","inputs":[0,0],"outputs":[1,1],"stateful":false,"params":[{"name":"pin","type":"int","min":0,"max":255,"default":null,"required":true},{"name":"pull","type":"pull","min":0,"max":2,"default":0,"required":false,"options":["none","up","down"]},"#));
    assert_eq!(of("pwm")["class"], "sink");
    assert!(json.contains(r#"{"name":"freq_hz","type":"int","min":1,"max":40000000,"default":1000,"required":false}"#));
    assert_eq!(of("and")["inputs"].to_string(), "[2,8]");
    assert_eq!(of("count")["inputs"].to_string(), "[1,2]");
    assert_eq!(of("scale")["params"][0]["default"], 1);
}

#[test]
fn ms1_a_node_updates_on_the_ticks_its_period_divides() {
    // Base 10 ms: the pin is read every 20 ms, the copy taken every 50 ms
    // and the output written every 100 ms.
    let g = wiring(10, &["a=gpio1@20ms", "n=not(a)@50ms", "gpio2=n@100ms", "gpio3=a"]);
    let plan = Plan::new(&g).unwrap();
    let every: Vec<(&str, u32)> = plan.steps.iter().map(|s| (s.id.as_str(), s.every)).collect();
    assert_eq!(every, [("gpio1", 2), ("n.fn", 5), ("gpio2", 10), ("gpio3", 1)]);

    let mut sim = Sim::new(&plan);
    let mut board = Board::default();
    let mut read_on = Vec::new();
    let mut n_seen = Vec::new();
    for tick in 0..21u32 {
        // The pin goes high after tick 0 and low again after tick 10.
        board.levels.insert(1, (1..=10).contains(&tick));
        sim.step(&mut board);
        if !std::mem::take(&mut board.reads).is_empty() {
            read_on.push(tick);
        }
        n_seen.push(sim.channel("n").unwrap());
    }
    assert_eq!(read_on, [0, 2, 4, 6, 8, 10, 12, 14, 16, 18, 20]);
    // `a` is 1 from tick 2 and 0 from tick 12; `n` samples it on 0, 5, 10, 15, 20.
    assert_eq!(n_seen, [1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1]);
    // gpio2 writes on tick 0, then on 10 and 20 when the level changed;
    // gpio3 follows `a` at the base tick.
    let gpio2: Vec<&Event> = board.events.iter().filter(|e| matches!(e, Event::Gpio(2, _))).collect();
    assert_eq!(gpio2, [&Event::Gpio(2, true), &Event::Gpio(2, false), &Event::Gpio(2, true)]);
    let gpio3: Vec<&Event> = board.events.iter().filter(|e| matches!(e, Event::Gpio(3, _))).collect();
    assert_eq!(gpio3, [&Event::Gpio(3, false), &Event::Gpio(3, true), &Event::Gpio(3, false)]);

    // The payload does the same, tick for tick.
    let history = compare(&g, 21, |tick, board| {
        board.levels.insert(1, (1..=10).contains(&tick));
    });
    assert_eq!(history[0], [Event::Gpio(2, true), Event::Gpio(3, false)]);
    assert_eq!(history[2], [Event::Gpio(3, true)]);
    assert_eq!(history[10], [Event::Gpio(2, false)]);
    assert_eq!(history[12], [Event::Gpio(3, false)]);
    assert_eq!(history[20], [Event::Gpio(2, true)]);
}

#[test]
fn ms2_a_multi_rate_chain_matches_the_reference() {
    // An ADC and a decimator at the base tick, a filter and a threshold ten
    // times slower, an output a hundred times slower.
    let statements = [
        "raw=adc26@10ms",
        "avg=mean(raw,n=10)@10ms",
        "smooth=lowpass(avg,shift=2)@100ms",
        "hot=hyst(smooth,lo=1500,hi=2500)@100ms",
        "gpio25=hot@1000ms",
        "pwm0.0=smooth@100ms",
        "fast=diff(smooth)",
        "gpio2=fast",
    ];
    let mut g = wiring(10, &statements);
    text::set_trace(&mut g, &["avg", "smooth", "hot"]).unwrap();
    // A slow square wave under noise, so the threshold switches.
    let mut lcg = Lcg(7);
    let mut samples: Vec<i32> = Vec::new();
    let history = compare(&g, 3000, |tick, board| {
        while samples.len() <= tick as usize {
            let wave = if (samples.len() / 700).is_multiple_of(2) { 600 } else { 3400 };
            samples.push(wave + (lcg.next() % 600) as i32);
        }
        board.analog.insert(26, samples[tick as usize]);
    });
    let published = |name: &str| {
        history
            .iter()
            .enumerate()
            .flat_map(|(tick, events)| events.iter().map(move |e| (tick, e)))
            .filter_map(|(tick, e)| match e {
                Event::Publish(n, v) if n == name => Some((tick, *v)),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    // The decimator dumps on every tenth tick (9, 19, ...), the slower
    // blocks change only on their own ticks, and the output only on its.
    assert!(published("avg").iter().skip(1).all(|(tick, _)| tick % 10 == 9));
    assert!(published("smooth").iter().all(|(tick, _)| tick % 10 == 0));
    let hot = published("hot");
    assert!(hot.len() >= 4 && hot.iter().all(|(tick, _)| tick % 10 == 0), "{hot:?}");
    let lamp: Vec<usize> = history
        .iter()
        .enumerate()
        .filter(|(_, events)| events.iter().any(|e| matches!(e, Event::Gpio(25, _))))
        .map(|(tick, _)| tick)
        .collect();
    assert!(lamp.len() >= 3 && lamp.iter().all(|tick| tick % 100 == 0), "{lamp:?}");
    // The first dump is the mean of the first ten samples.
    assert_eq!(published("avg")[1], (9, samples[..10].iter().sum::<i32>() / 10));
}

#[test]
fn ms3_random_rates_on_every_node_match_the_reference() {
    let rates = ["", "@10ms", "@20ms", "@30ms", "@70ms", "@160ms"];
    let mut lcg = Lcg(99);
    for round in 0..40 {
        let mut pick = || rates[(lcg.next() % rates.len() as u32) as usize];
        let statements = [
            format!("a=gpio1{}", pick()),
            format!("b=gpio2,invert{}", pick()),
            format!("r=adc26{}", pick()),
            format!("k=37{}", pick()),
            format!("s=add(r,k){}", pick()),
            format!("f=lowpass(s,shift=3){}", pick()),
            format!("m=mean(s,n=4){}", pick()),
            format!("d=diff(f){}", pick()),
            format!("h=hyst(f,lo=1000,hi=3000){}", pick()),
            format!("e=rising(a){}", pick()),
            format!("c=count(e,b){}", pick()),
            format!("q=latch(a,b){}", pick()),
            format!("t=ton(a,ms=50){}", pick()),
            format!("z=delay(c){}", pick()),
            format!("o=select(q,m,d){}", pick()),
            format!("gpio10=h{}", pick()),
            format!("gpio11=t,invert{}", pick()),
            format!("gpio12=z{}", pick()),
            format!("pwm0.0=o{}", pick()),
        ];
        let refs: Vec<&str> = statements.iter().map(String::as_str).collect();
        let mut g = wiring(10, &refs);
        text::set_trace::<&str>(&mut g, &[]).unwrap();
        assert_eq!(text::parse("w", 10, &text::format(&g)).unwrap().edges, g.edges);
        compare(&g, 700, noise(1000 + round));
    }
}

#[test]
fn ms4_a_period_is_a_multiple_of_the_base_tick() {
    let e = |period: u32, statements: &[&str]| errors(&wiring(period, statements), None);
    assert_eq!(e(10, &["a=gpio1@15ms", "gpio2=a"]), ["gpio1 updates every 15 ms; that must be a multiple of the scan period (10 ms), at most 3600000 ms"]);
    assert_eq!(e(20, &["a=gpio1", "gpio2=a@30ms"]), ["gpio2 updates every 30 ms; that must be a multiple of the scan period (20 ms), at most 3600000 ms"]);
    assert_eq!(e(10, &["a=gpio1", "n=not(a)@0ms", "gpio2=n"]).len(), 1);
    assert_eq!(e(10, &["a=gpio1", "n=not(a)@1ms", "gpio2=n"]).len(), 1, "the ABI's shortest tick is 10 ms");
    assert_eq!(e(10, &["a=gpio1", "n=not(a)@3600010ms", "gpio2=n"]).len(), 1);
    assert!(e(10, &["a=gpio1", "n=not(a)@3600000ms", "gpio2=n"]).is_empty());
    assert!(e(50, &["a=gpio1@100ms", "n=not(a)@50ms", "gpio2=n@5000ms"]).is_empty());
    // The base tick is what the payload asks the runtime for.
    let g = wiring(50, &["a=gpio1@100ms", "gpio2=a@5000ms"]);
    assert_eq!(load(&g, Board::default()).next_tick(), Some(50));
    // The same period on two statements of one pin, or it is an error.
    assert!(text::parse("w", 10, &["a=gpio1@20ms", "b=gpio1@20ms"]).is_ok());
    assert!(text::parse("w", 10, &["a=gpio1@20ms", "b=gpio1"]).unwrap_err().contains("with other options"));
}

#[test]
fn ms5_rates_that_share_no_factor_stay_in_step() {
    // Periods of 3, 7 and 11 base ticks line up only every 231 ticks.
    let g = wiring(10, &["a=gpio1@30ms", "c=count(a)@70ms", "pwm0.0=c@110ms", "gpio2=a"]);
    let history = compare(&g, 3000, noise(5));
    let duties = history.iter().flatten().filter(|e| matches!(e, Event::Pwm(..))).count();
    assert!(duties > 20, "{duties}");
}

#[test]
fn tu1_a_delay_gives_the_value_of_its_previous_update() {
    assert_eq!(response("delay(x)", &[5, 6, 7, 7, 0]), [0, 5, 6, 7, 7]);
    // Slower than its input: the value its input had at its own last update.
    assert_eq!(response("delay(x)@30ms", &[1, 2, 3, 4, 5, 6, 7, 8]), [0, 0, 0, 1, 1, 1, 4, 4]);
    // The capture is at the end of the tick, so it takes this tick's value
    // even of a channel computed after the delay's place in the order.
    let g = wiring(10, &["prev=delay(sum)", "x=adc1", "one=1", "sum=add(x,one)", "gpio2=prev"]);
    let plan = Plan::new(&g).unwrap();
    assert_eq!(plan.steps[0].id, "prev.fn", "a delay is a source in the order");
    let mut sim = Sim::new(&plan);
    let mut board = Board::default();
    let mut seen = Vec::new();
    for x in [10, 20, 30] {
        board.analog.insert(1, x);
        sim.step(&mut board);
        seen.push((sim.channel("sum").unwrap(), sim.channel("prev").unwrap()));
    }
    assert_eq!(seen, [(11, 0), (21, 11), (31, 21)]);
}

#[test]
fn tu2_a_loop_through_a_delay_is_a_divider_or_an_accumulator() {
    // A toggling divider: q changes on every update.
    let mut g = wiring(10, &["q=delay(nq)", "nq=not(q)", "gpio2=q"]);
    assert!(errors(&g, None).is_empty());
    text::set_trace(&mut g, &["q"]).unwrap();
    let history = compare(&g, 9, |_, _| {});
    let levels: Vec<bool> = history
        .iter()
        .map(|events| match events.as_slice() {
            [Event::Publish(name, v), Event::Gpio(2, level)] if name == "q" => {
                assert_eq!(*v != 0, *level);
                *level
            }
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(levels, [false, true, false, true, false, true, false, true, false]);

    // Divided again by running the loop at a third of the base rate.
    let g = wiring(10, &["q=delay(nq)@30ms", "nq=not(q)@30ms", "gpio2=q"]);
    let history = compare(&g, 13, |_, _| {});
    let changes: Vec<usize> = (0..13).filter(|&t| !history[t].is_empty()).collect();
    assert_eq!(changes, [0, 3, 6, 9, 12]);

    // An accumulator: acc = acc_prev + x, with pseudo-random x.
    let mut g = wiring(10, &["x=adc26", "acc=add(acc_prev,x)", "acc_prev=delay(acc)", "pwm0.0=acc"]);
    text::set_trace(&mut g, &["acc"]).unwrap();
    let mut lcg = Lcg(3);
    let mut xs: Vec<i32> = Vec::new();
    let history = compare(&g, 500, |tick, board| {
        while xs.len() <= tick as usize {
            xs.push((lcg.next() % 4096) as i32);
        }
        board.analog.insert(26, xs[tick as usize]);
    });
    let mut sum = 0i32;
    for (tick, events) in history.iter().enumerate() {
        sum += xs[tick];
        if xs[tick] != 0 || tick == 0 {
            assert_eq!(events[0], Event::Publish("acc".into(), sum), "tick {tick}");
        }
    }
    assert!(sum > 500_000);

    // A wrapping one, over the whole range, with a reset and two rates.
    let statements = [
        "r=adc26", "step=scale(r,mul=1048583)", "rst=gpio1", "zero=0",
        "acc=add(held,step)", "next=select(rst,zero,acc)@20ms", "held=delay(next)", "gpio5=acc",
        "top=max(acc,peak_prev)", "peak_prev=delay(top)@50ms", "pwm0.1=top",
    ];
    let mut g = wiring(10, &statements);
    text::set_trace::<&str>(&mut g, &[]).unwrap();
    compare(&g, 2000, noise(77));
}

#[test]
fn tu3_a_loop_without_a_delay_is_an_error() {
    // The statements allow the loop; the check names every node on it.
    let g = wiring(10, &["a=gpio1", "x=or(a,y)", "y=not(x)", "gpio2=y"]);
    let issues = check(&g, None);
    let mut on_loop: Vec<&str> = issues
        .iter()
        .filter(|i| i.severity == Severity::Error)
        .map(|i| {
            assert!(i.message.ends_with("feeds back into itself; a loop needs a delay in it"), "{}", i.message);
            i.node.as_deref().unwrap()
        })
        .collect();
    on_loop.sort_unstable();
    assert_eq!(on_loop, ["x", "x.fn", "y", "y.fn"]);
    assert!(Plan::new(&g).is_err());
    assert!(build(&g, None).is_err());
    // Stateful blocks other than delay do not cut a loop.
    for block in ["lowpass(x,shift=2)", "latch(x,a)", "diff(x)", "mean(x,n=2)", "rising(x)"] {
        let y = format!("y={block}");
        let g = wiring(10, &["a=gpio1", "x=or(a,y)", &y, "gpio2=y"]);
        assert!(errors(&g, None).iter().any(|m| m.contains("x feeds back into itself")), "{block}");
    }
    // The same loop with a delay anywhere in it builds.
    for statements in [
        ["a=gpio1", "x=or(a,yd)", "y=not(x)", "yd=delay(y)", "gpio2=y"],
        ["a=gpio1", "x=or(a,y)", "xd=delay(x)", "y=not(xd)", "gpio2=y"],
    ] {
        let g = wiring(10, &statements);
        assert!(errors(&g, None).is_empty());
        compare(&g, 50, noise(11));
    }
    // A delay of itself is legal (and stays 0); any other function of itself is not.
    assert!(errors(&wiring(10, &["q=delay(q)", "gpio2=q"]), None).is_empty());
    assert!(text::parse("w", 10, &["q=not(q)"]).unwrap_err().contains("cannot be its own input (a loop needs a delay in it)"));
    // Two loops through one delay each.
    let g = wiring(10, &["a=delay(nb)", "b=delay(na)", "na=not(a)", "nb=add(b,na)", "gpio2=nb", "pwm0.0=nb"]);
    compare(&g, 64, |_, _| {});
}

#[test]
fn tu4_one_order_covers_sources_functions_and_sinks() {
    // Statements in any order: every node comes after what feeds it, a
    // delay first of all, and among ready nodes the earliest stated first.
    let g = wiring(10, &["gpio20=out", "out=add(mid,held)", "held=delay(out)", "mid=abs(raw)", "raw=adc26", "pwm0.0=mid"]);
    let plan = Plan::new(&g).unwrap();
    let order: Vec<&str> = plan.steps.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(order, ["held.fn", "adc26", "mid.fn", "out.fn", "gpio20", "pwm0.0"]);
    let before = |a: &str, b: &str| order.iter().position(|s| *s == a) < order.iter().position(|s| *s == b);
    assert!(before("adc26", "mid.fn") && before("mid.fn", "out.fn") && before("out.fn", "gpio20"));
    // So within one tick a change runs from the pin to the output.
    let mut sim = Sim::new(&plan);
    let mut board = Board::default();
    board.analog.insert(26, 9);
    sim.step(&mut board);
    assert_eq!((sim.channel("out"), sim.channels().len()), (Some(9), 4));
    assert_eq!(board.events, [Event::Gpio(20, true), Event::Pwm(0, 0, 9)]);
    // The old one-scan helpers are the first tick.
    assert_eq!(plan.eval(|_| false), [(20, false)]);
    assert_eq!(plan.channel_values(|_| false), [0, 0, 0, 0]);
    compare(&g, 30, noise(4));
}

#[test]
fn tu5_the_largest_wiring_runs_within_one_call() {
    // 127 of at most 128 nodes: a chain of stateful blocks at mixed rates, all traced.
    let mut statements = vec!["c0=adc26".to_string()];
    let blocks = ["lowpass(P,shift=1)", "diff(P)@20ms", "delay(P)", "abs(P)", "debounce(P,n=2)@30ms", "scale(P,mul=5,div=3)"];
    for i in 1..63 {
        statements.push(format!("c{i}={}", blocks[i % blocks.len()].replace('P', &format!("c{}", i - 1))));
    }
    statements.push("gpio2=c62".into());
    let refs: Vec<&str> = statements.iter().map(String::as_str).collect();
    let mut g = wiring(10, &refs);
    assert_eq!(g.nodes.len(), 127);
    text::set_trace::<&str>(&mut g, &[]).unwrap();
    compare(&g, 120, noise(8));
}

#[test]
fn tr1_a_traced_channel_is_published_when_its_value_changes() {
    let mut g = wiring(10, &["x=adc26", "y=scale(x,div=100)@30ms", "gpio2=y"]);
    text::set_trace(&mut g, &["y"]).unwrap();
    assert!(g.node("y").unwrap().config.trace && !g.node("x").unwrap().config.trace);
    assert!(g.to_json().contains(r#"{"id":"y","kind":"channel","config":{"trace":true}}"#));
    let xs = [0, 0, 0, 250, 250, 250, 260, 299, 299, 300, 300, 300, 0];
    let history = compare(&g, xs.len() as u32, |tick, board| {
        board.analog.insert(26, xs[tick as usize]);
    });
    let published: Vec<(usize, i32)> = history
        .iter()
        .enumerate()
        .flat_map(|(tick, events)| events.iter().map(move |e| (tick, e)))
        .filter_map(|(tick, e)| match e {
            Event::Publish(name, v) if name == "y" => Some((tick, *v)),
            _ => None,
        })
        .collect();
    // Once on the first tick, then only when `y` updated (every third
    // tick) to a different value: 2 on tick 3, still 2 on 6, 3 on 9, 0 on 12.
    assert_eq!(published, [(0, 0), (3, 2), (9, 3), (12, 0)]);

    // The payload publishes 4 little-endian bytes on wire/<channel>.
    struct Raw(Vec<(String, Vec<u8>)>);
    impl Device for Raw {
        fn log(&mut self, _line: &str) {}
        fn publish(&mut self, key: Key<'_>, data: &[u8]) -> Result<(), AbiError> {
            self.0.push((key.as_str().to_string(), data.to_vec()));
            Err(AbiError::Transport)
        }
        fn now_ms(&mut self) -> u32 {
            0
        }
        fn gpio_output(&mut self, _pin: u8) -> Result<(), AbiError> {
            Ok(())
        }
        fn gpio_input(&mut self, _pin: u8, _pull: HostPull) -> Result<(), AbiError> {
            Ok(())
        }
        fn gpio_write(&mut self, _pin: u8, _high: bool) -> Result<(), AbiError> {
            Ok(())
        }
        fn gpio_read(&mut self, _pin: u8) -> Result<bool, AbiError> {
            Ok(true)
        }
    }
    let mut g = wiring(10, &["a=gpio1", "k=-2", "v=mul(a,k)", "gpio2=v"]);
    text::set_trace(&mut g, &["v"]).unwrap();
    let (wasm, _) = build(&g, None).unwrap();
    let mut p = Payload::load(&wasm, Raw(Vec::new()), Limits::default()).unwrap();
    assert_eq!(p.device().0, [("wire/v".to_string(), vec![0xfe, 0xff, 0xff, 0xff])]);
    // A refused publication is not retried while the value stays.
    p.tick(10).unwrap();
    assert_eq!(p.device().0.len(), 1);
}

#[test]
fn tr2_traces_of_every_channel_match_the_reference_in_order() {
    let statements = [
        "a=gpio1", "b=gpio2@20ms", "r=adc26", "s=adc27@50ms",
        "both=and(a,b)", "d=sub(r,s)", "f=lowpass(d,shift=2)@20ms", "m=mean(r,n=5)",
        "e=rising(both)", "c=count(e)", "prev=delay(c)@30ms", "gpio5=prev", "pwm0.0=f",
    ];
    let mut g = wiring(10, &statements);
    text::set_trace::<&str>(&mut g, &[]).unwrap();
    assert!(text::command_line(&g).starts_with("mwctl wire --name w --trace a=gpio1 "));
    let history = compare(&g, 1500, noise(21));
    // The first tick publishes every channel, in update order.
    let first: Vec<&str> = history[0]
        .iter()
        .filter_map(|e| match e {
            Event::Publish(name, _) => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(first, ["a", "b", "r", "s", "both", "d", "f", "m", "e", "c", "prev"]);
    // A channel driven at 50 ms is published on those ticks only.
    for (tick, events) in history.iter().enumerate() {
        if events.iter().any(|e| matches!(e, Event::Publish(n, _) if n == "s")) {
            assert_eq!(tick % 5, 0);
        }
    }
    assert!(history.iter().flatten().filter(|e| matches!(e, Event::Publish(..))).count() > 3000);

    // A traced channel nothing reads is used, and a wiring that only
    // traces is not one that does nothing.
    let mut g = wiring(10, &["r=adc26"]);
    assert_eq!(check(&g, None).len(), 2);
    text::set_trace(&mut g, &["r"]).unwrap();
    assert!(check(&g, None).is_empty());
    assert_eq!(text::command_line(&g), "mwctl wire --name w --trace r=adc26");
    assert!(text::set_trace(&mut g, &["nope"]).unwrap_err().contains("no channel 'nope'"));
    assert!(text::set_trace(&mut g, &["adc26"]).unwrap_err().contains("no channel 'adc26'"));
    // Some channels: the command line names them.
    let mut g = wiring(20, &["r=adc26", "y=abs(r)@40ms", "gpio2=y"]);
    text::set_trace(&mut g, &["y"]).unwrap();
    assert_eq!(text::command_line(&g), "mwctl wire --name w --period 20 --trace y r=adc26 'y=abs(r)@40ms' gpio2=y");
}

#[test]
fn tr3_json_wirings_with_any_node_ids_run_like_their_statements() {
    // What a canvas saves: its own ids, one function driving two channels,
    // settings as JSON.
    let json = r#"{"name":"canvas","period_ms":20,"nodes":[
        {"id":"n1","kind":"analog","config":{"pin":26,"period_ms":40}},
        {"id":"raw","kind":"channel","config":{"trace":true}},
        {"id":"n2","kind":"hyst","config":{"lo":1000,"hi":2000}},
        {"id":"hot","kind":"channel"},
        {"id":"also","kind":"channel","config":{"trace":true}},
        {"id":"n3","kind":"pwm","config":{"timer":1,"channel":1,"freq_hz":500,"period_ms":60}},
        {"id":"n4","kind":"output","config":{"pin":4,"invert":true}},
        {"id":"n5","kind":"const","config":{"value":3}},
        {"id":"three","kind":"channel"},
        {"id":"n6","kind":"mul"},
        {"id":"prod","kind":"channel"}],
      "edges":[["n1","raw"],["raw","n2"],["n2","hot"],["n2","also"],["also","n4"],
               ["hot","n6"],["three","n6"],["n5","three"],["n6","prod"],["prod","n3"]]}"#;
    let g = Graph::from_json(json).unwrap();
    assert!(check(&g, None).is_empty(), "{:?}", check(&g, None));
    assert_eq!(
        text::format(&g),
        ["raw=adc26@40ms", "three=3", "hot=hyst(raw,lo=1000,hi=2000)", "also=hyst(raw,lo=1000,hi=2000)", "prod=mul(hot,three)", "pwm1.1=prod,freq=500@60ms", "gpio4=also,invert"]
    );
    let history = compare(&g, 300, noise(31));
    assert!(history.iter().flatten().any(|e| *e == Event::Pwm(1, 1, 3)));
    for bad in [
        r#"{"name":"w","nodes":[{"id":"a","kind":"lowpass","config":{"shift":300}}],"edges":[]}"#,
        r#"{"name":"w","nodes":[{"id":"a","kind":"lowpass","config":{"shfit":3}}],"edges":[]}"#,
        r#"{"name":"w","nodes":[{"id":"a","kind":"const","config":{"value":2147483648}}],"edges":[]}"#,
        r#"{"name":"w","nodes":[{"id":"a","kind":"channel","config":{"trace":1}}],"edges":[]}"#,
    ] {
        assert!(Graph::from_json(bad).unwrap_err().starts_with("wiring json:"), "{bad}");
    }
    let _ = Node::new("a", Kind::Channel, Config::default());
}
