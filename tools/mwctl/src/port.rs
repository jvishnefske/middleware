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

impl SerialTransport {
    /// Opens `path` at `baud`, leaving DTR and RTS deasserted.
    pub fn open(path: &str, baud: u32) -> io::Result<Self> {
        let mut port = serialport::new(path, baud)
            .preserve_dtr_on_open()
            .timeout(Duration::from_millis(100))
            .open()
            .map_err(|e| io::Error::other(format!("{path}: {e}")))?;
        port.write_request_to_send(false).map_err(to_io)?;
        port.write_data_terminal_ready(false).map_err(to_io)?;
        // Drop anything buffered before we were listening.
        port.clear(ClearBuffer::Input).map_err(to_io)?;
        Ok(SerialTransport { port })
    }

    /// Reboots the board into run mode through the auto-reset circuit.
    pub fn reset(&mut self) -> io::Result<()> {
        self.port.write_data_terminal_ready(false).map_err(to_io)?;
        self.port.write_request_to_send(true).map_err(to_io)?;
        std::thread::sleep(RESET_PULSE);
        self.port.write_request_to_send(false).map_err(to_io)
    }
}

impl Transport for SerialTransport {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.port.write_all(bytes)?;
        self.port.flush()
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
