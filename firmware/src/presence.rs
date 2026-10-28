//! Physical presence from the BOOT button: the pure hold detector.
//!
//! The shell samples the BOOT button (GPIO0 on ESP32 and ESP32-S3, GPIO9 on
//! ESP32-C3) every [`POLL_MS`] and feeds [`Hold::step`]. Presence is
//! asserted once per press, when the button has been held continuously for
//! [`PRESENCE_HOLD_MS`]. The button must first be seen released after
//! boot: holding it through reset selects the ROM download mode anyway, and
//! a pin stuck low (or a board that straps it low) must never assert
//! presence on its own.
//!
//! A sample is `None` when the pin cannot be read honestly — a payload
//! drives it as an output, routes it to a peripheral or pulls it down —
//! and counts as released, so a payload cannot fake a press.
//!
//! On presence the shell opens the re-key
//! [`Window`](middleware::loader::trust::Window) for
//! [`REKEY_WINDOW_MS`](middleware::loader::trust::REKEY_WINDOW_MS) and logs
//! it on `sys/log`.

pub use middleware::loader::trust::PRESENCE_HOLD_MS;

/// Sampling period of the BOOT button.
pub const POLL_MS: u64 = 50;

/// Hold-detector state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Hold {
    /// Not yet seen released since boot.
    #[default]
    Unarmed,
    /// Released; the next press starts timing.
    Released,
    /// Pressed since `since_ms`; presence not yet asserted.
    Pressed {
        /// When the press was first seen.
        since_ms: u64,
    },
    /// Presence asserted for this press; waits for release.
    Fired,
}

impl Hold {
    /// Feeds one sample (`Some(true)` pressed, `Some(false)` released,
    /// `None` unreadable) taken at `now_ms`. Returns the next state and
    /// whether presence is asserted by this sample.
    pub const fn step(self, sample: Option<bool>, now_ms: u64) -> (Hold, bool) {
        let pressed = matches!(sample, Some(true));
        match (self, pressed) {
            (_, false) => (Hold::Released, false),
            (Hold::Unarmed, true) => (Hold::Unarmed, false),
            (Hold::Released, true) => (Hold::Pressed { since_ms: now_ms }, false),
            (Hold::Pressed { since_ms }, true) => {
                if now_ms.saturating_sub(since_ms) >= PRESENCE_HOLD_MS {
                    (Hold::Fired, true)
                } else {
                    (self, false)
                }
            }
            (Hold::Fired, true) => (Hold::Fired, false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs samples taken every `POLL_MS` from t = 0; returns the times at
    /// which presence fired.
    fn run(samples: &[Option<bool>]) -> heapless::Vec<u64, 8> {
        let mut h = Hold::default();
        let mut fired = heapless::Vec::new();
        for (i, &s) in samples.iter().enumerate() {
            let now = i as u64 * POLL_MS;
            let (next, f) = h.step(s, now);
            h = next;
            if f {
                let _ = fired.push(now);
            }
        }
        fired
    }

    fn pressed_for(ms: u64) -> impl Iterator<Item = Option<bool>> {
        core::iter::repeat_n(Some(true), (ms / POLL_MS) as usize + 1)
    }

    #[test]
    fn three_second_hold_after_release_fires_once() {
        let mut s: heapless::Vec<Option<bool>, 256> = heapless::Vec::new();
        s.push(Some(false)).unwrap();
        s.extend(pressed_for(5_000));
        let fired = run(&s);
        // Press seen at 50 ms; fires at 50 + 3000.
        assert_eq!(fired.as_slice(), &[3_050]);
    }

    #[test]
    fn short_press_does_not_fire_and_release_resets() {
        let mut s: heapless::Vec<Option<bool>, 256> = heapless::Vec::new();
        s.push(Some(false)).unwrap();
        s.extend(pressed_for(2_900));
        s.push(Some(false)).unwrap();
        s.extend(pressed_for(2_900));
        assert!(run(&s).is_empty());
    }

    #[test]
    fn held_from_boot_never_fires_until_released() {
        let mut s: heapless::Vec<Option<bool>, 256> = heapless::Vec::new();
        s.extend(pressed_for(5_000));
        assert!(run(&s).is_empty());
        s.push(Some(false)).unwrap();
        s.extend(pressed_for(3_000));
        assert_eq!(run(&s).len(), 1);
    }

    #[test]
    fn unreadable_pin_counts_as_released() {
        let mut s: heapless::Vec<Option<bool>, 256> = heapless::Vec::new();
        s.push(Some(false)).unwrap();
        s.extend(pressed_for(2_000));
        s.push(None).unwrap();
        s.extend(pressed_for(2_000));
        assert!(run(&s).is_empty());
        assert_eq!(Hold::Released.step(None, 0), (Hold::Released, false));
    }

    #[test]
    fn second_press_fires_again() {
        let mut s: heapless::Vec<Option<bool>, 256> = heapless::Vec::new();
        s.push(Some(false)).unwrap();
        s.extend(pressed_for(3_000));
        s.push(Some(false)).unwrap();
        s.extend(pressed_for(3_000));
        assert_eq!(run(&s).len(), 2);
    }
}
