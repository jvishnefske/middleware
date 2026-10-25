//! Virtual ABI 2 peripherals for the simulator (requirement D10): hardware
//! timers driven by the simulator clock, an SPI loopback device and a UART
//! terminal. Each model only decides; [`super::SimDevice`] owns them, feeds
//! their [`Event`]s into its queue and prints what they report.
//!
//! * [`Timers`] — ids `0..MAX_TIMERS`; [`Timers::fire`] raises at most one
//!   `Timer` event per timer per call, whose `value` counts every period
//!   that elapsed (so catching up after a long gap is O(1)).
//! * [`Spi`] — bus 0 loopback: received bytes equal the transmitted ones
//!   (padded with `0x00` to the transfer length) unless a pattern is set,
//!   in which case the pattern repeats. A transfer stays in flight until
//!   [`Spi::complete`] turns it into a `DmaDone` event; `dma_read` then
//!   drains the received bytes.
//! * [`Uart`] — port 1: writes stay in flight until [`Uart::complete`];
//!   received bytes are buffered up to [`UART_RX_BUFFER`] (excess dropped)
//!   and reported as a (queue-coalesced) `UartRx` event.
//!
//! Using a peripheral before opening it, or starting a transfer while one
//! is in flight, is [`AbiError::Busy`].

use std::collections::VecDeque;

use middleware::wasm_abi::periph::{MAX_TIMERS, UART_RX_BUFFER};
use wasm_host::{AbiError, DmaChannel, Event, EventKind, SpiConfig, TimerSpec, UartConfig};

/// One running timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Armed {
    spec: TimerSpec,
    due_us: u64,
}

/// The payload's hardware timers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Timers {
    slots: [Option<Armed>; MAX_TIMERS],
}

impl Timers {
    /// Starts (or restarts) `spec.id`, first due one period after `now_us`.
    pub fn start(&mut self, spec: TimerSpec, now_us: u64) -> Result<(), AbiError> {
        let slot = self
            .slots
            .get_mut(usize::from(spec.id))
            .ok_or(AbiError::InvalidArgument)?;
        *slot = Some(Armed {
            spec,
            due_us: now_us + u64::from(spec.period_us),
        });
        Ok(())
    }

    /// Stops timer `id` (a no-op if it is not running).
    pub fn stop(&mut self, id: u8) -> Result<(), AbiError> {
        let slot = self
            .slots
            .get_mut(usize::from(id))
            .ok_or(AbiError::InvalidArgument)?;
        *slot = None;
        Ok(())
    }

    /// The earliest due time of any running timer.
    pub fn next_due(&self) -> Option<u64> {
        self.slots.iter().flatten().map(|a| a.due_us).min()
    }

    /// Fires every timer due at `now_us`. A periodic timer reports how many
    /// periods elapsed and re-arms after `now_us`; a one-shot timer stops.
    pub fn fire(&mut self, now_us: u64) -> Vec<Event> {
        let mut events = Vec::new();
        for (id, slot) in self.slots.iter_mut().enumerate() {
            let Some(armed) = *slot else { continue };
            if armed.due_us > now_us {
                continue;
            }
            let period = u64::from(armed.spec.period_us.max(1));
            let (periods, last) = if armed.spec.periodic {
                let n = (now_us - armed.due_us) / period + 1;
                (n, armed.due_us + (n - 1) * period)
            } else {
                (1, armed.due_us)
            };
            *slot = armed.spec.periodic.then_some(Armed {
                spec: armed.spec,
                due_us: last + period,
            });
            events.push(Event {
                kind: EventKind::Timer,
                source: id as i32,
                value: i32::try_from(periods).unwrap_or(i32::MAX),
                time_us: last as u32,
            });
        }
        events
    }
}

/// The SPI loopback device on bus 0.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Spi {
    config: Option<SpiConfig>,
    pattern: Option<Vec<u8>>,
    in_flight: Option<Vec<u8>>,
    received: Vec<u8>,
}

impl Spi {
    /// A loopback device; with `pattern`, received bytes repeat it instead.
    pub fn new(pattern: Option<Vec<u8>>) -> Self {
        Spi {
            pattern: pattern.filter(|p| !p.is_empty()),
            ..Spi::default()
        }
    }

    /// Configures the bus (reconfiguring drops any received data).
    pub fn open(&mut self, cfg: SpiConfig) {
        self.config = Some(cfg);
        self.received.clear();
    }

    /// Whether a transfer awaits completion.
    pub fn in_flight(&self) -> bool {
        self.in_flight.is_some()
    }

    /// Starts a `len`-byte transfer of `tx` (padded with `0x00`).
    pub fn transfer(&mut self, bus: u8, tx: &[u8], len: usize) -> Result<(), AbiError> {
        match self.config {
            Some(cfg) if cfg.bus == bus && self.in_flight.is_none() => {}
            _ => return Err(AbiError::Busy),
        }
        let rx = match &self.pattern {
            Some(p) => p.iter().copied().cycle().take(len).collect(),
            None => tx
                .iter()
                .copied()
                .chain(std::iter::repeat(0))
                .take(len)
                .collect(),
        };
        self.in_flight = Some(rx);
        Ok(())
    }

    /// Finishes the transfer in flight, if any, as a `DmaDone` event.
    pub fn complete(&mut self, time_us: u32) -> Option<Event> {
        let rx = self.in_flight.take()?;
        let value = i32::try_from(rx.len()).unwrap_or(i32::MAX);
        self.received = rx;
        Some(Event {
            kind: EventKind::DmaDone,
            source: DmaChannel::Spi0 as i32,
            value,
            time_us,
        })
    }

    /// Moves received bytes into `out`; `Busy` while a transfer is in flight.
    pub fn read(&mut self, out: &mut [u8]) -> Result<usize, AbiError> {
        if self.in_flight.is_some() {
            return Err(AbiError::Busy);
        }
        let n = self.received.len().min(out.len());
        for (o, b) in out.iter_mut().zip(self.received.drain(..n)) {
            *o = b;
        }
        Ok(n)
    }
}

/// The UART terminal on port 1.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Uart {
    config: Option<UartConfig>,
    tx_in_flight: Option<usize>,
    rx: VecDeque<u8>,
}

impl Uart {
    /// Configures the port.
    pub fn open(&mut self, cfg: UartConfig) {
        self.config = Some(cfg);
    }

    /// Whether a transmission awaits completion.
    pub fn in_flight(&self) -> bool {
        self.tx_in_flight.is_some()
    }

    /// Starts transmitting `data` on `port`.
    pub fn write(&mut self, port: u8, data: &[u8]) -> Result<(), AbiError> {
        match self.config {
            Some(cfg) if cfg.port == port && cfg.tx.is_some() && self.tx_in_flight.is_none() => {
                self.tx_in_flight = Some(data.len());
                Ok(())
            }
            Some(cfg) if cfg.port == port && cfg.tx.is_none() => Err(AbiError::Unsupported),
            _ => Err(AbiError::Busy),
        }
    }

    /// Finishes the transmission in flight, if any, as a `DmaDone` event.
    pub fn complete(&mut self, time_us: u32) -> Option<Event> {
        let len = self.tx_in_flight.take()?;
        Some(Event {
            kind: EventKind::DmaDone,
            source: DmaChannel::Uart1Tx as i32,
            value: i32::try_from(len).unwrap_or(i32::MAX),
            time_us,
        })
    }

    /// Buffers bytes typed at the terminal and returns the `UartRx` event,
    /// or an error message when the port cannot receive.
    pub fn receive(&mut self, bytes: &[u8], time_us: u32) -> Result<Event, String> {
        let Some(cfg) = self.config.filter(|c| c.rx.is_some()) else {
            return Err("uart1 is not open for receive".to_string());
        };
        let room = UART_RX_BUFFER.saturating_sub(self.rx.len());
        self.rx.extend(bytes.iter().take(room));
        Ok(Event {
            kind: EventKind::UartRx,
            source: i32::from(cfg.port),
            value: i32::try_from(self.rx.len()).unwrap_or(i32::MAX),
            time_us,
        })
    }

    /// Moves buffered received bytes into `out`.
    pub fn read(&mut self, port: u8, out: &mut [u8]) -> Result<usize, AbiError> {
        if self.config.is_none_or(|c| c.port != port) {
            return Err(AbiError::Busy);
        }
        let n = self.rx.len().min(out.len());
        for (o, b) in out.iter_mut().zip(self.rx.drain(..n)) {
            *o = b;
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(id: u8, period_us: u32, periodic: bool) -> TimerSpec {
        TimerSpec {
            id,
            period_us,
            periodic,
        }
    }

    #[test]
    fn timers_fire_coalesce_and_stop() {
        let mut t = Timers::default();
        t.start(spec(0, 1000, true), 0).unwrap();
        t.start(spec(1, 2500, false), 0).unwrap();
        assert_eq!(t.next_due(), Some(1000));
        assert!(t.fire(999).is_empty());
        let e = t.fire(1000);
        assert_eq!((e.len(), e[0].value, e[0].time_us), (1, 1, 1000));
        // 3.5 periods late: one event counting 3 periods, re-armed at 5000.
        let e = t.fire(4500);
        assert_eq!(e.len(), 2);
        assert_eq!((e[0].source, e[0].value, e[0].time_us), (0, 3, 4000));
        assert_eq!((e[1].source, e[1].value), (1, 1));
        assert_eq!(t.next_due(), Some(5000));
        t.stop(0).unwrap();
        assert_eq!(t.next_due(), None);
        assert_eq!(t.stop(9), Err(AbiError::InvalidArgument));
    }

    fn spi_cfg() -> SpiConfig {
        SpiConfig {
            bus: 0,
            sck: 6,
            mosi: Some(7),
            miso: Some(8),
            cs: None,
            freq_khz: 1000,
            mode: 0,
        }
    }

    #[test]
    fn spi_loopback_and_pattern() {
        let mut s = Spi::new(None);
        assert_eq!(s.transfer(0, b"hi", 2), Err(AbiError::Busy), "not open");
        s.open(spi_cfg());
        s.transfer(0, b"hi", 4).unwrap();
        assert_eq!(s.transfer(0, b"hi", 2), Err(AbiError::Busy), "in flight");
        let mut out = [0xFF; 8];
        assert_eq!(s.read(&mut out), Err(AbiError::Busy));
        let done = s.complete(7).unwrap();
        assert_eq!(
            (done.kind, done.source, done.value),
            (EventKind::DmaDone, 0, 4)
        );
        assert_eq!(s.read(&mut out), Ok(4));
        assert_eq!(&out[..4], b"hi\0\0");
        assert_eq!(s.read(&mut out), Ok(0));

        let mut s = Spi::new(Some(vec![0xA5, 0x5A]));
        s.open(spi_cfg());
        s.transfer(0, b"xyz", 3).unwrap();
        s.complete(0);
        assert_eq!(s.read(&mut out), Ok(3));
        assert_eq!(&out[..3], &[0xA5, 0x5A, 0xA5]);
    }

    #[test]
    fn uart_terminal() {
        let mut u = Uart::default();
        assert_eq!(u.write(1, b"x"), Err(AbiError::Busy));
        assert!(u.receive(b"x", 0).is_err());
        u.open(UartConfig {
            port: 1,
            tx: Some(4),
            rx: Some(5),
            baud: 9600,
        });
        u.write(1, b"abc").unwrap();
        assert_eq!(u.write(1, b"x"), Err(AbiError::Busy));
        assert_eq!(u.complete(3).map(|e| (e.source, e.value)), Some((1, 3)));
        assert_eq!(u.complete(3), None);
        let e = u.receive(b"hello", 9).unwrap();
        assert_eq!((e.kind, e.source, e.value), (EventKind::UartRx, 1, 5));
        let mut out = [0; 3];
        assert_eq!(u.read(1, &mut out), Ok(3));
        assert_eq!(&out, b"hel");
        let big = vec![b'z'; UART_RX_BUFFER + 10];
        assert_eq!(
            u.receive(&big, 0).map(|e| e.value),
            Ok(UART_RX_BUFFER as i32)
        );
    }
}
