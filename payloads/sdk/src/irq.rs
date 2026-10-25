//! Interrupt and DMA events delivered to [`Payload::on_irq`](crate::Payload::on_irq).
//!
//! # The interrupt model
//!
//! Payload code never runs in interrupt context. The firmware's ISR only
//! timestamps the event and pushes it into a bounded queue
//! ([`MAX_EVENTS`] entries); the main loop later calls the payload's
//! `on_irq(kind, source, value, time_us)` export once per event, with the
//! usual fuel budget. Consequences for payload code:
//!
//! * Handlers are ordinary code: they may publish, log and start transfers.
//!   They are never preempted by another `on_irq`, `tick` or `on_message`.
//! * Latency is the main-loop latency, not the interrupt latency. Use
//!   [`Event::time_us`] (taken in the ISR) for timing, not [`now_us`].
//! * Periodic timer and UART RX events coalesce while one is pending: the
//!   payload then sees one [`Event::Timer`] with `periods > 1`, or one
//!   [`Event::UartRx`] with the latest byte count.
//! * Other events take a queue slot each; when the queue is full they are
//!   dropped and counted. [`overflow`] returns (and resets) that count. A
//!   dropped [`Event::DmaDone`] means a transfer finished unseen; see
//!   [`Spi::recover`](crate::spi::Spi::recover).
//!
//! [`Event::decode`] turns the four raw `i32` arguments into an [`Event`]. It
//! returns `None` for an unknown kind (a newer host) or an argument outside
//! the ABI's range, and the generated `on_irq` export ignores such events
//! instead of panicking.

use crate::{AbiError, check, sys};
pub use middleware::wasm_abi::events::{DmaChannel, EventKind, MAX_EVENTS};

/// One decoded interrupt or DMA event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// A GPIO edge enabled with [`Input::enable_irq`](crate::gpio::Input::enable_irq).
    Gpio {
        /// The pin.
        pin: u8,
        /// Level after the edge.
        high: bool,
        /// Microseconds since boot when the ISR ran (wraps).
        time_us: u32,
    },
    /// A hardware timer alarm from [`Timer`](crate::timer::Timer).
    Timer {
        /// Timer id, `< MAX_TIMERS`.
        id: u8,
        /// Periods elapsed since the payload last saw this timer (≥ 1;
        /// more than 1 means alarms coalesced while the event was queued).
        periods: u32,
        /// Microseconds since boot of the latest alarm (wraps).
        time_us: u32,
    },
    /// A DMA transfer finished.
    DmaDone {
        /// Which transfer.
        channel: DmaChannel,
        /// Bytes transferred, or the error the transfer failed with.
        result: Result<usize, AbiError>,
        /// Microseconds since boot when the transfer completed (wraps).
        time_us: u32,
    },
    /// Bytes are waiting on a secondary UART; read them with
    /// [`Uart::read`](crate::uart::Uart::read).
    UartRx {
        /// The UART port.
        port: u8,
        /// Bytes buffered when the latest byte arrived.
        available: usize,
        /// Microseconds since boot of the latest receive interrupt (wraps).
        time_us: u32,
    },
}

impl Event {
    /// Decodes the `on_irq(kind, source, value, time_us)` arguments.
    ///
    /// Returns `None` for an unknown `kind`, a source outside its type (pin
    /// or timer id above 255, unknown DMA channel), a GPIO level other than
    /// 0/1, a timer period count below 1 or a negative UART byte count.
    pub fn decode(kind: i32, source: i32, value: i32, time_us: i32) -> Option<Self> {
        let time_us = time_us as u32;
        match EventKind::from_i32(kind)? {
            EventKind::Gpio => Some(Event::Gpio {
                pin: u8::try_from(source).ok()?,
                high: match value {
                    0 => false,
                    1 => true,
                    _ => return None,
                },
                time_us,
            }),
            EventKind::Timer => Some(Event::Timer {
                id: u8::try_from(source).ok()?,
                periods: u32::try_from(value).ok().filter(|&p| p >= 1)?,
                time_us,
            }),
            EventKind::DmaDone => Some(Event::DmaDone {
                channel: DmaChannel::from_i32(source)?,
                result: check(value).map(|n| n as usize),
                time_us,
            }),
            EventKind::UartRx => Some(Event::UartRx {
                port: u8::try_from(source).ok()?,
                available: usize::try_from(value).ok()?,
                time_us,
            }),
        }
    }

    /// The ISR timestamp, in microseconds since boot (wraps).
    pub const fn time_us(&self) -> u32 {
        match *self {
            Event::Gpio { time_us, .. }
            | Event::Timer { time_us, .. }
            | Event::DmaDone { time_us, .. }
            | Event::UartRx { time_us, .. } => time_us,
        }
    }
}

/// Microseconds since boot; wraps at `u32::MAX` (about 71 minutes). Compare
/// timestamps with `wrapping_sub`.
pub fn now_us() -> u32 {
    sys::now_us() as u32
}

/// Events dropped because the queue was full since the previous call; the
/// host resets the count on each call.
pub fn overflow() -> Result<u32, AbiError> {
    check(sys::irq_overflow()).map(|n| n as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::wasm_abi::events::Event as Raw;

    #[test]
    fn decodes_every_kind() {
        assert_eq!(
            Event::decode(1, 4, 1, 10),
            Some(Event::Gpio {
                pin: 4,
                high: true,
                time_us: 10
            })
        );
        assert_eq!(
            Event::decode(2, 3, 5, 20),
            Some(Event::Timer {
                id: 3,
                periods: 5,
                time_us: 20
            })
        );
        assert_eq!(
            Event::decode(3, 0, 16, 30),
            Some(Event::DmaDone {
                channel: DmaChannel::Spi0,
                result: Ok(16),
                time_us: 30
            })
        );
        assert_eq!(
            Event::decode(3, 1, AbiError::Transport.code(), 40),
            Some(Event::DmaDone {
                channel: DmaChannel::Uart1Tx,
                result: Err(AbiError::Transport),
                time_us: 40
            })
        );
        assert_eq!(
            Event::decode(4, 1, 7, -1),
            Some(Event::UartRx {
                port: 1,
                available: 7,
                time_us: u32::MAX
            })
        );
    }

    #[test]
    fn matches_the_abi_encoding() {
        let raw = Raw {
            kind: EventKind::Timer,
            source: 2,
            value: 1,
            time_us: 3_000_000_000,
        };
        let [k, s, v, t] = raw.to_args();
        let ev = Event::decode(k, s, v, t).expect("valid");
        assert_eq!(ev.time_us(), 3_000_000_000);
        assert!(matches!(
            ev,
            Event::Timer {
                id: 2,
                periods: 1,
                ..
            }
        ));
    }

    #[test]
    fn rejects_unknown_or_out_of_range_arguments() {
        assert_eq!(Event::decode(0, 0, 0, 0), None, "unknown kind");
        assert_eq!(Event::decode(99, 0, 0, 0), None, "unknown kind");
        assert_eq!(Event::decode(1, 256, 1, 0), None, "pin range");
        assert_eq!(Event::decode(1, -1, 1, 0), None, "pin range");
        assert_eq!(Event::decode(1, 4, 2, 0), None, "level");
        assert_eq!(Event::decode(2, 0, 0, 0), None, "periods");
        assert_eq!(Event::decode(2, 300, 1, 0), None, "timer id");
        assert_eq!(Event::decode(3, 2, 4, 0), None, "channel");
        assert_eq!(Event::decode(4, 1, -1, 0), None, "byte count");
    }

    #[test]
    fn unknown_dma_error_codes_map_to_unsupported() {
        assert_eq!(
            Event::decode(3, 0, -99, 0),
            Some(Event::DmaDone {
                channel: DmaChannel::Spi0,
                result: Err(AbiError::Unsupported),
                time_us: 0
            })
        );
    }

    #[test]
    fn host_stubs() {
        assert_eq!(now_us(), 0);
        assert_eq!(overflow(), Err(AbiError::Unsupported));
    }
}
