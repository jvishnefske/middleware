//! The `mw-posix-node` binary: see the crate docs. Prints the gateway
//! service's spec or manifests like every `mw-service` binary
//! (`--print-spec`, `--print-manifests`); otherwise reads its
//! configuration (`MW_POSIX_NODE_*`, `MW_GATEWAY_*`, the runner's `MW_*`;
//! `MW_ZENOH_ROUTER` is required), derives the node id, opens the state
//! directory, connects to the Zenoh router, starts the node and runs the
//! `gw/sys` service until SIGTERM/SIGINT (exit 0) or until the device
//! thread or the runner ends (exit 1). On the way out the payload is
//! stopped, the flash synced and the gateway's announcements withdrawn.
#![forbid(unsafe_code)]

use std::process::ExitCode;

use middleware::zenoh::{SessionConfig, ZenohTransport};
use mw_gateway::config::DEFAULT_BAUD;
use mw_gateway::config::ENV_STATE as GW_ENV_STATE;
use mw_gateway::state::ControlStore;
use mw_gateway::{GatewayConfig, SysForward, io, service};
use mw_posix_node::config::{NodeIdSource, gateway_state_dir};
use mw_posix_node::{
    DeviceParts, GatewayParts, PosixNodeConfig, control_floor, node_id, open_state_flash, start,
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
        .and_then(|g| {
            PosixNodeConfig::from_process_env()
                .map(|n| (g, n))
                .map_err(|e| e.to_string())
        });
    let (mut gateway, node) = match configs {
        Ok(c) => c,
        Err(e) => {
            eprintln!("invalid environment: {e}");
            return ExitCode::FAILURE;
        }
    };
    for old in &node.deprecated {
        eprintln!(
            "warning: {old} is the name from before mw-pi-node became mw-posix-node; it still works, but rename it"
        );
    }
    // The gateway's state lives under the node's unless set explicitly.
    gateway.state_dir = gateway_state_dir(std::env::var(GW_ENV_STATE).ok().as_deref(), &node.state);
    if command != Command::Run {
        let result = service(&gateway, SysForward::detached())
            .map_err(|e| e.to_string())
            .and_then(|s| {
                s.execute(&command, &mut std::io::stdout().lock())
                    .map_err(|e| e.to_string())
            });
        return match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        };
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
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
            eprintln!("mw-posix-node: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(gateway: GatewayConfig, node: PosixNodeConfig) -> Result<(), String> {
    let runner_config =
        Config::from_process_env().map_err(|e| format!("invalid environment: {e}"))?;
    let router = runner_config
        .zenoh_router
        .clone()
        .ok_or("MW_ZENOH_ROUTER is required (the Zenoh router the node publishes to)")?;
    let id = match &node.node_id {
        NodeIdSource::Configured(id) => *id,
        NodeIdSource::MachineId(path) => {
            let contents = std::fs::read_to_string(path).map_err(|e| {
                format!(
                    "{}: {e} (set MW_POSIX_NODE_NODE_ID instead)",
                    path.display()
                )
            })?;
            node_id::from_machine_id(&contents).map_err(|e| format!("{}: {e}", path.display()))?
        }
    };
    let flash = open_state_flash(&node.state)
        .map_err(|e| format!("state {}: {e}", node.state.display()))?;
    let local = match &node.local_serial {
        // A gadget port or a PTY ignores the rate; 115200 suits a UART.
        Some(path) => {
            Some(io::open_serial(path, DEFAULT_BAUD).map_err(|e| format!("local serial: {e}"))?)
        }
        None => None,
    };
    // The Zenoh listener (`MW_ZENOH_LISTEN`, e.g. `tcp/0.0.0.0:7447`) is the
    // endpoint the node binds for peers; DNS-SD advertises its port.
    let listen = std::env::var("MW_ZENOH_LISTEN").ok().filter(|s| !s.is_empty());
    let mut zenoh_config = SessionConfig::new().connect(router);
    // MW_ZENOH_TLS_DIR: the node's mutual-TLS files (ca, cert, key) for a
    // `tls/` router (deploy/zenoh, MESH.md). Unset: plain TCP, as before.
    if let Some(dir) = std::env::var_os("MW_ZENOH_TLS_DIR").filter(|d| !d.is_empty()) {
        let dir = std::path::PathBuf::from(dir);
        zenoh_config = zenoh_config.client_tls(middleware::zenoh::ClientTls {
            ca: dir.join("ca"),
            cert: dir.join("cert"),
            key: dir.join("key"),
        });
    }
    if let Some(endpoint) = &listen {
        zenoh_config = zenoh_config.listen(endpoint.clone());
    }
    let zenoh = zenoh_config
        .build()
        .map_err(|e| format!("zenoh config: {e}"))?;
    // The per-node queryables are declared under the node's own id: the id
    // `sys/info` reports and `mwctl --node` names.
    let session = ZenohTransport::open(zenoh)
        .await
        .map_err(|e| format!("zenoh: {e}"))?
        .with_sender(id);
    eprintln!(
        "mw-posix-node: node {} ({}), state {}",
        node_id::hex(id),
        mw_posix_node::service::CHIP,
        node.state.display()
    );
    let store = ControlStore::in_dir(&gateway.state_dir);
    let floor = control_floor(&store, wall_clock_ms());
    if floor.is_none() {
        eprintln!(
            "mw-posix-node: no control counter floor in {} and no synced clock; signed start/stop through gw/sys refused",
            gateway.state_dir.display()
        );
    }
    let driver = node.driver.open();
    // The Linux A/B root (MW_POSIX_NODE_OS_*): the slots and the boot state
    // are opened now, the record of the floor is read from the state
    // directory. A root that cannot be read leaves sys/os/* refused.
    let os = match &node.os {
        Some(c) => {
            let dev = mw_posix_node::os::FileDevice::open(&c.slots, &c.state, &c.cmdline, c.reboot.clone())
                .map_err(|e| format!("os: {e}"))?;
            let updates = mw_posix_node::os::OsUpdates::open(
                Box::new(dev),
                node.state.join(mw_posix_node::os::RECORD_FILE),
                id,
            )
            .map_err(|e| format!("os: {e}"))?;
            eprintln!(
                "mw-posix-node: linux root A/B on {} and {}, boot state {}",
                c.slots[0].display(),
                c.slots[1].display(),
                c.state.display()
            );
            Some(updates)
        }
        None => None,
    };
    eprintln!(
        "mw-posix-node: driver {} ({} control-module points)",
        driver.name(),
        driver.points().len()
    );
    let device = DeviceParts {
        flash,
        facts: mw_posix_node::facts_with_driver(id, driver.as_ref()),
        limits: mw_posix_node::limits_with_fuel(node.memory_bytes, node.fuel_per_call),
        local,
        driver,
        os,
    };
    let parts = GatewayParts {
        bridge: gateway.bridge_with_floor(floor),
        options: gateway.shell_options(),
        store,
    };
    let (mut pi, sys) = start(device, parts, session)
        .await
        .map_err(|e| e.to_string())?;
    let svc = service(&gateway, sys).map_err(|e| e.to_string())?;
    let (stop_runner, runner_stopped) = tokio::sync::oneshot::channel::<()>();
    let mut runner = tokio::spawn(svc.run_with(runner_config, async {
        let _ = runner_stopped.await;
    }));
    // DNS-SD on the local link, for the node's Zenoh listener. A failure
    // here does not stop the node; it is reported.
    let announcement = match listen.as_deref().map(listen_port) {
        Some(Ok(port)) => {
            let host = std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|h| h.trim().to_owned())
                .filter(|h| !h.is_empty())
                .unwrap_or_else(|| "mw-node".to_owned());
            match mw_posix_node::dnssd::start(host, node_id::hex(id), port).await {
                Ok(a) => {
                    eprintln!("mw-posix-node: dns-sd {} on port {port}", mw_posix_node::dnssd::KIND);
                    Some(a)
                }
                Err(e) => {
                    eprintln!("mw-posix-node: dns-sd not started: {e}");
                    None
                }
            }
        }
        Some(Err(e)) => {
            eprintln!("mw-posix-node: dns-sd not started: {e}");
            None
        }
        None => None,
    };
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
    if let Some(a) = announcement {
        a.stop().await;
    }
    let report = pi.stop().await;
    eprintln!("mw-posix-node: {report:?}");
    outcome
}

/// The port of a Zenoh endpoint such as `tcp/0.0.0.0:7447`.
fn listen_port(endpoint: &str) -> Result<u16, String> {
    let (_, addr) = endpoint
        .split_once('/')
        .ok_or_else(|| format!("MW_ZENOH_LISTEN {endpoint:?}: expected proto/host:port"))?;
    addr.rsplit_once(':')
        .and_then(|(_, port)| port.parse().ok())
        .ok_or_else(|| format!("MW_ZENOH_LISTEN {endpoint:?}: no port"))
}
