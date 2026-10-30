//! Request/response over ESP-NOW unicast: [`RpcRequester`] implements
//! [`Requester`] and [`RpcResponder`] implements [`Responder`], with the
//! frame format and the retry and duplicate rules of [`crate::rpc`].
//!
//! Requests and responses travel as `MR` frames ([`crate::rpc`]) sent to
//! one peer ([`EspNowUnicast::send_to`]); the publication transport
//! ([`super::EspNowTransport`]) skips them as foreign, so both can share a
//! station (the ESP base firmware taps `MR` frames off its radio before the
//! transport sees them).
//!
//! # Requester
//!
//! [`RpcRequester`] talks to one peer. Each [`Requester::request`] takes a
//! fresh correlation id (advanced before the first await, so a dropped call
//! never makes the next one reuse its id), sends the request and waits up
//! to its timeout ([`Delay`]) for the matching response
//! ([`Call::answers`]); on a timeout or a failed send it resends with the
//! **same** id, up to its attempt limit, then fails with
//! [`RpcClientError::Timeout`]. Frames that are not the answer (stale
//! responses, publications, other stations) are skipped, at most
//! [`super::MAX_IGNORED_FRAMES`] per attempt. A response with an error
//! status is [`RpcClientError::Remote`].
//!
//! # Responder
//!
//! [`RpcResponder`] answers requests addressed to its own MAC from any
//! peer. [`Responder::serve_one`] waits for the next request (skipping
//! other frames, at most [`super::MAX_IGNORED_FRAMES`]), and decides with
//! [`Replies`]: a new request runs the handler and its response is cached;
//! a repeat (the requester did not hear the response) is answered from the
//! cache without running the handler again. The handler runs synchronously
//! between two awaits, so nothing is ever in flight across a cancellation.
//!
//! Tested on the host over an in-memory air that loses, duplicates and
//! reorders frames.

use super::{EspNowRadio, MAX_IGNORED_FRAMES, Mac};
use crate::key::Key;
use crate::rpc::{Admit, Call, MAX_FRAME, MAX_RESPONSE_BODY, Message, Replies, RpcError, next_id};
use crate::transport::{Handler, HandlerError, Requester, Responder};
use core::future::{Future, poll_fn};
use core::pin::pin;
use core::task::Poll;

/// A radio that can also send to one peer (ESP-NOW unicast, acknowledged
/// at the MAC layer).
pub trait EspNowUnicast: EspNowRadio {
    /// Sends one frame to `peer` (registering it with the driver first if
    /// the binding needs that).
    async fn send_to(&mut self, peer: Mac, frame: &[u8]) -> Result<(), Self::Error>;
}

/// Waits a number of milliseconds (the requester's timeout).
pub trait Delay {
    /// Completes after `ms` milliseconds. Cancel-safe.
    async fn delay_ms(&mut self, ms: u32);
}

/// Which of two futures finished first.
enum First<A, B> {
    Left(A),
    Right(B),
}

/// Runs `a` and `b` together and returns the first to finish, dropping the
/// other; `a` wins a tie.
async fn first<A: Future, B: Future>(a: A, b: B) -> First<A::Output, B::Output> {
    let mut a = pin!(a);
    let mut b = pin!(b);
    poll_fn(|cx| {
        if let Poll::Ready(x) = a.as_mut().poll(cx) {
            return Poll::Ready(First::Left(x));
        }
        if let Poll::Ready(y) = b.as_mut().poll(cx) {
            return Poll::Ready(First::Right(y));
        }
        Poll::Pending
    })
    .await
}

/// Failure of an [`RpcRequester`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcClientError<E> {
    /// Receiving failed.
    Radio(E),
    /// The request does not fit one frame.
    Frame(RpcError),
    /// The responder answered with this status.
    Remote(HandlerError),
    /// No answer after every attempt.
    Timeout,
    /// The response body does not fit the caller's buffer.
    BufferTooSmall,
}

/// Retry settings of an [`RpcRequester`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retry {
    /// How long one attempt waits for its response.
    pub timeout_ms: u32,
    /// Sends in all, including the first (at least one).
    pub attempts: u8,
}

/// Counters of an [`RpcRequester`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClientStats {
    /// Requests started.
    pub calls: u32,
    /// Resends after a timeout or failed send.
    pub retransmissions: u32,
    /// Frames skipped while waiting (stale responses, other traffic).
    pub skipped: u32,
}

/// Client side: sends requests to one peer and waits for the answers.
pub struct RpcRequester<R, D> {
    radio: R,
    delay: D,
    own: Option<Mac>,
    peer: Mac,
    next: u16,
    retry: Retry,
    stats: ClientStats,
}

impl<R: EspNowUnicast, D: Delay> RpcRequester<R, D> {
    /// A requester to `peer` over `radio`, first id `first_id`.
    pub const fn new(radio: R, delay: D, peer: Mac, retry: Retry, first_id: u16) -> Self {
        RpcRequester {
            radio,
            delay,
            own: None,
            peer,
            next: first_id,
            retry,
            stats: ClientStats {
                calls: 0,
                retransmissions: 0,
                skipped: 0,
            },
        }
    }

    /// The same requester also checking that responses are addressed to
    /// `mac` (our own station address).
    pub fn with_own_address(self, mac: Mac) -> Self {
        RpcRequester {
            own: Some(mac),
            ..self
        }
    }

    /// Counters so far.
    pub const fn stats(&self) -> ClientStats {
        self.stats
    }

    /// Borrows the radio.
    pub fn radio_mut(&mut self) -> &mut R {
        &mut self.radio
    }

    /// Waits for the frame answering `call`; copies its body into `out`.
    async fn answer(
        radio: &mut R,
        stats: &mut ClientStats,
        own: Option<Mac>,
        call: Call,
        out: &mut [u8],
    ) -> Result<Result<usize, HandlerError>, RpcClientError<R::Error>> {
        let mut raw = [0u8; MAX_FRAME];
        for _ in 0..MAX_IGNORED_FRAMES {
            let (bytes, src) = radio
                .receive(&mut raw)
                .await
                .map_err(RpcClientError::Radio)?;
            let Ok(msg) = Message::decode(bytes) else {
                stats.skipped = stats.skipped.saturating_add(1);
                continue;
            };
            match call.answers(own, src, &msg) {
                Some(Ok(body)) => {
                    let slot = out
                        .get_mut(..body.len())
                        .ok_or(RpcClientError::BufferTooSmall)?;
                    slot.copy_from_slice(body);
                    return Ok(Ok(body.len()));
                }
                Some(Err(e)) => return Ok(Err(e)),
                None => stats.skipped = stats.skipped.saturating_add(1),
            }
        }
        // Flooded with other traffic: wait out the attempt instead.
        poll_fn(|_| Poll::<()>::Pending).await;
        Err(RpcClientError::Timeout)
    }
}

impl<R: EspNowUnicast, D: Delay> Requester for RpcRequester<R, D> {
    type Error = RpcClientError<R::Error>;

    /// Sends `request` to the peer and waits for the response, resending
    /// with the same id on timeout (see the module docs).
    async fn request<'b>(
        &mut self,
        key: Key<'_>,
        request: &[u8],
        response: &'b mut [u8],
    ) -> Result<&'b [u8], Self::Error> {
        let id = self.next;
        self.next = next_id(id);
        self.stats.calls = self.stats.calls.saturating_add(1);
        let mut frame = [0u8; MAX_FRAME];
        let n = Message::Request {
            id,
            to: self.peer,
            key,
            body: request,
        }
        .encode(&mut frame)
        .map_err(RpcClientError::Frame)?;
        let frame = frame
            .get(..n)
            .ok_or(RpcClientError::Frame(RpcError::TooLong))?;
        let mut call = Call::first(id, self.peer, self.retry.attempts);
        loop {
            let sent = self.radio.send_to(self.peer, frame).await.is_ok();
            let outcome = if sent {
                let wait = Self::answer(&mut self.radio, &mut self.stats, self.own, call, response);
                match first(wait, self.delay.delay_ms(self.retry.timeout_ms)).await {
                    First::Left(r) => Some(r?),
                    First::Right(()) => None,
                }
            } else {
                None
            };
            match outcome {
                Some(Ok(len)) => return response.get(..len).ok_or(RpcClientError::BufferTooSmall),
                Some(Err(e)) => return Err(RpcClientError::Remote(e)),
                None => {
                    call = call.retry().ok_or(RpcClientError::Timeout)?;
                    self.stats.retransmissions = self.stats.retransmissions.saturating_add(1);
                }
            }
        }
    }
}

/// Failure of an [`RpcResponder`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcServeError<E> {
    /// The radio failed to receive or send.
    Radio(E),
    /// A received `MR` frame was malformed.
    Frame(RpcError),
    /// [`super::MAX_IGNORED_FRAMES`] frames passed without a request for us.
    IgnoredLimit,
}

/// Counters of an [`RpcResponder`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ServeStats {
    /// Requests that ran the handler.
    pub executed: u32,
    /// Repeats answered from the cache.
    pub replayed: u32,
}

/// Server side: answers requests addressed to `own`, remembering the last
/// `N` responses (bodies up to `B` bytes) to answer repeats.
pub struct RpcResponder<R, const N: usize = 8, const B: usize = 128> {
    radio: R,
    own: Mac,
    replies: Replies<N, B>,
    stats: ServeStats,
}

impl<R: EspNowUnicast, const N: usize, const B: usize> RpcResponder<R, N, B> {
    /// A responder for station `own` over `radio`.
    pub const fn new(radio: R, own: Mac) -> Self {
        RpcResponder {
            radio,
            own,
            replies: Replies::new(),
            stats: ServeStats {
                executed: 0,
                replayed: 0,
            },
        }
    }

    /// Counters so far.
    pub const fn stats(&self) -> ServeStats {
        self.stats
    }

    /// Borrows the radio.
    pub fn radio_mut(&mut self) -> &mut R {
        &mut self.radio
    }

    async fn reply(
        &mut self,
        peer: Mac,
        id: u16,
        result: Result<&[u8], HandlerError>,
    ) -> Result<(), RpcServeError<R::Error>> {
        let mut frame = [0u8; MAX_FRAME];
        let msg = Message::Response {
            id,
            to: peer,
            result,
        };
        let n = match msg.encode(&mut frame) {
            Ok(n) => n,
            // Cannot happen for a body within MAX_RESPONSE_BODY; answer the
            // status instead of nothing.
            Err(_) => Message::Response {
                id,
                to: peer,
                result: Err(HandlerError::BufferTooSmall),
            }
            .encode(&mut frame)
            .map_err(RpcServeError::Frame)?,
        };
        let bytes = frame
            .get(..n)
            .ok_or(RpcServeError::Frame(RpcError::TooLong))?;
        self.radio
            .send_to(peer, bytes)
            .await
            .map_err(RpcServeError::Radio)
    }
}

impl<R: EspNowUnicast, const N: usize, const B: usize> Responder for RpcResponder<R, N, B> {
    type Error = RpcServeError<R::Error>;

    /// Waits for one request addressed to us, answers it (from the cache for
    /// a repeat) and returns. `scratch` holds the handler's response.
    async fn serve_one<H: Handler>(
        &mut self,
        handler: &mut H,
        scratch: &mut [u8],
    ) -> Result<(), Self::Error> {
        let mut raw = [0u8; MAX_FRAME];
        for _ in 0..MAX_IGNORED_FRAMES {
            let (bytes, src) = self
                .radio
                .receive(&mut raw)
                .await
                .map_err(RpcServeError::Radio)?;
            if !crate::rpc::is_rpc(bytes) {
                continue;
            }
            let Message::Request { id, to, key, body } =
                Message::decode(bytes).map_err(RpcServeError::Frame)?
            else {
                continue;
            };
            if to != self.own || src == self.own {
                continue;
            }
            let (replies, admit) = self.replies.admit(src, id, 0);
            self.replies = replies;
            match admit {
                Admit::Busy => continue,
                Admit::Replay => {
                    self.stats.replayed = self.stats.replayed.saturating_add(1);
                    let mut copy = [0u8; MAX_RESPONSE_BODY];
                    let result = match self.replies.cached(src, id) {
                        Some(Ok(b)) => copy
                            .get_mut(..b.len())
                            .map(|s| {
                                s.copy_from_slice(b);
                                &*s
                            })
                            .ok_or(HandlerError::Internal),
                        Some(Err(e)) => Err(e),
                        None => Err(HandlerError::Internal),
                    };
                    return self.reply(src, id, result).await;
                }
                Admit::Execute => {
                    self.stats.executed = self.stats.executed.saturating_add(1);
                    let room = scratch.len().min(MAX_RESPONSE_BODY);
                    let out = scratch.get_mut(..room).unwrap_or_default();
                    let result = handler
                        .handle(key, body, out)
                        .and_then(|n| out.get(..n).ok_or(HandlerError::BufferTooSmall));
                    self.replies = self.replies.complete(src, id, result);
                    return self.reply(src, id, result).await;
                }
            }
        }
        Err(RpcServeError::IgnoredLimit)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::loader::keys;
    use core::cell::RefCell;
    use core::task::{Context, Waker};
    use std::collections::VecDeque;
    use std::rc::Rc;
    use std::vec::Vec;

    const DEV: Mac = [0xe4, 0xb0, 0x63, 0x81, 0xd3, 0x24];
    const HOST: Mac = [0x5c, 0xcf, 0x7f, 0x72, 0xf6, 0x54];
    const OTHER: Mac = [2, 0, 0, 0, 0, 9];

    /// What the air does to the next frame sent.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Fate {
        Deliver,
        Lose,
        Duplicate,
        /// Held back and delivered after the next frame to that station.
        Delay,
    }

    /// The shared medium: one inbox per station, and a script of fates
    /// applied to frames in send order (then `Deliver`).
    /// Frames waiting at one station, with their sender.
    type Inbox = VecDeque<(Vec<u8>, Mac)>;

    #[derive(Default)]
    struct Air {
        inboxes: Vec<(Mac, Inbox)>,
        held: Vec<(Mac, Vec<u8>, Mac)>,
        script: VecDeque<Fate>,
        sent: usize,
    }

    impl Air {
        fn inbox(&mut self, mac: Mac) -> &mut Inbox {
            if !self.inboxes.iter().any(|(m, _)| *m == mac) {
                self.inboxes.push((mac, VecDeque::new()));
            }
            let i = self.inboxes.iter().position(|(m, _)| *m == mac).unwrap();
            &mut self.inboxes[i].1
        }

        fn send(&mut self, from: Mac, to: Mac, frame: &[u8]) {
            self.sent += 1;
            let fate = self.script.pop_front().unwrap_or(Fate::Deliver);
            let held: Vec<_> = self.held.iter().filter(|h| h.0 == to).cloned().collect();
            self.held.retain(|h| h.0 != to);
            match fate {
                Fate::Lose => {}
                Fate::Deliver => self.inbox(to).push_back((frame.to_vec(), from)),
                Fate::Duplicate => {
                    self.inbox(to).push_back((frame.to_vec(), from));
                    self.inbox(to).push_back((frame.to_vec(), from));
                }
                Fate::Delay => self.held.push((to, frame.to_vec(), from)),
            }
            for (to, f, from) in held {
                self.inbox(to).push_back((f, from));
            }
        }
    }

    /// One station on the shared air.
    struct Station {
        mac: Mac,
        air: Rc<RefCell<Air>>,
    }

    impl EspNowRadio for Station {
        type Error = ();

        async fn broadcast(&mut self, _frame: &[u8]) -> Result<(), ()> {
            Ok(())
        }

        async fn receive<'b>(&mut self, buf: &'b mut [u8]) -> Result<(&'b [u8], Mac), ()> {
            let air = self.air.clone();
            let mac = self.mac;
            let (frame, src) = poll_fn(|_| match air.borrow_mut().inbox(mac).pop_front() {
                Some(f) => Poll::Ready(f),
                None => Poll::Pending,
            })
            .await;
            let slot = buf.get_mut(..frame.len()).ok_or(())?;
            slot.copy_from_slice(&frame);
            Ok((slot, src))
        }

        fn try_receive<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<(&'b [u8], Mac)>, ()> {
            let Some((frame, src)) = self.air.borrow_mut().inbox(self.mac).pop_front() else {
                return Ok(None);
            };
            let slot = buf.get_mut(..frame.len()).ok_or(())?;
            slot.copy_from_slice(&frame);
            Ok(Some((slot, src)))
        }
    }

    impl EspNowUnicast for Station {
        async fn send_to(&mut self, peer: Mac, frame: &[u8]) -> Result<(), ()> {
            self.air.borrow_mut().send(self.mac, peer, frame);
            Ok(())
        }
    }

    /// A timeout that fires after `polls` polls (polls stand in for time).
    struct Polls(u32);

    impl Delay for Polls {
        async fn delay_ms(&mut self, _ms: u32) {
            let mut left = self.0;
            poll_fn(|_| {
                if left == 0 {
                    Poll::Ready(())
                } else {
                    left -= 1;
                    Poll::Pending
                }
            })
            .await;
        }
    }

    /// The device's handler: counts executions per key, answers `sys/info`
    /// with "esp32s3" and everything else with an empty body.
    #[derive(Default)]
    struct Device {
        runs: Vec<std::string::String>,
    }

    impl Handler for Device {
        fn handle(
            &mut self,
            key: Key<'_>,
            request: &[u8],
            response: &mut [u8],
        ) -> Result<usize, HandlerError> {
            self.runs.push(key.as_str().into());
            match key.as_str() {
                "sys/info" => {
                    response
                        .get_mut(..7)
                        .ok_or(HandlerError::BufferTooSmall)?
                        .copy_from_slice(b"esp32s3");
                    Ok(7)
                }
                "sys/trust/claim" => Err(HandlerError::Denied),
                "echo" => {
                    response
                        .get_mut(..request.len())
                        .ok_or(HandlerError::BufferTooSmall)?
                        .copy_from_slice(request);
                    Ok(request.len())
                }
                _ => Ok(0),
            }
        }
    }

    const RETRY: Retry = Retry {
        timeout_ms: 100,
        attempts: 4,
    };

    fn net(
        script: &[Fate],
    ) -> (
        RpcRequester<Station, Polls>,
        RpcResponder<Station, 4, 64>,
        Rc<RefCell<Air>>,
    ) {
        let air = Rc::new(RefCell::new(Air {
            script: script.iter().copied().collect(),
            ..Air::default()
        }));
        let host = Station {
            mac: HOST,
            air: air.clone(),
        };
        let dev = Station {
            mac: DEV,
            air: air.clone(),
        };
        (
            RpcRequester::new(host, Polls(3), DEV, RETRY, 100).with_own_address(HOST),
            RpcResponder::new(dev, DEV),
            air,
        )
    }

    /// Runs one request against a responder that keeps serving, polling
    /// both with a no-op waker (bounded).
    fn call(
        req: &mut RpcRequester<Station, Polls>,
        resp: &mut RpcResponder<Station, 4, 64>,
        dev: &mut Device,
        key: &str,
        body: &[u8],
        out: &mut [u8],
    ) -> Result<std::vec::Vec<u8>, RpcClientError<()>> {
        let key = Key::new(key).unwrap();
        let mut cx = Context::from_waker(Waker::noop());
        let mut scratch = [0u8; 64];
        let mut client = pin!(req.request(key, body, out));
        let mut server = pin!(async {
            loop {
                let _ = resp.serve_one(dev, &mut scratch).await;
            }
        });
        for _ in 0..1000 {
            if let Poll::Ready(r) = client.as_mut().poll(&mut cx) {
                return r.map(<[u8]>::to_vec);
            }
            let _ = server.as_mut().poll(&mut cx);
        }
        panic!("request did not finish");
    }

    #[test]
    fn o4_request_and_response_end_to_end() {
        let (mut req, mut resp, air) = net(&[]);
        let mut dev = Device::default();
        let mut out = [0u8; 64];
        assert_eq!(
            call(&mut req, &mut resp, &mut dev, "sys/info", &[], &mut out),
            Ok(b"esp32s3".to_vec())
        );
        assert_eq!(
            call(&mut req, &mut resp, &mut dev, "echo", b"abc", &mut out),
            Ok(b"abc".to_vec())
        );
        assert_eq!(
            call(
                &mut req,
                &mut resp,
                &mut dev,
                "sys/trust/claim",
                &[0; 32],
                &mut out
            ),
            Err(RpcClientError::Remote(HandlerError::Denied))
        );
        assert_eq!(dev.runs, ["sys/info", "echo", "sys/trust/claim"]);
        assert_eq!(air.borrow().sent, 6);
        assert_eq!(
            req.stats(),
            ClientStats {
                calls: 3,
                retransmissions: 0,
                skipped: 0
            }
        );
        assert_eq!(
            resp.stats(),
            ServeStats {
                executed: 3,
                replayed: 0
            }
        );
    }

    #[test]
    fn o4_a_lost_request_is_resent_with_the_same_id() {
        let (mut req, mut resp, air) = net(&[Fate::Lose]);
        let mut dev = Device::default();
        let mut out = [0u8; 64];
        assert_eq!(
            call(&mut req, &mut resp, &mut dev, "sys/info", &[], &mut out),
            Ok(b"esp32s3".to_vec())
        );
        assert_eq!(dev.runs.len(), 1);
        assert_eq!(req.stats().retransmissions, 1);
        assert_eq!(air.borrow().sent, 3);
    }

    #[test]
    fn o4_a_lost_response_is_replayed_without_running_the_handler_again() {
        // The request arrives, its response is lost; the resend is answered
        // from the cache: commit and start must not run twice.
        let (mut req, mut resp, _) = net(&[Fate::Deliver, Fate::Lose, Fate::Deliver]);
        let mut dev = Device::default();
        let mut out = [0u8; 64];
        assert_eq!(
            call(
                &mut req,
                &mut resp,
                &mut dev,
                keys::COMMIT.as_str(),
                &[],
                &mut out
            ),
            Ok(Vec::new())
        );
        assert_eq!(dev.runs, ["sys/loader/commit"]);
        assert_eq!(
            resp.stats(),
            ServeStats {
                executed: 1,
                replayed: 1
            }
        );
        assert_eq!(req.stats().retransmissions, 1);
    }

    #[test]
    fn o4_a_duplicated_request_runs_once() {
        let (mut req, mut resp, _) = net(&[Fate::Duplicate]);
        let mut dev = Device::default();
        let mut out = [0u8; 64];
        assert_eq!(
            call(
                &mut req,
                &mut resp,
                &mut dev,
                keys::START.as_str(),
                &[],
                &mut out
            ),
            Ok(Vec::new())
        );
        // The duplicate is replayed; its second response is stale for the
        // next call, which still gets its own answer.
        assert_eq!(
            call(&mut req, &mut resp, &mut dev, "echo", b"2", &mut out),
            Ok(b"2".to_vec())
        );
        assert_eq!(dev.runs, ["sys/payload/start", "echo"]);
        assert_eq!(resp.stats().replayed, 1);
        assert!(
            req.stats().skipped >= 1,
            "the stale duplicate response was skipped"
        );
    }

    #[test]
    fn o4_reordered_frames_never_mix_answers() {
        // The first response is delayed past the requester's timeout, so
        // the request is resent and answered again; the delayed response
        // arrives later and is skipped as stale by the next call.
        let (mut req, mut resp, _) =
            net(&[Fate::Deliver, Fate::Delay, Fate::Deliver, Fate::Deliver]);
        let mut dev = Device::default();
        let mut out = [0u8; 64];
        assert_eq!(
            call(&mut req, &mut resp, &mut dev, "echo", b"one", &mut out),
            Ok(b"one".to_vec())
        );
        assert_eq!(
            call(&mut req, &mut resp, &mut dev, "echo", b"two", &mut out),
            Ok(b"two".to_vec())
        );
        assert_eq!(dev.runs, ["echo", "echo"]);
        // A delayed *request* behind a newer one: replayed, not re-run.
        let (mut req, mut resp, _) = net(&[Fate::Delay, Fate::Deliver, Fate::Deliver]);
        let mut dev = Device::default();
        assert_eq!(
            call(&mut req, &mut resp, &mut dev, "echo", b"a", &mut out),
            Ok(b"a".to_vec())
        );
        assert_eq!(dev.runs, ["echo"]);
    }

    #[test]
    fn o4_attempts_are_bounded_and_other_stations_are_ignored() {
        let (mut req, mut resp, air) = net(&[Fate::Lose; 4]);
        let mut dev = Device::default();
        let mut out = [0u8; 64];
        assert_eq!(
            call(&mut req, &mut resp, &mut dev, "sys/info", &[], &mut out),
            Err(RpcClientError::Timeout)
        );
        assert_eq!(air.borrow().sent, 4);
        assert!(dev.runs.is_empty());
        // A request addressed to another station is not answered, and a
        // response from a station we did not ask is not accepted.
        let mut frame = [0u8; MAX_FRAME];
        let n = Message::Request {
            id: 1,
            to: OTHER,
            key: keys::INFO,
            body: &[],
        }
        .encode(&mut frame)
        .unwrap();
        air.borrow_mut().send(HOST, DEV, &frame[..n]);
        let n = Message::Response {
            id: 101,
            to: HOST,
            result: Ok(b"forged"),
        }
        .encode(&mut frame)
        .unwrap();
        air.borrow_mut().send(OTHER, HOST, &frame[..n]);
        assert_eq!(
            call(&mut req, &mut resp, &mut dev, "sys/info", &[], &mut out),
            Ok(b"esp32s3".to_vec())
        );
        assert_eq!(dev.runs, ["sys/info"]);
    }

    #[test]
    fn o4_oversized_requests_and_small_buffers_are_errors() {
        let (mut req, mut resp, _) = net(&[]);
        let mut dev = Device::default();
        let mut out = [0u8; 64];
        let big = [0u8; crate::rpc::ROOM];
        assert_eq!(
            call(&mut req, &mut resp, &mut dev, "echo", &big, &mut out),
            Err(RpcClientError::Frame(RpcError::BodyTooLarge))
        );
        let mut small = [0u8; 3];
        assert_eq!(
            call(&mut req, &mut resp, &mut dev, "sys/info", &[], &mut small),
            Err(RpcClientError::BufferTooSmall)
        );
    }
}
