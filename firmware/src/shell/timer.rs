//! The four payload timers on one hardware alarm.
//!
//! Timer group 1, timer 0 (`TIMG1`, present on ESP32, ESP32-S3 and
//! ESP32-C3; `TIMG0` drives the embassy time driver) runs as an esp-hal
//! [`OneShotTimer`]. Its interrupt handler is the compile-time
//! `#[handler]` [`alarm_isr`], installed once at boot by [`init`].
//!
//! The pure [`Timers`] deadline table decides everything: the ISR clears
//! the interrupt, calls [`Timers::expire`] at the current time, pushes one
//! `Timer` event per fired timer (value = periods elapsed; the queue
//! coalesces further) and re-arms the alarm for [`Timers::alarm_in`]. Work
//! per interrupt is O(`MAX_TIMERS`) with no allocation.
//!
//! [`start`] and [`stop`] update the table and re-arm inside the same
//! critical section, and purge the timer's pending events.

use core::cell::RefCell;

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use esp_base::periph::timer_event;
use esp_base::periph::timers::Timers;
use esp_hal::Blocking;
use esp_hal::handler;
use esp_hal::time::Duration;
use esp_hal::timer::OneShotTimer;
use esp_hal::timer::timg::Timer as TimgTimer;
use middleware::wasm_abi::AbiError;
use middleware::wasm_abi::events::EventKind;
use middleware::wasm_abi::periph::TimerSpec;

use super::irq;

struct Alarm {
    hw: OneShotTimer<'static, Blocking>,
    table: Timers,
}

impl Alarm {
    /// Arms the hardware for the table's next deadline, or stops it.
    fn rearm(&mut self, now: u64) -> Result<(), AbiError> {
        match self.table.alarm_in(now) {
            Some(us) => self.hw.schedule(Duration::from_micros(us)).map_err(|_| AbiError::Unsupported),
            None => {
                self.hw.stop();
                Ok(())
            }
        }
    }
}

static ALARM: Mutex<CriticalSectionRawMutex, RefCell<Option<Alarm>>> = Mutex::new(RefCell::new(None));

fn with<R>(f: impl FnOnce(&mut Alarm) -> R) -> Option<R> {
    ALARM.lock(|cell| cell.try_borrow_mut().ok().and_then(|mut a| a.as_mut().map(f)))
}

#[handler]
fn alarm_isr() {
    let now = irq::now_us();
    with(|a| {
        a.hw.clear_interrupt();
        let (table, fired) = a.table.expire(now);
        a.table = table;
        for f in fired {
            irq::push(timer_event(f.id, f.periods, now as u32));
        }
        // A failed re-arm leaves the remaining timers silent; nothing in an
        // ISR can report it, and `schedule` only fails for out-of-range
        // durations the table never produces.
        let _ = a.rearm(now);
    });
}

/// Takes the alarm timer and installs [`alarm_isr`].
pub fn init(timer: TimgTimer<'static>) {
    let mut hw = OneShotTimer::new(timer);
    hw.set_interrupt_handler(alarm_isr);
    hw.listen();
    ALARM.lock(|cell| {
        if let Ok(mut slot) = cell.try_borrow_mut() {
            *slot = Some(Alarm { hw, table: Timers::new() });
        }
    });
}

/// Starts or restarts a timer; its pending events are purged.
pub fn start(spec: TimerSpec) -> Result<(), AbiError> {
    irq::purge(EventKind::Timer, i32::from(spec.id));
    with(|a| {
        let now = irq::now_us();
        a.table = a.table.start(spec, now);
        a.rearm(now)
    })
    .unwrap_or(Err(AbiError::Unsupported))
}

/// Stops a timer and purges its pending events.
pub fn stop(id: u8) -> Result<(), AbiError> {
    let result = with(|a| {
        a.table = a.table.stop(id);
        a.rearm(irq::now_us())
    })
    .unwrap_or(Err(AbiError::Unsupported));
    irq::purge(EventKind::Timer, i32::from(id));
    result
}

/// Stops every timer (payload stop).
pub fn stop_all() {
    with(|a| {
        a.table = Timers::new();
        a.hw.stop();
        a.hw.clear_interrupt();
    });
}
