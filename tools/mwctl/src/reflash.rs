//! `mwctl reflash`: an application image sent to the nRF52 bootloader over
//! its VCOM serial port in the MWFW wire format.
//!
//! This is not the middleware serial/COBS protocol and not the A/B
//! `fw` updater: the bootloader has one application slot. The device must
//! already be in reflash mode (button A held while reset is pressed); this
//! module never pulses reset.
//!
//! The 60-byte header is the persisted [`middleware::fw_ota::FwRecord`]
//! encoding (`src/fw_ota/record.rs`) with sequence number 0, flag "pending"
//! and slot A: the tests check [`header`] against that encoder.
//!
//! Exchange: header, then one reply byte 0x06 (ready: the application pages
//! are erased) within [`READY_WAIT`]; then the body in [`BODY_CHUNK`]-byte
//! writes; then one reply byte 0x06 (done: the image is on flash and
//! verified) within [`DONE_WAIT`]. 0x15 rejects (bad header, or verification
//! failed), 0x18 is a timeout on the device's side.

use crate::link::Transport;
use middleware::crc::Crc32;
use std::time::{Duration, Instant};

/// Largest application image the bootloader accepts (bytes).
pub const APP_MAX: u32 = 0x6A000;
/// Bytes per body write.
pub const BODY_CHUNK: usize = 256;
/// Encoded header length.
pub const HEADER_LEN: usize = 60;
/// How long to wait for the ready reply after the header.
pub const READY_WAIT: Duration = Duration::from_secs(60);
/// How long to wait for the done reply after the body.
pub const DONE_WAIT: Duration = Duration::from_secs(120);

const MAGIC: &[u8; 4] = b"MWFW";
const FORMAT: u8 = 1;
const FLAG_PENDING: u8 = 1;
const ACK: u8 = 0x06;
const NAK: u8 = 0x15;
const CAN: u8 = 0x18;

const NO_REPLY: &str =
    "no reply from the bootloader; hold button A and reset the board, then retry";

/// Builds the 60-byte header for `image` with `version` (also the
/// anti-rollback floor, `min_version`). Refuses an empty image and one over
/// [`APP_MAX`] before anything is sent.
pub fn header(image: &[u8], version: u32) -> Result<[u8; HEADER_LEN], String> {
    if image.is_empty() {
        return Err("the image is empty".into());
    }
    let len = u32::try_from(image.len())
        .ok()
        .filter(|&n| n <= APP_MAX)
        .ok_or_else(|| {
            format!(
                "the image is {} bytes; the application slot holds at most {APP_MAX} (0x{APP_MAX:X})",
                image.len()
            )
        })?;
    let mut h = [0u8; HEADER_LEN];
    h[..4].copy_from_slice(MAGIC);
    h[4] = FORMAT;
    h[5] = FLAG_PENDING;
    // 6: slot A (0), 7: zero, 8..12: sequence 0.
    h[12..16].copy_from_slice(&version.to_le_bytes());
    h[16..20].copy_from_slice(&version.to_le_bytes());
    h[20..24].copy_from_slice(&len.to_le_bytes());
    h[24..56].copy_from_slice(&crate::sign::image_sha256(image));
    let crc = Crc32::checksum(&h[..56]);
    h[56..60].copy_from_slice(&crc.to_le_bytes());
    Ok(h)
}

/// One reply byte, read against the stage it answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    /// 0x06 after the header: the application pages are erased.
    Ready,
    /// 0x06 after the body: the image is on flash and verified.
    Done,
    /// 0x15: rejected.
    Nak,
    /// 0x18: timeout on the device's side.
    Can,
    /// Any other byte.
    Other(u8),
}

/// Classifies `byte`; `after_body` selects whether 0x06 means ready (after
/// the header) or done (after the body).
pub fn classify(byte: u8, after_body: bool) -> Reply {
    match byte {
        ACK if after_body => Reply::Done,
        ACK => Reply::Ready,
        NAK => Reply::Nak,
        CAN => Reply::Can,
        other => Reply::Other(other),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    AfterHeader,
    AfterBody,
}

/// The error for a reply (or none) at `stage`.
fn check(reply: Option<u8>, stage: Stage) -> Result<(), String> {
    let Some(byte) = reply else {
        return Err(NO_REPLY.into());
    };
    let after_body = stage == Stage::AfterBody;
    match (classify(byte, after_body), stage) {
        (Reply::Ready | Reply::Done, _) => Ok(()),
        (Reply::Nak, Stage::AfterHeader) => {
            Err("the bootloader rejected the header (0x15): bad header".into())
        }
        (Reply::Nak, Stage::AfterBody) => Err(
            "the bootloader rejected the image (0x15): verification failed (length or SHA-256)"
                .into(),
        ),
        (Reply::Can, Stage::AfterHeader) => {
            Err("the bootloader timed out on the device (0x18) before the image was sent".into())
        }
        (Reply::Can, Stage::AfterBody) => {
            Err("the bootloader timed out on the device (0x18) after the image was sent".into())
        }
        (Reply::Other(x), Stage::AfterHeader) => Err(format!(
            "unexpected reply 0x{x:02X} from the bootloader (after the header)"
        )),
        (Reply::Other(x), Stage::AfterBody) => Err(format!(
            "unexpected reply 0x{x:02X} from the bootloader (after the image)"
        )),
    }
}

fn write<T: Transport>(t: &mut T, bytes: &[u8]) -> Result<(), String> {
    t.write_all(bytes).map_err(|e| format!("serial port: {e}"))
}

/// Waits at most `wait` for one reply byte; `None` on timeout.
fn await_reply<T: Transport>(t: &mut T, wait: Duration) -> Result<Option<u8>, String> {
    let deadline = Instant::now() + wait;
    let mut one = [0u8; 1];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(None);
        }
        let n = t
            .read(&mut one, left)
            .map_err(|e| format!("serial port: {e}"))?;
        if n > 0 {
            return Ok(one.first().copied());
        }
    }
}

/// Sends `image` to the bootloader as version `version` and waits until it
/// reports the image verified. `progress` gets the bytes of the body sent
/// so far after each write.
pub fn reflash<T: Transport>(
    t: &mut T,
    image: &[u8],
    version: u32,
    progress: impl FnMut(usize),
) -> Result<(), String> {
    reflash_with(t, image, version, (READY_WAIT, DONE_WAIT), progress)
}

fn reflash_with<T: Transport>(
    t: &mut T,
    image: &[u8],
    version: u32,
    waits: (Duration, Duration),
    mut progress: impl FnMut(usize),
) -> Result<(), String> {
    let head = header(image, version)?;
    write(t, &head)?;
    check(await_reply(t, waits.0)?, Stage::AfterHeader)?;
    let mut sent = 0;
    for chunk in image.chunks(BODY_CHUNK) {
        write(t, chunk)?;
        sent += chunk.len();
        progress(sent);
    }
    check(await_reply(t, waits.1)?, Stage::AfterBody)
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::fw_ota::Slot;
    use middleware::fw_ota::record::{FwRecord, Pending};
    use middleware::loader::trust::Version;
    use std::collections::VecDeque;
    use std::io;

    /// What the fake bootloader does.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Script {
        Happy,
        /// 0x15 instead of ready.
        NakHeader,
        /// No reply to the header at all.
        Silent,
        /// 0x15 after the body, as a failed verification.
        NakBody,
        /// 0x18 after the body.
        CanBody,
        /// 0x15 once this many body bytes have arrived (replies early).
        EarlyNak(usize),
    }

    /// An in-memory bootloader: decodes the header with the middleware's
    /// record decoder, replies as the protocol says, and keeps what it
    /// would have written to flash.
    struct Bootloader {
        script: Script,
        /// Every byte the host wrote.
        written: Vec<u8>,
        inbox: Vec<u8>,
        expect: Option<(usize, [u8; 32])>,
        body: Vec<u8>,
        dead: bool,
        replied: bool,
        replies: VecDeque<u8>,
        flash: Option<Vec<u8>>,
    }

    impl Bootloader {
        fn new(script: Script) -> Self {
            Bootloader {
                script,
                written: Vec::new(),
                inbox: Vec::new(),
                expect: None,
                body: Vec::new(),
                dead: false,
                replied: false,
                replies: VecDeque::new(),
                flash: None,
            }
        }

        fn step(&mut self) {
            if self.dead {
                return;
            }
            if self.expect.is_none() {
                if self.inbox.len() < HEADER_LEN {
                    return;
                }
                let head: Vec<u8> = self.inbox.drain(..HEADER_LEN).collect();
                let decoded = FwRecord::decode(&head).and_then(|(_, r)| r.pending);
                let Some(p) = decoded.filter(|p| p.len >= 1 && p.len <= APP_MAX) else {
                    self.replies.push_back(NAK);
                    self.dead = true;
                    return;
                };
                if self.script == Script::NakHeader {
                    self.replies.push_back(NAK);
                    self.dead = true;
                    return;
                }
                if self.script == Script::Silent {
                    self.dead = true;
                    return;
                }
                self.expect = Some((p.len as usize, p.sha256));
                self.replies.push_back(ACK);
            }
            let Some((len, sha)) = self.expect else {
                return;
            };
            self.body.append(&mut self.inbox);
            if matches!(self.script, Script::EarlyNak(n) if self.body.len() >= n) {
                self.replies.push_back(NAK);
                self.dead = true;
                return;
            }
            if self.body.len() >= len && !self.replied {
                self.replied = true;
                self.body.truncate(len);
                let reply = match self.script {
                    Script::NakBody => NAK,
                    Script::CanBody => CAN,
                    _ if crate::sign::image_sha256(&self.body) == sha => {
                        self.flash = Some(self.body.clone());
                        ACK
                    }
                    _ => NAK,
                };
                self.replies.push_back(reply);
            }
        }
    }

    impl Transport for Bootloader {
        fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.written.extend_from_slice(bytes);
            self.inbox.extend_from_slice(bytes);
            self.step();
            Ok(())
        }

        fn read(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
            if let Some(b) = self.replies.pop_front() {
                if let Some(slot) = buf.first_mut() {
                    *slot = b;
                    return Ok(1);
                }
                return Ok(0);
            }
            // Behave like a port that waited and heard nothing, without
            // making the test wait for the real timeout.
            std::thread::sleep(timeout.min(Duration::from_millis(2)));
            Ok(0)
        }
    }

    const SHORT: (Duration, Duration) = (Duration::from_millis(60), Duration::from_millis(60));

    fn image(len: usize) -> Vec<u8> {
        (0..len).map(|i| ((i * 7 + i / 251) % 256) as u8).collect()
    }

    fn run(script: Script, img: &[u8]) -> (Bootloader, Result<(), String>) {
        let mut dev = Bootloader::new(script);
        let r = reflash_with(&mut dev, img, 3, SHORT, |_| {});
        (dev, r)
    }

    #[test]
    fn happy_path_writes_the_image_and_reports_progress() {
        let img = image(5000);
        let mut dev = Bootloader::new(Script::Happy);
        let mut seen = Vec::new();
        reflash_with(&mut dev, &img, 3, SHORT, |n| seen.push(n)).unwrap();
        assert_eq!(dev.flash.as_deref(), Some(img.as_slice()));
        assert_eq!(seen.last().copied(), Some(5000));
        assert!(seen.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(seen.len(), 5000usize.div_ceil(BODY_CHUNK));
        assert_eq!(dev.written.len(), HEADER_LEN + 5000);
    }

    #[test]
    fn empty_image_is_refused_without_writing() {
        let (dev, r) = run(Script::Happy, &[]);
        assert!(r.unwrap_err().contains("empty"));
        assert!(dev.written.is_empty());
        assert!(header(&[], 0).is_err());
    }

    #[test]
    fn image_over_the_slot_is_refused_without_writing() {
        let big = vec![0u8; APP_MAX as usize + 1];
        let (dev, r) = run(Script::Happy, &big);
        assert!(r.unwrap_err().contains("at most"));
        assert!(dev.written.is_empty());
        assert!(header(&big, 0).is_err());
        assert!(header(&vec![0u8; APP_MAX as usize], 0).is_ok());
    }

    #[test]
    fn nak_after_the_header_is_an_error() {
        let (dev, r) = run(Script::NakHeader, &image(100));
        let e = r.unwrap_err();
        assert!(e.contains("0x15") && e.contains("header"), "{e}");
        assert_eq!(dev.written.len(), HEADER_LEN);
    }

    #[test]
    fn nak_after_the_body_is_an_error() {
        let (dev, r) = run(Script::NakBody, &image(5000));
        let e = r.unwrap_err();
        assert!(
            e.contains("0x15") && e.contains("verification failed"),
            "{e}"
        );
        assert!(dev.flash.is_none());
    }

    #[test]
    fn device_timeout_after_the_body_is_an_error() {
        let (_, r) = run(Script::CanBody, &image(5000));
        let e = r.unwrap_err();
        assert!(e.contains("0x18"), "{e}");
    }

    #[test]
    fn no_reply_is_an_error_with_the_button_a_instruction() {
        let (_, r) = run(Script::Silent, &image(100));
        let e = r.unwrap_err();
        assert!(e.contains("no reply") && e.contains("button A"), "{e}");
    }

    #[test]
    fn early_reply_during_the_body_is_reported() {
        let img = image(5000);
        let (dev, r) = run(Script::EarlyNak(1000), &img);
        assert!(r.unwrap_err().contains("0x15"));
        // The host did not stop writing; the device stopped listening.
        assert_eq!(dev.written.len(), HEADER_LEN + 5000);
        assert!(dev.flash.is_none());
    }

    #[test]
    fn unexpected_reply_byte_is_shown_in_hex() {
        assert_eq!(classify(0x41, false), Reply::Other(0x41));
        assert_eq!(classify(0x41, true), Reply::Other(0x41));
        let e = check(Some(0x41), Stage::AfterBody).unwrap_err();
        assert!(e.contains("0x41"), "{e}");
    }

    #[test]
    fn classifier_reads_ack_by_stage() {
        assert_eq!(classify(0x06, false), Reply::Ready);
        assert_eq!(classify(0x06, true), Reply::Done);
        assert_eq!(classify(0x15, false), Reply::Nak);
        assert_eq!(classify(0x15, true), Reply::Nak);
        assert_eq!(classify(0x18, true), Reply::Can);
    }

    #[test]
    fn header_is_the_middleware_record_encoding() {
        for (v, len) in [(0u32, 5000usize), (0x0001_0002, 1), (7, APP_MAX as usize)] {
            let img = image(len);
            let h = header(&img, v).unwrap();
            let record = FwRecord {
                min_version: Version(v),
                pending: Some(Pending {
                    slot: Slot::A,
                    version: Version(v),
                    len: len as u32,
                    sha256: crate::sign::image_sha256(&img),
                    rebooted: false,
                }),
            };
            assert_eq!(h, record.encode(0), "version {v}, len {len}");
        }
    }
}
