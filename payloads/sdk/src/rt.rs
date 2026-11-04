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
//!   in `0..=MAX_MESSAGE`, and `0` (an invalid buffer) otherwise;
//!   [`serve_alloc`] does the same for a serving payload's
//!   [`MAX_REQUEST_BUFFER`]-byte buffer (`payload!(T, serve)`), and
//!   [`serve_on_message`] delivers messages from it.
//! * [`on_message`] accepts only deliveries into that buffer of at most
//!   [`MAX_MESSAGE`] bytes, copies the bytes to the stack and then calls
//!   [`Payload::on_message`], so a re-entrant `mw_alloc` cannot alias the
//!   slice the payload is reading.
//! * [`on_request`] (ABI 5) accepts only requests whose meta block, body and
//!   reply space all lie in that buffer ([`request_parts`]), copies the meta
//!   block and body to the stack, calls [`Payload::on_request`] with a stack
//!   reply buffer of the offered capacity, copies the reply into the buffer
//!   and returns [`serve::reply_code`]. A request it cannot place, or a
//!   re-entrant one, answers `Internal` or `Unavailable`.
//!
//! Calls that arrive while the state is already borrowed (re-entry from inside
//! a callback) are dropped rather than panicking.

use crate::irq::Event;
use crate::serve::{
    self, HandlerError, MAX_REPLY, MAX_REQUEST, MAX_REQUEST_BUFFER, REQUEST_META_LEN, Request,
    ServeSlot,
};
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

/// Receive buffer of a payload that serves requests (`payload!(T, serve)`).
pub type ServeBuffer = GuestCell<[u8; MAX_REQUEST_BUFFER]>;

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

/// `mw_alloc` export of a serving payload (ABI 5): the receive buffer's
/// address for any `len` in `0..=MAX_REQUEST_BUFFER`, else `0`.
pub fn serve_alloc(rx: &ServeBuffer, len: i32) -> i32 {
    match usize::try_from(len) {
        Ok(l) if l <= MAX_REQUEST_BUFFER => addr_i32(rx.addr()),
        _ => 0,
    }
}

/// `on_message` export of a serving payload: [`on_message`] on its larger
/// buffer (messages are still at most [`MAX_MESSAGE`] bytes).
pub fn serve_on_message<P: Payload>(
    state: &State<P>,
    rx: &ServeBuffer,
    slot: i32,
    ptr: i32,
    len: i32,
) {
    let expected = addr_i32(rx.addr());
    if expected != 0 && ptr == expected {
        serve_deliver(state, rx, slot, len);
    }
}

/// [`deliver`] from a serving payload's receive buffer.
fn serve_deliver<P: Payload>(state: &State<P>, rx: &ServeBuffer, slot: i32, len: i32) {
    let (Some(slot), Some(len)) = (Slot::from_raw(slot), delivery_len(len)) else {
        return;
    };
    let mut local = [0u8; MAX_MESSAGE];
    match rx.borrow_mut() {
        Some(buf) => copy(&mut local, buf.as_slice(), len),
        None => return,
    }
    with_state(state, |app| {
        app.on_message(slot, local.get(..len).unwrap_or(&[]))
    });
}

/// Where a request's parts lie in a receive buffer of `n` bytes at `base`,
/// from the `on_request` arguments `[slot, req_ptr, req_len, meta_ptr,
/// reply_ptr, reply_cap]`: the slot and the offsets of the meta block,
/// the body and the reply space, or `None` when any part leaves the buffer,
/// the body is over [`MAX_REQUEST`] or the reply capacity over
/// [`MAX_REPLY`].
pub fn request_parts(base: usize, n: usize, args: [i32; 6]) -> Option<RequestParts> {
    let [slot, req_ptr, req_len, meta_ptr, reply_ptr, reply_cap] = args;
    let slot = ServeSlot::from_raw(slot)?;
    // Pointers are 32-bit addresses (the i32 bit pattern); `base` is too.
    let off = |ptr: i32, len: usize| -> Option<usize> {
        let at = (ptr as u32 as usize).checked_sub(base)?;
        (at.checked_add(len)? <= n).then_some(at)
    };
    let req_len = usize::try_from(req_len).ok().filter(|&l| l <= MAX_REQUEST)?;
    let reply_cap = usize::try_from(reply_cap)
        .ok()
        .filter(|&l| l <= MAX_REPLY)?;
    Some(RequestParts {
        slot,
        meta: off(meta_ptr, REQUEST_META_LEN)?,
        req: off(req_ptr, req_len)?,
        req_len,
        reply: off(reply_ptr, reply_cap)?,
        reply_cap,
    })
}

/// Offsets of one request's parts in the receive buffer
/// ([`request_parts`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestParts {
    /// The serve slot.
    pub slot: ServeSlot,
    /// Offset of the meta block.
    pub meta: usize,
    /// Offset of the body.
    pub req: usize,
    /// Body length.
    pub req_len: usize,
    /// Offset of the reply space.
    pub reply: usize,
    /// Reply capacity.
    pub reply_cap: usize,
}

/// `on_request` export (ABI 5): see the module docs.
pub fn on_request<P: Payload>(state: &State<P>, rx: &ServeBuffer, args: [i32; 6]) -> i32 {
    const N: usize = MAX_REQUEST_BUFFER;
    // The buffer's 32-bit address (exact on wasm32; truncated like the
    // pointers in host tests).
    let base = rx.addr() as u32 as usize;
    let Some(parts) = request_parts(base, N, args) else {
        return serve::reply_code(Err(HandlerError::Internal));
    };
    let mut meta = [0u8; REQUEST_META_LEN];
    let mut body = [0u8; MAX_REQUEST];
    match rx.borrow_mut() {
        Some(buf) => {
            copy(&mut meta, buf.get(parts.meta..).unwrap_or(&[]), REQUEST_META_LEN);
            copy(&mut body, buf.get(parts.req..).unwrap_or(&[]), parts.req_len);
        }
        None => return serve::reply_code(Err(HandlerError::Unavailable)),
    }
    let Ok(meta) = serve::decode_meta(&meta) else {
        return serve::reply_code(Err(HandlerError::Internal));
    };
    let req = Request {
        body: body.get(..parts.req_len).unwrap_or(&[]),
        meta,
    };
    let mut reply = [0u8; MAX_REPLY];
    let mut result = Err(HandlerError::Unavailable);
    with_state(state, |app| {
        let out = reply.get_mut(..parts.reply_cap).unwrap_or(&mut []);
        result = app
            .on_request(parts.slot, &req, out)
            .and_then(|n| if n <= parts.reply_cap { Ok(n) } else { Err(HandlerError::Internal) });
    });
    if let Ok(n) = result {
        match rx.borrow_mut() {
            Some(mut buf) => {
                let dst = buf.get_mut(parts.reply..).unwrap_or(&mut []);
                copy(dst, &reply, n);
            }
            None => return serve::reply_code(Err(HandlerError::Unavailable)),
        }
    }
    serve::reply_code(result)
}

/// Copies the first `n` bytes of `src` into `dst` (fewer if either is
/// shorter); a zip, for the reason given in [`deliver`].
fn copy(dst: &mut [u8], src: &[u8], n: usize) {
    dst.iter_mut()
        .zip(src.iter())
        .take(n)
        .for_each(|(d, s)| *d = *s);
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
        fn on_request(
            &mut self,
            slot: ServeSlot,
            req: &Request<'_>,
            reply: &mut [u8],
        ) -> Result<usize, HandlerError> {
            if slot.index() != 1 {
                return Err(HandlerError::NotFound);
            }
            Rec::echo(req, reply)
        }
    }

    fn get<R>(state: &State<Rec>, f: impl FnOnce(&Rec) -> R) -> R {
        let guard = state.borrow_mut().expect("not borrowed");
        f(guard.as_ref().expect("initialised"))
    }

    impl Rec {
        fn echo(req: &Request<'_>, reply: &mut [u8]) -> Result<usize, HandlerError> {
            let n = req.body.len() + 1;
            let out = reply.get_mut(..n).ok_or(HandlerError::BufferTooSmall)?;
            out[0] = req.meta.deadline_ms.map_or(0, |d| d as u8);
            out[1..].copy_from_slice(req.body);
            Ok(n)
        }
    }

    fn serve_args(rx: &ServeBuffer, req_len: i32, reply_cap: i32) -> [i32; 6] {
        let base = rx.addr() as u32 as i32;
        [
            1,
            base.wrapping_add(16),
            req_len,
            base,
            base.wrapping_add(16 + req_len),
            reply_cap,
        ]
    }

    #[test]
    fn on_request_answers_from_the_receive_buffer() {
        let state: State<Rec> = GuestCell::new(None);
        let rx: ServeBuffer = GuestCell::new([0; MAX_REQUEST_BUFFER]);
        {
            let mut b = rx.borrow_mut().expect("free");
            b[..16].copy_from_slice(&serve::encode_meta(
                &middleware::RequestMeta::NONE.with_deadline_ms(9),
            ));
            b[16..19].copy_from_slice(b"abc");
        }
        assert_eq!(on_request(&state, &rx, serve_args(&rx, 3, 8)), 4);
        assert_eq!(&rx.borrow_mut().expect("free")[19..23], &[9, b'a', b'b', b'c']);
        // Too small a reply: the payload's BufferTooSmall.
        assert_eq!(
            on_request(&state, &rx, serve_args(&rx, 3, 2)),
            serve::reply_code(Err(HandlerError::BufferTooSmall))
        );
        // Parts outside the buffer, a bad slot or meta: Internal.
        let internal = serve::reply_code(Err(HandlerError::Internal));
        let mut bad = serve_args(&rx, 3, 8);
        bad[4] = (rx.addr() as u32 as i32).wrapping_add(MAX_REQUEST_BUFFER as i32 - 4);
        assert_eq!(on_request(&state, &rx, bad), internal);
        let mut bad = serve_args(&rx, 3, 8);
        bad[0] = 99;
        assert_eq!(on_request(&state, &rx, bad), internal);
        rx.borrow_mut().expect("free")[0] = 0x80;
        assert_eq!(on_request(&state, &rx, serve_args(&rx, 3, 8)), internal);
    }

    #[test]
    fn request_parts_bounds_every_region() {
        let a = |r: [i32; 6]| request_parts(1000, 100, r);
        assert!(a([0, 1016, 4, 1000, 1020, 80]).is_some());
        assert_eq!(a([0, 1016, 4, 1000, 1020, 81]), None);
        assert_eq!(a([0, 999, 4, 1000, 1020, 8]), None);
        assert_eq!(a([0, 1016, -1, 1000, 1020, 8]), None);
        assert_eq!(a([8, 1016, 4, 1000, 1020, 8]), None);
        assert_eq!(a([0, 1016, 4, 1090, 1020, 8]), None);
    }

    #[test]
    fn alloc_follows_the_buffer_size() {
        let rx: RxBuffer = GuestCell::new([0; MAX_MESSAGE]);
        assert_eq!(alloc(&rx, MAX_MESSAGE as i32), addr_i32(rx.addr()));
        assert_eq!(alloc(&rx, MAX_MESSAGE as i32 + 1), 0);
        let big: ServeBuffer = GuestCell::new([0; MAX_REQUEST_BUFFER]);
        assert_eq!(
            serve_alloc(&big, MAX_REQUEST_BUFFER as i32),
            addr_i32(big.addr())
        );
        assert_eq!(serve_alloc(&big, MAX_REQUEST_BUFFER as i32 + 1), 0);
        assert_eq!(serve_alloc(&big, -1), 0);
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
