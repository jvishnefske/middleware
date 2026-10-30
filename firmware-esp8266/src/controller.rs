//! The controller: dispatcher state plus the payload store, without a WASM
//! runtime.
//!
//! A synchronous port of the non-`wasm` path of `firmware/src/shell/
//! controller.rs`. All decisions come from [`esp_base::dispatch`]; this
//! module performs the storage effects on an [`esp_base::store::
//! PayloadStore`] and emits the replies and publications through an
//! [`Outbound`] sink (in the shell, the UART [`crate::link::Port`]).
//!
//! * [`Controller::boot`] opens the store over the whole payload region
//!   (region-relative base `0`, [`REGION_SIZE`] bytes split into two
//!   128 KiB slots, images capped at the chip profile's `max_image` of
//!   124 KiB), which selects the newest slot whose image matches its CRC,
//!   verifies it, and starts in `Stopped` if one is valid, else `Empty`. It logs the firmware banner
//!   and the stored image on `sys/log` and publishes the status on
//!   `sys/payload/status`.
//! * [`Controller::on_frame`] answers requests: `sys/info`, the loader
//!   (`begin` erases the inactive slot, `chunk` writes it, `commit`
//!   activates it), so a rejected or interrupted upload leaves the previous
//!   image installed and the status returns to `Stopped`;
//!   `sys/payload/stop` (`Ok`, nothing runs). Signed payloads are not
//!   enabled here (`esp-base` without its `trust` feature): `sys/trust/*`
//!   is `NotFound` and a signature in a commit body is accepted unchecked. `sys/payload/start` is
//!   answered `NotFound` by dispatch because [`Context::runtime`] is
//!   `false`, exactly as on the ESP32-C3 build without `wasm`. Publications
//!   and responses from the host are ignored: there is no payload to
//!   deliver them to.
//! * A failed storage action logs `storage: <error>`, applies
//!   [`State::storage_failed`] (back to `Stopped` if an image is installed,
//!   else `Empty`) and replies `Internal`. A successful
//!   activate logs `payload installed (<n> bytes)`. Every status change is
//!   published.
//!
//! [`FIRMWARE`] is `<CARGO_PKG_VERSION>+no-wasm`, formed like `firmware/`
//! forms its no-runtime version.

use core::fmt::{self, Write as _};

use embedded_storage::nor_flash::NorFlash;
use esp_base::chip::Profile;
use esp_base::dispatch::{Action, Context, Outcome, State, handle_request};
use esp_base::store::{PayloadStore, StoreError};
use middleware::Key;
use middleware::loader::{PayloadStatus, keys};
use middleware::serial::frame::Frame;
use middleware::transport::HandlerError;
use middleware::wasm_abi::MAX_LOG;

use crate::flash::REGION_SIZE;

/// Firmware version reported in `sys/info`; the suffix marks a build
/// without the WASM runtime.
pub const FIRMWARE: &str = concat!(env!("CARGO_PKG_VERSION"), "+no-wasm");

/// Largest reply body (`sys/info` is at most 73 bytes).
pub const MAX_REPLY: usize = 128;

/// Where the controller's replies and publications go.
pub trait Outbound {
    /// Sends the response to request `id`.
    fn respond(&mut self, id: u8, result: Result<&[u8], HandlerError>);
    /// Publishes `payload` on `key`.
    fn publish(&mut self, key: Key<'_>, payload: &[u8]);
}

/// Publishes a formatted `sys/log` line, truncated to [`MAX_LOG`] bytes.
pub fn log(out: &mut impl Outbound, args: fmt::Arguments<'_>) {
    let mut line = Line {
        buf: [0; MAX_LOG],
        len: 0,
    };
    // `Line` never fails; it truncates.
    let _ = line.write_fmt(args);
    out.publish(keys::LOG, line.as_bytes());
}

/// A fixed-size line that drops what does not fit, on a char boundary.
struct Line {
    buf: [u8; MAX_LOG],
    len: usize,
}

impl Line {
    fn as_bytes(&self) -> &[u8] {
        self.buf.get(..self.len).unwrap_or_default()
    }
}

impl fmt::Write for Line {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for c in s.chars() {
            let mut utf8 = [0u8; 4];
            let enc = c.encode_utf8(&mut utf8).as_bytes();
            let end = self.len + enc.len();
            match self.buf.get_mut(self.len..end) {
                Some(dst) => dst.copy_from_slice(enc),
                None => break,
            }
            self.len = end;
        }
        Ok(())
    }
}

/// Owner of the device state and the payload store.
pub struct Controller<F> {
    state: State,
    ctx: Context<'static>,
    store: Option<PayloadStore<F>>,
}

impl<F: NorFlash> Controller<F> {
    /// Opens the store on `flash` (region-relative offsets, see
    /// [`crate::flash`]), verifies the stored image, and announces the
    /// boot on `out`.
    pub fn boot(flash: F, profile: &'static Profile, out: &mut impl Outbound) -> Self {
        let mut store = PayloadStore::new(flash, 0, REGION_SIZE, profile.max_image).ok();
        let installed = store.as_mut().map(PayloadStore::verify);
        let capacity = store.as_ref().map_or(0, PayloadStore::capacity);
        let state = State::boot(matches!(installed, Some(Ok(_))));
        let ctx = Context {
            chip: profile.name,
            firmware: FIRMWARE,
            capacity,
            runtime: profile.runtime,
            rekey_window: false,
            staging: false,
        };
        log(
            out,
            format_args!("esp-base {FIRMWARE} on {} (no WASM runtime)", profile.name),
        );
        match installed {
            None => log(
                out,
                format_args!("payload region unavailable; loader disabled"),
            ),
            Some(Ok(h)) => log(
                out,
                format_args!("stored payload: {} bytes, crc {:08x}", h.len, h.crc),
            ),
            Some(Err(e)) => log(out, format_args!("no valid stored payload: {e}")),
        }
        publish_status(out, state.status());
        Controller { state, ctx, store }
    }

    /// Current dispatcher state.
    pub const fn state(&self) -> State {
        self.state
    }

    /// The facts `sys/info` reports.
    pub const fn context(&self) -> &Context<'static> {
        &self.ctx
    }

    /// Handles one frame from the host.
    pub fn on_frame(&mut self, frame: &Frame<'_>, out: &mut impl Outbound) {
        if let Frame::Request { id, key, payload } = *frame {
            self.request(id, key, payload, out);
        }
    }

    /// Dispatches one request, performs its storage action, and replies.
    fn request(&mut self, id: u8, key: Key<'_>, body: &[u8], out: &mut impl Outbound) {
        let mut reply = [0u8; MAX_REPLY];
        let outcome = handle_request(self.state, &self.ctx, key, body, &mut reply);
        let result = match self.storage(&outcome) {
            Ok(()) => {
                if let Action::Activate { len, .. } = outcome.action {
                    log(out, format_args!("payload installed ({len} bytes)"));
                }
                self.set_state(outcome.state, out);
                // Without a runtime dispatch never asks to start; refuse
                // like the no-`wasm` firmware if it ever does.
                if outcome.action == Action::Start {
                    Err(HandlerError::NotFound)
                } else {
                    outcome.reply
                }
            }
            Err(e) => {
                log(out, format_args!("storage: {e}"));
                self.set_state(self.state.storage_failed(), out);
                Err(HandlerError::Internal)
            }
        };
        let body = result.and_then(|n| reply.get(..n).ok_or(HandlerError::BufferTooSmall));
        out.respond(id, body);
    }

    fn store(&mut self) -> Result<&mut PayloadStore<F>, StoreError> {
        self.store.as_mut().ok_or(StoreError::NoImage)
    }

    fn storage(&mut self, outcome: &Outcome<'_>) -> Result<(), StoreError> {
        match outcome.action {
            Action::Erase { len, .. } => self.store()?.erase(len),
            Action::Write { offset, data } => self.store()?.write(offset, data),
            // Built without `trust`: commits are never checked, so there is
            // never a trust record to persist.
            Action::Activate { len, crc, .. } => self.store()?.activate(len, crc),
            Action::None | Action::Start | Action::Stop => Ok(()),
        }
    }

    fn set_state(&mut self, next: State, out: &mut impl Outbound) {
        if next.status() != self.state.status() {
            publish_status(out, next.status());
        }
        self.state = next;
    }
}

fn publish_status(out: &mut impl Outbound, status: PayloadStatus) {
    out.publish(keys::STATUS, &[status as u8]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flash::ram::RamFlash;
    use crate::flash::{REGION_BASE, RegionFlash, SECTOR};
    use crate::link::fake::FakeSerial;
    use crate::link::{Link, Port, Rx, TxBuffers};
    use esp_base::chip::ESP8266;
    use middleware::crc::Crc32;
    use middleware::loader::{CHUNK_MAX, DeviceInfo};
    use middleware::serial::frame::{MAX_WIRE_FRAME, Splitter};
    use middleware::wasm_abi::ABI_VERSION;

    /// Records what the controller sends.
    #[derive(Default)]
    struct Recorder {
        responses: Vec<(u8, Result<Vec<u8>, HandlerError>)>,
        publications: Vec<(String, Vec<u8>)>,
    }

    impl Outbound for Recorder {
        fn respond(&mut self, id: u8, result: Result<&[u8], HandlerError>) {
            self.responses.push((id, result.map(<[u8]>::to_vec)));
        }
        fn publish(&mut self, key: Key<'_>, payload: &[u8]) {
            self.publications
                .push((key.as_str().to_owned(), payload.to_vec()));
        }
    }

    impl Recorder {
        fn logs(&self) -> Vec<String> {
            self.publications
                .iter()
                .filter(|(k, _)| k == "sys/log")
                .map(|(_, p)| String::from_utf8(p.clone()).unwrap())
                .collect()
        }
        fn statuses(&self) -> Vec<u8> {
            self.publications
                .iter()
                .filter(|(k, _)| k == "sys/payload/status")
                .map(|(_, p)| p[0])
                .collect()
        }
        fn last(&self) -> &(u8, Result<Vec<u8>, HandlerError>) {
            self.responses.last().unwrap()
        }
    }

    type Ram = RegionFlash<RamFlash>;
    /// The controller borrows the RAM flash, so a test can "reboot" by
    /// booting a new controller over the same contents.
    type Ctl<'f> = Controller<&'f mut Ram>;

    fn boot(flash: &mut Ram) -> (Ctl<'_>, Recorder) {
        let mut rec = Recorder::default();
        let ctl = Controller::boot(flash, &ESP8266, &mut rec);
        (ctl, rec)
    }

    fn ram() -> Ram {
        RegionFlash::new(RamFlash::new())
    }

    fn req(
        ctl: &mut Ctl,
        rec: &mut Recorder,
        id: u8,
        key: Key<'static>,
        body: &[u8],
    ) -> Result<Vec<u8>, HandlerError> {
        ctl.on_frame(
            &Frame::Request {
                id,
                key,
                payload: body,
            },
            rec,
        );
        let (got, result) = rec.last().clone();
        assert_eq!(got, id);
        result
    }

    fn info(ctl: &mut Ctl, rec: &mut Recorder) -> (u32, PayloadStatus, String, String) {
        let body = req(ctl, rec, 1, keys::INFO, &[]).unwrap();
        let i = DeviceInfo::decode(&body).unwrap();
        assert_eq!(i.abi_version, ABI_VERSION);
        (
            i.capacity,
            i.status,
            i.chip.to_owned(),
            i.firmware.to_owned(),
        )
    }

    fn begin_body(len: u32, crc: u32) -> [u8; 8] {
        let mut b = [0u8; 8];
        b[..4].copy_from_slice(&len.to_le_bytes());
        b[4..].copy_from_slice(&crc.to_le_bytes());
        b
    }

    /// Uploads `image` in `CHUNK_MAX` chunks with `crc` in the begin.
    fn upload(
        ctl: &mut Ctl,
        rec: &mut Recorder,
        image: &[u8],
        crc: u32,
    ) -> Result<Vec<u8>, HandlerError> {
        req(
            ctl,
            rec,
            2,
            keys::BEGIN,
            &begin_body(image.len() as u32, crc),
        )?;
        for (i, part) in image.chunks(CHUNK_MAX).enumerate() {
            let mut body = ((i * CHUNK_MAX) as u32).to_le_bytes().to_vec();
            body.extend_from_slice(part);
            req(ctl, rec, 3, keys::CHUNK, &body)?;
        }
        req(ctl, rec, 4, keys::COMMIT, &[])
    }

    fn image(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 31 % 251) as u8).collect()
    }

    #[test]
    fn fresh_boot_reports_empty_esp8266_without_runtime() {
        let mut flash = ram();
        let (mut ctl, mut rec) = boot(&mut flash);
        assert!(FIRMWARE.ends_with("+no-wasm"));
        assert!(!ctl.context().runtime);
        assert_eq!(
            info(&mut ctl, &mut rec),
            (
                124 * 1024,
                PayloadStatus::Empty,
                "esp8266".into(),
                FIRMWARE.into()
            )
        );
        assert_eq!(rec.statuses(), [PayloadStatus::Empty as u8]);
        let logs = rec.logs();
        assert!(
            logs[0].contains("esp8266") && logs[0].contains(FIRMWARE),
            "{logs:?}"
        );
        assert!(logs[1].starts_with("no valid stored payload"), "{logs:?}");
    }

    #[test]
    fn upload_installs_and_survives_reboot() {
        let mut flash = ram();
        let (mut ctl, mut rec) = boot(&mut flash);
        let img = image(5003);
        assert_eq!(
            upload(&mut ctl, &mut rec, &img, Crc32::checksum(&img)),
            Ok(vec![])
        );
        assert_eq!(ctl.state().status(), PayloadStatus::Stopped);
        let empty = PayloadStatus::Empty as u8;
        assert_eq!(
            rec.statuses(),
            [
                empty,
                PayloadStatus::Loading as u8,
                PayloadStatus::Stopped as u8
            ]
        );
        assert!(
            rec.logs()
                .iter()
                .any(|l| l == "payload installed (5003 bytes)")
        );

        // Reboot: a new controller over the same flash contents.
        assert_eq!(
            flash.raw().byte(REGION_BASE + SECTOR),
            img[0],
            "image starts after the header sector"
        );
        let (mut ctl, mut rec) = boot(&mut flash);
        assert_eq!(info(&mut ctl, &mut rec).1, PayloadStatus::Stopped);
        assert_eq!(rec.statuses(), [PayloadStatus::Stopped as u8]);
        assert!(rec.logs()[1].starts_with("stored payload: 5003 bytes"));
    }

    #[test]
    fn start_is_refused_and_stop_is_a_no_op() {
        let mut flash = ram();
        let (mut ctl, mut rec) = boot(&mut flash);
        assert_eq!(
            req(&mut ctl, &mut rec, 7, keys::START, &[]),
            Err(HandlerError::NotFound)
        );
        let img = image(100);
        upload(&mut ctl, &mut rec, &img, Crc32::checksum(&img)).unwrap();
        assert_eq!(
            req(&mut ctl, &mut rec, 8, keys::START, &[]),
            Err(HandlerError::NotFound)
        );
        assert_eq!(req(&mut ctl, &mut rec, 9, keys::STOP, &[]), Ok(vec![]));
        assert_eq!(ctl.state().status(), PayloadStatus::Stopped);
    }

    #[test]
    fn wrong_crc_commit_on_empty_flash_leaves_no_image() {
        let mut flash = ram();
        let (mut ctl, mut rec) = boot(&mut flash);
        let img = image(700);
        assert_eq!(
            upload(&mut ctl, &mut rec, &img, 0x1234_5678),
            Err(HandlerError::InvalidRequest)
        );
        assert_eq!(ctl.state().status(), PayloadStatus::Empty);
        let (mut ctl, mut rec) = boot(&mut flash);
        assert_eq!(info(&mut ctl, &mut rec).1, PayloadStatus::Empty);
    }

    #[test]
    fn rejected_upload_keeps_previous_image() {
        let mut flash = ram();
        let (mut ctl, mut rec) = boot(&mut flash);
        let old = image(700);
        upload(&mut ctl, &mut rec, &old, Crc32::checksum(&old)).unwrap();
        // Like `mwctl upload --corrupt-chunk`: the begin CRC does not match
        // the bytes sent, so commit is refused.
        let new: Vec<u8> = image(900).iter().map(|b| b ^ 0x5A).collect();
        assert_eq!(
            upload(&mut ctl, &mut rec, &new, Crc32::checksum(&new) ^ 1),
            Err(HandlerError::InvalidRequest)
        );
        assert_eq!(ctl.state().status(), PayloadStatus::Stopped);
        let stopped = PayloadStatus::Stopped as u8;
        let loading = PayloadStatus::Loading as u8;
        assert_eq!(rec.statuses()[1..], [loading, stopped, loading, stopped]);

        // Reboot: the previous image is still the stored one.
        let (mut ctl, mut rec) = boot(&mut flash);
        assert_eq!(info(&mut ctl, &mut rec).1, PayloadStatus::Stopped);
        assert!(
            rec.logs()[1].starts_with(&format!(
                "stored payload: 700 bytes, crc {:08x}",
                Crc32::checksum(&old)
            )),
            "{:?}",
            rec.logs()
        );
    }

    #[test]
    fn uploads_alternate_slots_and_newest_wins() {
        let mut flash = ram();
        let (mut ctl, mut rec) = boot(&mut flash);
        let first = image(300);
        upload(&mut ctl, &mut rec, &first, Crc32::checksum(&first)).unwrap();
        let second: Vec<u8> = image(400).iter().map(|b| !b).collect();
        upload(&mut ctl, &mut rec, &second, Crc32::checksum(&second)).unwrap();
        const SLOT_B: u32 = REGION_SIZE / 2;
        assert_eq!(
            flash.raw().byte(REGION_BASE + SECTOR),
            first[0],
            "slot A kept"
        );
        assert_eq!(
            flash.raw().byte(REGION_BASE + SLOT_B + SECTOR),
            second[0],
            "second image in slot B"
        );
        let (mut ctl, mut rec) = boot(&mut flash);
        assert_eq!(info(&mut ctl, &mut rec).1, PayloadStatus::Stopped);
        assert!(rec.logs()[1].starts_with("stored payload: 400 bytes"));
        // A corrupt newest slot falls back to the older image.
        flash
            .raw_mut()
            .corrupt(REGION_BASE + SLOT_B + SECTOR + 1, 0);
        let (mut ctl, mut rec) = boot(&mut flash);
        assert_eq!(info(&mut ctl, &mut rec).1, PayloadStatus::Stopped);
        assert!(rec.logs()[1].starts_with("stored payload: 300 bytes"));
    }

    #[test]
    fn corrupted_stored_image_boots_empty() {
        let mut flash = ram();
        let (mut ctl, mut rec) = boot(&mut flash);
        let img = image(64);
        upload(&mut ctl, &mut rec, &img, Crc32::checksum(&img)).unwrap();
        flash.raw_mut().corrupt(REGION_BASE + SECTOR + 10, 0);
        let (mut ctl, mut rec) = boot(&mut flash);
        assert_eq!(info(&mut ctl, &mut rec).1, PayloadStatus::Empty);
        assert!(rec.logs()[1].contains("CRC"), "{:?}", rec.logs());
    }

    #[test]
    fn oversized_and_unknown_requests_are_refused() {
        let mut flash = ram();
        let (mut ctl, mut rec) = boot(&mut flash);
        let too_big = begin_body(124 * 1024 + 1, 0);
        assert_eq!(
            req(&mut ctl, &mut rec, 1, keys::BEGIN, &too_big),
            Err(HandlerError::BufferTooSmall)
        );
        assert_eq!(
            req(&mut ctl, &mut rec, 2, Key::from_static("app/x"), b""),
            Err(HandlerError::NotFound)
        );
        // Publications and responses from the host get no answer.
        ctl.on_frame(
            &Frame::Publish {
                key: Key::from_static("app/x"),
                payload: b"",
            },
            &mut rec,
        );
        ctl.on_frame(
            &Frame::Response {
                id: 5,
                result: Ok(b""),
            },
            &mut rec,
        );
        assert_eq!(rec.responses.len(), 2);
    }

    #[test]
    fn largest_image_fits_a_slot() {
        let mut flash = ram();
        let (mut ctl, mut rec) = boot(&mut flash);
        let img = image(124 * 1024);
        assert_eq!(
            upload(&mut ctl, &mut rec, &img, Crc32::checksum(&img)),
            Ok(vec![])
        );
        let (mut ctl, mut rec) = boot(&mut flash);
        assert_eq!(info(&mut ctl, &mut rec).1, PayloadStatus::Stopped);
    }

    #[test]
    fn device_failure_replies_internal_and_empties() {
        let mut flash = RegionFlash::new(RamFlash::failing());
        let (mut ctl, mut rec) = boot(&mut flash);
        assert_eq!(
            req(&mut ctl, &mut rec, 1, keys::BEGIN, &begin_body(10, 0)),
            Err(HandlerError::Internal)
        );
        assert_eq!(ctl.state().status(), PayloadStatus::Empty);
        assert!(rec.logs().iter().any(|l| l == "storage: flash error"));
    }

    #[test]
    fn log_lines_truncate() {
        let mut rec = Recorder::default();
        let long = "é".repeat(200);
        log(&mut rec, format_args!("{long}"));
        let line = &rec.publications[0].1;
        assert_eq!(line.len(), MAX_LOG);
        assert!(core::str::from_utf8(line).is_ok());
    }

    /// End to end over the fake UART: wire bytes in, wire bytes out.
    #[test]
    fn info_request_over_the_link() {
        let mut bufs = TxBuffers::new();
        let mut link = Link::new(FakeSerial::default());
        let mut splitter: Splitter<MAX_WIRE_FRAME> = Splitter::new();
        let mut ctl = Controller::boot(
            RegionFlash::new(RamFlash::new()),
            &ESP8266,
            &mut Port::new(&mut link, &mut bufs),
        );

        // Host request bytes, encoded the same way the host does.
        let mut host = Link::new(FakeSerial::default());
        host.send(
            &Frame::Request {
                id: 42,
                key: keys::INFO,
                payload: &[],
            },
            &mut TxBuffers::new(),
        )
        .unwrap();
        let mut serial = link.into_inner();
        serial.tx.clear();
        serial.rx.extend(host.into_inner().tx);
        let mut link = Link::new(serial);

        let frame = match link.poll(&mut splitter) {
            Ok(Rx::Frame(Ok(f))) => f,
            other => panic!("{other:?}"),
        };
        ctl.on_frame(&frame, &mut Port::new(&mut link, &mut bufs));
        let mut tx = link.into_inner().tx;
        assert_eq!(tx.pop(), Some(0));
        match middleware::serial::frame::decode_wire(&mut tx).unwrap() {
            Frame::Response {
                id: 42,
                result: Ok(body),
            } => {
                let i = DeviceInfo::decode(body).unwrap();
                assert_eq!(
                    (i.chip, i.firmware, i.status),
                    ("esp8266", FIRMWARE, PayloadStatus::Empty)
                );
            }
            other => panic!("{other:?}"),
        }
    }
}
