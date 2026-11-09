//! The `mw-gateway` binary: see the crate docs. Prints its spec or
//! manifests like every `mw-service` binary (`--print-spec`,
//! `--print-manifests`); otherwise opens the serial port, connects to the
//! Zenoh router and runs until SIGTERM/SIGINT, the device stream closing,
//! or the runner failing (exit 1).
#![forbid(unsafe_code)]

use std::process::ExitCode;

use middleware::zenoh::{SessionConfig, ZenohTransport};
use mw_gateway::{GatewayConfig, SysForward, io, service, start};
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
    let config = match GatewayConfig::from_process_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("invalid environment: {e}");
            return ExitCode::FAILURE;
        }
    };
    if command != Command::Run {
        let result = service(&config, SysForward::detached())
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
    match runtime.block_on(run(config)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(config: GatewayConfig) -> Result<(), String> {
    let runner_config =
        Config::from_process_env().map_err(|e| format!("invalid environment: {e}"))?;
    let router = runner_config
        .zenoh_router
        .clone()
        .ok_or("MW_ZENOH_ROUTER is required (the Zenoh router the gateway publishes to)")?;
    let zenoh = SessionConfig::new()
        .connect(router)
        .build()
        .map_err(|e| format!("zenoh config: {e}"))?;
    let session = ZenohTransport::open(zenoh)
        .await
        .map_err(|e| format!("zenoh: {e}"))?;
    let stream =
        io::open_serial(&config.serial, config.baud).map_err(|e| format!("serial: {e}"))?;
    let (mut gateway, sys) = start(stream, session, config.bridge())
        .await
        .map_err(|e| format!("zenoh: {e}"))?;
    let svc = service(&config, sys).map_err(|e| e.to_string())?;
    let (stop_runner, runner_stopped) = tokio::sync::oneshot::channel::<()>();
    let mut runner = tokio::spawn(svc.run_with(runner_config, async {
        let _ = runner_stopped.await;
    }));
    let outcome = tokio::select! {
        _ = terminate_signal() => Ok(()),
        _ = gateway.link_closed() => Err("the device stream closed".to_owned()),
        r = &mut runner => Err(match r {
            Ok(Ok(())) => "the runner stopped".to_owned(),
            Ok(Err(e)) => format!("runner: {e}"),
            Err(e) => format!("runner: {e}"),
        }),
    };
    let _ = stop_runner.send(());
    let report = gateway.stop().await;
    eprintln!("mw-gateway: {report:?}");
    outcome
}
