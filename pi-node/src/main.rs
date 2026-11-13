//! The `mw-pi-node` binary: see the crate docs. Prints the gateway
//! service's spec or manifests like every `mw-service` binary
//! (`--print-spec`, `--print-manifests`); otherwise reads its
//! configuration (`MW_PI_NODE_*`, `MW_GATEWAY_*`, the runner's `MW_*`;
//! `MW_ZENOH_ROUTER` is required), derives the node id, opens the state
//! directory, connects to the Zenoh router, starts the node and runs the
//! `gw/sys` service until SIGTERM/SIGINT (exit 0) or until the device
//! thread or the runner ends (exit 1). On the way out the payload is
//! stopped, the flash synced and the gateway's announcements withdrawn.
#![forbid(unsafe_code)]

use std::process::ExitCode;

use middleware::zenoh::{SessionConfig, ZenohTransport};
use mw_gateway::config::DEFAULT_BAUD;
use mw_gateway::{GatewayConfig, SysForward, io, service};
use mw_gateway::config::ENV_STATE as GW_ENV_STATE;
use mw_gateway::state::ControlStore;
use mw_pi_node::config::{NodeIdSource, gateway_state_dir};
use mw_pi_node::{
    DeviceParts, GatewayParts, PiNodeConfig, control_floor, facts, limits, node_id, open_state_flash, start,
    wall_clock_ms,
};
use mw_service::run::{USAGE, terminate_signal};
use mw_service::{Command, Config};

fn main() -> ExitCode {
    let command = match Command::parse(std::env::args().skip(1)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let configs = GatewayConfig::from_process_env()
        .map_err(|e| e.to_string())
        .and_then(|g| PiNodeConfig::from_process_env().map(|n| (g, n)).map_err(|e| e.to_string()));
    let (mut gateway, node) = match configs {
        Ok(c) => c,
        Err(e) => {
            eprintln!("invalid environment: {e}");
            return ExitCode::FAILURE;
        }
    };
    // The gateway's state lives under the node's unless set explicitly.
    gateway.state_dir = gateway_state_dir(std::env::var(GW_ENV_STATE).ok().as_deref(), &node.state);
    if command != Command::Run {
        let result = service(&gateway, SysForward::detached())
            .map_err(|e| e.to_string())
            .and_then(|s| s.execute(&command, &mut std::io::stdout().lock()).map_err(|e| e.to_string()));
        return match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        };
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("tokio: {e}");
            return ExitCode::FAILURE;
        }
    };
    // Off the main thread: a static musl binary cannot measure the main
    // thread's stack, and the Zenoh configuration parse refuses to run
    // there (mw_service::stack, MS57).
    let result = mw_service::block_on_thread(&runtime, run(gateway, node))
        .unwrap_or_else(|e| Err(format!("main thread: {e}")));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mw-pi-node: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(gateway: GatewayConfig, node: PiNodeConfig) -> Result<(), String> {
    let runner_config = Config::from_process_env().map_err(|e| format!("invalid environment: {e}"))?;
    let router = runner_config
        .zenoh_router
        .clone()
        .ok_or("MW_ZENOH_ROUTER is required (the Zenoh router the node publishes to)")?;
    let id = match &node.node_id {
        NodeIdSource::Configured(id) => *id,
        NodeIdSource::MachineId(path) => {
            let contents = std::fs::read_to_string(path)
                .map_err(|e| format!("{}: {e} (set MW_PI_NODE_NODE_ID instead)", path.display()))?;
            node_id::from_machine_id(&contents).map_err(|e| format!("{}: {e}", path.display()))?
        }
    };
    let flash = open_state_flash(&node.state).map_err(|e| format!("state {}: {e}", node.state.display()))?;
    let local = match &node.local_serial {
        // A gadget port or a PTY ignores the rate; 115200 suits a UART.
        Some(path) => Some(io::open_serial(path, DEFAULT_BAUD).map_err(|e| format!("local serial: {e}"))?),
        None => None,
    };
    let zenoh = SessionConfig::new().connect(router).build().map_err(|e| format!("zenoh config: {e}"))?;
    let session = ZenohTransport::open(zenoh).await.map_err(|e| format!("zenoh: {e}"))?;
    eprintln!(
        "mw-pi-node: node {} ({}), state {}",
        node_id::hex(id),
        mw_pi_node::service::CHIP,
        node.state.display()
    );
    let store = ControlStore::in_dir(&gateway.state_dir);
    let floor = control_floor(&store, wall_clock_ms());
    if floor.is_none() {
        eprintln!(
            "mw-pi-node: no control counter floor in {} and no synced clock; signed start/stop through gw/sys refused",
            gateway.state_dir.display()
        );
    }
    let device = DeviceParts { flash, facts: facts(id), limits: limits(node.memory_bytes), local };
    let parts = GatewayParts { bridge: gateway.bridge_with_floor(floor), options: gateway.shell_options(), store };
    let (mut pi, sys) = start(device, parts, session).await.map_err(|e| e.to_string())?;
    let svc = service(&gateway, sys).map_err(|e| e.to_string())?;
    let (stop_runner, runner_stopped) = tokio::sync::oneshot::channel::<()>();
    let mut runner = tokio::spawn(svc.run_with(runner_config, async {
        let _ = runner_stopped.await;
    }));
    let outcome = tokio::select! {
        _ = terminate_signal() => Ok(()),
        _ = pi.device_ended() => Err("the device thread ended".to_owned()),
        r = &mut runner => Err(match r {
            Ok(Ok(())) => "the runner stopped".to_owned(),
            Ok(Err(e)) => format!("runner: {e}"),
            Err(e) => format!("runner: {e}"),
        }),
    };
    let _ = stop_runner.send(());
    let report = pi.stop().await;
    eprintln!("mw-pi-node: {report:?}");
    outcome
}
