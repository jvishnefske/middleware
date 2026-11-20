//! `mw-updates`: the updates channel's server (crate `mw_updates`, module
//! `server`), and the config of its zenohd.
//!
//! ```text
//! mw-updates [serve]
//! mw-updates router-config --server <user> --publishers <u,..> --updaters <u,..> [--dictionary <path>]
//! ```
#![forbid(unsafe_code)]

use std::process::ExitCode;
use std::sync::Arc;

use mw_updates::client::session_config;
use mw_updates::config::{ServerConfig, credentials};
use mw_updates::router::{Users, render};
use mw_updates::server::serve;
use mw_updates::store::Store;

const HELP: &str = "usage:
  mw-updates [serve]
      env: MW_UPDATES_DATA (/data), MW_UPDATES_ROOT_KEY, MW_ZENOH_ROUTER
           (tcp/127.0.0.1:7447), MW_ZENOH_CREDENTIALS
  mw-updates router-config --server <user> --publishers <u,..> --updaters <u,..>
                           [--dictionary /etc/zenoh/auth/dictionary]
";

fn list(s: &str) -> Vec<String> {
    s.split(',')
        .map(|x| x.trim().to_owned())
        .filter(|x| !x.is_empty())
        .collect()
}

fn router_config(args: &[String]) -> Result<String, String> {
    let mut users = Users {
        server: vec![],
        publishers: vec![],
        updaters: vec![],
    };
    let mut dictionary = "/etc/zenoh/auth/dictionary".to_owned();
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let value = it.next().ok_or(format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--server" => users.server = list(value),
            "--publishers" => users.publishers = list(value),
            "--updaters" => users.updaters = list(value),
            "--dictionary" => dictionary = value.clone(),
            _ => return Err(format!("unknown flag {flag}")),
        }
    }
    if users.server.is_empty() {
        return Err("--server is required".into());
    }
    Ok(render(&users, &dictionary))
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("router-config") => {
            return match router_config(&args[1..]) {
                Ok(c) => {
                    print!("{c}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("mw-updates: {e}\n\n{HELP}");
                    ExitCode::from(2)
                }
            };
        }
        None | Some("serve") => {}
        Some("-h" | "--help") => {
            print!("{HELP}");
            return ExitCode::SUCCESS;
        }
        Some(other) => {
            eprintln!("mw-updates: unknown command {other}\n\n{HELP}");
            return ExitCode::from(2);
        }
    }
    let env = |k: &str| std::env::var(k).ok();
    let (cfg, creds) = match ServerConfig::from_env(&env)
        .and_then(|c| Ok((c, credentials(&env, &|p| std::fs::read_to_string(p))?)))
    {
        Ok(x) => x,
        Err(e) => {
            eprintln!("mw-updates: {e}");
            return ExitCode::from(2);
        }
    };
    let store = match Store::open(&cfg.data, cfg.root) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("mw-updates: {}: {e}", cfg.data.display());
            return ExitCode::FAILURE;
        }
    };
    let session = match session_config(&cfg.router, creds.as_ref()) {
        Ok(c) => zenoh::open(c).await,
        Err(e) => {
            eprintln!("mw-updates: {e}");
            return ExitCode::FAILURE;
        }
    };
    let session = match session {
        Ok(s) => s,
        Err(e) => {
            eprintln!("mw-updates: zenoh {}: {e}", cfg.router);
            return ExitCode::FAILURE;
        }
    };
    eprintln!(
        "mw-updates: store {}, router {}",
        cfg.data.display(),
        cfg.router
    );
    match serve(session, store).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mw-updates: {e}");
            ExitCode::FAILURE
        }
    }
}
