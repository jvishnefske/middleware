//! Real arithmetic for model-based blocks: a straight-line program of
//! `f64` operations that the reference interprets and the compiler emits
//! op for op.
//!
//! A [`Program`] is what one update of a `ukf` or `cbf` node computes. It
//! is built once ([`Builder`]) and then used twice: [`Program::eval`] is
//! the reference, and [`crate::wasm`] translates the same operations, in
//! the same order, to WebAssembly. Every operation is one IEEE 754
//! double-precision operation that Rust and WebAssembly define alike
//! (add, sub, mul, div, sqrt, floor, abs, neg, min, max, comparisons), so
//! the payload and the reference agree bit for bit, on every target.
//!
//! * `min` and `max` are WebAssembly's: NaN if either operand is NaN, and
//!   `min(-0, +0) = -0`, `max(-0, +0) = +0` ([`fmin`], [`fmax`]).
//! * `sqrt` is `libm`'s, correctly rounded like `f64.sqrt`.
//! * `sin` and `cos` are not library calls: they are one fixed program of
//!   the operations above ([`sin_program`]), interpreted by the reference
//!   and emitted as a helper function in the payload.
//!
//! Integers at the edges: a channel `c` enters as `c as f64` (the block
//! divides by its scale), and a result leaves through [`round_sat`]:
//! rounded half away from zero and saturated to `i32`, NaN as 0.

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

/// A value of a [`Program`]: the index of the operation that computes it.
pub type Val = u32;

/// One operation. Operands are earlier values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Op {
    /// A constant.
    Const(f64),
    /// The node's k-th input channel, as a real (not yet divided by the scale).
    Chan(u32),
    /// The node's k-th word of state, as it was before this update.
    State(u32),
    /// The k-th argument of a helper ([`sin_program`] only).
    Arg(u32),
    /// `a + b`.
    Add(Val, Val),
    /// `a - b`.
    Sub(Val, Val),
    /// `a * b`.
    Mul(Val, Val),
    /// `a / b`.
    Div(Val, Val),
    /// The smaller ([`fmin`]).
    Min(Val, Val),
    /// The larger ([`fmax`]).
    Max(Val, Val),
    /// `-a`.
    Neg(Val),
    /// `|a|`.
    Abs(Val),
    /// The square root.
    Sqrt(Val),
    /// The largest integer not above `a`.
    Floor(Val),
    /// `sin(a + b * pi/2)` for `b` 0 or 1: the helper ([`sin_program`]).
    Sin(Val, Val),
    /// A condition: `a < b`.
    Lt(Val, Val),
    /// A condition: `a <= b`.
    Le(Val, Val),
    /// A condition: `a == b`.
    Eq(Val, Val),
    /// A condition: both conditions hold.
    And(Val, Val),
    /// `a` if the condition holds, else `b`: `Select(condition, a, b)`.
    Select(Val, Val, Val),
}

impl Op {
    /// Whether the value is a condition (an `i32` in the payload).
    pub const fn is_condition(self) -> bool {
        matches!(self, Op::Lt(..) | Op::Le(..) | Op::Eq(..) | Op::And(..))
    }

    /// The operands.
    pub fn operands(self) -> ([Val; 3], usize) {
        match self {
            Op::Const(_) | Op::Chan(_) | Op::State(_) | Op::Arg(_) => ([0; 3], 0),
            Op::Neg(a) | Op::Abs(a) | Op::Sqrt(a) | Op::Floor(a) => ([a, 0, 0], 1),
            Op::Add(a, b)
            | Op::Sub(a, b)
            | Op::Mul(a, b)
            | Op::Div(a, b)
            | Op::Min(a, b)
            | Op::Max(a, b)
            | Op::Sin(a, b)
            | Op::Lt(a, b)
            | Op::Le(a, b)
            | Op::Eq(a, b)
            | Op::And(a, b) => ([a, b, 0], 2),
            Op::Select(c, a, b) => ([c, a, b], 3),
        }
    }

    fn key(self) -> (u8, u64, u64, u64) {
        let (o, _) = self.operands();
        let (a, b, c) = (u64::from(o[0]), u64::from(o[1]), u64::from(o[2]));
        match self {
            Op::Const(v) => (0, v.to_bits(), 0, 0),
            Op::Chan(k) => (1, u64::from(k), 0, 0),
            Op::State(k) => (2, u64::from(k), 0, 0),
            Op::Arg(k) => (3, u64::from(k), 0, 0),
            Op::Add(..) => (4, a, b, c),
            Op::Sub(..) => (5, a, b, c),
            Op::Mul(..) => (6, a, b, c),
            Op::Div(..) => (7, a, b, c),
            Op::Min(..) => (8, a, b, c),
            Op::Max(..) => (9, a, b, c),
            Op::Neg(..) => (10, a, b, c),
            Op::Abs(..) => (11, a, b, c),
            Op::Sqrt(..) => (12, a, b, c),
            Op::Floor(..) => (13, a, b, c),
            Op::Sin(..) => (14, a, b, c),
            Op::Lt(..) => (15, a, b, c),
            Op::Le(..) => (16, a, b, c),
            Op::Eq(..) => (17, a, b, c),
            Op::And(..) => (18, a, b, c),
            Op::Select(..) => (19, a, b, c),
        }
    }
}

/// WebAssembly's `f64.min`: NaN if either is NaN; `-0` is below `+0`.
pub fn fmin(a: f64, b: f64) -> f64 {
    if a < b {
        a
    } else if b < a {
        b
    } else if a == b {
        if a.is_sign_negative() { a } else { b }
    } else {
        f64::NAN
    }
}

/// WebAssembly's `f64.max`: NaN if either is NaN; `+0` is above `-0`.
pub fn fmax(a: f64, b: f64) -> f64 {
    if a > b {
        a
    } else if b > a {
        b
    } else if a == b {
        if a.is_sign_negative() { b } else { a }
    } else {
        f64::NAN
    }
}

/// A real result as a channel value: rounded half away from zero and
/// saturated to `i32`; NaN is 0. The payload's `round` helper does the
/// same operations.
pub fn round_sat(v: f64) -> i32 {
    if v.is_nan() {
        return 0;
    }
    let t = libm::trunc(v);
    // v - t is exact: below 2^52 it has no more bits than v, above v is whole.
    let r = if libm::fabs(v - t) >= 0.5 { t + libm::copysign(1.0, v) } else { t };
    fmax(fmin(r, 2_147_483_647.0), -2_147_483_648.0) as i32
}

/// What one update of a node computes.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Program {
    /// The operations, each after its operands.
    pub ops: Vec<Op>,
    /// The state before the first update.
    pub init: Vec<f64>,
    /// The state after the update: `(word, value)`.
    pub stores: Vec<(u32, Val)>,
    /// The results, one per output channel in order, each passed through
    /// [`round_sat`].
    pub outputs: Vec<Val>,
    /// The state word that counts faults, if the node has one.
    pub faults: Option<u32>,
}

/// Which values the payload keeps in locals, and in which.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Locals {
    /// Whether value `v` gets a local; any other value has one use and is
    /// computed on the stack where it is used.
    pub kept: Vec<bool>,
    /// The local of a kept value, within its pool (conditions and reals
    /// have a pool each).
    pub slot: Vec<u32>,
    /// How many condition locals (`i32`) the program needs.
    pub conditions: u32,
    /// How many real locals (`f64`) the program needs.
    pub reals: u32,
}

impl Program {
    /// Whether any operation is [`Op::Sin`].
    pub fn uses_sin(&self) -> bool {
        self.ops.iter().any(|op| matches!(op, Op::Sin(..)))
    }

    /// Every value of one update, in operation order (a condition is 1 or
    /// 0). `chan(k)` is input channel `k`, `state` the node's state before
    /// the update and `sin` the helper program ([`sin_program`]).
    pub fn eval(
        &self,
        chan: &dyn Fn(u32) -> i32,
        args: &[f64],
        state: &[f64],
        sin: &Program,
    ) -> Vec<f64> {
        let mut v: Vec<f64> = Vec::with_capacity(self.ops.len());
        let truth = |c: bool| if c { 1.0 } else { 0.0 };
        for op in &self.ops {
            let at = |i: Val| v[i as usize];
            let value = match *op {
                Op::Const(c) => c,
                Op::Chan(k) => f64::from(chan(k)),
                Op::State(k) => state.get(k as usize).copied().unwrap_or(0.0),
                Op::Arg(k) => args.get(k as usize).copied().unwrap_or(0.0),
                Op::Add(a, b) => at(a) + at(b),
                Op::Sub(a, b) => at(a) - at(b),
                Op::Mul(a, b) => at(a) * at(b),
                Op::Div(a, b) => at(a) / at(b),
                Op::Min(a, b) => fmin(at(a), at(b)),
                Op::Max(a, b) => fmax(at(a), at(b)),
                Op::Neg(a) => -at(a),
                Op::Abs(a) => libm::fabs(at(a)),
                Op::Sqrt(a) => libm::sqrt(at(a)),
                Op::Floor(a) => libm::floor(at(a)),
                Op::Sin(a, b) => {
                    let inner = sin.eval(&|_| 0, &[at(a), at(b)], &[], sin);
                    sin.outputs.first().map_or(0.0, |&o| inner[o as usize])
                }
                Op::Lt(a, b) => truth(at(a) < at(b)),
                Op::Le(a, b) => truth(at(a) <= at(b)),
                Op::Eq(a, b) => truth(at(a) == at(b)),
                Op::And(a, b) => truth(at(a) != 0.0 && at(b) != 0.0),
                Op::Select(c, a, b) => {
                    if at(c) != 0.0 {
                        at(a)
                    } else {
                        at(b)
                    }
                }
            };
            v.push(value);
        }
        v
    }

    /// Chooses the payload's locals. A value used once is computed on the
    /// stack at its use; a value used more often, stored or output is
    /// computed where it is defined and kept in a local, which is free
    /// again after the last kept value that reads it. All state and
    /// outputs are therefore computed before any is written.
    pub fn locals(&self) -> Locals {
        let n = self.ops.len();
        let mut uses = vec![0u32; n];
        for op in &self.ops {
            let (operands, count) = op.operands();
            for &o in &operands[..count] {
                uses[o as usize] += 1;
            }
        }
        let mut kept: Vec<bool> = uses.iter().map(|&u| u >= 2).collect();
        const END: usize = usize::MAX;
        // The last kept value whose tree reads each kept value; END for a
        // value that is stored or output.
        let mut last: Vec<Option<usize>> = vec![None; n];
        for &v in self.stores.iter().map(|(_, v)| v).chain(&self.outputs) {
            kept[v as usize] = true;
            last[v as usize] = Some(END);
        }
        let mut stack: Vec<Val> = Vec::new();
        for root in 0..n {
            if !kept[root] {
                continue;
            }
            stack.push(root as Val);
            while let Some(v) = stack.pop() {
                let (operands, count) = self.ops[v as usize].operands();
                for &o in &operands[..count] {
                    if !kept[o as usize] {
                        stack.push(o);
                    } else if last[o as usize] != Some(END) {
                        last[o as usize] = Some(root);
                    }
                }
            }
        }
        let mut ending: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (v, l) in last.iter().enumerate() {
            if let Some(l) = l.filter(|&l| kept[v] && l != END) {
                ending.entry(l).or_default().push(v);
            }
        }
        // Lowest free local first; a local read for the last time in a
        // tree is free for that tree's result.
        let mut slot = vec![0u32; n];
        let mut free: [Vec<u32>; 2] = [Vec::new(), Vec::new()];
        let mut count = [0u32; 2];
        for root in 0..n {
            if !kept[root] {
                continue;
            }
            for &v in ending.get(&root).map_or(&[][..], Vec::as_slice) {
                free[usize::from(self.ops[v].is_condition())].push(slot[v]);
            }
            let pool = usize::from(self.ops[root].is_condition());
            free[pool].sort_unstable_by(|a, b| b.cmp(a));
            slot[root] = free[pool].pop().unwrap_or_else(|| {
                count[pool] += 1;
                count[pool] - 1
            });
        }
        Locals { kept, slot, conditions: count[1], reals: count[0] }
    }
}

/// Builds a [`Program`], sharing identical operations.
#[derive(Debug, Default)]
pub struct Builder {
    ops: Vec<Op>,
    seen: BTreeMap<(u8, u64, u64, u64), Val>,
}

impl Builder {
    /// An empty program.
    pub fn new() -> Self {
        Builder::default()
    }

    /// How many operations there are so far.
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    /// Whether there is no operation yet.
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// The value of `op`, reusing an identical earlier operation.
    pub fn op(&mut self, op: Op) -> Val {
        let key = op.key();
        if let Some(&v) = self.seen.get(&key) {
            return v;
        }
        let v = self.ops.len() as Val;
        self.ops.push(op);
        self.seen.insert(key, v);
        v
    }

    /// A constant.
    pub fn num(&mut self, v: f64) -> Val {
        self.op(Op::Const(v))
    }
    /// `a + b`.
    pub fn add(&mut self, a: Val, b: Val) -> Val {
        self.op(Op::Add(a, b))
    }
    /// `a - b`.
    pub fn sub(&mut self, a: Val, b: Val) -> Val {
        self.op(Op::Sub(a, b))
    }
    /// `a * b`.
    pub fn mul(&mut self, a: Val, b: Val) -> Val {
        self.op(Op::Mul(a, b))
    }
    /// `a / b`.
    pub fn div(&mut self, a: Val, b: Val) -> Val {
        self.op(Op::Div(a, b))
    }
    /// The terms added left to right; 0 for none.
    pub fn sum(&mut self, terms: &[Val]) -> Val {
        match terms.split_first() {
            None => self.num(0.0),
            Some((&first, rest)) => rest.iter().fold(first, |acc, &t| self.add(acc, t)),
        }
    }
    /// Whether `v` is neither infinite nor NaN: `v - v == 0`.
    pub fn finite(&mut self, v: Val) -> Val {
        let difference = self.sub(v, v);
        let zero = self.num(0.0);
        self.op(Op::Eq(difference, zero))
    }
    /// Whether every condition holds; `None` for no condition.
    pub fn all(&mut self, conditions: &[Val]) -> Option<Val> {
        let (&first, rest) = conditions.split_first()?;
        Some(rest.iter().fold(first, |acc, &c| self.op(Op::And(acc, c))))
    }

    /// The finished program: the operations the state and outputs need,
    /// in the order they were built.
    pub fn finish(
        self,
        init: Vec<f64>,
        stores: Vec<(u32, Val)>,
        outputs: Vec<Val>,
        faults: Option<u32>,
    ) -> Program {
        let mut needed = vec![false; self.ops.len()];
        for &v in stores.iter().map(|(_, v)| v).chain(&outputs) {
            needed[v as usize] = true;
        }
        for v in (0..self.ops.len()).rev() {
            if needed[v] {
                let (operands, count) = self.ops[v].operands();
                for &o in &operands[..count] {
                    needed[o as usize] = true;
                }
            }
        }
        let mut renamed = vec![0 as Val; self.ops.len()];
        let mut ops = Vec::new();
        for (v, op) in self.ops.iter().enumerate() {
            if !needed[v] {
                continue;
            }
            let at = ops.len() as Val;
            let r = |o: Val| renamed[o as usize];
            let op = match *op {
                Op::Add(a, b) => Op::Add(r(a), r(b)),
                Op::Sub(a, b) => Op::Sub(r(a), r(b)),
                Op::Mul(a, b) => Op::Mul(r(a), r(b)),
                Op::Div(a, b) => Op::Div(r(a), r(b)),
                Op::Min(a, b) => Op::Min(r(a), r(b)),
                Op::Max(a, b) => Op::Max(r(a), r(b)),
                Op::Neg(a) => Op::Neg(r(a)),
                Op::Abs(a) => Op::Abs(r(a)),
                Op::Sqrt(a) => Op::Sqrt(r(a)),
                Op::Floor(a) => Op::Floor(r(a)),
                Op::Sin(a, b) => Op::Sin(r(a), r(b)),
                Op::Lt(a, b) => Op::Lt(r(a), r(b)),
                Op::Le(a, b) => Op::Le(r(a), r(b)),
                Op::Eq(a, b) => Op::Eq(r(a), r(b)),
                Op::And(a, b) => Op::And(r(a), r(b)),
                Op::Select(c, a, b) => Op::Select(r(c), r(a), r(b)),
                leaf => leaf,
            };
            renamed[v] = at;
            ops.push(op);
        }
        Program {
            ops,
            init,
            stores: stores.into_iter().map(|(k, v)| (k, renamed[v as usize])).collect(),
            outputs: outputs.into_iter().map(|v| renamed[v as usize]).collect(),
            faults,
        }
    }
}

// The fdlibm kernels' coefficients (sin and cos on [-pi/4, pi/4]) and its
// split of pi/2: the first 33 bits and the rest. The digits are fdlibm's,
// more than an f64 holds.
const TWO_OVER_PI: f64 = core::f64::consts::FRAC_2_PI;
#[allow(clippy::excessive_precision)]
const PIO2_HIGH: f64 = 1.570_796_326_734_125_614_17;
#[allow(clippy::excessive_precision)]
const PIO2_LOW: f64 = 6.077_100_506_506_192_249_32e-11;
#[allow(clippy::excessive_precision)]
const S: [f64; 6] = [
    -1.666_666_666_666_663_243_48e-1,
    8.333_333_333_322_489_461_24e-3,
    -1.984_126_982_985_794_931_34e-4,
    2.755_731_370_707_006_767_89e-6,
    -2.505_076_025_340_686_341_95e-8,
    1.589_690_995_211_550_102_21e-10,
];
#[allow(clippy::excessive_precision)]
const C: [f64; 6] = [
    4.166_666_666_666_660_190_37e-2,
    -1.388_888_888_887_410_957_49e-3,
    2.480_158_728_947_672_941_78e-5,
    -2.755_731_435_139_066_330_35e-7,
    2.087_572_321_298_174_827_90e-9,
    -1.135_964_755_778_819_482_65e-11,
];

/// The one sine: `sin(x + s * pi/2)` for arguments `x` and `s` (0 for
/// sine, 1 for cosine).
///
/// 1. `k = floor(x * 2/pi + 1/2)`, the nearest multiple of pi/2;
/// 2. `r = (x - k * P1) - k * P2`, with pi/2 split in a 33-bit head `P1`
///    and a tail `P2` (exact head product for `|k| < 2^20`), so `|r| <=
///    pi/4` up to rounding;
/// 3. the quadrant `q = (k + s) mod 4`, computed in reals;
/// 4. fdlibm's degree-13 sine and degree-14 cosine polynomials in `r`,
///    and by quadrant `sin r`, `cos r`, `-sin r` or `-cos r`.
///
/// Accuracy: within 1e-15 of the true value for `|x| <= 1e5` (measured
/// at 1.2e-16 from `libm`); beyond about `1.6e6` the reduction
/// loses precision gradually. Infinite or NaN `x` gives NaN. The result
/// is exactly reproducible, which is the point.
pub fn sin_program() -> Program {
    let mut b = Builder::new();
    let x = b.op(Op::Arg(0));
    let s = b.op(Op::Arg(1));
    let half = b.num(0.5);
    let scaled = b.num(TWO_OVER_PI);
    let scaled = b.mul(x, scaled);
    let k = b.add(scaled, half);
    let k = b.op(Op::Floor(k));
    let high = b.num(PIO2_HIGH);
    let high = b.mul(k, high);
    let low = b.num(PIO2_LOW);
    let low = b.mul(k, low);
    let r = b.sub(x, high);
    let r = b.sub(r, low);
    // q = (k + s) - 4 * floor((k + s) / 4)
    let ks = b.add(k, s);
    let quarter = b.num(0.25);
    let turns = b.mul(ks, quarter);
    let turns = b.op(Op::Floor(turns));
    let four = b.num(4.0);
    let turns = b.mul(four, turns);
    let q = b.sub(ks, turns);
    let z = b.mul(r, r);
    let horner = |b: &mut Builder, c: &[f64; 6]| {
        let mut acc = b.num(c[5]);
        for &coefficient in c[..5].iter().rev() {
            let zc = b.mul(z, acc);
            let k = b.num(coefficient);
            acc = b.add(k, zc);
        }
        acc
    };
    // sin r = r + r z S(z)
    let sp = horner(&mut b, &S);
    let rz = b.mul(r, z);
    let tail = b.mul(rz, sp);
    let sin_r = b.add(r, tail);
    // cos r = 1 - z/2 + z z C(z)
    let cp = horner(&mut b, &C);
    let zz = b.mul(z, z);
    let tail = b.mul(zz, cp);
    let one = b.num(1.0);
    let hz = b.mul(half, z);
    let head = b.sub(one, hz);
    let cos_r = b.add(head, tail);
    let minus_sin = b.op(Op::Neg(sin_r));
    let minus_cos = b.op(Op::Neg(cos_r));
    let first = b.op(Op::Lt(q, half));
    let limit = b.num(1.5);
    let second = b.op(Op::Lt(q, limit));
    let limit = b.num(2.5);
    let third = b.op(Op::Lt(q, limit));
    let tail = b.op(Op::Select(third, minus_sin, minus_cos));
    let tail = b.op(Op::Select(second, cos_r, tail));
    let result = b.op(Op::Select(first, sin_r, tail));
    b.finish(Vec::new(), Vec::new(), vec![result], None)
}

fn sin_shifted(x: f64, shift: f64) -> f64 {
    let program = sin_program();
    let values = program.eval(&|_| 0, &[x, shift], &[], &program);
    values[program.outputs[0] as usize]
}

/// The sine every block computes ([`sin_program`]).
pub fn sin(x: f64) -> f64 {
    sin_shifted(x, 0.0)
}

/// The cosine every block computes ([`sin_program`]).
pub fn cos(x: f64) -> f64 {
    sin_shifted(x, 1.0)
}
