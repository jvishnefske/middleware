//! The interrupt event queue shared by the ISRs and the controller.
//!
//! One [`EventQueue<MAX_EVENTS>`] lives in a critical-section mutex. ISRs
//! (GPIO in [`super::gpio`], the timer alarm in [`super::timer`]) and the
//! completion paths of SPI and UART DMA call [`push`]; it never blocks,
//! never allocates and scans at most `MAX_EVENTS` entries (the queue's own
//! bound), then signals [`WAKE`] so the controller's `select` returns.
//!
//! The controller drains the queue with [`pop`] (at most `MAX_EVENTS` per
//! loop iteration) into `Payload::dispatch`, and asks [`overflow_report`]
//! after each drain whether new drops happened since the last `sys/log`
//! line. `irq_overflow` reads and resets the count through
//! [`take_overflow`]. [`clear`] empties everything between payload runs.
//!
//! Lock order: GPIO ISR → `GPIO_LOCK` (esp-hal) → this mutex; thread code
//! takes this mutex alone. Only core 0 runs firmware code, so the
//! critical section masks every interrupt that could contend.

use core::cell::RefCell;

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use esp_base::periph::overflow_report as report;
use middleware::wasm_abi::events::{Event, EventKind, EventQueue, MAX_EVENTS, Pushed};

struct Queue {
    events: EventQueue<MAX_EVENTS>,
    /// Overflow count already reported on `sys/log`.
    logged: u32,
}

static QUEUE: Mutex<CriticalSectionRawMutex, RefCell<Queue>> =
    Mutex::new(RefCell::new(Queue { events: EventQueue::new(), logged: 0 }));

/// Signalled on every push; the controller waits on it.
pub static WAKE: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Microseconds since boot (esp-hal system time; does not wrap in practice).
pub fn now_us() -> u64 {
    esp_hal::time::Instant::now().duration_since_epoch().as_micros()
}

/// Runs `f` on the queue. The `RefCell` is only borrowed inside the
/// critical section, so the borrow cannot fail; `None` is returned if it
/// ever did, rather than panicking in an ISR.
fn with<R>(f: impl FnOnce(&mut Queue) -> R) -> Option<R> {
    QUEUE.lock(|cell| cell.try_borrow_mut().ok().map(|mut q| f(&mut q)))
}

/// Enqueues `event` (ISR safe) and wakes the controller.
pub fn push(event: Event) -> Pushed {
    let pushed = with(|q| q.events.push(event)).unwrap_or(Pushed::Dropped);
    WAKE.signal(());
    pushed
}

/// Removes the oldest event.
pub fn pop() -> Option<Event> {
    with(|q| q.events.pop()).flatten()
}

/// Drops pending events of `kind` from `source`.
pub fn purge(kind: EventKind, source: i32) {
    with(|q| q.events.purge(kind, source));
}

/// Events dropped since the payload last asked; resets the count.
pub fn take_overflow() -> u32 {
    with(|q| {
        q.logged = 0;
        q.events.take_overflow()
    })
    .unwrap_or(0)
}

/// Newly dropped events since the last report, if any.
pub fn overflow_report() -> Option<u32> {
    with(|q| {
        let current = q.events.overflow();
        let new = report(q.logged, current);
        q.logged = current;
        new
    })
    .flatten()
}

/// Empties the queue and its counts (payload stop or start).
pub fn clear() {
    with(|q| {
        q.events = EventQueue::new();
        q.logged = 0;
    });
    WAKE.reset();
}
