//! `mw-updater`: what to install where (pure), and the installs (UP6, UP7).
//!
//! Every device is a serial port: an MCU board on USB, or the local
//! posix-node's link (`MW_POSIX_NODE_LOCAL_SERIAL`, a PTY). Over a serial
//! port physical access is the authority for start, reboot and confirm, so
//! only the commit needs a signature: the release cert.
//!
//! One pass ([`decide`] per device and kind):
//!
//! 1. Read the device ([`read_facts`]): `sys/info` (chip, node), the trust
//!    state, and `sys/fw/info` when it has a firmware updater.
//! 2. Ask the channel for the chip's `latest` (the shell, [`crate::client`]).
//! 3. Install a release only when the device is **claimed by the root key**
//!    and the cert is for the updater's own channel,
//!    (an unclaimed device would take anything; provisioning is a
//!    deliberate `mwctl trust claim`), the cert verifies against the root
//!    and is for this chip, and its version is above the device's floor:
//!    the trust state's `min_version` for payloads, the larger of the
//!    firmware record's floor and the running image's version for
//!    firmware. A device mid-update (committed or on trial) is left alone.
//! 4. Payload ([`install_payload`]): `sys/loader/begin`, chunks, the cert
//!    commit, `sys/payload/start`. Firmware ([`install_firmware`]):
//!    `sys/fw/begin`, chunks, the cert commit (the device picks the slot),
//!    `reboot`; then reopen the port until the board answers, and
//!    `confirm` when it runs the new slot on trial, or report the
//!    rollback.
//!
//! The updater checks the image hash and the cert itself before it sends
//! anything (the device checks both again).

use std::time::{Duration, Instant};

use middleware::fw_ota::wire::{FwInfo, StateCode};
use middleware::loader::release::{CertKind, verify_cert};
use middleware::loader::trust::{PublicKey, TrustInfo, TrustState, Version};
use middleware::transport::HandlerError;
use mwctl::device::{Device, Info, UploadOptions};
use mwctl::error::Error;
use mwctl::fw::{UpdateOptions, load_image};
use mwctl::link::Endpoint;
use serde::Serialize;

use crate::wire::Release;

/// What one device said about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    /// `sys/info`.
    pub info: Info,
    /// `sys/trust/info`.
    pub trust: TrustInfo,
    /// `sys/fw/info`, when the device has a firmware updater.
    pub fw: Option<FwInfo>,
}

/// What to do for one kind on one device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Install this release.
    Install(Release),
    /// The device has it (or something newer).
    UpToDate(Version),
    /// Leave the device alone, and why.
    Skip(String),
}

/// The firmware version a device runs and its floor.
pub fn fw_floor(fw: &FwInfo) -> Version {
    let running = fw.slots[fw.running as usize].version.unwrap_or_default();
    fw.min_version.max(running)
}

/// The decision for `kind` on a device with `facts`, given `channel`'s
/// `latest` for its chip and the `root` key (module docs). A cert for
/// another channel is skipped: the server is not trusted to keep
/// channels apart (a `test` release must never reach a `stable` device).
/// Pure.
pub fn decide(
    facts: &Facts,
    kind: CertKind,
    channel: &str,
    latest: &[Release],
    root: &PublicKey,
) -> Decision {
    let skip = |s: &str| Decision::Skip(s.to_owned());
    let Some(release) = latest.iter().find(|r| r.kind() == kind) else {
        return skip("no release");
    };
    let signed = release.signed();
    if !verify_cert(root, &signed) || !signed.cert.is_for(&facts.info.chip) {
        return skip("the release is not the root's for this chip");
    }
    if signed.cert.channel != channel {
        return skip("the release is for another channel");
    }
    let min_version = match facts.trust.state {
        TrustState::Unclaimed => return skip("unclaimed (claim it with the root key)"),
        TrustState::Claimed { key, .. } if key != *root => {
            return skip("claimed by another key");
        }
        TrustState::Claimed { min_version, .. } => min_version,
    };
    let version = signed.cert.version;
    let floor = match kind {
        CertKind::Payload => {
            if !facts.info.runs_payloads() {
                return skip("the firmware runs no payloads");
            }
            min_version
        }
        CertKind::Firmware => {
            let Some(fw) = &facts.fw else {
                return skip("no firmware updater");
            };
            if matches!(fw.state, StateCode::Committed | StateCode::Trial) {
                return skip("a firmware update is in progress");
            }
            fw_floor(fw)
        }
    };
    if version <= floor {
        Decision::UpToDate(floor)
    } else {
        Decision::Install(release.clone())
    }
}

/// Reads [`Facts`] from `dev`.
pub fn read_facts<L: Endpoint>(dev: &mut Device<L>) -> Result<Facts, Error> {
    let info = dev.info()?;
    let trust = dev.trust_info()?;
    let fw = dev.fw_info().ok();
    Ok(Facts { info, trust, fw })
}

/// Installs and starts the payload `image` of `release`.
///
/// A commit that outlives the link's timeout is resent, and the resend
/// meets an idle loader (`InvalidRequest`) although the first copy took (a
/// slow SD card); so after a commit refused as `InvalidRequest` the trust
/// floor is read back, and when it is exactly this release's version the
/// commit took and the payload is started. Any other failure (a `Denied`
/// replay included) is returned.
pub fn install_payload<L: Endpoint>(
    dev: &mut Device<L>,
    release: &Release,
    image: &[u8],
) -> Result<(), Error> {
    let upload = dev.upload_release(
        image,
        release.signed(),
        UploadOptions {
            start: true,
            ..UploadOptions::default()
        },
        |_| {},
    );
    let Err(e) = upload else {
        return Ok(());
    };
    let resent_commit = matches!(
        &e,
        Error::Upload { key, cause, .. }
            if key == middleware::loader::keys::COMMIT.as_str()
                && matches!(**cause, Error::Device { status: HandlerError::InvalidRequest, .. })
    );
    if !resent_commit {
        return Err(e);
    }
    match dev.trust_info().map(|t| t.state) {
        Ok(TrustState::Claimed { min_version, .. }) if min_version == release.version() => {
            dev.start()
        }
        _ => Err(e),
    }
}

/// Opens a device; called again after a firmware reboot (a USB port goes
/// away with the board and comes back).
pub trait Opener {
    /// The path to the device.
    type Link: Endpoint;
    /// Opens it.
    fn open(&mut self) -> Result<Device<Self::Link>, Error>;
}

/// How a firmware install ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FwOutcome {
    /// The board runs the new image and it is confirmed.
    Confirmed,
    /// The board came back on the old image.
    RolledBack,
}

/// Installs the firmware `image` of `release` into the inactive slot,
/// reboots into it, and confirms it once the board answers on it (within
/// `within`, retrying every `pause`).
pub fn install_firmware<O: Opener>(
    opener: &mut O,
    release: &Release,
    image: &[u8],
    within: Duration,
    pause: Duration,
) -> Result<FwOutcome, Error> {
    let version = release.version();
    let fw = load_image(image, Some(version))?;
    if fw.bytes != image {
        // The cert covers the bytes sent; a UF2 is published converted.
        return Err(Error::Fw("the release image is not a raw binary".into()));
    }
    let done = {
        let mut dev = opener.open()?;
        dev.fw_update(
            &fw,
            UpdateOptions {
                key: None,
                release: Some(release.signed()),
                reboot: true,
                control: None,
            },
            |_| {},
        )?
    };
    let deadline = Instant::now() + within;
    loop {
        std::thread::sleep(pause);
        let answer = opener.open().and_then(|mut d| {
            let info = d.fw_info()?;
            Ok((d, info))
        });
        match answer {
            Ok((mut dev, info)) => {
                if info.running == done.slot && info.state == StateCode::Trial {
                    dev.fw_confirm(30, || std::thread::sleep(pause))?;
                    return Ok(FwOutcome::Confirmed);
                }
                if info.running == done.slot && info.state == StateCode::Confirmed {
                    return Ok(FwOutcome::Confirmed);
                }
                if info.running != done.slot
                    && matches!(info.state, StateCode::RolledBack | StateCode::Idle)
                {
                    return Ok(FwOutcome::RolledBack);
                }
            }
            Err(e) if Instant::now() >= deadline => return Err(e),
            Err(_) => {}
        }
        if Instant::now() >= deadline {
            return Err(Error::Fw(
                "the board did not come back on the new image".into(),
            ));
        }
    }
}

/// One line of `@mw/updates/status/<node>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    /// The device (its port).
    pub device: String,
    /// Its node id (hex), if it reports one.
    pub node: Option<String>,
    /// Its chip.
    pub chip: String,
    /// `payload` or `fw`.
    pub kind: &'static str,
    /// The release version considered, if any.
    pub version: Option<u32>,
    /// `installed`, `confirmed`, `rolled-back`, `up-to-date`,
    /// `skipped: …` or `failed: …`.
    pub result: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::tests::{cert, seed};
    use middleware::fw_ota::Slot;
    use middleware::fw_ota::wire::SlotInfo;
    use middleware::loader::{Caps, PayloadStatus};
    use middleware::transport::SenderId;
    use mwctl::sign::Seed;

    fn facts(chip: &str, trust: TrustState, fw: Option<FwInfo>) -> Facts {
        Facts {
            info: Info {
                abi_version: 9,
                capacity: 65536,
                status: PayloadStatus::Stopped,
                chip: chip.into(),
                firmware: "0.1.0".into(),
                node: Some(SenderId([1; 8])),
                caps: Caps::NONE,
            },
            trust: TrustInfo {
                state: trust,
                rekey_open: false,
            },
            fw,
        }
    }

    fn claimed(min: u32) -> TrustState {
        TrustState::Claimed {
            key: seed().public_key(),
            min_version: Version(min),
        }
    }

    fn fw_info(running_version: u32, min: u32, state: StateCode) -> FwInfo {
        FwInfo {
            state,
            running: Slot::A,
            slots: [
                SlotInfo {
                    version: Some(Version(running_version)),
                    tbyb: false,
                },
                SlotInfo {
                    version: None,
                    tbyb: false,
                },
            ],
            min_version: Version(min),
            node: SenderId([1; 8]),
            target: None,
            version: Version(0),
            len: 0,
            received: 0,
            self_test_ok: true,
        }
    }

    fn rel(kind: CertKind, target: &'static str, v: u32) -> Release {
        Release::sign(&cert(kind, target, v, b"img"), &seed()).unwrap()
    }

    #[test]
    fn up6_payloads_install_above_the_trust_floor_only_on_root_claimed_devices() {
        let root = seed().public_key();
        let latest = [rel(CertKind::Payload, "linux-x86_64", 10)];
        let d = |t| {
            decide(
                &facts("linux-x86_64", t, None),
                CertKind::Payload,
                "test",
                &latest,
                &root,
            )
        };
        assert_eq!(d(claimed(9)), Decision::Install(latest[0].clone()));
        assert_eq!(d(claimed(10)), Decision::UpToDate(Version(10)));
        assert_eq!(d(claimed(11)), Decision::UpToDate(Version(11)));
        assert!(matches!(d(TrustState::Unclaimed), Decision::Skip(_)));
        let other = TrustState::Claimed {
            key: Seed::new([1; 32]).public_key(),
            min_version: Version(0),
        };
        assert!(matches!(d(other), Decision::Skip(_)));
        // Wrong chip, a release not signed by the root, nothing published.
        let f = facts("rp2350", claimed(0), None);
        assert!(matches!(
            decide(&f, CertKind::Payload, "test", &latest, &root),
            Decision::Skip(_)
        ));
        let forged = [Release::sign(
            &cert(CertKind::Payload, "linux-x86_64", 10, b"img"),
            &Seed::new([9; 32]),
        )
        .unwrap()];
        let f = facts("linux-x86_64", claimed(0), None);
        assert!(matches!(
            decide(&f, CertKind::Payload, "test", &forged, &root),
            Decision::Skip(_)
        ));
        assert!(matches!(
            decide(&f, CertKind::Payload, "test", &[], &root),
            Decision::Skip(_)
        ));
        // `any` matches every chip.
        let any = [rel(CertKind::Payload, "any", 3)];
        assert!(matches!(
            decide(
                &facts("esp32s3", claimed(0), None),
                CertKind::Payload,
                "test",
                &any,
                &root
            ),
            Decision::Install(_)
        ));
        // A release for another channel, even signed by the root.
        let f = facts("linux-x86_64", claimed(0), None);
        assert!(matches!(
            decide(&f, CertKind::Payload, "stable", &latest, &root),
            Decision::Skip(_)
        ));
        // A no-wasm firmware.
        let mut f = facts("linux-x86_64", claimed(0), None);
        f.info.firmware = "0.1.0+no-wasm".into();
        assert!(matches!(
            decide(&f, CertKind::Payload, "test", &latest, &root),
            Decision::Skip(_)
        ));
    }

    #[test]
    fn up7_firmware_installs_above_running_and_floor_and_never_mid_update() {
        let root = seed().public_key();
        let latest = [rel(CertKind::Firmware, "rp2350", 0x0001_0002)];
        let d = |fw| {
            decide(
                &facts("rp2350", claimed(0), fw),
                CertKind::Firmware,
                "test",
                &latest,
                &root,
            )
        };
        assert_eq!(
            d(Some(fw_info(0x0001_0001, 0, StateCode::Idle))),
            Decision::Install(latest[0].clone())
        );
        assert_eq!(
            d(Some(fw_info(0x0001_0002, 0, StateCode::Confirmed))),
            Decision::UpToDate(Version(0x0001_0002))
        );
        assert_eq!(
            d(Some(fw_info(
                0x0001_0000,
                0x0001_0003,
                StateCode::RolledBack
            ))),
            Decision::UpToDate(Version(0x0001_0003))
        );
        for s in [StateCode::Committed, StateCode::Trial] {
            assert!(matches!(d(Some(fw_info(1, 0, s))), Decision::Skip(_)));
        }
        assert!(matches!(d(None), Decision::Skip(_)));
    }
}
