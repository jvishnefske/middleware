//! [`Transfer`]: the payload-side state of one DMA channel.
//!
//! [`Spi`](crate::spi::Spi) and [`Uart`](crate::uart::Uart) track their
//! in-flight transfer with this pure state machine, so a second start fails
//! with [`AbiError::Busy`] without a host round trip and receive data is only
//! read after completion. Transitions are `const fn`s that take a state and
//! return the next one:
//!
//! ```text
//! Idle ──start──▶ InFlight ──finish(result)──▶ Done(result) ──start──▶ InFlight
//!                    │ start → Err(Busy)
//!                    └──────recover──────▶ Idle
//! ```
//!
//! The host stays authoritative: it also answers `Busy` while its DMA engine
//! is running. If the completion event was lost to queue overflow
//! ([`irq::overflow`](crate::irq::overflow) > 0) the payload calls
//! `recover`, and the next start either succeeds or gets the host's `Busy`.

use crate::AbiError;

/// State of one DMA channel as the payload has observed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Transfer {
    /// Nothing started yet (or recovered).
    #[default]
    Idle,
    /// Started; waiting for the `DmaDone` event.
    InFlight,
    /// Finished with this many bytes, or this error.
    Done(Result<usize, AbiError>),
}

impl Transfer {
    /// The state after starting a transfer, or [`AbiError::Busy`] while one
    /// is in flight.
    pub const fn start(self) -> Result<Self, AbiError> {
        match self {
            Transfer::InFlight => Err(AbiError::Busy),
            Transfer::Idle | Transfer::Done(_) => Ok(Transfer::InFlight),
        }
    }

    /// The state after the channel's `DmaDone` event.
    pub const fn finish(self, result: Result<usize, AbiError>) -> Self {
        Transfer::Done(result)
    }

    /// The state after giving up on a lost completion event.
    pub const fn recover(self) -> Self {
        Transfer::Idle
    }

    /// Whether a transfer is in flight.
    pub const fn is_busy(self) -> bool {
        matches!(self, Transfer::InFlight)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_start_is_busy_until_done() {
        let s = Transfer::Idle.start().expect("idle starts");
        assert!(s.is_busy());
        assert_eq!(s.start(), Err(AbiError::Busy));
        let done = s.finish(Ok(4));
        assert_eq!(done, Transfer::Done(Ok(4)));
        assert!(!done.is_busy());
        assert_eq!(done.start(), Ok(Transfer::InFlight));
    }

    #[test]
    fn recover_unblocks_after_a_lost_event() {
        let s = Transfer::InFlight.recover();
        assert_eq!(s, Transfer::Idle);
        assert_eq!(s.start(), Ok(Transfer::InFlight));
        assert_eq!(
            Transfer::Idle.finish(Err(AbiError::Transport)),
            Transfer::Done(Err(AbiError::Transport))
        );
    }
}
