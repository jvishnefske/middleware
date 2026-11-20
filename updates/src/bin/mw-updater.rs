//! `mw-updater`: pulls releases from the updates channel into the serial
//! devices it is given (crate `mw_updates`, module `run`). Settings are
//! environment variables (`mw_updates::config`); a pass runs at start,
//! every `MW_UPDATES_POLL` seconds, and whenever a release is published on
//! the channel.
#![forbid(unsafe_code)]

use std::process::ExitCode;
use std::time::Duration;

use mw_updates::client::Client;
use mw_updates::config::{UpdaterConfig, credentials};
use mw_updates::run::{FW_COME_BACK, pass};
use mw_updates::updater::Opener;
use mwctl::device::Device;
use mwctl::error::Error;
use mwctl::link::{Link, Timeouts};
use mwctl::port::SerialTransport;

/// A serial port, opened afresh for every use (it may come and go with
/// the board's resets).
struct Serial {
    path: String,
    baud: u32,
}

impl Opener for Serial {
    type Link = Link<SerialTransport>;

    fn open(&mut self) -> Result<Device<Self::Link>, Error> {
        let t = SerialTransport::open(&self.path, self.baud)?;
        Ok(Device::new(Link::new(t, Timeouts::default())))
    }
}

fn log(line: &str) {
    eprintln!("mw-updater: {line}");
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> ExitCode {
    let env = |k: &str| std::env::var(k).ok();
    let hostname = std::fs::read_to_string("/etc/hostname").unwrap_or_default();
    let cfg = match UpdaterConfig::from_env(&env, &hostname) {
        Ok(c) => c,
        Err(e) => {
            log(&e.to_string());
            return ExitCode::from(2);
        }
    };
    let creds = match credentials(&env, &|p| std::fs::read_to_string(p)) {
        Ok(c) => c,
        Err(e) => {
            log(&e.to_string());
            return ExitCode::from(2);
        }
    };
    let mut devices: Vec<(String, Serial)> = cfg
        .devices
        .iter()
        .map(|p| {
            (
                p.clone(),
                Serial {
                    path: p.clone(),
                    baud: cfg.baud,
                },
            )
        })
        .collect();
    log(&format!(
        "channel {}, {} device(s), every {} s",
        cfg.channel,
        devices.len(),
        cfg.poll.as_secs()
    ));
    loop {
        let client = match Client::connect(&cfg.route, creds.as_ref()).await {
            Ok(c) => c,
            Err(e) => {
                log(&format!("connect: {e}; retrying in 30 s"));
                tokio::time::sleep(Duration::from_secs(30)).await;
                continue;
            }
        };
        let watch = match client.watch(&cfg.channel).await {
            Ok(w) => w,
            Err(e) => {
                log(&format!("watch: {e}; retrying in 30 s"));
                tokio::time::sleep(Duration::from_secs(30)).await;
                continue;
            }
        };
        loop {
            for r in pass(
                &client,
                &cfg.channel,
                &cfg.root,
                &mut devices,
                &cfg.node,
                FW_COME_BACK,
            )
            .await
            {
                log(&format!(
                    "{} {} {}: {}",
                    r.device,
                    r.kind,
                    r.version.map(|v| v.to_string()).unwrap_or_default(),
                    r.result
                ));
            }
            tokio::select! {
                _ = tokio::time::sleep(cfg.poll) => {}
                s = watch.recv_async() => {
                    if s.is_err() {
                        break;
                    }
                    // Let a burst of publishes settle into one pass.
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    while watch.try_recv().ok().flatten().is_some() {}
                }
            }
        }
    }
}
