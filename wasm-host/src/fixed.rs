//! Fixed-capacity storage for what a payload's host state holds: keys,
//! channel names, log lines. Everything here is inline in
//! [`State`](crate::link::State); none of it is allocated.

use middleware::wasm_abi::channels::{MAX_CHANNELS, MAX_NAME};
use middleware::wasm_abi::periph::MAX_DMA;
use middleware::wasm_abi::serve::MAX_SERVES;
use middleware::wasm_abi::{MAX_LOG, SLOT_MAX};

/// A key: at most [`middleware::key::MAX_KEY_LEN`] bytes.
pub type KeyBuf = heapless::String<{ middleware::key::MAX_KEY_LEN }>;
/// A channel name: at most [`MAX_NAME`] bytes.
pub type Name = heapless::String<MAX_NAME>;
/// A log line: at most [`MAX_LOG`] bytes.
pub type Line = heapless::String<MAX_LOG>;

/// The subscribed keys; the index is the slot. Its capacity is the most any
/// host offers ([`SLOT_MAX`]); a payload's own limit is
/// [`Limits::max_subscriptions`](crate::Limits::max_subscriptions).
pub type Subscriptions = heapless::Vec<KeyBuf, SLOT_MAX>;
/// The served keys (ABI 5); the index is the slot.
pub type Serves = heapless::Vec<KeyBuf, MAX_SERVES>;
/// The host copy buffer of the DMA and UART imports.
pub type Scratch = heapless::Vec<u8, MAX_DMA>;
/// One entry per declared channel (ABI 8).
pub type PerChannel<T> = heapless::Vec<T, MAX_CHANNELS>;

/// `s`, or its longest prefix that fits `N` bytes (on a character
/// boundary). Keys and names reach here already checked against their
/// limits, so nothing is cut in practice.
pub fn text<const N: usize>(s: &str) -> heapless::String<N> {
    let mut end = s.len().min(N);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = heapless::String::new();
    // Fits: `end <= N`.
    let _ = out.push_str(s.get(..end).unwrap_or(""));
    out
}

/// The last `N` items pushed, oldest first: a ring that overwrites its
/// oldest item when full.
#[derive(Debug, Clone)]
pub struct Ring<T, const N: usize> {
    items: [T; N],
    /// Index of the oldest item.
    start: usize,
    len: usize,
}

impl<const M: usize, const N: usize> Ring<heapless::String<M>, N> {
    /// An empty ring.
    pub const fn new() -> Self {
        Ring {
            items: [const { heapless::String::new() }; N],
            start: 0,
            len: 0,
        }
    }
}

impl<const M: usize, const N: usize> Default for Ring<heapless::String<M>, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const N: usize> Ring<T, N> {
    /// Appends `item`; returns whether the oldest one was overwritten.
    pub fn push(&mut self, item: T) -> bool {
        if N == 0 {
            return true;
        }
        let full = self.len == N;
        let at = (self.start + self.len) % N;
        if let Some(slot) = self.items.get_mut(at) {
            *slot = item;
        }
        if full {
            self.start = (self.start + 1) % N;
        } else {
            self.len += 1;
        }
        full
    }

    /// The items, oldest first.
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        let (tail, head) = self.items.split_at(self.start.min(N));
        head.iter().chain(tail).take(self.len)
    }

    /// Number of items held (at most `N`).
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether nothing is held.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<T: PartialEq, const N: usize> PartialEq for Ring<T, N> {
    /// Equal when they hold the same items in the same order, wherever
    /// each ring's start happens to be.
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len && self.iter().eq(other.iter())
    }
}

impl<T: Eq, const N: usize> Eq for Ring<T, N> {}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    #[test]
    fn ring_keeps_the_newest_in_order_and_compares_by_content() {
        let mut r: Ring<heapless::String<8>, 3> = Ring::new();
        assert!(r.is_empty());
        for (i, s) in ["a", "b", "c"].iter().enumerate() {
            assert!(!r.push(text(s)), "room for item {i}");
        }
        assert_eq!(
            r.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        assert!(r.push(text("d")), "full: the oldest goes");
        assert!(r.push(text("e")));
        assert_eq!(
            r.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            ["c", "d", "e"]
        );
        assert_eq!(r.len(), 3);
        // The same contents reached another way, so with another start.
        let mut other: Ring<heapless::String<8>, 3> = Ring::new();
        for s in ["c", "d", "e"] {
            other.push(text(s));
        }
        assert_eq!(r, other);
        other.push(text("f"));
        assert_ne!(r, other);
    }

    #[test]
    fn text_cuts_at_a_character_boundary() {
        assert_eq!(text::<8>("short").as_str(), "short");
        assert_eq!(text::<4>("abcdef").as_str(), "abcd");
        // 'é' is two bytes: it does not fit the last byte.
        assert_eq!(text::<4>("abcé").as_str(), "abc");
        assert_eq!(text::<0>("x").as_str(), "");
    }
}
