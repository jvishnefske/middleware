//! The expressions a model-based block's settings are written in.
//!
//! ```text
//! list    := expr (';' expr)*
//! expr    := term (('+' | '-') term)*
//! term    := unary (('*' | '/') unary)*
//! unary   := '-' unary | primary
//! primary := number | name | function '(' expr (',' expr)* ')' | '(' expr ')'
//! number  := digits ['.' digits] [('e' | 'E') ['+' | '-'] digits]   (or '.' digits ...)
//! ```
//!
//! Names: `x0`, `x1`, ... (the state), `u0`, `u1`, ... (the controls),
//! `dt` (the node's own update period in seconds) and the constant `pi`.
//! Functions: `sqrt(a)`, `abs(a)`, `sin(a)`, `cos(a)`, `sq(a)` (= `a*a`),
//! `min(a,b)`, `max(a,b)`. There is no `atan2` or `exp`: a function is
//! only offered when the payload and the reference compute it with the
//! very same operations ([`crate::real`]).
//!
//! One parsed [`Expr`] serves the check (names, arity, size), the
//! reference and the compiler (both through [`Expr::lower`], which turns
//! it into operations of a [`crate::real::Program`]) and symbolic
//! differentiation ([`Expr::diff`]).
//!
//! An expression has at most [`MAX_NODES`] nodes nested at most
//! [`MAX_DEPTH`] deep, and a setting at most [`MAX_TEXT`] bytes, which
//! bounds the code a block compiles to.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use crate::real::{Builder, Op, Val};

/// Most nodes in one parsed expression.
pub const MAX_NODES: usize = 256;
/// Deepest nesting of one parsed expression.
pub const MAX_DEPTH: usize = 32;
/// Longest expression setting, in bytes.
pub const MAX_TEXT: usize = 1024;
/// Most nodes in a derivative ([`Expr::diff`] output a block accepts).
pub const MAX_DERIVATIVE_NODES: usize = 4096;

/// A function of one or two expressions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Func {
    /// Square root.
    Sqrt,
    /// Absolute value.
    Abs,
    /// Sine ([`crate::real::sin_program`]).
    Sin,
    /// Cosine.
    Cos,
    /// The square, `a*a`.
    Sq,
    /// The smaller of two.
    Min,
    /// The larger of two.
    Max,
}

impl Func {
    const ALL: [Func; 7] =
        [Func::Sqrt, Func::Abs, Func::Sin, Func::Cos, Func::Sq, Func::Min, Func::Max];

    /// Its name.
    pub const fn name(self) -> &'static str {
        match self {
            Func::Sqrt => "sqrt",
            Func::Abs => "abs",
            Func::Sin => "sin",
            Func::Cos => "cos",
            Func::Sq => "sq",
            Func::Min => "min",
            Func::Max => "max",
        }
    }

    /// How many arguments it takes.
    pub const fn arity(self) -> usize {
        match self {
            Func::Min | Func::Max => 2,
            _ => 1,
        }
    }
}

/// A parsed expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// A number (`pi` is one).
    Num(f64),
    /// State `x<i>`.
    X(usize),
    /// Control `u<i>`.
    U(usize),
    /// The update period in seconds.
    Dt,
    /// `-a`.
    Neg(Box<Expr>),
    /// `a + b`.
    Add(Box<Expr>, Box<Expr>),
    /// `a - b`.
    Sub(Box<Expr>, Box<Expr>),
    /// `a * b`.
    Mul(Box<Expr>, Box<Expr>),
    /// `a / b`.
    Div(Box<Expr>, Box<Expr>),
    /// A function call.
    Call(Func, Vec<Expr>),
    /// `then` if `left <= right`, else `otherwise`. Not written by users:
    /// differentiation produces it for `abs`, `min` and `max`. It prints
    /// as `ifle(left,right,then,otherwise)`.
    IfLe(Box<Expr>, Box<Expr>, Box<Expr>, Box<Expr>),
}

/// The names an expression may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vars {
    /// `x0` to `x<states-1>`.
    pub states: usize,
    /// `u0` to `u<controls-1>`.
    pub controls: usize,
    /// Whether `dt` is allowed.
    pub dt: bool,
}

impl Vars {
    fn describe(&self) -> String {
        let mut names = Vec::new();
        let range = |letter: char, n: usize| match n {
            1 => format!("{letter}0"),
            n => format!("{letter}0 to {letter}{}", n - 1),
        };
        if self.states > 0 {
            names.push(range('x', self.states));
        }
        if self.controls > 0 {
            names.push(range('u', self.controls));
        }
        if self.dt {
            names.push("dt".to_string());
        }
        names.push("pi".to_string());
        names.join(", ")
    }
}

/// Why a text is not an expression, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExprError {
    /// Which expression of a list, from 1.
    pub index: usize,
    /// The column in the whole text, from 1.
    pub column: usize,
    /// What is wrong.
    pub message: String,
}

impl fmt::Display for ExprError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "expression {}, column {}: {}", self.index, self.column, self.message)
    }
}

struct Parser<'a> {
    text: &'a [u8],
    at: usize,
    end: usize,
    vars: &'a Vars,
    nodes: usize,
}

type Parsed<T> = Result<T, (usize, String)>;

impl Parser<'_> {
    fn space(&mut self) {
        while self.at < self.end && self.text[self.at].is_ascii_whitespace() {
            self.at += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.space();
        (self.at < self.end).then(|| self.text[self.at])
    }

    fn node(&mut self, at: usize, e: Expr) -> Parsed<Expr> {
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            return Err((at, format!("the expression is too large (more than {MAX_NODES} terms)")));
        }
        Ok(e)
    }

    fn expr(&mut self, depth: usize) -> Parsed<Expr> {
        let mut left = self.term(depth)?;
        while let Some(op @ (b'+' | b'-')) = self.peek() {
            let at = self.at;
            self.at += 1;
            let right = Box::new(self.term(depth)?);
            let e = if op == b'+' { Expr::Add(Box::new(left), right) } else { Expr::Sub(Box::new(left), right) };
            left = self.node(at, e)?;
        }
        Ok(left)
    }

    fn term(&mut self, depth: usize) -> Parsed<Expr> {
        let mut left = self.unary(depth)?;
        while let Some(op @ (b'*' | b'/')) = self.peek() {
            let at = self.at;
            self.at += 1;
            let right = Box::new(self.unary(depth)?);
            let e = if op == b'*' { Expr::Mul(Box::new(left), right) } else { Expr::Div(Box::new(left), right) };
            left = self.node(at, e)?;
        }
        Ok(left)
    }

    fn unary(&mut self, depth: usize) -> Parsed<Expr> {
        if depth > MAX_DEPTH {
            return Err((self.at, format!("the expression is nested too deep (more than {MAX_DEPTH})")));
        }
        if self.peek() == Some(b'-') {
            let at = self.at;
            self.at += 1;
            let inner = self.unary(depth + 1)?;
            return self.node(at, Expr::Neg(Box::new(inner)));
        }
        self.primary(depth)
    }

    fn primary(&mut self, depth: usize) -> Parsed<Expr> {
        let Some(c) = self.peek() else {
            return Err((self.at, "an expression ends here; a number, a name or '(' is missing".into()));
        };
        let start = self.at;
        if c == b'(' {
            self.at += 1;
            let inner = self.expr(depth + 1)?;
            if self.peek() != Some(b')') {
                return Err((self.at, "a ')' is missing".into()));
            }
            self.at += 1;
            return Ok(inner);
        }
        if c.is_ascii_digit() || c == b'.' {
            let digits = |p: &mut Self| {
                while p.at < p.end && p.text[p.at].is_ascii_digit() {
                    p.at += 1;
                }
            };
            digits(self);
            if self.at < self.end && self.text[self.at] == b'.' {
                self.at += 1;
                digits(self);
            }
            if self.at < self.end && matches!(self.text[self.at], b'e' | b'E') {
                let mark = self.at;
                self.at += 1;
                if self.at < self.end && matches!(self.text[self.at], b'+' | b'-') {
                    self.at += 1;
                }
                if self.at < self.end && self.text[self.at].is_ascii_digit() {
                    digits(self);
                } else {
                    self.at = mark;
                }
            }
            let word = core::str::from_utf8(&self.text[start..self.at]).unwrap_or("");
            return match word.parse::<f64>() {
                Ok(v) if v.is_finite() => self.node(start, Expr::Num(v)),
                _ => Err((start, format!("'{word}' is not a number"))),
            };
        }
        if c.is_ascii_alphabetic() {
            while self.at < self.end && (self.text[self.at].is_ascii_alphanumeric() || self.text[self.at] == b'_') {
                self.at += 1;
            }
            let word = core::str::from_utf8(&self.text[start..self.at]).unwrap_or("");
            if self.peek() == Some(b'(') {
                let Some(func) = Func::ALL.into_iter().find(|f| f.name() == word) else {
                    let names: Vec<&str> = Func::ALL.iter().map(|f| f.name()).collect();
                    return Err((start, format!("'{word}' is not a function ({})", names.join(", "))));
                };
                self.at += 1;
                let mut args = Vec::new();
                loop {
                    args.push(self.expr(depth + 1)?);
                    match self.peek() {
                        Some(b',') => self.at += 1,
                        Some(b')') => {
                            self.at += 1;
                            break;
                        }
                        _ => return Err((self.at, format!("a ',' or ')' is missing in {word}(...)"))),
                    }
                }
                if args.len() != func.arity() {
                    return Err((
                        start,
                        format!("{word} takes {} argument(s), not {}", func.arity(), args.len()),
                    ));
                }
                return self.node(start, Expr::Call(func, args));
            }
            let indexed = |prefix: &str| {
                word.strip_prefix(prefix)
                    .filter(|d| !d.is_empty() && d.len() <= 3 && d.bytes().all(|b| b.is_ascii_digit()))
                    .and_then(|d| d.parse::<usize>().ok())
            };
            let e = match word {
                "pi" => Expr::Num(core::f64::consts::PI),
                "dt" if self.vars.dt => Expr::Dt,
                _ => match (indexed("x"), indexed("u")) {
                    (Some(i), _) if i < self.vars.states => Expr::X(i),
                    (_, Some(i)) if i < self.vars.controls => Expr::U(i),
                    (Some(i), _) => {
                        return Err((
                            start,
                            format!("there is no state x{i}: this block has {} ({})", self.vars.states, self.vars.describe()),
                        ));
                    }
                    (_, Some(i)) if self.vars.controls > 0 => {
                        return Err((
                            start,
                            format!(
                                "there is no control u{i}: this block has {} ({})",
                                self.vars.controls,
                                self.vars.describe()
                            ),
                        ));
                    }
                    _ => {
                        return Err((
                            start,
                            format!("'{word}' is not a name here ({})", self.vars.describe()),
                        ));
                    }
                },
            };
            return self.node(start, e);
        }
        Err((start, format!("'{}' does not belong in an expression", char::from(c))))
    }
}

/// Parses one expression.
pub fn parse(text: &str, vars: &Vars) -> Result<Expr, ExprError> {
    match parse_list(text, vars)?.as_mut_slice() {
        [one] => Ok(core::mem::replace(one, Expr::Dt)),
        _ => Err(ExprError {
            index: 2,
            column: text.find(';').map_or(1, |i| i + 1),
            message: "one expression is expected, not a list".into(),
        }),
    }
}

/// Parses a `;`-separated list of expressions.
pub fn parse_list(text: &str, vars: &Vars) -> Result<Vec<Expr>, ExprError> {
    if text.len() > MAX_TEXT {
        return Err(ExprError {
            index: 1,
            column: MAX_TEXT + 1,
            message: format!("the text is longer than {MAX_TEXT} characters"),
        });
    }
    let mut out = Vec::new();
    let mut start = 0;
    for (index, part) in text.split(';').enumerate() {
        let end = start + part.len();
        let mut p = Parser { text: text.as_bytes(), at: start, end, vars, nodes: 0 };
        let fail = |(at, message): (usize, String)| ExprError { index: index + 1, column: at + 1, message };
        let e = p.expr(0).map_err(fail)?;
        if let Some(c) = p.peek() {
            let message = match c {
                b')' => "this ')' has no '('".to_string(),
                b',' => "a ',' belongs between a function's arguments only".to_string(),
                c if c.is_ascii_alphanumeric() || c == b'(' || c == b'.' => {
                    "an operator (+ - * /) is missing before this".to_string()
                }
                c => format!("'{}' does not belong in an expression", char::from(c)),
            };
            return Err(fail((p.at, message)));
        }
        out.push(e);
        start = end + 1;
    }
    Ok(out)
}

/// Parses a `;`-separated list of numbers.
pub fn parse_numbers(text: &str) -> Result<Vec<f64>, ExprError> {
    let mut out = Vec::new();
    let mut start = 0;
    for (index, part) in text.split(';').enumerate() {
        let word = part.trim();
        match word.parse::<f64>() {
            Ok(v) if v.is_finite() && !word.is_empty() && word.len() <= 32 => out.push(v),
            _ => {
                return Err(ExprError {
                    index: index + 1,
                    column: start + (part.len() - part.trim_start().len()) + 1,
                    message: format!("'{word}' is not a number"),
                });
            }
        }
        start += part.len() + 1;
    }
    Ok(out)
}

impl Expr {
    fn precedence(&self) -> u8 {
        match self {
            Expr::Add(..) | Expr::Sub(..) => 1,
            Expr::Mul(..) | Expr::Div(..) => 2,
            Expr::Neg(..) => 3,
            Expr::Num(v) if v.is_sign_negative() => 3,
            _ => 4,
        }
    }

    fn write(&self, f: &mut fmt::Formatter<'_>, at_least: u8) -> fmt::Result {
        let mine = self.precedence();
        if mine < at_least {
            f.write_str("(")?;
        }
        match self {
            Expr::Num(v) => write!(f, "{v}")?,
            Expr::X(i) => write!(f, "x{i}")?,
            Expr::U(i) => write!(f, "u{i}")?,
            Expr::Dt => f.write_str("dt")?,
            Expr::Neg(a) => {
                f.write_str("-")?;
                // A unary minus binds its operand whole: -(-a), -(a*b) as -a*b is the same value.
                a.write(f, 4)?;
            }
            Expr::Add(a, b) => {
                a.write(f, 1)?;
                f.write_str("+")?;
                b.write(f, 2)?;
            }
            Expr::Sub(a, b) => {
                a.write(f, 1)?;
                f.write_str("-")?;
                b.write(f, 2)?;
            }
            Expr::Mul(a, b) => {
                a.write(f, 2)?;
                f.write_str("*")?;
                b.write(f, 3)?;
            }
            Expr::Div(a, b) => {
                a.write(f, 2)?;
                f.write_str("/")?;
                b.write(f, 3)?;
            }
            Expr::Call(func, args) => {
                write!(f, "{}(", func.name())?;
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    a.write(f, 0)?;
                }
                f.write_str(")")?;
            }
            Expr::IfLe(l, r, t, e) => {
                f.write_str("ifle(")?;
                l.write(f, 0)?;
                f.write_str(",")?;
                r.write(f, 0)?;
                f.write_str(",")?;
                t.write(f, 0)?;
                f.write_str(",")?;
                e.write(f, 0)?;
                f.write_str(")")?;
            }
        }
        if mine < at_least {
            f.write_str(")")?;
        }
        Ok(())
    }

    /// How many nodes the expression has.
    pub fn size(&self) -> usize {
        1 + match self {
            Expr::Num(_) | Expr::X(_) | Expr::U(_) | Expr::Dt => 0,
            Expr::Neg(a) => a.size(),
            Expr::Add(a, b) | Expr::Sub(a, b) | Expr::Mul(a, b) | Expr::Div(a, b) => a.size() + b.size(),
            Expr::Call(_, args) => args.iter().map(Expr::size).sum(),
            Expr::IfLe(l, r, t, e) => l.size() + r.size() + t.size() + e.size(),
        }
    }

    /// The largest control index the expression names, plus one.
    pub fn controls(&self) -> usize {
        match self {
            Expr::U(i) => i + 1,
            Expr::Num(_) | Expr::X(_) | Expr::Dt => 0,
            Expr::Neg(a) => a.controls(),
            Expr::Add(a, b) | Expr::Sub(a, b) | Expr::Mul(a, b) | Expr::Div(a, b) => a.controls().max(b.controls()),
            Expr::Call(_, args) => args.iter().map(Expr::controls).max().unwrap_or(0),
            Expr::IfLe(l, r, t, e) => l.controls().max(r.controls()).max(t.controls()).max(e.controls()),
        }
    }

    /// The value for the given state, controls and period, with the same
    /// operations a block performs (through [`Expr::lower`]).
    pub fn value(&self, x: &[f64], u: &[f64], dt: f64) -> f64 {
        let mut b = Builder::new();
        let xs: Vec<Val> = x.iter().map(|&v| b.num(v)).collect();
        let us: Vec<Val> = u.iter().map(|&v| b.num(v)).collect();
        let dt = b.num(dt);
        let result = self.lower(&mut b, &xs, &us, dt);
        let program = b.finish(Vec::new(), Vec::new(), alloc::vec![result], None);
        let sin = crate::real::sin_program();
        program.eval(&|_| 0, &[], &[], &sin)[program.outputs[0] as usize]
    }

    /// Adds the expression's operations to `b`; `x`, `u` and `dt` are the
    /// values of the names. A missing name is 0.
    pub fn lower(&self, b: &mut Builder, x: &[Val], u: &[Val], dt: Val) -> Val {
        let go = |e: &Expr, b: &mut Builder| e.lower(b, x, u, dt);
        match self {
            Expr::Num(v) => b.num(*v),
            Expr::X(i) => x.get(*i).copied().unwrap_or_else(|| b.num(0.0)),
            Expr::U(i) => u.get(*i).copied().unwrap_or_else(|| b.num(0.0)),
            Expr::Dt => dt,
            Expr::Neg(a) => {
                let a = go(a, b);
                b.op(Op::Neg(a))
            }
            Expr::Add(l, r) => {
                let (l, r) = (go(l, b), go(r, b));
                b.add(l, r)
            }
            Expr::Sub(l, r) => {
                let (l, r) = (go(l, b), go(r, b));
                b.sub(l, r)
            }
            Expr::Mul(l, r) => {
                let (l, r) = (go(l, b), go(r, b));
                b.mul(l, r)
            }
            Expr::Div(l, r) => {
                let (l, r) = (go(l, b), go(r, b));
                b.div(l, r)
            }
            Expr::Call(func, args) => {
                let a = match args.first() {
                    Some(a) => go(a, b),
                    None => b.num(0.0),
                };
                match func {
                    Func::Sqrt => b.op(Op::Sqrt(a)),
                    Func::Abs => b.op(Op::Abs(a)),
                    Func::Sq => b.mul(a, a),
                    Func::Sin | Func::Cos => {
                        let shift = b.num(if *func == Func::Sin { 0.0 } else { 1.0 });
                        b.op(Op::Sin(a, shift))
                    }
                    Func::Min | Func::Max => {
                        let second = match args.get(1) {
                            Some(a) => go(a, b),
                            None => b.num(0.0),
                        };
                        b.op(if *func == Func::Min { Op::Min(a, second) } else { Op::Max(a, second) })
                    }
                }
            }
            Expr::IfLe(l, r, t, e) => {
                let (l, r) = (go(l, b), go(r, b));
                let condition = b.op(Op::Le(l, r));
                let (t, e) = (go(t, b), go(e, b));
                b.op(Op::Select(condition, t, e))
            }
        }
    }

    /// The partial derivative with respect to state `x<i>`, lightly
    /// simplified (`0*a`, `1*a`, `a+0`, constant folding).
    ///
    /// Where a function has a kink the derivative is piecewise, and at the
    /// kink itself one side is chosen: `abs(a)` differentiates as `a` for
    /// `a >= 0` (so +1 at 0), and `min(a,b)` and `max(a,b)` as their first
    /// argument when the two are equal. `sqrt` differentiates to
    /// `a'/(2*sqrt(a))`, which is infinite or NaN at 0, as it should be.
    pub fn diff(&self, i: usize) -> Expr {
        match self {
            Expr::Num(_) | Expr::U(_) | Expr::Dt => Expr::Num(0.0),
            Expr::X(k) => Expr::Num(if *k == i { 1.0 } else { 0.0 }),
            Expr::Neg(a) => neg(a.diff(i)),
            Expr::Add(a, b) => add(a.diff(i), b.diff(i)),
            Expr::Sub(a, b) => sub(a.diff(i), b.diff(i)),
            Expr::Mul(a, b) => add(mul(a.diff(i), (**b).clone()), mul((**a).clone(), b.diff(i))),
            Expr::Div(a, b) => {
                let (da, db) = (a.diff(i), b.diff(i));
                if db == Expr::Num(0.0) {
                    div(da, (**b).clone())
                } else {
                    let top = sub(mul(da, (**b).clone()), mul((**a).clone(), db));
                    div(top, mul((**b).clone(), (**b).clone()))
                }
            }
            Expr::Call(func, args) => {
                let a = &args[0];
                let da = a.diff(i);
                match func {
                    Func::Sqrt => div(da, mul(Expr::Num(2.0), self.clone())),
                    Func::Sq => mul(mul(Expr::Num(2.0), a.clone()), da),
                    Func::Sin => mul(Expr::Call(Func::Cos, alloc::vec![a.clone()]), da),
                    Func::Cos => neg(mul(Expr::Call(Func::Sin, alloc::vec![a.clone()]), da)),
                    Func::Abs => if_le(Expr::Num(0.0), a.clone(), da.clone(), neg(da)),
                    Func::Min => if_le(a.clone(), args[1].clone(), da, args[1].diff(i)),
                    Func::Max => if_le(args[1].clone(), a.clone(), da, args[1].diff(i)),
                }
            }
            Expr::IfLe(l, r, t, e) => if_le((**l).clone(), (**r).clone(), t.diff(i), e.diff(i)),
        }
    }
}

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.write(f, 0)
    }
}

fn is(e: &Expr, v: f64) -> bool {
    matches!(e, Expr::Num(n) if *n == v)
}

fn neg(a: Expr) -> Expr {
    match a {
        Expr::Num(v) => Expr::Num(if v == 0.0 { 0.0 } else { -v }),
        Expr::Neg(inner) => *inner,
        a => Expr::Neg(Box::new(a)),
    }
}

fn add(a: Expr, b: Expr) -> Expr {
    match (a, b) {
        (Expr::Num(x), Expr::Num(y)) => Expr::Num(x + y),
        (a, b) if is(&a, 0.0) => b,
        (a, b) if is(&b, 0.0) => a,
        (a, b) => Expr::Add(Box::new(a), Box::new(b)),
    }
}

fn sub(a: Expr, b: Expr) -> Expr {
    match (a, b) {
        (Expr::Num(x), Expr::Num(y)) => Expr::Num(x - y),
        (a, b) if is(&b, 0.0) => a,
        (a, b) if is(&a, 0.0) => neg(b),
        (a, b) => Expr::Sub(Box::new(a), Box::new(b)),
    }
}

fn mul(a: Expr, b: Expr) -> Expr {
    match (a, b) {
        (Expr::Num(x), Expr::Num(y)) => Expr::Num(x * y),
        (a, b) if is(&a, 0.0) || is(&b, 0.0) => Expr::Num(0.0),
        (a, b) if is(&a, 1.0) => b,
        (a, b) if is(&b, 1.0) => a,
        (a, b) => Expr::Mul(Box::new(a), Box::new(b)),
    }
}

fn div(a: Expr, b: Expr) -> Expr {
    match (a, b) {
        (a, _) if is(&a, 0.0) => Expr::Num(0.0),
        (a, b) if is(&b, 1.0) => a,
        (a, b) => Expr::Div(Box::new(a), Box::new(b)),
    }
}

fn if_le(l: Expr, r: Expr, t: Expr, e: Expr) -> Expr {
    if t == e {
        return t;
    }
    if let (Expr::Num(x), Expr::Num(y)) = (&l, &r) {
        return if x <= y { t } else { e };
    }
    Expr::IfLe(Box::new(l), Box::new(r), Box::new(t), Box::new(e))
}
