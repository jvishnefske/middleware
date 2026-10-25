//! The state machine of one DMA channel (SPI bus 0 or UART 1 transmit).
//!
//! ```text
//!   Idle ──start(len)──▶ InFlight{len} ──complete(Ok)──▶ Done{len}
//!    ▲                        │                             │
//!    │                        └──complete(Err)──▶ Idle      │
//!    └──────────────────────── take() ──────────────────────┘
//! ```
//!
//! * [`DmaState::start`] refuses with [`AbiError::Busy`] while a transfer is
//!   in flight; starting from `Done` discards unread data.
//! * [`DmaState::complete`] returns the `DmaDone` event value: the byte
//!   count, or the negative error code.
//! * [`DmaState::take`] backs `dma_read`: `Busy` while in flight, the
//!   received length once (`Done → Idle`), then 0 until the next transfer
//!   completes. The received bytes are consumed by the first read, as in the
//!   simulators.
//! * [`DmaState::abort`] is the payload-stop path: whatever the state, the
//!   channel ends `Idle`.

use middleware::wasm_abi::AbiError;

/// One channel's transfer state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DmaState {
    /// No transfer, nothing to read.
    #[default]
    Idle,
    /// A transfer of `len` bytes is running.
    InFlight {
        /// Bytes being clocked.
        len: usize,
    },
    /// A transfer finished; `len` received bytes wait for `dma_read`.
    Done {
        /// Bytes received.
        len: usize,
    },
}

impl DmaState {
    /// Starts a transfer of `len` bytes.
    pub const fn start(self, len: usize) -> Result<Self, AbiError> {
        match self {
            DmaState::InFlight { .. } => Err(AbiError::Busy),
            DmaState::Idle | DmaState::Done { .. } => Ok(DmaState::InFlight { len }),
        }
    }

    /// Whether a transfer is running.
    pub const fn in_flight(&self) -> bool {
        matches!(self, DmaState::InFlight { .. })
    }

    /// Finishes the running transfer. Returns the new state and the
    /// `DmaDone` value (length, or negative error code); `None` when no
    /// transfer was running.
    pub const fn complete(self, result: Result<(), AbiError>) -> (Self, Option<i32>) {
        match (self, result) {
            (DmaState::InFlight { len }, Ok(())) => {
                let value = if len > i32::MAX as usize { i32::MAX } else { len as i32 };
                (DmaState::Done { len }, Some(value))
            }
            (DmaState::InFlight { .. }, Err(e)) => (DmaState::Idle, Some(e.code())),
            (other, _) => (other, None),
        }
    }

    /// Takes the received length for `dma_read`: `Busy` while in flight,
    /// the length once after completion, 0 otherwise.
    pub const fn take(self) -> Result<(Self, usize), AbiError> {
        match self {
            DmaState::InFlight { .. } => Err(AbiError::Busy),
            DmaState::Done { len } => Ok((DmaState::Idle, len)),
            DmaState::Idle => Ok((DmaState::Idle, 0)),
        }
    }

    /// The state after a payload stop: always `Idle`.
    pub const fn abort(self) -> Self {
        DmaState::Idle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn d6_transfer_lifecycle_and_consuming_read() {
        let s = DmaState::Idle;
        assert_eq!(s.take(), Ok((DmaState::Idle, 0)), "nothing received yet");
        let s = s.start(8).unwrap();
        assert!(s.in_flight());
        assert_eq!(s.start(4), Err(AbiError::Busy), "second transfer while in flight");
        assert_eq!(s.take(), Err(AbiError::Busy), "dma_read while in flight");
        let (s, value) = s.complete(Ok(()));
        assert_eq!((s, value), (DmaState::Done { len: 8 }, Some(8)));
        let (s, n) = s.take().unwrap();
        assert_eq!(n, 8);
        assert_eq!(s.take(), Ok((DmaState::Idle, 0)), "consumed by the first read");
    }

    #[test]
    fn d6_errors_and_aborts_return_to_idle() {
        let (s, value) = DmaState::Idle.start(3).unwrap().complete(Err(AbiError::Transport));
        assert_eq!((s, value), (DmaState::Idle, Some(AbiError::Transport.code())));
        assert_eq!(DmaState::Idle.complete(Ok(())), (DmaState::Idle, None), "spurious completion");
        assert_eq!(DmaState::Idle.start(3).unwrap().abort(), DmaState::Idle);
        // Starting again from Done discards unread data.
        let done = DmaState::Done { len: 4 };
        assert_eq!(done.start(2), Ok(DmaState::InFlight { len: 2 }));
    }

    #[test]
    fn d7_uart_tx_uses_the_same_machine() {
        let s = DmaState::Idle.start(5).unwrap();
        assert_eq!(s.start(1), Err(AbiError::Busy));
        let (s, v) = s.complete(Ok(()));
        assert_eq!(v, Some(5));
        assert!(s.start(1).is_ok());
    }
}
