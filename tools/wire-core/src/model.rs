//! The model-based blocks: what one update of a `ukf` or a `cbf` node
//! computes, as a [`Program`] of real operations.
//!
//! Both read their inputs as `channel / scale`, compute in `f64`, and
//! write each output as `round(real * scale)` ([`crate::real::round_sat`]).
//! [`program`] builds the program for a node, or says what is wrong with
//! its settings; the check, the reference and the compiler all call it.
//!
//! # `ukf`: unscented Kalman filter
//!
//! Model: `x[k+1] = f(x, u, dt)` with process noise variances `q`, and
//! measurement `z = h(x)` with noise variances `r` (both diagonal).
//! Inputs: the `m` measurements (one per expression of `h`), then the
//! controls `u0..`. Outputs: the `n` state estimates, or `2n`: then the
//! `n` standard deviations `sqrt(P_ii)`.
//!
//! The state is `x` (from `x0`), the lower triangle of `P` (from
//! `diag(p0)`) and a fault count. One update, with `c = alpha^2 (n +
//! kappa)` and `lambda = c - n`:
//!
//! 1. `L` = lower Cholesky factor of `c P`; sigma points `x`, `x + L_i`,
//!    `x - L_i` (the columns of `L`), weights `Wm0 = lambda / c`, `Wc0 =
//!    Wm0 + 1 - alpha^2 + beta`, the others `1 / (2c)`;
//! 2. each sigma point through `f`; predicted mean `xp` and covariance
//!    `Pp` (plus `q`);
//! 3. new sigma points of `xp` and `Pp` (a second Cholesky factor, of
//!    `c Pp`), each through `h`; predicted measurement `zp`, innovation
//!    covariance `S` (plus `r`) and cross covariance `C`. Drawing them
//!    again is what lets the process noise `q` reach `S` and `C`: on a
//!    linear model the filter is then exactly the Kalman filter;
//! 4. `Ls` = Cholesky factor of `S`; `Y = C Ls^-T` by forward
//!    substitution and the gain `K = Y Ls^-1` by back substitution;
//! 5. `x = xp + K (z - zp)` and `P = Pp - Y Y^T`, which is `Pp - K S K^T`
//!    and symmetric by construction (only the lower triangle is kept).
//!
//! **Faults.** The update is *sound* when every pivot of the three
//! Cholesky factorisations (of `c P`, `c Pp` and `S`) is greater than 0
//! and every new `x` and `P` entry is finite. An unsound update does not take effect: the fault count goes
//! up by one, `P` becomes `diag(p0)`, and `x` becomes the predicted mean
//! `xp` if all of it is finite, else `x0`. The outputs of that tick are
//! the reset values.
//!
//! # `cbf`: control barrier function safety filter
//!
//! For `xdot = f(x) + g(x) u` and the safe set `h(x) >= 0`, it changes the
//! nominal control as little as possible so that `hdot + alpha h >= 0`.
//! Inputs: the `m` nominal controls (1 or 2), then the `n` states.
//! Outputs: the `m` safe controls, or `m + 1`: then a flag, not scaled.
//!
//! With the gradient of `h` taken symbolically ([`crate::expr::Expr::diff`]):
//! `a = grad h . f + alpha h`, `b_j = grad h . g_j` (column `j` of `g`),
//! `s = a + b . u`. If `s >= 0` the nominal control passes (flag 0). Else
//! if `|b|^2 < 1e-12` the control cannot influence the constraint: it
//! passes, with flag 2. Else `u += -s b / |b|^2` (flag 1), the closed-form
//! solution of the one-constraint quadratic program `min |u - u_nom|^2`.
//! Then each control is limited to `umin..=umax`, when given.
//!
//! A barrier `h` that does not depend on any state the control moves
//! directly (relative degree above 1, such as a position limit with a
//! force input) has `b = 0` everywhere: the filter reports flag 2 and
//! does nothing. Write a higher-order barrier yourself (for a position
//! limit: `h = xmax - x0 - x1/k`, say). Two `cbf` nodes in a row project
//! one after the other; that is not the joint quadratic program, and the
//! second may undo the first.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::expr::{self, Expr, MAX_DERIVATIVE_NODES, Vars};
use crate::graph::{Config, Kind};
use crate::real::{Builder, Op, Program, Val};

/// Most operations in one node's program.
pub const MAX_OPS: usize = 20_000;
/// The default of the `scale` setting: three decimal places.
pub const DEFAULT_SCALE: u32 = 1000;
/// Below this `|b|^2` a `cbf` cannot influence its constraint (flag 2).
pub const CBF_WEAK: f64 = 1e-12;

/// How a node is connected and timed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shape {
    /// How many channels it reads.
    pub inputs: usize,
    /// How many channels it drives.
    pub outputs: usize,
    /// Its own update period.
    pub period_ms: u32,
}

/// Whether a kind is a model-based block: its outputs are ordered, one
/// channel each, and [`program`] says what it computes.
pub const fn is_model(kind: Kind) -> bool {
    matches!(kind, Kind::Ukf | Kind::Cbf)
}

struct Problems(Vec<String>);

impl Problems {
    fn exprs(&mut self, config: &Config, name: &str, vars: &Vars) -> Option<Vec<Expr>> {
        let text = config.get_text(name)?;
        match expr::parse_list(text, vars) {
            Ok(list) => Some(list),
            Err(e) => {
                self.0.push(format!("has a mistake in {name}, {e}"));
                None
            }
        }
    }

    fn numbers(&mut self, config: &Config, name: &str, count: usize, default: f64, least: f64) -> Option<Vec<f64>> {
        let Some(text) = config.get_text(name) else { return Some(vec![default; count]) };
        match expr::parse_numbers(text) {
            Ok(list) if list.len() != count => {
                self.0.push(format!("has {} number(s) in {name}; it needs {count}, separated by ;", list.len()));
                None
            }
            Ok(list) => {
                if let Some(bad) = list.iter().find(|v| **v < least) {
                    self.0.push(format!("has {bad} in {name}; a variance is not negative"));
                    return None;
                }
                Some(list)
            }
            Err(e) => {
                self.0.push(format!("has a mistake in {name}, {e}"));
                None
            }
        }
    }

    fn count(&mut self, name: &str, what: &str, have: usize, want: usize) -> bool {
        if have != want {
            self.0.push(format!("has {have} expression(s) in {name}; it needs {want}, {what}, separated by ;"));
        }
        have == want
    }
}

/// The program of a `ukf` or `cbf` node, or everything wrong with its
/// settings and connections. Each message completes "`<id> (<kind>) ...`".
/// Settings that are missing or out of their table range are the table
/// check's to report: with those this returns no message and no program.
pub fn program(kind: Kind, config: &Config, shape: &Shape) -> Result<Program, Vec<String>> {
    let mut problems = Problems(Vec::new());
    let built = match kind {
        Kind::Ukf => ukf(config, shape, &mut problems),
        Kind::Cbf => cbf(config, shape, &mut problems),
        _ => None,
    };
    match built {
        Some(program) if problems.0.is_empty() => {
            if program.ops.len() > MAX_OPS {
                return Err(vec![format!(
                    "is too large: its model compiles to {} operations, the most is {MAX_OPS}",
                    program.ops.len()
                )]);
            }
            Ok(program)
        }
        _ => Err(problems.0),
    }
}

/// The lower Cholesky factor of the symmetric `m` (its lower triangle,
/// `m[i][j]` for `j <= i`), and whether every pivot was greater than 0.
// Matrix code: the indices are the mathematics.
#[allow(clippy::needless_range_loop)]
fn cholesky(b: &mut Builder, m: &[Vec<Val>]) -> (Vec<Vec<Val>>, Val) {
    let n = m.len();
    let zero = b.num(0.0);
    let mut l: Vec<Vec<Val>> = vec![Vec::new(); n];
    let mut sound = Vec::new();
    for j in 0..n {
        let mut d = m[j][j];
        for k in 0..j {
            let square = b.mul(l[j][k], l[j][k]);
            d = b.sub(d, square);
        }
        sound.push(b.op(Op::Lt(zero, d)));
        let pivot = b.op(Op::Sqrt(d));
        l[j].push(pivot);
        for i in j + 1..n {
            let mut v = m[i][j];
            for k in 0..j {
                let product = b.mul(l[i][k], l[j][k]);
                v = b.sub(v, product);
            }
            let entry = b.div(v, pivot);
            l[i].push(entry);
        }
    }
    let sound = b.all(&sound).unwrap_or(zero);
    (l, sound)
}

/// The 2n + 1 sigma points of mean `x` and covariance `p` (its lower
/// triangle): `x`, then `x + L_i` and `x - L_i` for the columns of the
/// lower Cholesky factor `L` of `c p`; and whether the factor is sound.
// Matrix code: the indices are the mathematics.
#[allow(clippy::needless_range_loop)]
fn sigma_points(b: &mut Builder, c: Val, x: &[Val], p: &[Vec<Val>]) -> (Vec<Vec<Val>>, Val) {
    let n = x.len();
    let scaled: Vec<Vec<Val>> =
        p.iter().map(|row| row.iter().map(|&v| b.mul(c, v)).collect()).collect();
    let (l, sound) = cholesky(b, &scaled);
    let mut points: Vec<Vec<Val>> = vec![x.to_vec()];
    for sign in [1, -1] {
        for column in 0..n {
            let point = (0..n)
                .map(|i| match (i >= column, sign) {
                    (false, _) => x[i],
                    (true, 1) => b.add(x[i], l[i][column]),
                    (true, _) => b.sub(x[i], l[i][column]),
                })
                .collect();
            points.push(point);
        }
    }
    (points, sound)
}

/// `w0 * first + w * (the rest added up)`.
fn weighted(b: &mut Builder, w0: Val, w: Val, terms: &[Val]) -> Val {
    let head = b.mul(w0, terms[0]);
    let rest = b.sum(&terms[1..]);
    let rest = b.mul(w, rest);
    b.add(head, rest)
}

// Matrix code: the indices are the mathematics.
#[allow(clippy::needless_range_loop)]
fn ukf(config: &Config, shape: &Shape, problems: &mut Problems) -> Option<Program> {
    let n = config.n.filter(|n| (1..=4).contains(n))? as usize;
    // The measurement model first: its length says how many inputs are controls.
    let h = problems.exprs(config, "h", &Vars { states: n, controls: 0, dt: false });
    let m = h.as_ref().map(Vec::len);
    if let Some(m) = m.filter(|m| !(1..=4).contains(m)) {
        problems.0.push(format!("has {m} expression(s) in h; it needs 1 to 4, one per measurement"));
    }
    let controls = m.map(|m| shape.inputs.saturating_sub(m));
    if let Some(m) = m.filter(|m| (1..=4).contains(m))
        && !(m..=m + 4).contains(&shape.inputs)
    {
        problems.0.push(format!(
            "reads {} channel(s); it needs its {m} measurement(s) (one per expression of h) and then 0 to 4 controls",
            shape.inputs
        ));
    }
    // Without a known control count, any control parses; the count is reported above.
    let vars = Vars { states: n, controls: controls.map_or(4, |c| c.min(4)), dt: true };
    let f = problems.exprs(config, "f", &vars);
    if let Some(f) = &f {
        problems.count("f", "one per state", f.len(), n);
    }
    let q = problems.numbers(config, "q", n, 0.0, 0.0);
    let r = m.and_then(|m| problems.numbers(config, "r", m, 0.0, 0.0));
    let x0 = problems.numbers(config, "x0", n, 0.0, f64::NEG_INFINITY);
    let p0 = problems.numbers(config, "p0", n, 1.0, 0.0);
    let alpha = config.alpha.unwrap_or(1.0);
    let beta = config.beta.unwrap_or(2.0);
    let kappa = config.kappa.unwrap_or(0.0);
    let c = alpha * alpha * (n as f64 + kappa);
    if !(c > 0.0 && c.is_finite()) {
        problems.0.push(format!("has kappa {kappa}; n + kappa must be greater than 0"));
    }
    if shape.outputs != n && shape.outputs != 2 * n {
        problems.0.push(format!(
            "drives {} channel(s); it drives its {n} state estimate(s), or {} with their standard deviations",
            shape.outputs,
            2 * n
        ));
    }
    let (f, h, q, r, x0, p0, m) = (f?, h?, q?, r?, x0?, p0?, m?);
    if !problems.0.is_empty() {
        return None;
    }

    let mut b = Builder::new();
    let scale = b.num(f64::from(config.scale.unwrap_or(DEFAULT_SCALE)));
    let zero = b.num(0.0);
    let chan = |b: &mut Builder, k: usize| {
        let raw = b.op(Op::Chan(k as u32));
        b.div(raw, scale)
    };
    let z: Vec<Val> = (0..m).map(|k| chan(&mut b, k)).collect();
    let u: Vec<Val> = (m..shape.inputs).map(|k| chan(&mut b, k)).collect();
    let dt = b.num(f64::from(shape.period_ms) / 1000.0);

    // State words: x, then the lower triangle of P by rows, then the faults.
    let tri = |i: usize, j: usize| (n + i * (i + 1) / 2 + j) as u32;
    let faults_word = (n + n * (n + 1) / 2) as u32;
    let mut init = x0.clone();
    for i in 0..n {
        for j in 0..=i {
            init.push(if i == j { p0[i] } else { 0.0 });
        }
    }
    init.push(0.0);
    let x: Vec<Val> = (0..n).map(|i| b.op(Op::State(i as u32))).collect();
    let p: Vec<Vec<Val>> =
        (0..n).map(|i| (0..=i).map(|j| b.op(Op::State(tri(i, j)))).collect()).collect();

    let lambda = c - n as f64;
    let wm0 = b.num(lambda / c);
    let wc0 = b.num(lambda / c + (1.0 - alpha * alpha + beta));
    let w = b.num(1.0 / (2.0 * c));
    let c = b.num(c);

    // 1. Sigma points.
    let (points, sound_p) = sigma_points(&mut b, c, &x, &p);
    // 2. Through the transition; predicted mean and covariance.
    let moved: Vec<Vec<Val>> =
        points.iter().map(|point| f.iter().map(|e| e.lower(&mut b, point, &u, dt)).collect()).collect();
    let column = |rows: &[Vec<Val>], i: usize| rows.iter().map(|row| row[i]).collect::<Vec<Val>>();
    let xp: Vec<Val> = (0..n).map(|i| weighted(&mut b, wm0, w, &column(&moved, i))).collect();
    let dx: Vec<Vec<Val>> =
        moved.iter().map(|row| (0..n).map(|i| b.sub(row[i], xp[i])).collect()).collect();
    let covariance = |b: &mut Builder, left: &[Vec<Val>], i: usize, right: &[Vec<Val>], j: usize| {
        let products: Vec<Val> =
            left.iter().zip(right).map(|(l, r)| b.mul(l[i], r[j])).collect();
        weighted(b, wc0, w, &products)
    };
    let mut pp: Vec<Vec<Val>> = Vec::new();
    for i in 0..n {
        let mut row = Vec::new();
        for j in 0..=i {
            let mut v = covariance(&mut b, &dx, i, &dx, j);
            if i == j {
                let noise = b.num(q[i]);
                v = b.add(v, noise);
            }
            row.push(v);
        }
        pp.push(row);
    }
    // 3. New sigma points of the prediction, through the measurement model.
    let (again, sound_pp) = sigma_points(&mut b, c, &xp, &pp);
    let dx: Vec<Vec<Val>> =
        again.iter().map(|row| (0..n).map(|i| b.sub(row[i], xp[i])).collect()).collect();
    let seen: Vec<Vec<Val>> =
        again.iter().map(|point| h.iter().map(|e| e.lower(&mut b, point, &[], dt)).collect()).collect();
    let zp: Vec<Val> = (0..m).map(|a| weighted(&mut b, wm0, w, &column(&seen, a))).collect();
    let dz: Vec<Vec<Val>> =
        seen.iter().map(|row| (0..m).map(|a| b.sub(row[a], zp[a])).collect()).collect();
    let mut s: Vec<Vec<Val>> = Vec::new();
    for i in 0..m {
        let mut row = Vec::new();
        for j in 0..=i {
            let mut v = covariance(&mut b, &dz, i, &dz, j);
            if i == j {
                let noise = b.num(r[i]);
                v = b.add(v, noise);
            }
            row.push(v);
        }
        s.push(row);
    }
    let cross: Vec<Vec<Val>> =
        (0..n).map(|i| (0..m).map(|a| covariance(&mut b, &dx, i, &dz, a)).collect()).collect();
    // 4. Y = C Ls^-T (rows by forward substitution), K = Y Ls^-1 (back substitution).
    let (ls, sound_s) = cholesky(&mut b, &s);
    let mut y: Vec<Vec<Val>> = Vec::new();
    let mut gain: Vec<Vec<Val>> = Vec::new();
    for row in &cross {
        let mut yr: Vec<Val> = Vec::new();
        for a in 0..m {
            let mut v = row[a];
            for k in 0..a {
                let product = b.mul(ls[a][k], yr[k]);
                v = b.sub(v, product);
            }
            yr.push(b.div(v, ls[a][a]));
        }
        let mut kr = vec![zero; m];
        for a in (0..m).rev() {
            let mut v = yr[a];
            for k in a + 1..m {
                let product = b.mul(ls[k][a], kr[k]);
                v = b.sub(v, product);
            }
            kr[a] = b.div(v, ls[a][a]);
        }
        y.push(yr);
        gain.push(kr);
    }
    // 5. The update.
    let innovation: Vec<Val> = (0..m).map(|a| b.sub(z[a], zp[a])).collect();
    let mut finite = Vec::new();
    let mut xn = Vec::new();
    for i in 0..n {
        let terms: Vec<Val> = (0..m).map(|a| b.mul(gain[i][a], innovation[a])).collect();
        let correction = b.sum(&terms);
        let v = b.add(xp[i], correction);
        finite.push(b.finite(v));
        xn.push(v);
    }
    let mut pn: Vec<Vec<Val>> = Vec::new();
    for i in 0..n {
        let mut row = Vec::new();
        for j in 0..=i {
            let terms: Vec<Val> = (0..m).map(|a| b.mul(y[i][a], y[j][a])).collect();
            let drop = b.sum(&terms);
            let v = b.sub(pp[i][j], drop);
            finite.push(b.finite(v));
            row.push(v);
        }
        pn.push(row);
    }
    // Sound, or reset.
    let finite = b.all(&finite).unwrap_or(zero);
    let sound = b.op(Op::And(sound_p, sound_pp));
    let sound = b.op(Op::And(sound, sound_s));
    let sound = b.op(Op::And(sound, finite));
    let predicted: Vec<Val> = xp.iter().map(|&v| b.finite(v)).collect();
    let predicted = b.all(&predicted).unwrap_or(zero);
    let mut stores = Vec::new();
    let mut outputs = Vec::new();
    let mut spread = Vec::new();
    for i in 0..n {
        let start = b.num(x0[i]);
        let reset = b.op(Op::Select(predicted, xp[i], start));
        let value = b.op(Op::Select(sound, xn[i], reset));
        stores.push((i as u32, value));
        outputs.push(b.mul(value, scale));
        for j in 0..=i {
            let start = b.num(if i == j { p0[i] } else { 0.0 });
            let value = b.op(Op::Select(sound, pn[i][j], start));
            stores.push((tri(i, j), value));
            if i == j {
                let variance = b.op(Op::Max(value, zero));
                let deviation = b.op(Op::Sqrt(variance));
                spread.push(b.mul(deviation, scale));
            }
        }
    }
    let faults = b.op(Op::State(faults_word));
    let one = b.num(1.0);
    let step = b.op(Op::Select(sound, zero, one));
    stores.push((faults_word, b.add(faults, step)));
    if shape.outputs == 2 * n {
        outputs.extend(spread);
    }
    Some(b.finish(init, stores, outputs, Some(faults_word)))
}

fn cbf(config: &Config, shape: &Shape, problems: &mut Problems) -> Option<Program> {
    let n = config.n.filter(|n| (1..=4).contains(n))? as usize;
    let vars = Vars { states: n, controls: 0, dt: false };
    let m = shape.inputs.checked_sub(n).filter(|m| (1..=2).contains(m));
    if m.is_none() {
        problems.0.push(format!(
            "reads {} channel(s); it needs 1 or 2 nominal controls and then its {n} state(s)",
            shape.inputs
        ));
    }
    let h = problems.exprs(config, "h", &vars);
    if let Some(h) = &h {
        problems.count("h", "the one barrier function", h.len(), 1);
    }
    let f = problems.exprs(config, "f", &vars);
    if let Some(f) = &f {
        problems.count("f", "one per state", f.len(), n);
    }
    let g = problems.exprs(config, "g", &vars);
    if let (Some(g), Some(m)) = (&g, m) {
        problems.count("g", "a row per state with one entry per control", g.len(), n * m);
    }
    if let Some(m) = m
        && shape.outputs != m
        && shape.outputs != m + 1
    {
        problems.0.push(format!(
            "drives {} channel(s); it drives its {m} safe control(s), or {} with the flag",
            shape.outputs,
            m + 1
        ));
    }
    if let (Some(lo), Some(hi)) = (config.umin, config.umax)
        && lo > hi
    {
        problems.0.push(format!("has umin {lo} above umax {hi}; umin must not exceed umax"));
    }
    let (h, f, g, m) = (h?, f?, g?, m?);
    if !problems.0.is_empty() {
        return None;
    }
    let gradient: Vec<Expr> = (0..n).map(|i| h[0].diff(i)).collect();
    if gradient.iter().map(Expr::size).sum::<usize>() > MAX_DERIVATIVE_NODES {
        problems.0.push(format!(
            "has an h whose derivatives are too large (more than {MAX_DERIVATIVE_NODES} terms); simplify it"
        ));
        return None;
    }

    let mut b = Builder::new();
    let scale = b.num(f64::from(config.scale.unwrap_or(DEFAULT_SCALE)));
    let zero = b.num(0.0);
    let chan = |b: &mut Builder, k: usize| {
        let raw = b.op(Op::Chan(k as u32));
        b.div(raw, scale)
    };
    let u: Vec<Val> = (0..m).map(|j| chan(&mut b, j)).collect();
    let x: Vec<Val> = (m..m + n).map(|k| chan(&mut b, k)).collect();
    let lower = |b: &mut Builder, e: &Expr| e.lower(b, &x, &[], zero);
    let hv = lower(&mut b, &h[0]);
    let grad: Vec<Val> = gradient.iter().map(|e| lower(&mut b, e)).collect();
    let fv: Vec<Val> = f.iter().map(|e| lower(&mut b, e)).collect();
    let gv: Vec<Val> = g.iter().map(|e| lower(&mut b, e)).collect();
    let dot = |b: &mut Builder, right: &[Val]| {
        let terms: Vec<Val> = grad.iter().zip(right).map(|(&l, &r)| b.mul(l, r)).collect();
        b.sum(&terms)
    };
    // a = grad h . f + alpha h;  b_j = grad h . g_j;  s = a + b . u
    let drift = dot(&mut b, &fv);
    let alpha = b.num(config.alpha.unwrap_or(1.0));
    let pull = b.mul(alpha, hv);
    let a = b.add(drift, pull);
    let bs: Vec<Val> = (0..m)
        .map(|j| {
            let column: Vec<Val> = (0..n).map(|i| gv[i * m + j]).collect();
            dot(&mut b, &column)
        })
        .collect();
    let through: Vec<Val> = bs.iter().zip(&u).map(|(&l, &r)| b.mul(l, r)).collect();
    let through = b.sum(&through);
    let s = b.add(a, through);
    let squares: Vec<Val> = bs.iter().map(|&v| b.mul(v, v)).collect();
    let norm = b.sum(&squares);
    let keep = b.op(Op::Le(zero, s));
    let weak_limit = b.num(CBF_WEAK);
    let weak = b.op(Op::Lt(norm, weak_limit));
    let lack = b.op(Op::Neg(s));
    let mut outputs = Vec::new();
    for j in 0..m {
        let push = b.mul(lack, bs[j]);
        let push = b.div(push, norm);
        let moved = b.add(u[j], push);
        let v = b.op(Op::Select(weak, u[j], moved));
        let mut v = b.op(Op::Select(keep, u[j], v));
        if let Some(hi) = config.umax {
            let hi = b.num(hi);
            v = b.op(Op::Min(v, hi));
        }
        if let Some(lo) = config.umin {
            let lo = b.num(lo);
            v = b.op(Op::Max(v, lo));
        }
        outputs.push(b.mul(v, scale));
    }
    if shape.outputs == m + 1 {
        let (one, two) = (b.num(1.0), b.num(2.0));
        let flag = b.op(Op::Select(weak, two, one));
        outputs.push(b.op(Op::Select(keep, zero, flag)));
    }
    Some(b.finish(Vec::new(), Vec::new(), outputs, None))
}
