//! Every conformance case against the in-process backend
//! (`middleware::transport::memory`, feature `memory`, enabled for tests by
//! the crate's self dev-dependency). The reference for plugging in other
//! backends; see `tests/conformance.rs`.

mod conformance;

use conformance::{
    DiscoveryFixture, GatherFixture, PubSubFixture, ReqRepFixture, discovery, pubsub, reqrep,
};
use middleware::key::{Key, KeyFilter};
use middleware::transport::memory::{
    MemoryBus, MemoryDiscovery, MemoryError, MemoryNode, MemoryResponder, MemorySubscriber,
};
use middleware::{HandlerError, SenderId};

const CLIENT: SenderId = SenderId([0xC1; 8]);
const PRINCIPAL: &str = "spiffe://cluster.local/ns/test/sa/client";

struct Memory {
    bus: MemoryBus,
}

impl Memory {
    fn new() -> Self {
        Memory {
            bus: MemoryBus::with_queue_depth(4),
        }
    }
}

impl ReqRepFixture for Memory {
    type Requester = MemoryNode;
    type Responder = MemoryResponder;
    const PROPAGATES_DEADLINE: bool = true;

    async fn connect(&mut self, key: Key<'static>) -> (MemoryNode, MemoryResponder) {
        let client = self.bus.node_with_principal(CLIENT, PRINCIPAL);
        let server = self
            .bus
            .node(SenderId([0x5E; 8]))
            .responder(KeyFilter::Exact(key))
            .unwrap();
        (client, server)
    }

    fn handler_error(e: &MemoryError) -> Option<HandlerError> {
        match e {
            MemoryError::Handler(h) => Some(*h),
            _ => None,
        }
    }

    fn is_timeout(e: &MemoryError) -> bool {
        *e == MemoryError::Timeout
    }

    fn expected_sender(&self) -> Option<SenderId> {
        Some(CLIENT)
    }

    fn expected_principal(&self) -> Option<&str> {
        Some(PRINCIPAL)
    }
}

impl GatherFixture for Memory {
    type Gatherer = MemoryNode;
    type Responder = MemoryResponder;

    async fn connect(&mut self, key: Key<'static>, n: usize) -> (MemoryNode, Vec<MemoryResponder>) {
        let responders = (0..n)
            .map(|i| {
                self.bus
                    .node(SenderId([i as u8 + 1; 8]))
                    .responder(KeyFilter::Exact(key))
                    .unwrap()
            })
            .collect();
        (self.bus.node(CLIENT), responders)
    }
}

impl PubSubFixture for Memory {
    type Publisher = MemoryNode;
    type Subscriber = MemorySubscriber;
    const REPORTS_GAPS: bool = true;

    async fn connect(&mut self, filter: KeyFilter<'static>) -> (MemoryNode, MemorySubscriber) {
        (
            self.bus.node(CLIENT),
            self.bus.node(SenderId([2; 8])).subscriber(filter).unwrap(),
        )
    }

    fn queue_depth(&self) -> usize {
        self.bus.queue_depth()
    }
}

impl DiscoveryFixture for Memory {
    type Announcer = MemoryDiscovery;
    type Browser = MemoryDiscovery;

    async fn connect(&mut self) -> (MemoryDiscovery, MemoryDiscovery) {
        let a = self.bus.node(SenderId([0xA0; 8])).discovery().unwrap();
        let b = self.bus.node(SenderId([0xB0; 8])).discovery().unwrap();
        (a, b)
    }
}

#[tokio::test]
async fn k_round_trip() {
    reqrep::round_trip(&mut Memory::new()).await;
}

#[tokio::test]
async fn k_timeout_is_honoured() {
    reqrep::timeout_is_honoured(&mut Memory::new()).await;
}

#[tokio::test]
async fn k_handler_errors_round_trip() {
    reqrep::handler_errors_round_trip(&mut Memory::new()).await;
}

#[tokio::test]
async fn k_handler_sees_caller_identity() {
    reqrep::handler_sees_caller_identity(&mut Memory::new()).await;
}

#[tokio::test]
async fn k_gather_collects_every_responder() {
    reqrep::gather_collects_every_responder(&mut Memory::new(), 3).await;
}

#[tokio::test]
async fn k_overload_matches_declared_delivery() {
    pubsub::overload_matches_declared_delivery(&mut Memory::new()).await;
}

#[tokio::test]
async fn k_gap_is_reported() {
    pubsub::gap_is_reported(&mut Memory::new()).await;
}

#[tokio::test]
async fn l_announce_then_withdraw() {
    discovery::announce_then_withdraw(&mut Memory::new()).await;
}

#[tokio::test]
async fn l_goodbye_removes_record_at_once() {
    discovery::goodbye_removes_record_at_once(&mut Memory::new()).await;
}

#[test]
fn l_directory_expiry() {
    discovery::directory_expiry();
}
