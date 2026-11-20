//! What the end-to-end tests share: the test key, the WAT fixture, a state
//! directory under the build's temporary directory and an in-process
//! Zenoh pair (copied from `tests/node.rs`, which keeps its own).

#![allow(dead_code)]

use std::path::PathBuf;

use middleware::SenderId;
use middleware::zenoh::{SessionConfig, ZenohTransport};
use mwctl::sign::Seed;

/// The node id the tests give the node.
pub const NODE: SenderId = SenderId([0x5e, 0x11, 0x0d, 0xe0, 0x2a, 0x9c, 0x41, 0x07]);

/// The golden vector's key (`web/test/fixtures/trust-vector.json`, TEST
/// KEY ONLY: seed 00..1f).
pub fn test_seed() -> Seed {
    let mut b = [0u8; 32];
    for (i, x) in b.iter_mut().enumerate() {
        *x = i as u8;
    }
    Seed::new(b)
}

/// The demo payload.
pub fn demo() -> Vec<u8> {
    wat::parse_str(include_str!("../fixtures/pi_demo.wat")).expect("WAT")
}

/// A fresh state directory named `name`.
pub fn state_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("posix-node-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// A fresh loopback endpoint: a port reserved by binding port 0.
pub async fn endpoint() -> String {
    let port = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind").local_addr().expect("addr").port();
    format!("tcp/127.0.0.1:{port}")
}

/// The cluster session (listening on a fresh loopback port), a session
/// for the node connected to it, and the endpoint.
pub async fn zenoh_pair() -> (ZenohTransport, ZenohTransport, String) {
    let endpoint = endpoint().await;
    let cluster = SessionConfig::new().listen(endpoint.clone()).build().expect("config");
    let node = SessionConfig::new().connect(endpoint.clone()).build().expect("config");
    let cluster = ZenohTransport::open(cluster).await.expect("open cluster");
    let node = ZenohTransport::open(node).await.expect("open node");
    (cluster, node, endpoint)
}

/// A Zenoh router session (what `zenohd` is to the nodes) listening on a
/// fresh loopback endpoint, and the endpoint.
pub async fn router() -> (ZenohTransport, String) {
    let endpoint = endpoint().await;
    let mut config = SessionConfig::new().listen(endpoint.clone()).build().expect("config");
    config.insert_json5("mode", "\"router\"").expect("router mode");
    // As zenohd: gossip on, so it brokers between the peers connected to it.
    config.insert_json5("scouting/gossip/enabled", "true").expect("gossip");
    (ZenohTransport::open(config).await.expect("open router"), endpoint)
}

/// A peer session connected to `endpoint`.
pub async fn peer(endpoint: &str) -> ZenohTransport {
    let config = SessionConfig::new().connect(endpoint.to_owned()).build().expect("config");
    ZenohTransport::open(config).await.expect("open peer")
}

/// The session the binary opens for node `node` on `endpoint`
/// (`mw_posix_node::session_config`: a client with the id derived from
/// the node id).
pub async fn node_session(endpoint: &str, node: SenderId) -> ZenohTransport {
    let config = mw_posix_node::session_config(endpoint, node).expect("config");
    ZenohTransport::open(config).await.expect("open node session")
}
