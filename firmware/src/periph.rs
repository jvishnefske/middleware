//! Pure decision logic behind the ABI 2 peripherals (interrupts, timers,
//! SPI and UART DMA).
//!
//! The imperative shell (`src/shell/irq.rs`, `timer.rs`, `spi.rs`,
//! `uart1.rs`, `device.rs`) owns the esp-hal drivers and ISRs; every
//! decision it makes comes from here, so it is unit tested on the host:
//!
//! * [`claims`] — which pins the SPI bus, the secondary UART and GPIO
//!   interrupts hold, so they never collide with each other or with the
//!   chip's reserved pins (D5, D6, D7).
//! * [`timers`] — the deadline table that multiplexes the four payload
//!   timers onto one hardware alarm: next deadline, periodic re-arm, missed
//!   periods (D8).
//! * [`dma`] — the per-channel transfer state machine
//!   (`Idle → InFlight → Done`, read back while `Done`) (D6, D7).
//! * [`dma_hw`] — who holds a DMA peripheral's driver and buffers (idle,
//!   or the running transfer), so a payload stop in any state returns them
//!   (D6, R12).
//! * [`ring`] — the fixed UART receive ring that drops and counts bytes
//!   beyond its capacity (D7).
//!
//! The helpers in this module turn ISR observations into queue events
//! ([`gpio_event`], [`timer_event`], [`dma_done_event`]) and decide when an
//! overflow of the event queue is reported on `sys/log`
//! ([`overflow_report`]) (D5).

pub mod claims;
pub mod dma;
pub mod dma_hw;
pub mod ring;
pub mod timers;

use middleware::wasm_abi::events::{DmaChannel, Event, EventKind};
use middleware::wasm_abi::periph::Edge;

/// The event a GPIO interrupt on `pin` produces when the level read in the
/// ISR is `level`, or `None` when `edge` does not match (a bounce read back
/// the other level, or the interrupt is off).
pub fn gpio_event(pin: u8, edge: Edge, level: bool, time_us: u32) -> Option<Event> {
    edge.matches(level).then_some(Event {
        kind: EventKind::Gpio,
        source: i32::from(pin),
        value: i32::from(level),
        time_us,
    })
}

/// The event for timer `id` firing after `periods` elapsed periods (at
/// least 1; saturates at `i32::MAX`).
pub fn timer_event(id: u8, periods: u32, time_us: u32) -> Event {
    Event {
        kind: EventKind::Timer,
        source: i32::from(id),
        value: i32::try_from(periods.max(1)).unwrap_or(i32::MAX),
        time_us,
    }
}

/// The `DmaDone` event for `channel`: `value` is the byte count on success
/// or the negative [`AbiError`](middleware::wasm_abi::AbiError) code.
pub fn dma_done_event(channel: DmaChannel, value: i32, time_us: u32) -> Event {
    Event { kind: EventKind::DmaDone, source: channel as i32, value, time_us }
}

/// The coalesced `UartRx` event for `port` with `buffered` bytes waiting.
pub fn uart_rx_event(port: u8, buffered: usize, time_us: u32) -> Event {
    Event {
        kind: EventKind::UartRx,
        source: i32::from(port),
        value: i32::try_from(buffered).unwrap_or(i32::MAX),
        time_us,
    }
}

/// Decides whether the controller logs a queue overflow: `logged` is the
/// overflow count already reported, `current` the queue's count now.
/// Returns the number of newly dropped events, if any.
pub const fn overflow_report(logged: u32, current: u32) -> Option<u32> {
    if current > logged { Some(current - logged) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::wasm_abi::AbiError;
    use middleware::wasm_abi::events::{EventQueue, MAX_EVENTS, Pushed};

    #[test]
    fn d5_gpio_event_respects_edge() {
        let e = gpio_event(4, Edge::Rising, true, 77).unwrap();
        assert_eq!(e.to_args(), [EventKind::Gpio as i32, 4, 1, 77]);
        assert_eq!(gpio_event(4, Edge::Rising, false, 0), None, "bounce read low");
        assert_eq!(gpio_event(4, Edge::Falling, false, 5).map(|e| e.value), Some(0));
        assert!(gpio_event(4, Edge::Any, false, 0).is_some());
        assert!(gpio_event(4, Edge::Any, true, 0).is_some());
        assert_eq!(gpio_event(4, Edge::Off, true, 0), None);
    }

    #[test]
    fn d5_overflow_is_reported_once_per_rise() {
        assert_eq!(overflow_report(0, 0), None);
        assert_eq!(overflow_report(0, 3), Some(3));
        assert_eq!(overflow_report(3, 3), None);
        assert_eq!(overflow_report(3, 5), Some(2));
        // After the payload reads (and resets) the count, nothing is owed.
        assert_eq!(overflow_report(5, 0), None);
    }

    #[test]
    fn d5_isr_pushes_are_bounded_and_counted() {
        // What the ISRs do with a full queue: drop, count, never block.
        let mut q: EventQueue<MAX_EVENTS> = EventQueue::new();
        for i in 0..MAX_EVENTS as u32 + 5 {
            q.push(gpio_event(2, Edge::Any, i % 2 == 0, i).unwrap());
        }
        assert_eq!(q.len(), MAX_EVENTS);
        assert_eq!(overflow_report(0, q.overflow()), Some(5));
        // Timer events still coalesce into the slot already queued for them.
        let mut q: EventQueue<2> = EventQueue::new();
        assert_eq!(q.push(timer_event(1, 1, 0)), Pushed::Queued);
        assert_eq!(q.push(timer_event(1, 3, 10)), Pushed::Coalesced);
        assert_eq!(q.pop().map(|e| e.value), Some(4));
    }

    #[test]
    fn d6_d7_completion_events() {
        let ok = dma_done_event(DmaChannel::Spi0, 16, 9);
        assert_eq!(ok.to_args(), [EventKind::DmaDone as i32, 0, 16, 9]);
        let err = dma_done_event(DmaChannel::Uart1Tx, AbiError::Transport.code(), 1);
        assert_eq!((err.source, err.value), (1, -5));
        let rx = uart_rx_event(1, 2000, 3);
        assert_eq!((rx.kind, rx.source, rx.value), (EventKind::UartRx, 1, 2000));
        assert_eq!(timer_event(0, 0, 0).value, 1, "at least one period");
        assert_eq!(timer_event(0, u32::MAX, 0).value, i32::MAX);
    }
}
