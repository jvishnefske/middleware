//! Blocking frame link over an embedded-hal 0.2 serial port.
//!
//! [`Link`] owns the port (`serial::Read<u8> + serial::Write<u8>`, the
//! ESP8266 HAL's UART0 with `nb` non-blocking reads). It is generic so the
//! host tests drive it with a fake port.
//!
//! * Receive: [`Link::poll`] drains at most [`RX_BUDGET`] bytes (the
//!   UART RX FIFO is about 128 bytes, so the super-loop must call it often)
//!   into the caller's [`Splitter`] and stops at the first delimiter. A
//!   completed run is COBS-decoded **in place** in the splitter
//!   ([`frame::decode_wire`]) and returned as [`Rx::Frame`]; the frame
//!   borrows the splitter until the next poll. Runs that are not frames
//!   (the ROM's 74880-baud boot noise, line noise, overflow) come back as
//!   errors for the caller to skip.
//! * Send: [`Link::send`] encodes with [`frame::encode`] into the caller's
//!   [`TxBuffers`] and writes the bytes with a bounded spin per byte
//!   ([`SPIN_LIMIT`] polls), so a stuck transmitter cannot hang the loop.
//! * [`Link::prime`] writes one `0x00` at boot, as `firmware/` does, so the
//!   first frame is never glued to the tail of the ROM banner.
//!
//! [`Port`] pairs a link with its transmit buffers and implements the
//! controller's [`Outbound`] sink. Send failures are dropped there: the
//! host retries requests, and a cut-short frame fails its CRC on the host.

use embedded_hal::serial::{Read, Write};
use middleware::Key;
use middleware::serial::frame::{
    self, Frame, FrameError, MAX_RAW_FRAME, MAX_WIRE_FRAME, Split, Splitter,
};
use middleware::transport::HandlerError;

use crate::controller::Outbound;

/// Most bytes consumed by one [`Link::poll`].
pub const RX_BUDGET: usize = 128;

/// Polls of a full transmitter before a byte write is abandoned. One byte
/// takes about 87 µs at 115200 baud; at 80 MHz a poll is far below 1 µs,
/// so this is milliseconds of slack, not a real limit.
pub const SPIN_LIMIT: u32 = 200_000;

/// Why a link operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkError {
    /// The port reported a receive error (framing, overrun).
    Read,
    /// The port reported a transmit error.
    Write,
    /// The transmitter stayed busy for [`SPIN_LIMIT`] polls.
    Timeout,
    /// The outgoing frame could not be encoded.
    Frame(FrameError),
}

/// Result of one [`Link::poll`].
#[derive(Debug, PartialEq, Eq)]
pub enum Rx<'s> {
    /// No complete run yet (budget used or no bytes waiting).
    Idle,
    /// A delimited run, decoded (or why it is not a frame).
    Frame(Result<Frame<'s>, FrameError>),
    /// A run longer than the splitter was discarded.
    Overflow,
}

/// Transmit buffers, owned by the caller (on `main`'s stack).
pub struct TxBuffers {
    scratch: [u8; MAX_RAW_FRAME],
    wire: [u8; MAX_WIRE_FRAME],
}

impl TxBuffers {
    /// Zeroed buffers.
    pub const fn new() -> Self {
        TxBuffers {
            scratch: [0; MAX_RAW_FRAME],
            wire: [0; MAX_WIRE_FRAME],
        }
    }
}

impl Default for TxBuffers {
    fn default() -> Self {
        Self::new()
    }
}

/// The protocol endpoint over one serial port.
pub struct Link<S> {
    serial: S,
}

impl<S> Link<S> {
    /// Wraps a port.
    pub const fn new(serial: S) -> Self {
        Link { serial }
    }

    /// Returns the port.
    pub fn into_inner(self) -> S {
        self.serial
    }
}

impl<S: Read<u8> + Write<u8>> Link<S> {
    /// Feeds waiting bytes to `splitter` until a run completes or
    /// [`RX_BUDGET`] bytes were consumed.
    pub fn poll<'s, const N: usize>(
        &mut self,
        splitter: &'s mut Splitter<N>,
    ) -> Result<Rx<'s>, LinkError> {
        for _ in 0..RX_BUDGET {
            let byte = match self.serial.read() {
                Ok(b) => b,
                Err(nb::Error::WouldBlock) => return Ok(Rx::Idle),
                Err(nb::Error::Other(_)) => return Err(LinkError::Read),
            };
            match splitter.push(byte) {
                Split::Pending => {}
                Split::Overflow => return Ok(Rx::Overflow),
                Split::Frame(_) => return Ok(Rx::Frame(frame::decode_wire(splitter.frame()))),
            }
        }
        Ok(Rx::Idle)
    }

    /// Writes one `0x00` delimiter.
    pub fn prime(&mut self) -> Result<(), LinkError> {
        self.write_byte(0)?;
        self.flush()
    }

    /// Encodes `frame` into `bufs` and writes it.
    pub fn send(&mut self, frame: &Frame<'_>, bufs: &mut TxBuffers) -> Result<(), LinkError> {
        let n =
            frame::encode(frame, &mut bufs.scratch, &mut bufs.wire).map_err(LinkError::Frame)?;
        let bytes = bufs
            .wire
            .get(..n)
            .ok_or(LinkError::Frame(FrameError::TooLarge))?;
        for &b in bytes {
            self.write_byte(b)?;
        }
        self.flush()
    }

    fn write_byte(&mut self, byte: u8) -> Result<(), LinkError> {
        for _ in 0..SPIN_LIMIT {
            match self.serial.write(byte) {
                Ok(()) => return Ok(()),
                Err(nb::Error::WouldBlock) => {}
                Err(nb::Error::Other(_)) => return Err(LinkError::Write),
            }
        }
        Err(LinkError::Timeout)
    }

    fn flush(&mut self) -> Result<(), LinkError> {
        for _ in 0..SPIN_LIMIT {
            match self.serial.flush() {
                Ok(()) => return Ok(()),
                Err(nb::Error::WouldBlock) => {}
                Err(nb::Error::Other(_)) => return Err(LinkError::Write),
            }
        }
        Err(LinkError::Timeout)
    }
}

/// A link with its transmit buffers: the controller's output sink.
pub struct Port<'a, S> {
    link: &'a mut Link<S>,
    bufs: &'a mut TxBuffers,
}

impl<'a, S> Port<'a, S> {
    /// Borrows a link and buffers for sending.
    pub fn new(link: &'a mut Link<S>, bufs: &'a mut TxBuffers) -> Self {
        Port { link, bufs }
    }
}

impl<S: Read<u8> + Write<u8>> Outbound for Port<'_, S> {
    fn respond(&mut self, id: u8, result: Result<&[u8], HandlerError>) {
        let _ = self.link.send(&Frame::Response { id, result }, self.bufs);
    }

    fn publish(&mut self, key: Key<'_>, payload: &[u8]) {
        let _ = self.link.send(&Frame::Publish { key, payload }, self.bufs);
    }
}

#[cfg(test)]
pub mod fake {
    //! In-memory serial port for host tests.

    use core::convert::Infallible;
    use embedded_hal::serial::{Read, Write};
    use std::collections::VecDeque;

    /// A port that reads from a queue and records writes. `stall` makes
    /// the transmitter report `WouldBlock` forever.
    #[derive(Default)]
    pub struct FakeSerial {
        /// Bytes the "host" sent, not yet read by the device.
        pub rx: VecDeque<u8>,
        /// Bytes the device wrote.
        pub tx: Vec<u8>,
        /// Transmitter never ready.
        pub stall: bool,
        /// Next read reports a receive error.
        pub rx_error: bool,
    }

    impl Read<u8> for FakeSerial {
        type Error = ();
        fn read(&mut self) -> nb::Result<u8, ()> {
            if core::mem::take(&mut self.rx_error) {
                return Err(nb::Error::Other(()));
            }
            self.rx.pop_front().ok_or(nb::Error::WouldBlock)
        }
    }

    impl Write<u8> for FakeSerial {
        type Error = Infallible;
        fn write(&mut self, word: u8) -> nb::Result<(), Infallible> {
            if self.stall {
                return Err(nb::Error::WouldBlock);
            }
            self.tx.push(word);
            Ok(())
        }
        fn flush(&mut self) -> nb::Result<(), Infallible> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeSerial;
    use super::*;

    const K: Key<'static> = Key::from_static("sys/info");

    fn wire(f: &Frame<'_>) -> Vec<u8> {
        let mut bufs = TxBuffers::new();
        let mut link = Link::new(FakeSerial::default());
        link.send(f, &mut bufs).unwrap();
        link.into_inner().tx
    }

    #[test]
    fn prime_writes_one_delimiter() {
        let mut link = Link::new(FakeSerial::default());
        link.prime().unwrap();
        assert_eq!(link.into_inner().tx, [0]);
    }

    #[test]
    fn frames_are_found_after_noise() {
        let req = Frame::Request {
            id: 3,
            key: K,
            payload: &[1, 0, 2],
        };
        let mut serial = FakeSerial::default();
        serial.rx.extend(b"\x8a\xf3 rom noise\r\n");
        serial.rx.push_back(0);
        serial.rx.extend(wire(&req));
        let mut link = Link::new(serial);
        let mut splitter: Splitter<MAX_WIRE_FRAME> = Splitter::new();
        assert!(matches!(link.poll(&mut splitter), Ok(Rx::Frame(Err(_)))));
        assert_eq!(link.poll(&mut splitter), Ok(Rx::Frame(Ok(req))));
        assert_eq!(link.poll(&mut splitter), Ok(Rx::Idle));
    }

    #[test]
    fn poll_is_bounded_by_the_budget() {
        let mut serial = FakeSerial::default();
        serial.rx.extend(core::iter::repeat_n(7u8, RX_BUDGET + 10));
        let mut link = Link::new(serial);
        let mut splitter: Splitter<MAX_WIRE_FRAME> = Splitter::new();
        assert_eq!(link.poll(&mut splitter), Ok(Rx::Idle));
        assert_eq!(link.into_inner().rx.len(), 10);
    }

    #[test]
    fn oversized_runs_and_read_errors_are_reported() {
        let mut serial = FakeSerial::default();
        serial.rx.extend([5u8; 9]);
        serial.rx.push_back(0);
        let mut link = Link::new(serial);
        let mut small: Splitter<4> = Splitter::new();
        assert_eq!(link.poll(&mut small), Ok(Rx::Overflow));
        let mut serial = link.into_inner();
        serial.rx_error = true;
        let mut link = Link::new(serial);
        assert_eq!(link.poll(&mut small), Err(LinkError::Read));
    }

    #[test]
    fn sent_frames_decode_and_stalls_time_out() {
        let resp = Frame::Response {
            id: 9,
            result: Err(HandlerError::NotFound),
        };
        let mut bytes = wire(&resp);
        assert_eq!(bytes.pop(), Some(0));
        assert_eq!(frame::decode_wire(&mut bytes), Ok(resp));

        let mut link = Link::new(FakeSerial {
            stall: true,
            ..FakeSerial::default()
        });
        let mut bufs = TxBuffers::new();
        assert_eq!(link.send(&resp, &mut bufs), Err(LinkError::Timeout));
        let big = [1u8; MAX_RAW_FRAME];
        let mut link = Link::new(FakeSerial::default());
        let too_big = Frame::Publish {
            key: K,
            payload: &big,
        };
        assert_eq!(
            link.send(&too_big, &mut bufs),
            Err(LinkError::Frame(FrameError::TooLarge))
        );
    }

    #[test]
    fn port_sends_responses_and_publications() {
        let mut link = Link::new(FakeSerial::default());
        let mut bufs = TxBuffers::new();
        let mut port = Port::new(&mut link, &mut bufs);
        port.respond(1, Ok(b"ok"));
        port.publish(Key::from_static("sys/log"), b"hi");
        let tx = link.into_inner().tx;
        let mut runs = tx
            .split(|&b| b == 0)
            .filter(|r| !r.is_empty())
            .map(<[u8]>::to_vec);
        let mut a = runs.next().unwrap();
        assert_eq!(
            frame::decode_wire(&mut a),
            Ok(Frame::Response {
                id: 1,
                result: Ok(b"ok")
            })
        );
        let mut b = runs.next().unwrap();
        let key = Key::from_static("sys/log");
        assert_eq!(
            frame::decode_wire(&mut b),
            Ok(Frame::Publish {
                key,
                payload: b"hi"
            })
        );
    }
}
