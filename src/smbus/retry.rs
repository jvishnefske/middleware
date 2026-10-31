//! What to do when a block write fails: the bounded retry policy and its
//! deterministic, address-derived backoff (pure).
//!
//! The unit of retry is the **block**: a failed block is written again,
//! the fragments before it stand (their targets have them), and the
//! message continues. Retrying whole messages would resend fragments that
//! arrived and multiply the bus time a collision costs.
//!
//! | failure ([`Fault`])                                    | unicast | broadcast |
//! |--------------------------------------------------------|---------|-----------|
//! | `ArbitrationLoss`                                      | back off, retry, at most [`RetryPolicy::arbitration_attempts`] losses, then [`GiveUp::ArbitrationLost`] | same |
//! | `NoAcknowledge(Address)`                               | back off, retry, at most [`RetryPolicy::nack_attempts`] NACKs, then [`GiveUp::NoAcknowledge`] | [`Decision::Unheard`]: no target answers the broadcast address, the message is dropped (fire and forget) |
//! | `NoAcknowledge(Data)` / `NoAcknowledge(Unknown)`       | as an address NACK                       | back off, retry, then [`GiveUp::NoAcknowledge`] |
//! | `Bus`, `Overrun`, `Other`                              | [`Decision::Fatal`]: propagate at once   | same |
//!
//! Arbitration losses and NACKs are counted separately, so one block is
//! written at most `arbitration_attempts + nack_attempts - 1` times: never
//! an unbounded loop. With [`RetryPolicy::DEFAULT`] that is 10 writes and
//! under 250 ms of backoff in the worst case.
//!
//! # Backoff
//!
//! After the `k`-th failure of a block the master waits
//! [`RetryPolicy::backoff_us`]`(own, k)`: `slot_us × (1 + h mod W) +
//! ⌊own × slot_us / 128⌋`, where the window `W = 2^(min(k-1,
//! max_doublings) + 1)` slots doubles per failure and `h` is a fixed
//! integer hash of the node's own address and `k`. The second term is a
//! sub-slot offset unique to each 7-bit address, so two masters that
//! collided (they have different addresses) never wait the same time when
//! `slot_us >= 128`, and the hashed slot term spreads them further apart.
//! The schedule is reproducible: no RNG, no float. The wait is always
//! within `[slot_us, slot_us × (W + 1))`.
//!
//! # What belongs to the HAL
//!
//! The SMBus timing obligations are the I2C peripheral's, not this
//! layer's: the clock-low timeout (a device that holds SCL low for
//! `T_TIMEOUT` 25..=35 ms resets its interface; a master that sees it
//! reports a bus error), the bus free time between STOP and START
//! (`T_BUF` 4.7 µs at 100 kHz, 1.3 µs at 400 kHz), detecting a busy bus
//! before START and arbitration itself. This layer only sees their
//! outcome as an `embedded_hal::i2c::ErrorKind`.

use super::address::Address;
use embedded_hal::i2c::{ErrorKind, NoAcknowledgeSource};

/// A failed block write, as this layer classifies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// Another master won arbitration.
    ArbitrationLoss,
    /// The address byte was not acknowledged.
    NackAddress,
    /// A data byte (or an unknown byte) was not acknowledged.
    NackData,
    /// A bus error, overrun or other failure.
    Bus,
}

impl Fault {
    /// Classifies an `embedded-hal` I2C error kind.
    pub const fn from_kind(kind: ErrorKind) -> Fault {
        match kind {
            ErrorKind::ArbitrationLoss => Fault::ArbitrationLoss,
            ErrorKind::NoAcknowledge(NoAcknowledgeSource::Address) => Fault::NackAddress,
            ErrorKind::NoAcknowledge(_) => Fault::NackData,
            _ => Fault::Bus,
        }
    }
}

/// Why a block was given up on after its retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GiveUp {
    /// Arbitration was lost on every allowed attempt.
    ArbitrationLost,
    /// The destination did not acknowledge on every allowed attempt.
    NoAcknowledge,
}

/// Failures of one block so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tries {
    /// Arbitration losses.
    pub arbitration: u8,
    /// NACKs.
    pub nack: u8,
}

impl Tries {
    /// All failures.
    pub const fn total(self) -> u8 {
        self.arbitration.saturating_add(self.nack)
    }
}

/// What to do after a failed block write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Wait `delay_us`, then write the block again; `tries` counts this
    /// failure.
    Retry {
        /// Backoff before the next attempt.
        delay_us: u32,
        /// Failures including this one.
        tries: Tries,
    },
    /// Nobody acknowledged the broadcast address: drop the message.
    Unheard,
    /// Stop retrying.
    GiveUp(GiveUp),
    /// Propagate the bus error.
    Fatal,
}

/// Bounds and timing of block retries (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Arbitration losses a block may suffer; the last one gives up.
    pub arbitration_attempts: u8,
    /// NACKs a block may suffer; the last one gives up.
    pub nack_attempts: u8,
    /// One backoff slot, about one block's time on the bus (a 35-byte
    /// transaction takes about 3.2 ms at 100 kHz, 0.8 ms at 400 kHz).
    pub slot_us: u32,
    /// How often the window doubles, at most.
    pub max_doublings: u8,
}

impl RetryPolicy {
    /// 8 arbitration losses, 3 NACKs, 1 ms slots, windows up to 32 slots.
    pub const DEFAULT: RetryPolicy = RetryPolicy {
        arbitration_attempts: 8,
        nack_attempts: 3,
        slot_us: 1_000,
        max_doublings: 4,
    };

    /// The most writes one block can take: every allowed loss and NACK but
    /// one of each, plus the failure that gives up (at least one write).
    pub const fn max_writes(&self) -> usize {
        let n = self.arbitration_attempts as usize + self.nack_attempts as usize;
        if n <= 1 { 1 } else { n - 1 }
    }

    /// The backoff after the `failure`-th failure (1-based) of a block sent
    /// by `own` (see the module docs).
    pub const fn backoff_us(&self, own: Address, failure: u8) -> u32 {
        let k = if failure == 0 { 1 } else { failure };
        let doublings = if k - 1 < self.max_doublings {
            k - 1
        } else {
            self.max_doublings
        };
        let doublings = if doublings > 20 { 20 } else { doublings };
        let window = 2u32 << doublings;
        let h = mix(own.get(), k);
        let slots = 1 + h % window;
        // own < 128, so the offset is below one slot.
        let sub = (own.get() as u64 * self.slot_us as u64 / 128) as u32;
        self.slot_us.saturating_mul(slots).saturating_add(sub)
    }

    /// What to do after `fault` on a block to a unicast (`broadcast` false)
    /// or broadcast destination, given the block's failures `before` this
    /// one (pure; see the module docs).
    pub const fn decide(
        &self,
        own: Address,
        fault: Fault,
        broadcast: bool,
        before: Tries,
    ) -> Decision {
        let tries = match fault {
            Fault::ArbitrationLoss => Tries {
                arbitration: before.arbitration.saturating_add(1),
                ..before
            },
            Fault::NackAddress | Fault::NackData => Tries {
                nack: before.nack.saturating_add(1),
                ..before
            },
            Fault::Bus => return Decision::Fatal,
        };
        if broadcast && matches!(fault, Fault::NackAddress) {
            return Decision::Unheard;
        }
        if tries.arbitration >= self.arbitration_attempts && tries.arbitration > before.arbitration
        {
            return Decision::GiveUp(GiveUp::ArbitrationLost);
        }
        if tries.nack >= self.nack_attempts && tries.nack > before.nack {
            return Decision::GiveUp(GiveUp::NoAcknowledge);
        }
        Decision::Retry {
            delay_us: self.backoff_us(own, tries.total()),
            tries,
        }
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// A fixed integer hash of an address and a failure number.
const fn mix(address: u8, failure: u8) -> u32 {
    let mut h =
        (address as u32).wrapping_mul(0x9E37_79B1) ^ (failure as u32).wrapping_mul(0x85EB_CA6B);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    h
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    const P: RetryPolicy = RetryPolicy::DEFAULT;

    fn nodes() -> impl Iterator<Item = Address> {
        (0u8..=0x7F).filter_map(|raw| Address::node(raw).ok())
    }

    #[test]
    fn m6_backoff_stays_within_its_window() {
        for own in nodes() {
            for k in 1..=20u8 {
                let w = 2u32 << (k - 1).min(P.max_doublings);
                let d = P.backoff_us(own, k);
                assert!(d >= P.slot_us, "{own:?} {k}: {d}");
                assert!(d < P.slot_us * (w + 1), "{own:?} {k}: {d}");
            }
        }
        // The worst case a block can wait over all its retries is bounded.
        let worst: u32 = (1..P.max_writes() as u8)
            .map(|k| P.slot_us * ((2u32 << (k - 1).min(P.max_doublings)) + 1))
            .sum();
        assert!(worst < 250_000, "{worst} µs");
        // Saturates instead of overflowing.
        let huge = RetryPolicy {
            slot_us: u32::MAX,
            max_doublings: 255,
            ..P
        };
        assert_eq!(huge.backoff_us(Address::node(0x10).unwrap(), 255), u32::MAX);
    }

    #[test]
    fn m6_backoff_is_deterministic_and_differs_between_addresses() {
        let a = Address::node(0x10).unwrap();
        let b = Address::node(0x11).unwrap();
        let schedule = |x| {
            (1..=8u8)
                .map(|k| P.backoff_us(x, k))
                .collect::<std::vec::Vec<_>>()
        };
        assert_eq!(schedule(a), schedule(a));
        // Two distinct addresses never wait the same time, at any retry, so
        // two colliding masters separate.
        for k in 1..=8u8 {
            let mut waits: std::vec::Vec<u32> = nodes().map(|x| P.backoff_us(x, k)).collect();
            let n = waits.len();
            waits.sort_unstable();
            waits.dedup();
            assert_eq!(waits.len(), n, "retry {k}");
        }
        assert_ne!(schedule(a), schedule(b));
    }

    #[test]
    fn m6_arbitration_loss_is_retried_a_bounded_number_of_times() {
        let own = Address::node(0x20).unwrap();
        let mut tries = Tries::default();
        let mut retries = 0;
        for _ in 0..100 {
            match P.decide(own, Fault::ArbitrationLoss, false, tries) {
                Decision::Retry { delay_us, tries: t } => {
                    assert_eq!(delay_us, P.backoff_us(own, t.total()));
                    tries = t;
                    retries += 1;
                }
                Decision::GiveUp(GiveUp::ArbitrationLost) => break,
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(retries, P.arbitration_attempts - 1);
    }

    #[test]
    fn m6_nacks_and_bus_errors() {
        let own = Address::node(0x20).unwrap();
        let t0 = Tries::default();
        // Unicast NACKs retry, then give up.
        assert!(matches!(
            P.decide(own, Fault::NackAddress, false, t0),
            Decision::Retry { .. }
        ));
        let t2 = Tries {
            arbitration: 0,
            nack: 2,
        };
        assert_eq!(
            P.decide(own, Fault::NackAddress, false, t2),
            Decision::GiveUp(GiveUp::NoAcknowledge)
        );
        assert_eq!(
            P.decide(own, Fault::NackData, false, t2),
            Decision::GiveUp(GiveUp::NoAcknowledge)
        );
        // A broadcast nobody answers is dropped at once; a data NACK retries.
        assert_eq!(
            P.decide(own, Fault::NackAddress, true, t0),
            Decision::Unheard
        );
        assert!(matches!(
            P.decide(own, Fault::NackData, true, t0),
            Decision::Retry { .. }
        ));
        // Bus errors propagate whatever came before.
        assert_eq!(P.decide(own, Fault::Bus, false, t2), Decision::Fatal);
        // Losses and NACKs are counted apart: a NACK after 7 losses retries.
        let t7 = Tries {
            arbitration: 7,
            nack: 0,
        };
        assert!(matches!(
            P.decide(own, Fault::NackData, false, t7),
            Decision::Retry { .. }
        ));
        assert_eq!(P.max_writes(), 10);
    }

    #[test]
    fn m6_hal_error_kinds_map_to_faults() {
        assert_eq!(
            Fault::from_kind(ErrorKind::ArbitrationLoss),
            Fault::ArbitrationLoss
        );
        assert_eq!(
            Fault::from_kind(ErrorKind::NoAcknowledge(NoAcknowledgeSource::Address)),
            Fault::NackAddress
        );
        assert_eq!(
            Fault::from_kind(ErrorKind::NoAcknowledge(NoAcknowledgeSource::Data)),
            Fault::NackData
        );
        assert_eq!(
            Fault::from_kind(ErrorKind::NoAcknowledge(NoAcknowledgeSource::Unknown)),
            Fault::NackData
        );
        for k in [ErrorKind::Bus, ErrorKind::Overrun, ErrorKind::Other] {
            assert_eq!(Fault::from_kind(k), Fault::Bus);
        }
    }
}
