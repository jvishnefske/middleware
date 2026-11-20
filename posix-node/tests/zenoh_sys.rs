//! The node's own `sys/...` keys over Zenoh (PN21–PN25), end to end in
//! one process: the device thread on a state directory, its in-process
//! gateway, and the third link, the mesh link, whose far end the Zenoh
//! task (`zenoh_sys::run`) or the test drives. The node is claimed over the
//! local link (physical access); `sys/trust/claim` never passes Zenoh.

mod common;

use std::path::Path;
use std::time::Duration;

use common::{NODE, state_dir, test_seed, zenoh_pair};
use middleware::loader::trust::{TrustInfo, TrustState};
use middleware::loader::{DeviceInfo, keys};
use middleware::serial::link::{INBOX_BYTES, LinkError, RECV_BUF_LEN, SerialLink};
use middleware::transport::{HandlerError, RequestOptions, Requester};
use middleware::zenoh::ZenohTransport;
use middleware::{Key, SenderId};
use mw_gateway::GatewayConfig;
use mw_gateway::io::{ChannelIo, Closed, pair};
use mw_gateway::state::ControlStore;
use mw_posix_node::config::gateway_state_dir;
use mw_posix_node::mesh_gate::wrap;
use mw_posix_node::{DeviceParts, GatewayParts, PosixNode, facts, limits, open_state_flash, start};

const TIMEOUT: Duration = Duration::from_secs(15);
const A: SenderId = SenderId([0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8]);

type Link = SerialLink<ChannelIo, INBOX_BYTES>;

/// The node on `state` with a local link and a mesh link; the test's ends
/// of both.
async fn node(state: &Path, session: ZenohTransport) -> (PosixNode, Link, ChannelIo) {
    let (local_test, local_node) = pair();
    let (mesh_test, mesh_node) = pair();
    let mut config = GatewayConfig::from_env(&|_| None).expect("config");
    config.state_dir = gateway_state_dir(None, state);
    let store = ControlStore::in_dir(&config.state_dir);
    let device = DeviceParts {
        flash: open_state_flash(state).expect("state"),
        facts: facts(NODE),
        limits: limits(1024 * 1024),
        local: Some(local_node),
        mesh: Some(mesh_node),
        driver: Box::new(mw_posix_node::driver::NoDriver),
    };
    let gateway = GatewayParts { bridge: config.bridge_with_floor(None), options: config.shell_options(), store };
    let (pi, _sys) = start(device, gateway, session).await.expect("start");
    (pi, SerialLink::with_inbox(local_test), mesh_test)
}

async fn request(link: &mut Link, key: Key<'_>, body: &[u8]) -> Result<Vec<u8>, LinkError<Closed>> {
    let mut out = vec![0u8; 2048];
    let r = tokio::time::timeout(TIMEOUT, link.request_with(key, body, &mut out, &RequestOptions::default()))
        .await
        .expect("in time")?;
    Ok(r.to_vec())
}

fn trust(raw: &[u8]) -> TrustState {
    TrustInfo::decode(raw).expect("trust info").state
}

#[tokio::test(flavor = "multi_thread")]
async fn pn25_the_mesh_link_carries_requests_with_their_requester_and_never_publications() {
    let state = state_dir("mesh-link");
    let (_cluster, session, _) = zenoh_pair().await;
    let (pi, mut local, mesh) = node(&state, session).await;
    let mut mesh: Link = SerialLink::with_inbox(mesh);

    let info = request(&mut mesh, keys::INFO, &wrap(Some(A), &[])).await.expect("info over the mesh link");
    assert_eq!(DeviceInfo::decode(&info).expect("decode").node, Some(NODE));
    let info = request(&mut mesh, keys::INFO, &wrap(None, &[])).await.expect("anonymous info");
    assert_eq!(DeviceInfo::decode(&info).expect("decode").node, Some(NODE));
    // A body without the envelope is malformed.
    assert!(matches!(request(&mut mesh, keys::INFO, &[]).await, Err(LinkError::Handler(HandlerError::InvalidRequest))));
    assert!(matches!(
        request(&mut mesh, keys::INFO, &[7, 1, 2]).await,
        Err(LinkError::Handler(HandlerError::InvalidRequest))
    ));
    // The mesh link has no physical-access authority: no claim.
    let key = test_seed().public_key();
    assert!(matches!(
        request(&mut mesh, keys::TRUST_CLAIM, &wrap(Some(A), key.as_bytes())).await,
        Err(LinkError::Handler(HandlerError::Denied))
    ));
    assert_eq!(trust(&request(&mut local, keys::TRUST_INFO, &[]).await.expect("trust")), TrustState::Unclaimed);
    // The local link claims; its sys/log line goes to the gateway and the
    // local link, never to the mesh link.
    request(&mut local, keys::TRUST_CLAIM, key.as_bytes()).await.expect("claim");
    let mut buf = vec![0u8; RECV_BUF_LEN];
    let heard = tokio::time::timeout(Duration::from_millis(500), mesh.next_frame(&mut buf)).await;
    assert!(heard.is_err(), "the mesh link was sent a frame: {:?}", heard.map(|f| f.map(|_| ())));
    assert!(matches!(
        trust(&request(&mut mesh, keys::TRUST_INFO, &wrap(Some(A), &[])).await.expect("trust")),
        TrustState::Claimed { .. }
    ));

    let report = pi.stop().await;
    assert!(report.device.synced);
    let _ = std::fs::remove_dir_all(&state);
}
