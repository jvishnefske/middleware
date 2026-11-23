//! One updater pass over every device (the shell around
//! [`updater::decide`]; UP6, UP7).
//!
//! Device requests are blocking (`mwctl`'s serial link), so they run in
//! [`tokio::task::block_in_place`]: the runtime must be multi-threaded.

use std::collections::HashMap;
use std::time::Duration;

use middleware::loader::release::CertKind;
use middleware::loader::trust::PublicKey;
use mwctl::error::Error;

use crate::client::Client;
use crate::updater::{
    Decision, Facts, FwOutcome, Opener, Report, decide, install_firmware, install_payload,
    read_facts,
};

/// How long a board may take to come back after a firmware reboot.
pub const FW_COME_BACK: Duration = Duration::from_secs(90);
/// The pause between attempts to reach it.
pub const FW_PAUSE: Duration = Duration::from_secs(1);

fn read<O: Opener>(o: &mut O) -> Result<Facts, Error> {
    tokio::task::block_in_place(|| o.open().and_then(|mut d| read_facts(&mut d)))
}

fn report(device: &str, facts: Option<&Facts>, kind: &'static str) -> Report {
    Report {
        device: device.to_owned(),
        node: facts.and_then(|f| f.info.node).map(|n| crate::hex(&n.0)),
        chip: facts.map(|f| f.info.chip.clone()).unwrap_or_default(),
        kind,
        version: None,
        result: String::new(),
    }
}

/// Runs one pass: for each `(name, opener)`, firmware first (a new
/// firmware may change what payloads it runs), then the payload, each per
/// [`decide`]; publishes and returns one [`Report`] per device and kind.
/// Images are fetched once per pass.
pub async fn pass<O: Opener>(
    client: &Client,
    channel: &str,
    root: &PublicKey,
    devices: &mut [(String, O)],
    node: &str,
    fw_wait: Duration,
) -> Vec<Report> {
    let mut images: HashMap<[u8; 32], Vec<u8>> = HashMap::new();
    let mut reports = Vec::new();
    for (name, opener) in devices.iter_mut() {
        let mut facts = match read(opener) {
            Ok(f) => f,
            Err(e) => {
                let mut r = report(name, None, "device");
                r.result = format!("failed: {e}");
                reports.push(r);
                continue;
            }
        };
        let latest = match client.latest(channel, &facts.info.chip).await {
            Ok(l) => l,
            Err(e) => {
                let mut r = report(name, Some(&facts), "device");
                r.result = format!("failed: {e}");
                reports.push(r);
                continue;
            }
        };
        for kind in [CertKind::Firmware, CertKind::Payload, CertKind::Os] {
            let mut r = report(name, Some(&facts), kind.name());
            let release = match decide(&facts, kind, channel, &latest, root) {
                Decision::Skip(why) => {
                    r.result = format!("skipped: {why}");
                    reports.push(r);
                    continue;
                }
                Decision::UpToDate(v) => {
                    r.version = Some(v.0);
                    r.result = "up-to-date".into();
                    reports.push(r);
                    continue;
                }
                Decision::Install(release) => release,
            };
            if kind == CertKind::Os {
                // A root needs a signed control request to boot and confirm,
                // and this pass holds no signing key: `just os-update` does it.
                r.result = "skipped: a Linux root is installed with `just os-update` (signed control)".into();
                reports.push(r);
                continue;
            }
            r.version = Some(release.version().0);
            let sha = release.cert().sha256;
            let image = match images.get(&sha) {
                Some(i) => i.clone(),
                None => match client.fetch(&release).await {
                    Ok(i) => {
                        images.insert(sha, i.clone());
                        i
                    }
                    Err(e) => {
                        r.result = format!("failed: fetch: {e}");
                        reports.push(r);
                        continue;
                    }
                },
            };
            let result = tokio::task::block_in_place(|| match kind {
                CertKind::Payload => opener
                    .open()
                    .and_then(|mut d| install_payload(&mut d, &release, &image))
                    .map(|()| "installed".to_owned()),
                CertKind::Firmware => install_firmware(opener, &release, &image, fw_wait, FW_PAUSE)
                    .map(|o| match o {
                        FwOutcome::Confirmed => "confirmed".to_owned(),
                        FwOutcome::RolledBack => "rolled-back".to_owned(),
                    }),
                CertKind::Os => Err(Error::Fw("a Linux root is not installed by a pass".into())),
            });
            r.result = result.unwrap_or_else(|e| format!("failed: {e}"));
            reports.push(r);
            if matches!(kind, CertKind::Firmware | CertKind::Os) {
                // The board runs other firmware now: read it again.
                match read(opener) {
                    Ok(f) => facts = f,
                    Err(_) => break,
                }
            }
        }
    }
    for r in &reports {
        let json = serde_json::to_string(r).unwrap_or_default();
        let _ = client.status(node, json).await;
    }
    reports
}
