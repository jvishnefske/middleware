//! Export glue called by the code that [`payload!`](crate::payload) generates.
//!
//! These functions are safe and generic over the [`Payload`] type; the macro
//! only adds the `extern "C"` signatures. Application code does not call them
//! directly.
//!
//! * [`init`] builds the state with [`Payload::init`] once.
//! * [`tick`], [`on_message`] and [`on_irq`] dispatch to the state, building
//!   it first if the host skipped `init`.
//! * [`on_irq`] decodes its four arguments with
//!   [`Event::decode`](crate::irq::Event::decode) and silently ignores events
//!   it cannot decode (an unknown kind from a newer host, or out-of-range
//!   arguments), so it never panics.
//! * [`alloc`] returns the address of the fixed receive buffer for any length
//!   in `0..=MAX_MESSAGE`, and `0` (an invalid buffer) otherwise.
//! * [`on_message`] accepts only deliveries into that buffer, copies the bytes
//!   to the stack and then calls [`Payload::on_message`], so a re-entrant
//!   `mw_alloc` cannot alias the slice the payload is reading.
//!
//! Calls that arrive while the state is already borrowed (re-entry from inside
//! a callback) are dropped rather than panicking.

use crate::irq::Event;
use crate::{GuestCell, MAX_MESSAGE, Payload, Slot};

/// Payload state cell type used by the generated exports.
pub type State<P> = GuestCell<Option<P>>;

/// Receive buffer type used by the generated exports.
pub type RxBuffer = GuestCell<[u8; MAX_MESSAGE]>;

/// `init` export: builds the state unless it already exists.
pub fn init<P: Payload>(state: &State<P>) {
    let missing = state.borrow_mut().is_some_and(|s| s.is_none());
    if missing {
        // Build outside the borrow so `P::init` may call any SDK function.
        let built = P::init();
        if let Some(mut slot) = state.borrow_mut()
            && slot.is_none()
        {
            *slot = Some(built);
        }
    }
}

/// Runs `f` on the state, building it first if needed.
fn with_state<P: Payload>(state: &State<P>, f: impl FnOnce(&mut P)) {
    init(state);
    if let Some(mut guard) = state.borrow_mut()
        && let Some(app) = guard.as_mut()
    {
        f(app);
    }
}

/// `tick` export.
pub fn tick<P: Payload>(state: &State<P>, now_ms: i32) {
    with_state(state, |app| app.tick(now_ms as u32));
}

/// `on_irq` export: decodes the event and calls [`Payload::on_irq`].
pub fn on_irq<P: Payload>(state: &State<P>, kind: i32, source: i32, value: i32, time_us: i32) {
    if let Some(event) = Event::decode(kind, source, value, time_us) {
        with_state(state, |app| app.on_irq(event));
    }
}

/// Validates a requested delivery length: `Some(len)` for `0..=MAX_MESSAGE`.
pub fn delivery_len(len: i32) -> Option<usize> {
    usize::try_from(len).ok().filter(|&l| l <= MAX_MESSAGE)
}

/// Converts a buffer address to the ABI pointer; `0` if it does not fit
/// (never on wasm32).
fn addr_i32(addr: usize) -> i32 {
    u32::try_from(addr).map(|a| a as i32).unwrap_or(0)
}

/// `mw_alloc` export: the receive buffer's address, or `0` if `len` is too
/// large or negative.
pub fn alloc(rx: &RxBuffer, len: i32) -> i32 {
    match delivery_len(len) {
        Some(_) => addr_i32(rx.addr()),
        None => 0,
    }
}

/// `on_message` export: checks that the host delivered into the receive
/// buffer, then [`deliver`]s.
pub fn on_message<P: Payload>(state: &State<P>, rx: &RxBuffer, slot: i32, ptr: i32, len: i32) {
    let expected = addr_i32(rx.addr());
    if expected != 0 && ptr == expected {
        deliver(state, rx, slot, len);
    }
}

/// Copies `len` bytes out of the receive buffer and calls
/// [`Payload::on_message`]. Invalid slots or lengths are ignored.
pub fn deliver<P: Payload>(state: &State<P>, rx: &RxBuffer, slot: i32, len: i32) {
    let (Some(slot), Some(len)) = (Slot::from_raw(slot), delivery_len(len)) else {
        return;
    };
    let mut local = [0u8; MAX_MESSAGE];
    match rx.borrow_mut() {
        // A zip, not `copy_from_slice`: under `opt-level = "z"` that stays
        // an out-of-line call whose length-mismatch panic drags core's
        // panic formatting into every payload.
        Some(buf) => local
            .iter_mut()
            .zip(buf.iter())
            .take(len)
            .for_each(|(d, s)| *d = *s),
        None => return,
    }
    with_state(state, |app| {
        app.on_message(slot, local.get(..len).unwrap_or(&[]))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Rec {
        inits: u32,
        ticks: u32,
        last_now: u32,
        got: Option<(Slot, [u8; 4], usize)>,
        irqs: u32,
        last_irq: Option<Event>,
    }

    impl Payload for Rec {
        fn init() -> Self {
            Rec {
                inits: 1,
                ..Rec::default()
            }
        }
        fn tick(&mut self, now_ms: u32) {
            self.ticks += 1;
            self.last_now = now_ms;
        }
        fn on_message(&mut self, slot: Slot, data: &[u8]) {
            let mut head = [0u8; 4];
            let n = data.len().min(4);
            head[..n].copy_from_slice(&data[..n]);
            self.got = Some((slot, head, data.len()));
        }
        fn on_irq(&mut self, event: Event) {
            self.irqs += 1;
            self.last_irq = Some(event);
        }
    }

    fn get<R>(state: &State<Rec>, f: impl FnOnce(&Rec) -> R) -> R {
        let guard = state.borrow_mut().expect("not borrowed");
        f(guard.as_ref().expect("initialised"))
    }

    #[test]
    fn init_runs_once_and_tick_dispatches() {
        let state: State<Rec> = GuestCell::new(None);
        init(&state);
        init(&state);
        tick(&state, 1234);
        tick(&state, -1);
        get(&state, |r| {
            assert_eq!(r.inits, 1);
            assert_eq!(r.ticks, 2);
            assert_eq!(r.last_now, u32::MAX);
        });
    }

    #[test]
    fn tick_builds_state_when_init_was_skipped() {
        let state: State<Rec> = GuestCell::new(None);
        tick(&state, 5);
        get(&state, |r| assert_eq!((r.inits, r.ticks), (1, 1)));
    }

    #[test]
    fn delivery_len_bounds() {
        assert_eq!(delivery_len(0), Some(0));
        assert_eq!(delivery_len(MAX_MESSAGE as i32), Some(MAX_MESSAGE));
        assert_eq!(delivery_len(MAX_MESSAGE as i32 + 1), None);
        assert_eq!(delivery_len(-1), None);
    }

    #[test]
    fn deliver_copies_from_the_receive_buffer() {
        let state: State<Rec> = GuestCell::new(None);
        let rx: RxBuffer = GuestCell::new([0; MAX_MESSAGE]);
        rx.borrow_mut().expect("free")[..5].copy_from_slice(b"hello");
        deliver(&state, &rx, 3, 5);
        get(&state, |r| {
            let (slot, head, len) = r.got.expect("delivered");
            assert_eq!(slot.index(), 3);
            assert_eq!(&head, b"hell");
            assert_eq!(len, 5);
        });
    }

    #[test]
    fn deliver_ignores_bad_slot_or_length() {
        let state: State<Rec> = GuestCell::new(None);
        let rx: RxBuffer = GuestCell::new([0; MAX_MESSAGE]);
        deliver(&state, &rx, -1, 1);
        deliver(&state, &rx, crate::MAX_SUBSCRIPTIONS as i32, 1);
        deliver(&state, &rx, 0, MAX_MESSAGE as i32 + 1);
        assert!(state.borrow_mut().expect("free").is_none());
    }

    #[test]
    fn on_message_rejects_foreign_pointers() {
        let state: State<Rec> = GuestCell::new(None);
        let rx: RxBuffer = GuestCell::new([0; MAX_MESSAGE]);
        on_message(&state, &rx, 0, 0, 1);
        assert!(state.borrow_mut().expect("free").is_none());
    }

    #[test]
    fn reentrant_calls_are_dropped() {
        let state: State<Rec> = GuestCell::new(None);
        init(&state);
        let _held = state.borrow_mut();
        tick(&state, 1);
        drop(_held);
        get(&state, |r| assert_eq!(r.ticks, 0));
    }

    #[test]
    fn on_irq_decodes_and_dispatches() {
        let state: State<Rec> = GuestCell::new(None);
        on_irq(&state, 1, 4, 0, 77);
        get(&state, |r| {
            assert_eq!(r.inits, 1, "built on first event");
            assert_eq!(r.irqs, 1);
            assert_eq!(
                r.last_irq,
                Some(Event::Gpio {
                    pin: 4,
                    high: false,
                    time_us: 77
                })
            );
        });
    }

    #[test]
    fn on_irq_ignores_unknown_kinds_without_panicking() {
        let state: State<Rec> = GuestCell::new(None);
        init(&state);
        for kind in [0, 8, -1, i32::MIN, i32::MAX] {
            on_irq(&state, kind, 0, 1, 0);
        }
        on_irq(&state, 2, 0, 0, 0); // timer with 0 periods: malformed
        get(&state, |r| assert_eq!(r.irqs, 0));
    }

    #[test]
    fn reentrant_on_irq_is_dropped() {
        let state: State<Rec> = GuestCell::new(None);
        init(&state);
        let held = state.borrow_mut();
        on_irq(&state, 2, 1, 1, 5);
        drop(held);
        get(&state, |r| assert_eq!(r.irqs, 0));
        on_irq(&state, 2, 1, 1, 5);
        get(&state, |r| assert_eq!(r.irqs, 1));
    }
}
