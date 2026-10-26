//! UART echo: echoes UART1 receive data back with DMA writes and publishes
//! each received line.
//!
//! * `init` opens UART1 on [`TX_PIN`] / [`RX_PIN`] at [`BAUD`] 8N1.
//! * Each coalesced [`Event::UartRx`] drains the firmware's receive buffer
//!   in [`CHUNK`]-byte reads (bounded: at most [`MAX_READS`] per event).
//!   The bytes are appended to a [`PENDING`]-byte transmit queue and fed to a
//!   [`LineBuf`]; every completed line (ended by `\n` or `\r`, empty lines
//!   skipped, longer lines split at [`MAX_LINE`] bytes) is published on
//!   [`LINE_KEY`] without its terminator.
//! * A partial line also ends after [`IDLE_US`] without new bytes: each
//!   receive restarts one-shot hardware timer [`IDLE_TIMER`], whose alarm
//!   publishes what has arrived. Devices that send unterminated messages
//!   work this way, and so does the simulator's `!uart <text>`, which sends
//!   the text without a newline.
//! * The transmit queue is handed to [`Uart::write`] whenever no write is in
//!   flight; the host copies it, so the queue is reusable at once. The
//!   `DmaDone` of the write flushes anything queued meanwhile. Bytes that do
//!   not fit the queue are dropped and counted in the log. A write in flight
//!   for longer than [`STALL_US`] is assumed lost (event queue overflow) and
//!   recovered.
//!
//! Pins default to the ESP32-S3 UART1 defaults, GPIO 17 (TX) and GPIO 18
//! (RX), which are free on ESP32-S3-DevKitC-1 boards. Connect a 3.3 V USB
//! serial adapter crosswise (adapter RX to GPIO 17, adapter TX to GPIO 18,
//! common ground). In the simulator, type `!uart <text>`.
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use mw_payload::irq::Event;
use mw_payload::timer::Timer;
use mw_payload::uart::{UART_PORT, UART_RX_BUFFER, Uart};
use mw_payload::{AbiError, LogLine, Payload};

/// UART1 transmit pin.
pub const TX_PIN: u8 = 17;
/// UART1 receive pin.
pub const RX_PIN: u8 = 18;
/// Baud rate.
pub const BAUD: u32 = 115_200;
/// Key received lines are published on.
pub const LINE_KEY: &str = "app/uart/line";
/// Longest published line; longer lines are split.
pub const MAX_LINE: usize = 128;
/// Bytes per `uart_read`.
pub const CHUNK: usize = 64;
/// Reads per `UartRx` event: enough to drain the firmware buffer once.
pub const MAX_READS: usize = UART_RX_BUFFER / CHUNK + 1;
/// Transmit queue size.
pub const PENDING: usize = 512;
/// A write in flight this long is assumed to have lost its `DmaDone`.
pub const STALL_US: u32 = 1_000_000;
/// Hardware timer that ends a partial line after a pause.
pub const IDLE_TIMER: u8 = 1;
/// Pause after which a partial line is published.
pub const IDLE_US: u32 = 200_000;

/// Splits a byte stream into lines of at most [`MAX_LINE`] bytes.
///
/// Mutated in place because it owns a buffer; [`LineBuf::push`] is the only
/// transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineBuf {
    buf: [u8; MAX_LINE],
    len: usize,
}

impl Default for LineBuf {
    fn default() -> Self {
        LineBuf {
            buf: [0; MAX_LINE],
            len: 0,
        }
    }
}

impl LineBuf {
    /// Feeds one byte and calls `emit` with each completed, non-empty line.
    pub fn push(&mut self, byte: u8, emit: &mut impl FnMut(&[u8])) {
        if byte == b'\n' || byte == b'\r' {
            if self.len > 0 {
                emit(&self.buf[..self.len]);
                self.len = 0;
            }
            return;
        }
        if self.len == MAX_LINE {
            emit(&self.buf[..self.len]);
            self.len = 0;
        }
        self.buf[self.len] = byte;
        self.len += 1;
    }

    /// Calls `emit` with the partial line, if any, and empties the buffer.
    pub fn flush(&mut self, emit: &mut impl FnMut(&[u8])) {
        if self.len > 0 {
            emit(&self.buf[..self.len]);
            self.len = 0;
        }
    }

    /// Whether a partial line is waiting.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Bytes waiting for the transmitter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxQueue {
    buf: [u8; PENDING],
    len: usize,
}

impl Default for TxQueue {
    fn default() -> Self {
        TxQueue {
            buf: [0; PENDING],
            len: 0,
        }
    }
}

impl TxQueue {
    /// Appends what fits of `data`; returns how many bytes were dropped.
    pub fn append(&mut self, data: &[u8]) -> usize {
        let take = data.len().min(PENDING - self.len);
        self.buf[self.len..self.len + take].copy_from_slice(&data[..take]);
        self.len += take;
        data.len() - take
    }

    /// The queued bytes.
    pub fn as_slice(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// Empties the queue.
    pub fn clear(&mut self) {
        self.len = 0;
    }
}

/// Application state.
pub struct UartEcho {
    uart: Option<Uart>,
    line: LineBuf,
    tx: TxQueue,
    tx_started_us: u32,
    dropped: u32,
    idle: Option<Timer>,
}

/// Logs `"uart_echo: <what> error <code>"`.
fn log_err(what: &str, err: AbiError) {
    let mut line = LogLine::new();
    line.push_str("uart_echo: ")
        .push_str(what)
        .push_str(" error ")
        .push_u32(err.code().unsigned_abs());
    line.log();
}

fn publish_line(line: &[u8]) {
    if let Err(e) = mw_payload::publish(LINE_KEY, line) {
        log_err("publish", e);
    }
}

impl UartEcho {
    /// Starts a write of the queued bytes if the transmitter is free.
    fn flush(&mut self, now_us: u32) {
        let Some(uart) = self.uart.as_mut() else {
            return;
        };
        if uart.is_busy() && now_us.wrapping_sub(self.tx_started_us) >= STALL_US {
            mw_payload::log("uart_echo: write completion lost, recovering");
            uart.recover();
        }
        if self.tx.as_slice().is_empty() || uart.is_busy() {
            return;
        }
        match uart.write(self.tx.as_slice()) {
            Ok(()) => {
                self.tx.clear();
                self.tx_started_us = now_us;
            }
            Err(AbiError::Busy) => {}
            Err(e) => {
                log_err("uart_write", e);
                self.tx.clear();
            }
        }
    }

    fn receive(&mut self) {
        let Some(uart) = self.uart.as_mut() else {
            return;
        };
        let mut chunk = [0u8; CHUNK];
        for _ in 0..MAX_READS {
            let n = match uart.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => n.min(CHUNK),
                Err(e) => {
                    log_err("uart_read", e);
                    break;
                }
            };
            let data = &chunk[..n];
            let lost = self.tx.append(data);
            for &b in data {
                self.line.push(b, &mut |l| publish_line(l));
            }
            if lost > 0 {
                self.dropped = self.dropped.wrapping_add(lost as u32);
                let mut line = LogLine::new();
                line.push_str("uart_echo: tx queue full, dropped ")
                    .push_u32(self.dropped);
                line.log();
            }
        }
        if !self.line.is_empty() {
            self.restart_idle();
        }
    }

    /// (Re)arms the one-shot idle timer. Stopping first keeps the restart
    /// portable; stopping an expired timer is harmless.
    fn restart_idle(&mut self) {
        if let Some(t) = self.idle.take()
            && let Err(e) = t.stop()
        {
            log_err("timer_stop", e);
        }
        self.idle = Timer::start_once(IDLE_TIMER, IDLE_US)
            .map_err(|e| log_err("timer_start", e))
            .ok();
    }
}

impl Payload for UartEcho {
    fn init() -> Self {
        let uart = Uart::open(Some(TX_PIN), Some(RX_PIN), BAUD)
            .map_err(|e| log_err("uart_open", e))
            .ok();
        mw_payload::log("uart_echo: started");
        UartEcho {
            uart,
            line: LineBuf::default(),
            tx: TxQueue::default(),
            tx_started_us: 0,
            dropped: 0,
            idle: None,
        }
    }

    fn on_irq(&mut self, event: Event) {
        match event {
            Event::UartRx { port, .. } if i32::from(port) == UART_PORT => self.receive(),
            Event::Timer { id: IDLE_TIMER, .. } => {
                self.idle = None;
                self.line.flush(&mut |l| publish_line(l));
            }
            Event::DmaDone { .. } => {
                if let Some(Err(e)) = self.uart.as_mut().and_then(|u| u.on_event(&event)) {
                    log_err("uart tx", e);
                }
            }
            _ => return,
        }
        self.flush(event.time_us());
    }
}

// The exports need a single-threaded wasm32 module; host builds only run
// the unit tests below.
#[cfg(target_arch = "wasm32")]
mw_payload::payload!(UartEcho);

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(input: &[u8]) -> Vec<Vec<u8>> {
        let mut lb = LineBuf::default();
        let mut out = Vec::new();
        for &b in input {
            lb.push(b, &mut |l| out.push(l.to_vec()));
        }
        out
    }

    #[test]
    fn splits_on_cr_lf_and_skips_empty_lines() {
        assert_eq!(
            lines(b"hello\r\nworld\n\npartial"),
            [b"hello".to_vec(), b"world".to_vec()]
        );
    }

    #[test]
    fn flush_emits_the_partial_line_once() {
        let mut lb = LineBuf::default();
        let mut out = Vec::new();
        for &b in b"abc" {
            lb.push(b, &mut |l| out.push(l.to_vec()));
        }
        assert!(!lb.is_empty());
        lb.flush(&mut |l| out.push(l.to_vec()));
        lb.flush(&mut |l| out.push(l.to_vec()));
        assert_eq!(out, [b"abc".to_vec()]);
        assert!(lb.is_empty());
    }

    #[test]
    fn splits_overlong_lines() {
        let long = [b'x'; MAX_LINE + 3];
        let out = lines(&[&long[..], b"\n"].concat());
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].len(), MAX_LINE);
        assert_eq!(out[1].len(), 3);
    }

    #[test]
    fn tx_queue_drops_overflow() {
        let mut q = TxQueue::default();
        assert_eq!(q.append(&[1; PENDING - 2]), 0);
        assert_eq!(q.append(&[2; 5]), 3);
        assert_eq!(q.as_slice().len(), PENDING);
        q.clear();
        assert!(q.as_slice().is_empty());
    }
}
