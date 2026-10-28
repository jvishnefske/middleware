//! Protocol operations over a [`Link`]: the CLI commands minus printing.
//!
//! * [`Device::info`] — `sys/info`, decoded with
//!   [`middleware::loader::DeviceInfo::decode`] into an owned [`Info`].
//! * [`Device::upload`] — runs an [`upload_plan`]
//!   step by step, reporting progress after each acknowledged step, then
//!   optionally `sys/payload/start`. A failed step is wrapped in
//!   [`Error::Upload`] with its index and key. Retries and the longer
//!   `begin` timeout come from the link's [`Timeouts`](crate::link::Timeouts).
//!   With [`UploadOptions::auth`] the commit is signed (see
//!   [`crate::sign`]); a device with a pinned key refuses an unsigned or
//!   stale commit with `Denied` and keeps its installed payload.
//! * [`Device::trust_info`], [`Device::trust_claim`] — `sys/trust/info`
//!   (decoded with [`TrustInfo::decode`]) and `sys/trust/claim`.
//! * [`Device::start`], [`Device::stop`] — `sys/payload/start` / `stop`.
//! * [`Device::next_event`] — the next publication or noise run, for
//!   `monitor`.

use crate::error::Error;
use crate::link::{Event, Link, Transport};
use crate::plan::upload_plan;
use middleware::loader::trust::{PublicKey, SignedCommit, TrustInfo};
use middleware::loader::{Command, DeviceInfo, PayloadStatus, keys};
use std::time::Instant;

/// Suffix of the firmware version of builds without the WASM runtime.
pub const NO_WASM_SUFFIX: &str = "+no-wasm";

/// Owned `sys/info` response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    /// Payload ABI version implemented by the firmware.
    pub abi_version: u16,
    /// Largest payload image in bytes.
    pub capacity: u32,
    /// Payload state.
    pub status: PayloadStatus,
    /// Chip name, e.g. `esp8266`.
    pub chip: String,
    /// Firmware version, e.g. `0.1.0+no-wasm`.
    pub firmware: String,
}

impl Info {
    /// Decodes a `sys/info` response body.
    pub fn decode(body: &[u8]) -> Option<Self> {
        let i = DeviceInfo::decode(body)?;
        Some(Info {
            abi_version: i.abi_version,
            capacity: i.capacity,
            status: i.status,
            chip: i.chip.to_owned(),
            firmware: i.firmware.to_owned(),
        })
    }

    /// Whether the firmware includes the WASM runtime (can start payloads).
    pub fn runs_payloads(&self) -> bool {
        !self.firmware.ends_with(NO_WASM_SUFFIX)
    }
}

/// Upload settings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UploadOptions {
    /// Send `sys/payload/start` after a successful commit.
    pub start: bool,
    /// Test hook: corrupt this chunk's data (zero-based) while declaring
    /// the original image's CRC, so commit fails on the device.
    pub corrupt_chunk: Option<usize>,
    /// Sign the commit with this authorisation; `None` sends an unsigned
    /// commit.
    pub auth: Option<SignedCommit>,
}

/// Progress after one acknowledged upload step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    /// Zero-based index of the step just acknowledged.
    pub step: usize,
    /// Number of steps.
    pub steps: usize,
    /// Image bytes delivered.
    pub done: u32,
    /// Image length.
    pub total: u32,
    /// Publications and noise received during the step.
    pub events: Vec<Event>,
}

/// A device on the other end of a transport.
pub struct Device<T> {
    link: Link<T>,
}

impl<T: Transport> Device<T> {
    /// Wraps a link.
    pub fn new(link: Link<T>) -> Self {
        Device { link }
    }

    /// The underlying link.
    pub fn link(&self) -> &Link<T> {
        &self.link
    }

    /// Publications and noise received outside [`Device::next_event`].
    pub fn take_events(&mut self) -> Vec<Event> {
        self.link.take_events()
    }

    /// `sys/info`.
    pub fn info(&mut self) -> Result<Info, Error> {
        let body = self.link.command(&Command::Info)?;
        Info::decode(&body).ok_or_else(|| Error::Malformed {
            key: keys::INFO.as_str().to_owned(),
        })
    }

    /// `sys/trust/info`.
    pub fn trust_info(&mut self) -> Result<TrustInfo, Error> {
        let body = self.link.command(&Command::TrustInfo)?;
        TrustInfo::decode(&body).ok_or_else(|| Error::Malformed {
            key: keys::TRUST_INFO.as_str().to_owned(),
        })
    }

    /// `sys/trust/claim`: pins `key` as the payload signing key.
    pub fn trust_claim(&mut self, key: PublicKey) -> Result<(), Error> {
        self.link.command(&Command::TrustClaim { key }).map(drop)
    }

    /// `sys/payload/start`.
    pub fn start(&mut self) -> Result<(), Error> {
        self.link.command(&Command::Start).map(drop)
    }

    /// `sys/payload/stop`.
    pub fn stop(&mut self) -> Result<(), Error> {
        self.link.command(&Command::Stop).map(drop)
    }

    /// Installs `image`; calls `progress` after each acknowledged step.
    pub fn upload(
        &mut self,
        image: &[u8],
        opts: UploadOptions,
        mut progress: impl FnMut(Progress),
    ) -> Result<(), Error> {
        let plan = upload_plan(image, opts.corrupt_chunk, opts.auth)?;
        let total = u32::try_from(image.len()).unwrap_or(u32::MAX);
        let steps = plan.len() + usize::from(opts.start);
        for (i, s) in plan.iter().enumerate() {
            self.link
                .request(s.key, &s.body)
                .map_err(|e| Error::Upload {
                    step: i,
                    steps,
                    key: s.key.as_str().to_owned(),
                    cause: Box::new(e),
                })?;
            progress(Progress {
                step: i,
                steps,
                done: s.done,
                total,
                events: self.link.take_events(),
            });
        }
        if opts.start {
            self.start().map_err(|e| Error::Upload {
                step: plan.len(),
                steps,
                key: keys::START.as_str().to_owned(),
                cause: Box::new(e),
            })?;
        }
        Ok(())
    }

    /// The next publication or noise run before `deadline`.
    pub fn next_event(&mut self, deadline: Instant) -> Result<Option<Event>, Error> {
        self.link.next_event(deadline)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FakeDevice, Script};
    use crate::link::Timeouts;
    use crate::sign::sign_commit;
    use crate::sign::tests::{vector_image, vector_seed};
    use middleware::crc::Crc32;
    use middleware::loader::trust::{TrustCode, TrustState, Version};
    use middleware::transport::HandlerError;
    use std::time::Duration;

    fn image(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 31 + 7) as u8).collect()
    }

    fn device(fake: FakeDevice) -> Device<FakeDevice> {
        // Short timeouts: the fake answers synchronously, and reports a
        // dropped reply as an immediate read timeout anyway.
        let t = Timeouts {
            request: Duration::from_millis(50),
            begin: Duration::from_millis(50),
            retries: 1,
        };
        Device::new(Link::new(fake, t))
    }

    #[test]
    fn info_decodes_a_protocol_only_esp8266() {
        let mut d = device(FakeDevice::new("esp8266", "0.1.0+no-wasm", 4096, false));
        let i = d.info().unwrap();
        assert_eq!(i.chip, "esp8266");
        assert_eq!(i.firmware, "0.1.0+no-wasm");
        assert_eq!(i.capacity, 4096);
        assert_eq!(i.status, PayloadStatus::Empty);
        assert_eq!(i.abi_version, middleware::wasm_abi::ABI_VERSION);
        assert!(!i.runs_payloads());
        assert!(Info::decode(&[1, 2, 3]).is_none());
        // The host sent one leading delimiter before the first frame.
        assert_eq!(d.link().transport().raw_rx().first(), Some(&0));
    }

    #[test]
    fn upload_happy_path_installs_reports_progress_and_starts() {
        let img = image(1300);
        let mut d = device(FakeDevice::new("esp32", "0.1.0", 4096, true));
        let mut seen = Vec::new();
        d.upload(
            &img,
            UploadOptions {
                start: true,
                ..UploadOptions::default()
            },
            |p| seen.push((p.step, p.steps, p.done, p.total)),
        )
        .unwrap();
        assert_eq!(
            seen,
            [
                (0, 6, 0, 1300),
                (1, 6, 512, 1300),
                (2, 6, 1024, 1300),
                (3, 6, 1300, 1300),
                (4, 6, 1300, 1300)
            ]
        );
        let fake = d.link().transport();
        assert_eq!(fake.installed(), Some(img.as_slice()));
        assert_eq!(fake.status(), PayloadStatus::Running);
        assert_eq!(fake.requests(), 6);
        let i = d.info().unwrap();
        assert_eq!(i.status, PayloadStatus::Running);
        // Status publications made during the upload are surfaced.
        let ev = d.take_events();
        assert!(
            ev.iter()
                .any(|e| matches!(e, Event::Publish { key, .. } if key == "sys/payload/status")),
            "{ev:?}"
        );
    }

    #[test]
    fn dropped_chunk_reply_is_retried_and_the_device_deduplicates() {
        let img = image(1300);
        // Request #3 is the second chunk; its reply is lost.
        let fake = FakeDevice::new("esp32", "0.1.0", 4096, true).with_script(Script {
            drop_reply: vec![3],
            ..Script::default()
        });
        let mut d = device(fake);
        d.upload(&img, UploadOptions::default(), |_| {}).unwrap();
        let fake = d.link().transport();
        assert_eq!(fake.installed(), Some(img.as_slice()));
        assert_eq!(fake.requests(), 6, "one chunk sent twice");
        assert_eq!(fake.status(), PayloadStatus::Stopped);
    }

    #[test]
    fn dropped_request_and_late_reply_are_handled() {
        let img = image(700);
        // Request #1 (begin) never reaches the device, so begin is resent as
        // #2; the reply to #3 (first chunk) arrives late, just before the
        // reply to its resend, and is discarded as stale.
        let fake = FakeDevice::new("esp32", "0.1.0", 4096, true).with_script(Script {
            drop_request: vec![1],
            late_reply: vec![3],
            ..Script::default()
        });
        let mut d = device(fake);
        d.upload(&img, UploadOptions::default(), |_| {}).unwrap();
        let fake = d.link().transport();
        assert_eq!(fake.installed(), Some(img.as_slice()));
        assert_eq!(fake.status(), PayloadStatus::Stopped);
    }

    #[test]
    fn exhausted_retries_time_out() {
        let fake = FakeDevice::new("esp32", "0.1.0", 4096, true).with_script(Script {
            drop_reply: vec![1, 2],
            ..Script::default()
        });
        let mut d = device(fake);
        match d.info() {
            Err(Error::Timeout { key, attempts }) => {
                assert_eq!((key.as_str(), attempts), ("sys/info", 2));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn corrupt_chunk_makes_commit_fail_on_the_device() {
        let img = image(1300);
        let mut d = device(FakeDevice::new("esp8266", "0.1.0+no-wasm", 4096, false));
        let err = d
            .upload(
                &img,
                UploadOptions {
                    corrupt_chunk: Some(1),
                    ..UploadOptions::default()
                },
                |_| {},
            )
            .unwrap_err();
        match &err {
            Error::Upload {
                step, steps, key, ..
            } => assert_eq!((*step, *steps, key.as_str()), (4, 5, "sys/loader/commit")),
            other => panic!("{other:?}"),
        }
        assert_eq!(err.status(), Some(HandlerError::InvalidRequest));
        assert!(err.to_string().contains("CRC-32 mismatch"), "{err}");
        let fake = d.link().transport();
        assert_eq!(fake.installed(), None);
        assert_eq!(fake.status(), PayloadStatus::Empty);
        // The declared CRC is the original image's.
        assert_eq!(fake.declared_crc(), Some(Crc32::checksum(&img)));
        assert_eq!(d.info().unwrap().status, PayloadStatus::Empty);
    }

    #[test]
    fn start_without_runtime_is_a_clear_error() {
        let img = image(100);
        let mut d = device(FakeDevice::new("esp8266", "0.1.0+no-wasm", 4096, false));
        d.upload(&img, UploadOptions::default(), |_| {}).unwrap();
        let err = d.start().unwrap_err();
        assert_eq!(err.status(), Some(HandlerError::NotFound));
        assert!(err.to_string().contains("no WASM runtime"), "{err}");

        let err = d
            .upload(
                &img,
                UploadOptions {
                    start: true,
                    ..UploadOptions::default()
                },
                |_| {},
            )
            .unwrap_err();
        assert!(
            err.to_string()
                .starts_with("upload failed at step 4/4 (sys/payload/start)"),
            "{err}"
        );
        assert_eq!(
            d.link().transport().installed(),
            Some(img.as_slice()),
            "the payload is stored even though it cannot run"
        );
    }

    #[test]
    fn start_and_stop_with_runtime() {
        let mut d = device(FakeDevice::new("esp32s3", "0.1.0", 4096, true));
        assert_eq!(
            d.start().unwrap_err().status(),
            Some(HandlerError::InvalidRequest),
            "nothing installed"
        );
        d.upload(&image(10), UploadOptions::default(), |_| {})
            .unwrap();
        d.start().unwrap();
        assert_eq!(d.link().transport().status(), PayloadStatus::Running);
        d.stop().unwrap();
        assert_eq!(d.link().transport().status(), PayloadStatus::Stopped);
    }

    #[test]
    fn oversized_image_is_refused_at_begin() {
        let mut d = device(FakeDevice::new("esp32", "0.1.0", 64, true));
        let err = d
            .upload(&image(65), UploadOptions::default(), |_| {})
            .unwrap_err();
        assert_eq!(err.status(), Some(HandlerError::BufferTooSmall));
        assert!(err.to_string().contains("capacity"), "{err}");
    }

    #[test]
    fn boot_garbage_before_the_first_reply_needs_no_retry() {
        let fake = FakeDevice::new("esp8266", "0.1.0+no-wasm", 4096, false).with_script(Script {
            // A 74880-baud banner seen at 115200: no delimiter in it.
            preamble: (0..200u32)
                .map(|i| ((i * 97 + 11) % 255 + 1) as u8)
                .collect(),
            ..Script::default()
        });
        let mut d = device(fake);
        assert_eq!(d.info().unwrap().chip, "esp8266");
        assert_eq!(d.link().transport().requests(), 1);
        assert!(
            d.take_events()
                .iter()
                .any(|e| matches!(e, Event::Noise(n) if n.len() == 200))
        );
    }

    fn signed(img: &[u8], version: u32) -> UploadOptions {
        let auth = sign_commit(img, &vector_seed(), Version(version));
        UploadOptions {
            auth,
            ..UploadOptions::default()
        }
    }

    #[test]
    fn trust_info_round_trips_through_claim() {
        let mut d = device(FakeDevice::new("esp32s3", "0.1.0", 4096, true));
        let i = d.trust_info().unwrap();
        assert_eq!(i.state, TrustState::Unclaimed);
        assert_eq!(i.code(), TrustCode::Unclaimed);
        let key = vector_seed().public_key();
        d.trust_claim(key).unwrap();
        let i = d.trust_info().unwrap();
        assert_eq!(
            i.state,
            TrustState::Claimed {
                key,
                min_version: Version(0)
            }
        );
        assert_eq!(i.code(), TrustCode::Claimed);
    }

    #[test]
    fn second_claim_is_denied_with_an_explanation() {
        let mut d = device(FakeDevice::new("esp32s3", "0.1.0", 4096, true));
        d.trust_claim(vector_seed().public_key()).unwrap();
        let err = d
            .trust_claim(crate::sign::Seed::new([42; 32]).public_key())
            .unwrap_err();
        assert_eq!(err.status(), Some(HandlerError::Denied));
        assert!(err.to_string().contains("re-key window"), "{err}");
        // With the window open (BOOT held), the re-key is accepted.
        let fake = FakeDevice::new("esp32s3", "0.1.0", 4096, true)
            .with_claim(vector_seed().public_key())
            .with_rekey_window();
        let mut d = device(fake);
        assert_eq!(d.trust_info().unwrap().code(), TrustCode::RekeyOpen);
        let new = crate::sign::Seed::new([42; 32]).public_key();
        d.trust_claim(new).unwrap();
        let i = d.trust_info().unwrap();
        assert_eq!(i.code(), TrustCode::Claimed, "a claim closes the window");
        assert_eq!(
            i.state,
            TrustState::Claimed {
                key: new,
                min_version: Version(0)
            }
        );
        let err = d.trust_claim(vector_seed().public_key()).unwrap_err();
        assert_eq!(err.status(), Some(HandlerError::Denied));
    }

    #[test]
    fn claim_then_signed_upload_installs_and_raises_min_version() {
        let img = vector_image();
        let mut d = device(FakeDevice::new("esp32s3", "0.1.0", 4096, true));
        d.trust_claim(vector_seed().public_key()).unwrap();
        d.upload(&img, signed(&img, 7), |_| {}).unwrap();
        assert_eq!(d.link().transport().installed(), Some(img.as_slice()));
        match d.trust_info().unwrap().state {
            TrustState::Claimed { min_version, .. } => assert_eq!(min_version, Version(7)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unsigned_upload_to_a_claimed_device_is_denied() {
        let old = image(100);
        let mut d = device(FakeDevice::new("esp32s3", "0.1.0", 4096, true));
        d.upload(&old, UploadOptions::default(), |_| {}).unwrap();
        d.trust_claim(vector_seed().public_key()).unwrap();
        let err = d
            .upload(&image(200), UploadOptions::default(), |_| {})
            .unwrap_err();
        assert_eq!(err.status(), Some(HandlerError::Denied));
        let s = err.to_string();
        assert!(
            s.contains("sys/loader/commit") && s.contains("pinned key"),
            "{s}"
        );
        assert_eq!(
            d.link().transport().installed(),
            Some(old.as_slice()),
            "old image kept"
        );
        assert_eq!(d.info().unwrap().status, PayloadStatus::Stopped);
        // Signed by another key: denied too.
        let other = crate::sign::Seed::new([42; 32]);
        let bad = UploadOptions {
            auth: sign_commit(&image(200), &other, Version(9)),
            ..UploadOptions::default()
        };
        let err = d.upload(&image(200), bad, |_| {}).unwrap_err();
        assert_eq!(err.status(), Some(HandlerError::Denied));
        assert_eq!(d.link().transport().installed(), Some(old.as_slice()));
    }

    #[test]
    fn replayed_or_older_version_is_denied_and_keeps_the_image() {
        let first = vector_image();
        let mut d = device(FakeDevice::new("esp32s3", "0.1.0", 4096, true));
        d.trust_claim(vector_seed().public_key()).unwrap();
        d.upload(&first, signed(&first, 10), |_| {}).unwrap();
        // The same signed upload replayed.
        let err = d.upload(&first, signed(&first, 10), |_| {}).unwrap_err();
        assert_eq!(err.status(), Some(HandlerError::Denied));
        // An older, validly signed image (rollback).
        let older = image(300);
        let err = d.upload(&older, signed(&older, 9), |_| {}).unwrap_err();
        assert_eq!(err.status(), Some(HandlerError::Denied));
        assert_eq!(d.link().transport().installed(), Some(first.as_slice()));
        // A newer one installs.
        d.upload(&older, signed(&older, 11), |_| {}).unwrap();
        assert_eq!(d.link().transport().installed(), Some(older.as_slice()));
    }

    #[test]
    fn unclaimed_device_accepts_signed_and_unsigned_commits() {
        let img = vector_image();
        let mut d = device(FakeDevice::new("esp32s3", "0.1.0", 4096, true));
        d.upload(&img, signed(&img, 3), |_| {}).unwrap();
        d.upload(&img, UploadOptions::default(), |_| {}).unwrap();
        assert_eq!(d.trust_info().unwrap().state, TrustState::Unclaimed);
    }

    #[test]
    fn monitor_yields_publications_then_times_out() {
        let fake = FakeDevice::new("esp32", "0.1.0", 4096, true);
        let mut d = device(fake);
        d.upload(&image(10), UploadOptions::default(), |_| {})
            .unwrap();
        d.take_events();
        d.start().unwrap();
        let deadline = Instant::now() + Duration::from_millis(20);
        let first = d.next_event(deadline).unwrap();
        assert_eq!(
            first,
            Some(Event::Publish {
                key: "sys/payload/status".into(),
                payload: vec![PayloadStatus::Running as u8]
            })
        );
        let mut rest = Vec::new();
        while let Some(e) = d.next_event(deadline).unwrap() {
            rest.push(e);
        }
        assert!(
            rest.iter()
                .all(|e| matches!(e, Event::Publish { key, .. } if key == "sys/log"))
        );
    }
}
