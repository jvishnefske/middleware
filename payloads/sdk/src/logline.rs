//! [`LogLine`]: a fixed-capacity line builder for [`log`](crate::log).
//!
//! Payloads have no heap, so a formatted log line is assembled in a
//! [`MAX_LOG`]-byte stack buffer. Text beyond the capacity is dropped (at a
//! UTF-8 character boundary, so the line stays valid UTF-8). `push_str`,
//! `push_bytes_lossy` and `push_u32` avoid pulling `core::fmt` into the
//! module; `core::fmt::Write` is also implemented for `write!` when size does
//! not matter.

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

    /// Appends as much of `s` as fits, cutting at a character boundary.
    pub fn push_str(&mut self, s: &str) -> &mut Self {
        let room = MAX_LOG - self.len;
        let mut take = s.len().min(room);
        while take > 0 && !s.is_char_boundary(take) {
            take -= 1;
        }
        self.buf[self.len..self.len + take].copy_from_slice(&s.as_bytes()[..take]);
        self.len += take;
        self
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

    /// Appends `n` in decimal.
    pub fn push_u32(&mut self, n: u32) -> &mut Self {
        let mut digits = [0u8; 10];
        let mut i = digits.len();
        let mut v = n;
        loop {
            i -= 1;
            digits[i] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        // Digits are ASCII, so this cannot fail.
        match core::str::from_utf8(&digits[i..]) {
            Ok(s) => self.push_str(s),
            Err(_) => self,
        }
    }

    /// The line so far.
    pub fn as_str(&self) -> &str {
        // Only whole characters are ever appended.
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }

    /// Sends the line with [`log`](crate::log).
    pub fn log(&self) {
        crate::log(self.as_str());
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
}
