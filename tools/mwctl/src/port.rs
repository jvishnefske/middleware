//! The serial port shell: opens exactly the port it is given and drives the
//! DTR/RTS lines of a USB-UART bridge (CH340, CP2102, FTDI) wired to the
//! classic two-transistor auto-reset circuit, where
//!
//! | DTR        | RTS        | effect                        |
//! |------------|------------|-------------------------------|
//! | deasserted | deasserted | EN high, GPIO0 high: run      |
//! | asserted   | asserted   | same (the transistors cancel) |
//! | deasserted | asserted   | EN low: chip held in reset    |
//! | asserted   | deasserted | GPIO0 low: bootloader if reset|
//!
//! Linux asserts both lines together when a tty is opened; that is the
//! harmless "both asserted" row. [`SerialTransport::open`] then releases RTS
//! first and DTR second, so the lines pass through "DTR only" (GPIO0 low,
//! no reset) rather than "RTS only" (reset). The board keeps running.
//!
//! [`SerialTransport::reset`] holds RTS for 100 ms with DTR deasserted:
//! EN pulses low with GPIO0 high, so the chip reboots into the application.

use mwctl::link::Transport;
use serialport::{ClearBuffer, SerialPort};
use std::io::{self, Read, Write};
use std::time::Duration;

/// How long `reset` holds EN low.
pub const RESET_PULSE: Duration = Duration::from_millis(100);

/// A serial port as a [`Transport`].
pub struct SerialTransport {
    port: Box<dyn SerialPort>,
}

fn to_io(e: serialport::Error) -> io::Error {
    io::Error::other(e)
}

/// serialport's text for ENOTTY: it maps the errno to `ErrorKind::Unknown`
/// with nix's fixed `Errno::desc()` and drops the number (as in
/// `mw_gateway::io`, RG28). The PTY test breaks if an upgrade changes it.
const ENOTTY_DESC: &str = "Not a typewriter";

/// The result of setting one modem line (H7): a port without modem lines
/// (ENOTTY, a PTY such as the KD240's local claim link) is fine, since
/// there is no auto-reset circuit to drive; any other failure is fatal and
/// names the port and the line.
fn modem_line(path: &str, line: &str, r: serialport::Result<()>) -> io::Result<()> {
    match r {
        Ok(()) => Ok(()),
        Err(e) if e.kind == serialport::ErrorKind::Unknown && e.description == ENOTTY_DESC => {
            Ok(())
        }
        Err(e) => Err(io::Error::other(format!("{path}: {line}: {e}"))),
    }
}

impl SerialTransport {
    /// Opens `path` at `baud`, leaving DTR and RTS deasserted.
    pub fn open(path: &str, baud: u32) -> io::Result<Self> {
        let mut port = serialport::new(path, baud)
            .preserve_dtr_on_open()
            .timeout(Duration::from_millis(100))
            .open()
            .map_err(|e| io::Error::other(format!("{path}: {e}")))?;
        modem_line(path, "RTS", port.write_request_to_send(false))?;
        modem_line(path, "DTR", port.write_data_terminal_ready(false))?;
        // Drop anything buffered before we were listening.
        port.clear(ClearBuffer::Input).map_err(to_io)?;
        Ok(SerialTransport { port })
    }

    /// Reboots the board into run mode through the auto-reset circuit.
    pub fn reset(&mut self) -> io::Result<()> {
        let path = self.port.name().unwrap_or_default();
        modem_line(&path, "DTR", self.port.write_data_terminal_ready(false))?;
        modem_line(&path, "RTS", self.port.write_request_to_send(true))?;
        std::thread::sleep(RESET_PULSE);
        modem_line(&path, "RTS", self.port.write_request_to_send(false))
    }
}

impl Transport for SerialTransport {
    /// Hands `bytes` to the kernel; each write waits at most the port's
    /// timeout for room. No `flush`: serialport's is `tcdrain`, which
    /// waits without a timeout until the device has taken every byte, and
    /// a device whose serial task is not running (a USB-Serial-JTAG port
    /// stops accepting data) then blocks mwctl indefinitely instead of
    /// letting the request time out.
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.port.write_all(bytes)
    }

    fn read(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
        // serialport rejects a zero timeout on some platforms.
        self.port
            .set_timeout(timeout.max(Duration::from_millis(1)))
            .map_err(to_io)?;
        match self.port.read(buf) {
            Ok(n) => Ok(n),
            Err(e) if e.kind() == io::ErrorKind::TimedOut => Ok(0),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => Ok(0),
            Err(e) => Err(e),
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    /// A PTY pair: the master as a file, the slave's path, and the slave's
    /// fd, kept open so the master never sees a hangup.
    fn openpty() -> (std::fs::File, String, OwnedFd) {
        let (mut master, mut slave) = (-1, -1);
        // SAFETY: openpty writes two fds into the given ints; the name,
        // termios and winsize arguments may be null.
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        assert_eq!(rc, 0, "openpty: {}", io::Error::last_os_error());
        // SAFETY: both fds were just opened by openpty and nothing else owns them.
        let (master, slave) = unsafe {
            (
                std::fs::File::from_raw_fd(master),
                OwnedFd::from_raw_fd(slave),
            )
        };
        let path = std::fs::read_link(format!("/proc/self/fd/{}", slave.as_raw_fd())).unwrap();
        (master, path.to_str().unwrap().to_owned(), slave)
    }

    #[test]
    fn h7_opens_a_pty_slave_and_carries_bytes_both_ways() {
        let (mut master, path, _slave) = openpty();
        let mut port = SerialTransport::open(&path, 115_200).expect("a PTY slave opens");
        port.reset().expect("reset is a no-op without modem lines");

        port.write_all(b"to the master").unwrap();
        let mut got = [0u8; 13];
        master.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"to the master");

        master.write_all(b"to mwctl").unwrap();
        let mut back = Vec::new();
        let mut buf = [0u8; 16];
        for _ in 0..50 {
            let n = port.read(&mut buf, Duration::from_millis(100)).unwrap();
            back.extend_from_slice(&buf[..n]);
            if back.len() >= 8 {
                break;
            }
        }
        assert_eq!(back, b"to mwctl");
    }

    #[test]
    fn h7_only_inappropriate_ioctl_is_a_missing_modem_line() {
        let enotty = serialport::Error::new(serialport::ErrorKind::Unknown, "Not a typewriter");
        assert!(modem_line("p", "RTS", Err(enotty)).is_ok());
        for e in [
            serialport::Error::new(serialport::ErrorKind::Unknown, "Input/output error"),
            serialport::Error::new(serialport::ErrorKind::NoDevice, "Not a typewriter"),
            serialport::Error::new(
                serialport::ErrorKind::Io(io::ErrorKind::PermissionDenied),
                "x",
            ),
        ] {
            assert!(modem_line("p", "RTS", Err(e)).is_err());
        }
    }
}
