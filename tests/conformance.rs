//! Conformance harness for the 0.5 transport and discovery contract.
//!
//! Generic test cases over the traits, written once and run against every
//! backend. A backend plugs in by implementing the fixture trait of each
//! area it supports, then calling the cases from its own test file:
//!
//! * [`ReqRepFixture`] → [`reqrep`]: round trip, `request_with` timeout,
//!   `Unavailable` / `DeadlineExceeded` round trips, caller identity and
//!   deadline in [`RequestMeta`](middleware::RequestMeta).
//! * [`GatherFixture`] → [`reqrep::gather_collects_every_responder`].
//! * [`PubSubFixture`] → [`pubsub`]: the declared
//!   [`Subscriber::DELIVERY`](middleware::Subscriber::DELIVERY) holds under
//!   overload; gaps are reported after a loss.
//! * [`DiscoveryFixture`] → [`discovery`]: announce then browse sees `Up`,
//!   withdraw sees `Down` well within the TTL, a goodbye removes the
//!   [`Directory`](middleware::discovery::Directory) record at once, and
//!   records expire on their TTL.
//!
//! # Plugging in a backend
//!
//! In `tests/<backend>.rs`:
//!
//! ```ignore
//! mod conformance;
//! use conformance::{ReqRepFixture, reqrep};
//!
//! struct Zenoh { /* sessions on 127.0.0.1 */ }
//! impl ReqRepFixture for Zenoh {
//!     type Requester = ZenohTransport;
//!     type Responder = ZenohResponder<'static>;
//!     const PROPAGATES_DEADLINE: bool = false;
//!     async fn connect(&mut self, key: Key<'static>) -> (Self::Requester, Self::Responder) { ... }
//!     fn handler_error(e: &ZenohError) -> Option<HandlerError> { ... }
//!     fn is_timeout(e: &ZenohError) -> bool { ... }
//!     fn expected_sender(&self) -> Option<SenderId> { None }
//!     fn expected_principal(&self) -> Option<&str> { None }
//! }
//!
//! #[tokio::test]
//! async fn zenoh_timeout_is_honoured() {
//!     reqrep::timeout_is_honoured(&mut Zenoh::new().await).await;
//! }
//! ```
//!
//! `mod conformance;` from a test crate root resolves to this file, and its
//! `#[path]` attributes to the cases in `tests/conformance/*.rs`. Run every case for every trait the
//! backend implements; a backend that cannot meet a case documents why in
//! its test file instead of skipping it silently. `tests/memory.rs` is the
//! reference: every case against the in-process backend. Tests must stay on
//! loopback (127.0.0.1) and never scan or multicast onto the network.
//!
//! The cases run requester and responder futures concurrently in one task
//! (`tokio::join!`), so fixtures need neither `Send` nor spawned tasks.
//! Cases panic on failure, like any test.
#![allow(dead_code, async_fn_in_trait)]

// `#[path]` is relative to this file's directory both when this file is a
// test crate root and when another test crate includes it as a module.
#[path = "conformance/discovery.rs"]
pub mod discovery;
#[path = "conformance/pubsub.rs"]
pub mod pubsub;
#[path = "conformance/reqrep.rs"]
pub mod reqrep;

use std::time::Duration;

use middleware::discovery::{Announce, Browse};
use middleware::key::{Key, KeyFilter};
use middleware::{Gather, HandlerError, Publisher, Requester, Responder, SenderId, Subscriber};

/// How long a case waits for something that should happen promptly
/// before failing.
pub const PROMPT: Duration = Duration::from_secs(5);

/// Request/response: a connected requester and responder.
pub trait ReqRepFixture {
    /// The client side.
    type Requester: Requester;
    /// The server side.
    type Responder: Responder;

    /// True if the handler sees the caller's timeout as
    /// `RequestMeta::deadline_ms`.
    const PROPAGATES_DEADLINE: bool;

    /// How far past its timeout a `request_with` may return.
    const TIMEOUT_SLACK: Duration = Duration::from_millis(500);

    /// A requester and a responder serving `key`, ready: a request sent now
    /// reaches the responder's next `serve_one`.
    async fn connect(&mut self, key: Key<'static>) -> (Self::Requester, Self::Responder);

    /// The handler error a requester error reports, if it is one.
    fn handler_error(e: &<Self::Requester as Requester>::Error) -> Option<HandlerError>;

    /// True if a requester error is the backend's timeout.
    fn is_timeout(e: &<Self::Requester as Requester>::Error) -> bool;

    /// The `RequestMeta::sender` the responder's handler should see.
    fn expected_sender(&self) -> Option<SenderId>;

    /// The `RequestMeta::principal` the responder's handler should see.
    fn expected_principal(&self) -> Option<&str>;
}

/// Scatter/gather: one gatherer and `n` responders of one key.
pub trait GatherFixture {
    /// The client side.
    type Gatherer: Gather;
    /// The server side.
    type Responder: Responder;

    /// A gatherer and `n` responders serving `key`, ready.
    async fn connect(
        &mut self,
        key: Key<'static>,
        n: usize,
    ) -> (Self::Gatherer, Vec<Self::Responder>);
}

/// Pub/sub: a publisher and a subscriber that receives it.
pub trait PubSubFixture {
    /// The sending side.
    type Publisher: Publisher;
    /// The receiving side.
    type Subscriber: Subscriber;

    /// True if the subscriber reports `SampleMeta::gap`.
    const REPORTS_GAPS: bool;

    /// A publisher and a subscriber to `filter`, ready: a sample published
    /// now reaches the subscriber (after [`PubSubFixture::settle`]).
    async fn connect(&mut self, filter: KeyFilter<'static>) -> (Self::Publisher, Self::Subscriber);

    /// Samples the subscriber queues before its `DELIVERY.overflow`
    /// applies.
    fn queue_depth(&self) -> usize;

    /// Waits until everything published so far has reached the
    /// subscriber's queue (a no-op in process; a short sleep on a network).
    async fn settle(&mut self) {}
}

/// Discovery: an announcer and a browser that sees it.
pub trait DiscoveryFixture {
    /// The announcing side.
    type Announcer: Announce;
    /// The browsing side.
    type Browser: Browse;

    /// An announcer and a browser, ready.
    async fn connect(&mut self) -> (Self::Announcer, Self::Browser);

    /// Waits until messages sent so far have arrived.
    async fn settle(&mut self) {}
}

/// Awaits `f`, failing the case if it takes longer than [`PROMPT`].
pub async fn prompt<T>(what: &str, f: impl Future<Output = T>) -> T {
    match tokio::time::timeout(PROMPT, f).await {
        Ok(v) => v,
        Err(_) => panic!("{what}: nothing within {PROMPT:?}"),
    }
}
