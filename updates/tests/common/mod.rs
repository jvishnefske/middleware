//! A posix-node in the test process and a blocking byte stream over its
//! local serial link, for `mwctl`'s link (shared by the tests).

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use middleware::SenderId;
use middleware::zenoh::{SessionConfig, ZenohTransport};
use mw_gateway::GatewayConfig;
use mw_gateway::io::ChannelIo;
use mw_gateway::state::ControlStore;
use mw_posix_node::{
    DeviceParts, GatewayParts, PosixNode, control_floor, facts, limits, open_state_flash, start,
    wall_clock_ms,
};
use mwctl::link::Transport;
use tokio::sync::mpsc;

/// The test's end of the link: to the node, from the node, and the bytes
/// of the last chunk not yet read.
type Ends = (mpsc::Sender<Vec<u8>>, mpsc::Receiver<Vec<u8>>, Vec<u8>);

/// A byte stream over the node's local link. It polls the channels, so
/// the blocking thread never calls into the runtime.
#[derive(Clone)]
pub struct Chan(Arc<Mutex<Ends>>);

impl Transport for Chan {
    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let mut msg = bytes.to_vec();
        loop {
            match self.0.lock().unwrap().0.try_send(msg) {
                Ok(()) => return Ok(()),
                Err(mpsc::error::TrySendError::Full(m)) => msg = m,
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    return Err(std::io::ErrorKind::BrokenPipe.into());
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn read(&mut self, buf: &mut [u8], timeout: Duration) -> std::io::Result<usize> {
        let until = Instant::now() + timeout;
        loop {
            {
                let mut g = self.0.lock().unwrap();
                if g.2.is_empty()
                    && let Ok(m) = g.1.try_recv()
                {
                    g.2 = m;
                }
                if !g.2.is_empty() {
                    let n = g.2.len().min(buf.len());
                    buf[..n].copy_from_slice(&g.2[..n]);
                    g.2.drain(..n);
                    return Ok(n);
                }
            }
            if Instant::now() >= until {
                return Ok(0);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

/// Starts a posix-node with node id `node` and its state in `state`; its
/// mesh session is isolated (the updates channel is not its business).
pub async fn node(state: &Path, node: SenderId) -> (PosixNode, Chan) {
    let (to_node, node_rx) = mpsc::channel(64);
    let (node_tx, from_node) = mpsc::channel(64);
    let mut config = GatewayConfig::from_env(&|_| None).expect("gateway config");
    config.state_dir = state.join("gateway");
    let store = ControlStore::in_dir(&config.state_dir);
    let floor = control_floor(&store, wall_clock_ms());
    let device = DeviceParts {
        flash: open_state_flash(state).expect("state"),
        facts: facts(node),
        limits: limits(1024 * 1024),
        local: Some(ChannelIo::new(node_rx, node_tx)),
        driver: Box::new(mw_posix_node::driver::NoDriver),
    };
    let gateway = GatewayParts {
        bridge: config.bridge_with_floor(floor),
        options: config.shell_options(),
        store,
    };
    let mesh = ZenohTransport::open(SessionConfig::new().build().unwrap())
        .await
        .unwrap();
    let (pi, _sys) = start(device, gateway, mesh).await.expect("start node");
    (
        pi,
        Chan(Arc::new(Mutex::new((to_node, from_node, Vec::new())))),
    )
}

/// The posix-node fixture payload.
pub fn demo() -> Vec<u8> {
    wat::parse_str(include_str!(
        "../../../posix-node/tests/fixtures/pi_demo.wat"
    ))
    .expect("WAT")
}
