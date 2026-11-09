//! Renders the Zenoh router deployment from service specs.
//!
//! ```text
//! zenoh_router_manifests [--options ROUTER.yaml] [--out DIR] SPEC.yaml...
//! zenoh_router_manifests [--options ROUTER.yaml] --users SPEC.yaml...
//! zenoh_router_manifests [--options ROUTER.yaml] --credentials DIR SPEC.yaml...
//! ```
//!
//! * Each `SPEC.yaml` is a service's `--print-spec` output; `ROUTER.yaml`
//!   is a `RouterOptions` (every field optional, lab defaults).
//! * Default: the router's manifests (config included, no secrets) on
//!   stdout, ready for `kubectl apply -f -`; with `--out DIR`, written to
//!   `DIR/config.json5` and `DIR/zenohd.yaml` instead (this is how
//!   `deploy/zenoh/` is generated).
//! * `--users`: the Zenoh usernames that need credentials, one per line,
//!   so a caller can generate a password for each.
//! * `--credentials DIR`: reads `DIR/<user>` (the credentials file format,
//!   `<user>\n<password>\n`) for every user and prints the Secrets (the
//!   router's dictionary and each service's credentials) on stdout. Never
//!   commit that output.

use std::path::PathBuf;
use std::process::ExitCode;

use mw_service::ServiceSpec;
use mw_service::zenoh_router::credentials::Credentials;
use mw_service::zenoh_router::{RouterOptions, plan};

/// What to print.
enum Mode {
    Manifests { out: Option<PathBuf> },
    Users,
    Secrets { dir: PathBuf },
}

struct Args {
    options: Option<PathBuf>,
    mode: Mode,
    specs: Vec<PathBuf>,
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut options = None;
    let mut mode = Mode::Manifests { out: None };
    let mut specs = Vec::new();
    while let Some(a) = args.next() {
        let mut value = |flag: &str| args.next().ok_or(format!("{flag} needs a value"));
        match a.as_str() {
            "--options" => options = Some(PathBuf::from(value("--options")?)),
            "--out" => {
                mode = Mode::Manifests {
                    out: Some(PathBuf::from(value("--out")?)),
                }
            }
            "--users" => mode = Mode::Users,
            "--credentials" => {
                mode = Mode::Secrets {
                    dir: PathBuf::from(value("--credentials")?),
                }
            }
            flag if flag.starts_with("--") => return Err(format!("unknown flag {flag}")),
            spec => specs.push(PathBuf::from(spec)),
        }
    }
    Ok(Args {
        options,
        mode,
        specs,
    })
}

fn read(path: &PathBuf) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}

fn run() -> Result<(), String> {
    let args = parse(std::env::args().skip(1))?;
    let options: RouterOptions = match &args.options {
        Some(p) => {
            serde_norway::from_str(&read(p)?).map_err(|e| format!("{}: {e}", p.display()))?
        }
        None => RouterOptions::default(),
    };
    let specs = args
        .specs
        .iter()
        .map(|p| {
            serde_norway::from_str::<ServiceSpec>(&read(p)?)
                .map_err(|e| format!("{}: {e}", p.display()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let plan = plan(&specs, &options).map_err(|e| e.to_string())?;
    match args.mode {
        Mode::Manifests { out: None } => print!("{}", plan.k8s_manifests_yaml),
        Mode::Manifests { out: Some(dir) } => {
            let write = |name: &str, text: &str| {
                std::fs::write(dir.join(name), text).map_err(|e| format!("{name}: {e}"))
            };
            write("config.json5", &plan.config_json5)?;
            write("zenohd.yaml", &plan.k8s_manifests_yaml)?;
        }
        Mode::Users => {
            for p in &plan.principals {
                println!("{}", p.user);
            }
        }
        Mode::Secrets { dir } => {
            let credentials = plan
                .principals
                .iter()
                .map(|p| {
                    Credentials::from_file(&dir.join(&p.user))
                        .map_err(|e| format!("{}: {e}", p.user))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let (_, secrets) = plan
                .secrets(&options, &credentials)
                .map_err(|e| e.to_string())?;
            print!("{secrets}");
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("zenoh_router_manifests: {e}");
            ExitCode::FAILURE
        }
    }
}
