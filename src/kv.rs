//! Persistence: a key-value store behind one trait, local or remote.
//!
//! Keys are the middleware's validated [`Key`]s, so a stored entry and a
//! published sample share one address space (`config/wifi/ssid`). Values are
//! opaque bytes; pair a store with a [`Codec`](crate::codec::Codec) for typed
//! values, as [`typed::Client`](crate::typed::Client) does for messages.
//!
//! # Contract
//!
//! * **Revisions.** Every successful write (put or delete) is stamped with a
//!   [`Revision`] taken from one counter per store. The counter strictly
//!   increases, starts at 1, and is never reused, not even across a restart
//!   of a durable store. While a store stays open it issues revisions
//!   consecutively, and a failed write consumes none. Across a restart they
//!   only increase: a store may skip numbers it reserved but never used (see
//!   `file::Durability::Batched`). Clients sharing one store see each other's
//!   revisions, so one client's own writes need not be consecutive. An
//!   entry's revision is the revision of the write that last set it.
//! * **Conditional writes.** [`Expect`] makes a put or delete
//!   compare-and-swap. The write happens only if the key's current state
//!   matches: [`Expect::Any`] (always), [`Expect::Absent`] (key not
//!   present), or [`Expect::Exact`] (present at that revision). On a mismatch
//!   the result is [`KvError::Conflict`] carrying the current revision, and
//!   the store is unchanged.
//! * **Buffers.** As in [`transport`](crate::transport), reads copy into
//!   caller-owned buffers, so heap-free stores and remote stores share one
//!   signature. A value that does not fit is [`KvError::BufferTooSmall`]
//!   with the size needed, and the entry is unaffected.
//! * **Limits.** [`KvStore::LIMITS`] states the largest key and value the
//!   store accepts. A larger write is [`KvError::TooLarge`]; a store with no
//!   room left for a new entry returns [`KvError::Full`]. Neither changes
//!   the store.
//! * **Scans** visit matching entries in ascending byte order of their
//!   keys, starting strictly after the `after` cursor, so a caller pages
//!   through a large store by passing the last key it saw. A scan is
//!   bounded: it ends when the callback returns [`Flow::Stop`], when the
//!   store has nothing more, or at the store's documented page limit. The
//!   returned [`Scan`] says whether it reached the end.
//! * **Durability** is per implementation, and each states it: when a
//!   successful write survives power loss, and what a torn write leaves
//!   behind.
//!
//! # Sync and async
//!
//! [`KvStore`] is async: a remote store waits on the network, and a flash
//! store may wait on its driver. [`KvStoreSync`] is the same contract with
//! blocking methods, for stores that never wait, such as an in-RAM table or
//! a local file. It exists because a [`Handler`](crate::transport::Handler)
//! (the server side of request/reply) is synchronous. [`Blocking`] adapts
//! any [`KvStoreSync`] into a [`KvStore`].
//!
//! # Implementations
//!
//! * [`table::KvTable`] is a fixed-capacity, heap-free store in RAM
//!   (always built, `no_std`). It is the reference implementation.
//! * [`wire`] is the request/reply format for a remote store (always built,
//!   pure).
//! * [`remote::RemoteKv`] is a [`KvStore`] over any
//!   [`Requester`](crate::transport::Requester), and
//!   [`remote::KvHandler`] serves any [`KvStoreSync`] through any
//!   [`Responder`](crate::transport::Responder). Together they put a store
//!   behind Zenoh, gRPC, serial, or the in-process bus.
//! * `file::FileKv` (feature `kv-file`, `std`) is a durable append-only
//!   log on the local filesystem.

pub mod remote;
pub mod table;
pub mod wire;

#[cfg(feature = "kv-file")]
pub mod file;

use crate::key::{Key, KeyFilter};
pub use crate::transport::Flow;

/// The stamp of one successful write. Strictly increasing per store and
/// never reused; see the module docs. Revision 0 is never issued.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Revision(pub u64);

/// The precondition of a conditional write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expect {
    /// Write unconditionally.
    Any,
    /// Write only if the key is not present.
    Absent,
    /// Write only if the key is present at exactly this revision.
    Exact(Revision),
}

/// A stored value as read back: its bytes in the caller's buffer, and the
/// revision of the write that set it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry<'b> {
    pub value: &'b [u8],
    pub revision: Revision,
}

/// The largest key and value a store accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KvLimits {
    /// Longest key in bytes; at most [`crate::key::MAX_KEY_LEN`].
    pub max_key: usize,
    /// Longest value in bytes.
    pub max_value: usize,
}

/// How a [`KvStore::scan`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scan {
    /// Entries passed to the callback.
    pub visited: usize,
    /// `true` if no matching entry is left after the last one visited.
    /// `false` if the callback stopped the scan or the page limit was
    /// reached. Resume by passing the last visited key as `after`.
    pub complete: bool,
}

/// Failure of a store operation. The store is unchanged by any error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum KvError<E> {
    /// The [`Expect`] precondition did not hold. `current` is the key's
    /// revision, or `None` if the key is absent.
    Conflict { current: Option<Revision> },
    /// The caller's buffer is shorter than the value, which is `needed`
    /// bytes long.
    BufferTooSmall { needed: usize },
    /// The key or value exceeds the store's [`KvLimits`].
    TooLarge,
    /// No room for a new entry.
    Full,
    /// The backend failed (I/O, transport, corruption).
    Backend(E),
}

/// An async key-value store; the contract is in the module docs.
pub trait KvStore {
    /// Backend-specific failure, carried in [`KvError::Backend`].
    type Error: core::fmt::Debug;

    /// The largest key and value this store accepts.
    const LIMITS: KvLimits;

    /// Reads `key` into `buf`. Returns `Ok(None)` if the key is absent.
    async fn get<'b>(
        &mut self,
        key: Key<'_>,
        buf: &'b mut [u8],
    ) -> Result<Option<Entry<'b>>, KvError<Self::Error>>;

    /// Writes `value` at `key` if `expect` holds, and returns the new
    /// revision.
    async fn put(
        &mut self,
        key: Key<'_>,
        value: &[u8],
        expect: Expect,
    ) -> Result<Revision, KvError<Self::Error>>;

    /// Removes `key` if `expect` holds. Returns the revision the removed
    /// entry had (the put that last set it). The delete itself consumes a new
    /// revision, which is not returned. If the key is already absent,
    /// [`Expect::Any`] and [`Expect::Absent`] return `Ok(None)` and write
    /// nothing (no revision is consumed), and [`Expect::Exact`] is a
    /// [`KvError::Conflict`].
    async fn delete(
        &mut self,
        key: Key<'_>,
        expect: Expect,
    ) -> Result<Option<Revision>, KvError<Self::Error>>;

    /// Visits the entries matching `filter` whose keys sort strictly after
    /// `after`, in ascending key order. Each value is read into `buf`.
    /// `on_entry`'s borrows are valid only during that call.
    async fn scan<F>(
        &mut self,
        filter: KeyFilter<'_>,
        after: Option<Key<'_>>,
        buf: &mut [u8],
        on_entry: F,
    ) -> Result<Scan, KvError<Self::Error>>
    where
        F: FnMut(Key<'_>, Entry<'_>) -> Flow;
}

/// [`KvStore`] with blocking methods, for stores that never wait. See the
/// module docs for why both exist.
pub trait KvStoreSync {
    /// Backend-specific failure, carried in [`KvError::Backend`].
    type Error: core::fmt::Debug;

    /// The largest key and value this store accepts.
    const LIMITS: KvLimits;

    /// As [`KvStore::get`].
    fn get<'b>(
        &mut self,
        key: Key<'_>,
        buf: &'b mut [u8],
    ) -> Result<Option<Entry<'b>>, KvError<Self::Error>>;

    /// As [`KvStore::put`].
    fn put(
        &mut self,
        key: Key<'_>,
        value: &[u8],
        expect: Expect,
    ) -> Result<Revision, KvError<Self::Error>>;

    /// As [`KvStore::delete`].
    fn delete(
        &mut self,
        key: Key<'_>,
        expect: Expect,
    ) -> Result<Option<Revision>, KvError<Self::Error>>;

    /// As [`KvStore::scan`].
    fn scan<F>(
        &mut self,
        filter: KeyFilter<'_>,
        after: Option<Key<'_>>,
        buf: &mut [u8],
        on_entry: F,
    ) -> Result<Scan, KvError<Self::Error>>
    where
        F: FnMut(Key<'_>, Entry<'_>) -> Flow;
}

/// Adapts a [`KvStoreSync`] into a [`KvStore`] whose futures complete on
/// first poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocking<S>(pub S);

impl<S: KvStoreSync> KvStore for Blocking<S> {
    type Error = S::Error;
    const LIMITS: KvLimits = S::LIMITS;

    async fn get<'b>(
        &mut self,
        key: Key<'_>,
        buf: &'b mut [u8],
    ) -> Result<Option<Entry<'b>>, KvError<Self::Error>> {
        self.0.get(key, buf)
    }

    async fn put(
        &mut self,
        key: Key<'_>,
        value: &[u8],
        expect: Expect,
    ) -> Result<Revision, KvError<Self::Error>> {
        self.0.put(key, value, expect)
    }

    async fn delete(
        &mut self,
        key: Key<'_>,
        expect: Expect,
    ) -> Result<Option<Revision>, KvError<Self::Error>> {
        self.0.delete(key, expect)
    }

    async fn scan<F>(
        &mut self,
        filter: KeyFilter<'_>,
        after: Option<Key<'_>>,
        buf: &mut [u8],
        on_entry: F,
    ) -> Result<Scan, KvError<Self::Error>>
    where
        F: FnMut(Key<'_>, Entry<'_>) -> Flow,
    {
        self.0.scan(filter, after, buf, on_entry)
    }
}

/// Whether `expect` holds for a key whose current revision is `current`
/// (`None` if absent). Shared by every implementation so they agree.
pub const fn expect_holds(expect: Expect, current: Option<Revision>) -> bool {
    match (expect, current) {
        (Expect::Any, _) => true,
        (Expect::Absent, None) => true,
        (Expect::Absent, Some(_)) => false,
        (Expect::Exact(_), None) => false,
        (Expect::Exact(want), Some(have)) => want.0 == have.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expect_holds_truth_table() {
        let r1 = Some(Revision(1));
        assert!(expect_holds(Expect::Any, None));
        assert!(expect_holds(Expect::Any, r1));
        assert!(expect_holds(Expect::Absent, None));
        assert!(!expect_holds(Expect::Absent, r1));
        assert!(!expect_holds(Expect::Exact(Revision(1)), None));
        assert!(expect_holds(Expect::Exact(Revision(1)), r1));
        assert!(!expect_holds(Expect::Exact(Revision(2)), r1));
    }
}
