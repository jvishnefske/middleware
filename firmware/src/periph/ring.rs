//! The fixed receive ring of the secondary UART.
//!
//! [`RxRing`] holds at most `N` bytes ([`UART_RX_BUFFER`] in the firmware).
//! [`RxRing::push`] stores what fits and drops the rest, counting dropped
//! bytes; [`RxRing::pop_into`] moves the oldest bytes out in order. Both are
//! O(bytes moved) and never allocate.
//!
//! The ring is mutated in place: it is a kilobyte that lives in the UART
//! driver for the life of the firmware, and copying it per byte received
//! would be wasteful.
//!
//! [`UART_RX_BUFFER`]: middleware::wasm_abi::periph::UART_RX_BUFFER

/// Fixed-capacity byte FIFO with drop-and-count overflow.
#[derive(Debug, Clone)]
pub struct RxRing<const N: usize> {
    buf: [u8; N],
    head: usize,
    len: usize,
    dropped: u32,
}

impl<const N: usize> Default for RxRing<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> RxRing<N> {
    /// An empty ring.
    pub const fn new() -> Self {
        RxRing { buf: [0; N], head: 0, len: 0, dropped: 0 }
    }

    /// Bytes buffered.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether nothing is buffered.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Bytes dropped because the ring was full (saturating).
    pub const fn dropped(&self) -> u32 {
        self.dropped
    }

    /// Appends what fits of `data`; returns how many bytes were stored.
    pub fn push(&mut self, data: &[u8]) -> usize {
        let room = N - self.len;
        let stored = room.min(data.len());
        for &b in data.iter().take(stored) {
            let tail = if N == 0 { 0 } else { (self.head + self.len) % N };
            if let Some(slot) = self.buf.get_mut(tail) {
                *slot = b;
                self.len += 1;
            }
        }
        let lost = u32::try_from(data.len() - stored).unwrap_or(u32::MAX);
        self.dropped = self.dropped.saturating_add(lost);
        stored
    }

    /// Moves up to `out.len()` of the oldest bytes into `out`; returns the
    /// count.
    pub fn pop_into(&mut self, out: &mut [u8]) -> usize {
        let n = out.len().min(self.len);
        for o in out.iter_mut().take(n) {
            if let Some(&b) = self.buf.get(self.head) {
                *o = b;
            }
            self.head = if N == 0 { 0 } else { (self.head + 1) % N };
            self.len -= 1;
        }
        n
    }

    /// Discards everything buffered and the drop count.
    pub fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
        self.dropped = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::wasm_abi::periph::UART_RX_BUFFER;

    #[test]
    fn d7_ring_keeps_order_across_wrap() {
        let mut r: RxRing<4> = RxRing::new();
        assert_eq!(r.push(b"abc"), 3);
        let mut out = [0u8; 2];
        assert_eq!(r.pop_into(&mut out), 2);
        assert_eq!(&out, b"ab");
        assert_eq!(r.push(b"def"), 3);
        let mut out = [0u8; 8];
        assert_eq!(r.pop_into(&mut out), 4);
        assert_eq!(&out[..4], b"cdef");
        assert!(r.is_empty());
        assert_eq!(r.pop_into(&mut out), 0);
    }

    #[test]
    fn d7_ring_drops_and_counts_overflow() {
        let mut r: RxRing<UART_RX_BUFFER> = RxRing::new();
        let chunk = [0x55u8; 300];
        let mut stored = 0;
        for _ in 0..4 {
            stored += r.push(&chunk);
        }
        assert_eq!(stored, UART_RX_BUFFER);
        assert_eq!(r.len(), UART_RX_BUFFER);
        assert_eq!(r.dropped(), 1200 - UART_RX_BUFFER as u32);
        r.clear();
        assert_eq!((r.len(), r.dropped()), (0, 0));
        let mut z: RxRing<0> = RxRing::new();
        assert_eq!(z.push(b"x"), 0);
        assert_eq!(z.dropped(), 1);
    }
}
