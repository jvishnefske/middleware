//! [`LogLine`]: a fixed-capacity line builder for [`log`](crate::log).
//!
//! Payloads have no heap, so a formatted log line is assembled in a
//! [`MAX_LOG`]-byte stack buffer. Text beyond the capacity is dropped (at a
//! UTF-8 character boundary, so the line stays valid UTF-8). `push_str`,
//! `push_bytes_lossy` and `push_u32` avoid pulling `core::fmt` into the
//! module; `core::fmt::Write` is also implemented for `write!` when size does
//! not matter.
//!
//! Nothing here can panic, and [`LogLine::log`] hands the bytes to the
//! host without re-validating them (they are UTF-8 by construction; the
//! host checks again): a payload that logs only through `LogLine` links
//! none of core's panic formatting (`panic_fmt`, `Display for usize`) or
//! `str::from_utf8`, about 2 KB of code and data.

use crate::MAX_LOG;

/// A log line of at most [`MAX_LOG`] bytes.
pub struct LogLine {
    buf: [u8; MAX_LOG],
    len: usize,
}

impl Default for LogLine {
    fn default() -> Self {
        Self::new()
    }
}

impl LogLine {
    /// An empty line.
    pub const fn new() -> Self {
        LogLine {
            buf: [0; MAX_LOG],
            len: 0,
        }
    }

    /// Appends `bytes`, which the caller has cut to fit and to whole UTF-8
    /// characters.
    fn push_fitted(&mut self, bytes: &[u8]) -> &mut Self {
        let dst = self
            .len
            .checked_add(bytes.len())
            .and_then(|end| self.buf.get_mut(self.len..end).map(|d| (d, end)));
        if let Some((dst, end)) = dst {
            dst.copy_from_slice(bytes);
            self.len = end;
        }
        self
    }

    /// Appends as much of `s` as fits, cutting at a character boundary.
    pub fn push_str(&mut self, s: &str) -> &mut Self {
        let room = MAX_LOG.saturating_sub(self.len);
        let mut take = s.len().min(room);
        // At most three steps back: a character is at most four bytes.
        while take > 0 && !s.is_char_boundary(take) {
            take -= 1;
        }
        match s.as_bytes().get(..take) {
            Some(bytes) => self.push_fitted(bytes),
            None => self,
        }
    }

    /// Appends `bytes` if they are UTF-8, otherwise `<N bytes>`.
    pub fn push_bytes_lossy(&mut self, bytes: &[u8]) -> &mut Self {
        match core::str::from_utf8(bytes) {
            Ok(s) => self.push_str(s),
            Err(_) => self
                .push_str("<")
                .push_u32(bytes.len() as u32)
                .push_str(" bytes>"),
        }
    }

    /// Appends `n` in decimal, or nothing if the digits do not all fit.
    pub fn push_u32(&mut self, n: u32) -> &mut Self {
        let mut digits = [0u8; 10];
        let mut start = digits.len();
        let mut v = n;
        // At most ten digits; `digits.len()` steps bound the loop.
        for d in digits.iter_mut().rev() {
            *d = b'0' + (v % 10) as u8;
            start -= 1;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        match digits.get(start..) {
            // ASCII digits: whole characters.
            Some(ascii) if ascii.len() <= MAX_LOG.saturating_sub(self.len) => {
                self.push_fitted(ascii)
            }
            _ => self,
        }
    }

    /// The line so far.
    pub fn as_str(&self) -> &str {
        // Only whole characters are ever appended.
        core::str::from_utf8(self.bytes()).unwrap_or("")
    }

    /// The bytes of the line so far: UTF-8, only whole characters.
    fn bytes(&self) -> &[u8] {
        self.buf.get(..self.len).unwrap_or(&[])
    }

    /// Sends the line with [`log`](crate::log).
    pub fn log(&self) {
        crate::log_utf8(self.bytes());
    }
}

impl core::fmt::Write for LogLine {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.push_str(s);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_lines() {
        let mut l = LogLine::new();
        l.push_str("n=")
            .push_u32(0)
            .push_str(",")
            .push_u32(4_294_967_295);
        assert_eq!(l.as_str(), "n=0,4294967295");
        let mut b = LogLine::new();
        b.push_bytes_lossy(b"hi").push_bytes_lossy(&[0xff, 0xfe]);
        assert_eq!(b.as_str(), "hi<2 bytes>");
    }

    #[test]
    fn truncates_at_char_boundary() {
        let mut l = LogLine::new();
        for _ in 0..MAX_LOG - 1 {
            l.push_str("a");
        }
        l.push_str("é"); // two bytes, only one free
        assert_eq!(l.as_str().len(), MAX_LOG - 1);
        l.push_str("bc");
        assert_eq!(l.as_str().len(), MAX_LOG);
        assert!(l.as_str().ends_with('b'));
    }

    #[test]
    fn a_number_that_does_not_fit_is_dropped_whole() {
        let mut l = LogLine::new();
        for _ in 0..MAX_LOG - 3 {
            l.push_str("a");
        }
        l.push_u32(1234);
        assert_eq!(l.as_str().len(), MAX_LOG - 3, "never half a number");
        l.push_u32(567);
        assert!(l.as_str().ends_with("a567"));
        l.push_u32(8);
        assert_eq!(l.as_str().len(), MAX_LOG);
    }
}
