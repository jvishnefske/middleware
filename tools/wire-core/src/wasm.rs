//! A [`Plan`] compiled to a payload for the middleware ABI.
//!
//! The module is written out byte by byte; no compiler runs. It imports
//! only what it calls: `log` and `set_tick` always; `gpio_input` and
//! `gpio_read` for a digital input; `gpio_output` and `gpio_write` for a
//! digital output; `adc_read` for an analog input and `pwm_open` and
//! `pwm_set` for a PWM output (ABI 3); `publish` for a traced channel. A
//! wiring of digital pins therefore still loads on every base firmware
//! with the payload runtime. It exports `memory` (one fixed 64 KiB page:
//! a 4-byte publish scratch, the log lines, the trace keys), `init` and
//! `tick`.
//!
//! Everything a wiring remembers is a mutable global: the tick number,
//! one `i32` per channel, up to two `i32` of state per stateful function
//! (what each holds is in [`crate::plan::Sim`]), the level or duty each
//! sink last wrote (-1 before the first, so the first tick writes all),
//! and per traced channel an `i64` holding the value last published
//! (2^32 before the first, which no `i32` equals).
//!
//! * `init` configures every digital input with its pull and every
//!   digital output, opens each PWM timer once, logs a line for a pin or
//!   timer the board refuses, sets the tick to the base period, logs
//!   `wire <name>: N in, M ch, K out` (N digital and analog input pins, M
//!   channels, K output pins) and runs tick 0.
//! * `tick` runs one base tick ([`crate::plan`]): each step in update
//!   order, inside `if tick % every == 0` when it is slower than the base
//!   (consecutive steps of one rate share the test); then the due delays
//!   capture; then the tick number goes up by one (wrapping, tested
//!   unsigned, exactly as the reference does).
//!
//! A digital input is `gpio_read(pin) > 0` and an analog one
//! `max(adc_read(pin), 0)`, so a read error is 0. A traced channel is
//! marked published whatever `publish` returns: a refused publication is
//! not retried until the value changes again.
//!
//! No loops, no recursion, no allocation: one tick is a fixed number of
//! instructions, far below the runtime's fuel per call.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::graph::Kind;
use crate::plan::{PWM_FULL, Plan, Step, pin_counts, pwm_freq};
use crate::real::{Locals, Op, Program, Val, sin_program};

/// Where the 4 bytes handed to `publish` live in linear memory.
const SCRATCH: i32 = 0;
/// Where the log lines and trace keys start in linear memory.
const DATA_BASE: u32 = 16;

fn uleb(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn sleb(out: &mut Vec<u8>, mut v: i64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        let done = (v == 0 && byte & 0x40 == 0) || (v == -1 && byte & 0x40 != 0);
        if done {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn name(out: &mut Vec<u8>, s: &str) {
    uleb(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
}

fn section(module: &mut Vec<u8>, id: u8, body: &[u8]) {
    module.push(id);
    uleb(module, body.len() as u32);
    module.extend_from_slice(body);
}

// Opcodes.
const END: u8 = 0x0b;
const IF: u8 = 0x04;
const ELSE: u8 = 0x05;
const VOID: u8 = 0x40;
const CALL: u8 = 0x10;
const DROP: u8 = 0x1a;
const SELECT: u8 = 0x1b;
const LOCAL_GET: u8 = 0x20;
const LOCAL_SET: u8 = 0x21;
const LOCAL_TEE: u8 = 0x22;
const GLOBAL_GET: u8 = 0x23;
const GLOBAL_SET: u8 = 0x24;
const I32_STORE: u8 = 0x36;
const I32_CONST: u8 = 0x41;
const I64_CONST: u8 = 0x42;
const I32_EQZ: u8 = 0x45;
const I32_EQ: u8 = 0x46;
const I32_NE: u8 = 0x47;
const I32_LT_S: u8 = 0x48;
const I32_GT_S: u8 = 0x4a;
const I32_LE_S: u8 = 0x4c;
const I32_GE_S: u8 = 0x4e;
const I64_NE: u8 = 0x52;
const I32_ADD: u8 = 0x6a;
const I32_SUB: u8 = 0x6b;
const I32_MUL: u8 = 0x6c;
const I32_DIV_S: u8 = 0x6d;
const I32_REM_U: u8 = 0x70;
const I32_AND: u8 = 0x71;
const I32_OR: u8 = 0x72;
const I32_XOR: u8 = 0x73;
const I32_SHR_S: u8 = 0x75;
const I64_MUL: u8 = 0x7e;
const I64_DIV_S: u8 = 0x7f;
const I32_WRAP_I64: u8 = 0xa7;
const I64_EXTEND_I32_S: u8 = 0xac;
const F64_CONST: u8 = 0x44;
const F64_EQ: u8 = 0x61;
const F64_NE: u8 = 0x62;
const F64_LT: u8 = 0x63;
const F64_LE: u8 = 0x65;
const F64_GE: u8 = 0x66;
const F64_ABS: u8 = 0x99;
const F64_NEG: u8 = 0x9a;
const F64_FLOOR: u8 = 0x9c;
const F64_TRUNC: u8 = 0x9d;
const F64_SQRT: u8 = 0x9f;
const F64_ADD: u8 = 0xa0;
const F64_SUB: u8 = 0xa1;
const F64_MUL: u8 = 0xa2;
const F64_DIV: u8 = 0xa3;
const F64_MIN: u8 = 0xa4;
const F64_MAX: u8 = 0xa5;
const F64_COPYSIGN: u8 = 0xa6;
const I32_TRUNC_F64_S: u8 = 0xaa;
const F64_CONVERT_I32_S: u8 = 0xb7;
const I32: u8 = 0x7f;
const I64: u8 = 0x7e;
const F64: u8 = 0x7c;

// Type indices.
const T_LOG: u32 = 0; // (i32, i32) -> ()
const T_ONE: u32 = 1; // (i32) -> i32
const T_TWO: u32 = 2; // (i32, i32) -> i32
const T_VOID: u32 = 3; // () -> ()
const T_TICK: u32 = 4; // (i32) -> ()
const T_THREE: u32 = 5; // (i32, i32, i32) -> i32
const T_FOUR: u32 = 6; // (i32, i32, i32, i32) -> i32
const T_SIN: u32 = 7; // (f64, f64) -> f64
const T_ROUND: u32 = 8; // (f64) -> i32

/// Every function type, by index: parameters and results.
const TYPES: [(&[u8], &[u8]); 9] = [
    (&[I32, I32], &[]),
    (&[I32], &[I32]),
    (&[I32, I32], &[I32]),
    (&[], &[]),
    (&[I32], &[]),
    (&[I32, I32, I32], &[I32]),
    (&[I32, I32, I32, I32], &[I32]),
    (&[F64, F64], &[F64]),
    (&[F64], &[I32]),
];

// The scan function's locals.
const V: u32 = 0; // the value a step computed
const A: u32 = 1; // scratch
const B: u32 = 2; // scratch

struct Code(Vec<u8>);

impl Code {
    fn op(&mut self, op: u8) -> &mut Self {
        self.0.push(op);
        self
    }
    fn constant(&mut self, v: i32) -> &mut Self {
        self.0.push(I32_CONST);
        sleb(&mut self.0, i64::from(v));
        self
    }
    fn real(&mut self, v: f64) -> &mut Self {
        self.0.push(F64_CONST);
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn constant64(&mut self, v: i64) -> &mut Self {
        self.0.push(I64_CONST);
        sleb(&mut self.0, v);
        self
    }
    fn index(&mut self, op: u8, i: u32) -> &mut Self {
        self.0.push(op);
        uleb(&mut self.0, i);
        self
    }
    fn get(&mut self, global: u32) -> &mut Self {
        self.index(GLOBAL_GET, global)
    }
    fn set(&mut self, global: u32) -> &mut Self {
        self.index(GLOBAL_SET, global)
    }
    fn local(&mut self, local: u32) -> &mut Self {
        self.index(LOCAL_GET, local)
    }
    fn when(&mut self) -> &mut Self {
        self.op(IF).op(VOID)
    }
}

/// The imports a plan needs, in import order, as `(name, type)`.
fn imports(plan: &Plan) -> Vec<(&'static str, u32)> {
    let has = |kind: Kind| plan.steps.iter().any(|s| s.kind == kind);
    let mut list = alloc::vec![("log", T_LOG), ("set_tick", T_ONE)];
    if has(Kind::Input) {
        list.push(("gpio_input", T_TWO));
        list.push(("gpio_read", T_ONE));
    }
    if has(Kind::Output) {
        list.push(("gpio_output", T_ONE));
        list.push(("gpio_write", T_TWO));
    }
    if has(Kind::Analog) {
        list.push(("adc_read", T_ONE));
    }
    if has(Kind::Pwm) {
        list.push(("pwm_open", T_TWO));
        list.push(("pwm_set", T_THREE));
    }
    if plan.channels.iter().any(|c| c.trace) {
        list.push(("publish", T_FOUR));
    }
    list
}

/// Where everything lives: global indices and import indices.
struct Layout {
    imports: Vec<(&'static str, u32)>,
    /// The tick number.
    tick: u32,
    /// Channel `k` is global `channel + k`.
    channel: u32,
    /// Step `i`'s state words are globals `state[i]` and `state[i] + 1`.
    state: Vec<u32>,
    /// What sink step `i` last wrote.
    written: Vec<u32>,
    /// What traced channel `k` last published (an `i64`).
    published: Vec<u32>,
    /// The first real state word of step `i` (an `f64` each).
    real: Vec<u32>,
    /// How each model step keeps its values in locals.
    locals: Vec<Option<Locals>>,
    /// The scan function's first condition local and first real local,
    /// and how many of each it has.
    pools: [(u32, u32); 2],
    /// The helper functions, when a step needs them.
    sin_fn: u32,
    round_fn: u32,
    /// Every global's type and initial value (an `f64`'s bits), in index order.
    globals: Vec<(u8, i64)>,
    /// Where traced channel `k`'s key is in memory: `(address, length)`.
    keys: Vec<(i32, i32)>,
}

/// How many state words a kind keeps.
fn state_words(kind: Kind) -> u32 {
    match kind {
        Kind::Mean | Kind::Debounce | Kind::Count => 2,
        k if k.stateful() => 1,
        _ => 0,
    }
}

impl Layout {
    fn new(plan: &Plan) -> Self {
        let mut globals: Vec<(u8, i64)> = Vec::new();
        let mut take = |ty: u8, init: i64, count: u32| {
            let at = globals.len() as u32;
            globals.extend(core::iter::repeat_n((ty, init), count as usize));
            at
        };
        let tick = take(I32, 0, 1);
        let channel = take(I32, 0, plan.channels.len() as u32);
        let state = plan.steps.iter().map(|s| take(I32, 0, state_words(s.kind))).collect();
        let written = plan
            .steps
            .iter()
            .map(|s| take(I32, -1, u32::from(matches!(s.kind, Kind::Output | Kind::Pwm))))
            .collect();
        let published =
            plan.channels.iter().map(|c| take(I64, 1 << 32, u32::from(c.trace))).collect();
        let real = plan
            .steps
            .iter()
            .map(|s| {
                let init = s.program.as_ref().map_or(&[][..], |p| p.init.as_slice());
                let at = take(F64, 0, 0);
                for &v in init {
                    take(F64, v.to_bits() as i64, 1);
                }
                at
            })
            .collect();
        let locals: Vec<Option<Locals>> =
            plan.steps.iter().map(|s| s.program.as_ref().map(Program::locals)).collect();
        let most = |pick: fn(&Locals) -> u32| locals.iter().flatten().map(pick).max().unwrap_or(0);
        let (conditions, reals) = (most(|l| l.conditions), most(|l| l.reals));
        let imports = imports(plan);
        let scan_fn = imports.len() as u32;
        let uses_sin = plan.steps.iter().any(|s| s.program.as_ref().is_some_and(Program::uses_sin));
        Layout {
            imports,
            tick,
            channel,
            state,
            written,
            published,
            real,
            locals,
            pools: [(3, conditions), (3 + conditions, reals)],
            sin_fn: scan_fn + 3,
            round_fn: scan_fn + 3 + u32::from(uses_sin),
            globals,
            keys: Vec::new(),
        }
    }

    fn import(&self, wanted: &str) -> u32 {
        self.imports.iter().position(|(n, _)| *n == wanted).unwrap_or(0) as u32
    }
}

/// Leaves the value a source or function step computes on the stack. A
/// `mean` is not here: it writes its channels itself ([`step_code`]).
fn value_code(c: &mut Code, l: &Layout, i: usize, step: &Step) {
    let ch = |k: usize| l.channel + step.args.get(k).copied().unwrap_or(0) as u32;
    let (s0, s1) = (l.state[i], l.state[i] + 1);
    let pin = i32::from(step.config.pin.unwrap_or(0));
    match step.kind {
        Kind::Input => {
            c.constant(pin).index(CALL, l.import("gpio_read")).constant(0).op(I32_GT_S);
            if step.config.invert {
                c.op(I32_EQZ);
            }
        }
        Kind::Analog => {
            // max(reading, 0): an error code reads as 0.
            c.constant(pin)
                .index(CALL, l.import("adc_read"))
                .index(LOCAL_TEE, A)
                .constant(0)
                .local(A)
                .constant(0)
                .op(I32_GE_S)
                .op(SELECT);
        }
        Kind::Const => {
            c.constant(step.param("value"));
        }
        Kind::Not => {
            c.get(ch(0)).op(I32_EQZ);
        }
        Kind::Or | Kind::Nor => {
            // Any bit set in the union is a nonzero input.
            for k in 0..step.args.len() {
                c.get(ch(k));
                if k > 0 {
                    c.op(I32_OR);
                }
            }
            if step.kind == Kind::Or {
                c.constant(0).op(I32_NE);
            } else {
                c.op(I32_EQZ);
            }
        }
        Kind::And | Kind::Nand | Kind::Xor | Kind::Xnor => {
            let fold = if matches!(step.kind, Kind::And | Kind::Nand) { I32_AND } else { I32_XOR };
            for k in 0..step.args.len() {
                c.get(ch(k)).constant(0).op(I32_NE);
                if k > 0 {
                    c.op(fold);
                }
            }
            if matches!(step.kind, Kind::Nand | Kind::Xnor) {
                c.op(I32_EQZ);
            }
        }
        Kind::Add => {
            for k in 0..step.args.len() {
                c.get(ch(k));
                if k > 0 {
                    c.op(I32_ADD);
                }
            }
        }
        Kind::Sub | Kind::Mul | Kind::Gt | Kind::Lt | Kind::Ge | Kind::Le | Kind::Eq | Kind::Ne => {
            let op = match step.kind {
                Kind::Sub => I32_SUB,
                Kind::Mul => I32_MUL,
                Kind::Gt => I32_GT_S,
                Kind::Lt => I32_LT_S,
                Kind::Ge => I32_GE_S,
                Kind::Le => I32_LE_S,
                Kind::Eq => I32_EQ,
                _ => I32_NE,
            };
            c.get(ch(0)).get(ch(1)).op(op);
        }
        Kind::Min | Kind::Max => {
            let keep = if step.kind == Kind::Min { I32_LT_S } else { I32_GT_S };
            c.get(ch(0)).index(LOCAL_SET, A);
            for k in 1..step.args.len() {
                c.get(ch(k)).index(LOCAL_SET, B);
                c.local(A).local(B).local(A).local(B).op(keep).op(SELECT).index(LOCAL_SET, A);
            }
            c.local(A);
        }
        Kind::Abs => {
            c.constant(0).get(ch(0)).op(I32_SUB).get(ch(0)).get(ch(0)).constant(0).op(I32_LT_S).op(SELECT);
        }
        Kind::Select => {
            c.get(ch(1)).get(ch(2)).get(ch(0)).op(SELECT);
        }
        Kind::Scale => {
            c.get(ch(0))
                .op(I64_EXTEND_I32_S)
                .constant64(i64::from(step.param("mul")))
                .op(I64_MUL)
                .constant64(i64::from(step.param("div")))
                .op(I64_DIV_S)
                .op(I32_WRAP_I64);
        }
        Kind::Clamp => {
            let (lo, hi) = (step.param("lo"), step.param("hi"));
            c.get(ch(0)).constant(hi).get(ch(0)).constant(hi).op(I32_LT_S).op(SELECT).index(LOCAL_SET, A);
            c.local(A).constant(lo).local(A).constant(lo).op(I32_GT_S).op(SELECT);
        }
        Kind::Delay => {
            c.get(s0);
        }
        Kind::Lowpass => {
            let shift = step.param("shift");
            c.get(s0).get(ch(0)).get(s0).constant(shift).op(I32_SHR_S).op(I32_SUB).op(I32_ADD).set(s0);
            c.get(s0).constant(shift).op(I32_SHR_S);
        }
        Kind::Diff => {
            c.get(ch(0)).get(s0).op(I32_SUB).get(ch(0)).set(s0);
        }
        Kind::Hyst => {
            // s0 = x >= hi ? 1 : (x <= lo ? 0 : s0)
            c.constant(1).constant(0).get(s0);
            c.get(ch(0)).constant(step.param("lo")).op(I32_LE_S).op(SELECT);
            c.get(ch(0)).constant(step.param("hi")).op(I32_GE_S).op(SELECT);
            c.set(s0).get(s0);
        }
        Kind::Debounce => {
            c.get(ch(0)).constant(0).op(I32_NE).index(LOCAL_SET, A);
            c.local(A).get(s1).op(I32_EQ).when();
            c.constant(0).set(s0);
            c.op(ELSE);
            c.get(s0).constant(1).op(I32_ADD).set(s0);
            c.get(s0).constant(step.param("n")).op(I32_GE_S).when();
            c.local(A).set(s1).constant(0).set(s0);
            c.op(END).op(END);
            c.get(s1);
        }
        Kind::Rising => {
            c.get(ch(0)).constant(0).op(I32_NE).index(LOCAL_SET, A);
            c.local(A).get(s0).op(I32_EQZ).op(I32_AND).local(A).set(s0);
        }
        Kind::Falling => {
            c.get(ch(0)).constant(0).op(I32_NE).index(LOCAL_SET, A);
            c.get(s0).local(A).op(I32_EQZ).op(I32_AND).local(A).set(s0);
        }
        Kind::Count => {
            c.get(ch(0)).constant(0).op(I32_NE).index(LOCAL_SET, A);
            // The count goes up by (clock now and not before).
            c.get(s1).local(A).get(s0).op(I32_EQZ).op(I32_AND).op(I32_ADD);
            if step.args.len() > 1 {
                c.constant(0).get(ch(1)).op(I32_EQZ).op(SELECT);
            }
            c.set(s1).local(A).set(s0).get(s1);
        }
        Kind::Latch => {
            // s0 = reset ? 0 : (set ? 1 : s0)
            c.constant(0).constant(1).get(s0).get(ch(0)).op(SELECT).get(ch(1)).op(SELECT).set(s0).get(s0);
        }
        Kind::Ton => {
            let wait = step.ton_updates();
            // s0 = x ? s0 + (s0 < wait) : 0
            c.get(s0).get(s0).constant(wait).op(I32_LT_S).op(I32_ADD).constant(0).get(ch(0)).op(SELECT);
            c.set(s0).get(s0).constant(wait).op(I32_GE_S);
        }
        Kind::Mean | Kind::Channel | Kind::Output | Kind::Pwm | Kind::Ukf | Kind::Cbf => {
            c.constant(0);
        }
    }
}

/// Where a [`Program`]'s names live in the function being emitted.
struct Frame<'a> {
    program: &'a Program,
    locals: &'a Locals,
    /// The global of input channel `k`.
    channels: &'a [u32],
    /// The global of state word 0.
    state: u32,
    /// The first condition local and the first real local.
    bases: [u32; 2],
    sin_fn: u32,
}

impl Frame<'_> {
    fn local(&self, v: Val) -> u32 {
        let pool = usize::from(self.program.ops[v as usize].is_condition());
        self.bases[1 - pool] + self.locals.slot[v as usize]
    }

    /// A value where it is used: its local, or computed in place.
    fn operand(&self, c: &mut Code, v: Val) {
        if self.locals.kept[v as usize] {
            c.local(self.local(v));
        } else {
            self.tree(c, v);
        }
    }

    /// Computes value `v` onto the stack: the same operation the
    /// reference performs ([`Program::eval`]).
    fn tree(&self, c: &mut Code, v: Val) {
        let op = self.program.ops[v as usize];
        let (operands, count) = op.operands();
        match op {
            Op::Const(value) => {
                c.real(value);
            }
            Op::Chan(k) => {
                c.get(self.channels.get(k as usize).copied().unwrap_or(0)).op(F64_CONVERT_I32_S);
            }
            Op::State(k) => {
                c.get(self.state + k);
            }
            Op::Arg(k) => {
                c.local(k);
            }
            Op::Select(condition, a, b) => {
                self.operand(c, a);
                self.operand(c, b);
                self.operand(c, condition);
                c.op(SELECT);
            }
            _ => {
                for &o in &operands[..count] {
                    self.operand(c, o);
                }
                match op {
                    Op::Add(..) => c.op(F64_ADD),
                    Op::Sub(..) => c.op(F64_SUB),
                    Op::Mul(..) => c.op(F64_MUL),
                    Op::Div(..) => c.op(F64_DIV),
                    Op::Min(..) => c.op(F64_MIN),
                    Op::Max(..) => c.op(F64_MAX),
                    Op::Neg(..) => c.op(F64_NEG),
                    Op::Abs(..) => c.op(F64_ABS),
                    Op::Sqrt(..) => c.op(F64_SQRT),
                    Op::Floor(..) => c.op(F64_FLOOR),
                    Op::Sin(..) => c.index(CALL, self.sin_fn),
                    Op::Lt(..) => c.op(F64_LT),
                    Op::Le(..) => c.op(F64_LE),
                    Op::Eq(..) => c.op(F64_EQ),
                    _ => c.op(I32_AND),
                };
            }
        }
    }

    /// Every kept value into its local, in order.
    fn compute(&self, c: &mut Code) {
        for v in 0..self.program.ops.len() as Val {
            if self.locals.kept[v as usize] {
                self.tree(c, v);
                c.index(LOCAL_SET, self.local(v));
            }
        }
    }
}

/// A function body's locals: `(count, type)` groups, empty ones left out.
fn locals_header(out: &mut Vec<u8>, groups: &[(u32, u8)]) {
    let groups: Vec<&(u32, u8)> = groups.iter().filter(|(count, _)| *count > 0).collect();
    uleb(out, groups.len() as u32);
    for (count, ty) in groups {
        uleb(out, *count);
        out.push(*ty);
    }
}

/// The `sin` helper: [`sin_program`] with its two arguments as locals 0 and 1.
fn sin_body() -> Vec<u8> {
    let program = sin_program();
    let locals = program.locals();
    let mut c = Code(Vec::new());
    locals_header(&mut c.0, &[(locals.conditions, I32), (locals.reals, F64)]);
    let frame = Frame {
        program: &program,
        locals: &locals,
        channels: &[],
        state: 0,
        bases: [2, 2 + locals.conditions],
        sin_fn: 0,
    };
    frame.compute(&mut c);
    c.local(frame.local(program.outputs[0])).op(END);
    c.0
}

/// The `round` helper: [`crate::real::round_sat`], operation for operation.
fn round_body() -> Vec<u8> {
    let mut c = Code(Vec::new());
    locals_header(&mut c.0, &[(1, F64)]);
    // NaN is 0.
    c.local(0).local(0).op(F64_NE).op(IF).op(I32).constant(0).op(ELSE);
    // t = trunc(v); r = |v - t| >= 0.5 ? t + copysign(1, v) : t
    c.local(0).op(F64_TRUNC).index(LOCAL_SET, 1);
    c.local(1).real(1.0).local(0).op(F64_COPYSIGN).op(F64_ADD);
    c.local(1);
    c.local(0).local(1).op(F64_SUB).op(F64_ABS).real(0.5).op(F64_GE).op(SELECT);
    c.real(2_147_483_647.0).op(F64_MIN).real(-2_147_483_648.0).op(F64_MAX).op(I32_TRUNC_F64_S);
    c.op(END).op(END);
    c.0
}

/// One update of step `i`: compute and write its channels and publish the
/// traced ones, or for a sink write the pin when the level or duty changed.
fn step_code(c: &mut Code, l: &Layout, plan: &Plan, i: usize, step: &Step) {
    let from = l.channel + step.args.first().copied().unwrap_or(0) as u32;
    let write_outs = |c: &mut Code| {
        c.index(LOCAL_SET, V);
        for &out in &step.outs {
            c.local(V).set(l.channel + out as u32);
        }
    };
    if let (Some(program), Some(locals)) = (&step.program, &l.locals[i]) {
        // A model: every value from the old state and the channels as
        // they are, then the state and the outputs written.
        let channels: Vec<u32> = step.args.iter().map(|&a| l.channel + a as u32).collect();
        let frame = Frame {
            program,
            locals,
            channels: &channels,
            state: l.real[i],
            bases: [l.pools[0].0, l.pools[1].0],
            sin_fn: l.sin_fn,
        };
        frame.compute(c);
        for &(word, v) in &program.stores {
            c.local(frame.local(v)).set(l.real[i] + word);
        }
        for (&out, &v) in step.outs.iter().zip(&program.outputs) {
            c.local(frame.local(v)).index(CALL, l.round_fn).set(l.channel + out as u32);
        }
    }
    match step.kind {
        Kind::Ukf | Kind::Cbf => {}
        Kind::Output | Kind::Pwm => {
            if step.kind == Kind::Pwm {
                // The duty: the channel limited to 0..=65535.
                c.get(from).constant(PWM_FULL).get(from).constant(PWM_FULL).op(I32_LT_S).op(SELECT);
                c.index(LOCAL_SET, A);
                c.local(A).constant(0).local(A).constant(0).op(I32_GT_S).op(SELECT);
            } else if step.config.invert {
                c.get(from).op(I32_EQZ);
            } else {
                c.get(from).constant(0).op(I32_NE);
            }
            let written = l.written[i];
            c.index(LOCAL_SET, V).local(V).get(written).op(I32_NE).when().local(V).set(written);
            if step.kind == Kind::Pwm {
                c.constant(i32::from(step.config.timer.unwrap_or(0)))
                    .constant(i32::from(step.config.channel.unwrap_or(0)))
                    .local(V)
                    .index(CALL, l.import("pwm_set"));
            } else {
                c.constant(i32::from(step.config.pin.unwrap_or(0)))
                    .local(V)
                    .index(CALL, l.import("gpio_write"));
            }
            c.op(DROP).op(END);
            return;
        }
        Kind::Mean => {
            // Integrate; on the n-th input write the mean and start over.
            let (sum, count) = (l.state[i], l.state[i] + 1);
            let n = step.param("n");
            c.get(sum).get(from).op(I32_ADD).set(sum);
            c.get(count).constant(1).op(I32_ADD).set(count);
            c.get(count).constant(n).op(I32_GE_S).when();
            c.get(sum).constant(n).op(I32_DIV_S);
            write_outs(c);
            c.constant(0).set(sum).constant(0).set(count).op(END);
        }
        _ => {
            value_code(c, l, i, step);
            write_outs(c);
        }
    }
    for &out in &step.outs {
        if !plan.channels[out].trace {
            continue;
        }
        let (value, published) = (l.channel + out as u32, l.published[out]);
        let (key_at, key_len) = l.keys[out];
        c.get(value).op(I64_EXTEND_I32_S).get(published).op(I64_NE).when();
        c.get(value).op(I64_EXTEND_I32_S).set(published);
        c.constant(SCRATCH).get(value).op(I32_STORE).op(0x02).op(0x00);
        c.constant(key_at).constant(key_len).constant(SCRATCH).constant(4);
        c.index(CALL, l.import("publish")).op(DROP).op(END);
    }
}

/// Emits `body` for each run of consecutive steps with one rate, inside
/// `if tick % every == 0` when the run is slower than the base tick.
fn at_rate(c: &mut Code, l: &Layout, steps: &[(usize, &Step)], mut body: impl FnMut(&mut Code, usize, &Step)) {
    let mut at = 0;
    while at < steps.len() {
        let every = steps[at].1.every;
        let run = steps[at..].iter().take_while(|(_, s)| s.every == every).count();
        if every > 1 {
            c.get(l.tick).constant(every as i32).op(I32_REM_U).op(I32_EQZ).when();
        }
        for (i, step) in &steps[at..at + run] {
            body(c, *i, step);
        }
        if every > 1 {
            c.op(END);
        }
        at += run;
    }
}

/// One call `init` makes to set a pin or timer up.
struct Setup {
    import: &'static str,
    /// The arguments; the first `count` are passed.
    args: [i32; 2],
    count: usize,
    /// The log line if the board refuses: `(address, length)`.
    line: (i32, i32),
}

/// Compiles `plan` to a payload module.
pub fn compile(plan: &Plan) -> Vec<u8> {
    let mut l = Layout::new(plan);
    let scan_fn = l.imports.len() as u32;
    let init_fn = scan_fn + 1;
    let tick_fn = scan_fn + 2;

    // The log lines and trace keys, laid out from DATA_BASE.
    let mut data: Vec<u8> = Vec::new();
    let mut text = |text: String| {
        let at = DATA_BASE + data.len() as u32;
        data.extend_from_slice(text.as_bytes());
        (at as i32, text.len() as i32)
    };
    let (pins, sinks) = pin_counts(plan);
    let summary =
        text(format!("wire {}: {pins} in, {} ch, {sinks} out", plan.name, plan.channels.len()));
    let by_node = plan.steps_by_node();
    // What init sets up, in node order: inputs, then outputs, then each
    // PWM timer once.
    let mut setup: Vec<Setup> = Vec::new();
    for step in by_node.iter().filter(|s| s.kind == Kind::Input) {
        let pin = i32::from(step.config.pin.unwrap_or(0));
        let line = text(format!("wire: input gpio {pin} refused"));
        setup.push(Setup { import: "gpio_input", args: [pin, step.config.pull.code()], count: 2, line });
    }
    for step in by_node.iter().filter(|s| s.kind == Kind::Output) {
        let pin = i32::from(step.config.pin.unwrap_or(0));
        let line = text(format!("wire: output gpio {pin} refused"));
        setup.push(Setup { import: "gpio_output", args: [pin, 0], count: 1, line });
    }
    let mut timers: Vec<i32> = Vec::new();
    for step in by_node.iter().filter(|s| s.kind == Kind::Pwm) {
        let timer = i32::from(step.config.timer.unwrap_or(0));
        if !timers.contains(&timer) {
            timers.push(timer);
            let line = text(format!("wire: pwm timer {timer} refused"));
            setup.push(Setup { import: "pwm_open", args: [timer, pwm_freq(step) as i32], count: 2, line });
        }
    }
    l.keys = plan
        .channels
        .iter()
        .map(|c| if c.trace { text(format!("wire/{}", c.name)) } else { (0, 0) })
        .collect();

    // scan: one base tick.
    let steps: Vec<(usize, &Step)> = plan.steps.iter().enumerate().collect();
    let mut scan = Code(Vec::new());
    locals_header(&mut scan.0, &[(3 + l.pools[0].1, I32), (l.pools[1].1, F64)]);
    at_rate(&mut scan, &l, &steps, |c, i, step| step_code(c, &l, plan, i, step));
    let delays: Vec<(usize, &Step)> =
        steps.iter().copied().filter(|(_, s)| s.kind == Kind::Delay).collect();
    at_rate(&mut scan, &l, &delays, |c, i, step| {
        c.get(l.channel + step.args.first().copied().unwrap_or(0) as u32).set(l.state[i]);
    });
    scan.get(l.tick).constant(1).op(I32_ADD).set(l.tick).op(END);

    // init.
    let mut init = Code(Vec::new());
    uleb(&mut init.0, 0);
    for call in &setup {
        for arg in &call.args[..call.count] {
            init.constant(*arg);
        }
        init.index(CALL, l.import(call.import))
            .constant(0)
            .op(I32_LT_S)
            .when()
            .constant(call.line.0)
            .constant(call.line.1)
            .index(CALL, l.import("log"))
            .op(END);
    }
    init.constant(plan.period_ms as i32)
        .index(CALL, l.import("set_tick"))
        .op(DROP)
        .constant(summary.0)
        .constant(summary.1)
        .index(CALL, l.import("log"))
        .index(CALL, scan_fn)
        .op(END);

    // tick(now_ms).
    let mut tick = Code(Vec::new());
    uleb(&mut tick.0, 0);
    tick.index(CALL, scan_fn).op(END);

    let mut module: Vec<u8> = alloc::vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];

    // Types.
    let mut s = Vec::new();
    uleb(&mut s, TYPES.len() as u32);
    for (params, results) in TYPES {
        s.push(0x60);
        uleb(&mut s, params.len() as u32);
        s.extend_from_slice(params);
        uleb(&mut s, results.len() as u32);
        s.extend_from_slice(results);
    }
    section(&mut module, 1, &s);

    // Imports.
    let mut s = Vec::new();
    uleb(&mut s, l.imports.len() as u32);
    for (import_name, ty) in &l.imports {
        name(&mut s, "mw");
        name(&mut s, import_name);
        s.push(0x00);
        uleb(&mut s, *ty);
    }
    section(&mut module, 2, &s);

    // Functions: scan, init, tick, and the helpers a model needs.
    let models = plan.steps.iter().any(|s| s.program.is_some());
    let mut bodies: Vec<(u32, Vec<u8>)> =
        alloc::vec![(T_VOID, scan.0), (T_VOID, init.0), (T_TICK, tick.0)];
    if l.round_fn != l.sin_fn {
        bodies.push((T_SIN, sin_body()));
    }
    if models {
        bodies.push((T_ROUND, round_body()));
    }
    let mut s = Vec::new();
    uleb(&mut s, bodies.len() as u32);
    for (ty, _) in &bodies {
        uleb(&mut s, *ty);
    }
    section(&mut module, 3, &s);

    // Memory: one page, fixed.
    section(&mut module, 5, &[0x01, 0x01, 0x01, 0x01]);

    // Globals, all mutable.
    let mut s = Vec::new();
    uleb(&mut s, l.globals.len() as u32);
    for (ty, value) in &l.globals {
        s.extend_from_slice(&[*ty, 0x01]);
        match *ty {
            F64 => {
                s.push(F64_CONST);
                s.extend_from_slice(&(*value as u64).to_le_bytes());
            }
            I64 => {
                s.push(I64_CONST);
                sleb(&mut s, *value);
            }
            _ => {
                s.push(I32_CONST);
                sleb(&mut s, *value);
            }
        }
        s.push(END);
    }
    section(&mut module, 6, &s);

    // Exports.
    let mut s = Vec::new();
    uleb(&mut s, 3);
    name(&mut s, "memory");
    s.extend_from_slice(&[0x02, 0x00]);
    name(&mut s, "init");
    s.push(0x00);
    uleb(&mut s, init_fn);
    name(&mut s, "tick");
    s.push(0x00);
    uleb(&mut s, tick_fn);
    section(&mut module, 7, &s);

    // Code.
    let mut s = Vec::new();
    uleb(&mut s, bodies.len() as u32);
    for (_, body) in &bodies {
        uleb(&mut s, body.len() as u32);
        s.extend_from_slice(body);
    }
    section(&mut module, 10, &s);

    // Data: the log lines and trace keys.
    let mut s = Vec::new();
    uleb(&mut s, 1);
    s.push(0x00);
    s.push(I32_CONST);
    sleb(&mut s, i64::from(DATA_BASE));
    s.push(END);
    uleb(&mut s, data.len() as u32);
    s.extend_from_slice(&data);
    section(&mut module, 11, &s);

    module
}
