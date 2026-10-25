//! Virtual ABI 2 peripherals behind [`super::SimDevice`]: the interrupt
//! event queue, GPIO edge detection, hardware timers, an SPI device and a
//! secondary UART.
//!
//! [`Peripherals`] is a plain value driven by explicit time arguments
//! (microseconds since load, as a non-wrapping `u64`), so it can be tested
//! natively and in virtual time. The JS side supplies the clock.
//!
//! * **Queue.** Every interrupt source pushes into one
//!   [`EventQueue<MAX_EVENTS>`], with the coalescing and drop-and-count
//!   rules of [`middleware::wasm_abi::events`]. [`Peripherals::take_overflow`]
//!   backs the `irq_overflow` import; [`Peripherals::dropped`] keeps a
//!   running total for the UI that the payload's reads do not reset.
//! * **GPIO.** [`Peripherals::set_edge`] arms a pin; [`Peripherals::level_changed`]
//!   raises a [`EventKind::Gpio`] event when a level change matches the
//!   armed [`Edge`]. Disabling (or reconfiguring the pin) purges its pending
//!   events.
//! * **Timers.** Ids `0..MAX_TIMERS`, each one-shot or periodic.
//!   [`Peripherals::advance`] raises at most one event per timer per call,
//!   whose `value` is the number of periods that elapsed (so a large time
//!   step costs O(1), and matches what coalescing would produce). Due timers
//!   are queued in order of their first missed deadline. Starting or
//!   stopping a timer purges its pending events.
//! * **SPI.** Bus 0 talks to a virtual device that answers with the bytes it
//!   received ([`SpiResponder::Loopback`], the default) or with a repeating
//!   pattern ([`SpiResponder::Pattern`]). A transfer is clocked
//!   immediately, but its [`EventKind::DmaDone`] arrives on the next
//!   [`Peripherals::complete_dma`] (the simulator's next step); `dma_read`
//!   before that is [`AbiError::Busy`]. `dma_read` consumes the received
//!   bytes: a second read returns 0 until the next transfer. Reading
//!   [`DmaChannel::Uart1Tx`] (transmit only) returns 0.
//! * **UART 1.** `uart_write` hands the bytes to the UI at once and raises
//!   `DmaDone(Uart1Tx)` on the next step. Bytes injected from the UI are
//!   buffered up to [`UART_RX_BUFFER`] (excess dropped) and raise a
//!   coalescing [`EventKind::UartRx`] carrying the buffered count.
//!
//! Using SPI or UART before opening it, or starting a second transfer while
//! one is in flight, is [`AbiError::Busy`]; `uart_write` on a port opened
//! without a TX pin is [`AbiError::Unsupported`]. These semantics match the
//! desktop simulator (`tools/payload-sim`).

use std::collections::VecDeque;

use middleware::wasm_abi::AbiError;
use middleware::wasm_abi::events::{DmaChannel, Event, EventKind, EventQueue, MAX_EVENTS, Pushed};
use middleware::wasm_abi::periph::{
    Edge, MAX_TIMERS, SpiConfig, TimerSpec, UART_RX_BUFFER, UartConfig,
};

use super::PIN_COUNT;

/// What the virtual SPI device answers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SpiResponder {
    /// Echo the transmitted bytes (MOSI wired to MISO).
    #[default]
    Loopback,
    /// Repeat this non-empty pattern for as many bytes as are clocked.
    Pattern(Vec<u8>),
}

impl SpiResponder {
    /// A responder from a pattern; an empty pattern means loopback.
    pub fn from_pattern(pattern: &[u8]) -> Self {
        if pattern.is_empty() {
            SpiResponder::Loopback
        } else {
            SpiResponder::Pattern(pattern.to_vec())
        }
    }

    /// The bytes received while clocking out `tx`.
    pub fn respond(&self, tx: &[u8]) -> Vec<u8> {
        match self {
            SpiResponder::Loopback => tx.to_vec(),
            SpiResponder::Pattern(p) => p.iter().copied().cycle().take(tx.len()).collect(),
        }
    }
}

/// A completed SPI transfer, for the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpiTransfer {
    /// Bytes clocked out (padded with `0x00` to the transfer length).
    pub tx: Vec<u8>,
    /// Bytes clocked in.
    pub rx: Vec<u8>,
}

/// One timer, for the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimerInfo {
    /// Timer id.
    pub id: u8,
    /// Last configured period.
    pub period_us: u32,
    /// Whether it re-arms.
    pub periodic: bool,
    /// Whether it is armed now.
    pub active: bool,
    /// Periods elapsed since load (across restarts).
    pub fires: u32,
}

#[derive(Debug, Clone, Copy, Default)]
struct TimerSlot {
    spec: Option<TimerSpec>,
    due: Option<u64>,
    fires: u32,
}

/// The ABI 2 peripheral state of the virtual device.
#[derive(Debug, Clone)]
pub struct Peripherals {
    queue: EventQueue<MAX_EVENTS>,
    dropped: u32,
    edges: [Edge; PIN_COUNT],
    timers: [TimerSlot; MAX_TIMERS],
    spi: Option<SpiConfig>,
    spi_pending: Option<usize>,
    spi_rx: Vec<u8>,
    responder: SpiResponder,
    uart: Option<UartConfig>,
    uart_pending: Option<usize>,
    uart_rx: VecDeque<u8>,
}

impl Default for Peripherals {
    fn default() -> Self {
        Self::new()
    }
}

/// Event timestamps are the low 32 bits of the clock, wrapping like the
/// device's `now_us`.
const fn stamp(now: u64) -> u32 {
    now as u32
}

const fn saturate(n: u64) -> i32 {
    if n > i32::MAX as u64 {
        i32::MAX
    } else {
        n as i32
    }
}

impl Peripherals {
    /// Everything idle, nothing armed or open, loopback SPI.
    pub fn new() -> Self {
        Self {
            queue: EventQueue::new(),
            dropped: 0,
            edges: [Edge::Off; PIN_COUNT],
            timers: [TimerSlot::default(); MAX_TIMERS],
            spi: None,
            spi_pending: None,
            spi_rx: Vec::new(),
            responder: SpiResponder::Loopback,
            uart: None,
            uart_pending: None,
            uart_rx: VecDeque::new(),
        }
    }

    // ---- queue

    /// Pushes an event, counting drops.
    pub fn push(&mut self, event: Event) -> Pushed {
        let r = self.queue.push(event);
        if r == Pushed::Dropped {
            self.dropped = self.dropped.saturating_add(1);
        }
        r
    }

    /// Removes the oldest pending event.
    pub fn pop(&mut self) -> Option<Event> {
        self.queue.pop()
    }

    /// Events waiting for dispatch.
    pub fn pending(&self) -> usize {
        self.queue.len()
    }

    /// Drops since the last call (the `irq_overflow` import).
    pub fn take_overflow(&mut self) -> u32 {
        self.queue.take_overflow()
    }

    /// Drops since load; not reset by [`Peripherals::take_overflow`].
    pub const fn dropped(&self) -> u32 {
        self.dropped
    }

    // ---- GPIO

    /// Arms (or with [`Edge::Off`] disarms and purges) `pin`.
    pub fn set_edge(&mut self, pin: u8, edge: Edge) -> Result<(), AbiError> {
        let slot = self
            .edges
            .get_mut(usize::from(pin))
            .ok_or(AbiError::InvalidArgument)?;
        *slot = edge;
        if edge == Edge::Off {
            self.queue.purge(EventKind::Gpio, i32::from(pin));
        }
        Ok(())
    }

    /// The edge armed on `pin` (`Off` for unknown pins).
    pub fn edge(&self, pin: u8) -> Edge {
        self.edges
            .get(usize::from(pin))
            .copied()
            .unwrap_or(Edge::Off)
    }

    /// Records a level change on `pin`; queues a GPIO event if the armed
    /// edge matches. `None` when nothing was queued.
    pub fn level_changed(&mut self, pin: u8, high: bool, now: u64) -> Option<Pushed> {
        if !self.edge(pin).matches(high) {
            return None;
        }
        Some(self.push(Event {
            kind: EventKind::Gpio,
            source: i32::from(pin),
            value: i32::from(high),
            time_us: stamp(now),
        }))
    }

    // ---- timers

    /// Arms (or re-arms) a timer and purges its pending events; its first
    /// alarm is one period after `now`.
    pub fn timer_start(&mut self, spec: TimerSpec, now: u64) -> Result<(), AbiError> {
        let slot = self
            .timers
            .get_mut(usize::from(spec.id))
            .ok_or(AbiError::InvalidArgument)?;
        slot.spec = Some(spec);
        slot.due = Some(now.saturating_add(u64::from(spec.period_us)));
        self.queue.purge(EventKind::Timer, i32::from(spec.id));
        Ok(())
    }

    /// Disarms a timer (idempotent) and purges its pending events.
    pub fn timer_stop(&mut self, id: u8) -> Result<(), AbiError> {
        let slot = self
            .timers
            .get_mut(usize::from(id))
            .ok_or(AbiError::InvalidArgument)?;
        slot.due = None;
        self.queue.purge(EventKind::Timer, i32::from(id));
        Ok(())
    }

    /// Raises the alarms due at `now`: one event per due timer whose value
    /// counts the elapsed periods, queued in deadline order.
    pub fn advance(&mut self, now: u64) {
        // (first missed deadline, id, periods, last deadline)
        let mut due: Vec<(u64, u8, u64, u64)> = Vec::with_capacity(MAX_TIMERS);
        for (id, slot) in self.timers.iter_mut().enumerate() {
            let (Some(spec), Some(first)) = (slot.spec, slot.due) else {
                continue;
            };
            if first > now {
                continue;
            }
            let period = u64::from(spec.period_us.max(1));
            let n = if spec.periodic {
                (now - first) / period + 1
            } else {
                1
            };
            let last = first + (n - 1) * period;
            slot.due = spec.periodic.then(|| last + period);
            slot.fires = slot
                .fires
                .saturating_add(u32::try_from(n).unwrap_or(u32::MAX));
            due.push((first, id as u8, n, last));
        }
        due.sort_unstable();
        for (_, id, n, last) in due {
            self.push(Event {
                kind: EventKind::Timer,
                source: i32::from(id),
                value: saturate(n),
                time_us: stamp(last),
            });
        }
    }

    /// The earliest armed deadline.
    pub fn next_deadline(&self) -> Option<u64> {
        self.timers.iter().filter_map(|t| t.due).min()
    }

    /// Every timer that has been started, by id.
    pub fn timers(&self) -> Vec<TimerInfo> {
        self.timers
            .iter()
            .enumerate()
            .filter_map(|(id, t)| {
                t.spec.map(|s| TimerInfo {
                    id: id as u8,
                    period_us: s.period_us,
                    periodic: s.periodic,
                    active: t.due.is_some(),
                    fires: t.fires,
                })
            })
            .collect()
    }

    // ---- DMA

    /// Completes the transfers started before this step: raises
    /// `DmaDone(Spi0)` and `DmaDone(Uart1Tx)` with the byte counts.
    pub fn complete_dma(&mut self, now: u64) {
        for (pending, channel) in [
            (self.spi_pending.take(), DmaChannel::Spi0),
            (self.uart_pending.take(), DmaChannel::Uart1Tx),
        ] {
            if let Some(len) = pending {
                self.push(Event {
                    kind: EventKind::DmaDone,
                    source: channel as i32,
                    value: saturate(len as u64),
                    time_us: stamp(now),
                });
            }
        }
    }

    /// Moves received data of a finished transfer into `out`; the bytes
    /// are consumed.
    pub fn dma_read(&mut self, channel: DmaChannel, out: &mut [u8]) -> Result<usize, AbiError> {
        match channel {
            DmaChannel::Spi0 => {
                if self.spi_pending.is_some() {
                    return Err(AbiError::Busy);
                }
                let n = out.len().min(self.spi_rx.len());
                out.iter_mut()
                    .zip(self.spi_rx.drain(..n))
                    .for_each(|(o, b)| *o = b);
                self.spi_rx.clear();
                Ok(n)
            }
            // Transmit only: there is nothing to read back.
            DmaChannel::Uart1Tx => Ok(0),
        }
    }

    // ---- SPI

    /// Opens (or reconfigures) bus 0.
    pub fn spi_open(&mut self, cfg: SpiConfig) -> Result<(), AbiError> {
        if self.spi_pending.is_some() {
            return Err(AbiError::Busy);
        }
        self.spi = Some(cfg);
        Ok(())
    }

    /// The open bus configuration.
    pub const fn spi_config(&self) -> Option<SpiConfig> {
        self.spi
    }

    /// Clocks `len` bytes (`tx` padded with `0x00`) through the virtual
    /// device. Completion is reported on the next [`Peripherals::complete_dma`].
    pub fn spi_transfer(
        &mut self,
        bus: u8,
        tx: &[u8],
        len: usize,
    ) -> Result<SpiTransfer, AbiError> {
        match self.spi {
            Some(cfg) if cfg.bus == bus => {}
            _ => return Err(AbiError::Busy),
        }
        if self.spi_pending.is_some() {
            return Err(AbiError::Busy);
        }
        let mut out = tx.get(..len.min(tx.len())).unwrap_or(&[]).to_vec();
        out.resize(len, 0);
        let rx = self.responder.respond(&out);
        self.spi_rx.clone_from(&rx);
        self.spi_pending = Some(len);
        Ok(SpiTransfer { tx: out, rx })
    }

    /// Sets what the virtual SPI device answers.
    pub fn set_responder(&mut self, responder: SpiResponder) {
        self.responder = responder;
    }

    /// What the virtual SPI device answers.
    pub fn responder(&self) -> &SpiResponder {
        &self.responder
    }

    // ---- UART

    /// Opens (or reconfigures) the secondary UART.
    pub fn uart_open(&mut self, cfg: UartConfig) -> Result<(), AbiError> {
        if self.uart_pending.is_some() {
            return Err(AbiError::Busy);
        }
        self.uart = Some(cfg);
        Ok(())
    }

    /// The open UART configuration.
    pub const fn uart_config(&self) -> Option<UartConfig> {
        self.uart
    }

    fn open_port(&self, port: u8) -> Result<UartConfig, AbiError> {
        self.uart.filter(|c| c.port == port).ok_or(AbiError::Busy)
    }

    /// Starts a transmit; the caller hands `data` to the UI.
    pub fn uart_write(&mut self, port: u8, data: &[u8]) -> Result<(), AbiError> {
        let cfg = self.open_port(port)?;
        if cfg.tx.is_none() {
            return Err(AbiError::Unsupported);
        }
        if self.uart_pending.is_some() {
            return Err(AbiError::Busy);
        }
        self.uart_pending = Some(data.len());
        Ok(())
    }

    /// Moves buffered received bytes into `out`.
    pub fn uart_read(&mut self, port: u8, out: &mut [u8]) -> Result<usize, AbiError> {
        self.open_port(port)?;
        let n = out.len().min(self.uart_rx.len());
        for (o, b) in out.iter_mut().zip(self.uart_rx.drain(..n)) {
            *o = b;
        }
        Ok(n)
    }

    /// Bytes arriving on the UART RX pin. Returns how many were buffered
    /// (0 when the port is closed or has no RX pin); raises `UartRx` when
    /// any were.
    pub fn inject_rx(&mut self, data: &[u8], now: u64) -> usize {
        let Some(cfg) = self.uart.filter(|c| c.rx.is_some()) else {
            return 0;
        };
        let room = UART_RX_BUFFER.saturating_sub(self.uart_rx.len());
        let n = data.len().min(room);
        self.uart_rx.extend(data.iter().take(n));
        if n > 0 {
            self.push(Event {
                kind: EventKind::UartRx,
                source: i32::from(cfg.port),
                value: saturate(self.uart_rx.len() as u64),
                time_us: stamp(now),
            });
        }
        n
    }

    /// Received bytes waiting for `uart_read`.
    pub fn uart_buffered(&self) -> usize {
        self.uart_rx.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::wasm_abi::periph::{MAX_DMA, UART_PORT};

    fn spi_cfg() -> SpiConfig {
        SpiConfig::parse(0, 6, 7, 2, 10, 1000, 0).unwrap()
    }

    fn uart_cfg() -> UartConfig {
        UartConfig::parse(UART_PORT, 4, 5, 115_200).unwrap()
    }

    fn timer(id: i32, period: i32, periodic: i32) -> TimerSpec {
        TimerSpec::parse(id, period, periodic).unwrap()
    }

    #[test]
    fn d10_gpio_edges_follow_the_armed_edge_and_purge_when_disarmed() {
        let mut p = Peripherals::new();
        assert_eq!(p.level_changed(4, true, 1), None, "not armed");
        p.set_edge(4, Edge::Falling).unwrap();
        assert_eq!(p.level_changed(4, true, 2), None);
        assert_eq!(p.level_changed(4, false, 3), Some(Pushed::Queued));
        p.set_edge(5, Edge::Any).unwrap();
        p.level_changed(5, true, 4);
        p.set_edge(4, Edge::Off).unwrap();
        let e = p.pop().unwrap();
        assert_eq!(
            (e.kind, e.source, e.value, e.time_us),
            (EventKind::Gpio, 5, 1, 4)
        );
        assert_eq!(p.pop(), None);
        assert_eq!(
            p.set_edge(PIN_COUNT as u8, Edge::Any),
            Err(AbiError::InvalidArgument)
        );
    }

    #[test]
    fn d10_periodic_timer_counts_elapsed_periods_in_one_event() {
        let mut p = Peripherals::new();
        p.timer_start(timer(1, 1000, 1), 0).unwrap();
        p.advance(999);
        assert_eq!(p.pending(), 0);
        p.advance(1000);
        assert_eq!(
            p.pop().map(|e| (e.source, e.value, e.time_us)),
            Some((1, 1, 1000))
        );
        p.advance(10_500);
        // Deadlines 2000..=10000: nine periods, stamped at the last one.
        assert_eq!(p.pop().map(|e| (e.value, e.time_us)), Some((9, 10_000)));
        assert_eq!(p.next_deadline(), Some(11_000));
        assert_eq!(
            p.timers(),
            [TimerInfo {
                id: 1,
                period_us: 1000,
                periodic: true,
                active: true,
                fires: 10
            }]
        );
    }

    #[test]
    fn d10_one_shot_timer_fires_once_and_timers_queue_in_deadline_order() {
        let mut p = Peripherals::new();
        p.timer_start(timer(0, 500, 0), 0).unwrap();
        p.timer_start(timer(3, 200, 1), 0).unwrap();
        p.advance(600);
        let order: Vec<_> = core::iter::from_fn(|| p.pop())
            .map(|e| (e.source, e.value))
            .collect();
        assert_eq!(order, [(3, 3), (0, 1)]);
        p.advance(5000);
        assert!(core::iter::from_fn(|| p.pop()).all(|e| e.source == 3));
        assert!(!p.timers()[0].active);
    }

    #[test]
    fn d10_starting_or_stopping_a_timer_purges_its_events() {
        let mut p = Peripherals::new();
        p.timer_start(timer(2, 100, 1), 0).unwrap();
        p.advance(1000);
        assert_eq!(p.pending(), 1);
        p.timer_stop(2).unwrap();
        assert_eq!(p.pending(), 0);
        p.advance(5000);
        assert_eq!(p.pending(), 0);
        assert_eq!(p.next_deadline(), None);
        assert_eq!(p.timers()[0].fires, 10);
        assert!(p.timer_stop(2).is_ok(), "idempotent");
        p.timer_start(timer(2, 100, 1), 5000).unwrap();
        p.advance(6000);
        assert_eq!(p.pending(), 1);
        p.timer_start(timer(2, 100, 1), 6000).unwrap();
        assert_eq!(p.pending(), 0, "restart purges too");
    }

    #[test]
    fn d10_spi_loopback_completes_on_next_step_and_reads_back() {
        let mut p = Peripherals::new();
        assert_eq!(
            p.spi_transfer(0, &[1], 1),
            Err(AbiError::Busy),
            "not opened"
        );
        p.spi_open(spi_cfg()).unwrap();
        let t = p.spi_transfer(0, &[0xa5, 0x5a], 4).unwrap();
        assert_eq!(
            t,
            SpiTransfer {
                tx: vec![0xa5, 0x5a, 0, 0],
                rx: vec![0xa5, 0x5a, 0, 0]
            }
        );
        let mut buf = [0u8; 8];
        assert_eq!(p.dma_read(DmaChannel::Spi0, &mut buf), Err(AbiError::Busy));
        assert_eq!(p.spi_transfer(0, &[1], 1), Err(AbiError::Busy), "in flight");
        assert_eq!(p.pending(), 0);
        p.complete_dma(77);
        let e = p.pop().unwrap();
        assert_eq!(
            (e.kind, e.source, e.value, e.time_us),
            (EventKind::DmaDone, 0, 4, 77)
        );
        assert_eq!(p.dma_read(DmaChannel::Spi0, &mut buf), Ok(4));
        assert_eq!(&buf[..4], &[0xa5, 0x5a, 0, 0]);
        assert_eq!(p.dma_read(DmaChannel::Spi0, &mut buf), Ok(0), "consumed");
        assert_eq!(
            p.dma_read(DmaChannel::Uart1Tx, &mut buf),
            Ok(0),
            "transmit only"
        );
    }

    #[test]
    fn d10_spi_pattern_responder_repeats() {
        let mut p = Peripherals::new();
        p.spi_open(spi_cfg()).unwrap();
        p.set_responder(SpiResponder::from_pattern(&[1, 2, 3]));
        assert_eq!(p.spi_transfer(0, &[], 5).unwrap().rx, [1, 2, 3, 1, 2]);
        assert_eq!(SpiResponder::from_pattern(&[]), SpiResponder::Loopback);
        let big = vec![7u8; MAX_DMA];
        p.complete_dma(0);
        assert_eq!(p.spi_transfer(0, &big, MAX_DMA).unwrap().tx.len(), MAX_DMA);
    }

    #[test]
    fn d10_uart_write_completes_next_step_and_rx_is_buffered_and_coalesced() {
        let mut p = Peripherals::new();
        assert_eq!(p.uart_write(1, b"hi"), Err(AbiError::Busy), "not opened");
        assert_eq!(p.inject_rx(b"x", 0), 0, "closed port drops input");
        p.uart_open(uart_cfg()).unwrap();
        p.uart_write(1, b"hi").unwrap();
        assert_eq!(p.uart_write(1, b"again"), Err(AbiError::Busy));
        p.complete_dma(5);
        let e = p.pop().unwrap();
        assert_eq!(
            (e.kind, e.source, e.value),
            (EventKind::DmaDone, DmaChannel::Uart1Tx as i32, 2)
        );
        assert_eq!(p.inject_rx(b"abc", 6), 3);
        assert_eq!(p.inject_rx(b"de", 7), 2);
        let e = p.pop().unwrap();
        assert_eq!(
            (e.kind, e.source, e.value, e.time_us),
            (EventKind::UartRx, 1, 5, 7)
        );
        assert_eq!(p.pop(), None, "coalesced");
        let mut buf = [0u8; 4];
        assert_eq!(p.uart_read(1, &mut buf), Ok(4));
        assert_eq!(&buf, b"abcd");
        assert_eq!(p.uart_buffered(), 1);
        let flood = vec![0u8; UART_RX_BUFFER + 10];
        assert_eq!(p.inject_rx(&flood, 8), UART_RX_BUFFER - 1);
        let tx_only = UartConfig::parse(UART_PORT, 4, -1, 9600).unwrap();
        p.uart_open(tx_only).unwrap();
        assert_eq!(p.inject_rx(b"x", 9), 0, "no RX pin");
        let rx_only = UartConfig::parse(UART_PORT, -1, 5, 9600).unwrap();
        p.uart_open(rx_only).unwrap();
        assert_eq!(
            p.uart_write(1, b"x"),
            Err(AbiError::Unsupported),
            "no TX pin"
        );
    }

    #[test]
    fn d10_queue_overflow_is_counted_and_reported_once() {
        let mut p = Peripherals::new();
        p.set_edge(3, Edge::Any).unwrap();
        for i in 0..(MAX_EVENTS as u64 + 8) {
            p.level_changed(3, i % 2 == 0, i);
        }
        assert_eq!(p.pending(), MAX_EVENTS);
        assert_eq!(p.take_overflow(), 8);
        assert_eq!(p.take_overflow(), 0);
        assert_eq!(p.dropped(), 8, "UI total survives the payload's read");
    }
}
