//! The serial port of the browser build (`--target wasm32-wasip1`): the
//! page's Web Serial port behind five host imports in module `mw_port`.
//!
//! mwctl stays synchronous. In the browser the page wraps these imports
//! with `WebAssembly.Suspending` (JavaScript Promise Integration), so a
//! `read` that waits for the board suspends this module instead of
//! blocking the page; a test supplies plain synchronous functions backed
//! by an in-memory device (`web/src/mwctl/host.ts`, `web/test/mwctl.test.ts`).
//!
//! | import | arguments | result |
//! |---|---|---|
//! | `open` | `path_ptr, path_len, baud` | 0, or a negative error |
//! | `write` | `ptr, len` | 0, or a negative error |
//! | `read` | `ptr, cap, timeout_ms` | bytes copied (0: nothing in time), or a negative error |
//! | `signals` | `dtr, rts` (0 or 1) | 0, or a negative error |
//! | `close` | | |
//!
//! Errors: -1 no such port (none granted, or not `web`), -2 the port went
//! away, -3 any other failure. The path is whatever follows `--port`; the
//! page serves the one port the user picked as `web`.
//!
//! The import block is the only `unsafe` in mwctl: each call passes a
//! pointer and a length taken from one live slice.

use mwctl::link::Transport;
use std::io;
use std::time::Duration;

/// How long `reset` holds EN low.
pub const RESET_PULSE: Duration = Duration::from_millis(100);

#[link(wasm_import_module = "mw_port")]
unsafe extern "C" {
    fn open(path: *const u8, len: usize, baud: u32) -> i32;
    fn write(bytes: *const u8, len: usize) -> i32;
    fn read(buf: *mut u8, cap: usize, timeout_ms: u32) -> i32;
    fn signals(dtr: i32, rts: i32) -> i32;
    fn close();
}

fn check(what: &str, code: i32) -> io::Result<usize> {
    match code {
        n if n >= 0 => Ok(n as usize),
        -1 => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{what}: no such port (in a browser the port is `web`, the one the page was granted)"),
        )),
        -2 => Err(io::Error::new(io::ErrorKind::NotConnected, format!("{what}: the port went away"))),
        _ => Err(io::Error::other(format!("{what}: the browser's serial port failed"))),
    }
}

/// The page's serial port as a [`Transport`].
pub struct SerialTransport(());

impl SerialTransport {
    /// Opens the page's port at `baud`, DTR and RTS deasserted.
    pub fn open(path: &str, baud: u32) -> io::Result<Self> {
        // SAFETY: the pointer and length are those of `path`, alive for the call.
        check(path, unsafe { open(path.as_ptr(), path.len(), baud) })?;
        Ok(SerialTransport(()))
    }

    /// Reboots the board into run mode through the auto-reset circuit.
    pub fn reset(&mut self) -> io::Result<()> {
        // SAFETY: no pointers.
        check("reset", unsafe { signals(0, 1) })?;
        std::thread::sleep(RESET_PULSE);
        // SAFETY: no pointers.
        check("reset", unsafe { signals(0, 0) }).map(|_| ())
    }
}

impl Transport for SerialTransport {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        // SAFETY: the pointer and length are those of `bytes`, alive for the call.
        check("write", unsafe { write(bytes.as_ptr(), bytes.len()) }).map(|_| ())
    }

    fn read(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
        let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX).max(1);
        // SAFETY: the pointer and capacity are those of `buf`, alive and
        // exclusively borrowed for the call; the host writes at most `cap`.
        let n = check("read", unsafe { read(buf.as_mut_ptr(), buf.len(), ms) })?;
        Ok(n.min(buf.len()))
    }
}

impl Drop for SerialTransport {
    fn drop(&mut self) {
        // SAFETY: no pointers.
        unsafe { close() }
    }
}
