//! `mw-release publish`: build the cert, sign it with the root, put it,
//! and wait until the server lists it (UP4).
//!
//! The server answers a put with nothing, so the publisher queries
//! `latest` until its release appears; one that never does was refused
//! (the server logs why: a key other than the root, a downgrade, ...).

use std::time::{Duration, Instant, SystemTime};

use middleware::loader::release::{CertKind, SoftwareCert};
use middleware::loader::trust::Version;
use mwctl::fw::load_image;
use mwctl::sign::{Signer, image_sha256, version_at};

use crate::client::Client;
use crate::wire::Release;

/// The image bytes a cert covers and the version it carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    /// What the device receives.
    pub image: Vec<u8>,
    /// The release version.
    pub version: Version,
}

/// Prepares `file` as a `kind` release. Firmware: the image as the device
/// receives it (a UF2 converted to its binary), checked as `mwctl fw
/// update` checks it, with its own version (`version`, when given, must
/// equal it). Payload: the file as is, at `version` or the Unix time in
/// seconds at `now`.
pub fn prepare(
    kind: CertKind,
    file: &[u8],
    version: Option<Version>,
    now: SystemTime,
) -> Result<Prepared, String> {
    match kind {
        CertKind::Firmware => {
            let img = load_image(file, version).map_err(|e| e.to_string())?;
            Ok(Prepared {
                image: img.bytes,
                version: img.version,
            })
        }
        CertKind::Payload => {
            if file.is_empty() {
                return Err("empty payload".into());
            }
            let version = match version {
                Some(v) => v,
                None => version_at(now).map_err(|e| e.to_string())?,
            };
            Ok(Prepared {
                image: file.to_vec(),
                version,
            })
        }
    }
}

/// Settles the version of a `kind` release against what the channel has
/// (`current`, the version of its latest release of that kind): a payload
/// published without `--version` (`requested` `None`) takes the larger of
/// `prepared.version` (the clock) and `current + 1`, so two publishes in
/// one second still rise; any other version must exceed `current`, or the
/// server would refuse it, so the publisher says so first. Pure.
pub fn settle_version(
    kind: CertKind,
    requested: Option<Version>,
    prepared: &mut Prepared,
    current: Option<Version>,
) -> Result<(), String> {
    let Some(cur) = current else {
        return Ok(());
    };
    if kind == CertKind::Payload && requested.is_none() {
        prepared.version = prepared.version.max(Version(cur.0.saturating_add(1)));
        return Ok(());
    }
    if prepared.version <= cur {
        let show = |v: Version| match kind {
            CertKind::Firmware => mwctl::fw::format_version(v),
            CertKind::Payload => v.0.to_string(),
        };
        return Err(format!(
            "the channel already has {} {}; this release ({}) must be newer",
            kind.name(),
            show(cur),
            show(prepared.version)
        ));
    }
    Ok(())
}

/// Signs the release of `prepared` for `target` on `channel`.
pub fn sign(
    kind: CertKind,
    channel: &str,
    target: &str,
    prepared: &Prepared,
    signer: &dyn Signer,
) -> Result<Release, String> {
    if !(crate::keys::valid_name(channel) && crate::keys::valid_name(target)) {
        return Err("channel and target: 1-32 of a-z 0-9 . _ -".into());
    }
    let len = u32::try_from(prepared.image.len()).map_err(|_| "image too large".to_owned())?;
    let cert = SoftwareCert {
        kind,
        target,
        channel,
        version: prepared.version,
        len,
        sha256: image_sha256(&prepared.image),
    };
    Release::sign(&cert, signer).map_err(|e| e.to_string())
}

/// Puts `release` with `image` and waits up to `within` for it in
/// `latest`.
pub async fn publish(
    client: &Client,
    release: &Release,
    image: &[u8],
    within: Duration,
) -> Result<(), String> {
    let c = release.cert();
    client
        .publish(release, image)
        .await
        .map_err(|e| e.to_string())?;
    let deadline = Instant::now() + within;
    loop {
        if let Ok(latest) = client.latest(c.channel, c.target).await
            && latest.contains(release)
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "{} {} is not in {}/{} latest: the server refused it or is unreachable (see its log)",
                c.kind.name(),
                c.version.0,
                c.channel,
                c.target
            ));
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(v: u32) -> Prepared {
        Prepared {
            image: vec![1],
            version: Version(v),
        }
    }

    #[test]
    fn up4_versions_rise_past_what_the_channel_has() {
        let mut a = p(100);
        settle_version(CertKind::Payload, None, &mut a, Some(Version(100))).unwrap();
        assert_eq!(a.version, Version(101), "same second: one above");
        let mut b = p(200);
        settle_version(CertKind::Payload, None, &mut b, Some(Version(100))).unwrap();
        assert_eq!(b.version, Version(200), "the clock when it is newer");
        let mut c = p(5);
        settle_version(CertKind::Payload, None, &mut c, None).unwrap();
        assert_eq!(c.version, Version(5));
        let mut d = p(7);
        let e = settle_version(
            CertKind::Payload,
            Some(Version(7)),
            &mut d,
            Some(Version(7)),
        )
        .unwrap_err();
        assert!(e.contains("already has payload 7"), "{e}");
        let mut f = p(0x0001_0002);
        let e = settle_version(CertKind::Firmware, None, &mut f, Some(Version(0x0001_0002)))
            .unwrap_err();
        assert!(e.contains("already has fw 1.2"), "{e}");
    }
}
