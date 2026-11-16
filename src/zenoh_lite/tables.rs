//! The client's network-layer state as pure, fixed-capacity tables: what
//! it declared (subscribers and queryables, re-sent after every open and
//! offered to the router's interests) and the key expression ids the
//! router declared to it, plus key-expression resolution and a bounded,
//! conservative intersection test. No heap: `heapless` vectors and
//! strings; every operation returns the new table or an error.
//!
//! # Declarations
//!
//! A subscriber on [`KeyFilter::Exact`]`(k)` is declared as the key
//! expression `k`, on [`KeyFilter::Prefix`]`(k)` as `k/**` (which Zenoh
//! matches against `k` itself too), as the host backend does. A queryable
//! on `k` is two declarations, `k` and the per-node `@mw/node/<zid16>/<k>`
//! ([`node_key_buf`]), so `Target::Node` requests reach it. Entity ids
//! count from 1 per table.
//!
//! # Key expressions from the router
//!
//! A router may declare a numeric id for a key expression
//! (`DeclareKeyExpr`) and then send `scope = id` with a suffix. [`resolve`]
//! joins a scope's expression and the suffix; a scope it does not know
//! (or one the router says is ours: this client declares none) is `None`
//! and the message is dropped.
//!
//! # Intersection
//!
//! [`intersects`] decides whether two key expressions can match a common
//! key, chunk by chunk with `*` (one chunk) and `**` (any number), verbatim
//! `@` chunks only matching themselves, and any chunk with `$*` treated as
//! `*`. It answers `true` (over-approximates) past [`MAX_CHUNKS`] chunks.
//! It is only used to pick the declarations answering an interest, where
//! sending one too many is harmless.

use crate::key::{Filter, Key, KeyBuf, KeyFilter, MAX_KEY_LEN};
use crate::transport::SenderId;
use crate::zenoh_wire::{NODE_KEY_MAX, node_key_buf};

use super::codec::WireExpr;
use super::codec::network::{Declare, DeclareBody, NetExts, interest};

/// Longest key expression the client handles (a per-node key).
pub const EXPR_MAX: usize = NODE_KEY_MAX;
/// A key expression in a fixed-capacity string.
pub type Expr = heapless::String<EXPR_MAX>;
/// Chunks beyond which [`intersects`] over-approximates.
pub const MAX_CHUNKS: usize = 32;

/// What a declaration is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclKind {
    /// A subscriber: `prefix` is true for a [`Filter::Prefix`].
    Subscriber {
        /// Declared as `key/**`.
        prefix: bool,
    },
    /// A queryable on the key itself.
    Queryable,
    /// The per-node queryable of the key.
    NodeQueryable,
}

/// One declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decl {
    /// Entity id on the wire.
    pub id: u32,
    /// Kind.
    pub kind: DeclKind,
    /// The application key.
    pub key: KeyBuf<MAX_KEY_LEN>,
    /// The key expression declared.
    pub expr: Expr,
}

impl Decl {
    /// The Declare message for this entry (`interest_id` when answering an
    /// interest).
    pub fn declare(&self, interest_id: Option<u32>) -> Declare<'_> {
        let wire_expr = WireExpr::plain(self.expr.as_str());
        let body = match self.kind {
            DeclKind::Subscriber { .. } => DeclareBody::Subscriber {
                id: self.id,
                wire_expr,
            },
            DeclKind::Queryable | DeclKind::NodeQueryable => DeclareBody::Queryable {
                id: self.id,
                wire_expr,
                complete: false,
                distance: 0,
            },
        };
        Declare {
            interest_id,
            exts: NetExts::DEFAULT,
            body,
        }
    }

    /// The subscriber filter of a subscriber entry.
    pub fn filter(&self) -> Option<KeyFilter<'_>> {
        use crate::key::AsKey;
        match self.kind {
            DeclKind::Subscriber { prefix: false } => Some(Filter::Exact(self.key.as_key())),
            DeclKind::Subscriber { prefix: true } => Some(Filter::Prefix(self.key.as_key())),
            _ => None,
        }
    }
}

/// Why a table refused a change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableError {
    /// No room left.
    Full,
    /// The expression does not fit [`EXPR_MAX`].
    TooLong,
}

/// The client's declarations, at most `D`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declarations<const D: usize> {
    entries: heapless::Vec<Decl, D>,
    next_id: u32,
}

impl<const D: usize> Default for Declarations<D> {
    fn default() -> Self {
        Self::new()
    }
}

/// The key expression a subscriber filter is declared as.
pub fn filter_expr(filter: KeyFilter<'_>) -> Result<Expr, TableError> {
    let mut e = Expr::new();
    e.push_str(filter.key().as_str())
        .map_err(|_| TableError::TooLong)?;
    if matches!(filter, Filter::Prefix(_)) {
        e.push_str("/**").map_err(|_| TableError::TooLong)?;
    }
    Ok(e)
}

impl<const D: usize> Declarations<D> {
    /// No declarations.
    pub const fn new() -> Self {
        Declarations {
            entries: heapless::Vec::new(),
            next_id: 1,
        }
    }

    /// The entries in declaration order.
    pub fn entries(&self) -> &[Decl] {
        &self.entries
    }

    fn push(mut self, kind: DeclKind, key: Key<'_>, expr: Expr) -> Result<Self, TableError> {
        let id = self.next_id;
        let key = KeyBuf::from_key(key).map_err(|_| TableError::TooLong)?;
        self.entries
            .push(Decl {
                id,
                kind,
                key,
                expr,
            })
            .map_err(|_| TableError::Full)?;
        self.next_id = id.wrapping_add(1).max(1);
        Ok(self)
    }

    /// The table with a subscriber for `filter` added, and the new
    /// entry's index; `None` (and an unchanged table) if an equal one
    /// exists. `self` is unchanged on error.
    pub fn add_subscriber(
        &self,
        filter: KeyFilter<'_>,
    ) -> Result<(Self, Option<usize>), TableError> {
        let expr = filter_expr(filter)?;
        if self
            .entries
            .iter()
            .any(|d| matches!(d.kind, DeclKind::Subscriber { .. }) && d.expr == expr)
        {
            return Ok((self.clone(), None));
        }
        let prefix = matches!(filter, Filter::Prefix(_));
        let index = self.entries.len();
        let t = self
            .clone()
            .push(DeclKind::Subscriber { prefix }, filter.key(), expr)?;
        Ok((t, Some(index)))
    }

    /// The table without the subscriber for `filter`, and its entity id;
    /// `None` (and an unchanged table) if there is none.
    pub fn remove_subscriber(
        &self,
        filter: KeyFilter<'_>,
    ) -> Result<(Self, Option<u32>), TableError> {
        let expr = filter_expr(filter)?;
        let Some(pos) = self
            .entries
            .iter()
            .position(|d| matches!(d.kind, DeclKind::Subscriber { .. }) && d.expr == expr)
        else {
            return Ok((self.clone(), None));
        };
        let mut next = self.clone();
        let id = next.entries.get(pos).map(|d| d.id);
        next.entries.remove(pos);
        Ok((next, id))
    }

    /// The table with the two queryables of `key` added (the key and its
    /// per-node key for `sender`), and the index of the first; `None` if
    /// they exist. `self` is unchanged on error.
    pub fn add_queryable(
        &self,
        key: Key<'_>,
        sender: SenderId,
    ) -> Result<(Self, Option<usize>), TableError> {
        if self
            .entries
            .iter()
            .any(|d| d.kind == DeclKind::Queryable && d.key == key)
        {
            return Ok((self.clone(), None));
        }
        if self.entries.len() + 2 > D {
            return Err(TableError::Full);
        }
        let mut plain = Expr::new();
        plain
            .push_str(key.as_str())
            .map_err(|_| TableError::TooLong)?;
        let node = node_key_buf(sender, key).ok_or(TableError::TooLong)?;
        let mut node_expr = Expr::new();
        node_expr
            .push_str(node.as_str())
            .map_err(|_| TableError::TooLong)?;
        let index = self.entries.len();
        let t = self.clone().push(DeclKind::Queryable, key, plain)?.push(
            DeclKind::NodeQueryable,
            key,
            node_expr,
        )?;
        Ok((t, Some(index)))
    }

    /// True if any subscriber's filter selects `key`.
    pub fn wants(&self, key: Key<'_>) -> bool {
        self.entries
            .iter()
            .filter_map(Decl::filter)
            .any(|f| f.matches(key))
    }

    /// The queryable entry whose declared expression is exactly `expr`.
    pub fn queryable_for(&self, expr: &str) -> Option<&Decl> {
        self.entries.iter().find(|d| {
            matches!(d.kind, DeclKind::Queryable | DeclKind::NodeQueryable) && d.expr == expr
        })
    }

    /// The entries answering an interest with `options` restricted to
    /// `expr` (`None`: all key expressions).
    pub fn answering<'s>(
        &'s self,
        options: u8,
        expr: Option<&'s str>,
    ) -> impl Iterator<Item = &'s Decl> + 's {
        self.entries.iter().filter(move |d| {
            let kind = match d.kind {
                DeclKind::Subscriber { .. } => options & interest::SUBSCRIBERS != 0,
                DeclKind::Queryable | DeclKind::NodeQueryable => {
                    options & interest::QUERYABLES != 0
                }
            };
            kind && expr.is_none_or(|e| intersects(e, d.expr.as_str()))
        })
    }
}

/// Key expression ids the router declared, at most `M`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mappings<const M: usize> {
    entries: heapless::Vec<(u16, Expr), M>,
}

impl<const M: usize> Default for Mappings<M> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const M: usize> Mappings<M> {
    /// None.
    pub const fn new() -> Self {
        Mappings {
            entries: heapless::Vec::new(),
        }
    }

    /// The expression of `id`.
    pub fn get(&self, id: u16) -> Option<&str> {
        self.entries
            .iter()
            .find(|(i, _)| *i == id)
            .map(|(_, e)| e.as_str())
    }

    /// The table with `id` recorded for the expression `wire_expr`
    /// resolves to (replacing an earlier one with the same id). `self` is
    /// unchanged on error.
    pub fn declare(&self, id: u16, wire_expr: &WireExpr<'_>) -> Result<Self, TableError> {
        let expr = resolve(self, wire_expr).ok_or(TableError::TooLong)?;
        let mut next = self.clone();
        next.entries.retain(|(i, _)| *i != id);
        next.entries
            .push((id, expr))
            .map_err(|_| TableError::Full)?;
        Ok(next)
    }

    /// The table without `id`.
    pub fn undeclare(&self, id: u16) -> Self {
        let mut next = self.clone();
        next.entries.retain(|(i, _)| *i != id);
        next
    }
}

/// The full key expression of `wire_expr` from the router: its scope's
/// expression (if any) followed by the suffix. `None` if the scope is
/// unknown or ours, or the result does not fit.
pub fn resolve<const M: usize>(mappings: &Mappings<M>, wire_expr: &WireExpr<'_>) -> Option<Expr> {
    let mut out = Expr::new();
    if wire_expr.scope != 0 {
        if !wire_expr.sender_mapping {
            return None;
        }
        out.push_str(mappings.get(wire_expr.scope)?).ok()?;
    }
    out.push_str(wire_expr.suffix).ok()?;
    Some(out)
}

fn chunks<'a>(expr: &'a str, out: &mut [&'a str; MAX_CHUNKS]) -> Option<usize> {
    let mut n = 0;
    for c in expr.split('/') {
        *out.get_mut(n)? = c;
        n += 1;
    }
    Some(n)
}

fn is_wild(c: &str) -> bool {
    c == "*" || c.contains("$*")
}

fn chunk_intersects(a: &str, b: &str) -> bool {
    if a.starts_with('@') || b.starts_with('@') {
        return a == b;
    }
    a == b || is_wild(a) || is_wild(b)
}

/// True if some key may match both `a` and `b` (see the module docs; an
/// over-approximation past [`MAX_CHUNKS`] chunks).
pub fn intersects(a: &str, b: &str) -> bool {
    let mut ca = [""; MAX_CHUNKS];
    let mut cb = [""; MAX_CHUNKS];
    let (Some(na), Some(nb)) = (chunks(a, &mut ca), chunks(b, &mut cb)) else {
        return true;
    };
    // reach[i] bit j: a[..i] and b[..j] can match a common key prefix.
    let mut reach = [0u64; MAX_CHUNKS + 1];
    reach[0] = 1;
    for i in 0..=na {
        for j in 0..=nb {
            if reach[i] & (1 << j) == 0 {
                continue;
            }
            let ai = ca.get(i).filter(|_| i < na).copied();
            let bj = cb.get(j).filter(|_| j < nb).copied();
            if ai == Some("**") {
                // `**` matches nothing more of b.
                if let Some(next) = reach.get_mut(i + 1) {
                    *next |= 1 << j;
                }
                // `**` absorbs b[j] (not a verbatim chunk).
                if bj.is_some_and(|y| !y.starts_with('@')) {
                    reach[i] |= 1 << (j + 1);
                }
            }
            if bj == Some("**") {
                reach[i] |= 1 << (j + 1);
                if ai.is_some_and(|x| !x.starts_with('@'))
                    && let Some(next) = reach.get_mut(i + 1)
                {
                    *next |= 1 << j;
                }
            }
            if let (Some(x), Some(y)) = (ai, bj)
                && x != "**"
                && y != "**"
                && chunk_intersects(x, y)
                && let Some(next) = reach.get_mut(i + 1)
            {
                *next |= 1 << (j + 1);
            }
        }
    }
    reach[na] & (1 << nb) != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: SenderId = SenderId([1, 2, 3, 4, 5, 6, 7, 8]);

    #[test]
    fn zn2_intersection_handles_wildcards_and_verbatim_chunks() {
        for (a, b) in [
            ("a/b", "a/b"),
            ("a/*", "a/b"),
            ("a/**", "a"),
            ("a/**", "a/b/c"),
            ("**", "x/y"),
            ("a/**/c", "a/b/b/c"),
            ("a/$*b", "a/xb"),
            ("@mw/node/x/**", "@mw/node/x/k"),
            ("a/**", "a/*/c"),
        ] {
            assert!(intersects(a, b), "{a} {b}");
            assert!(intersects(b, a), "{b} {a}");
        }
        for (a, b) in [
            ("a/b", "a/c"),
            ("a/*", "a/b/c"),
            ("**", "@mw/node/x"),
            ("*/node", "@mw/node"),
            ("a/**/c", "a/b/d"),
        ] {
            assert!(!intersects(a, b), "{a} {b}");
            assert!(!intersects(b, a), "{b} {a}");
        }
        // Too many chunks: over-approximated.
        let long = "a/".repeat(40) + "z";
        assert!(intersects(&long, "q"));
    }

    #[test]
    fn zn2_declarations_dedupe_and_answer_interests_by_kind() {
        let t: Declarations<4> = Declarations::new();
        let k = Key::from_static("imu/accel");
        let (t, i) = t.add_subscriber(KeyFilter::Prefix(k)).expect("room");
        assert_eq!(i, Some(0));
        let (t, i) = t.add_subscriber(KeyFilter::Prefix(k)).expect("room");
        assert_eq!(i, None);
        let (t, i) = t
            .add_queryable(Key::from_static("sys/info"), ID)
            .expect("room");
        assert_eq!(i, Some(1));
        assert_eq!(t.entries().len(), 3);
        assert_eq!(t.entries()[0].expr.as_str(), "imu/accel/**");
        assert_eq!(
            t.entries()[2].expr.as_str(),
            "@mw/node/0102030405060708/sys/info"
        );
        assert!(t.wants(Key::from_static("imu/accel/x")));
        assert!(!t.wants(Key::from_static("imu/accelx")));
        assert!(t.queryable_for("sys/info").is_some());
        assert!(
            t.queryable_for("@mw/node/0102030405060708/sys/info")
                .is_some()
        );
        let subs = t.answering(interest::SUBSCRIBERS, None).count();
        let qbls = t.answering(interest::QUERYABLES, None).count();
        let narrow = t.answering(interest::QUERYABLES, Some("sys/*")).count();
        assert_eq!((subs, qbls, narrow), (1, 2, 1));
        // Two more queryables do not fit four entries.
        assert_eq!(
            t.add_queryable(Key::from_static("b"), ID).err(),
            Some(TableError::Full)
        );
        // Ids are distinct.
        let ids: heapless::Vec<u32, 4> = t.entries().iter().map(|d| d.id).collect();
        assert_eq!(&ids[..], &[1, 2, 3]);
    }

    #[test]
    fn zn2_router_key_expression_ids_resolve() {
        let m: Mappings<2> = Mappings::new();
        let m = m.declare(7, &WireExpr::plain("imu")).expect("room");
        let scoped = WireExpr {
            scope: 7,
            suffix: "/accel",
            sender_mapping: true,
        };
        assert_eq!(resolve(&m, &scoped).as_deref(), Some("imu/accel"));
        // Our own mapping (we declare none) and unknown ids are refused.
        let ours = WireExpr {
            sender_mapping: false,
            ..scoped
        };
        assert_eq!(resolve(&m, &ours), None);
        let unknown = WireExpr { scope: 9, ..scoped };
        assert_eq!(resolve(&m, &unknown), None);
        let m = m.undeclare(7);
        assert_eq!(resolve(&m, &scoped), None);
    }
}
