//! The node as a payload sees it: `wasm_host::Device` over the process's
//! clock, four software timers and a bounded outbox of publications.
//!
//! | ABI call | mw-posix-node |
//! |----------|------------|
//! | `log` | a `sys/log` line in the outbox (truncated to `MAX_LOG` by wasm-host) |
//! | `publish` | a publication in the outbox (`sys/...` keys `InvalidKey`; larger than one serial frame `InvalidArgument`; outbox full `Transport`, counted) |
//! | `subscribe`, `set_tick` | wasm-host's own tables; the controller delivers and ticks |
//! | `now_ms`, `now_us` | the [`Clock`] (monotonic, from the node's start), wrapping |
//! | `timer_start`, `timer_stop`, `irq_overflow` | four software timers (`esp_base::periph::timers`) fired by the controller as `Timer` events to `on_irq`; the event queue's drop count |
//! | `serve`, `node_id` (ABI 5) | any non-`sys` key (`esp_base::serve::servable`), routed by the controller; the node id `sys/info` reports |
//! | `gpio_*`, `gpio_irq`, `adc_read`, `spi_*`, `dma_read`, `i2c_*`, `uart_*`, `pwm_*`, `route_*` | `Unsupported` |
//!
//! Peripherals are `Unsupported` for now: a Linux host reaches GPIO, SPI
//! and I2C through the kernel's character devices (`/dev/gpiochipN`,
//! `/dev/spidevB.C`, `/dev/i2c-N`), which is a later item with its own
//! pin policy. A payload that needs them fails its calls with the
//! documented code, as on a board that lacks the peripheral.
//!
//! Nothing here performs I/O: publications wait in the outbox until the
//! shell drains it after the payload call returns.

use std::collections::VecDeque;

use esp_base::periph::timer_event;
use esp_base::periph::timers::Timers;
use middleware::loader::{is_system_key, keys};
use middleware::serial::frame::{FRAME_OVERHEAD, MAX_RAW_FRAME};
use middleware::wasm_abi::events::{Event, EventKind, EventQueue, MAX_EVENTS};
use middleware::wasm_abi::periph::TimerSpec;
use middleware::wasm_abi::{AbiError, Pull};
use middleware::{Key, SenderId};

/// Publications the outbox holds between drains.
pub const OUTBOX: usize = 64;

/// A monotonic microsecond clock.
pub trait Clock {
    /// Microseconds since an arbitrary fixed start.
    fn now_us(&self) -> u64;
    /// Milliseconds since the start, wrapping at `u32`.
    fn now_ms(&self) -> u32 {
        (self.now_us() / 1000) as u32
    }
}

/// The process's monotonic clock, from when it was made.
#[derive(Debug, Clone, Copy)]
pub struct SystemClock(std::time::Instant);

impl SystemClock {
    /// A clock starting now.
    pub fn start() -> Self {
        SystemClock(std::time::Instant::now())
    }
}

impl Clock for SystemClock {
    fn now_us(&self) -> u64 {
        u64::try_from(self.0.elapsed().as_micros()).unwrap_or(u64::MAX)
    }
}

/// A publication waiting to be sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Out {
    /// The key.
    pub key: String,
    /// The bytes.
    pub payload: Vec<u8>,
}

/// Whether a publication on `key` with `len` bytes fits one serial frame.
pub fn fits_frame(key: Key<'_>, len: usize) -> bool {
    key.as_str().len() + len + FRAME_OVERHEAD <= MAX_RAW_FRAME
}

/// A bounded queue of publications that drops the newest when full.
#[derive(Debug)]
pub struct Outbox {
    queue: VecDeque<Out>,
    capacity: usize,
    dropped: u32,
}

impl Default for Outbox {
    /// Holding [`OUTBOX`] publications.
    fn default() -> Self {
        Outbox::with_capacity(OUTBOX)
    }
}

impl Outbox {
    /// An empty outbox holding at most `capacity` publications.
    pub fn with_capacity(capacity: usize) -> Self {
        Outbox { queue: VecDeque::with_capacity(capacity), capacity, dropped: 0 }
    }

    /// Queues a publication; `false` (counted) when full.
    pub fn push(&mut self, key: Key<'_>, payload: &[u8]) -> bool {
        self.push_out(Out { key: key.as_str().to_owned(), payload: payload.to_vec() })
    }

    /// Queues an owned publication; `false` (counted) when full.
    pub fn push_out(&mut self, out: Out) -> bool {
        if self.queue.len() >= self.capacity {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        }
        self.queue.push_back(out);
        true
    }

    /// Queues a `sys/log` line, cut to one frame.
    pub fn log(&mut self, line: &str) {
        let max = MAX_RAW_FRAME - FRAME_OVERHEAD - keys::LOG.as_str().len();
        let mut end = line.len().min(max);
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        self.push(keys::LOG, line.get(..end).unwrap_or("").as_bytes());
    }

    /// Takes everything queued, oldest first.
    pub fn drain(&mut self) -> impl Iterator<Item = Out> + '_ {
        self.queue.drain(..)
    }

    /// Publications dropped so far.
    pub fn dropped(&self) -> u32 {
        self.dropped
    }

    /// Queued publications.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Nothing queued.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

/// The device a payload runs against (module docs).
#[derive(Debug)]
pub struct PiDevice<C> {
    clock: C,
    node: SenderId,
    timers: Timers,
    events: EventQueue<MAX_EVENTS>,
    outbox: Outbox,
}

impl<C: Clock> PiDevice<C> {
    /// A device reading `clock`, answering `node_id` with `node`.
    pub fn new(clock: C, node: SenderId) -> Self {
        PiDevice { clock, node, timers: Timers::new(), events: EventQueue::new(), outbox: Outbox::default() }
    }

    /// When the earliest running timer is due, in µs on the clock.
    pub fn next_alarm_us(&self) -> Option<u64> {
        self.timers.next_deadline()
    }

    /// Fires the timers due now into the event queue (a full queue drops
    /// and counts the event).
    pub fn fire_timers(&mut self) {
        let now = self.clock.now_us();
        let (next, fired) = self.timers.expire(now);
        self.timers = next;
        for f in fired {
            let _ = self.events.push(timer_event(f.id, f.periods, now as u32));
        }
    }

    /// The next queued event.
    pub fn pop_event(&mut self) -> Option<Event> {
        self.events.pop()
    }

    /// Events dropped so far (not reset).
    pub fn overflow(&self) -> u32 {
        self.events.overflow()
    }

    /// The outbox.
    pub fn outbox(&mut self) -> &mut Outbox {
        &mut self.outbox
    }
}

impl<C: Clock> wasm_host::Device for PiDevice<C> {
    fn log(&mut self, line: &str) {
        self.outbox.log(line);
    }

    fn publish(&mut self, key: Key<'_>, data: &[u8]) -> Result<(), AbiError> {
        if is_system_key(key) {
            return Err(AbiError::InvalidKey);
        }
        if !fits_frame(key, data.len()) {
            return Err(AbiError::InvalidArgument);
        }
        if self.outbox.push(key, data) { Ok(()) } else { Err(AbiError::Transport) }
    }

    fn now_ms(&mut self) -> u32 {
        self.clock.now_ms()
    }

    fn now_us(&mut self) -> u32 {
        self.clock.now_us() as u32
    }

    fn gpio_output(&mut self, _pin: u8) -> Result<(), AbiError> {
        Err(AbiError::Unsupported)
    }

    fn gpio_input(&mut self, _pin: u8, _pull: Pull) -> Result<(), AbiError> {
        Err(AbiError::Unsupported)
    }

    fn gpio_write(&mut self, _pin: u8, _high: bool) -> Result<(), AbiError> {
        Err(AbiError::Unsupported)
    }

    fn gpio_read(&mut self, _pin: u8) -> Result<bool, AbiError> {
        Err(AbiError::Unsupported)
    }

    fn irq_overflow(&mut self) -> u32 {
        self.events.take_overflow()
    }

    fn timer_start(&mut self, spec: TimerSpec) -> Result<(), AbiError> {
        self.events.purge(EventKind::Timer, i32::from(spec.id));
        self.timers = self.timers.start(spec, self.clock.now_us());
        Ok(())
    }

    fn timer_stop(&mut self, id: u8) -> Result<(), AbiError> {
        self.timers = self.timers.stop(id);
        self.events.purge(EventKind::Timer, i32::from(id));
        Ok(())
    }

    fn serve(&mut self, key: Key<'_>) -> Result<(), AbiError> {
        esp_base::serve::servable(key)
    }

    fn node_id(&mut self) -> Result<SenderId, AbiError> {
        Ok(self.node)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::Cell;
    use middleware::wasm_abi::periph::{I2cConfig, SpiConfig, UartConfig};
    use wasm_host::Device;

    /// A clock the test moves.
    #[derive(Debug, Default)]
    struct Manual(Cell<u64>);

    impl Clock for &Manual {
        fn now_us(&self) -> u64 {
            self.0.get()
        }
    }

    const NODE: SenderId = SenderId([1, 2, 3, 4, 5, 6, 7, 8]);

    #[test]
    fn pn6_publish_log_clock_serve_and_node_id() {
        let clock = Manual::default();
        let mut d = PiDevice::new(&clock, NODE);
        clock.0.set(5_000_123);
        assert_eq!(d.now_ms(), 5_000);
        assert_eq!(d.now_us(), 5_000_123);
        d.publish(Key::from_static("app/pi/x"), b"hi").expect("publish");
        assert_eq!(d.publish(Key::from_static("sys/payload/status"), b"x"), Err(AbiError::InvalidKey));
        let big = vec![0u8; MAX_RAW_FRAME];
        assert_eq!(d.publish(Key::from_static("app/pi/x"), &big), Err(AbiError::InvalidArgument));
        d.log("hello");
        let out: Vec<Out> = d.outbox().drain().collect();
        assert_eq!(
            out,
            [
                Out { key: "app/pi/x".into(), payload: b"hi".to_vec() },
                Out { key: "sys/log".into(), payload: b"hello".to_vec() },
            ]
        );
        assert_eq!(d.serve(Key::from_static("app/pi/echo")), Ok(()));
        assert_eq!(d.serve(Key::from_static("sys/info")), Err(AbiError::InvalidKey));
        assert_eq!(d.node_id(), Ok(NODE));
    }

    #[test]
    fn pn6_the_outbox_is_bounded_and_drops_the_newest() {
        let clock = Manual::default();
        let mut d = PiDevice::new(&clock, NODE);
        for i in 0..OUTBOX {
            d.publish(Key::from_static("app/n"), &[i as u8]).expect("room");
        }
        assert_eq!(d.publish(Key::from_static("app/n"), b"late"), Err(AbiError::Transport));
        d.log("also dropped");
        assert_eq!(d.outbox().dropped(), 2);
        let first: Vec<u8> = d.outbox().drain().map(|o| o.payload[0]).collect();
        assert_eq!(first, (0..OUTBOX as u8).collect::<Vec<_>>());
        // A long log line is cut to a frame, on a character boundary.
        let line = "é".repeat(MAX_RAW_FRAME);
        d.outbox().log(&line);
        let cut = d.outbox().drain().next().expect("line");
        assert!(fits_frame(keys::LOG, cut.payload.len()));
        assert!(std::str::from_utf8(&cut.payload).is_ok());
    }

    #[test]
    fn pn6_timers_fire_as_events_and_peripherals_are_unsupported() {
        let clock = Manual::default();
        let mut d = PiDevice::new(&clock, NODE);
        d.timer_start(TimerSpec::parse(1, 1000, 1).expect("spec")).expect("start");
        assert_eq!(d.next_alarm_us(), Some(1000));
        clock.0.set(2_500);
        d.fire_timers();
        let e = d.pop_event().expect("timer event");
        assert_eq!(e.kind, EventKind::Timer);
        assert_eq!(d.pop_event(), None);
        assert_eq!(d.next_alarm_us(), Some(3000));
        d.timer_stop(1).expect("stop");
        assert_eq!(d.next_alarm_us(), None);
        let u = Err(AbiError::Unsupported);
        assert_eq!(d.gpio_output(4), u);
        assert_eq!(d.gpio_input(4, Pull::Up), u);
        assert_eq!(d.gpio_write(4, true), u);
        assert_eq!(d.gpio_read(4), Err(AbiError::Unsupported));
        assert_eq!(d.adc_read(26), Err(AbiError::Unsupported));
        assert_eq!(
            d.i2c_open(I2cConfig { bus: 0, freq_hz: 100_000 }),
            u
        );
        assert_eq!(d.spi_open(SpiConfig::parse(0, 2, 3, 4, 5, 1000, 0).expect("spi")), u);
        assert_eq!(d.uart_open(UartConfig::parse(1, 4, 5, 115_200).expect("uart")), u);
    }

    /// MF3: the manifest lists the timers and serving, nothing the device
    /// answers `Unsupported` (above), and no sampled source.
    #[test]
    fn mf3_the_manifest_lists_only_what_the_device_does() {
        use middleware::loader::Caps;
        use middleware::loader::manifest::Groups;
        use middleware::wasm_abi::route::{ImuConfig, SampleSource};
        let m = crate::service::MANIFEST;
        assert_eq!(m.groups, Groups::TIMERS.with(Groups::SERVE).with(Groups::CHANNELS));
        let clock = Manual::default();
        let mut d = PiDevice::new(&clock, NODE);
        let imu = SampleSource::Imu(ImuConfig::new(100, 2, false, 10).expect("cfg"));
        assert!(!m.accepts_samples(&imu));
        assert!(d.route_samples(imu, middleware::Key::from_static("a")).is_err());
        assert!((m.i2c, m.spi, m.frames) == (None, None, None));
        assert_eq!(m.caps(), Caps::SERVE);
    }
}
