//! [`ZenohLite`](super::ZenohLite) over a message link (a WebSocket):
//! [`Unframed`] turns a link that carries one Zenoh batch per message into
//! the byte stream the client reads and writes.
//!
//! On TCP every batch is preceded by its 2-byte little-endian length
//! ([`LEN_PREFIX`]); a link that keeps message boundaries (Zenoh's `ws`
//! link) carries the batch alone. The client is unchanged: on read the
//! adapter puts the length in front of each message, on write it takes the
//! length off and sends the batch as one message. `no_std`, no heap; the
//! two buffers are `N` bytes each.
//!
//! # Cancel safety
//!
//! [`MessageLink::recv`] must be cancel safe and [`MessageLink::send`]
//! must send all of a message or none of it when dropped. Then
//! [`Unframed`]'s `read`, `write` and `flush` are cancel safe: state only
//! changes after the link call returns, and bytes are accepted by `write`
//! only once no complete batch is waiting. A complete batch goes out on
//! `flush` (the client flushes after every batch) or before the next
//! batch's first bytes are taken.

use embedded_io_async::{Read, Write};

/// The error type a [`MessageLink`] names (`embedded_io_async`'s), so a
/// link can be written without that crate.
pub use embedded_io_async::ErrorType;

use super::codec::transport::LEN_PREFIX;

/// What one [`MessageLink::recv`] got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recv {
    /// A message of this many bytes, at the front of the buffer.
    Message(usize),
    /// A message larger than the buffer, dropped by the link.
    Oversized,
    /// The link is closed; nothing more will arrive.
    Closed,
}

/// A link that keeps message boundaries.
pub trait MessageLink: ErrorType {
    /// Waits for the next message and copies it into `buf`. Must be cancel
    /// safe.
    async fn recv(&mut self, buf: &mut [u8]) -> Result<Recv, Self::Error>;
    /// Sends `message` as one message: all of it, or (when dropped) none.
    async fn send(&mut self, message: &[u8]) -> Result<(), Self::Error>;
}

/// The byte stream of [`ZenohLite`](super::ZenohLite) over a
/// [`MessageLink`]. `N` must hold the largest batch plus its length:
/// at least the client's `RX + 2` and `TX`, and at most `65535 + 2`.
pub struct Unframed<L, const N: usize> {
    link: L,
    rd: [u8; N],
    rpos: usize,
    rlen: usize,
    wr: [u8; N],
    wlen: usize,
    oversized: u32,
}

impl<L: MessageLink, const N: usize> Unframed<L, N> {
    const SIZE_OK: () = assert!(
        N >= 64 && N <= u16::MAX as usize + LEN_PREFIX,
        "Unframed buffer: N in 64..=65537"
    );

    /// The stream over `link`, with nothing buffered.
    pub const fn new(link: L) -> Self {
        let () = Self::SIZE_OK;
        Unframed {
            link,
            rd: [0; N],
            rpos: 0,
            rlen: 0,
            wr: [0; N],
            wlen: 0,
            oversized: 0,
        }
    }

    /// The link.
    pub fn link_mut(&mut self) -> &mut L {
        &mut self.link
    }

    /// Messages skipped because they did not fit (dropped by the link as
    /// [`Recv::Oversized`]).
    pub fn oversized(&self) -> u32 {
        self.oversized
    }

    /// Drops whatever is buffered in either direction (for a new
    /// connection).
    pub fn reset(&mut self) {
        self.rpos = 0;
        self.rlen = 0;
        self.wlen = 0;
    }

    /// The length of the complete batch at the front of `wr`, if there is
    /// one.
    fn complete(&self) -> Option<usize> {
        let head = self.wr.get(..LEN_PREFIX)?;
        if self.wlen < LEN_PREFIX {
            return None;
        }
        let len = usize::from(u16::from_le_bytes([head[0], head[1]]));
        (self.wlen >= LEN_PREFIX + len).then_some(len)
    }

    /// Sends the complete batch waiting in `wr`, if any, and drops it.
    async fn send_complete(&mut self) -> Result<(), L::Error> {
        let Some(len) = self.complete() else {
            return Ok(());
        };
        let end = LEN_PREFIX + len;
        let batch = self.wr.get(LEN_PREFIX..end).unwrap_or(&[]);
        if !batch.is_empty() {
            self.link.send(batch).await?;
        }
        self.wr.copy_within(end..self.wlen, 0);
        self.wlen -= end;
        Ok(())
    }
}

impl<L: MessageLink, const N: usize> ErrorType for Unframed<L, N> {
    type Error = L::Error;
}

impl<L: MessageLink, const N: usize> Read for Unframed<L, N> {
    /// The next bytes of the stream: each message preceded by its length.
    /// Empty and oversized messages are skipped; a closed link reads 0.
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        while self.rpos >= self.rlen {
            let body = self.rd.get_mut(LEN_PREFIX..).unwrap_or(&mut []);
            match self.link.recv(body).await? {
                Recv::Closed => return Ok(0),
                Recv::Oversized => self.oversized = self.oversized.saturating_add(1),
                Recv::Message(0) => {}
                Recv::Message(n) => {
                    let Ok(len) = u16::try_from(n) else {
                        self.oversized = self.oversized.saturating_add(1);
                        continue;
                    };
                    if n > body.len() {
                        self.oversized = self.oversized.saturating_add(1);
                        continue;
                    }
                    self.rd[..LEN_PREFIX].copy_from_slice(&len.to_le_bytes());
                    self.rpos = 0;
                    self.rlen = LEN_PREFIX + n;
                }
            }
        }
        let have = self.rd.get(self.rpos..self.rlen).unwrap_or(&[]);
        let n = have.len().min(buf.len());
        buf[..n].copy_from_slice(&have[..n]);
        self.rpos += n;
        Ok(n)
    }
}

impl<L: MessageLink, const N: usize> Write for Unframed<L, N> {
    /// Takes stream bytes (length, batch, length, batch...). A batch whose
    /// length does not fit `N` writes 0 (the client treats that as the
    /// end of the link).
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        self.send_complete().await?;
        if buf.is_empty() {
            return Ok(0);
        }
        let room = N - self.wlen;
        let mut n = buf.len().min(room);
        // Take no more than the current batch, so a batch is never sent
        // with the next one's bytes behind it in the buffer.
        if self.wlen >= LEN_PREFIX {
            let len = usize::from(u16::from_le_bytes([self.wr[0], self.wr[1]]));
            n = n.min(LEN_PREFIX + len - self.wlen);
        } else if self.wlen + n > LEN_PREFIX {
            let mut head = [0u8; LEN_PREFIX];
            head[..self.wlen].copy_from_slice(&self.wr[..self.wlen]);
            let rest = LEN_PREFIX - self.wlen;
            head[self.wlen..].copy_from_slice(&buf[..rest]);
            let len = usize::from(u16::from_le_bytes(head));
            if LEN_PREFIX + len > N {
                self.wlen = 0;
                return Ok(0);
            }
            n = n.min(LEN_PREFIX + len - self.wlen);
        }
        self.wr[self.wlen..self.wlen + n].copy_from_slice(&buf[..n]);
        self.wlen += n;
        Ok(n)
    }

    /// Sends the complete batch, if one is waiting.
    async fn flush(&mut self) -> Result<(), Self::Error> {
        self.send_complete().await
    }
}

#[cfg(test)]
mod tests;
