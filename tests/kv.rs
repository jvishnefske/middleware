//! Every key-value conformance case (`tests/kv_conformance.rs`) against
//! the stores built without extra features:
//!
//! * `table::*` — `Blocking<KvTable>`, the RAM reference store.
//! * `remote::*` — `RemoteKv` over the in-process bus (`transport::memory`,
//!   enabled for tests by the crate's self dev-dependency) to a
//!   `KvHandler<KvTable>`, with a 512-byte reply buffer.
//! * `remote_small_pages::*` — the same with a 128-byte reply buffer, so
//!   scans take many pages (three small entries per page).
//!
//! Neither store is durable, so `revisions_survive_reopen` returns at once
//! for both.

mod kv_conformance;

use std::time::Duration;

use kv_conformance::KvFixture;
use middleware::key::{Key, KeyBuf, KeyFilter};
use middleware::kv::remote::{KvHandler, RemoteError, RemoteKv};
use middleware::kv::table::KvTable;
use middleware::kv::{
    Blocking, Entry, Expect, Flow, KvError, KvLimits, KvStore, KvStoreSync, Revision, Scan,
};
use middleware::transport::memory::{MemoryBus, MemoryError, MemoryNode, MemoryResponder};
use middleware::{RequestOptions, Responder, SenderId};

/// 64 entries, 32-byte keys, 64-byte values.
type Table = KvTable<64, 32, 64>;

/// The key the remote store is served at.
const STORE: Key<'static> = Key::from_static("kv/conformance");

struct TableFixture;

impl KvFixture for TableFixture {
    type Store = Blocking<Table>;

    fn fresh(&mut self) -> Blocking<Table> {
        Blocking(Table::new())
    }

    fn capacity(&self) -> Option<usize> {
        Some(64)
    }
}

type Client = RemoteKv<MemoryNode, 512>;

/// A `RemoteKv` and the responder serving it, driven together: each
/// operation runs the client's future and a serve loop concurrently on
/// this task until the client's finishes. `S` is the reply buffer.
struct Served<const S: usize> {
    client: Client,
    responder: MemoryResponder,
    handler: KvHandler<Table>,
    scratch: [u8; S],
}

/// Runs `op` while serving requests with `handler`.
async fn serving<T>(
    responder: &mut MemoryResponder,
    handler: &mut KvHandler<Table>,
    scratch: &mut [u8],
    op: impl Future<Output = T>,
) -> T {
    let serve = async {
        loop {
            responder
                .serve_one(handler, scratch)
                .await
                .expect("serve_one");
        }
    };
    tokio::select! {
        biased;
        r = op => r,
        () = serve => unreachable!("the serve loop never ends"),
    }
}

impl<const S: usize> KvStore for Served<S> {
    type Error = RemoteError<MemoryError>;
    const LIMITS: KvLimits = <Client as KvStore>::LIMITS;

    async fn get<'b>(
        &mut self,
        key: Key<'_>,
        buf: &'b mut [u8],
    ) -> Result<Option<Entry<'b>>, KvError<Self::Error>> {
        let Served {
            client,
            responder,
            handler,
            scratch,
        } = self;
        serving(responder, handler, scratch, client.get(key, buf)).await
    }

    async fn put(
        &mut self,
        key: Key<'_>,
        value: &[u8],
        expect: Expect,
    ) -> Result<Revision, KvError<Self::Error>> {
        let Served {
            client,
            responder,
            handler,
            scratch,
        } = self;
        serving(responder, handler, scratch, client.put(key, value, expect)).await
    }

    async fn delete(
        &mut self,
        key: Key<'_>,
        expect: Expect,
    ) -> Result<Option<Revision>, KvError<Self::Error>> {
        let Served {
            client,
            responder,
            handler,
            scratch,
        } = self;
        serving(responder, handler, scratch, client.delete(key, expect)).await
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
        let Served {
            client,
            responder,
            handler,
            scratch,
        } = self;
        serving(
            responder,
            handler,
            scratch,
            client.scan(filter, after, buf, on_entry),
        )
        .await
    }
}

struct RemoteFixture<const S: usize>;

impl<const S: usize> KvFixture for RemoteFixture<S> {
    type Store = Served<S>;

    fn fresh(&mut self) -> Served<S> {
        let bus = MemoryBus::new();
        let responder = bus
            .node(SenderId([0x5E; 8]))
            .responder(KeyFilter::Exact(STORE))
            .expect("responder");
        // A timeout, so a broken serve loop fails the case instead of
        // hanging it.
        let options = RequestOptions::DEFAULT.with_timeout(Duration::from_secs(5));
        let client = RemoteKv::new(
            bus.node(SenderId([0xC1; 8])),
            KeyBuf::from_key(STORE).expect("store key"),
        )
        .with_options(options);
        Served {
            client,
            responder,
            handler: KvHandler::new(Table::new()),
            scratch: [0; S],
        }
    }

    /// The served table's limits: its `TooLarge` must come back intact.
    fn limits(&self) -> KvLimits {
        <Table as KvStoreSync>::LIMITS
    }

    fn capacity(&self) -> Option<usize> {
        Some(64)
    }
}

kv_cases!(table, TableFixture);
kv_cases!(remote, RemoteFixture::<512>);
kv_cases!(remote_small_pages, RemoteFixture::<128>);

/// A transport failure reaches the caller as `Backend(Transport(..))`:
/// here, no responder serves the store's key.
#[tokio::test]
async fn remote_transport_error_is_backend() {
    let bus = MemoryBus::new();
    let mut kv: Client = RemoteKv::new(
        bus.node(SenderId([1; 8])),
        KeyBuf::from_key(STORE).expect("store key"),
    );
    let mut buf = [0u8; 8];
    let r = kv.get(Key::from_static("a"), &mut buf).await;
    assert!(
        matches!(
            r,
            Err(KvError::Backend(RemoteError::Transport(
                MemoryError::NoResponder
            )))
        ),
        "{r:?}"
    );
}
