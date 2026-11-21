//! The browser's Zenoh session, driven by hand from JS: the middleware's
//! [`ZenohLite`] over a WebSocket the page owns.
//!
//! There is no executor in the page and none in this crate. A [`Pump`]
//! holds one long-lived actor future that owns the client; JS hands it each
//! WebSocket message ([`Pump::feed`]) and each command ([`Pump::submit`]),
//! and calls [`Pump::poll`] after either and whenever [`Pump::next_wake_ms`]
//! comes due, with the current time. A poll runs the actor as far as it
//! can with a no-op waker and returns the commands that finished; the
//! messages it wants sent are in [`Pump::take_output`]. Everything is
//! synchronous, so a call can never re-enter the pump.
//!
//! * The WebSocket carries one Zenoh batch per message with no length
//!   prefix ([`Unframed`]).
//! * Time is the `now` of the last poll, never decreasing; a sleep is due
//!   when that passes its deadline, and the earliest pending deadline is
//!   [`Pump::next_wake_ms`].
//! * While no command runs the actor waits in [`ZenohLite::next_event`],
//!   which sends keep-alives and watches the lease, so the page must keep
//!   polling on [`Pump::next_wake_ms`] for the session to stay up.
//! * Commands run one at a time, in order.
//! * One pump is one connection: after the link closes or the session goes
//!   down (also while idle: the lease, the router's Close),
//!   [`Pump::is_down`] says so, input is ignored and every command fails
//!   with [`Failure::Down`]; the page makes a new pump for a new WebSocket.
//!
//! Pure Rust with owned values (no `js_sys`), so it is tested natively;
//! `mesh_js` wraps it for JS.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::convert::Infallible;
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use middleware::key::Key;
use middleware::transport::{Flow, HandlerError, RequestOptions, Requester, SenderId, Target};
use middleware::zenoh_lite::message::{ErrorType, MessageLink, Recv, Unframed};
use middleware::zenoh_lite::{Clock, Config, LiteError, ZenohLite, Zid};

/// Bytes of one received or sent batch (the batch size we announce).
pub const BATCH: usize = 8192;
/// The adapter's buffers: a batch and its length.
const FRAMED: usize = BATCH + 2;

/// What the page asks the session to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    /// Run the handshake on the connected WebSocket.
    Open,
    /// Send Close and end the session.
    Close,
    /// Ask `key` once: of the node `node` (its per-node key, only its
    /// reply accepted), or of whoever serves `key` when `None`.
    Request {
        /// The node, by sender id.
        node: Option<SenderId>,
        /// The key.
        key: String,
        /// The request body.
        body: Vec<u8>,
        /// How long to wait for a reply.
        timeout_ms: u32,
    },
    /// Ask `key` of every node (`@mw/node/*/<key>`) and gather the replies
    /// until the router's final response, `max` of them or the timeout.
    Discover {
        /// The key.
        key: String,
        /// The request body.
        body: Vec<u8>,
        /// How long to gather.
        timeout_ms: u32,
        /// At most this many replies.
        max: usize,
    },
}

/// Why a command failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// The node answered with this `HandlerError` code.
    Handler(u8),
    /// No reply before the final response or the timeout.
    NoReply,
    /// The link closed or the session is not up.
    Down,
    /// The request or its reply does not fit one batch.
    TooLarge,
    /// Anything else (a bad key, a refused open), in words.
    Other(String),
}

/// One reply to a [`Cmd::Discover`]: its sender, and the body or the
/// `HandlerError` code.
pub type Gathered = (Option<SenderId>, Result<Vec<u8>, u8>);

/// How a command ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The session opened; our sender id.
    Opened(SenderId),
    /// The session was closed.
    Closed,
    /// The reply body of a [`Cmd::Request`].
    Reply(Vec<u8>),
    /// The replies to a [`Cmd::Discover`], in arrival order.
    Replies(Vec<Gathered>),
    /// The command failed.
    Failed(Failure),
}

/// A finished command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Done {
    /// The id [`Pump::submit`] returned.
    pub id: u32,
    /// How it ended.
    pub outcome: Outcome,
}

/// What the page and the actor share.
#[derive(Default)]
struct Shared {
    inbox: VecDeque<Vec<u8>>,
    closed: bool,
    outbox: Vec<Vec<u8>>,
    now: u64,
    wake: Option<u64>,
    cmds: VecDeque<(u32, Cmd)>,
    done: Vec<Done>,
    /// The session was opened (or its open tried) and is not up now.
    down: bool,
}

type Cell = Rc<RefCell<Shared>>;

/// The WebSocket as the client's message link: the inbox and outbox.
struct WsLink(Cell);

impl ErrorType for WsLink {
    type Error = Infallible;
}

impl MessageLink for WsLink {
    async fn recv(&mut self, buf: &mut [u8]) -> Result<Recv, Infallible> {
        poll_fn(|_| {
            let mut s = self.0.borrow_mut();
            if let Some(m) = s.inbox.pop_front() {
                return Poll::Ready(Ok(match buf.get_mut(..m.len()) {
                    Some(slot) => {
                        slot.copy_from_slice(&m);
                        Recv::Message(m.len())
                    }
                    None => Recv::Oversized,
                }));
            }
            if s.closed {
                Poll::Ready(Ok(Recv::Closed))
            } else {
                Poll::Pending
            }
        })
        .await
    }

    async fn send(&mut self, message: &[u8]) -> Result<(), Infallible> {
        self.0.borrow_mut().outbox.push(message.to_vec());
        Ok(())
    }
}

/// The page's time: the `now` of the last poll.
struct WasmClock(Cell);

impl Clock for WasmClock {
    fn now_ms(&mut self) -> u64 {
        self.0.borrow().now
    }

    async fn sleep_until_ms(&mut self, deadline_ms: u64) {
        poll_fn(|_| {
            let mut s = self.0.borrow_mut();
            if s.now >= deadline_ms {
                return Poll::Ready(());
            }
            s.wake = Some(s.wake.map_or(deadline_ms, |w| w.min(deadline_ms)));
            Poll::Pending
        })
        .await
    }
}

type Client = ZenohLite<Unframed<WsLink, FRAMED>, WasmClock, BATCH, BATCH>;

fn failure(e: LiteError<Infallible>) -> Failure {
    match e {
        LiteError::Handler(h) => Failure::Handler(h.code()),
        LiteError::NoReply => Failure::NoReply,
        LiteError::Io(never) => match never {},
        LiteError::Eof | LiteError::Down(_) | LiteError::NotOpen => Failure::Down,
        LiteError::TooLarge | LiteError::BufferTooSmall { .. } => Failure::TooLarge,
        LiteError::InvalidKey => Failure::Other("not a valid key".into()),
        other => Failure::Other(format!("{other:?}")),
    }
}

fn timeout(ms: u32) -> RequestOptions<'static> {
    RequestOptions::DEFAULT.with_timeout(Duration::from_millis(u64::from(ms)))
}

enum Either<A, B> {
    Left(A),
    Right(B),
}

/// Polls both; the first ready wins and the other is dropped.
async fn race<A: Future, B: Future>(a: A, b: B) -> Either<A::Output, B::Output> {
    let mut a = std::pin::pin!(a);
    let mut b = std::pin::pin!(b);
    poll_fn(|cx| {
        if let Poll::Ready(x) = a.as_mut().poll(cx) {
            return Poll::Ready(Either::Left(x));
        }
        if let Poll::Ready(y) = b.as_mut().poll(cx) {
            return Poll::Ready(Either::Right(y));
        }
        Poll::Pending
    })
    .await
}

/// The next queued command, once there is one.
async fn queued(cell: &Cell) -> (u32, Cmd) {
    poll_fn(|_| match cell.borrow_mut().cmds.pop_front() {
        Some(c) => Poll::Ready(c),
        None => Poll::Pending,
    })
    .await
}

/// The next command, servicing the session meanwhile if it is up.
async fn next_cmd(cell: &Cell, client: &mut Client, buf: &mut [u8], up: &mut bool) -> (u32, Cmd) {
    while *up {
        // next_event is cancel safe: dropping it for a command loses
        // nothing. No subscriptions or queryables are declared, so an
        // event is a stray one, dropped.
        match race(queued(cell), client.next_event(buf)).await {
            Either::Left(c) => return c,
            Either::Right(Ok(_)) => {}
            Either::Right(Err(_)) => {
                *up = false;
                cell.borrow_mut().down = true;
            }
        }
    }
    queued(cell).await
}

async fn run(cell: Cell, mut client: Box<Client>) {
    let mut buf = vec![0u8; BATCH];
    let mut up = false;
    let mut tried = false;
    loop {
        let (id, cmd) = next_cmd(&cell, &mut client, &mut buf, &mut up).await;
        tried |= cmd == Cmd::Open;
        let outcome = match cmd {
            Cmd::Open => match client.open().await {
                Ok(()) => {
                    up = true;
                    Outcome::Opened(client.sender_id())
                }
                Err(e) => Outcome::Failed(failure(e)),
            },
            Cmd::Close => {
                let _ = client.close().await;
                up = false;
                Outcome::Closed
            }
            Cmd::Request { .. } | Cmd::Discover { .. } if !up => Outcome::Failed(Failure::Down),
            Cmd::Request {
                node,
                key,
                body,
                timeout_ms,
            } => match Key::new(&key) {
                Err(_) => Outcome::Failed(Failure::Other(format!("{key:?} is not a valid key"))),
                Ok(k) => {
                    let target = node.map_or(Target::Any, Target::Node);
                    let options = timeout(timeout_ms).with_target(target);
                    match client.request_with(k, &body, &mut buf, &options).await {
                        Ok(reply) => Outcome::Reply(reply.to_vec()),
                        Err(e) => Outcome::Failed(failure(e)),
                    }
                }
            },
            Cmd::Discover {
                key,
                body,
                timeout_ms,
                max,
            } => match Key::new(&key) {
                Err(_) => Outcome::Failed(Failure::Other(format!("{key:?} is not a valid key"))),
                Ok(k) => {
                    let mut got: Vec<Gathered> = Vec::new();
                    let r = client
                        .request_each_nodes(k, &body, &mut buf, &timeout(timeout_ms), max, |r| {
                            got.push((
                                r.sender,
                                r.result.map(<[u8]>::to_vec).map_err(HandlerError::code),
                            ));
                            Flow::Continue
                        })
                        .await;
                    match r {
                        Ok(_) => Outcome::Replies(got),
                        Err(e) => Outcome::Failed(failure(e)),
                    }
                }
            },
        };
        if matches!(outcome, Outcome::Failed(Failure::Down)) {
            up = false;
        }
        let mut s = cell.borrow_mut();
        if tried && !up {
            s.down = true;
        }
        s.done.push(Done { id, outcome });
    }
}

/// One Zenoh session over one WebSocket (see the module docs).
pub struct Pump {
    cell: Cell,
    actor: Pin<Box<dyn Future<Output = ()>>>,
    next_id: u32,
    sender: SenderId,
}

impl Pump {
    /// A session with Zenoh id `zid` (the page draws 16 random bytes),
    /// Zenoh's default lease, not yet opened.
    pub fn new(zid: Zid) -> Pump {
        let cell: Cell = Rc::default();
        let link = Unframed::new(WsLink(cell.clone()));
        let client: Box<Client> = Box::new(ZenohLite::new(
            link,
            WasmClock(cell.clone()),
            Config::new(zid),
        ));
        let sender = client.sender_id();
        Pump {
            actor: Box::pin(run(cell.clone(), client)),
            cell,
            next_id: 1,
            sender,
        }
    }

    /// One WebSocket message (a batch) from the router; ignored once the
    /// session is down.
    pub fn feed(&mut self, msg: &[u8]) {
        let mut s = self.cell.borrow_mut();
        if !s.down {
            s.inbox.push_back(msg.to_vec());
        }
    }

    /// True once the session, opened before, has ended: the lease ran out,
    /// the router closed it, the link closed, or a command found it gone.
    /// It does not come back; the page closes the WebSocket and fails what
    /// it waits for. Checked after every [`Pump::poll`].
    pub fn is_down(&self) -> bool {
        self.cell.borrow().down
    }

    /// The WebSocket closed: once the messages fed before are read, the
    /// link ends and the session is down.
    pub fn close(&mut self) {
        self.cell.borrow_mut().closed = true;
    }

    /// Queues `cmd`; its [`Done`] carries the returned id.
    pub fn submit(&mut self, cmd: Cmd) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.cell.borrow_mut().cmds.push_back((id, cmd));
        id
    }

    /// Runs the session at time `now_ms` (never earlier than a previous
    /// poll's) as far as it can go, and returns the commands that finished.
    pub fn poll(&mut self, now_ms: u64) -> Vec<Done> {
        {
            let mut s = self.cell.borrow_mut();
            s.now = s.now.max(now_ms);
            s.wake = None;
        }
        let mut cx = Context::from_waker(Waker::noop());
        // The actor never ends; a Ready would only mean it panicked.
        let _ = self.actor.as_mut().poll(&mut cx);
        std::mem::take(&mut self.cell.borrow_mut().done)
    }

    /// The WebSocket messages to send, in order, each one batch with no
    /// length prefix.
    pub fn take_output(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.cell.borrow_mut().outbox)
    }

    /// When to poll next even if nothing arrives: the earliest deadline
    /// the session waits for (a keep-alive, the lease, a request timeout).
    pub fn next_wake_ms(&self) -> Option<u64> {
        self.cell.borrow().wake
    }

    /// Our sender id, as nodes see it in our query attachments.
    pub fn sender_id(&self) -> SenderId {
        self.sender
    }
}

#[cfg(test)]
mod tests;
