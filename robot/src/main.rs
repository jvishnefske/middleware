//! The `mw-robot` binary: `--print-spec` / `--print-manifests` like every
//! `mw-service` binary; otherwise runs the robot loop until SIGTERM or
//! SIGINT (exit 0), or fails at start (exit 1).
#![forbid(unsafe_code)]

use std::process::ExitCode;

use mw_robot::{Robot, RobotConfig};
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
    let robot = match RobotConfig::from_process_env()
        .map_err(|e| format!("invalid environment: {e}"))
        .and_then(|c| Robot::new(c).map_err(|e| e.to_string()))
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let result = match command {
        Command::Run => run(robot),
        print => robot
            .service()
            .execute(&print, &mut std::io::stdout().lock())
            .map_err(|e| e.to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn run(robot: Robot) -> Result<(), String> {
    let config = Config::from_process_env().map_err(|e| format!("invalid environment: {e}"))?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio: {e}"))?;
    let report = runtime
        .block_on(robot.run(config, terminate_signal()))
        .map_err(|e| e.to_string())?;
    eprintln!("mw-robot: {report:?}");
    Ok(())
}
