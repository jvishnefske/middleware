//! The `mw-update` binary: signed payload uploads and RP2350 firmware
//! updates through a gateway's `gw/sys` (see `mw_gateway::remote`). The
//! shell only reads the environment, the files and the clock, opens a
//! PKCS#11 token key for `--pkcs11` (PIN from `PKCS11_PIN` or `~/.env`, as
//! mwctl), opens the mesh client and prints; every decision is in
//! `mw_gateway::remote`.
#![forbid(unsafe_code)]

use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use mw_gateway::remote::cli::{HELP, Parsed, parse};
use mw_gateway::remote::mesh::MeshSysCall;
use mw_gateway::remote::{GwEndpoint, RETRY_PAUSE, RequestIds, execute, job};
use mwctl::device::Device;
use mwctl::error::Error;
use mwctl::pkcs11::{self, Location, Pkcs11Signer};
use mwctl::sign::Signer;

/// Opens the token key `label` (as `mwctl --pkcs11`).
fn open_token(label: &str) -> Result<Box<dyn Signer + Send>, Error> {
    let at = Location::from_settings();
    let pin = pkcs11::pin(pkcs11::ENV_PIN)?;
    let signer = Pkcs11Signer::open(&at, label, &pin)?;
    eprintln!("signing with {signer:?} via {}", at.module.display());
    Ok(Box::new(signer))
}

fn main() -> ExitCode {
    let env = std::env::var("MW_STATIC_ENDPOINTS").ok();
    let cli = match parse(std::env::args().skip(1), env.as_deref()) {
        Ok(Parsed::Run(c)) => c,
        Ok(Parsed::Help) => {
            print!("{HELP}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("mw-update: {e}\n\n{HELP}");
            return ExitCode::from(2);
        }
    };
    let now = SystemTime::now();
    let work = match job(&cli.cmd, |p| std::fs::read(p), open_token, now) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("mw-update: {e}");
            return ExitCode::FAILURE;
        }
    };
    let call = match MeshSysCall::new(&cli.endpoint, RETRY_PAUSE) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("mw-update: runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let nanos = now
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let session = (nanos as u64) ^ (u64::from(std::process::id()) << 40);
    let endpoint = GwEndpoint::new(call, RequestIds::new(session), cli.timeout, cli.attempts);
    let mut device = Device::new(endpoint);
    match execute(work, &mut device, || std::thread::sleep(RETRY_PAUSE)) {
        Ok(text) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("mw-update: {e}");
            ExitCode::FAILURE
        }
    }
}
