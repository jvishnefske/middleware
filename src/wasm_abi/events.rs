//! Interrupt events and the bounded queue between ISRs and the payload.
//!
//! An ISR builds an [`Event`] (kind, source, value, timestamp) and pushes it
//! into an [`EventQueue`]; the firmware main loop pops events and calls the
//! payload's `on_irq(kind, source, value, time_us)` export with
//! [`Event::to_args`].
//!
//! The queue is a fixed ring of `N` entries. Pushing never blocks and never
//! allocates:
//!
//! * [`EventKind::Timer`], [`EventKind::UartRx`] and (ABI 4)
//!   [`EventKind::RouteFrames`] and [`EventKind::RouteDropped`] coalesce
//!   with a pending event from the same source. Timer and route events add
//!   their `value` to the pending event's (missed periods, frames); UART RX
//!   replaces `value` with the latest byte count. A fast periodic source
//!   therefore occupies one slot.
//! * Other kinds take a new slot; when the ring is full the event is dropped
//!   and [`EventQueue::overflow`] counts it.
//!
//! Every operation scans at most `N` entries, so ISR time is bounded. The
//! queue itself is not synchronised; the firmware wraps it in a critical
//! section shared with the ISR.

/// Queue depth used by the firmware and simulators.
pub const MAX_EVENTS: usize = 32;

/// What raised the event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum EventKind {
    /// GPIO edge: `source` = pin, `value` = level after the edge (0/1).
    Gpio = 1,
    /// Hardware timer alarm: `source` = timer id, `value` = periods elapsed
    /// since the payload last saw this timer (≥ 1; > 1 means coalesced).
    Timer = 2,
    /// DMA transfer finished: `source` = [`DmaChannel`] code, `value` =
    /// bytes transferred, or a negative [`AbiError`](super::AbiError) code.
    DmaDone = 3,
    /// Bytes received on a secondary UART: `source` = port, `value` = bytes
    /// now buffered (read them with `uart_read`).
    UartRx = 4,
    /// Frames a DMA route completed (ABI 4): `source` = route handle,
    /// `value` = frames since the payload last saw this event (≥ 1). A sink
    /// route counts frames fully sent to its bus, a source route frames fully
    /// published.
    RouteFrames = 5,
    /// Frames a DMA route lost (ABI 4): `source` = route handle, `value` =
    /// frames since the payload last saw this event (≥ 1). A sink route
    /// counts frames abandoned after a missing chunk, a source route frames
    /// skipped (capture overrun, failed publish).
    RouteDropped = 6,
    /// A DMA route stopped by itself (ABI 4): `source` = route handle,
    /// `value` = the negative [`AbiError`](super::AbiError) code of the
    /// fault. The handle is free afterwards. `route_stop` raises no event.
    RouteStopped = 7,
}

impl EventKind {
    /// Parses the `kind` argument of `on_irq`.
    pub const fn from_i32(v: i32) -> Option<Self> {
        match v {
            1 => Some(EventKind::Gpio),
            2 => Some(EventKind::Timer),
            3 => Some(EventKind::DmaDone),
            4 => Some(EventKind::UartRx),
            5 => Some(EventKind::RouteFrames),
            6 => Some(EventKind::RouteDropped),
            7 => Some(EventKind::RouteStopped),
            _ => None,
        }
    }

    const fn coalesces(self) -> bool {
        matches!(
            self,
            EventKind::Timer | EventKind::UartRx | EventKind::RouteFrames | EventKind::RouteDropped
        )
    }

    /// Whether coalescing adds `value`s (counts) rather than replacing them.
    const fn sums(self) -> bool {
        matches!(
            self,
            EventKind::Timer | EventKind::RouteFrames | EventKind::RouteDropped
        )
    }
}

/// The event kinds a DMA route raises (ABI 4), for purging all of one
/// route's pending events when it stops.
pub const ROUTE_EVENTS: [EventKind; 3] = [
    EventKind::RouteFrames,
    EventKind::RouteDropped,
    EventKind::RouteStopped,
];

/// DMA channels a payload can use, as the `source` of a
/// [`EventKind::DmaDone`] event and the `channel` argument of `dma_read`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum DmaChannel {
    /// SPI bus 0 (full duplex; receive data via `dma_read`).
    Spi0 = 0,
    /// Secondary UART 1 transmit (no receive data).
    Uart1Tx = 1,
}

impl DmaChannel {
    /// Parses a channel code.
    pub const fn from_i32(v: i32) -> Option<Self> {
        match v {
            0 => Some(DmaChannel::Spi0),
            1 => Some(DmaChannel::Uart1Tx),
            _ => None,
        }
    }
}

/// One interrupt event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Event {
    /// Event kind.
    pub kind: EventKind,
    /// Pin, timer id, DMA channel, UART port or route handle.
    pub source: i32,
    /// Kind-specific value (see [`EventKind`]).
    pub value: i32,
    /// Microseconds since boot when the ISR ran (wraps).
    pub time_us: u32,
}

impl Event {
    /// The four `on_irq` arguments: `(kind, source, value, time_us)`.
    pub const fn to_args(self) -> [i32; 4] {
        [
            self.kind as i32,
            self.source,
            self.value,
            self.time_us as i32,
        ]
    }

    /// Inverse of [`Event::to_args`].
    pub const fn from_args(args: [i32; 4]) -> Option<Self> {
        match EventKind::from_i32(args[0]) {
            Some(kind) => Some(Event {
                kind,
                source: args[1],
                value: args[2],
                time_us: args[3] as u32,
            }),
            None => None,
        }
    }

    const fn same_source(&self, other: &Event) -> bool {
        self.kind as i32 == other.kind as i32 && self.source == other.source
    }
}

/// Outcome of [`EventQueue::push`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pushed {
    /// Stored in a new slot.
    Queued,
    /// Merged into a pending event from the same source.
    Coalesced,
    /// The queue was full; the event was dropped and counted.
    Dropped,
}

/// Fixed-capacity FIFO of events with coalescing and an overflow count.
///
/// Mutated in place: it lives in a critical-section cell shared with ISRs,
/// where moving `N` entries per event would lengthen interrupt latency.
#[derive(Debug, Clone)]
pub struct EventQueue<const N: usize> {
    slots: [Option<Event>; N],
    head: usize,
    len: usize,
    overflow: u32,
}

impl<const N: usize> Default for EventQueue<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> EventQueue<N> {
    /// An empty queue.
    pub const fn new() -> Self {
        Self {
            slots: [None; N],
            head: 0,
            len: 0,
            overflow: 0,
        }
    }

    /// Events waiting.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether no events are waiting.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn index(&self, offset: usize) -> usize {
        if N == 0 { 0 } else { (self.head + offset) % N }
    }

    /// Adds `event`; see the module docs for coalescing and overflow.
    pub fn push(&mut self, event: Event) -> Pushed {
        if event.kind.coalesces() {
            for offset in 0..self.len {
                let i = self.index(offset);
                if let Some(Some(pending)) = self.slots.get_mut(i)
                    && pending.same_source(&event)
                {
                    pending.value = if event.kind.sums() {
                        pending.value.saturating_add(event.value.max(1))
                    } else {
                        event.value
                    };
                    pending.time_us = event.time_us;
                    return Pushed::Coalesced;
                }
            }
        }
        if self.len >= N {
            self.overflow = self.overflow.saturating_add(1);
            return Pushed::Dropped;
        }
        let tail = self.index(self.len);
        match self.slots.get_mut(tail) {
            Some(slot) => {
                *slot = Some(event);
                self.len += 1;
                Pushed::Queued
            }
            None => {
                self.overflow = self.overflow.saturating_add(1);
                Pushed::Dropped
            }
        }
    }

    /// Removes and returns the oldest event.
    pub fn pop(&mut self) -> Option<Event> {
        if self.len == 0 {
            return None;
        }
        let event = self.slots.get_mut(self.head).and_then(Option::take);
        self.head = self.index(1);
        self.len -= 1;
        event
    }

    /// Events dropped since the last call; resets the count. Backs the
    /// `irq_overflow` import.
    pub fn take_overflow(&mut self) -> u32 {
        core::mem::take(&mut self.overflow)
    }

    /// Events dropped since the last [`EventQueue::take_overflow`].
    pub const fn overflow(&self) -> u32 {
        self.overflow
    }

    /// Discards all pending events of `kind` from `source` (e.g. when a
    /// timer is stopped or a pin's interrupt is disabled). Keeps order.
    pub fn purge(&mut self, kind: EventKind, source: i32) {
        let mut kept = Self::new();
        kept.overflow = self.overflow;
        while let Some(e) = self.pop() {
            if !(e.kind == kind && e.source == source) {
                kept.push_raw(e);
            }
        }
        *self = kept;
    }

    fn push_raw(&mut self, event: Event) {
        let tail = self.index(self.len);
        if let Some(slot) = self.slots.get_mut(tail) {
            *slot = Some(event);
            self.len += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: EventKind, source: i32, value: i32, t: u32) -> Event {
        Event {
            kind,
            source,
            value,
            time_us: t,
        }
    }

    #[test]
    fn fifo_order_and_args_round_trip() {
        let mut q: EventQueue<4> = EventQueue::new();
        let a = ev(EventKind::Gpio, 4, 1, 10);
        let b = ev(EventKind::DmaDone, DmaChannel::Spi0 as i32, 16, 20);
        assert_eq!(q.push(a), Pushed::Queued);
        assert_eq!(q.push(b), Pushed::Queued);
        assert_eq!(q.pop(), Some(a));
        assert_eq!(Event::from_args(b.to_args()), Some(b));
        assert_eq!(q.pop(), Some(b));
        assert_eq!(q.pop(), None);
        assert_eq!(Event::from_args([9, 0, 0, 0]), None);
    }

    #[test]
    fn timers_and_uart_rx_coalesce() {
        let mut q: EventQueue<4> = EventQueue::new();
        q.push(ev(EventKind::Timer, 0, 1, 100));
        q.push(ev(EventKind::Gpio, 2, 0, 150));
        assert_eq!(q.push(ev(EventKind::Timer, 0, 1, 200)), Pushed::Coalesced);
        assert_eq!(q.push(ev(EventKind::Timer, 1, 1, 210)), Pushed::Queued);
        q.push(ev(EventKind::UartRx, 1, 5, 220));
        assert_eq!(q.push(ev(EventKind::UartRx, 1, 9, 230)), Pushed::Coalesced);
        assert_eq!(q.pop(), Some(ev(EventKind::Timer, 0, 2, 200)));
        q.pop();
        q.pop();
        assert_eq!(q.pop(), Some(ev(EventKind::UartRx, 1, 9, 230)));
    }

    #[test]
    fn full_queue_drops_and_counts_without_blocking() {
        let mut q: EventQueue<2> = EventQueue::new();
        q.push(ev(EventKind::Gpio, 1, 1, 0));
        q.push(ev(EventKind::Gpio, 1, 0, 1));
        assert_eq!(q.push(ev(EventKind::Gpio, 1, 1, 2)), Pushed::Dropped);
        assert_eq!(q.push(ev(EventKind::DmaDone, 0, 4, 3)), Pushed::Dropped);
        assert_eq!(q.take_overflow(), 2);
        assert_eq!(q.take_overflow(), 0);
        // Wraps around after draining.
        q.pop();
        assert_eq!(q.push(ev(EventKind::Gpio, 2, 1, 4)), Pushed::Queued);
        assert_eq!(q.pop().map(|e| e.time_us), Some(1));
        assert_eq!(q.pop().map(|e| e.time_us), Some(4));
        assert!(q.is_empty());
    }

    #[test]
    fn purge_removes_one_source_and_keeps_order() {
        let mut q: EventQueue<8> = EventQueue::new();
        q.push(ev(EventKind::Timer, 0, 1, 1));
        q.push(ev(EventKind::Gpio, 3, 1, 2));
        q.push(ev(EventKind::Timer, 1, 1, 3));
        q.push(ev(EventKind::Gpio, 3, 0, 4));
        q.purge(EventKind::Gpio, 3);
        assert_eq!(q.len(), 2);
        assert_eq!(q.pop().map(|e| e.source), Some(0));
        assert_eq!(q.pop().map(|e| e.source), Some(1));
    }

    #[test]
    fn route_events_coalesce_per_handle_and_stops_do_not() {
        let mut q: EventQueue<8> = EventQueue::new();
        q.push(ev(EventKind::RouteFrames, 0, 1, 10));
        assert_eq!(
            q.push(ev(EventKind::RouteFrames, 0, 2, 20)),
            Pushed::Coalesced
        );
        assert_eq!(q.push(ev(EventKind::RouteFrames, 1, 1, 21)), Pushed::Queued);
        q.push(ev(EventKind::RouteDropped, 0, 1, 30));
        assert_eq!(
            q.push(ev(EventKind::RouteDropped, 0, 1, 40)),
            Pushed::Coalesced
        );
        let stop = ev(EventKind::RouteStopped, 0, -5, 50);
        assert_eq!(q.push(stop), Pushed::Queued);
        assert_eq!(q.push(stop), Pushed::Queued);
        assert_eq!(q.pop(), Some(ev(EventKind::RouteFrames, 0, 3, 20)));
        assert_eq!(q.pop().map(|e| e.source), Some(1));
        assert_eq!(q.pop(), Some(ev(EventKind::RouteDropped, 0, 2, 40)));
        q.purge(EventKind::RouteStopped, 0);
        assert!(q.is_empty());
        for k in 5..=7 {
            assert_eq!(EventKind::from_i32(k).map(|e| e as i32), Some(k));
        }
        assert_eq!(EventKind::from_i32(8), None);
    }

    #[test]
    fn zero_capacity_queue_drops_everything() {
        let mut q: EventQueue<0> = EventQueue::new();
        assert_eq!(q.push(ev(EventKind::Gpio, 1, 1, 0)), Pushed::Dropped);
        assert_eq!(q.pop(), None);
    }
}
