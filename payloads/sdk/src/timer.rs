//! Hardware timers through the `timer_start` / `timer_stop` imports.
//!
//! A payload has [`MAX_TIMERS`] timers, ids `0..MAX_TIMERS`. Each alarm
//! arrives as an [`Event::Timer`](crate::irq::Event::Timer) in
//! [`Payload::on_irq`](crate::Payload::on_irq), timestamped in the ISR.
//! Periodic alarms that fire while the previous one is still queued coalesce
//! into one event whose `periods` counts them, so a short period cannot flood
//! the event queue.
//!
//! Arguments are validated here with the ABI's own parser
//! ([`TimerSpec::parse`]) before the host is called, so an out-of-range id or
//! a period below [`MIN_TIMER_US`] fails with [`AbiError::InvalidArgument`]
//! even without a host. The ABI does not say whether starting a timer that
//! is already running re-arms it or fails, so to restart one portably,
//! [`Timer::stop`] it first (stopping an idle timer is harmless).
//! [`Timer::stop`] also discards that timer's queued events. Dropping a
//! [`Timer`] does *not* stop it.

use crate::{AbiError, check, sys};
pub use middleware::wasm_abi::periph::{MAX_TIMERS, MIN_TIMER_US, TimerSpec};

/// A started hardware timer.
#[derive(Debug, PartialEq, Eq)]
pub struct Timer {
    spec: TimerSpec,
}

/// Validates a `timer_start` request with the ABI parser.
fn spec(id: u8, period_us: u32, periodic: bool) -> Result<TimerSpec, AbiError> {
    let period = i32::try_from(period_us).map_err(|_| AbiError::InvalidArgument)?;
    TimerSpec::parse(i32::from(id), period, i32::from(periodic))
}

impl Timer {
    fn start(spec: TimerSpec) -> Result<Self, AbiError> {
        check(sys::timer_start(
            i32::from(spec.id),
            spec.period_us as i32,
            i32::from(spec.periodic),
        ))?;
        Ok(Timer { spec })
    }

    /// Starts timer `id` firing every `period_us` microseconds.
    pub fn start_periodic(id: u8, period_us: u32) -> Result<Self, AbiError> {
        Self::start(spec(id, period_us, true)?)
    }

    /// Starts timer `id` firing once, `delay_us` microseconds from now.
    pub fn start_once(id: u8, delay_us: u32) -> Result<Self, AbiError> {
        Self::start(spec(id, delay_us, false)?)
    }

    /// The timer id.
    pub fn id(&self) -> u8 {
        self.spec.id
    }

    /// The period (or one-shot delay) in microseconds.
    pub fn period_us(&self) -> u32 {
        self.spec.period_us
    }

    /// Whether the timer re-arms after each alarm.
    pub fn is_periodic(&self) -> bool {
        self.spec.periodic
    }

    /// Whether `event` is an alarm of this timer; returns the elapsed
    /// periods if so.
    pub fn periods(&self, event: &crate::irq::Event) -> Option<u32> {
        match *event {
            crate::irq::Event::Timer { id, periods, .. } if id == self.spec.id => Some(periods),
            _ => None,
        }
    }

    /// Stops the timer and purges its queued events.
    pub fn stop(self) -> Result<(), AbiError> {
        check(sys::timer_stop(i32::from(self.spec.id))).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::irq::Event;

    #[test]
    fn arguments_are_validated_before_the_host() {
        assert_eq!(
            Timer::start_periodic(MAX_TIMERS as u8, 1000),
            Err(AbiError::InvalidArgument)
        );
        assert_eq!(
            Timer::start_once(0, MIN_TIMER_US - 1),
            Err(AbiError::InvalidArgument)
        );
        assert_eq!(
            Timer::start_once(0, u32::MAX),
            Err(AbiError::InvalidArgument)
        );
        // Valid arguments reach the host stub.
        assert_eq!(
            Timer::start_periodic(3, MIN_TIMER_US),
            Err(AbiError::Unsupported)
        );
    }

    #[test]
    fn spec_mirrors_the_abi() {
        assert_eq!(
            spec(1, 500, true),
            Ok(TimerSpec {
                id: 1,
                period_us: 500,
                periodic: true
            })
        );
    }

    #[test]
    fn periods_matches_only_its_id() {
        let t = Timer {
            spec: spec(2, 1000, true).expect("valid"),
        };
        let mine = Event::Timer {
            id: 2,
            periods: 3,
            time_us: 0,
        };
        let other = Event::Timer {
            id: 1,
            periods: 1,
            time_us: 0,
        };
        assert_eq!(t.periods(&mine), Some(3));
        assert_eq!(t.periods(&other), None);
        assert_eq!((t.id(), t.period_us(), t.is_periodic()), (2, 1000, true));
        assert_eq!(t.stop(), Err(AbiError::Unsupported));
    }
}
