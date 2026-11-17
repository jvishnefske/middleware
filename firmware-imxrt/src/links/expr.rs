//! The key expressions a serial host narrows its link with
//! (`sys/link/sub`): Zenoh's chunk wildcards over middleware keys.
//!
//! An expression is 1..=[`MAX_KEY_LEN`] bytes of `/`-separated chunks;
//! each chunk is `*` (exactly one key segment), `**` (any number of key
//! segments, none included) or a literal segment under the [`Key`] rules
//! (ASCII letters, digits, `_`, `-`, `.`). So `sensor/**` selects
//! `sensor`, `sensor/imu` and `sensor/imu/raw`; `*/status` selects
//! `payload/status` but not `status` or `a/b/status`. Keys themselves never
//! hold wildcards, so matching is one expression against one key, not
//! Zenoh's expression intersection (`middleware::zenoh_lite::tables`,
//! not built in this crate). Zenoh's `$*` and verbatim `@` chunks are not
//! accepted: no middleware key could match them.
//!
//! Matching is the classic wildcard algorithm over chunks (`*` as one
//! chunk, `**` as any run, the last `**` the only backtrack point), on
//! byte offsets: no allocation, no recursion, at most
//! chunks(expr) × chunks(key) steps.

use heapless::String;
use middleware::Key;
use middleware::key::MAX_KEY_LEN;

/// Why a string is not a key expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExprError {
    /// Empty, or longer than [`MAX_KEY_LEN`] bytes.
    Length,
    /// An empty chunk (a leading, trailing or doubled `/`), or a chunk that
    /// is neither `*`, `**` nor a valid key segment.
    Chunk,
}

/// A validated key expression, owned (fixed capacity).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expr(String<MAX_KEY_LEN>);

impl Expr {
    /// Validates `s` (see the module docs) and copies it in.
    pub fn new(s: &str) -> Result<Self, ExprError> {
        if s.is_empty() || s.len() > MAX_KEY_LEN {
            return Err(ExprError::Length);
        }
        let valid = s
            .split('/')
            .all(|c| c == "*" || c == "**" || (!c.is_empty() && Key::new(c).is_ok()));
        if !valid {
            return Err(ExprError::Chunk);
        }
        let mut out = String::new();
        out.push_str(s).map_err(|_| ExprError::Length)?;
        Ok(Expr(out))
    }

    /// Parses a request body: the expression as UTF-8, nothing else.
    pub fn from_body(body: &[u8]) -> Result<Self, ExprError> {
        let s = core::str::from_utf8(body).map_err(|_| ExprError::Chunk)?;
        Self::new(s)
    }

    /// The expression text.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Whether `key` is selected by this expression.
    pub fn matches(&self, key: Key<'_>) -> bool {
        matches(self.as_str(), key.as_str())
    }
}

/// The chunk of `s` starting at byte `at` and the offset of the next one
/// (`s.len() + 1` after the last); `None` once `s` is exhausted.
fn chunk(s: &str, at: usize) -> Option<(&str, usize)> {
    let rest = s.get(at..)?;
    match rest.find('/') {
        Some(i) => Some((rest.get(..i)?, at + i + 1)),
        None => Some((rest, s.len() + 1)),
    }
}

/// Whether the expression `expr` (validated) selects the key `key`.
fn matches(expr: &str, key: &str) -> bool {
    let (mut e, mut k) = (0usize, 0usize);
    // After the last `**`: the expression offset following it and the key
    // offset it has absorbed up to.
    let mut star: Option<(usize, usize)> = None;
    loop {
        let kc = chunk(key, k);
        match chunk(expr, e) {
            Some(("**", next)) => {
                star = Some((next, k));
                e = next;
                continue;
            }
            Some((c, next)) => {
                if let Some((kch, knext)) = kc
                    && (c == "*" || c == kch)
                {
                    e = next;
                    k = knext;
                    continue;
                }
            }
            None if kc.is_none() => return true,
            None => {}
        }
        // Mismatch: let the last `**` absorb one more key chunk.
        let Some((after, absorbed)) = star else {
            return false;
        };
        let Some((_, knext)) = chunk(key, absorbed) else {
            return false;
        };
        star = Some((after, knext));
        e = after;
        k = knext;
    }
}
