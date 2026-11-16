//! The multi-link bus's routing core (FL1): which of the board's links a
//! publication goes to, which link a reply goes back on, and the
//! `sys/link/sub` / `sys/link/unsub` requests by which a serial host
//! narrows what its link carries. Pure and fixed-capacity (`heapless`), no
//! embassy: the shell keeps one [`Links`] behind its mutex, the controller
//! publishes and replies into it, and each link task pops its own queues.
//!
//! # Links
//!
//! Up to [`MAX_LINKS`] links, each a [`LinkId`] with a fixed
//! [`LinkKind`]: LPUART1 ([`LinkId::UART`]) and USB CDC ([`LinkId::USB`])
//! run the serial protocol, Ethernet ([`LinkId::ZENOH`]) a Zenoh session.
//!
//! # Publications
//!
//! [`Links::publish`] puts a copy of the publication on the queue of
//! exactly the links whose filter selects its key:
//!
//! * a [`LinkKind::Serial`] link selects everything until its host narrows
//!   it; after the first `sys/link/sub` it selects the keys matched by one
//!   of its (at most `S`) key expressions ([`Expr`]);
//! * a [`LinkKind::Remote`] link (Zenoh) selects what its peers declared
//!   subscriptions for: the caller answers that per key (the Zenoh
//!   client's table of remote subscriptions), so nothing is sent that no
//!   peer wants;
//! * an [`LinkKind::Absent`] link selects nothing.
//!
//! Each link has its own queue of `Q` publications. A full queue drops the
//! new publication for that link only and counts it
//! ([`Links::dropped`]); the other links still get theirs, and publishing
//! never waits. A key or payload too large for the queues
//! ([`TooLarge`]) is queued nowhere.
//!
//! # Requests and replies
//!
//! An inbound request carries the link it arrived on and its correlation
//! id as a [`ReplyTo`]; [`Links::reply`] queues the answer on that link's
//! reply queue (`R` deep) and no other, and the link pops it with
//! [`Links::pop_reply`]. A full reply queue refuses the reply
//! ([`ReplyFull`]) rather than drop it unseen.
//!
//! # `sys/link/sub`, `sys/link/unsub`
//!
//! Handled by [`Links::link_request`] before the request service, and only
//! for the asking link:
//!
//! | key              | body                     | effect on the asking link | reply |
//! |------------------|--------------------------|---------------------------|-------|
//! | `sys/link/sub`   | key expression, UTF-8    | from "everything": exactly this expression; otherwise adds it (already there: unchanged) | `Ok`, empty |
//! | `sys/link/unsub` | key expression, UTF-8    | removes it if present; on a link still selecting everything, nothing | `Ok`, empty |
//!
//! * A body that is not UTF-8 or not a valid expression ([`Expr::new`]):
//!   `InvalidRequest`, filter unchanged.
//! * `sub` with `S` expressions already held: `Unavailable` (the link may
//!   retry after an `unsub`), filter unchanged.
//! * Either key on a remote or absent link: `NotFound` (its filter is not
//!   the host's to set; over Zenoh the management gate refuses it first,
//!   [`crate::zenoh_mgmt`]).
//!
//! Removing the last expression leaves the link selecting nothing (it was
//! narrowed); `sys/link/sub **` selects everything again, as does
//! [`Links::reset`], which the shell calls when a new host connects
//! (USB enumeration, a serial reconnect). Publications already queued are
//! not filtered again.

pub mod expr;

use heapless::{Deque, Vec};
use middleware::Key;
use middleware::key::{AsKey, KeyBuf, MAX_KEY_LEN};
use middleware::transport::HandlerError;

pub use expr::{Expr, ExprError};

/// Most links a board has.
pub const MAX_LINKS: usize = 4;

/// The `sys/link/*` keys.
pub mod keys {
    use middleware::Key;

    /// Narrows the asking serial link to one more key expression.
    pub const SUB: Key<'static> = Key::from_static("sys/link/sub");
    /// Removes a key expression from the asking serial link.
    pub const UNSUB: Key<'static> = Key::from_static("sys/link/unsub");
}

/// Whether `key` is one of the link requests [`Links::link_request`]
/// handles.
pub fn is_link_key(key: Key<'_>) -> bool {
    key == keys::SUB || key == keys::UNSUB
}

/// One of the board's links, by index (`0..MAX_LINKS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LinkId(u8);

impl LinkId {
    /// LPUART1, the serial protocol.
    pub const UART: LinkId = LinkId(0);
    /// USB CDC-ACM on OTG1, the serial protocol.
    pub const USB: LinkId = LinkId(1);
    /// Ethernet, the Zenoh session.
    pub const ZENOH: LinkId = LinkId(2);
    /// Every link id, in index order.
    pub const ALL: [LinkId; MAX_LINKS] = [LinkId(0), LinkId(1), LinkId(2), LinkId(3)];

    /// The link at `index`, `None` past [`MAX_LINKS`].
    pub const fn new(index: usize) -> Option<Self> {
        if index < MAX_LINKS {
            // Lossless: index < MAX_LINKS = 4.
            Some(LinkId(index as u8))
        } else {
            None
        }
    }

    /// The link's index, below [`MAX_LINKS`].
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// What a link is, which decides its filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    /// Not fitted: selects nothing, takes no requests.
    Absent,
    /// The serial protocol (LPUART1, USB CDC): everything until the host
    /// narrows it with `sys/link/sub`.
    Serial,
    /// A Zenoh session: the subscriptions its peers declared, decided by
    /// the caller of [`Links::publish`].
    Remote,
}

/// A set of links (a bit per [`LinkId`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LinkSet(u8);

impl LinkSet {
    /// No links.
    pub const EMPTY: LinkSet = LinkSet(0);

    /// Whether `link` is in the set.
    pub const fn contains(self, link: LinkId) -> bool {
        self.0 & (1 << link.0) != 0
    }

    /// The set with `link` added.
    pub const fn with(self, link: LinkId) -> Self {
        LinkSet(self.0 | (1 << link.0))
    }

    /// Whether the set is empty.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// What one [`Links::publish`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Fanout {
    /// The links the publication was queued on.
    pub queued: LinkSet,
    /// The links whose filter selected it but whose queue was full (each
    /// counted in [`Links::dropped`]).
    pub dropped: LinkSet,
}

/// A publication's key or payload does not fit the queues (`P` bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooLarge;

/// A link's reply queue is full; the reply was not queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplyFull;

/// A queued publication, owned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Publication<const P: usize> {
    /// Its key.
    pub key: KeyBuf<MAX_KEY_LEN>,
    /// Its payload.
    pub payload: Vec<u8, P>,
}

impl<const P: usize> Publication<P> {
    /// The key, borrowed.
    pub fn key(&self) -> Key<'_> {
        self.key.as_key()
    }
}

/// Where a request came from, and so where its reply goes: the link and
/// the link's own correlation id (a serial frame's id, a Zenoh query id).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplyTo {
    /// The link the request arrived on.
    pub link: LinkId,
    /// The request's correlation id on that link.
    pub id: u32,
}

/// A queued reply, owned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply<const P: usize> {
    /// The request's correlation id on its link.
    pub id: u32,
    /// The body, or the error.
    pub result: Result<Vec<u8, P>, HandlerError>,
}

/// A link's filter, as [`Links::filter`] shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterView<'a> {
    /// Not fitted: nothing.
    Absent,
    /// A serial link its host has not narrowed: everything.
    Everything,
    /// A narrowed serial link: the keys one of these expressions matches
    /// (none when empty).
    Only(&'a [Expr]),
    /// A Zenoh link: its peers' subscriptions.
    Remote,
}

/// A serial link's filter.
#[derive(Debug, Clone)]
enum Filter<const S: usize> {
    Everything,
    Only(Vec<Expr, S>),
}

/// One link's state.
#[derive(Debug, Clone)]
struct Link<const Q: usize, const R: usize, const S: usize, const P: usize> {
    kind: LinkKind,
    filter: Filter<S>,
    queue: Deque<Publication<P>, Q>,
    replies: Deque<Reply<P>, R>,
    dropped: u32,
}

impl<const Q: usize, const R: usize, const S: usize, const P: usize> Link<Q, R, S, P> {
    const fn new(kind: LinkKind) -> Self {
        Link {
            kind,
            filter: Filter::Everything,
            queue: Deque::new(),
            replies: Deque::new(),
            dropped: 0,
        }
    }

    /// Whether this link's filter selects `key`; `remote` answers for a
    /// remote link.
    fn selects(&self, id: LinkId, key: Key<'_>, remote: &impl Fn(LinkId, Key<'_>) -> bool) -> bool {
        match (self.kind, &self.filter) {
            (LinkKind::Absent, _) => false,
            (LinkKind::Remote, _) => remote(id, key),
            (LinkKind::Serial, Filter::Everything) => true,
            (LinkKind::Serial, Filter::Only(exprs)) => exprs.iter().any(|e| e.matches(key)),
        }
    }
}

/// The links of a board: their kinds, filters, publication queues (`Q`
/// deep), reply queues (`R` deep), at most `S` key expressions per serial
/// link, and bodies of at most `P` bytes. See the module docs.
#[derive(Debug, Clone)]
pub struct Links<const Q: usize, const R: usize, const S: usize, const P: usize> {
    links: [Link<Q, R, S, P>; MAX_LINKS],
}

impl<const Q: usize, const R: usize, const S: usize, const P: usize> Links<Q, R, S, P> {
    /// Links of these kinds (by [`LinkId`] index), each selecting
    /// everything its kind allows, queues empty.
    pub const fn new(kinds: [LinkKind; MAX_LINKS]) -> Self {
        let [a, b, c, d] = kinds;
        Links {
            links: [Link::new(a), Link::new(b), Link::new(c), Link::new(d)],
        }
    }

    /// The RT1050-EVKB's links: LPUART1 and USB serial, Ethernet Zenoh,
    /// the fourth absent.
    pub const fn evkb() -> Self {
        Self::new([
            LinkKind::Serial,
            LinkKind::Serial,
            LinkKind::Remote,
            LinkKind::Absent,
        ])
    }

    fn link(&self, id: LinkId) -> Option<&Link<Q, R, S, P>> {
        self.links.get(id.index())
    }

    fn link_mut(&mut self, id: LinkId) -> Option<&mut Link<Q, R, S, P>> {
        self.links.get_mut(id.index())
    }

    /// The kind of `link`.
    pub fn kind(&self, link: LinkId) -> LinkKind {
        self.link(link).map_or(LinkKind::Absent, |l| l.kind)
    }

    /// The filter of `link`.
    pub fn filter(&self, link: LinkId) -> FilterView<'_> {
        match self.link(link) {
            None => FilterView::Absent,
            Some(l) => match (l.kind, &l.filter) {
                (LinkKind::Absent, _) => FilterView::Absent,
                (LinkKind::Remote, _) => FilterView::Remote,
                (LinkKind::Serial, Filter::Everything) => FilterView::Everything,
                (LinkKind::Serial, Filter::Only(e)) => FilterView::Only(e),
            },
        }
    }

    /// Queues a publication on every link whose filter selects `key`
    /// (`remote` answers for the remote links: whether a peer subscribed);
    /// a link with a full queue drops it and counts the drop. Never waits.
    /// [`TooLarge`] (queued nowhere) if the payload exceeds `P` bytes.
    pub fn publish(
        &mut self,
        key: Key<'_>,
        payload: &[u8],
        remote: impl Fn(LinkId, Key<'_>) -> bool,
    ) -> Result<Fanout, TooLarge> {
        let publication = Publication {
            key: KeyBuf::from_key(key).map_err(|_| TooLarge)?,
            payload: Vec::from_slice(payload).map_err(|_| TooLarge)?,
        };
        let mut fanout = Fanout::default();
        for (id, link) in LinkId::ALL.into_iter().zip(self.links.iter_mut()) {
            if !link.selects(id, key, &remote) {
                continue;
            }
            match link.queue.push_back(publication.clone()) {
                Ok(()) => fanout.queued = fanout.queued.with(id),
                Err(_) => {
                    link.dropped = link.dropped.saturating_add(1);
                    fanout.dropped = fanout.dropped.with(id);
                }
            }
        }
        Ok(fanout)
    }

    /// The oldest publication queued for `link`.
    pub fn pop(&mut self, link: LinkId) -> Option<Publication<P>> {
        self.link_mut(link)?.queue.pop_front()
    }

    /// Publications queued for `link`.
    pub fn queued(&self, link: LinkId) -> usize {
        self.link(link).map_or(0, |l| l.queue.len())
    }

    /// Publications `link` dropped because its queue was full, since boot
    /// (saturating).
    pub fn dropped(&self, link: LinkId) -> u32 {
        self.link(link).map_or(0, |l| l.dropped)
    }

    /// Queues the reply to the request `to` on its link only: the body
    /// (`Ok`) or the error. [`ReplyFull`] if that link's reply queue is
    /// full or the link is absent; a body over `P` bytes is replied
    /// `BufferTooSmall`.
    pub fn reply(
        &mut self,
        to: ReplyTo,
        result: Result<&[u8], HandlerError>,
    ) -> Result<(), ReplyFull> {
        let link = self.link_mut(to.link).ok_or(ReplyFull)?;
        if link.kind == LinkKind::Absent {
            return Err(ReplyFull);
        }
        let result = match result {
            Ok(body) => Vec::from_slice(body).map_err(|_| HandlerError::BufferTooSmall),
            Err(e) => Err(e),
        };
        link.replies
            .push_back(Reply { id: to.id, result })
            .map_err(|_| ReplyFull)
    }

    /// The oldest reply queued for `link`.
    pub fn pop_reply(&mut self, link: LinkId) -> Option<Reply<P>> {
        self.link_mut(link)?.replies.pop_front()
    }

    /// A `sys/link/sub` or `sys/link/unsub` request from `link` (see the
    /// module docs): `None` when `key` is neither (the request goes on to
    /// the service), else the reply, whose body is empty. Changes only
    /// `link`'s filter.
    pub fn link_request(
        &mut self,
        link: LinkId,
        key: Key<'_>,
        body: &[u8],
    ) -> Option<Result<(), HandlerError>> {
        if !is_link_key(key) {
            return None;
        }
        let Some(l) = self.link_mut(link) else {
            return Some(Err(HandlerError::NotFound));
        };
        if l.kind != LinkKind::Serial {
            return Some(Err(HandlerError::NotFound));
        }
        let Ok(expr) = Expr::from_body(body) else {
            return Some(Err(HandlerError::InvalidRequest));
        };
        let reply = if key == keys::SUB {
            subscribe(&mut l.filter, expr)
        } else {
            unsubscribe(&mut l.filter, &expr);
            Ok(())
        };
        Some(reply)
    }

    /// A new host on `link`: its filter selects everything again and its
    /// queues are emptied (the drop count is kept).
    pub fn reset(&mut self, link: LinkId) {
        if let Some(l) = self.link_mut(link) {
            l.filter = Filter::Everything;
            l.queue.clear();
            l.replies.clear();
        }
    }
}

/// `sys/link/sub`: narrows from everything to `expr`, or adds it.
fn subscribe<const S: usize>(filter: &mut Filter<S>, expr: Expr) -> Result<(), HandlerError> {
    match filter {
        Filter::Everything => {
            let mut only = Vec::new();
            only.push(expr).map_err(|_| HandlerError::Unavailable)?;
            *filter = Filter::Only(only);
            Ok(())
        }
        Filter::Only(exprs) if exprs.contains(&expr) => Ok(()),
        Filter::Only(exprs) => exprs.push(expr).map_err(|_| HandlerError::Unavailable),
    }
}

/// `sys/link/unsub`: removes `expr` from a narrowed filter.
fn unsubscribe<const S: usize>(filter: &mut Filter<S>, expr: &Expr) {
    if let Filter::Only(exprs) = filter {
        exprs.retain(|e| e != expr);
    }
}

#[cfg(test)]
mod tests;
