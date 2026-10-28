//! Who holds a DMA peripheral's driver and buffers: the idle parts, or a
//! running transfer that owns them (D6, R12).
//!
//! ```text
//!            start(f) ── f fails ──▶ Idle(parts)      (parts handed back)
//!   Idle(parts) ──start(f)──▶ InFlight(transfer)
//!    ▲                              │
//!    └──── reclaim() / close() ─────┘   (close cancels first)
//! ```
//!
//! esp-hal's `SpiDma::transfer_buffers` moves the driver and both DMA
//! buffers into an `SpiDmaTransfer`, and `wait` moves them back out. The
//! buffers live in statics that are taken once at boot, so a driver or
//! buffer dropped on the way is gone until reset. [`DmaHw`] has exactly two
//! states and every transition takes the state by value and returns the
//! next one, so the parts are always in one of them:
//!
//! * [`DmaHw::start`] runs `f` on the idle parts; `f` either returns the
//!   transfer or gives the parts back with its error. Starting while a
//!   transfer runs is `Busy` and leaves that transfer in place.
//! * [`DmaHw::reclaim`] turns a finished (or cancelled) transfer back into
//!   idle parts; on idle parts it does nothing.
//! * [`DmaHw::close`] is the payload-stop path: whatever the state, it
//!   cancels a running transfer and returns the parts.
//!
//! The shell keeps the state in a [`HwSlot`], which lends it out by value
//! for one synchronous step ([`HwSlot::step`]) and takes the next state
//! back; the step's closure must return a [`DmaHw`], so it cannot drop the
//! parts. The slot is only empty while a step runs (a panic inside one
//! halts this firmware, so no caller observes it).
//!
//! Hardware stays out of this module: a [`Transfer`] says how to cancel and
//! finish one, and the shell implements it for esp-hal's transfer.

use middleware::wasm_abi::AbiError;

/// A running transfer that owns its peripheral's parts.
pub trait Transfer: Sized {
    /// The driver and buffers at rest.
    type Parts;

    /// Stops the transfer early; [`Transfer::finish`] still follows.
    fn cancel(&mut self);

    /// Waits for the (finished or cancelled) transfer and returns the
    /// parts.
    fn finish(self) -> Self::Parts;
}

/// The two places a peripheral's parts can be.
pub enum DmaHw<T: Transfer> {
    /// No transfer runs; the parts are here.
    Idle(T::Parts),
    /// A transfer runs and owns the parts.
    InFlight(T),
}

impl<T: Transfer> DmaHw<T> {
    /// Whether a transfer runs.
    pub fn in_flight(&self) -> bool {
        matches!(self, DmaHw::InFlight(_))
    }

    /// The idle parts; `None` while a transfer runs.
    pub fn parts(&self) -> Option<&T::Parts> {
        match self {
            DmaHw::Idle(p) => Some(p),
            DmaHw::InFlight(_) => None,
        }
    }

    /// The idle parts, mutably; `None` while a transfer runs.
    pub fn parts_mut(&mut self) -> Option<&mut T::Parts> {
        match self {
            DmaHw::Idle(p) => Some(p),
            DmaHw::InFlight(_) => None,
        }
    }

    /// The running transfer, to await or cancel it in place.
    pub fn transfer_mut(&mut self) -> Option<&mut T> {
        match self {
            DmaHw::InFlight(t) => Some(t),
            DmaHw::Idle(_) => None,
        }
    }

    /// Starts a transfer with `f` on the idle parts. `f` returns the
    /// transfer, or the parts with its error (the state stays idle).
    /// `Busy`, unchanged, while a transfer runs.
    pub fn start(
        self,
        f: impl FnOnce(T::Parts) -> Result<T, (AbiError, T::Parts)>,
    ) -> (Self, Result<(), AbiError>) {
        match self {
            DmaHw::InFlight(t) => (DmaHw::InFlight(t), Err(AbiError::Busy)),
            DmaHw::Idle(parts) => match f(parts) {
                Ok(t) => (DmaHw::InFlight(t), Ok(())),
                Err((e, parts)) => (DmaHw::Idle(parts), Err(e)),
            },
        }
    }

    /// Takes the parts back from a finished or cancelled transfer; idle
    /// parts stay as they are.
    pub fn reclaim(self) -> Self {
        match self {
            DmaHw::InFlight(t) => DmaHw::Idle(t.finish()),
            idle @ DmaHw::Idle(_) => idle,
        }
    }

    /// Payload stop: cancels a running transfer and returns the parts, in
    /// any state.
    pub fn close(self) -> T::Parts {
        match self {
            DmaHw::Idle(parts) => parts,
            DmaHw::InFlight(mut t) => {
                t.cancel();
                t.finish()
            }
        }
    }
}

/// Holds a [`DmaHw`] behind `&mut` and lends it by value for one step.
pub struct HwSlot<T: Transfer>(Option<DmaHw<T>>);

impl<T: Transfer> HwSlot<T> {
    /// A slot holding idle `parts`.
    pub fn new(parts: T::Parts) -> Self {
        HwSlot(Some(DmaHw::Idle(parts)))
    }

    /// The state, for in-place reads and awaits.
    pub fn get_mut(&mut self) -> Option<&mut DmaHw<T>> {
        self.0.as_mut()
    }

    /// The state, read only.
    pub fn get(&self) -> Option<&DmaHw<T>> {
        self.0.as_ref()
    }

    /// Runs one transition: `f` gets the state by value and must return
    /// the next state with its result. `None` only if the slot is empty,
    /// which no completed step leaves it.
    pub fn step<R>(&mut self, f: impl FnOnce(DmaHw<T>) -> (DmaHw<T>, R)) -> Option<R> {
        let (next, r) = f(self.0.take()?);
        self.0 = Some(next);
        Some(r)
    }

    /// Payload stop: [`DmaHw::close`] on the held state.
    pub fn close(self) -> Option<T::Parts> {
        self.0.map(DmaHw::close)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stands in for the driver and the two static DMA buffers: not
    /// `Clone`, so getting the same `id` back proves nothing was lost.
    #[derive(Debug, PartialEq, Eq)]
    struct Parts {
        id: u8,
        transfers: u8,
        cancelled: u8,
    }

    struct Xfer {
        parts: Parts,
        cancelled: bool,
    }

    impl Transfer for Xfer {
        type Parts = Parts;
        fn cancel(&mut self) {
            self.cancelled = true;
        }
        fn finish(self) -> Parts {
            let Parts { id, transfers, cancelled } = self.parts;
            Parts { id, transfers: transfers + 1, cancelled: cancelled + u8::from(self.cancelled) }
        }
    }

    fn parts(id: u8) -> Parts {
        Parts { id, transfers: 0, cancelled: 0 }
    }

    fn go(p: Parts) -> Result<Xfer, (AbiError, Parts)> {
        Ok(Xfer { parts: p, cancelled: false })
    }

    fn start(slot: &mut HwSlot<Xfer>) -> Result<(), AbiError> {
        slot.step(|hw| hw.start(go)).unwrap_or(Err(AbiError::Unsupported))
    }

    fn reclaim(slot: &mut HwSlot<Xfer>) {
        slot.step(|hw| (hw.reclaim(), ()));
    }

    #[test]
    fn r12_stop_while_idle_returns_the_parts() {
        // The hardware bug: closing an idle bus dropped the parts.
        let slot = HwSlot::<Xfer>::new(parts(7));
        assert_eq!(slot.close(), Some(parts(7)));
        let mut slot = HwSlot::<Xfer>::new(parts(7));
        reclaim(&mut slot);
        assert!(slot.get().and_then(DmaHw::parts).is_some(), "reclaim on idle keeps the parts");
        assert_eq!(slot.close(), Some(parts(7)));
    }

    #[test]
    fn r12_stop_during_a_payload_transfer_cancels_and_returns_the_parts() {
        let mut slot = HwSlot::<Xfer>::new(parts(1));
        assert_eq!(start(&mut slot), Ok(()));
        assert!(slot.get().is_some_and(DmaHw::in_flight));
        assert_eq!(slot.close(), Some(Parts { id: 1, transfers: 1, cancelled: 1 }));
    }

    #[test]
    fn r12_stop_during_a_route_write_cancels_and_returns_the_parts() {
        // A sink-route write starts, then its await is abandoned mid-frame
        // (the transfer is left in the slot) and the payload stops.
        let mut slot = HwSlot::<Xfer>::new(parts(2));
        assert_eq!(start(&mut slot), Ok(()));
        reclaim(&mut slot);
        assert_eq!(start(&mut slot), Ok(()));
        if let Some(t) = slot.get_mut().and_then(DmaHw::transfer_mut) {
            t.cancel(); // a route timeout cancels in place
        }
        assert_eq!(slot.close(), Some(Parts { id: 2, transfers: 2, cancelled: 1 }));
    }

    #[test]
    fn r12_starting_while_in_flight_is_busy_and_keeps_the_transfer() {
        let mut slot = HwSlot::<Xfer>::new(parts(3));
        assert_eq!(start(&mut slot), Ok(()));
        assert_eq!(start(&mut slot), Err(AbiError::Busy));
        assert!(slot.get().is_some_and(DmaHw::in_flight));
        reclaim(&mut slot);
        assert_eq!(slot.get().and_then(DmaHw::parts), Some(&Parts { id: 3, transfers: 1, cancelled: 0 }));
    }

    #[test]
    fn r12_a_transfer_that_fails_to_start_hands_the_parts_back() {
        let mut slot = HwSlot::<Xfer>::new(parts(4));
        let r = slot.step(|hw| hw.start(|p| Err((AbiError::Transport, p))));
        assert_eq!(r, Some(Err(AbiError::Transport)));
        assert_eq!(slot.get().and_then(DmaHw::parts), Some(&parts(4)));
    }

    #[test]
    fn r12_parts_survive_any_number_of_payload_runs() {
        // Each run opens the parts, maybe leaves a transfer running, and
        // stops; the next run gets the same parts.
        let mut home = Some(parts(9));
        for run in 0..6u8 {
            let mut slot = HwSlot::<Xfer>::new(home.take().unwrap_or_else(|| parts(0)));
            match run % 3 {
                0 => {}
                1 => assert_eq!(start(&mut slot), Ok(())),
                _ => {
                    assert_eq!(start(&mut slot), Ok(()));
                    reclaim(&mut slot);
                }
            }
            home = slot.close();
            assert_eq!(home.as_ref().map(|p| p.id), Some(9), "run {run}");
        }
    }
}
