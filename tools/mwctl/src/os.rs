//! `mwctl os`: the Linux A/B root (design.md, "Linux A/B root (OS)") over
//! `sys/os/*`. The transfer is the firmware's (`fw.rs`) in shape (begin,
//! chunks, commit with a root-signed cert, reboot, confirm, abort), with
//! its own key prefix, boot-state format and chunk size (64 KiB, so a
//! Zenoh or UDP link carries a root in minutes, not the serial's hours).
//! Each request goes through the `Endpoint` link, so serial, Zenoh and UDP
//! all work; `os update` stops at the commit, and `os reboot` then
//! `os confirm` (or the updater) finish the trial.

use std::time::Duration;

use middleware::loader::release::SignedCert;
use middleware::loader::trust::{ControlOp, SignedControl, Version};
use middleware::os_ab::bootstate::Slot;
use middleware::os_ab::wire::{CHUNK_LEN, Command, OsInfo, StateCode, keys};
use middleware::transport::HandlerError;

use crate::device::{Device, Progress};
use crate::fw::ControlSigner;
use crate::error::Error;
use crate::link::Endpoint;
use crate::sign::image_sha256;

/// `mwctl os info`, as lines.
pub fn format_os_info(i: &OsInfo) -> String {
    let slot = |s: Option<Slot>| s.map_or("none".to_owned(), |s| s.to_string());
    format!(
        "running:      {}\nstate:        {}\nupdate slot:  {}\nversion:      {}\nfloor:        {}\nslot size:    {} bytes\n",
        i.running,
        i.state.name(),
        slot(i.target),
        i.running_version,
        i.floor,
        i.capacity,
    )
}

impl<L: Endpoint> Device<L> {
    fn os(&mut self, cmd: &Command<'_>) -> Result<Vec<u8>, Error> {
        let mut body = vec![0u8; 4 + CHUNK_LEN];
        let n = cmd.encode_body(&mut body).ok_or(Error::Frame(
            middleware::serial::frame::FrameError::TooLarge,
        ))?;
        self.link_mut().request(cmd.key(), &body[..n])
    }

    /// `sys/os/info`.
    pub fn os_info(&mut self) -> Result<OsInfo, Error> {
        let body = self.os(&Command::Info)?;
        OsInfo::decode(&body).ok_or_else(|| Error::Malformed {
            key: keys::INFO.as_str().to_owned(),
        })
    }

    /// Writes `image` into the inactive slot and commits it with `cert`
    /// (the root-signed `Os` release; required by a claimed root). Refuses
    /// locally, before sending the image, when a root is committed or on
    /// trial, or when `version` does not exceed the device's floor.
    pub fn os_update(
        &mut self,
        image: &[u8],
        version: Version,
        cert: Option<SignedCert<'_>>,
        mut progress: impl FnMut(Progress),
    ) -> Result<Slot, Error> {
        let info = self.os_info()?;
        if matches!(info.state, StateCode::Committed | StateCode::Trial) {
            return Err(Error::Fw(format!(
                "a root update is {} (slot {}); `os confirm` or `os abort` first",
                info.state.name(),
                info.target.map_or("?".to_owned(), |s| s.to_string()),
            )));
        }
        let floor = info.floor.max(info.running_version);
        if version.0 <= floor {
            return Err(Error::Fw(format!(
                "version {} does not exceed the device's {floor}",
                version.0
            )));
        }
        if let Some(c) = &cert {
            if c.cert.len as usize != image.len() || c.cert.sha256 != image_sha256(image) || c.cert.version != version {
                return Err(Error::Fw("the cert is not for this image and version".into()));
            }
        }
        let len = u32::try_from(image.len()).map_err(|_| Error::Fw("image too large".into()))?;
        let steps = 2 + image.len().div_ceil(CHUNK_LEN);
        let wrap = |step: usize, key: middleware::Key<'static>, e: Error| Error::Upload {
            step,
            steps,
            key: key.as_str().to_owned(),
            cause: Box::new(e),
        };
        let begin = Command::Begin {
            len,
            version,
            sha256: image_sha256(image),
        };
        let reply = self.os(&begin).map_err(|e| wrap(0, keys::BEGIN, e))?;
        let slot = reply
            .first()
            .and_then(|b| Slot::from_index(*b))
            .ok_or_else(|| Error::Malformed {
                key: keys::BEGIN.as_str().to_owned(),
            })?;
        progress(Progress {
            step: 0,
            steps,
            done: 0,
            total: len,
            events: self.take_events(),
        });
        for (i, data) in image.chunks(CHUNK_LEN).enumerate() {
            let offset = (i * CHUNK_LEN) as u32;
            self.os(&Command::Chunk { offset, data })
                .map_err(|e| wrap(i + 1, keys::CHUNK, e))?;
            progress(Progress {
                step: i + 1,
                steps,
                done: offset + data.len() as u32,
                total: len,
                events: self.take_events(),
            });
        }
        self.os(&Command::Commit { cert })
            .map_err(|e| wrap(steps - 1, keys::COMMIT, e))?;
        progress(Progress {
            step: steps - 1,
            steps,
            done: len,
            total: len,
            events: self.take_events(),
        });
        Ok(slot)
    }

    /// `sys/os/reboot`: the trial boot of the committed slot. Signed when
    /// `control` is given (a claimed node on the network requires it). A
    /// port that goes away with the reset counts as done.
    pub fn os_reboot(&mut self, control: Option<ControlSigner<'_>>) -> Result<(), Error> {
        let control = os_control(ControlOp::OsReboot, control.as_ref().map(|c| c.0))?;
        match self.os(&Command::Reboot { control }) {
            Err(e) if e.is_port_lost() => Ok(()),
            r => r.map(drop),
        }
    }

    /// `sys/os/confirm`, retried while the root answers `Unavailable` (the
    /// trial is not yet the running slot), at most `attempts` times.
    pub fn os_confirm(
        &mut self,
        attempts: u32,
        mut pause: impl FnMut(),
        control: Option<ControlSigner<'_>>,
    ) -> Result<(), Error> {
        let sign = control.as_ref().map(|c| c.0);
        let mut left = attempts.max(1);
        loop {
            let signed = os_control(ControlOp::OsConfirm, sign)?;
            match self.os(&Command::Confirm { control: signed }) {
                Err(Error::Device {
                    status: HandlerError::Unavailable,
                    ..
                }) if left > 1 => {
                    left -= 1;
                    pause();
                }
                other => return other.map(drop),
            }
        }
    }

    /// `sys/os/abort`: drops a transfer or a committed slot; from a trial,
    /// reboots into the committed slot. Signed when `control` is given.
    /// Idempotent.
    pub fn os_abort(&mut self, control: Option<ControlSigner<'_>>) -> Result<(), Error> {
        let control = os_control(ControlOp::OsAbort, control.as_ref().map(|c| c.0))?;
        self.os(&Command::Abort { control }).map(drop)
    }
}

/// The signed form of a control request for `op`, or none.
fn os_control(
    op: ControlOp,
    sign: Option<&dyn Fn(ControlOp) -> Result<SignedControl, Error>>,
) -> Result<Option<SignedControl>, Error> {
    sign.map(|s| s(op)).transpose()
}

/// How long `os confirm` keeps retrying: the trial's own boot.
pub const CONFIRM_PAUSE: Duration = Duration::from_millis(500);

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::os_ab::bootstate::Slot;

    #[test]
    fn os_info_lines_name_the_slots_and_the_floor() {
        let i = OsInfo {
            running: Slot::A,
            state: StateCode::Idle,
            target: Some(Slot::B),
            running_version: 7,
            floor: 9,
            capacity: 3 << 30,
        };
        let s = format_os_info(&i);
        assert!(s.contains("running:      a\n"), "{s}");
        assert!(s.contains("update slot:  b\n"), "{s}");
        assert!(s.contains("floor:        9\n"), "{s}");
        let none = format_os_info(&OsInfo { target: None, ..i });
        assert!(none.contains("update slot:  none\n"));
    }
}
