//! An in-memory [`Datagram`] and [`Clock`] for host tests of the shells:
//! one [`FakeNet`] holds the inbox, what was sent, and virtual time.
//!
//! * [`FakeUdp::recv_from`] is pending while the inbox is empty (it
//!   registers no waker: drive it raced against a clock, or poll it by
//!   hand).
//! * [`FakeClock::sleep_until_ms`] jumps the virtual time to its deadline
//!   and is ready at once, so a wait on the clock never blocks a test.
//! * [`FakeNet::fail_sends`] makes every following `send_to` fail with
//!   [`FakeErr`].

extern crate std;

use core::cell::RefCell;
use core::net::SocketAddr;
use core::task::Poll;
use std::collections::VecDeque;
use std::rc::Rc;
use std::vec::Vec;

use super::Datagram;
use crate::clock::Clock;

/// The fake socket's error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FakeErr;

impl core::fmt::Display for FakeErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("fake socket error")
    }
}

impl core::error::Error for FakeErr {}

/// A datagram the fake socket sent: bytes, destination, virtual time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Sent {
    pub bytes: Vec<u8>,
    pub to: SocketAddr,
    pub at_ms: u64,
}

#[derive(Debug, Default)]
struct State {
    inbox: VecDeque<(Vec<u8>, SocketAddr)>,
    sent: Vec<Sent>,
    now_ms: u64,
    fail_sends: bool,
}

/// The shared state behind a [`FakeUdp`] and a [`FakeClock`].
#[derive(Debug, Clone, Default)]
pub(crate) struct FakeNet(Rc<RefCell<State>>);

impl FakeNet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn udp(&self) -> FakeUdp {
        FakeUdp(self.clone())
    }

    pub fn clock(&self) -> FakeClock {
        FakeClock(self.clone())
    }

    /// Queues a datagram for the socket to receive.
    pub fn deliver(&self, bytes: &[u8], from: SocketAddr) {
        self.0.borrow_mut().inbox.push_back((bytes.into(), from));
    }

    /// Datagrams still queued.
    pub fn queued(&self) -> usize {
        self.0.borrow().inbox.len()
    }

    /// Takes what was sent so far.
    pub fn take_sent(&self) -> Vec<Sent> {
        core::mem::take(&mut self.0.borrow_mut().sent)
    }

    pub fn now_ms(&self) -> u64 {
        self.0.borrow().now_ms
    }

    pub fn advance_to(&self, ms: u64) {
        let mut s = self.0.borrow_mut();
        s.now_ms = s.now_ms.max(ms);
    }

    pub fn fail_sends(&self, fail: bool) {
        self.0.borrow_mut().fail_sends = fail;
    }

    fn pop(&self, buf: &mut [u8]) -> Option<(usize, SocketAddr)> {
        let (bytes, from) = self.0.borrow_mut().inbox.pop_front()?;
        let n = bytes.len().min(buf.len());
        buf[..n].copy_from_slice(&bytes[..n]);
        Some((n, from))
    }
}

/// The fake socket.
#[derive(Debug)]
pub(crate) struct FakeUdp(FakeNet);

impl Datagram for FakeUdp {
    type Error = FakeErr;

    async fn send_to(&mut self, buf: &[u8], to: SocketAddr) -> Result<(), FakeErr> {
        let mut s = (self.0).0.borrow_mut();
        if s.fail_sends {
            return Err(FakeErr);
        }
        let at_ms = s.now_ms;
        s.sent.push(Sent {
            bytes: buf.into(),
            to,
            at_ms,
        });
        Ok(())
    }

    async fn recv_from(&mut self, buf: &mut [u8]) -> Result<(usize, SocketAddr), FakeErr> {
        core::future::poll_fn(|_| match self.0.pop(buf) {
            Some(r) => Poll::Ready(Ok(r)),
            None => Poll::Pending,
        })
        .await
    }

    fn try_recv_from(&mut self, buf: &mut [u8]) -> Result<Option<(usize, SocketAddr)>, FakeErr> {
        Ok(self.0.pop(buf))
    }
}

/// The fake clock: virtual milliseconds.
#[derive(Debug)]
pub(crate) struct FakeClock(FakeNet);

impl Clock for FakeClock {
    fn now_ms(&mut self) -> u64 {
        self.0.now_ms()
    }

    async fn sleep_until_ms(&mut self, deadline_ms: u64) {
        self.0.advance_to(deadline_ms);
    }
}

/// Polls `f` once with a no-op waker.
pub(crate) fn poll_once<F: Future>(f: F) -> Poll<F::Output> {
    let mut f = core::pin::pin!(f);
    f.as_mut().poll(&mut core::task::Context::from_waker(
        core::task::Waker::noop(),
    ))
}

/// Runs `f` to completion on the fake seam, where nothing ever waits for
/// a wakeup: the clock is ready at once, so `f` is ready on its first
/// poll unless it waits for a datagram that is not queued, which panics
/// instead of hanging the test.
#[track_caller]
pub(crate) fn run<F: Future>(f: F) -> F::Output {
    match poll_once(f) {
        Poll::Ready(v) => v,
        Poll::Pending => panic!("stuck: waiting for a datagram that is not queued"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr() -> SocketAddr {
        "192.168.1.9:5353".parse().unwrap()
    }

    #[test]
    fn a_datagram_longer_than_the_buffer_is_truncated() {
        let net = FakeNet::new();
        let mut udp = net.udp();
        net.deliver(&[1, 2, 3, 4, 5], addr());
        let mut buf = [0u8; 3];
        let got = futures::executor::block_on(udp.recv_from(&mut buf)).unwrap();
        assert_eq!(got, (3, addr()));
        assert_eq!(buf, [1, 2, 3]);
        net.deliver(&[6, 7, 8, 9], addr());
        assert_eq!(udp.try_recv_from(&mut buf).unwrap(), Some((3, addr())));
        assert_eq!(buf, [6, 7, 8]);
    }

    #[test]
    fn try_recv_is_none_when_nothing_is_queued() {
        let net = FakeNet::new();
        let mut buf = [0u8; 8];
        assert_eq!(net.udp().try_recv_from(&mut buf).unwrap(), None);
    }

    #[test]
    fn a_dropped_recv_loses_no_datagram() {
        let net = FakeNet::new();
        let mut udp = net.udp();
        let mut buf = [0u8; 8];
        assert!(poll_once(udp.recv_from(&mut buf)).is_pending());
        net.deliver(&[9], addr());
        assert_eq!(net.queued(), 1);
        assert_eq!(udp.try_recv_from(&mut buf).unwrap(), Some((1, addr())));
    }

    #[test]
    fn sends_record_time_and_can_fail() {
        let net = FakeNet::new();
        let mut udp = net.udp();
        let mut clock = net.clock();
        futures::executor::block_on(clock.sleep_until_ms(250));
        futures::executor::block_on(udp.send_to(&[1], addr())).unwrap();
        net.fail_sends(true);
        assert_eq!(
            futures::executor::block_on(udp.send_to(&[2], addr())),
            Err(FakeErr)
        );
        assert_eq!(
            net.take_sent(),
            [Sent {
                bytes: [1].into(),
                to: addr(),
                at_ms: 250
            }]
        );
        assert_eq!(clock.now_ms(), 250);
    }
}
