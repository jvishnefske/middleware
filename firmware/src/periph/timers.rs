//! The payload timer table: four one-shot or periodic timers multiplexed
//! onto one hardware alarm.
//!
//! Time is microseconds since boot as a `u64` (it does not wrap in practice);
//! events carry the low 32 bits. Each running timer has an absolute
//! deadline. The shell keeps one hardware alarm armed for
//! [`Timers::alarm_in`] and, when it fires, calls [`Timers::expire`], which:
//!
//! * removes due one-shot timers (1 period each);
//! * re-arms due periodic timers on their original grid,
//!   `deadline + k * period`, so late servicing does not drift, and reports
//!   `k` = the periods that elapsed (1 normally, more when the alarm was
//!   serviced late — the "missed periods" the queue would also have
//!   coalesced);
//! * returns the fired timers in order of the deadline they missed first.
//!
//! All transitions take a table and return a new one. The table has
//! [`MAX_TIMERS`] slots, so every operation is O(4).

use heapless::Vec;
use middleware::wasm_abi::periph::{MAX_TIMERS, TimerSpec};

/// Shortest delay the alarm is armed for, so a deadline already in the past
/// still produces an interrupt rather than a missed alarm.
pub const MIN_ALARM_US: u64 = 1;

/// One running timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    /// Absolute deadline, µs since boot.
    pub deadline: u64,
    /// Period in µs.
    pub period: u32,
    /// Re-arm after each alarm.
    pub periodic: bool,
}

/// A timer that fired: its id and the periods that elapsed (≥ 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fired {
    /// Timer id.
    pub id: u8,
    /// Periods elapsed since the previous alarm (≥ 1).
    pub periods: u32,
}

/// The four payload timers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Timers {
    slots: [Option<Slot>; MAX_TIMERS],
}

impl Timers {
    /// No timer running.
    pub const fn new() -> Self {
        Timers { slots: [None; MAX_TIMERS] }
    }

    /// The slot of timer `id`.
    pub fn slot(&self, id: u8) -> Option<Slot> {
        self.slots.get(usize::from(id)).copied().flatten()
    }

    /// Whether no timer runs.
    pub fn is_idle(&self) -> bool {
        self.slots.iter().all(Option::is_none)
    }

    fn with(mut self, id: u8, slot: Option<Slot>) -> Self {
        if let Some(s) = self.slots.get_mut(usize::from(id)) {
            *s = slot;
        }
        self
    }

    /// Starts (or restarts) a timer at `now`: first deadline one period away.
    pub fn start(self, spec: TimerSpec, now: u64) -> Self {
        let slot = Slot {
            deadline: now.saturating_add(u64::from(spec.period_us)),
            period: spec.period_us,
            periodic: spec.periodic,
        };
        self.with(spec.id, Some(slot))
    }

    /// Stops a timer.
    pub fn stop(self, id: u8) -> Self {
        self.with(id, None)
    }

    /// The earliest deadline of a running timer.
    pub fn next_deadline(&self) -> Option<u64> {
        self.slots.iter().flatten().map(|s| s.deadline).min()
    }

    /// How long to arm the alarm for at `now` (at least [`MIN_ALARM_US`]),
    /// or `None` to leave it stopped.
    pub fn alarm_in(&self, now: u64) -> Option<u64> {
        self.next_deadline().map(|d| d.saturating_sub(now).max(MIN_ALARM_US))
    }

    /// Fires every timer due at `now`. Returns the new table and the fired
    /// timers, ordered by the deadline they missed first.
    pub fn expire(self, now: u64) -> (Self, Vec<Fired, MAX_TIMERS>) {
        let mut next = self;
        let mut due: Vec<(u64, Fired), MAX_TIMERS> = Vec::new();
        for (id, slot) in self.slots.iter().enumerate() {
            let Some(s) = slot else { continue };
            if s.deadline > now {
                continue;
            }
            let id = id as u8;
            let period = u64::from(s.period.max(1));
            let periods = if s.periodic { 1 + (now - s.deadline) / period } else { 1 };
            let rearmed = s.periodic.then(|| Slot {
                deadline: s.deadline.saturating_add(periods.saturating_mul(period)),
                ..*s
            });
            next = next.with(id, rearmed);
            let fired = Fired { id, periods: u32::try_from(periods).unwrap_or(u32::MAX) };
            // At most MAX_TIMERS entries: the push cannot fail.
            let _ = due.push((s.deadline, fired));
        }
        due.sort_unstable_by_key(|(deadline, f)| (*deadline, f.id));
        (next, due.into_iter().map(|(_, f)| f).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(id: u8, period_us: u32, periodic: bool) -> TimerSpec {
        TimerSpec { id, period_us, periodic }
    }

    #[test]
    fn d8_one_shot_fires_once() {
        let t = Timers::new().start(spec(2, 500, false), 1_000);
        assert_eq!(t.next_deadline(), Some(1_500));
        assert_eq!(t.alarm_in(1_200), Some(300));
        let (t, fired) = t.expire(1_499);
        assert!(fired.is_empty(), "not yet due");
        let (t, fired) = t.expire(1_510);
        assert_eq!(fired.as_slice(), [Fired { id: 2, periods: 1 }]);
        assert!(t.is_idle());
        assert_eq!(t.alarm_in(2_000), None);
    }

    #[test]
    fn d8_periodic_rearms_on_its_grid_and_counts_missed_periods() {
        let t = Timers::new().start(spec(0, 100, true), 0);
        let (t, fired) = t.expire(105);
        assert_eq!(fired.as_slice(), [Fired { id: 0, periods: 1 }]);
        assert_eq!(t.next_deadline(), Some(200), "no drift from late service");
        // Serviced 3.5 periods late: 4 periods elapsed, next on the grid.
        let (t, fired) = t.expire(550);
        assert_eq!(fired.as_slice(), [Fired { id: 0, periods: 4 }]);
        assert_eq!(t.next_deadline(), Some(600));
        assert_eq!(t.slot(0).map(|s| s.periodic), Some(true));
    }

    #[test]
    fn d8_four_timers_fire_in_deadline_order() {
        let t = Timers::new()
            .start(spec(3, 100, false), 0)
            .start(spec(1, 300, true), 0)
            .start(spec(0, 200, true), 0)
            .start(spec(2, 1_000, true), 0);
        assert_eq!(t.next_deadline(), Some(100));
        let (t, fired) = t.expire(350);
        let ids: Vec<u8, 4> = fired.iter().map(|f| f.id).collect();
        assert_eq!(ids.as_slice(), [3, 0, 1]);
        assert_eq!(t.slot(3), None);
        assert_eq!(t.next_deadline(), Some(400));
        assert_eq!(t.slot(2).map(|s| s.deadline), Some(1_000));
    }

    #[test]
    fn d8_stop_and_restart() {
        let t = Timers::new().start(spec(1, 100, true), 0).stop(1);
        assert!(t.is_idle());
        let t = t.start(spec(1, 100, true), 0).start(spec(1, 250, false), 50);
        assert_eq!(t.slot(1), Some(Slot { deadline: 300, period: 250, periodic: false }));
        // A deadline already passed still arms the alarm.
        assert_eq!(t.alarm_in(400), Some(MIN_ALARM_US));
        // Out-of-range ids are ignored (the ABI parser rejects them first).
        assert_eq!(Timers::new().stop(9), Timers::new());
    }
}
