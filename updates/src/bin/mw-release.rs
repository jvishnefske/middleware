//! `mw-release`: signs a release with the root key and publishes it to the
//! updates channel (crate `mw_updates`, module `publish`), or lists what
//! the channel has.
//!
//! ```text
//! mw-release publish --channel <c> --target <chip|any> --kind fw|payload
//!                    [--version <n | major.minor>] (--key <seed file> | --pkcs11 <label>) <file>
//! mw-release latest --channel <c> --target <chip>
//! ```
//!
//! The route is the updater's (`MW_UPDATES_HOST`, `MW_UPDATES_ADDR` or
//! `MW_UPDATES_ENDPOINT`) with `MW_ZENOH_CREDENTIALS`; the token PIN is
//! read as `mwctl` reads it (`PKCS11_PIN` or `~/.env`).
#![forbid(unsafe_code)]

use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use middleware::loader::release::CertKind;
use mw_updates::client::Client;
use mw_updates::config::{credentials, route};
use mw_updates::publish::{prepare, publish, sign};
use mwctl::fw::{format_version, parse_version};
use mwctl::pkcs11::{self, Location, Pkcs11Signer};
use mwctl::sign::{Seed, Signer};

const HELP: &str = "usage:
  mw-release publish --channel <c> --target <chip|any> --kind fw|payload
                     [--version <n | major.minor>] (--key <seed file> | --pkcs11 <label>) <file>
  mw-release latest --channel <c> --target <chip>
env: MW_UPDATES_HOST | MW_UPDATES_ENDPOINT, MW_UPDATES_ADDR, MW_ZENOH_CREDENTIALS,
     PKCS11_MODULE, PKCS11_TOKEN, PKCS11_PIN
";

#[derive(Default)]
struct Args {
    channel: Option<String>,
    target: Option<String>,
    kind: Option<String>,
    version: Option<String>,
    key: Option<String>,
    pkcs11: Option<String>,
    file: Option<String>,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = args.iter();
    while let Some(x) = it.next() {
        let mut val = || it.next().cloned().ok_or(format!("{x} needs a value"));
        match x.as_str() {
            "--channel" => a.channel = Some(val()?),
            "--target" => a.target = Some(val()?),
            "--kind" => a.kind = Some(val()?),
            "--version" => a.version = Some(val()?),
            "--key" => a.key = Some(val()?),
            "--pkcs11" => a.pkcs11 = Some(val()?),
            f if f.starts_with("--") => return Err(format!("unknown flag {f}")),
            f => a.file = Some(f.to_owned()),
        }
    }
    Ok(a)
}

fn signer(a: &Args) -> Result<Box<dyn Signer>, String> {
    match (&a.key, &a.pkcs11) {
        (Some(path), None) => {
            let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
            Ok(Box::new(
                Seed::from_file_bytes(&bytes).map_err(|e| e.to_string())?,
            ))
        }
        (None, Some(label)) => {
            let at = Location::from_settings();
            let pin = pkcs11::pin(pkcs11::ENV_PIN).map_err(|e| e.to_string())?;
            let s = Pkcs11Signer::open(&at, label, &pin).map_err(|e| e.to_string())?;
            eprintln!("signing with {s:?} via {}", at.module.display());
            Ok(Box::new(s))
        }
        _ => Err("exactly one of --key and --pkcs11".into()),
    }
}

async fn run(args: &[String]) -> Result<(), String> {
    let cmd = args.first().map(String::as_str).ok_or("no command")?;
    let a = parse(&args[1..])?;
    let channel = a.channel.clone().ok_or("--channel is required")?;
    let target = a.target.clone().ok_or("--target is required")?;
    let env = |k: &str| std::env::var(k).ok();
    let route = route(&env).map_err(|e| e.to_string())?;
    let creds = credentials(&env, &|p| std::fs::read_to_string(p)).map_err(|e| e.to_string())?;
    match cmd {
        "latest" => {
            let client = Client::connect(&route, creds.as_ref())
                .await
                .map_err(|e| e.to_string())?;
            for r in client
                .latest(&channel, &target)
                .await
                .map_err(|e| e.to_string())?
            {
                let c = r.cert();
                let v = match c.kind {
                    CertKind::Firmware => format_version(c.version),
                    CertKind::Payload => c.version.0.to_string(),
                };
                println!(
                    "{} {v} {} bytes sha256 {}",
                    c.kind.name(),
                    c.len,
                    mw_updates::hex(&c.sha256)
                );
            }
            Ok(())
        }
        "publish" => {
            let kind = a
                .kind
                .as_deref()
                .and_then(CertKind::from_name)
                .ok_or("--kind fw|payload")?;
            let path = a.file.clone().ok_or("the image file is required")?;
            let file = std::fs::read(&path).map_err(|e| format!("{path}: {e}"))?;
            let version = match &a.version {
                None => None,
                Some(v) => Some(
                    parse_version(v)
                        .or_else(|| v.parse().ok().map(middleware::loader::trust::Version))
                        .ok_or("--version: n or major.minor")?,
                ),
            };
            let prepared = prepare(kind, &file, version, SystemTime::now())?;
            let signer = signer(&a)?;
            let release = sign(kind, &channel, &target, &prepared, &*signer)?;
            let client = Client::connect(&route, creds.as_ref())
                .await
                .map_err(|e| e.to_string())?;
            publish(&client, &release, &prepared.image, Duration::from_secs(20)).await?;
            println!(
                "published {} {} for {channel}/{target} ({} bytes)",
                kind.name(),
                prepared.version.0,
                prepared.image.len()
            );
            Ok(())
        }
        other => Err(format!("unknown command {other}")),
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args[0] == "-h" || args[0] == "--help" {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }
    match run(&args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mw-release: {e}\n\n{HELP}");
            ExitCode::FAILURE
        }
    }
}
