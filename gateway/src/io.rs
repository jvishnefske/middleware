//! Byte streams for the device link: [`ChannelIo`], an `embedded-io-async`
//! [`Read`] + [`Write`] over two bounded tokio channels of byte chunks, so
//! a `SerialLink` can run in a tokio task whatever produces the bytes.
//!
//! * [`pair`] joins two `ChannelIo`s back to back: an in-memory serial
//!   cable for tests (the fake device on one end, the gateway on the
//!   other) and for embedding.
//! * [`open_serial`] opens one serial port (exactly the path it is given;
//!   it never enumerates) and moves bytes between it and a `ChannelIo`
//!   with two blocking threads, the imperative shell around the port.
//!
//! # Cancel safety
//!
//! `read` waits in `mpsc::Receiver::recv`, which is cancel safe, and
//! consumes a chunk only when it completes; the part of a chunk that did
//! not fit the caller's buffer is kept for the next read. `write` sends
//! the whole buffer as one chunk or nothing. So a `SerialLink` over a
//! `ChannelIo` keeps the link's own cancel-safety guarantees.

use embedded_io_async::{ErrorKind, ErrorType, Read, Write};
use std::io;
use std::time::Duration;
use tokio::sync::mpsc;

/// Chunks each direction buffers before a writer waits.
pub const CHANNEL_CHUNKS: usize = 64;

/// The other end went away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closed;

impl core::fmt::Display for Closed {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("byte stream closed")
    }
}

impl std::error::Error for Closed {}

impl embedded_io_async::Error for Closed {
    fn kind(&self) -> ErrorKind {
        ErrorKind::BrokenPipe
    }
}

/// A byte stream over two channels (module docs).
#[derive(Debug)]
pub struct ChannelIo {
    rx: mpsc::Receiver<Vec<u8>>,
    chunk: Vec<u8>,
    at: usize,
    tx: mpsc::Sender<Vec<u8>>,
}

impl ChannelIo {
    /// Reads from `rx`, writes to `tx`.
    pub fn new(rx: mpsc::Receiver<Vec<u8>>, tx: mpsc::Sender<Vec<u8>>) -> Self {
        ChannelIo {
            rx,
            chunk: Vec::new(),
            at: 0,
            tx,
        }
    }
}

/// Two streams joined back to back: what one writes the other reads.
pub fn pair() -> (ChannelIo, ChannelIo) {
    let (a_tx, b_rx) = mpsc::channel(CHANNEL_CHUNKS);
    let (b_tx, a_rx) = mpsc::channel(CHANNEL_CHUNKS);
    (ChannelIo::new(a_rx, a_tx), ChannelIo::new(b_rx, b_tx))
}

impl ErrorType for ChannelIo {
    type Error = Closed;
}

impl Read for ChannelIo {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Closed> {
        if buf.is_empty() {
            return Ok(0);
        }
        // Bounded: each pass either returns or replaces an exhausted chunk
        // by a received one; empty chunks are skipped.
        loop {
            let rest = self.chunk.get(self.at..).unwrap_or(&[]);
            if !rest.is_empty() {
                let n = rest.len().min(buf.len());
                buf[..n].copy_from_slice(&rest[..n]);
                self.at += n;
                return Ok(n);
            }
            self.chunk = self.rx.recv().await.ok_or(Closed)?;
            self.at = 0;
        }
    }
}

impl Write for ChannelIo {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Closed> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.tx.send(buf.to_vec()).await.map_err(|_| Closed)?;
        Ok(buf.len())
    }

    async fn flush(&mut self) -> Result<(), Closed> {
        Ok(())
    }
}

/// How long one blocking port read waits before the reader thread checks
/// whether the gateway is still there.
pub const PORT_READ_TIMEOUT: Duration = Duration::from_millis(100);

/// Opens `path` at `baud` and returns the stream the gateway's link runs
/// on. DTR and RTS are left deasserted (as `mwctl` does), so opening the
/// port does not reset a board wired to the usual auto-reset circuit.
/// Two threads move the bytes and end when the stream is dropped or the
/// port fails.
pub fn open_serial(path: &str, baud: u32) -> io::Result<ChannelIo> {
    let mut port = serialport::new(path, baud)
        .preserve_dtr_on_open()
        .timeout(PORT_READ_TIMEOUT)
        .open()
        .map_err(|e| io::Error::other(format!("{path}: {e}")))?;
    port.write_request_to_send(false)
        .map_err(io::Error::other)?;
    port.write_data_terminal_ready(false)
        .map_err(io::Error::other)?;
    let mut writer = port.try_clone().map_err(io::Error::other)?;
    let (to_gateway, gateway_rx) = mpsc::channel::<Vec<u8>>(CHANNEL_CHUNKS);
    let (gateway_tx, mut to_port) = mpsc::channel::<Vec<u8>>(CHANNEL_CHUNKS);
    std::thread::Builder::new()
        .name("mw-gateway-rx".into())
        .spawn(move || {
            let mut buf = [0u8; 256];
            while !to_gateway.is_closed() {
                match port.read(&mut buf) {
                    Ok(0) => {}
                    Ok(n) => {
                        if to_gateway.blocking_send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
                        ) => {}
                    Err(e) => {
                        tracing::error!(error = %e, "serial read failed");
                        break;
                    }
                }
            }
        })?;
    std::thread::Builder::new()
        .name("mw-gateway-tx".into())
        .spawn(move || {
            while let Some(chunk) = to_port.blocking_recv() {
                if let Err(e) = io::Write::write_all(&mut writer, &chunk) {
                    tracing::error!(error = %e, "serial write failed");
                    break;
                }
            }
        })?;
    Ok(ChannelIo::new(gateway_rx, gateway_tx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn v14_pair_carries_bytes_both_ways_and_keeps_partial_chunks() {
        let (mut a, mut b) = pair();
        a.write_all(b"hello").await.unwrap();
        let mut two = [0u8; 2];
        assert_eq!(b.read(&mut two).await, Ok(2));
        assert_eq!(&two, b"he");
        let mut rest = [0u8; 8];
        assert_eq!(b.read(&mut rest).await, Ok(3));
        assert_eq!(&rest[..3], b"llo");
        b.write_all(b"x").await.unwrap();
        b.flush().await.unwrap();
        assert_eq!(a.read(&mut rest).await, Ok(1));
        assert_eq!(a.read(&mut []).await, Ok(0));
        assert_eq!(a.write(&[]).await, Ok(0));
        drop(b);
        assert_eq!(a.read(&mut rest).await, Err(Closed));
        assert_eq!(a.write(b"y").await, Err(Closed));
        assert_eq!(
            embedded_io_async::Error::kind(&Closed),
            ErrorKind::BrokenPipe
        );
    }
}
