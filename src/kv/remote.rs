//! A key-value store across request/reply: [`RemoteKv`] is the client,
//! [`KvHandler`] the server, and [`wire`] the format between
//! them. Both are `no_std` and heap-free, and work over any transport.
//!
//! ```text
//! RemoteKv<R: Requester, N> ──wire::Request──▶ Responder ──▶ KvHandler<S: KvStoreSync>
//!                           ◀─wire::Response──             ◀──
//! ```
//!
//! # Client: [`RemoteKv`]
//!
//! Every operation is one request to the store's key (a [`KeyBuf`] chosen
//! at construction, such as `kv/config`), except a scan, which pages
//! through as many requests as it needs. The client owns an `N`-byte
//! request buffer and an `N`-byte response buffer; `N` must be at least
//! [`wire::MIN_BUFFER`] (checked at compile time).
//!
//! * **Limits.** [`KvStore::LIMITS`] is what the client can carry: keys up
//!   to [`MAX_KEY_LEN`] and values up to [`wire::max_value`]`(N)` bytes. A
//!   larger value is refused locally with [`KvError::TooLarge`] without a
//!   request. The served store may accept less, and answers
//!   [`KvError::TooLarge`] itself.
//! * **Errors.** The served store's [`KvError::Conflict`],
//!   [`KvError::TooLarge`] and [`KvError::Full`] come back unchanged, and
//!   its [`KvError::Backend`] as [`RemoteError::Remote`] with the code the
//!   handler chose. A transport failure is [`RemoteError::Transport`], an
//!   undecodable reply [`RemoteError::Malformed`], a well-formed reply that
//!   does not answer the request [`RemoteError::Unexpected`], and a reply
//!   that the responder's buffer could not hold
//!   [`RemoteError::ResponderBuffer`]. [`KvError::BufferTooSmall`] is the
//!   caller's buffer only, checked when the value is copied out of the
//!   response buffer; the stored entry is unaffected.
//! * **Scans.** A scan requests pages of up to the entries it still may
//!   visit, starting after the caller's cursor and then after the last key
//!   of the previous page, and stops when the callback returns
//!   [`Flow::Stop`], when the responder says the page is the last one, or
//!   after the page limit ([`DEFAULT_SCAN_LIMIT`] entries, set with
//!   [`RemoteKv::with_scan_limit`]). It is bounded: every page except the
//!   last must bring at least one new entry, each strictly after the
//!   cursor and matching the filter, or the scan fails with
//!   [`RemoteError::Unexpected`]; so at most `limit + 1` requests are made.
//!   Pages are separate requests, so entries written between them are seen
//!   if their key sorts after the cursor; each key is still visited at most
//!   once.
//! * **Durability** is the served store's.
//! * **Cancel safety** is the requester's: dropping an operation abandons
//!   its request (a dropped put may or may not have been applied).
//!
//! # Server: [`KvHandler`]
//!
//! A [`Handler`] that decodes a [`wire::Request`], runs it on a
//! [`KvStoreSync`] and encodes the [`wire::Response`] into the responder's
//! reply buffer, so any store is served through any
//! [`Responder`](crate::transport::Responder). It answers every request it
//! is given; the responder's filter decides which key that is. A body that
//! does not decode is [`HandlerError::InvalidRequest`].
//!
//! Values are read straight into the reply buffer. A get needs
//! [`wire::ENTRY_VALUE_AT`] bytes besides the value. A scan page reserves,
//! for each entry, room for the store's longest key
//! (`S::LIMITS.max_key`) before reading its value, and holds as many
//! entries as fit, up to the request's `max_entries`; an entry that does
//! not fit ends the page (not complete), and the client asks again after
//! it. If not even the first entry fits, or a get's value does not, the
//! reply is [`wire::Failure::BufferTooSmall`] with the reply size needed.
//! Serve with a reply buffer of at least the client's `N` bytes and every
//! value within the client's limits fits.

use crate::key::{AsKey, Key, KeyBuf, KeyFilter, MAX_KEY_LEN};
use crate::transport::{
    Handler, HandlerError, Request as TransportRequest, RequestOptions, Requester,
};

use super::wire::{self, DecodeError, Failure, PageWriter, Request, Response};
use super::{Entry, Expect, Flow, KvError, KvLimits, KvStore, KvStoreSync, Revision, Scan};

/// Entries a [`RemoteKv`] scan visits at most, unless set with
/// [`RemoteKv::with_scan_limit`].
pub const DEFAULT_SCAN_LIMIT: usize = 1024;

/// Failure of a [`RemoteKv`] operation, carried in [`KvError::Backend`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteError<E> {
    /// The transport failed (including the responder's
    /// [`HandlerError`], as the transport reports it).
    Transport(E),
    /// The reply did not decode.
    Malformed(DecodeError),
    /// The reply decoded but does not answer the request: the wrong kind
    /// of response, or a scan page that made no progress, went backwards or
    /// held an entry outside the filter.
    Unexpected,
    /// The responder's reply buffer cannot hold the reply, which needs
    /// `needed` bytes.
    ResponderBuffer {
        /// Reply bytes needed.
        needed: u32,
    },
    /// The served store's backend failed with this code (chosen by its
    /// [`KvHandler`]).
    Remote(u16),
}

type RemoteResult<T, E> = Result<T, KvError<RemoteError<E>>>;

fn from_failure<E>(f: Failure) -> KvError<RemoteError<E>> {
    match f {
        Failure::Conflict { current } => KvError::Conflict { current },
        Failure::BufferTooSmall { needed } => {
            KvError::Backend(RemoteError::ResponderBuffer { needed })
        }
        Failure::TooLarge => KvError::TooLarge,
        Failure::Full => KvError::Full,
        Failure::Backend(code) => KvError::Backend(RemoteError::Remote(code)),
    }
}

const fn unexpected<E>() -> KvError<RemoteError<E>> {
    KvError::Backend(RemoteError::Unexpected)
}

/// A [`KvStore`] served by a [`KvHandler`] somewhere across `R`; see the
/// module docs.
#[derive(Debug, Clone)]
pub struct RemoteKv<R, const N: usize> {
    requester: R,
    key: KeyBuf<MAX_KEY_LEN>,
    options: RequestOptions<'static>,
    scan_limit: usize,
    tx: [u8; N],
    rx: [u8; N],
}

impl<R, const N: usize> RemoteKv<R, N> {
    /// Compile-time check that the buffers can hold every request.
    const BUFFER_OK: () = assert!(
        N >= wire::MIN_BUFFER,
        "RemoteKv buffers must be at least wire::MIN_BUFFER bytes"
    );

    /// A client sending to the store served at `key`, with
    /// [`RequestOptions::DEFAULT`] and [`DEFAULT_SCAN_LIMIT`].
    pub const fn new(requester: R, key: KeyBuf<MAX_KEY_LEN>) -> Self {
        let () = Self::BUFFER_OK;
        RemoteKv {
            requester,
            key,
            options: RequestOptions::DEFAULT,
            scan_limit: DEFAULT_SCAN_LIMIT,
            tx: [0; N],
            rx: [0; N],
        }
    }

    /// The same client sending every request with `options` (a timeout, a
    /// target node).
    pub fn with_options(self, options: RequestOptions<'static>) -> Self {
        RemoteKv { options, ..self }
    }

    /// The same client with a scan page limit of `limit` entries (at least
    /// 1).
    pub fn with_scan_limit(self, limit: usize) -> Self {
        let scan_limit = if limit == 0 { 1 } else { limit };
        RemoteKv { scan_limit, ..self }
    }

    /// The scan page limit.
    pub const fn scan_limit(&self) -> usize {
        self.scan_limit
    }

    /// The key requests are sent to.
    pub fn key(&self) -> Key<'_> {
        self.key.as_key()
    }

    /// Borrows the requester, e.g. to reach backend-specific methods.
    pub fn requester(&mut self) -> &mut R {
        &mut self.requester
    }

    /// Returns the requester.
    pub fn into_requester(self) -> R {
        self.requester
    }
}

impl<R: Requester, const N: usize> RemoteKv<R, N> {
    /// Sends `request` and decodes the reply; a [`Response::Failed`] is
    /// returned as the error it carries.
    async fn call(&mut self, request: Request<'_>) -> RemoteResult<Response<'_>, R::Error> {
        let len = request.encode(&mut self.tx).ok_or(KvError::TooLarge)?;
        let body = self.tx.get(..len).ok_or(KvError::TooLarge)?;
        let reply = self
            .requester
            .request_with(self.key.as_key(), body, &mut self.rx, &self.options)
            .await
            .map_err(|e| KvError::Backend(RemoteError::Transport(e)))?;
        match Response::decode(reply) {
            Ok(Response::Failed(f)) => Err(from_failure(f)),
            Ok(resp) => Ok(resp),
            Err(e) => Err(KvError::Backend(RemoteError::Malformed(e))),
        }
    }
}

impl<R: Requester, const N: usize> KvStore for RemoteKv<R, N> {
    type Error = RemoteError<R::Error>;

    const LIMITS: KvLimits = KvLimits {
        max_key: MAX_KEY_LEN,
        max_value: wire::max_value(N),
    };

    async fn get<'b>(
        &mut self,
        key: Key<'_>,
        buf: &'b mut [u8],
    ) -> RemoteResult<Option<Entry<'b>>, R::Error> {
        match self.call(Request::Get { key }).await? {
            Response::Absent => Ok(None),
            Response::Entry { revision, value } => {
                let needed = value.len();
                let out = buf
                    .get_mut(..needed)
                    .ok_or(KvError::BufferTooSmall { needed })?;
                out.copy_from_slice(value);
                Ok(Some(Entry {
                    value: out,
                    revision,
                }))
            }
            _ => Err(unexpected()),
        }
    }

    async fn put(
        &mut self,
        key: Key<'_>,
        value: &[u8],
        expect: Expect,
    ) -> RemoteResult<Revision, R::Error> {
        if key.as_bytes().len() > Self::LIMITS.max_key || value.len() > Self::LIMITS.max_value {
            return Err(KvError::TooLarge);
        }
        match self.call(Request::Put { key, expect, value }).await? {
            Response::Written(r) => Ok(r),
            _ => Err(unexpected()),
        }
    }

    async fn delete(
        &mut self,
        key: Key<'_>,
        expect: Expect,
    ) -> RemoteResult<Option<Revision>, R::Error> {
        match self.call(Request::Delete { key, expect }).await? {
            Response::Written(r) => Ok(Some(r)),
            Response::Absent => Ok(None),
            _ => Err(unexpected()),
        }
    }

    async fn scan<F>(
        &mut self,
        filter: KeyFilter<'_>,
        after: Option<Key<'_>>,
        buf: &mut [u8],
        mut on_entry: F,
    ) -> RemoteResult<Scan, R::Error>
    where
        F: FnMut(Key<'_>, Entry<'_>) -> Flow,
    {
        let limit = self.scan_limit;
        let mut visited = 0usize;
        // The last key of the previous page, once there is one.
        let mut cursor: Option<KeyBuf<MAX_KEY_LEN>> = None;
        // Every page but the last brings at least one entry (checked
        // below), so `limit + 1` requests always suffice.
        for _ in 0..=limit {
            let from = match &cursor {
                Some(c) => Some(c.as_key()),
                None => after,
            };
            let max_entries = u16::try_from(limit.saturating_sub(visited)).unwrap_or(u16::MAX);
            let request = Request::Scan {
                filter,
                after: from,
                max_entries,
            };
            let Response::Page(page) = self.call(request).await? else {
                return Err(unexpected());
            };
            let mut newest = None;
            for (i, entry) in page.entries().enumerate() {
                let behind = from.is_some_and(|f| entry.key.as_bytes() <= f.as_bytes());
                if behind || !filter.matches(entry.key) {
                    return Err(unexpected());
                }
                let needed = entry.value.len();
                let out = buf
                    .get_mut(..needed)
                    .ok_or(KvError::BufferTooSmall { needed })?;
                out.copy_from_slice(entry.value);
                visited += 1;
                newest = Some(KeyBuf::from_key(entry.key).map_err(|_| unexpected())?);
                let last_in_page = i.saturating_add(1) == usize::from(page.count);
                let flow = on_entry(
                    entry.key,
                    Entry {
                        value: out,
                        revision: entry.revision,
                    },
                );
                if flow == Flow::Stop || visited >= limit {
                    return Ok(Scan {
                        visited,
                        complete: last_in_page && page.complete,
                    });
                }
            }
            if page.complete {
                return Ok(Scan {
                    visited,
                    complete: true,
                });
            }
            match newest {
                Some(k) => cursor = Some(k),
                None => return Err(unexpected()),
            }
        }
        Ok(Scan {
            visited,
            complete: false,
        })
    }
}

/// Serves a [`KvStoreSync`] as a request [`Handler`]; see the module docs.
#[derive(Debug, Clone)]
pub struct KvHandler<S: KvStoreSync> {
    store: S,
    code: fn(&S::Error) -> u16,
}

impl<S: KvStoreSync> KvHandler<S> {
    /// Serves `store`, reporting every backend error as code 0.
    pub fn new(store: S) -> Self {
        Self::with_error_code(store, |_| 0)
    }

    /// Serves `store`, reporting a backend error as `code(error)`, which
    /// the client sees as [`RemoteError::Remote`].
    pub fn with_error_code(store: S, code: fn(&S::Error) -> u16) -> Self {
        KvHandler { store, code }
    }

    /// Borrows the store.
    pub fn store(&mut self) -> &mut S {
        &mut self.store
    }

    /// Returns the store.
    pub fn into_store(self) -> S {
        self.store
    }

    fn failure(&self, e: &KvError<S::Error>) -> Failure {
        Failure::from_kv(e, self.code)
    }

    fn get(&mut self, key: Key<'_>, out: &mut [u8]) -> Result<usize, Failure> {
        let slot = out.get_mut(wire::ENTRY_VALUE_AT..).unwrap_or(&mut []);
        let found = match self.store.get(key, slot) {
            Ok(found) => found.map(|e| (e.revision, e.value.len())),
            Err(KvError::BufferTooSmall { needed }) => {
                return Err(reply_too_small(wire::ENTRY_VALUE_AT, needed));
            }
            Err(e) => return Err(self.failure(&e)),
        };
        match found {
            None => encode(Response::Absent, out),
            Some((revision, len)) => {
                let header = wire::encode_entry_header(out, revision)
                    .ok_or(reply_too_small(wire::ENTRY_VALUE_AT, len))?;
                Ok(header.saturating_add(len))
            }
        }
    }

    fn scan(
        &mut self,
        filter: KeyFilter<'_>,
        after: Option<Key<'_>>,
        max_entries: u16,
        out: &mut [u8],
    ) -> Result<usize, Failure> {
        let key_room = S::LIMITS.max_key.min(MAX_KEY_LEN);
        let mut page = PageWriter::new(out).ok_or(Failure::BufferTooSmall {
            needed: u32::try_from(wire::PAGE_HEADER).unwrap_or(u32::MAX),
        })?;
        let mut cursor: Option<KeyBuf<MAX_KEY_LEN>> = None;
        let mut complete = false;
        let entry_room = wire::PAGE_HEADER
            .saturating_add(wire::PAGE_ENTRY_FIXED)
            .saturating_add(key_room);
        // One store scan per entry, each stopping after its first visit, so
        // the value lands in the reply buffer. Bounded by `max_entries`.
        for _ in 0..max_entries {
            let from = match &cursor {
                Some(c) => Some(c.as_key()),
                None => after,
            };
            let mut seen = None;
            let slot = page.value_slot(key_room);
            let scanned = self.store.scan(filter, from, slot, |key, entry| {
                seen = KeyBuf::from_key(key)
                    .ok()
                    .map(|k| (k, entry.revision, entry.value.len()));
                Flow::Stop
            });
            let scan = match scanned {
                Ok(scan) => scan,
                Err(KvError::BufferTooSmall { needed }) if page.count() == 0 => {
                    return Err(reply_too_small(entry_room, needed));
                }
                Err(KvError::BufferTooSmall { .. }) => break,
                Err(e) => return Err(self.failure(&e)),
            };
            let Some((key, revision, len)) = seen else {
                complete = scan.complete;
                break;
            };
            if page.commit(key.as_key(), revision, len, key_room).is_none() {
                if page.count() == 0 {
                    return Err(reply_too_small(entry_room, len));
                }
                break;
            }
            if scan.complete {
                complete = true;
                break;
            }
            cursor = Some(key);
        }
        Ok(page.finish(complete))
    }
}

/// [`Failure::BufferTooSmall`] for a reply of `overhead + value` bytes.
fn reply_too_small(overhead: usize, value: usize) -> Failure {
    Failure::BufferTooSmall {
        needed: u32::try_from(overhead.saturating_add(value)).unwrap_or(u32::MAX),
    }
}

fn encode(resp: Response<'_>, out: &mut [u8]) -> Result<usize, Failure> {
    // Fixed-size replies are at most 11 bytes; a reply buffer that cannot
    // hold one is reported as such.
    resp.encode(out)
        .ok_or(Failure::BufferTooSmall { needed: 11 })
}

impl<S: KvStoreSync> Handler for KvHandler<S> {
    fn handle(
        &mut self,
        req: &TransportRequest<'_>,
        response: &mut [u8],
    ) -> Result<usize, HandlerError> {
        let request = Request::decode(req.body).map_err(|_| HandlerError::InvalidRequest)?;
        let outcome = match request {
            Request::Get { key } => self.get(key, response),
            Request::Put { key, expect, value } => match self.store.put(key, value, expect) {
                Ok(r) => encode(Response::Written(r), response),
                Err(e) => Err(self.failure(&e)),
            },
            Request::Delete { key, expect } => match self.store.delete(key, expect) {
                Ok(Some(r)) => encode(Response::Written(r), response),
                Ok(None) => encode(Response::Absent, response),
                Err(e) => Err(self.failure(&e)),
            },
            Request::Scan {
                filter,
                after,
                max_entries,
            } => self.scan(filter, after, max_entries, response),
        };
        match outcome {
            Ok(n) => Ok(n),
            Err(f) => Response::Failed(f)
                .encode(response)
                .ok_or(HandlerError::BufferTooSmall),
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::kv::table::{KvTable, TableError};
    use futures::executor::block_on;
    use std::string::String;
    use std::vec::Vec;

    type Table = KvTable<64, 32, 64>;
    const STORE: Key<'static> = Key::from_static("kv/test");

    fn k(s: &str) -> Key<'_> {
        Key::new(s).unwrap()
    }

    fn store_key() -> KeyBuf<MAX_KEY_LEN> {
        KeyBuf::from_key(STORE).unwrap()
    }

    /// Runs the handler in the request call, with a `S`-byte reply buffer.
    struct Loopback<const S: usize> {
        handler: KvHandler<Table>,
        requests: usize,
        scratch: [u8; S],
    }

    impl<const S: usize> Loopback<S> {
        fn new() -> Self {
            Loopback {
                handler: KvHandler::with_error_code(Table::new(), |e| match e {
                    TableError::RevisionsExhausted => 7,
                }),
                requests: 0,
                scratch: [0; S],
            }
        }
    }

    impl<const S: usize> Requester for Loopback<S> {
        type Error = HandlerError;

        async fn request_with<'b>(
            &mut self,
            key: Key<'_>,
            request: &[u8],
            response: &'b mut [u8],
            _options: &RequestOptions<'_>,
        ) -> Result<&'b [u8], HandlerError> {
            assert_eq!(key, STORE);
            self.requests += 1;
            let n = self
                .handler
                .handle(&TransportRequest::new(key, request), &mut self.scratch)?;
            let out = response.get_mut(..n).ok_or(HandlerError::BufferTooSmall)?;
            out.copy_from_slice(&self.scratch[..n]);
            Ok(out)
        }
    }

    /// Answers every request with fixed bytes, or fails.
    struct Canned(Result<Vec<u8>, ()>);

    impl Requester for Canned {
        type Error = ();

        async fn request_with<'b>(
            &mut self,
            _key: Key<'_>,
            _request: &[u8],
            response: &'b mut [u8],
            _options: &RequestOptions<'_>,
        ) -> Result<&'b [u8], ()> {
            let bytes = self.0.clone()?;
            let out = response.get_mut(..bytes.len()).ok_or(())?;
            out.copy_from_slice(&bytes);
            Ok(out)
        }
    }

    fn remote<const S: usize>() -> RemoteKv<Loopback<S>, 512> {
        RemoteKv::new(Loopback::new(), store_key())
    }

    fn scan_all<R: Requester, const N: usize>(
        kv: &mut RemoteKv<R, N>,
        filter: KeyFilter<'_>,
        after: Option<Key<'_>>,
    ) -> (Vec<(String, u64)>, Scan) {
        let mut seen = Vec::new();
        let mut buf = [0u8; 64];
        let scan = block_on(kv.scan(filter, after, &mut buf, |key, e| {
            seen.push((String::from(key.as_str()), e.revision.0));
            Flow::Continue
        }))
        .unwrap();
        (seen, scan)
    }

    #[test]
    fn limits_derive_from_the_buffer_size() {
        assert_eq!(
            <RemoteKv<Canned, 512> as KvStore>::LIMITS,
            KvLimits {
                max_key: MAX_KEY_LEN,
                max_value: 512 - wire::PAGE_ENTRY_OVERHEAD
            }
        );
        let kv: RemoteKv<Canned, { wire::MIN_BUFFER }> =
            RemoteKv::new(Canned(Err(())), store_key());
        assert_eq!(kv.key(), STORE);
        assert_eq!(kv.scan_limit(), DEFAULT_SCAN_LIMIT);
        assert_eq!(kv.with_scan_limit(0).scan_limit(), 1);
    }

    #[test]
    fn operations_round_trip_through_the_handler() {
        let mut kv = remote::<512>();
        let mut buf = [0u8; 16];
        assert_eq!(block_on(kv.get(k("a"), &mut buf)), Ok(None));
        assert_eq!(
            block_on(kv.put(k("a"), b"one", Expect::Absent)),
            Ok(Revision(1))
        );
        assert_eq!(
            block_on(kv.get(k("a"), &mut buf)),
            Ok(Some(Entry {
                value: b"one",
                revision: Revision(1)
            }))
        );
        assert_eq!(
            block_on(kv.put(k("a"), b"two", Expect::Absent)),
            Err(KvError::Conflict {
                current: Some(Revision(1))
            })
        );
        assert_eq!(
            block_on(kv.get(k("a"), &mut buf[..2])),
            Err(KvError::BufferTooSmall { needed: 3 })
        );
        assert_eq!(
            block_on(kv.delete(k("a"), Expect::Exact(Revision(1)))),
            Ok(Some(Revision(1)))
        );
        assert_eq!(block_on(kv.delete(k("a"), Expect::Any)), Ok(None));
        assert_eq!(kv.requester().requests, 7);
        // The handler's store saw the same writes.
        assert_eq!(kv.requester().handler.store().next_revision(), Revision(3));
    }

    #[test]
    fn store_errors_are_preserved() {
        let mut kv = remote::<512>();
        // The table's limits (32-byte keys, 64-byte values), not the client's.
        assert_eq!(
            block_on(kv.put(k("a"), &[0; 65], Expect::Any)),
            Err(KvError::TooLarge)
        );
        let long = "k".repeat(33);
        assert_eq!(
            block_on(kv.put(k(&long), b"", Expect::Any)),
            Err(KvError::TooLarge)
        );
        // Over the client's own limit: refused without a request.
        let before = kv.requester().requests;
        assert_eq!(
            block_on(kv.put(k("a"), &[0; 512], Expect::Any)),
            Err(KvError::TooLarge)
        );
        assert_eq!(kv.requester().requests, before);
        for i in 0..64 {
            block_on(kv.put(k(&std::format!("f/{i:02}")), b"", Expect::Any)).unwrap();
        }
        assert_eq!(
            block_on(kv.put(k("new"), b"", Expect::Any)),
            Err(KvError::Full)
        );
        assert_eq!(
            block_on(kv.delete(k("new"), Expect::Exact(Revision(1)))),
            Err(KvError::Conflict { current: None })
        );
    }

    #[test]
    fn backend_codes_and_transport_errors_map_to_backend() {
        // A backend failure arrives as its handler-chosen code.
        let mut bytes = [0u8; 8];
        let n = Response::Failed(Failure::Backend(7))
            .encode(&mut bytes)
            .unwrap();
        let mut canned: RemoteKv<Canned, 512> =
            RemoteKv::new(Canned(Ok(bytes[..n].to_vec())), store_key());
        assert_eq!(
            block_on(canned.put(k("a"), b"", Expect::Any)),
            Err(KvError::Backend(RemoteError::Remote(7)))
        );
        let mut failing: RemoteKv<Canned, 512> = RemoteKv::new(Canned(Err(())), store_key());
        let mut buf = [0u8; 4];
        assert_eq!(
            block_on(failing.get(k("a"), &mut buf)),
            Err(KvError::Backend(RemoteError::Transport(())))
        );
        // A bad body reaches the handler as InvalidRequest.
        let mut h = KvHandler::new(Table::new());
        let mut out = [0u8; 64];
        assert_eq!(
            h.handle(&TransportRequest::new(STORE, b"\x01\x09"), &mut out),
            Err(HandlerError::InvalidRequest)
        );
    }

    #[test]
    fn malformed_and_unexpected_replies_are_errors() {
        let mut buf = [0u8; 16];
        let mut garbage: RemoteKv<Canned, 512> =
            RemoteKv::new(Canned(Ok(std::vec![9, 9, 9])), store_key());
        assert_eq!(
            block_on(garbage.get(k("a"), &mut buf)),
            Err(KvError::Backend(RemoteError::Malformed(
                DecodeError::Version(9)
            )))
        );
        // A written reply to a get.
        let mut bytes = [0u8; 16];
        let n = Response::Written(Revision(1)).encode(&mut bytes).unwrap();
        let mut wrong: RemoteKv<Canned, 512> =
            RemoteKv::new(Canned(Ok(bytes[..n].to_vec())), store_key());
        assert_eq!(
            block_on(wrong.get(k("a"), &mut buf)),
            Err(KvError::Backend(RemoteError::Unexpected))
        );
        // An empty, incomplete page: no progress.
        let mut page = [0u8; 64];
        let n = PageWriter::new(&mut page).unwrap().finish(false);
        let mut stuck: RemoteKv<Canned, 512> =
            RemoteKv::new(Canned(Ok(page[..n].to_vec())), store_key());
        let r = block_on(
            stuck.scan(KeyFilter::Prefix(k("a")), None, &mut buf, |_, _| {
                Flow::Continue
            }),
        );
        assert_eq!(r, Err(KvError::Backend(RemoteError::Unexpected)));
        // A page that repeats the cursor, or leaves the filter.
        for key in ["a/b", "z"] {
            let mut w = PageWriter::new(&mut page).unwrap();
            w.push(k(key), Revision(1), b"").unwrap();
            let n = w.finish(false);
            let mut bad: RemoteKv<Canned, 512> =
                RemoteKv::new(Canned(Ok(page[..n].to_vec())), store_key());
            let r = block_on(bad.scan(
                KeyFilter::Prefix(k("a")),
                Some(k("a/b")),
                &mut buf,
                |_, _| Flow::Continue,
            ));
            assert_eq!(r, Err(KvError::Backend(RemoteError::Unexpected)), "{key}");
        }
    }

    #[test]
    fn scans_page_through_small_replies() {
        // A 128-byte reply holds three of these entries (each slot reserves
        // room for a 32-byte key), so ten entries take four pages.
        let mut kv: RemoteKv<Loopback<128>, 512> = RemoteKv::new(Loopback::new(), store_key());
        for i in 0..10 {
            block_on(kv.put(k(&std::format!("s/{i}")), b"0123456789", Expect::Any)).unwrap();
        }
        block_on(kv.put(k("t"), b"x", Expect::Any)).unwrap();
        let before = kv.requester().requests;
        let (seen, scan) = scan_all(&mut kv, KeyFilter::Prefix(k("s")), None);
        let keys: Vec<_> = seen.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "s/0", "s/1", "s/2", "s/3", "s/4", "s/5", "s/6", "s/7", "s/8", "s/9"
            ]
        );
        assert_eq!(
            scan,
            Scan {
                visited: 10,
                complete: true
            }
        );
        let pages = kv.requester().requests - before;
        assert_eq!(pages, 4);
        // Resuming after a cursor.
        let (seen, _) = scan_all(&mut kv, KeyFilter::Prefix(k("s")), Some(k("s/7")));
        assert_eq!(seen.len(), 2);
    }

    #[test]
    fn scan_limit_and_stop_end_the_scan() {
        let mut kv = remote::<512>().with_scan_limit(3);
        for i in 0..5 {
            block_on(kv.put(k(&std::format!("s/{i}")), b"v", Expect::Any)).unwrap();
        }
        let (seen, scan) = scan_all(&mut kv, KeyFilter::Prefix(k("s")), None);
        assert_eq!(seen.len(), 3);
        assert_eq!(
            scan,
            Scan {
                visited: 3,
                complete: false
            }
        );
        let (seen, scan) = scan_all(&mut kv, KeyFilter::Prefix(k("s")), Some(k("s/2")));
        assert_eq!(seen.len(), 2);
        assert_eq!(
            scan,
            Scan {
                visited: 2,
                complete: true
            }
        );
        let mut buf = [0u8; 4];
        let stop = block_on(kv.scan(KeyFilter::Prefix(k("s")), None, &mut buf, |_, _| Flow::Stop));
        assert_eq!(
            stop,
            Ok(Scan {
                visited: 1,
                complete: false
            })
        );
        let mut tiny = [0u8; 0];
        let small = block_on(kv.scan(KeyFilter::Prefix(k("s")), None, &mut tiny, |_, _| {
            Flow::Continue
        }));
        assert_eq!(small, Err(KvError::BufferTooSmall { needed: 1 }));
    }

    #[test]
    fn handler_reports_a_reply_buffer_too_small() {
        let mut h = KvHandler::new(Table::new());
        let mut out = [0u8; 128];
        let mut req = [0u8; 128];
        let n = Request::Put {
            key: k("a"),
            expect: Expect::Any,
            value: &[1; 40],
        }
        .encode(&mut req)
        .unwrap();
        h.handle(&TransportRequest::new(STORE, &req[..n]), &mut out)
            .unwrap();
        let get = Request::Get { key: k("a") }.encode(&mut req).unwrap();
        let n = h
            .handle(&TransportRequest::new(STORE, &req[..get]), &mut out[..20])
            .unwrap();
        assert_eq!(
            Response::decode(&out[..n]),
            Ok(Response::Failed(Failure::BufferTooSmall { needed: 50 }))
        );
        let scan = Request::Scan {
            filter: KeyFilter::Prefix(k("a")),
            after: None,
            max_entries: 4,
        }
        .encode(&mut req)
        .unwrap();
        let n = h
            .handle(&TransportRequest::new(STORE, &req[..scan]), &mut out[..40])
            .unwrap();
        assert_eq!(
            Response::decode(&out[..n]),
            Ok(Response::Failed(Failure::BufferTooSmall {
                needed: (wire::PAGE_HEADER + wire::PAGE_ENTRY_FIXED + 32 + 40) as u32
            }))
        );
        // And the client sees it as the responder's buffer, not its own.
        let mut buf = [0u8; 64];
        let mut canned: RemoteKv<Canned, 512> =
            RemoteKv::new(Canned(Ok(out[..n].to_vec())), store_key());
        assert_eq!(
            block_on(canned.get(k("a"), &mut buf)),
            Err(KvError::Backend(RemoteError::ResponderBuffer {
                needed: 90
            }))
        );
        // Too small even for a fixed reply.
        let get = Request::Get { key: k("a") }.encode(&mut req).unwrap();
        assert_eq!(
            h.handle(&TransportRequest::new(STORE, &req[..get]), &mut out[..1]),
            Err(HandlerError::BufferTooSmall)
        );
    }
}
