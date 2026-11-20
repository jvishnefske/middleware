//! Waiting on the first of two futures, polled in order (no executor, no
//! allocation): the announcer's "next datagram or next timer".

use core::pin::pin;
use core::task::Poll;

/// Which future finished first.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Raced<A, B> {
    First(A),
    Second(B),
}

/// Polls `a` then `b` until one is ready; on a poll where both would be,
/// `a` wins. The other future is dropped, so both must be cancel safe.
pub(crate) async fn race<A: Future, B: Future>(a: A, b: B) -> Raced<A::Output, B::Output> {
    let mut a = pin!(a);
    let mut b = pin!(b);
    core::future::poll_fn(|cx| {
        if let Poll::Ready(r) = a.as_mut().poll(cx) {
            return Poll::Ready(Raced::First(r));
        }
        if let Poll::Ready(r) = b.as_mut().poll(cx) {
            return Poll::Ready(Raced::Second(r));
        }
        Poll::Pending
    })
    .await
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use core::cell::Cell;
    use core::pin::Pin;
    use core::task::{Context, Waker};
    use std::rc::Rc;

    /// Pending for its first `pending_polls` polls, then ready; counts its
    /// polls and its completions, so a test sees whether the race
    /// consumed it, and how often.
    struct Counted {
        pending_polls: u32,
        polls: Rc<Cell<u32>>,
        consumed: Rc<Cell<u32>>,
    }

    impl Counted {
        fn new(pending_polls: u32) -> (Self, Rc<Cell<u32>>, Rc<Cell<u32>>) {
            let polls = Rc::new(Cell::new(0));
            let consumed = Rc::new(Cell::new(0));
            let f = Counted {
                pending_polls,
                polls: polls.clone(),
                consumed: consumed.clone(),
            };
            (f, polls, consumed)
        }
    }

    impl Future for Counted {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
            self.polls.set(self.polls.get() + 1);
            if self.pending_polls > 0 {
                self.pending_polls -= 1;
                return Poll::Pending;
            }
            self.consumed.set(self.consumed.get() + 1);
            Poll::Ready(())
        }
    }

    fn poll<F: Future>(f: Pin<&mut F>) -> Poll<F::Output> {
        f.poll(&mut Context::from_waker(Waker::noop()))
    }

    #[test]
    fn a_tie_consumes_the_first_only_and_never_polls_the_second() {
        let (a, _, a_done) = Counted::new(0);
        let (b, b_polls, b_done) = Counted::new(0);
        let r = futures::executor::block_on(race(a, b));
        assert_eq!(r, Raced::First(()));
        assert_eq!((a_done.get(), b_done.get(), b_polls.get()), (1, 0, 0));
    }

    #[test]
    fn a_first_future_ready_on_its_second_poll_is_consumed_once() {
        let (a, a_polls, a_done) = Counted::new(1);
        let (b, _, b_done) = Counted::new(u32::MAX);
        let mut r = pin!(race(a, b));
        assert!(poll(r.as_mut()).is_pending());
        assert_eq!(a_done.get(), 0);
        assert_eq!(poll(r.as_mut()), Poll::Ready(Raced::First(())));
        assert_eq!((a_polls.get(), a_done.get(), b_done.get()), (2, 1, 0));
    }

    #[test]
    fn a_race_dropped_while_pending_consumes_nothing() {
        let (a, a_polls, a_done) = Counted::new(u32::MAX);
        let (b, b_polls, b_done) = Counted::new(u32::MAX);
        {
            let mut r = pin!(race(a, b));
            assert!(poll(r.as_mut()).is_pending());
        }
        assert_eq!((a_polls.get(), b_polls.get()), (1, 1));
        assert_eq!((a_done.get(), b_done.get()), (0, 0));
    }

    #[test]
    fn the_first_future_wins_a_tie() {
        let r = futures::executor::block_on(race(async { 1 }, async { 2 }));
        assert_eq!(r, Raced::First(1));
    }

    #[test]
    fn the_second_wins_when_the_first_is_pending() {
        let r = futures::executor::block_on(race(core::future::pending::<u8>(), async { 2 }));
        assert_eq!(r, Raced::Second(2));
    }
}
