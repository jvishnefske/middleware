//! Guard expressions as a flat arena, evaluated in one forward pass.
//!
//! A [`Guard`] is a slice of [`Node`]s in which every child index refers
//! to an *earlier* node and the last node is the root. That order makes
//! evaluation a single loop over the slice with a fixed-size value array
//! ([`MAX_GUARD_NODES`] entries): no recursion, no heap, and time linear
//! in the node count. A tree of depth [`MAX_GUARD_DEPTH`] (a leaf alone is
//! depth 1) has at most `2^4 - 1 = 15` nodes, which is the node limit.
//!
//! Values are `i32`. A pin read is 0 or 1; a state test
//! ([`Node::State`], "machine M is in state S") is 0 or 1; a comparison yields 0 or 1;
//! `and` / `or` treat any non-zero value as true and yield 0 or 1; `not`
//! yields 1 for 0 and 0 otherwise. Nothing does arithmetic, so nothing can
//! overflow.
//!
//! Pins and state tests are the guard's *leaves* ([`Leaf`]): every value
//! depends only on them, and each is 0 or 1, so trying every assignment of
//! a guard's distinct leaves ([`LeafSet`], [`holds_under`]) decides it
//! exactly. Two state tests of one machine naming different states cannot
//! both be 1 (a machine is in one state), so such assignments are skipped
//! ([`consistent`]).
//!
//! [`Guard::shape`] reports a malformed arena (empty, too many nodes, a
//! child that is not earlier) and the depth; [`Guard::eval`] refuses the
//! same malformed arenas but ignores depth, which is a policy the checks
//! ([`crate::check`]) apply.

/// Deepest guard: a leaf at the root is depth 1.
pub const MAX_GUARD_DEPTH: u8 = 4;

/// Most nodes in a guard's arena: a full binary tree of depth 4.
pub const MAX_GUARD_NODES: usize = 15;

/// A comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    /// `a == b`.
    Eq,
    /// `a != b`.
    Ne,
    /// `a < b`.
    Lt,
    /// `a <= b`.
    Le,
    /// `a > b`.
    Gt,
    /// `a >= b`.
    Ge,
}

impl CmpOp {
    /// Every operator, in the model's order.
    pub const ALL: [CmpOp; 6] = [
        CmpOp::Eq,
        CmpOp::Ne,
        CmpOp::Lt,
        CmpOp::Le,
        CmpOp::Gt,
        CmpOp::Ge,
    ];

    /// The model's name (`"eq"`, `"ne"`, ...).
    pub const fn name(self) -> &'static str {
        match self {
            CmpOp::Eq => "eq",
            CmpOp::Ne => "ne",
            CmpOp::Lt => "lt",
            CmpOp::Le => "le",
            CmpOp::Gt => "gt",
            CmpOp::Ge => "ge",
        }
    }

    /// The operator a model name stands for.
    pub fn parse(name: &str) -> Option<CmpOp> {
        CmpOp::ALL.into_iter().find(|o| o.name() == name)
    }

    /// `a op b`.
    pub const fn apply(self, a: i32, b: i32) -> bool {
        match self {
            CmpOp::Eq => a == b,
            CmpOp::Ne => a != b,
            CmpOp::Lt => a < b,
            CmpOp::Le => a <= b,
            CmpOp::Gt => a > b,
            CmpOp::Ge => a >= b,
        }
    }
}

/// A leaf a guard's value depends on: a digital pin or a machine's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leaf<'a> {
    /// The pin with this label reads high.
    Pin(&'a str),
    /// The machine with this id is in this state.
    State {
        /// The machine's id.
        machine: &'a str,
        /// The state's name.
        state: &'a str,
    },
}

/// One node of a guard's arena. Child indices refer to earlier nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Node<'a> {
    /// A digital read of the pin with this label: 0 or 1.
    Pin(&'a str),
    /// 1 while machine `machine` is in state `state`, else 0.
    State {
        /// The machine's id.
        machine: &'a str,
        /// The state's name.
        state: &'a str,
    },
    /// A constant.
    Const(i32),
    /// A comparison of two nodes: 0 or 1.
    Cmp {
        /// The operator.
        op: CmpOp,
        /// Left operand (node index).
        a: usize,
        /// Right operand (node index).
        b: usize,
    },
    /// Both non-zero: 0 or 1.
    And(usize, usize),
    /// Either non-zero: 0 or 1.
    Or(usize, usize),
    /// Zero: 0 or 1.
    Not(usize),
}

impl Node<'_> {
    /// The node's children (unused slots are `None`).
    pub const fn children(&self) -> [Option<usize>; 2] {
        match *self {
            Node::Pin(_) | Node::State { .. } | Node::Const(_) => [None, None],
            Node::Cmp { a, b, .. } | Node::And(a, b) | Node::Or(a, b) => [Some(a), Some(b)],
            Node::Not(a) => [Some(a), None],
        }
    }
}

/// Why a guard's arena cannot be evaluated or is not allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardError {
    /// No nodes.
    Empty,
    /// More than [`MAX_GUARD_NODES`] nodes.
    TooLarge {
        /// How many there are.
        nodes: usize,
    },
    /// Node `node` refers to itself or a later node.
    BadChild {
        /// The node's index.
        node: usize,
    },
    /// Nested deeper than [`MAX_GUARD_DEPTH`].
    TooDeep {
        /// The root's depth.
        depth: u8,
    },
}

/// A guard: a flat arena whose last node is the root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Guard<'a> {
    /// The nodes; children refer to earlier indices.
    pub nodes: &'a [Node<'a>],
}

impl<'a> Node<'a> {
    /// The leaf this node is, if it is one.
    pub const fn leaf(&self) -> Option<Leaf<'a>> {
        match *self {
            Node::Pin(label) => Some(Leaf::Pin(label)),
            Node::State { machine, state } => Some(Leaf::State { machine, state }),
            _ => None,
        }
    }
}

/// Distinct leaves of one or two guards, in first-seen order.
#[derive(Debug, Clone, Copy)]
pub struct LeafSet<'a> {
    leaves: [Leaf<'a>; 2 * MAX_GUARD_NODES],
    len: usize,
}

impl<'a> LeafSet<'a> {
    /// An empty set.
    pub const fn new() -> Self {
        LeafSet {
            leaves: [Leaf::Pin(""); 2 * MAX_GUARD_NODES],
            len: 0,
        }
    }

    /// Adds the leaves `g` reads (only the first [`MAX_GUARD_NODES`] nodes
    /// are looked at, so a set of two guards never overflows).
    pub fn with(mut self, g: &Guard<'a>) -> Self {
        for node in g.nodes.iter().take(MAX_GUARD_NODES) {
            if let Some(leaf) = node.leaf()
                && !self.contains(leaf)
                && let Some(slot) = self.leaves.get_mut(self.len)
            {
                *slot = leaf;
                self.len += 1;
            }
        }
        self
    }

    /// Whether `leaf` is in the set.
    pub fn contains(&self, leaf: Leaf<'_>) -> bool {
        self.index(leaf).is_some()
    }

    /// The leaf's position, if present.
    pub fn index(&self, leaf: Leaf<'_>) -> Option<usize> {
        self.as_slice().iter().position(|l| *l == leaf)
    }

    /// How many distinct leaves.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether there are none.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The leaves, in first-seen order.
    pub fn as_slice(&self) -> &[Leaf<'a>] {
        self.leaves.get(..self.len).unwrap_or(&[])
    }

    /// The pin labels among the leaves, in first-seen order.
    pub fn pins(&self) -> impl Iterator<Item = &'a str> + '_ {
        self.as_slice().iter().filter_map(|l| match *l {
            Leaf::Pin(label) => Some(label),
            Leaf::State { .. } => None,
        })
    }
}

impl Default for LeafSet<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> Guard<'a> {
    /// Checks the arena; returns the root's depth.
    pub fn shape(&self) -> Result<u8, GuardError> {
        let depths = self.depths()?;
        let root = depths
            .get(self.nodes.len() - 1)
            .copied()
            .ok_or(GuardError::Empty)?;
        if root > MAX_GUARD_DEPTH {
            Err(GuardError::TooDeep { depth: root })
        } else {
            Ok(root)
        }
    }

    /// Each node's depth, after checking size and child order.
    fn depths(&self) -> Result<[u8; MAX_GUARD_NODES], GuardError> {
        let n = self.nodes.len();
        if n == 0 {
            return Err(GuardError::Empty);
        }
        if n > MAX_GUARD_NODES {
            return Err(GuardError::TooLarge { nodes: n });
        }
        let mut depth = [0u8; MAX_GUARD_NODES];
        for (i, node) in self.nodes.iter().enumerate() {
            let mut d = 1u8;
            for c in node.children().into_iter().flatten() {
                if c >= i {
                    return Err(GuardError::BadChild { node: i });
                }
                d = d.max(depth.get(c).copied().unwrap_or(0).saturating_add(1));
            }
            if let Some(slot) = depth.get_mut(i) {
                *slot = d;
            }
        }
        Ok(depth)
    }

    /// The guard's value with its leaves read through `read` (one forward
    /// pass).
    pub fn eval(&self, read: impl Fn(Leaf<'_>) -> bool) -> Result<i32, GuardError> {
        self.depths()?;
        let mut v = [0i32; MAX_GUARD_NODES];
        for (i, node) in self.nodes.iter().enumerate() {
            let at = |j: usize| v.get(j).copied().unwrap_or(0);
            let x = match *node {
                Node::Pin(label) => i32::from(read(Leaf::Pin(label))),
                Node::State { machine, state } => i32::from(read(Leaf::State { machine, state })),
                Node::Const(c) => c,
                Node::Cmp { op, a, b } => i32::from(op.apply(at(a), at(b))),
                Node::And(a, b) => i32::from(at(a) != 0 && at(b) != 0),
                Node::Or(a, b) => i32::from(at(a) != 0 || at(b) != 0),
                Node::Not(a) => i32::from(at(a) == 0),
            };
            if let Some(slot) = v.get_mut(i) {
                *slot = x;
            }
        }
        Ok(v.get(self.nodes.len() - 1).copied().unwrap_or(0))
    }

    /// Whether the guard is true with its leaves read through `read`; a
    /// malformed arena is never true.
    pub fn holds(&self, read: impl Fn(Leaf<'_>) -> bool) -> bool {
        self.eval(read).is_ok_and(|x| x != 0)
    }

    /// The distinct leaves the guard reads.
    pub fn leaves(&self) -> LeafSet<'a> {
        LeafSet::new().with(self)
    }
}

/// Whether `g` is true under assignment `bits` of the leaves in `set`
/// (bit `k` is the value of `set`'s `k`-th leaf; others read 0).
pub fn holds_under(g: &Guard<'_>, set: &LeafSet<'_>, bits: u32) -> bool {
    g.holds(|leaf| set.index(leaf).is_some_and(|k| (bits >> k) & 1 == 1))
}

/// Whether assignment `bits` of `set` can happen: no two state tests of
/// one machine naming different states are both 1.
pub fn consistent(set: &LeafSet<'_>, bits: u32) -> bool {
    let on = |k: usize| (bits >> k) & 1 == 1;
    set.as_slice().iter().enumerate().all(|(i, a)| {
        let Leaf::State { machine, .. } = *a else {
            return true;
        };
        !on(i)
            || set.as_slice().iter().enumerate().skip(i + 1).all(|(j, b)| {
                !(on(j) && matches!(*b, Leaf::State { machine: m, .. } if m == machine))
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pin_eq(label: &str, v: i32) -> [Node<'_>; 3] {
        [
            Node::Pin(label),
            Node::Const(v),
            Node::Cmp {
                op: CmpOp::Eq,
                a: 0,
                b: 1,
            },
        ]
    }

    #[test]
    fn guard_eval_compares_pins_as_zero_or_one() {
        let nodes = pin_eq("BUTTON", 1);
        let g = Guard { nodes: &nodes };
        assert_eq!(g.eval(|_| true), Ok(1));
        assert_eq!(g.eval(|_| false), Ok(0));
    }

    #[test]
    fn guard_eval_and_or_not_use_nonzero_as_true() {
        let nodes = [
            Node::Const(7),
            Node::Const(0),
            Node::And(0, 1),
            Node::Or(0, 1),
            Node::Not(1),
            Node::Not(0),
        ];
        let at = |n: usize| Guard { nodes: &nodes[..n] }.eval(|_| false);
        assert_eq!(at(3), Ok(0));
        assert_eq!(at(4), Ok(1));
        assert_eq!(at(5), Ok(1));
        assert_eq!(at(6), Ok(0));
    }

    #[test]
    fn guard_eval_covers_every_comparison() {
        let cases = [
            (CmpOp::Eq, [false, true, false]),
            (CmpOp::Ne, [true, false, true]),
            (CmpOp::Lt, [true, false, false]),
            (CmpOp::Le, [true, true, false]),
            (CmpOp::Gt, [false, false, true]),
            (CmpOp::Ge, [false, true, true]),
        ];
        for (op, want) in cases {
            for (k, b) in [2, 1, 0].into_iter().enumerate() {
                let nodes = [Node::Const(1), Node::Const(b), Node::Cmp { op, a: 0, b: 1 }];
                let g = Guard { nodes: &nodes };
                assert_eq!(g.holds(|_| false), want[k], "{op:?} 1 vs {b}");
            }
            assert_eq!(CmpOp::parse(op.name()), Some(op));
        }
        assert_eq!(CmpOp::parse("xx"), None);
        assert!(CmpOp::Lt.apply(i32::MIN, i32::MAX));
    }

    #[test]
    fn guard_shape_reports_depth_and_malformed_arenas() {
        let nodes = pin_eq("A", 1);
        assert_eq!(Guard { nodes: &nodes }.shape(), Ok(2));
        assert_eq!(Guard { nodes: &[] }.shape(), Err(GuardError::Empty));
        let forward = [Node::Not(1), Node::Const(0)];
        assert_eq!(
            Guard { nodes: &forward }.shape(),
            Err(GuardError::BadChild { node: 0 })
        );
        let selfref = [Node::Not(0)];
        assert_eq!(
            Guard { nodes: &selfref }.eval(|_| false),
            Err(GuardError::BadChild { node: 0 })
        );
        let deep = [
            Node::Const(0),
            Node::Not(0),
            Node::Not(1),
            Node::Not(2),
            Node::Not(3),
        ];
        assert_eq!(
            Guard { nodes: &deep }.shape(),
            Err(GuardError::TooDeep { depth: 5 })
        );
        assert_eq!(Guard { nodes: &deep[..4] }.shape(), Ok(4));
        // Too deep is a policy; evaluation still works.
        assert_eq!(Guard { nodes: &deep }.eval(|_| false), Ok(0));
        let big = [Node::Const(0); MAX_GUARD_NODES + 1];
        assert_eq!(
            Guard { nodes: &big }.shape(),
            Err(GuardError::TooLarge { nodes: 16 })
        );
        assert!(!Guard { nodes: &big }.holds(|_| true));
    }

    #[test]
    fn leaf_set_keeps_distinct_leaves_in_first_seen_order() {
        let a = [
            Node::Pin("A"),
            Node::Pin("B"),
            Node::And(0, 1),
            Node::Pin("A"),
            Node::Or(2, 3),
        ];
        let b = [Node::Pin("C"), Node::Pin("B"), Node::Or(0, 1)];
        let set = Guard { nodes: &a }.leaves().with(&Guard { nodes: &b });
        assert_eq!(
            set.as_slice(),
            &[Leaf::Pin("A"), Leaf::Pin("B"), Leaf::Pin("C")]
        );
        assert_eq!(set.len(), 3);
        assert!(!set.is_empty());
        assert!(holds_under(&Guard { nodes: &b }, &set, 0b100));
        assert!(!holds_under(&Guard { nodes: &b }, &set, 0b001));
    }

    #[test]
    fn state_leaves_read_through_the_reader_and_exclude_each_other() {
        let nodes = [
            Node::State {
                machine: "Mode",
                state: "On",
            },
            Node::State {
                machine: "Mode",
                state: "Off",
            },
            Node::State {
                machine: "Door",
                state: "Open",
            },
            Node::Or(0, 1),
            Node::And(3, 2),
        ];
        let g = Guard { nodes: &nodes };
        let on = |l: Leaf<'_>| {
            matches!(
                l,
                Leaf::State {
                    state: "On" | "Open",
                    ..
                }
            )
        };
        assert_eq!(g.eval(on), Ok(1));
        assert_eq!(g.eval(|_| false), Ok(0));
        assert_eq!(g.shape(), Ok(3));
        let set = g.leaves();
        assert_eq!(set.len(), 3);
        assert_eq!(set.pins().count(), 0);
        // Mode.On and Mode.Off both 1 cannot happen; with Door.Open it can.
        assert!(!consistent(&set, 0b011));
        assert!(consistent(&set, 0b101));
        assert!(consistent(&set, 0b110));
        assert!(consistent(&set, 0b000));
    }
}
