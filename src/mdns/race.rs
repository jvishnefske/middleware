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
    use super::*;

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
