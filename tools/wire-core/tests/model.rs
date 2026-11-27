//! The model-based blocks: expressions (`ex*`), the unscented Kalman
//! filter (`kf*`) and the control barrier function safety filter (`cb*`).
//!
//! As in `signal.rs`, the reference is [`wire_core::Sim`] and the compiled
//! payload runs in the device runtime against the same pseudo-random pins;
//! every traced channel must match on every tick. The blocks compute in
//! `f64`, so "match" here means the payload and the reference performed
//! the same IEEE operations: a difference in the last bit of an
//! intermediate shows up in a rounded output sooner or later, and two
//! tests amplify the last bit on purpose (`ex6` for expressions, `kf9`
//! for everything a filter computes).

use std::collections::BTreeMap;

use middleware::Key;
use wasm_host::{AbiError, Device, Limits, Payload, Pull as HostPull, Ticked};
use wire_core::expr::{self, Expr, Vars};
use wire_core::graph::{Config, Graph, Kind};
use wire_core::spec::kinds_json;
use wire_core::{Plan, Severity, Sim, build, check, real, text};

/// A traced channel's publication.
type Event = (String, i32);

/// A board for both sides: analog pins and what was published.
#[derive(Default)]
struct Board {
    analog: BTreeMap<u8, i32>,
    events: Vec<Event>,
}

impl Device for Board {
    fn log(&mut self, _line: &str) {}
    fn publish(&mut self, key: Key<'_>, data: &[u8]) -> Result<(), AbiError> {
        let name = key.as_str().strip_prefix("wire/").expect("a trace key is wire/<channel>");
        self.events.push((name.to_string(), i32::from_le_bytes(data.try_into().expect("4 bytes"))));
        Ok(())
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
        Ok(false)
    }
    fn adc_read(&mut self, pin: u8) -> Result<u16, AbiError> {
        Ok(self.analog.get(&pin).copied().unwrap_or(0) as u16)
    }
}

impl wire_core::Io for Board {
    fn adc_read(&mut self, pin: u8) -> i32 {
        self.analog.get(&pin).copied().unwrap_or(0)
    }
    fn publish(&mut self, channel: &str, value: i32) {
        self.events.push((channel.to_string(), value));
    }
}

/// A small linear congruential generator (Numerical Recipes' constants).
struct Lcg(u32);

impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        self.0 >> 8
    }
    /// Uniform in `-spread..=spread`.
    fn noise(&mut self, spread: i32) -> i32 {
        (self.next() % (2 * spread as u32 + 1)) as i32 - spread
    }
    /// Uniform in `lo..hi`.
    fn real(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * f64::from(self.next()) / 16_777_216.0
    }
}

fn wiring(period: u32, statements: &[&str]) -> Graph {
    let mut g = text::parse("w", period, statements).unwrap_or_else(|e| panic!("{e}"));
    text::set_trace::<&str>(&mut g, &[]).unwrap();
    g
}

fn errors(statements: &[&str]) -> Vec<String> {
    let g = text::parse("w", 10, statements).unwrap_or_else(|e| panic!("{e}"));
    check(&g, None).into_iter().filter(|i| i.severity == Severity::Error).map(|i| i.message).collect()
}

/// What a side-by-side run measured.
struct Run {
    /// Every channel's value after each tick.
    values: Vec<BTreeMap<String, i32>>,
    /// The payload's size in bytes.
    bytes: usize,
    /// The most fuel one call into the payload used.
    fuel: u64,
}

impl Run {
    fn at(&self, tick: usize, channel: &str) -> i32 {
        self.values[tick][channel]
    }
    fn last(&self, channel: &str) -> i32 {
        self.values[self.values.len() - 1][channel]
    }
}

/// Runs the payload and the reference side by side for `ticks` base ticks
/// with the analog pins `pins(tick)` gives (the same for both), comparing
/// every publication of every tick; `each` sees the reference after each.
fn compare(
    graph: &Graph,
    ticks: u32,
    mut pins: impl FnMut(u32) -> Vec<(u8, i32)>,
    mut each: impl FnMut(u32, &Sim),
) -> Run {
    let issues = check(graph, None);
    assert!(issues.iter().all(|i| i.severity != Severity::Error), "{issues:#?}");
    let plan = Plan::new(graph).unwrap_or_else(|issues| panic!("{issues:#?}"));
    let (wasm, _) = build(graph, None).unwrap_or_else(|issues| panic!("{issues:#?}"));
    let mut sim = Sim::new(&plan);
    let mut reference = Board::default();
    let mut first = Board::default();
    let set = |board: &mut Board, values: &[(u8, i32)]| {
        for &(pin, value) in values {
            assert!((0..=4095).contains(&value), "an ADC reads 0 to 4095, not {value}");
            board.analog.insert(pin, value);
        }
    };
    let mut now = pins(0);
    set(&mut first, &now);
    let mut payload = Payload::load(&wasm, first, Limits::default()).expect("the runtime loads the payload");
    let mut current: BTreeMap<String, i32> = BTreeMap::new();
    let mut values = Vec::new();
    for tick in 0..ticks {
        if tick > 0 {
            now = pins(tick);
            set(payload.device(), &now);
            assert_eq!(payload.tick(tick * graph.period_ms).expect("no trap"), Ticked::Ran, "tick {tick}");
        }
        set(&mut reference, &now);
        sim.step(&mut reference);
        let want = std::mem::take(&mut reference.events);
        let got = std::mem::take(&mut payload.device().events);
        assert_eq!(got, want, "tick {tick} of {:?}", text::format(graph));
        current.extend(want);
        values.push(current.clone());
        each(tick, &sim);
    }
    Run { values, bytes: wasm.len(), fuel: payload.fuel_stats().max_call }
}

const XU: Vars = Vars { states: 3, controls: 2, dt: true };

#[test]
fn ex1_expressions_parse_with_the_usual_precedence_and_print_back() {
    for (text, canonical) in [
        ("1+2*3", "1+2*3"),
        ("(1+2)*3", "(1+2)*3"),
        ("x0-x1-x2", "x0-x1-x2"),
        ("x0-(x1-x2)", "x0-(x1-x2)"),
        ("x0/x1/x2", "x0/x1/x2"),
        ("x0/(x1*x2)", "x0/(x1*x2)"),
        ("x0*(x1/x2)", "x0*(x1/x2)"),
        ("-x0*x1", "-x0*x1"),
        ("-(x0+1)", "-(x0+1)"),
        ("--x0", "-(-x0)"),
        ("x0+-x1", "x0+-x1"),
        ("  x0 +  dt * u1 ", "x0+dt*u1"),
        ("sq( x0 )+min(x0, 1e-3)", "sq(x0)+min(x0,0.001)"),
        ("max(sin(x0),cos(x1))/sqrt(abs(x2))", "max(sin(x0),cos(x1))/sqrt(abs(x2))"),
        ("pi", "3.141592653589793"),
        (".5e1+2.50+3E-1", "5+2.5+0.3"),
        ("((x0))", "x0"),
    ] {
        let e = expr::parse(text, &XU).unwrap_or_else(|e| panic!("{text}: {e}"));
        assert_eq!(e.to_string(), canonical, "{text}");
        assert_eq!(expr::parse(canonical, &XU).unwrap(), e, "{canonical} parses to the same tree");
    }
    assert_eq!(expr::parse("1+2*3", &XU).unwrap().value(&[], &[], 0.0), 7.0);
    assert_eq!(expr::parse("(1+2)*3", &XU).unwrap().value(&[], &[], 0.0), 9.0);
    assert_eq!(expr::parse("8-4-2", &XU).unwrap().value(&[], &[], 0.0), 2.0);
    assert_eq!(expr::parse("8/4/2", &XU).unwrap().value(&[], &[], 0.0), 1.0);
    assert_eq!(expr::parse("-2*-3", &XU).unwrap().value(&[], &[], 0.0), 6.0);
    // A list is separated by ';'.
    let list = expr::parse_list("x0+dt*x1; x1 ;u0", &XU).unwrap();
    assert_eq!(list.iter().map(Expr::to_string).collect::<Vec<_>>(), ["x0+dt*x1", "x1", "u0"]);
    assert_eq!(list.iter().map(Expr::controls).collect::<Vec<_>>(), [0, 0, 1]);
    assert_eq!(expr::parse_numbers("1e-6; 0.5;-3 ").unwrap(), [1e-6, 0.5, -3.0]);
}

#[test]
fn ex2_a_mistake_is_reported_with_its_expression_and_column() {
    let vars = Vars { states: 2, controls: 1, dt: true };
    let at = |text: &str| {
        let e = expr::parse_list(text, &vars).unwrap_err();
        (e.index, e.column, e.message)
    };
    let says = |text: &str, index: usize, column: usize, needle: &str| {
        let (i, c, message) = at(text);
        assert!((i, c) == (index, column) && message.contains(needle), "{}: {i}, {c}: {message}", &text[..text.len().min(60)]);
    };
    says("x0 + y", 1, 6, "'y' is not a name here (x0 to x1, u0, dt, pi)");
    says("x0*x2", 1, 4, "there is no state x2: this block has 2");
    says("x0; x1+u1", 2, 8, "there is no control u1: this block has 1");
    says("sin(x0, x1)", 1, 1, "sin takes 1 argument(s), not 2");
    says("min(x0)", 1, 1, "min takes 2 argument(s), not 1");
    says("x0; x1; exp(x0)", 3, 9, "'exp' is not a function (sqrt, abs, sin, cos, sq, min, max)");
    says("(x0+1", 1, 6, "a ')' is missing");
    says("x0+1)", 1, 5, "this ')' has no '('");
    says("x0 x1", 1, 4, "an operator (+ - * /) is missing");
    says("x0 +", 1, 5, "an expression ends here");
    says("x0;;x1", 2, 4, "an expression ends here");
    says("x0; x1 $ 2", 2, 8, "'$' does not belong in an expression");
    says("x0, x1", 1, 3, "a ',' belongs between a function's arguments only");
    says("1e999", 1, 1, "'1e999' is not a number");
    says(&format!("{}x0{}", "(".repeat(40), ")".repeat(40)), 1, 34, "nested too deep");
    says(&vec!["x0"; 200].join("+"), 1, 384, "too large (more than 256 terms)");
    says(&"x0+".repeat(400), 1, 1025, "longer than 1024 characters");
    assert!(expr::parse("dt", &Vars { states: 1, controls: 0, dt: false }).unwrap_err().message.contains("'dt' is not a name here (x0, pi)"));
    assert!(expr::parse("x0; x1", &vars).unwrap_err().message.contains("one expression is expected"));
    let e = expr::parse_numbers("1; 2e-3; x").unwrap_err();
    assert_eq!((e.index, e.column, e.message.as_str()), (3, 10, "'x' is not a number"));
    assert_eq!(e.to_string(), "expression 3, column 10: 'x' is not a number");
    assert!(expr::parse_numbers("1;;2").is_err() && expr::parse_numbers("inf").is_err());
}

#[test]
fn ex3_expressions_evaluate_to_hand_values() {
    let value = |text: &str| expr::parse(text, &XU).unwrap().value(&[3.0, -4.0, 0.25], &[10.0, 2.0], 0.02);
    assert_eq!(value("x0*x1+x2"), -11.75);
    assert_eq!(value("sq(x1)-x0/4"), 15.25);
    assert_eq!(value("sqrt(abs(x1*4))"), 4.0);
    assert_eq!(value("min(x0,u1)+max(x1,x2)"), 2.25);
    assert_eq!(value("x0+dt*u0"), 3.2);
    assert_eq!(value("-x1/(u1-2)"), f64::INFINITY);
    assert!(value("sqrt(x1)").is_nan());
    assert_eq!(value("cos(0)"), 1.0);
    assert_eq!(value("sin(0)"), 0.0);
    assert!((value("sin(pi/2)") - 1.0).abs() < 1e-15);
    assert!(value("cos(pi/2)").abs() < 1e-15);
    assert!((value("sin(x0)*sin(x0)+cos(x0)*cos(x0)") - 1.0).abs() < 1e-15);
    // min and max are WebAssembly's: NaN wins, and -0 is below +0.
    assert!(real::fmin(f64::NAN, 1.0).is_nan() && real::fmax(1.0, f64::NAN).is_nan());
    assert!(real::fmin(0.0, -0.0).is_sign_negative() && real::fmax(-0.0, 0.0).is_sign_positive());
    // A result becomes a channel value rounded half away from zero, saturated.
    for (v, want) in [(0.5, 1), (-0.5, -1), (0.49999999999999994, 0), (2.5, 3), (-2.5, -3), (1e300, i32::MAX), (-1e300, i32::MIN), (f64::NAN, 0), (2147483647.4, i32::MAX), (-2147483648.5, i32::MIN), (f64::INFINITY, i32::MAX), (-0.2, 0)] {
        assert_eq!(real::round_sat(v), want, "{v}");
    }
}

#[test]
fn ex4_derivatives_match_central_differences() {
    let texts = [
        "x0*x1+sq(x2)",
        "sin(x0)*cos(x1)",
        "x0/(1+sq(x1))",
        "sqrt(1+sq(x0)+sq(x1))",
        "abs(x0-1.3)*x1",
        "min(x0,x1)*max(x1,x2)",
        "sq(sin(x0*x1))-x2/x0",
        "-(x0-x1)*(x0+x1)/2",
        "max(abs(x0-x1),sq(x2))/sqrt(x0)",
        "2-x0-x1/4+u0*x2+dt",
    ];
    let mut lcg = Lcg(41);
    let mut checked = 0;
    for text in texts {
        let e = expr::parse(text, &XU).unwrap();
        let d: Vec<Expr> = (0..3).map(|i| e.diff(i)).collect();
        for _ in 0..200 {
            let x = [lcg.real(0.5, 2.5), lcg.real(0.5, 2.5), lcg.real(0.5, 2.5)];
            let u = [lcg.real(-1.0, 1.0), 0.0];
            for (i, d) in d.iter().enumerate() {
                let step = 1e-6;
                let (mut up, mut down) = (x, x);
                up[i] += step;
                down[i] -= step;
                // A kink between the two points is not a derivative to compare.
                let kink = ["abs", "min", "max"].iter().any(|f| text.contains(f))
                    && (d.value(&up, &u, 0.01) != d.value(&down, &u, 0.01));
                if kink {
                    continue;
                }
                let numeric = (e.value(&up, &u, 0.01) - e.value(&down, &u, 0.01)) / (2.0 * step);
                let symbolic = d.value(&x, &u, 0.01);
                assert!((symbolic - numeric).abs() <= 1e-7 * (1.0 + symbolic.abs()), "d/dx{i} of {text} at {x:?}: {symbolic} vs {numeric} ({d})");
                checked += 1;
            }
        }
    }
    assert!(checked > 5500, "{checked}");
    // The derivative is simplified, and piecewise at a kink.
    let d = |text: &str, i: usize| expr::parse(text, &XU).unwrap().diff(i).to_string();
    assert_eq!(d("x0*x1", 0), "x1");
    assert_eq!(d("2*x0+3", 0), "2");
    assert_eq!(d("2-x0", 0), "-1");
    assert_eq!(d("x1+u0+dt", 0), "0");
    assert_eq!(d("sq(x0)", 0), "2*x0");
    assert_eq!(d("sin(2*x0)", 0), "cos(2*x0)*2");
    assert_eq!(d("cos(x0)", 0), "-sin(x0)");
    assert_eq!(d("x0/x1", 0), "1/x1");
    assert_eq!(d("x0/x1", 1), "-x0/(x1*x1)");
    assert_eq!(d("sqrt(x0)", 0), "1/(2*sqrt(x0))");
    assert_eq!(d("abs(x0)", 0), "ifle(0,x0,1,-1)");
    assert_eq!(d("min(x0,x1)", 0), "ifle(x0,x1,1,0)");
    assert_eq!(d("max(x0,2)", 1), "0");
    // At the kink: abs takes the right side, min and max their first argument.
    let at = |text: &str, i: usize, x: [f64; 3]| expr::parse(text, &XU).unwrap().diff(i).value(&x, &[], 0.0);
    assert_eq!(at("abs(x0)", 0, [0.0, 0.0, 0.0]), 1.0);
    assert_eq!((at("min(x0,x1)", 0, [1.0, 1.0, 0.0]), at("min(x0,x1)", 1, [1.0, 1.0, 0.0])), (1.0, 0.0));
    assert_eq!((at("max(x0,x1)", 0, [1.0, 1.0, 0.0]), at("max(x0,x1)", 1, [1.0, 1.0, 0.0])), (1.0, 0.0));
}

#[test]
fn ex5_sin_and_cos_are_within_their_documented_bound() {
    let mut lcg = Lcg(5);
    let mut worst: f64 = 0.0;
    for range in [0.8, 7.0, 100.0, 1e5] {
        for _ in 0..50_000 {
            let x = lcg.real(-range, range);
            worst = worst.max((real::sin(x) - libm::sin(x)).abs());
            worst = worst.max((real::cos(x) - libm::cos(x)).abs());
        }
    }
    // Documented: within 1e-15 of the true value for |x| <= 1e5 (libm is
    // itself within 1e-16 of it).
    eprintln!("sin/cos: largest difference from libm for |x| <= 1e5: {worst:e}");
    assert!(worst < 5e-16, "{worst:e}");
    for k in -8..=8 {
        let x = f64::from(k) * std::f64::consts::FRAC_PI_2;
        assert!((real::sin(x) - libm::sin(x)).abs() < 5e-16 && (real::cos(x) - libm::cos(x)).abs() < 5e-16, "{k}");
    }
    assert_eq!((real::sin(0.0), real::cos(0.0)), (0.0, 1.0));
    assert!(real::sin(f64::NAN).is_nan() && real::sin(f64::INFINITY).is_nan() && real::cos(f64::NEG_INFINITY).is_nan());
    // Far outside the bound it is still a number in -1..=1, and the same every time.
    let far = real::sin(1e15);
    assert!(far.abs() <= 1.0 && far.to_bits() == real::sin(1e15).to_bits());
}

#[test]
fn ex6_a_compiled_expression_equals_the_reference_to_the_last_bit() {
    // The expression E (of order 1) sits in a barrier's drift as
    // sin(1e11 * E) - 2, so the filter's output is 2 - sin(1e11 * E) -
    // 1e-9 * x0 in millionths: one unit in the last place of E turns the
    // sine by 2e-5 radians, twenty times the output's resolution.
    let expressions = [
        "x0+x1*x2",
        "x0-x1/x2",
        "-x0*x1-x2",
        "sq(x0)+sq(x1)-sq(x2)",
        "sqrt(abs(x0))*x1",
        "abs(x0-x1)/(1+abs(x2))",
        "min(x0,x1)+max(x1,x2)*min(x2,0.5)",
        "sin(x0)+cos(x1)",
        "sin(x0*x1)*cos(x2/7)",
        "sin(1000*x0)+cos(1e5*x1)",
        "x0/(x1-x1)",
        "sqrt(x0)+sqrt(x1)+sqrt(x2)",
        "(x0+x1+x2)/3*pi",
        "sq(sq(x0))/1e3-x1*x1*x1",
        "max(sin(x0),cos(x0))*min(sin(x1),cos(x2))",
        "1/(1+sq(x0))+x1/(2+cos(x2))",
    ];
    for (round, e) in expressions.iter().enumerate() {
        let block = format!(
            "u,flag=cbf(zero,a,b,c,n=3,h=\"x0\",f=\"sin(1e11*({e}))-2; 0; 0\",g=\"1;0;0\",alpha=0.000000001,scale=1000000)"
        );
        let statements = [
            "r1=adc26",
            "r2=adc27",
            "r3=adc28",
            "a=scale(r1,mul=977)",
            "wide=scale(r2,mul=1021)",
            "middle=2000000",
            "b=sub(wide,middle)",
            "c=scale(r3,mul=733,div=3)",
            "zero=0",
            &block,
        ];
        let g = wiring(10, &statements);
        let mut lcg = Lcg(600 + round as u32);
        let run = compare(&g, 400, |_| vec![(26, lcg.noise(2047) + 2047), (27, lcg.noise(2047) + 2047), (28, lcg.noise(2047) + 2047)], |_, _| {});
        // The output really is that function of E, and it moves about.
        let distinct: std::collections::BTreeSet<i32> = run.values.iter().map(|v| v["u"]).collect();
        assert!(distinct.len() > 150 || e.contains("x1-x1"), "{e}: {} values", distinct.len());
        let plan = Plan::new(&g).unwrap();
        let ex = expr::parse(e, &XU).unwrap();
        let mut sim = Sim::new(&plan);
        let mut board = Board::default();
        board.analog.extend([(26, 1000), (27, 300), (28, 4000)]);
        sim.step(&mut board);
        let x = [f64::from(sim.channel("a").unwrap()) / 1e6, f64::from(sim.channel("b").unwrap()) / 1e6, f64::from(sim.channel("c").unwrap()) / 1e6];
        let a = real::sin(1e11 * ex.value(&x, &[], 0.0)) - 2.0 + 1e-9 * x[0];
        if a.is_finite() {
            assert_eq!(sim.channel("u").unwrap(), real::round_sat((0.0 + -a * 1.0 / 1.0) * 1e6), "{e}");
            assert_eq!(sim.channel("flag"), Some(1));
        }
    }
}

/// A plain Kalman filter in `f64`, for linear models with one measurement
/// of the first state: `x = F x + B u`, `z = x0 + noise`.
struct Kalman {
    f: Vec<Vec<f64>>,
    b: Vec<f64>,
    q: Vec<f64>,
    r: f64,
    x: Vec<f64>,
    p: Vec<Vec<f64>>,
}

impl Kalman {
    fn step(&mut self, z: f64, u: f64) {
        let n = self.x.len();
        let x: Vec<f64> = (0..n).map(|i| (0..n).map(|j| self.f[i][j] * self.x[j]).sum::<f64>() + self.b[i] * u).collect();
        let fp: Vec<Vec<f64>> = (0..n).map(|i| (0..n).map(|j| (0..n).map(|k| self.f[i][k] * self.p[k][j]).sum()).collect()).collect();
        let mut p: Vec<Vec<f64>> = (0..n).map(|i| (0..n).map(|j| (0..n).map(|k| fp[i][k] * self.f[j][k]).sum()).collect()).collect();
        for (i, row) in p.iter_mut().enumerate() {
            row[i] += self.q[i];
        }
        let s = p[0][0] + self.r;
        let k: Vec<f64> = (0..n).map(|i| p[i][0] / s).collect();
        self.x = (0..n).map(|i| x[i] + k[i] * (z - x[0])).collect();
        self.p = (0..n).map(|i| (0..n).map(|j| p[i][j] - k[i] * s * k[j]).collect()).collect();
    }

    /// The filter's state in the block's layout: x, then P's lower triangle.
    fn words(&self) -> Vec<f64> {
        let mut words = self.x.clone();
        for i in 0..self.x.len() {
            words.extend(&self.p[i][..=i]);
        }
        words
    }
}

fn close(got: &[f64], want: &[f64], tolerance: f64, what: &str) {
    for (g, w) in got.iter().zip(want) {
        assert!((g - w).abs() <= tolerance * (1.0 + w.abs()), "{what}: {got:?} vs {want:?}");
    }
}

#[test]
fn kf1_a_constant_is_estimated_from_noisy_measurements() {
    let g = wiring(10, &["z=adc26", "est,sd=ukf(z,n=1,f=\"x0\",h=\"x0\",q=\"0\",r=\"0.01\")"]);
    let mut lcg = Lcg(1);
    let mut kalman = Kalman { f: vec![vec![1.0]], b: vec![0.0], q: vec![0.0], r: 0.01, x: vec![0.0], p: vec![vec![1.0]] };
    let mut zs = Vec::new();
    let run = compare(
        &g,
        3000,
        |_| {
            zs.push(2000 + lcg.noise(170));
            vec![(26, *zs.last().unwrap())]
        },
        |tick, sim| {
            // A UKF of a linear model is the Kalman filter.
            kalman.step(f64::from(sim.channel("z").unwrap()) / 1000.0, 0.0);
            close(&sim.reals("est.fn").unwrap()[..2], &kalman.words(), 1e-9, &format!("tick {tick}"));
            assert_eq!(sim.faults("est.fn"), Some(0));
        },
    );
    // The estimate converges to the mean of the measurements (2.0), and
    // the standard deviation shrinks as 0.1 / sqrt(k).
    let mean = zs.iter().map(|&z| f64::from(z)).sum::<f64>() / zs.len() as f64;
    assert!((f64::from(run.last("est")) - mean).abs() <= 1.0, "{} vs {mean}", run.last("est"));
    assert!((run.last("est") - 2000).abs() <= 6, "{}", run.last("est"));
    assert_eq!((run.at(0, "sd"), run.at(99, "sd"), run.last("sd")), (100, 10, 2));
    assert!(run.values.windows(2).all(|w| w[1]["sd"] <= w[0]["sd"]));
    eprintln!("ukf n=1 m=1: payload {} bytes, {} fuel per tick at most", run.bytes, run.fuel);
}

#[test]
fn kf2_a_constant_velocity_model_finds_the_slope() {
    // Position rises 2 counts per 10 ms tick: 0.2 units per second.
    let filter = "pos,vel=ukf(z,n=2,f=\"x0+dt*x1; x1\",h=\"x0\",q=\"1e-8;1e-7\",r=\"0.0004\",x0=\"0.5;0\",p0=\"1;1\")";
    let g = wiring(10, &["z=adc26", filter]);
    let mut lcg = Lcg(2);
    let mut kalman = Kalman {
        f: vec![vec![1.0, 0.01], vec![0.0, 1.0]],
        b: vec![0.0, 0.0],
        q: vec![1e-8, 1e-7],
        r: 0.0004,
        x: vec![0.5, 0.0],
        p: vec![vec![1.0, 0.0], vec![0.0, 1.0]],
    };
    let run = compare(
        &g,
        1700,
        |tick| vec![(26, 500 + 2 * tick as i32 + lcg.noise(35))],
        |tick, sim| {
            kalman.step(f64::from(sim.channel("z").unwrap()) / 1000.0, 0.0);
            close(&sim.reals("pos.fn").unwrap()[..5], &kalman.words(), 1e-6, &format!("tick {tick}"));
            assert_eq!(sim.faults("pos.fn"), Some(0));
        },
    );
    assert!((run.last("vel") - 200).abs() <= 8, "velocity {}", run.last("vel"));
    assert!((run.last("pos") - (500 + 2 * 1699)).abs() <= 8, "position {}", run.last("pos"));
    eprintln!("ukf n=2 m=1 (constant velocity): payload {} bytes, {} fuel per tick at most", run.bytes, run.fuel);
    // The same wiring without the filter: what the rest of the payload costs.
    let bare = build(&wiring(10, &["z=adc26"]), None).unwrap().0.len();
    eprintln!("the same wiring without the filter: {bare} bytes");
    assert!(run.bytes < 4096 && run.fuel < 20_000);
}

#[test]
fn kf3_a_nonlinear_measurement_still_converges() {
    // z = x^2 with x = 1.5: the measurement is 2.25.
    let g = wiring(10, &["z=adc26", "x,sd=ukf(z,n=1,f=\"x0\",h=\"sq(x0)\",q=\"1e-7\",r=\"0.0025\",x0=\"1\",p0=\"0.25\")"]);
    let mut lcg = Lcg(3);
    let run = compare(&g, 2000, |_| vec![(26, 2250 + lcg.noise(85))], |_, sim| assert_eq!(sim.faults("x.fn"), Some(0)));
    assert!((run.last("x") - 1500).abs() <= 10, "{}", run.last("x"));
    assert!(run.last("sd") < 20 && run.at(0, "sd") > 100);

    // A range measurement: z = sqrt(x^2 + 1) with x = 3, so z = sqrt(10),
    // with spread sigma points (alpha below 1) and a moving target.
    let filter = "x,v=ukf(z,n=2,f=\"x0+dt*x1; x1\",h=\"sqrt(sq(x0)+1)\",q=\"1e-7;1e-6\",r=\"0.0004\",x0=\"2;0\",p0=\"1;0.1\",alpha=0.5,kappa=1)";
    let g = wiring(10, &["z=adc26", filter]);
    let truth = |tick: u32| 3.0 + 0.0002 * f64::from(tick);
    let run = compare(
        &g,
        2000,
        |tick| vec![(26, (1000.0 * (truth(tick) * truth(tick) + 1.0).sqrt()).round() as i32 + lcg.noise(30))],
        |_, sim| assert_eq!(sim.faults("x.fn"), Some(0)),
    );
    assert!((f64::from(run.last("x")) - 1000.0 * truth(1999)).abs() <= 25.0, "{}", run.last("x"));
    assert!((run.last("v") - 20).abs() <= 12, "{}", run.last("v"));
}

#[test]
fn kf4_a_control_input_drives_the_prediction() {
    // x' = u: the state integrates the control, a square wave of +-1.5
    // around the ADC's 2.0.
    let statements = [
        "z=adc26",
        "raw=adc27",
        "two=2000",
        "u=sub(raw,two)",
        "x,sd=ukf(z,u,n=1,f=\"x0+dt*u0\",h=\"x0\",q=\"1e-6\",r=\"0.01\",x0=\"2\")",
        "blind,bsd=ukf(z,n=1,f=\"x0\",h=\"x0\",q=\"1e-6\",r=\"0.01\",x0=\"2\")",
    ];
    let g = wiring(10, &statements);
    let mut lcg = Lcg(4);
    let control = |tick: u32| if (tick / 100).is_multiple_of(2) { 1500 } else { -1500 };
    let mut truth = vec![2000.0];
    let mut kalman = Kalman { f: vec![vec![1.0]], b: vec![0.01], q: vec![1e-6], r: 0.01, x: vec![2.0], p: vec![vec![1.0]] };
    let mut worst = (0.0f64, 0.0f64);
    let run = compare(
        &g,
        2000,
        |tick| {
            // The state the measurement of this tick sees has moved by the control of this tick.
            let x = truth[truth.len() - 1] + 0.01 * f64::from(control(tick));
            truth.push(x);
            vec![(26, x.round() as i32 + lcg.noise(170)), (27, 2000 + control(tick))]
        },
        |tick, sim| {
            kalman.step(f64::from(sim.channel("z").unwrap()) / 1000.0, f64::from(sim.channel("u").unwrap()) / 1000.0);
            close(&sim.reals("x.fn").unwrap()[..2], &kalman.words(), 1e-9, &format!("tick {tick}"));
            assert_eq!((sim.faults("x.fn"), sim.faults("blind.fn")), (Some(0), Some(0)));
        },
    );
    for tick in 300..2000 {
        let x = truth[tick + 1];
        worst.0 = worst.0.max((f64::from(run.at(tick, "x")) - x).abs());
        worst.1 = worst.1.max((f64::from(run.at(tick, "blind")) - x).abs());
    }
    // With the control the estimate stays within the noise; without it lags the ramp.
    assert!(worst.0 < 40.0 && worst.1 > 3.0 * worst.0, "{worst:?}");
}

#[test]
fn kf5_a_filter_runs_at_its_own_period() {
    // Base 10 ms, filter every 100 ms: dt is 0.1 s, and the slope of
    // 1 count per tick is 0.1 units per second.
    let filter = "pos,vel=ukf(z,n=2,f=\"x0+dt*x1; x1\",h=\"x0\",q=\"1e-8;1e-7\",r=\"0.0004\",p0=\"4;1\")@100ms";
    let g = wiring(10, &["z=adc26", filter, "fast=diff(pos)"]);
    let mut lcg = Lcg(5);
    let mut kalman = Kalman {
        f: vec![vec![1.0, 0.1], vec![0.0, 1.0]],
        b: vec![0.0, 0.0],
        q: vec![1e-8, 1e-7],
        r: 0.0004,
        x: vec![0.0, 0.0],
        p: vec![vec![4.0, 0.0], vec![0.0, 1.0]],
    };
    let mut changed = Vec::new();
    let mut before = Vec::new();
    let run = compare(
        &g,
        3000,
        |tick| vec![(26, 300 + tick as i32 + lcg.noise(35))],
        |tick, sim| {
            let now = sim.reals("pos.fn").unwrap().to_vec();
            if now != before {
                changed.push(tick);
            }
            before = now;
            if tick % 10 == 0 {
                kalman.step(f64::from(sim.channel("z").unwrap()) / 1000.0, 0.0);
                close(&sim.reals("pos.fn").unwrap()[..5], &kalman.words(), 1e-6, &format!("tick {tick}"));
            }
        },
    );
    assert!(changed.len() == 300 && changed.iter().all(|tick| tick % 10 == 0), "{changed:?}");
    assert!((run.last("vel") - 100).abs() <= 6, "velocity {}", run.last("vel"));
    // A faster reader sees the estimate held between the filter's updates.
    assert!((0..3000).filter(|tick| tick % 10 != 0).all(|tick| run.at(tick, "fast") == 0));
}

const FOUR: &str = "ukf(MEAS,n=4,f=\"x0+dt*x2; x1+dt*x3; x2; x3\",h=\"x0; x1; x2; x3\",q=\"1e-7;1e-7;1e-6;1e-6\",r=\"0.0004;0.0004;0.01;0.01\",p0=\"1;1;1;1\")";

#[test]
fn kf6_four_states_four_measurements_and_three_filters_fit_in_one_tick() {
    // Planar constant velocity, all four states measured, estimates and
    // standard deviations out; three of them due on every tick.
    let names = |p: &str| format!("{p}x,{p}y,{p}vx,{p}vy,{p}sx,{p}sy,{p}svx,{p}svy");
    let a = format!("{}={}", names("a"), FOUR.replace("MEAS", "z0,z1,z2,z3"));
    let b = format!("{}={}", names("b"), FOUR.replace("MEAS", "z1,z0,z3,z2"));
    let c = format!("{}={}", names("c"), FOUR.replace("MEAS", "z3,z2,z1,z0").replace("x0+dt*x2", "x0+dt*x2*cos(x3)"));
    let statements = ["z0=adc26", "z1=adc27", "z2=adc28", "z3=adc29", &a, &b, &c];
    let g = wiring(10, &statements);
    let mut lcg = Lcg(6);
    let run = compare(
        &g,
        700,
        |tick| {
            let t = tick as i32;
            vec![(26, 400 + 3 * t + lcg.noise(30)), (27, 500 + 2 * t + lcg.noise(30)), (28, 300 + lcg.noise(150)), (29, 200 + lcg.noise(150))]
        },
        |_, sim| assert_eq!((sim.faults("ax.fn"), sim.faults("bx.fn"), sim.faults("cx.fn")), (Some(0), Some(0), Some(0))),
    );
    // x from z0 (slope 3 counts per tick: 0.3 units/s, as measured by z2),
    // y from z1 (0.2 units/s, as z3); the second filter has them swapped.
    assert!((run.last("ax") - (400 + 3 * 699)).abs() <= 15, "{}", run.last("ax"));
    assert!((run.last("ay") - (500 + 2 * 699)).abs() <= 15, "{}", run.last("ay"));
    assert!((run.last("avx") - 300).abs() <= 25 && (run.last("avy") - 200).abs() <= 25, "{} {}", run.last("avx"), run.last("avy"));
    assert_eq!((run.last("bx"), run.last("by")), (run.last("ay"), run.last("ax")));
    assert!(run.last("asx") > 0 && run.last("asx") < run.at(0, "asx") && run.last("asvx") < run.at(0, "asvx"));
    eprintln!("three ukf n=4 m=4 with 8 outputs: payload {} bytes, {} fuel per tick at most (the budget is {})", run.bytes, run.fuel, middleware::wasm_abi::FUEL_PER_CALL);
    assert!(run.fuel < middleware::wasm_abi::FUEL_PER_CALL / 4, "{}", run.fuel);
    let plan = Plan::new(&g).unwrap();
    for step in plan.steps.iter().filter(|s| s.kind == Kind::Ukf) {
        let program = step.program.as_ref().unwrap();
        let locals = program.locals();
        eprintln!("  {}: {} operations, {} real and {} condition locals", step.id, program.ops.len(), locals.reals, locals.conditions);
    }
}

#[test]
fn kf7_an_unsound_update_resets_the_filter_and_counts_a_fault() {
    // u0/u0 is 1, except NaN when the control reads 0: exactly those
    // updates are unsound, in the payload as in the reference.
    let g = wiring(10, &["z=adc26", "u=adc27", "x,sd=ukf(z,u,n=1,f=\"x0+0*u0/u0\",h=\"x0\",q=\"1e-6\",r=\"0.01\",x0=\"1.25\",p0=\"0.5\")"]);
    let mut lcg = Lcg(7);
    let mut zeros = 0;
    let mut controls = Vec::new();
    let run = compare(
        &g,
        3000,
        |_| {
            let u = if lcg.next().is_multiple_of(23) { 0 } else { 1 + lcg.next() as i32 % 4000 };
            controls.push(u);
            vec![(26, 2000 + lcg.noise(170)), (27, u)]
        },
        |tick, sim| {
            if sim.channel("u") == Some(0) {
                zeros += 1;
                // Reset: x0 (the prediction is not finite) and diag(p0).
                assert_eq!(sim.reals("x.fn").unwrap()[..2], [1.25, 0.5], "tick {tick}");
                assert_eq!((sim.channel("x"), sim.channel("sd")), (Some(1250), Some(707)));
            }
            assert_eq!(sim.faults("x.fn"), Some(zeros), "tick {tick}");
        },
    );
    assert!(zeros > 80 && controls.iter().filter(|&&u| u == 0).count() == zeros as usize, "{zeros}");
    assert!((run.last("x") - 2000).abs() < 150);

    // A variance of 0 has no Cholesky factor: every update is a fault, and
    // the estimate is the prediction (here the state it started from).
    let g = wiring(10, &["z=adc26", "x=ukf(z,n=1,f=\"x0+dt\",h=\"x0\",q=\"0\",r=\"0.01\",p0=\"0\")"]);
    let run = compare(&g, 50, |_| vec![(26, 2000)], |tick, sim| assert_eq!(sim.faults("x.fn"), Some(tick + 1)));
    assert_eq!((run.at(0, "x"), run.last("x")), (10, 500));
    // A model that overflows: the state stops being finite, the filter starts again from x0.
    let g = wiring(10, &["z=adc26", "x=ukf(z,n=1,f=\"x0*1e200\",h=\"x0\",q=\"1\",r=\"1\",x0=\"3\")"]);
    let mut faults = 0;
    let run = compare(&g, 50, |_| vec![(26, 2000)], |_, sim| faults = sim.faults("x.fn").unwrap());
    assert!(faults >= 10 && run.values.iter().all(|v| v["x"] != 0));
}

#[test]
fn kf9_a_chaotic_model_shows_any_last_bit_difference() {
    // The transition multiplies the state by 1e9 inside a sine, so one
    // unit in the last place of anything the filter computes (a Cholesky
    // pivot, a weight, a gain) is a different estimate two ticks later.
    // The payload still equals the reference on every tick.
    let filter = "a,b,sa,sb=ukf(z0,z1,u,n=2,f=\"sin(1e9*x0+x1)+u0/8; cos(1e9*x1-x0)\",h=\"x0; x0+x1\",q=\"1e-3;2e-3\",r=\"0.3;0.2\",p0=\"0.5;0.25\",alpha=0.7,kappa=0.5,scale=1000000)";
    let g = wiring(10, &["r0=adc26", "r1=adc27", "u=adc28", "z0=scale(r0,mul=300)", "z1=scale(r1,mul=-250)", filter]);
    let mut lcg = Lcg(90);
    let mut faults = 0;
    let run = compare(
        &g,
        2500,
        |_| vec![(26, lcg.noise(2047) + 2047), (27, lcg.noise(2047) + 2047), (28, lcg.noise(2047) + 2047)],
        |_, sim| faults = sim.faults("a.fn").unwrap(),
    );
    let distinct: std::collections::BTreeSet<i32> = run.values.iter().map(|v| v["a"]).collect();
    assert!(distinct.len() > 2400 && faults == 0, "{} values, {faults} faults", distinct.len());
}

#[test]
fn kf8_a_filter_with_a_wrong_model_is_an_error_naming_the_node() {
    let model = |rest: &str| format!("x,v=ukf(z,{rest})");
    let e = |statement: &str| errors(&["z=adc26", "c=adc27", statement]);
    let good = "n=2,f=\"x0+dt*x1; x1\",h=\"x0\",q=\"0;0\",r=\"1\"";
    assert!(e(&model(good)).is_empty());
    assert_eq!(e("x,v=ukf(z,n=2)"), [
        "x.fn (ukf) has no f: write it, such as f=\"x0+dt*x1; x1\"",
        "x.fn (ukf) has no h: write it, such as h=\"x0\"",
        "x.fn (ukf) has no q: write it, such as q=\"1e-6; 1e-4\"",
        "x.fn (ukf) has no r: write it, such as r=\"0.01\"",
    ]);
    assert_eq!(e(&model("n=5,f=\"x0\",h=\"x0\",q=\"0\",r=\"1\"")), ["x.fn (ukf) has n 5; it must be 1 to 4"]);
    assert_eq!(e(&model(&good.replace("x0+dt*x1; x1", "x0+dt*x1"))), ["x.fn (ukf) has 1 expression(s) in f; it needs 2, one per state, separated by ;"]);
    assert_eq!(e(&model(&good.replace("x0+dt*x1;", "x0+dt*y;"))), ["x.fn (ukf) has a mistake in f, expression 1, column 7: 'y' is not a name here (x0 to x1, dt, pi)"]);
    assert_eq!(e(&model(&good.replace("; x1\"", "; x1+u0\""))), ["x.fn (ukf) has a mistake in f, expression 2, column 14: 'u0' is not a name here (x0 to x1, dt, pi)"]);
    assert_eq!(e(&model(&good.replace("h=\"x0\"", "h=\"x0+dt\""))), ["x.fn (ukf) has a mistake in h, expression 1, column 4: 'dt' is not a name here (x0 to x1, pi)"]);
    assert_eq!(e(&model(&good.replace("q=\"0;0\"", "q=\"0\""))), ["x.fn (ukf) has 1 number(s) in q; it needs 2, separated by ;"]);
    assert_eq!(e(&model(&good.replace("r=\"1\"", "r=\"-1\""))), ["x.fn (ukf) has -1 in r; a variance is not negative"]);
    assert_eq!(e(&model(&good.replace("r=\"1\"", "r=\"one\""))), ["x.fn (ukf) has a mistake in r, expression 1, column 1: 'one' is not a number"]);
    assert_eq!(e(&model(&format!("{good},x0=\"1;2;3\""))), ["x.fn (ukf) has 3 number(s) in x0; it needs 2, separated by ;"]);
    assert_eq!(e(&model(&format!("{good},alpha=2"))), ["x.fn (ukf) has alpha 2; it must be 0.001 to 1"]);
    assert_eq!(e(&model(&format!("{good},kappa=-2"))), ["x.fn (ukf) has kappa -2; n + kappa must be greater than 0"]);
    assert_eq!(e(&model(&format!("{good},scale=0"))), ["x.fn (ukf) has scale 0; it must be 1 to 1000000"]);
    // Inputs: one per expression of h, then the controls an expression may name.
    assert_eq!(e(&model(&good.replace("h=\"x0\"", "h=\"x0;x1\""))), [
        "x.fn (ukf) reads 1 channel(s); it needs its 2 measurement(s) (one per expression of h) and then 0 to 4 controls",
        "x.fn (ukf) has 1 number(s) in r; it needs 2, separated by ;",
    ]);
    assert!(e(&format!("x,v=ukf(z,c,{})", good.replace("; x1\"", "; x1+u0\""))).is_empty());
    assert_eq!(e(&format!("x,v=ukf(z,c,{})", good.replace("; x1\"", "; x1+u1\""))), ["x.fn (ukf) has a mistake in f, expression 2, column 14: there is no control u1: this block has 1 (x0 to x1, u0, dt, pi)"]);
    // Outputs: the n estimates, or 2n.
    assert_eq!(e(&format!("x=ukf(z,{good})")), ["x.fn (ukf) drives 1 channel(s); it drives its 2 state estimate(s), or 4 with their standard deviations"]);
    assert_eq!(e(&format!("x,v,s=ukf(z,{good})")).len(), 1);
    assert!(e(&format!("x,v,s,t=ukf(z,{good})")).is_empty());
    // The largest model the limits on a setting's text allow still
    // compiles, loads and runs within one call's fuel.
    let long = |first: &str| format!("{first}{}", "+x1*x2-x3/7+x0*x3-x2".repeat(12));
    let (f, h) = (["x0", "x1", "x2", "x3"].map(long).join(";"), ["x3", "x2", "x1", "x0"].map(long).join(";"));
    assert!(f.len() > 950 && f.len() <= wire_core::expr::MAX_TEXT);
    let huge = format!("a,b,c,d=ukf(z,z2,z3,z4,n=4,f=\"{f}\",h=\"{h}\",q=\"0;0;0;0\",r=\"1;1;1;1\")");
    let g = wiring(10, &["z=adc26", "z2=adc27", "z3=adc28", "z4=adc29", &huge]);
    let run = compare(&g, 20, |tick| vec![(26, 100 + tick as i32), (27, 200), (28, 300), (29, 400)], |_, _| {});
    let plan = Plan::new(&g).unwrap();
    let operations = plan.steps.iter().find_map(|s| s.program.as_ref()).unwrap().ops.len();
    eprintln!("the largest ukf (n=4, m=4, nearly 1 KiB each of f and h): {operations} operations, payload {} bytes, {} fuel per tick at most", run.bytes, run.fuel);
    assert!(operations < wire_core::model::MAX_OPS && run.fuel < middleware::wasm_abi::FUEL_PER_CALL / 4);
    // The settings belong to these kinds only.
    let mut g = text::parse("w", 10, &["z=adc26", "y=abs(z)"]).unwrap();
    g.nodes.iter_mut().find(|n| n.id == "y.fn").unwrap().config.f = Some("x0".into());
    g.nodes.iter_mut().find(|n| n.id == "adc26").unwrap().config.alpha = Some(1.0);
    let messages: Vec<String> = check(&g, None).into_iter().filter(|i| i.severity == Severity::Error).map(|i| i.message).collect();
    assert_eq!(messages, ["adc26 is an analog input (analog) and takes no 'alpha' setting", "y.fn is a function (abs) and takes no 'f' setting"]);
}

/// A plant integrated in the wiring at the base rate (10 ms): the state
/// `x` (in thousandths) moves by `u / 100` per tick, that is `xdot = u`.
fn integrator(x: &str, u: &str) -> [String; 3] {
    [format!("{x}_step=scale({u},div=100)"), format!("{x}_next=add({x},{x}_step)"), format!("{x}=delay({x}_next)")]
}

#[test]
fn cb1_a_closed_loop_stays_inside_the_barrier() {
    // xdot = u, a nominal control of 0.5 toward a wall at x = 2.
    let plant = integrator("x", "u");
    let mut statements = vec!["push=500", "u,flag=cbf(push,x,n=1,h=\"2-x0\",f=\"0\",g=\"1\",alpha=5)"];
    statements.extend(plant.iter().map(String::as_str));
    let g = wiring(10, &statements);
    let run = compare(&g, 1500, |_| vec![], |_, _| {});
    let mut modified = None;
    for (tick, v) in run.values.iter().enumerate() {
        assert!(v["x"] <= 2001, "tick {tick}: x = {}", v["x"]);
        // hdot + alpha h >= 0 is u <= 5 (2 - x): inactive up to x = 1.9.
        match v["flag"] {
            0 => assert_eq!(v["u"], 500, "tick {tick}: the nominal control passes unchanged"),
            1 => {
                modified.get_or_insert(tick);
                assert!(v["u"] < 500 && v["x"] > 1900, "tick {tick}");
                assert_eq!(v["u"], 5 * (2000 - v["x"]), "tick {tick}");
            }
            flag => panic!("tick {tick}: flag {flag}"),
        }
    }
    // x rises 5 per tick: it passes 1.9 at tick 381.
    assert_eq!(modified, Some(381));
    assert!(run.values[..381].iter().all(|v| v["flag"] == 0 && v["u"] == 500));
    // (The plant moves in whole thousandths, so the approach stops when
    // the allowed speed rounds to no step.)
    assert!(run.last("x") >= 1975, "it does approach the wall: {}", run.last("x"));
    eprintln!("cbf n=1 m=1 closed loop: payload {} bytes, {} fuel per tick at most", run.bytes, run.fuel);

    // Without the filter the same loop runs through the wall.
    let plant = integrator("x", "push");
    let mut statements = vec!["push=500"];
    statements.extend(plant.iter().map(String::as_str));
    let run = compare(&wiring(10, &statements), 1500, |_| vec![], |_, _| {});
    assert!(run.last("x") > 7000 && run.values.iter().any(|v| v["x"] > 2001));
}

#[test]
fn cb2_two_controls_are_projected_onto_the_constraint() {
    // xdot = (u0, u1), safe inside the unit disc; the nominal control
    // pushes outward and to the side, with noise.
    let (px, py) = (integrator("x", "ux"), integrator("y", "uy"));
    let mut statements = vec![
        "r1=adc26",
        "r2=adc27",
        "off=1500",
        "nx=sub(r1,off)",
        "ny=sub(r2,off)",
        "ux,uy,flag=cbf(nx,ny,x,y,n=2,h=\"1-sq(x0)-sq(x1)\",f=\"0;0\",g=\"1;0;0;1\",alpha=4)",
    ];
    statements.extend(px.iter().chain(&py).map(String::as_str));
    let g = wiring(10, &statements);
    let mut lcg = Lcg(8);
    let run = compare(&g, 3000, |_| vec![(26, 2100 + lcg.noise(600)), (27, 1800 + lcg.noise(600))], |_, _| {});
    let mut flags = [0; 3];
    for (tick, v) in run.values.iter().enumerate() {
        let (x, y) = (f64::from(v["x"]) / 1000.0, f64::from(v["y"]) / 1000.0);
        // Within the plant's time step and rounding: the barrier is a
        // continuous-time condition applied every 10 ms.
        assert!(x * x + y * y <= 1.01, "tick {tick}: ({x}, {y})");
        flags[v["flag"] as usize] += 1;
        let (nx, ny) = (f64::from(v["nx"]), f64::from(v["ny"]));
        let (ux, uy) = (f64::from(v["ux"]), f64::from(v["uy"]));
        if v["flag"] == 0 {
            assert_eq!((v["ux"], v["uy"]), (v["nx"], v["ny"]), "tick {tick}");
        } else {
            // The correction is along the gradient of h, (-2x, -2y): it
            // changes the control by the least that meets the constraint.
            let (cx, cy) = (ux - nx, uy - ny);
            assert!((cx * y - cy * x).abs() <= 1.5 * (x.abs() + y.abs()) + 1.5, "tick {tick}: ({cx}, {cy}) at ({x}, {y})");
            assert!(cx * x + cy * y < 0.0, "tick {tick}: inward");
        }
    }
    assert!(flags[0] > 100 && flags[1] > 1000 && flags[2] == 0, "{flags:?}");
    // Unfiltered, the state leaves the disc at once.
    let (px, py) = (integrator("x", "nx"), integrator("y", "ny"));
    let mut statements = vec!["r1=adc26", "r2=adc27", "off=1500", "nx=sub(r1,off)", "ny=sub(r2,off)"];
    statements.extend(px.iter().chain(&py).map(String::as_str));
    let mut lcg = Lcg(8);
    let run = compare(&wiring(10, &statements), 3000, |_| vec![(26, 2100 + lcg.noise(600)), (27, 1800 + lcg.noise(600))], |_, _| {});
    assert!(run.last("x") > 5000);
}

#[test]
fn cb3_a_barrier_the_control_cannot_reach_is_flagged() {
    // Position x0 and velocity x1 with a force input: xdot = (x1, u).
    // A position limit has relative degree 2: its gradient meets no
    // column of g, so the filter can do nothing and says so.
    let statements = ["r=adc26", "pos=adc27", "vel=adc28", "u,flag=cbf(r,pos,vel,n=2,h=\"2-x0\",f=\"x1;0\",g=\"0;1\",alpha=3)"];
    let mut lcg = Lcg(9);
    let run = compare(&wiring(10, &statements), 400, |_| vec![(26, lcg.noise(2000) + 2000), (27, lcg.noise(2000) + 2000), (28, lcg.noise(2000) + 2000)], |_, _| {});
    let mut flags = [0; 3];
    for v in &run.values {
        // a = -x1 + 3 (2 - x0): where it is negative the constraint is violated and cannot be helped.
        let a = -f64::from(v["vel"]) / 1000.0 + 3.0 * (2.0 - f64::from(v["pos"]) / 1000.0);
        assert_eq!(v["flag"], if a >= 0.0 { 0 } else { 2 });
        assert_eq!(v["u"], v["r"], "the nominal control passes either way");
        flags[v["flag"] as usize] += 1;
    }
    assert!(flags[0] > 50 && flags[2] > 50 && flags[1] == 0, "{flags:?}");
    // The higher-order barrier the documentation suggests does act: it
    // brakes before the wall.
    let statements = ["r=adc26", "pos=adc27", "vel=adc28", "u,flag=cbf(r,pos,vel,n=2,h=\"2-x0-x1/2\",f=\"x1;0\",g=\"0;1\",alpha=3)"];
    let mut lcg = Lcg(9);
    let run = compare(&wiring(10, &statements), 400, |_| vec![(26, lcg.noise(2000) + 2000), (27, lcg.noise(2000) + 2000), (28, lcg.noise(2000) + 2000)], |_, _| {});
    assert!(run.values.iter().all(|v| v["flag"] != 2) && run.values.iter().any(|v| v["flag"] == 1 && v["u"] < v["r"]));
}

#[test]
fn cb4_the_safe_control_is_limited_to_its_range() {
    // u <= 5 (2 - x) from the barrier, then -1 <= u <= 1.
    let statements = ["r=adc26", "x=adc27", "off=2000", "nom=sub(r,off)", "u,flag=cbf(nom,x,n=1,h=\"2-x0\",f=\"0\",g=\"1\",alpha=5,umin=-1,umax=1)", "free=cbf(nom,x,n=1,h=\"2-x0\",f=\"0\",g=\"1\",alpha=5)"];
    let mut lcg = Lcg(10);
    let run = compare(&wiring(10, &statements), 600, |_| vec![(26, lcg.noise(2000) + 2000), (27, lcg.noise(600) + 1900)], |_, _| {});
    let mut seen = [false; 4];
    for v in &run.values {
        let bound = 5 * (2000 - v["x"]);
        assert_eq!(v["free"], v["nom"].min(bound));
        assert_eq!(v["u"], v["free"].clamp(-1000, 1000));
        assert_eq!(v["flag"], i32::from(v["nom"] > bound), "the flag is the barrier's, not the limit's");
        seen[0] |= v["u"] == 1000 && v["free"] > 1000;
        seen[1] |= v["u"] == -1000 && v["free"] < -1000 && v["flag"] == 1;
        seen[2] |= v["u"] == v["free"] && v["flag"] == 1;
        seen[3] |= v["u"] == v["nom"] && v["flag"] == 0;
    }
    assert_eq!(seen, [true; 4]);
    // Exactly on the constraint the nominal control passes: x = 1.5 and
    // u = 4 (2 - 1.5) = 2 are exact, so s is exactly 0.
    let run = compare(&wiring(10, &["nom=2000", "x=1500", "u,flag=cbf(nom,x,n=1,h=\"2-x0\",f=\"0\",g=\"1\",alpha=4)"]), 2, |_| vec![], |_, _| {});
    assert_eq!((run.last("u"), run.last("flag")), (2000, 0));
    assert_eq!(errors(&["a=adc26", "b=adc27", "u=cbf(a,b,n=1,h=\"x0\",f=\"0\",g=\"1\",umin=2,umax=1)"]), ["u.fn (cbf) has umin 2 above umax 1; umin must not exceed umax"]);
}

#[test]
fn cb5_statements_with_quoted_expressions_round_trip() {
    let statements = [
        "z=adc26",
        "push=500",
        "pos,vel=ukf(z,n=2,f=\"x0+dt*x1; x1\",h=\"x0\",q=\"1e-6;1e-4\",r=\"0.01\")@20ms",
        "u,flag=cbf(push,pos,vel,n=2,h=\"2 - x0 - x1/4\",f=\"x1; 0\",g=\"0; 1\",alpha=2.5,umin=-3,umax=0.75,scale=100)",
        "est,sd=ukf(z,u,n=1,f=\"x0+dt*u0\",h=\"sqrt(sq(x0)+1)\",q=\"0\",r=\"1\",x0=\"0.5\",p0=\"4\",alpha=0.5,beta=1.5,kappa=1)@100ms",
        "gpio2=flag",
    ];
    let g = text::parse("w", 10, &statements).unwrap();
    assert!(check(&g, None).iter().all(|i| i.severity == Severity::Warning), "{:?}", check(&g, None));
    assert_eq!(text::format(&g), statements);
    assert_eq!(text::parse("w", 10, &text::format(&g)).unwrap(), g);
    assert_eq!(Graph::from_json(&g.to_json()).unwrap(), g);
    // Ordered outputs: the edges out of the node, in order; its id is the first channel's.
    assert_eq!(g.targets("pos.fn").collect::<Vec<_>>(), ["pos", "vel"]);
    assert_eq!(g.sources("u.fn").collect::<Vec<_>>(), ["push", "pos", "vel"]);
    assert_eq!(g.targets("u.fn").collect::<Vec<_>>(), ["u", "flag"]);
    assert_eq!(
        g.node("u.fn").unwrap().config,
        Config {
            n: Some(2),
            h: Some("2 - x0 - x1/4".into()),
            f: Some("x1; 0".into()),
            g: Some("0; 1".into()),
            alpha: Some(2.5),
            umin: Some(-3.0),
            umax: Some(0.75),
            scale: Some(100),
            ..Config::default()
        }
    );
    let json = g.to_json();
    assert!(json.contains(r#"{"id":"pos.fn","kind":"ukf","config":{"n":2,"f":"x0+dt*x1; x1","h":"x0","q":"1e-6;1e-4","r":"0.01","period_ms":20}}"#), "{json}");
    assert!(json.contains(r#""config":{"n":2,"f":"x1; 0","h":"2 - x0 - x1/4","g":"0; 1","alpha":2.5,"umin":-3.0,"umax":0.75,"scale":100}"#), "{json}");
    // The command line quotes them for a shell.
    assert_eq!(
        text::command_line(&g),
        "mwctl wire --name w z=adc26 push=500 \
         'pos,vel=ukf(z,n=2,f=\"x0+dt*x1; x1\",h=\"x0\",q=\"1e-6;1e-4\",r=\"0.01\")@20ms' \
         'u,flag=cbf(push,pos,vel,n=2,h=\"2 - x0 - x1/4\",f=\"x1; 0\",g=\"0; 1\",alpha=2.5,umin=-3,umax=0.75,scale=100)' \
         'est,sd=ukf(z,u,n=1,f=\"x0+dt*u0\",h=\"sqrt(sq(x0)+1)\",q=\"0\",r=\"1\",x0=\"0.5\",p0=\"4\",alpha=0.5,beta=1.5,kappa=1)@100ms' \
         gpio2=flag"
    );
    assert_eq!(text::shell_word("a='b c'"), r"'a='\''b c'\'''");
    // Defaults are not written; quoted text keeps '@', ',' and '=' to itself.
    let g = text::parse("w", 10, &["a=adc1", "b=adc2", "u=cbf(a,b,n=1,h=\"min(x0, 2)\",f=\"0\",g=\"1\",alpha=1,scale=1000)@10ms"]).unwrap();
    assert_eq!(text::format(&g), ["a=adc1", "b=adc2", "u=cbf(a,b,n=1,h=\"min(x0, 2)\",f=\"0\",g=\"1\")"]);
    assert_eq!(g.sources("u.fn").collect::<Vec<_>>(), ["a", "b"]);

    let bad = |s: &[&str]| text::parse("w", 10, s).unwrap_err();
    assert!(bad(&["a,b=gpio1"]).contains("only a ukf or a cbf drives several channels"));
    assert!(bad(&["a,b=not(c)"]).contains("only a ukf or a cbf drives several channels"));
    assert!(bad(&["a,b=5"]).contains("only a ukf or a cbf drives several channels"));
    assert!(bad(&["a,a=ukf(z,n=1)"]).contains("channel a is driven twice"));
    assert!(bad(&["a,B=ukf(z,n=1)"]).contains("'B' is not a channel name"));
    assert!(bad(&["a,b=ukf(z,b,n=1)"]).contains("channel b cannot be its own input"));
    assert!(bad(&["a=ukf(z,n=1,f=x0)"]).contains("f holds expressions or numbers; write it in double quotes, f=\"x0+dt*x1; x1\""));
    assert!(bad(&["a=ukf(z,n=1,f=\"x0)"]).contains("write it in double quotes"));
    assert!(bad(&["a=ukf(z,n=1,alpha=big)"]).contains("'big' is not a number"));
    assert!(bad(&["a=ukf(z,n=1,alpha=inf)"]).contains("'inf' is not a number"));
    assert!(bad(&["a=ukf(z,n=1,g=\"1\")"]).contains("ukf takes n, f, h, q, r, x0, p0, alpha, beta, kappa, scale, not 'g'"));
    assert!(bad(&["x=adc1", "a=ukf(x,n=1)", "a,b=ukf(x,n=1)"]).contains("channel a already has a source"));
}

#[test]
fn cb6_a_barrier_with_a_wrong_model_is_an_error_naming_the_node() {
    let e = |statement: &str| errors(&["a=adc26", "b=adc27", "c=adc28", statement]);
    let good = "n=2,h=\"1-sq(x0)-sq(x1)\",f=\"0;0\",g=\"1;0\"";
    assert!(e(&format!("u=cbf(a,b,c,{good})")).is_empty());
    assert!(e(&format!("u,flag=cbf(a,b,c,{good})")).is_empty());
    assert_eq!(e("u=cbf(a,b,c,n=2)"), [
        "u.fn (cbf) has no h: write it, such as h=\"2-x0\"",
        "u.fn (cbf) has no f: write it, such as f=\"0\"",
        "u.fn (cbf) has no g: write it, such as g=\"1\"",
    ]);
    assert_eq!(e(&format!("u=cbf(a,b,{good})")), ["u.fn (cbf) reads 2 channel(s); it needs 1 or 2 nominal controls and then its 2 state(s)"]);
    assert_eq!(e(&format!("u,v,w,x=cbf(a,b,c,{good})")), ["u.fn (cbf) drives 4 channel(s); it drives its 1 safe control(s), or 2 with the flag"]);
    assert_eq!(e(&format!("u=cbf(a,b,c,{})", good.replace("g=\"1;0\"", "g=\"1;0;0;1\""))), ["u.fn (cbf) has 4 expression(s) in g; it needs 2, a row per state with one entry per control, separated by ;"]);
    assert_eq!(e(&format!("u=cbf(a,b,c,{})", good.replace("f=\"0;0\"", "f=\"0\""))), ["u.fn (cbf) has 1 expression(s) in f; it needs 2, one per state, separated by ;"]);
    assert_eq!(e(&format!("u=cbf(a,b,c,{})", good.replace("h=\"1-sq(x0)-sq(x1)\"", "h=\"1-x0;x1\""))), ["u.fn (cbf) has 2 expression(s) in h; it needs 1, the one barrier function, separated by ;"]);
    assert_eq!(e(&format!("u=cbf(a,b,c,{})", good.replace("sq(x1)", "sq(x2)"))), ["u.fn (cbf) has a mistake in h, expression 1, column 13: there is no state x2: this block has 2 (x0 to x1, pi)"]);
    assert_eq!(e(&format!("u=cbf(a,b,c,{})", good.replace("f=\"0;0\"", "f=\"0;u0\""))), ["u.fn (cbf) has a mistake in f, expression 2, column 3: 'u0' is not a name here (x0 to x1, pi)"]);
    assert_eq!(e(&format!("u=cbf(a,b,c,{good},alpha=0)")), ["u.fn (cbf) has alpha 0; it must be 0.000000001 to 1000000000"]);
    // What the node computes is in the plan, for the reference and the compiler alike.
    let g = text::parse("w", 10, &["a=adc26", "b=adc27", "c=adc28", &format!("u,flag=cbf(a,b,c,{good})")]).unwrap();
    let plan = Plan::new(&g).unwrap();
    let step = plan.steps.iter().find(|s| s.kind == Kind::Cbf).unwrap();
    let program = step.program.as_ref().unwrap();
    assert_eq!((program.outputs.len(), program.stores.len(), program.faults), (2, 0, None));
    assert!(!program.uses_sin() && program.ops.len() < 60, "{}", program.ops.len());
    // The table describes both kinds to a page.
    let kinds = kinds_json();
    assert!(kinds.contains(r#"{"kind":"cbf","class":"function","inputs":[2,6],"outputs":[1,3],"stateful":false,"params":[{"name":"n","type":"int","min":1,"max":4,"default":null,"required":true},{"name":"h","type":"text","min":null,"max":null,"default":null,"required":true,"hint":"2-x0"},"#));
    assert!(kinds.contains(r#"{"name":"alpha","type":"real","min":0.000000001,"max":1000000000,"default":1,"required":false},{"name":"umin","type":"real","min":null,"max":null,"default":null,"required":false}"#));
    assert!(kinds.contains(r#"{"kind":"ukf","class":"function","inputs":[1,8],"outputs":[1,8],"stateful":true,"params":[{"name":"n","type":"int","min":1,"max":4,"default":null,"required":true},{"name":"f","type":"text","min":null,"max":null,"default":null,"required":true,"hint":"x0+dt*x1; x1"},"#));
    assert!(kinds.contains(r#"{"name":"x0","type":"text","min":null,"max":null,"default":null,"required":false,"hint":"0; 0"}"#));
    assert!(kinds.contains(r#"{"name":"alpha","type":"real","min":0.001,"max":1,"default":1,"required":false},{"name":"beta","type":"real","min":null,"max":null,"default":2,"required":false}"#));
    assert!(kinds.contains(r#"{"name":"scale","type":"int","min":1,"max":1000000,"default":1000,"required":false}"#));
    let parsed: serde_json::Value = serde_json::from_str(&kinds).unwrap();
    assert!(parsed.as_array().unwrap().iter().all(|k| k["outputs"].as_array().is_some_and(|o| o.len() == 2)));
}
