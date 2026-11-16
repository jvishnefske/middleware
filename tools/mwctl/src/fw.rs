//! A/B firmware update (`mwctl fw ...`) for the RP2350 and the i.MX
//! RT1050, pure parts and protocol operations.
//!
//! * [`load_image`] — the image file: an **RT1050 slot image** (starts
//!   with `MWRT`, as `imxrt-image` writes it: the 256-byte slot header,
//!   then the image; [`middleware::fw_ota::rt_image`]), a **UF2** (as `picotool` or
//!   `elf2uf2` write it; blocks for the RP2350 families, reassembled into
//!   the flat image from the lowest target address, gaps `0xFF`) or a
//!   **raw binary** (`objcopy -O binary` of the ELF linked at the flash
//!   base). An ELF is refused with the commands that convert it. The
//!   version is read from the image's IMAGE_DEF
//!   ([`middleware::fw_ota::picobin`]); the image must be an RP2350
//!   executable flagged TBYB, without a rollback version, with a block in
//!   its first sector's last page (the device's rules,
//!   [`middleware::fw_ota::machine::acceptable`]), so a bad image is
//!   refused before anything is sent. `--version` is a cross-check, not an
//!   override: the device installs an image only as the version its
//!   IMAGE_DEF carries, so the bootrom's A/B choice and the anti-rollback
//!   floor agree. An RT slot image is sent whole, header first (the
//!   device's `begin` length and SHA-256 cover both); its header must
//!   decode (magic, header version 1, zero reserved bytes), its length
//!   must be exactly the file's and its SHA-256 must match, and its
//!   version is the header's. The picobin checks do not apply to it.
//! * [`parse_version`] / [`format_version`] — `MAJOR.MINOR` (the protocol
//!   version is `major << 16 | minor`).
//! * [`Device::fw_info`], [`Device::fw_update`], [`Device::fw_confirm`],
//!   [`Device::fw_abort`], [`Device::fw_reboot`] — the `sys/fw/*`
//!   requests. An update sends `begin` (the device answers the target
//!   slot), every chunk in order with progress, a commit signed for that
//!   slot and the device's node id (with a key), and optionally `reboot`.
//!   Chunk resends after a timeout are harmless (the device acknowledges a
//!   chunk it has without writing it again).
//!
//! # Waiting for flash erases (RT1050)
//!
//! The RT1050's HyperFlash erases 256 KiB blocks, each taking up to
//! [`RT_BLOCK_ERASE`] (4 s), during which the device does not answer
//! (`firmware-imxrt/src/ab/platform.rs`: each block of the target slot is
//! erased once, before its first program; a boot-record save erases one of
//! the two record blocks). Per request, at most:
//!
//! | request  | erases | per-attempt timeout ([`crate::link::Timeouts`]) | kept alive |
//! |----------|--------|-------------------------------------------------|------------|
//! | `begin`  | 1 (block 0) | 5 s floor | [`erase_wait`]`(1)` = 5 s (RT image) |
//! | `chunk`  | 1 (the first chunk of every other 256 KiB block) | `--timeout` (1 s) | [`erase_wait`]`(1)` = 5 s (RT image) |
//! | `commit` | 1 (the record) + hashing the image | 15 s | — |
//! | `confirm`| 1 (the record) | 5 s floor | — |
//! | `abort`  | 2 (the record, block 0) | `--timeout` (1 s) | [`erase_wait`]`(2)` = 9 s |
//! | `reboot` | 1 (the record, normally already saved) | `--timeout` | — (an unanswered reboot is accepted) |
//!
//! "Kept alive": when the link gives up on such a request (every attempt
//! timed out), it is sent again until that long has passed since the first
//! send, so a request blocked by an erase is answered to a later resend.
//! Every `sys/fw/*` request is idempotent on resend, so resending one that
//! the device is still working on is harmless. The RP2350 path is
//! unchanged except `abort`, which is not image-specific (its 4 KiB sector
//! erase never needs the wait; only an unresponsive device takes longer to
//! report).

use crate::device::{Device, Progress};
use crate::error::Error;
use crate::link::Endpoint;
use crate::sign::{Seed, image_sha256};
use ed25519_dalek::{Signer, SigningKey};
use middleware::fw_ota::picobin::{ImageDef, SENTINEL_FROM, find_image_def_in};
use middleware::fw_ota::rt_image::{HEADER_LEN, RtHeader, RtImageError, is_rt_image};
use middleware::fw_ota::wire::{Command, FwInfo, StateCode, keys, signed_message};
use middleware::fw_ota::{CHUNK_LEN, MAX_IMAGE, Slot};
use middleware::loader::trust::{Signature, Version};
use middleware::transport::HandlerError;
use sha2::{Digest, Sha256};
use std::fmt;
use std::time::{Duration, Instant};

/// UF2 first magic.
pub const UF2_MAGIC_START0: u32 = 0x0A32_4655;
/// UF2 second magic.
pub const UF2_MAGIC_START1: u32 = 0x9E5D_5157;
/// UF2 end magic.
pub const UF2_MAGIC_END: u32 = 0x0AB1_6F30;
/// UF2 flag: not for main flash.
pub const UF2_FLAG_NOT_MAIN_FLASH: u32 = 0x0000_0001;
/// UF2 flag: the family id field is set.
pub const UF2_FLAG_FAMILY_ID: u32 = 0x0000_2000;
/// RP2040 family (the Pico 1: no A/B support in its bootrom).
pub const FAMILY_RP2040: u32 = 0xe48b_ff56;
/// Families accepted: absolute, RP2350 Arm secure, RISC-V, Arm non-secure.
pub const FAMILIES_RP2350: [u32; 4] = [0xe48b_ff57, 0xe48b_ff59, 0xe48b_ff5a, 0xe48b_ff5b];
/// The RP2350's XIP flash base, where images are linked.
pub const FLASH_BASE: u32 = 0x1000_0000;
/// Longest one 256 KiB block erase of the RT1050's HyperFlash (S26KS512S)
/// blocks a request.
pub const RT_BLOCK_ERASE: Duration = Duration::from_secs(4);
/// Slack on top of the erases in [`erase_wait`].
pub const ERASE_SLACK: Duration = Duration::from_secs(1);

/// How long a request that may erase `blocks` RT flash blocks is kept
/// alive (see the module docs): `blocks` × [`RT_BLOCK_ERASE`] +
/// [`ERASE_SLACK`].
pub fn erase_wait(blocks: u32) -> Duration {
    RT_BLOCK_ERASE * blocks + ERASE_SLACK
}

/// Why an image file cannot be installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageError {
    /// The file is empty.
    Empty,
    /// The image exceeds what the protocol carries.
    TooLarge(usize),
    /// An ELF file was given.
    Elf,
    /// The UF2 is malformed.
    Uf2(String),
    /// The UF2 is for the RP2040.
    Rp2040,
    /// No IMAGE_DEF block loop in the first 4 KiB.
    NoImageDef,
    /// The IMAGE_DEF is not an RP2350 executable.
    NotRp2350,
    /// The IMAGE_DEF is not flagged try-before-you-buy.
    NotTbyb,
    /// The IMAGE_DEF has no VERSION item.
    NoVersion,
    /// The IMAGE_DEF carries a rollback version.
    Rollback,
    /// The block loop has no block in the first sector's last page.
    NoSentinel,
    /// The RT1050 slot image is not acceptable.
    Rt(RtImageError),
    /// `--version` differs from the image's.
    VersionMismatch {
        /// Given with `--version`.
        expected: Version,
        /// In the image.
        image: Version,
    },
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageError::Empty => f.write_str("the firmware image is empty"),
            ImageError::TooLarge(n) => write!(f, "the firmware image is too large ({n} bytes, at most {MAX_IMAGE})"),
            ImageError::Elf => f.write_str(
                "this is an ELF file; give a UF2 (`picotool uf2 convert fw.elf fw.uf2 --family rp2350-arm-s`) \
                 or a raw binary (`arm-none-eabi-objcopy -O binary fw.elf fw.bin`)",
            ),
            ImageError::Uf2(why) => write!(f, "malformed UF2: {why}"),
            ImageError::Rp2040 => f.write_str(
                "this UF2 is for the RP2040 (Pico 1), whose bootrom has no A/B partitions; \
                 firmware updates need an RP2350 (Pico 2)",
            ),
            ImageError::NoImageDef => {
                f.write_str("no IMAGE_DEF metadata block in the image's first 4 KiB (not an RP2350 image?)")
            }
            ImageError::NotRp2350 => f.write_str("the IMAGE_DEF is not an RP2350 executable"),
            ImageError::NotTbyb => f.write_str(
                "the image is not flagged try-before-you-buy (TBYB): the device would have no way back if it \
                 failed; set the TBYB bit (0x8000) in its IMAGE_TYPE item",
            ),
            ImageError::NoVersion => f.write_str(
                "the IMAGE_DEF has no VERSION item: give the image a major.minor version above the running one",
            ),
            ImageError::Rollback => {
                f.write_str("the IMAGE_DEF carries an OTP rollback version, which this updater does not handle")
            }
            ImageError::NoSentinel => write!(
                f,
                "the image's metadata block loop has no block in the first sector's last page \
                 (offset {SENTINEL_FROM:#x}..0x1000): a power cut during the bootrom's buy could leave \
                 a torn image bootable; add the sentinel block (middleware::fw_ota::picobin::sentinel_block)"
            ),
            ImageError::Rt(e) => {
                write!(f, "RT1050 slot image (MWRT): {}", e.as_str())?;
                match e {
                    RtImageError::LengthMismatch { header, actual } => write!(
                        f,
                        " (the header says {header} bytes with the header, the file has {actual}; \
                         give the file `imxrt-image` wrote, unchanged)"
                    ),
                    RtImageError::TooLarge { total, max } => write!(f, " ({total} bytes, at most {max})"),
                    RtImageError::HashMismatch | RtImageError::BadHeaderVersion => {
                        f.write_str(" (damaged, or written by another version of `imxrt-image`)")
                    }
                    _ => Ok(()),
                }
            }
            ImageError::VersionMismatch { expected, image } => write!(
                f,
                "--version {} but the image says {}",
                format_version(*expected),
                format_version(*image)
            ),
        }
    }
}

impl std::error::Error for ImageError {}

/// What kind of image a [`FwImage`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    /// An RP2350 image, with its IMAGE_DEF.
    Picobin(ImageDef),
    /// An i.MX RT1050 slot image, with its decoded header.
    Rt(RtHeader),
}

/// A firmware image ready to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FwImage {
    /// The flat image, as it lands at the start of a partition (an RT
    /// image: header and image, the slot's contents).
    pub bytes: Vec<u8>,
    /// What it is.
    pub kind: ImageKind,
    /// Its version (`major << 16 | minor`).
    pub version: Version,
    /// Whether it came from a UF2.
    pub from_uf2: bool,
}

impl FwImage {
    /// Whether this is an RT1050 slot image.
    pub fn is_rt(&self) -> bool {
        matches!(self.kind, ImageKind::Rt(_))
    }
}

/// `MAJOR.MINOR` (each 0..=65535) as `major << 16 | minor`.
pub fn parse_version(s: &str) -> Option<Version> {
    let (major, minor) = s.split_once('.')?;
    let major: u16 = major.parse().ok()?;
    let minor: u16 = minor.parse().ok()?;
    Some(Version(u32::from(major) << 16 | u32::from(minor)))
}

/// `major.minor` of a protocol version.
pub fn format_version(v: Version) -> String {
    format!("{}.{}", v.0 >> 16, v.0 & 0xffff)
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4)
        .map_or(0, |w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
}

/// Whether `bytes` start like a UF2 block.
pub fn is_uf2(bytes: &[u8]) -> bool {
    u32_at(bytes, 0) == UF2_MAGIC_START0 && u32_at(bytes, 4) == UF2_MAGIC_START1
}

/// Reassembles a UF2's main-flash blocks into a flat image starting at the
/// lowest target address (which must be in flash), gaps filled with `0xFF`.
pub fn uf2_to_bin(bytes: &[u8]) -> Result<Vec<u8>, ImageError> {
    if bytes.is_empty() || !bytes.len().is_multiple_of(512) {
        return Err(ImageError::Uf2(format!(
            "{} bytes is not a whole number of 512-byte blocks",
            bytes.len()
        )));
    }
    let mut parts: Vec<(u32, &[u8])> = Vec::new();
    for (n, b) in bytes.chunks(512).enumerate() {
        if !is_uf2(b) || u32_at(b, 508) != UF2_MAGIC_END {
            return Err(ImageError::Uf2(format!("block {n}: bad magic")));
        }
        let flags = u32_at(b, 8);
        if flags & UF2_FLAG_NOT_MAIN_FLASH != 0 {
            continue;
        }
        if flags & UF2_FLAG_FAMILY_ID != 0 {
            let family = u32_at(b, 28);
            if family == FAMILY_RP2040 {
                return Err(ImageError::Rp2040);
            }
            if !FAMILIES_RP2350.contains(&family) {
                return Err(ImageError::Uf2(format!(
                    "block {n}: family {family:#010x} is not an RP2350 family"
                )));
            }
        }
        let addr = u32_at(b, 12);
        let size = u32_at(b, 16) as usize;
        if size > 476 {
            return Err(ImageError::Uf2(format!(
                "block {n}: payload of {size} bytes"
            )));
        }
        parts.push((addr, &b[32..32 + size]));
    }
    let base = parts.iter().map(|p| p.0).min().ok_or(ImageError::Empty)?;
    if base < FLASH_BASE {
        return Err(ImageError::Uf2(format!(
            "target address {base:#010x} is not in flash"
        )));
    }
    let end = parts
        .iter()
        .map(|p| u64::from(p.0) + p.1.len() as u64)
        .max()
        .unwrap_or(u64::from(base));
    let len = (end - u64::from(base)) as usize;
    if len > MAX_IMAGE as usize {
        return Err(ImageError::TooLarge(len));
    }
    let mut image = vec![0xFFu8; len];
    for (addr, data) in parts {
        let at = (addr - base) as usize;
        image[at..at + data.len()].copy_from_slice(data);
    }
    Ok(image)
}

/// Checks an RT1050 slot image (header and image) as the device will:
/// the header decodes, its length is exactly `file`'s (and at most
/// [`MAX_IMAGE`]), and the SHA-256 over its fields and the image matches.
pub fn load_rt_image(file: &[u8]) -> Result<RtHeader, RtImageError> {
    let header = RtHeader::decode(file)?;
    header.check_len(file.len(), MAX_IMAGE)?;
    let image = file.get(HEADER_LEN..).unwrap_or_default();
    let digest: [u8; 32] = Sha256::new()
        .chain_update(header.hashed_fields())
        .chain_update(image)
        .finalize()
        .into();
    header.check_digest(&digest)?;
    Ok(header)
}

fn check_expected(expected: Option<Version>, version: Version) -> Result<(), ImageError> {
    match expected.filter(|e| *e != version) {
        Some(e) => Err(ImageError::VersionMismatch {
            expected: e,
            image: version,
        }),
        None => Ok(()),
    }
}

/// Reads an image file's bytes into a [`FwImage`], checking it as the
/// device will (see the module docs).
pub fn load_image(file: &[u8], expected: Option<Version>) -> Result<FwImage, ImageError> {
    if file.is_empty() {
        return Err(ImageError::Empty);
    }
    if is_rt_image(file) {
        let header = load_rt_image(file).map_err(ImageError::Rt)?;
        check_expected(expected, header.version)?;
        return Ok(FwImage {
            bytes: file.to_vec(),
            kind: ImageKind::Rt(header),
            version: header.version,
            from_uf2: false,
        });
    }
    if file.starts_with(b"\x7fELF") {
        return Err(ImageError::Elf);
    }
    let from_uf2 = is_uf2(file);
    let bytes = if from_uf2 {
        uf2_to_bin(file)?
    } else {
        file.to_vec()
    };
    if bytes.len() > MAX_IMAGE as usize {
        return Err(ImageError::TooLarge(bytes.len()));
    }
    let def = find_image_def_in(&bytes).ok_or(ImageError::NoImageDef)?;
    if !def.is_exe() || !def.is_rp2350() {
        return Err(ImageError::NotRp2350);
    }
    let v = def.version.ok_or(ImageError::NoVersion)?;
    if v.rollback.is_some() {
        return Err(ImageError::Rollback);
    }
    if !def.is_tbyb() {
        return Err(ImageError::NotTbyb);
    }
    if def.window_tail < SENTINEL_FROM {
        return Err(ImageError::NoSentinel);
    }
    let version = Version(v.as_u32());
    check_expected(expected, version)?;
    Ok(FwImage {
        bytes,
        kind: ImageKind::Picobin(def),
        version,
        from_uf2,
    })
}

/// Signs a firmware commit: Ed25519 over the device's 64-byte
/// [`signed_message`] for this image, version, slot and node.
pub fn sign_firmware(
    image: &[u8],
    seed: &Seed,
    version: Version,
    slot: Slot,
    node: &middleware::SenderId,
) -> Option<Signature> {
    let len = u32::try_from(image.len()).ok()?;
    let m = signed_message(len, version, slot, node, &image_sha256(image));
    Some(Signature(
        SigningKey::from_bytes(seed.bytes()).sign(&m).to_bytes(),
    ))
}

/// Settings of [`Device::fw_update`].
#[derive(Debug, Clone, Copy, Default)]
pub struct UpdateOptions<'a> {
    /// Sign the commit with this key (a claimed device requires it).
    pub key: Option<&'a Seed>,
    /// Send `sys/fw/reboot` after the commit.
    pub reboot: bool,
}

/// How an update ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Updated {
    /// The slot the image was installed in.
    pub slot: Slot,
    /// Whether the reboot was requested.
    pub rebooted: bool,
    /// The reboot got no reply (the device may have reset first).
    pub reboot_unanswered: bool,
}

impl<L: Endpoint> Device<L> {
    fn fw(&mut self, cmd: &Command<'_>) -> Result<Vec<u8>, Error> {
        let mut body = [0u8; 4 + CHUNK_LEN];
        let n = cmd.encode_body(&mut body).ok_or(Error::Frame(
            middleware::serial::frame::FrameError::TooLarge,
        ))?;
        self.link_mut()
            .request(cmd.key(), body.get(..n).unwrap_or_default())
    }

    /// [`Device::fw`], sent again while the link times out until `wait`
    /// has passed since the first send (a request the device answers only
    /// after flash erases; see the module docs). `Duration::ZERO` sends it
    /// once, as [`Device::fw`].
    fn fw_patient(&mut self, cmd: &Command<'_>, wait: Duration) -> Result<Vec<u8>, Error> {
        let first = Instant::now();
        // Bounded by `wait`: each pass is a full request (every attempt
        // timed out).
        loop {
            match self.fw(cmd) {
                Err(Error::Timeout { .. }) if first.elapsed() < wait => {}
                other => return other,
            }
        }
    }

    /// `sys/fw/info`.
    pub fn fw_info(&mut self) -> Result<FwInfo, Error> {
        let body = self.fw(&Command::Info)?;
        FwInfo::decode(&body).ok_or_else(|| Error::Malformed {
            key: keys::INFO.as_str().to_owned(),
        })
    }

    /// Installs `image` in the inactive slot and, with `opts.reboot`,
    /// reboots into it on trial. Refuses locally (before sending anything)
    /// when an update is committed or on trial, or the version does not
    /// exceed the device's floor.
    pub fn fw_update(
        &mut self,
        image: &FwImage,
        opts: UpdateOptions<'_>,
        mut progress: impl FnMut(Progress),
    ) -> Result<Updated, Error> {
        let info = self.fw_info()?;
        if matches!(info.state, StateCode::Committed | StateCode::Trial) {
            return Err(Error::Fw(format!(
                "an update is {} (slot {}); run `fw confirm` or `fw abort` first",
                info.state.name(),
                info.target.map_or("?", Slot::name)
            )));
        }
        let running = info.slots[info.running as usize]
            .version
            .unwrap_or_default();
        let floor = info.min_version.max(running);
        if image.version <= floor {
            return Err(Error::Fw(format!(
                "image version {} does not exceed the device's {} (running {}, last committed {})",
                format_version(image.version),
                format_version(floor),
                format_version(running),
                format_version(info.min_version)
            )));
        }
        let len =
            u32::try_from(image.bytes.len()).map_err(|_| Error::Fw("image too large".into()))?;
        let sha256 = image_sha256(&image.bytes);
        let chunks = image.bytes.len().div_ceil(CHUNK_LEN);
        let steps = 2 + chunks + usize::from(opts.reboot);
        let mut step = 0;
        let mut report = |s: &mut Self, step: usize, done: u32| {
            progress(Progress {
                step,
                steps,
                done,
                total: len,
                events: s.take_events(),
            });
        };
        let wrap = |step: usize, key: middleware::Key<'static>, e: Error| Error::Upload {
            step,
            steps,
            key: key.as_str().to_owned(),
            cause: Box::new(e),
        };
        let begin = Command::Begin {
            len,
            version: image.version,
            sha256,
        };
        // An RT device erases flash blocks at begin and at the first chunk
        // of each 256 KiB block (module docs).
        let wait = if image.is_rt() {
            erase_wait(1)
        } else {
            Duration::ZERO
        };
        let slot = self
            .fw_patient(&begin, wait)
            .map_err(|e| wrap(step, keys::BEGIN, e))?;
        let slot = slot
            .first()
            .and_then(|b| Slot::from_byte(*b))
            .ok_or_else(|| Error::Malformed {
                key: keys::BEGIN.as_str().to_owned(),
            })?;
        report(self, step, 0);
        for (i, data) in image.bytes.chunks(CHUNK_LEN).enumerate() {
            step += 1;
            let offset = (i * CHUNK_LEN) as u32;
            self.fw_patient(&Command::Chunk { offset, data }, wait)
                .map_err(|e| wrap(step, keys::CHUNK, e))?;
            report(self, step, offset + data.len() as u32);
        }
        step += 1;
        let signature = match opts.key {
            Some(seed) => Some(
                sign_firmware(&image.bytes, seed, image.version, slot, &info.node)
                    .ok_or_else(|| Error::Fw("image too large".into()))?,
            ),
            None => None,
        };
        self.fw(&Command::Commit { signature })
            .map_err(|e| wrap(step, keys::COMMIT, e))?;
        report(self, step, len);
        let mut done = Updated {
            slot,
            rebooted: false,
            reboot_unanswered: false,
        };
        if opts.reboot {
            step += 1;
            match self.fw(&Command::Reboot) {
                Ok(_) => {}
                // The reply can lose the race with the reset, and a USB
                // CDC port goes away with the device.
                Err(Error::Timeout { .. }) => done.reboot_unanswered = true,
                Err(e) if e.is_port_lost() => done.reboot_unanswered = true,
                Err(e) => return Err(wrap(step, keys::REBOOT, e)),
            }
            done.rebooted = true;
            report(self, step, len);
        }
        Ok(done)
    }

    /// `sys/fw/reboot`; a port that goes away with the reset counts as
    /// done.
    pub fn fw_reboot(&mut self) -> Result<(), Error> {
        match self.fw(&Command::Reboot) {
            Err(e) if e.is_port_lost() => Ok(()),
            r => r.map(drop),
        }
    }

    /// `sys/fw/confirm`, retried while the device answers `Unavailable`
    /// (its self-test has not passed yet), at most `attempts` times with
    /// `pause` between attempts.
    pub fn fw_confirm(&mut self, attempts: u32, mut pause: impl FnMut()) -> Result<(), Error> {
        let mut left = attempts.max(1);
        loop {
            match self.fw(&Command::Confirm) {
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

    /// `sys/fw/abort`, kept alive for two RT block erases (the record
    /// save and block 0; module docs).
    pub fn fw_abort(&mut self) -> Result<(), Error> {
        self.fw_patient(&Command::Abort, erase_wait(2)).map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FakeDevice, rt_image};
    use crate::link::{Link, Timeouts};
    use crate::sign::tests::vector_seed;
    use middleware::fw_ota::picobin::{EXE_TBYB, RP2350_ARM_S_EXE, image_def_block_bytes};
    use middleware::fw_ota::sim::{self, Rp2350Sim};
    use std::time::Duration;

    const V1_0: u32 = 0x0001_0000;
    const V1_1: u32 = 0x0001_0001;

    fn device(fake: FakeDevice) -> Device<Link<FakeDevice>> {
        let t = Timeouts {
            request: Duration::from_millis(50),
            begin: Duration::from_millis(50),
            retries: 1,
        };
        Device::new(Link::new(fake, t))
    }

    fn pico2() -> FakeDevice {
        let mut sim = Rp2350Sim::new(256 * 1024);
        sim.program(Slot::A, &sim::image(40_000, V1_0, false, 1));
        FakeDevice::new("rp2350", "0.1.0", 4096, true).with_fw(sim)
    }

    fn uf2(image: &[u8], family: u32) -> Vec<u8> {
        let n = image.len().div_ceil(256);
        let mut out = Vec::new();
        for (i, c) in image.chunks(256).enumerate() {
            let mut b = [0u8; 512];
            let words = [
                UF2_MAGIC_START0,
                UF2_MAGIC_START1,
                UF2_FLAG_FAMILY_ID,
                FLASH_BASE + (i * 256) as u32,
                c.len() as u32,
                i as u32,
                n as u32,
                family,
            ];
            for (j, w) in words.iter().enumerate() {
                b[4 * j..4 * j + 4].copy_from_slice(&w.to_le_bytes());
            }
            b[32..32 + c.len()].copy_from_slice(c);
            b[508..].copy_from_slice(&UF2_MAGIC_END.to_le_bytes());
            out.extend_from_slice(&b);
        }
        out
    }

    #[test]
    fn y9_images_load_from_bin_and_uf2_and_are_checked_like_the_device() {
        let bin = sim::image(10_000, V1_1, true, 2);
        let img = load_image(&bin, None).unwrap();
        assert_eq!(
            (img.version, img.from_uf2, img.bytes.len()),
            (Version(V1_1), false, 10_000)
        );
        let from_uf2 = load_image(&uf2(&bin, 0xe48b_ff59), Some(Version(V1_1))).unwrap();
        assert_eq!(from_uf2.bytes, bin);
        assert!(from_uf2.from_uf2);
        assert_eq!(
            load_image(&uf2(&bin, FAMILY_RP2040), None),
            Err(ImageError::Rp2040)
        );
        assert!(matches!(
            load_image(&uf2(&bin, 0x1234_5678), None),
            Err(ImageError::Uf2(_))
        ));
        assert!(matches!(
            load_image(&uf2(&bin, 0xe48b_ff59)[..700], None),
            Err(ImageError::Uf2(_))
        ));
        assert_eq!(load_image(b"\x7fELF\x01\x01", None), Err(ImageError::Elf));
        assert_eq!(load_image(&[], None), Err(ImageError::Empty));
        assert_eq!(load_image(&[0u8; 8192], None), Err(ImageError::NoImageDef));
        assert_eq!(
            load_image(&sim::image(10_000, V1_1, false, 2), None),
            Err(ImageError::NotTbyb)
        );
        assert_eq!(
            load_image(&bin, Some(Version(V1_0))),
            Err(ImageError::VersionMismatch {
                expected: Version(V1_0),
                image: Version(V1_1)
            })
        );
        // A single-block loop (no sentinel) is refused.
        let mut lone = vec![0u8; 8192];
        lone[0x40..0x40 + 28].copy_from_slice(&image_def_block_bytes(
            RP2350_ARM_S_EXE | EXE_TBYB,
            1,
            1,
            0,
        ));
        assert_eq!(load_image(&lone, None), Err(ImageError::NoSentinel));
        for e in [
            ImageError::Elf,
            ImageError::Rp2040,
            ImageError::NoSentinel,
            ImageError::NotTbyb,
        ] {
            assert!(!e.to_string().is_empty());
        }
        assert!(ImageError::Rp2040.to_string().contains("Pico 1"));
    }

    #[test]
    fn y9_versions_parse_as_major_minor() {
        assert_eq!(parse_version("1.2"), Some(Version(0x0001_0002)));
        assert_eq!(parse_version("65535.65535"), Some(Version(u32::MAX)));
        for bad in ["1", "1.2.3", "a.b", "65536.0", "-1.0", ""] {
            assert_eq!(parse_version(bad), None, "{bad}");
        }
        assert_eq!(format_version(Version(0x0003_0004)), "3.4");
    }

    #[test]
    fn y9_update_reboot_and_confirm_against_the_bootrom_model() {
        let mut d = device(pico2().with_claim(vector_seed().public_key()));
        let i = d.fw_info().unwrap();
        assert_eq!((i.state, i.running), (StateCode::Idle, Slot::A));
        let img = load_image(&sim::image(20_000, V1_1, true, 2), None).unwrap();
        let mut seen = Vec::new();
        let seed = vector_seed();
        let done = d
            .fw_update(
                &img,
                UpdateOptions {
                    key: Some(&seed),
                    reboot: true,
                },
                |p| seen.push((p.step, p.steps, p.done)),
            )
            .unwrap();
        assert_eq!(
            done,
            Updated {
                slot: Slot::B,
                rebooted: true,
                reboot_unanswered: false
            }
        );
        let chunks = 20_000usize.div_ceil(CHUNK_LEN);
        assert_eq!(seen.len(), chunks + 3);
        assert_eq!(seen.first(), Some(&(0, chunks + 3, 0)));
        assert_eq!(seen.last(), Some(&(chunks + 2, chunks + 3, 20_000)));
        // The fake reset into the trial after replying to the reboot.
        let i = d.fw_info().unwrap();
        assert_eq!(
            (i.state, i.running, i.version),
            (StateCode::Trial, Slot::B, Version(V1_1))
        );
        d.fw_confirm(3, || {}).unwrap();
        assert_eq!(d.fw_info().unwrap().state, StateCode::Confirmed);
        d.link_mut().transport_mut().fw_reset();
        let i = d.fw_info().unwrap();
        assert_eq!(
            (i.state, i.running, i.slots[1].tbyb),
            (StateCode::Idle, Slot::B, false)
        );
    }

    #[test]
    fn y9_refusals_are_explained() {
        // A claimed device and no key: Denied at commit, explained.
        let mut d = device(pico2().with_claim(vector_seed().public_key()));
        let img = load_image(&sim::image(9_000, V1_1, true, 2), None).unwrap();
        let err = d
            .fw_update(&img, UpdateOptions::default(), |_| {})
            .unwrap_err();
        assert_eq!(err.status(), Some(HandlerError::Denied));
        assert!(
            err.to_string().contains("sys/fw/commit") && err.to_string().contains("--key"),
            "{err}"
        );
        // Signed by another key.
        let other = Seed::new([42; 32]);
        let err = d
            .fw_update(
                &img,
                UpdateOptions {
                    key: Some(&other),
                    reboot: false,
                },
                |_| {},
            )
            .unwrap_err();
        assert_eq!(err.status(), Some(HandlerError::Denied));
        // Not newer than the running image: refused locally.
        let old = load_image(&sim::image(9_000, V1_0, true, 2), None).unwrap();
        let err = d
            .fw_update(&old, UpdateOptions::default(), |_| {})
            .unwrap_err();
        assert!(err.to_string().contains("does not exceed"), "{err}");
        // Confirm and reboot outside their states.
        let err = d.fw_confirm(1, || {}).unwrap_err();
        assert_eq!(err.status(), Some(HandlerError::InvalidRequest));
        assert!(err.to_string().contains("trial"), "{err}");
        let err = d.fw_reboot().unwrap_err();
        assert!(err.to_string().contains("commit"), "{err}");
        // A committed update blocks a new one until aborted.
        let seed = vector_seed();
        d.fw_update(
            &img,
            UpdateOptions {
                key: Some(&seed),
                reboot: false,
            },
            |_| {},
        )
        .unwrap();
        let newer = load_image(&sim::image(9_000, 0x0001_0002, true, 3), None).unwrap();
        let err = d
            .fw_update(
                &newer,
                UpdateOptions {
                    key: Some(&seed),
                    reboot: false,
                },
                |_| {},
            )
            .unwrap_err();
        assert!(err.to_string().contains("fw abort"), "{err}");
        d.fw_abort().unwrap();
        d.fw_update(
            &newer,
            UpdateOptions {
                key: Some(&seed),
                reboot: false,
            },
            |_| {},
        )
        .unwrap();
        assert_eq!(d.fw_info().unwrap().state, StateCode::Committed);
        d.fw_reboot().unwrap();
        assert_eq!(d.fw_info().unwrap().state, StateCode::Trial);
        // The self-test is still running: confirm retries, then succeeds.
        d.link_mut().transport_mut().fw_self_test(false);
        let mut pauses = 0;
        let err = d.fw_confirm(3, || pauses += 1).unwrap_err();
        assert_eq!((err.status(), pauses), (Some(HandlerError::Unavailable), 2));
        assert!(err.to_string().contains("self-test"), "{err}");
        d.link_mut().transport_mut().fw_self_test(true);
        d.fw_confirm(3, || {}).unwrap();
    }

    #[test]
    fn y9_lost_replies_are_resent_and_the_device_deduplicates() {
        use crate::fake::Script;
        // Request 1 is fw/info, 2 begin, 3.. chunks: lose a chunk's reply
        // and drop another chunk request.
        let fake = pico2().with_script(Script {
            drop_reply: vec![5],
            drop_request: vec![8],
            ..Script::default()
        });
        let mut d = device(fake);
        let img = load_image(&sim::image(9_000, V1_1, true, 2), None).unwrap();
        d.fw_update(
            &img,
            UpdateOptions {
                key: None,
                reboot: true,
            },
            |_| {},
        )
        .unwrap();
        d.fw_confirm(1, || {}).unwrap();
        let i = d.fw_info().unwrap();
        assert_eq!((i.state, i.running), (StateCode::Confirmed, Slot::B));
    }

    fn rt1050() -> FakeDevice {
        let mut sim = Rp2350Sim::new(512 * 1024);
        sim.program(Slot::A, &rt_image(40_000, V1_0, false, 1));
        FakeDevice::new("imxrt1052", "0.1.0", 4096, true).with_fw_rt(sim)
    }

    #[test]
    fn rt15_slot_images_load_without_the_picobin_checks() {
        // An MWRT image carries no IMAGE_DEF the host looks at: a payload
        // of zeros (which the picobin path refuses) is fine.
        let payload = vec![0u8; 8192];
        let header = RtHeader::for_image(Version(V1_1), &payload).unwrap();
        let mut file = header.encode().to_vec();
        file.extend_from_slice(&payload);
        let img = load_image(&file, Some(Version(V1_1))).unwrap();
        assert_eq!(
            (img.version, img.kind, img.is_rt(), img.from_uf2),
            (Version(V1_1), ImageKind::Rt(header), true, false)
        );
        // The whole file is sent, header first.
        assert_eq!(img.bytes, file);
        assert_eq!(load_rt_image(&file), Ok(header));
        assert_eq!(
            load_image(&file, Some(Version(V1_0))),
            Err(ImageError::VersionMismatch {
                expected: Version(V1_0),
                image: Version(V1_1)
            })
        );
        // Each defect is refused with its reason.
        let damaged = |at: usize, v: u8| {
            let mut f = file.clone();
            f[at] = v;
            load_image(&f, None)
        };
        assert_eq!(
            damaged(4, 2),
            Err(ImageError::Rt(RtImageError::BadHeaderVersion))
        );
        assert_eq!(
            damaged(100, 1),
            Err(ImageError::Rt(RtImageError::BadHeaderVersion))
        );
        assert_eq!(
            damaged(9, 9),
            Err(ImageError::Rt(RtImageError::HashMismatch))
        );
        assert_eq!(
            damaged(HEADER_LEN + 5, 1),
            Err(ImageError::Rt(RtImageError::HashMismatch))
        );
        assert_eq!(
            load_image(&file[..200], None),
            Err(ImageError::Rt(RtImageError::Short))
        );
        let short = load_image(&file[..file.len() - 1], None).unwrap_err();
        assert_eq!(
            short,
            ImageError::Rt(RtImageError::LengthMismatch {
                header: file.len() as u32,
                actual: file.len() - 1
            })
        );
        assert!(short.to_string().contains("imxrt-image"), "{short}");
        let mut padded = file.clone();
        padded.extend_from_slice(&[0xFF; 512]);
        assert!(matches!(
            load_image(&padded, None),
            Err(ImageError::Rt(RtImageError::LengthMismatch { .. }))
        ));
        let mut empty = RtHeader::for_image(Version(V1_1), &[])
            .unwrap()
            .encode()
            .to_vec();
        assert_eq!(
            load_image(&empty, None),
            Err(ImageError::Rt(RtImageError::Empty))
        );
        empty[12..16].copy_from_slice(&MAX_IMAGE.to_le_bytes());
        assert!(matches!(
            load_image(&empty, None),
            Err(ImageError::Rt(RtImageError::TooLarge { .. }))
        ));
        // The fake device's images are well-formed slot images too.
        let fake = load_image(&rt_image(9_000, V1_1, true, 2), None).unwrap();
        assert_eq!(
            (fake.version, fake.bytes.len()),
            (Version(V1_1), HEADER_LEN + 9_000)
        );
    }

    #[test]
    fn rt15_update_reboot_and_confirm_an_rt_slot_image() {
        let mut d = device(rt1050().with_claim(vector_seed().public_key()));
        let i = d.fw_info().unwrap();
        assert_eq!(
            (i.state, i.running, i.slots[0].version),
            (StateCode::Idle, Slot::A, Some(Version(V1_0)))
        );
        let file = rt_image(20_000, V1_1, true, 2);
        let img = load_image(&file, None).unwrap();
        let mut seen = Vec::new();
        let seed = vector_seed();
        let done = d
            .fw_update(
                &img,
                UpdateOptions {
                    key: Some(&seed),
                    reboot: true,
                },
                |p| seen.push((p.step, p.steps, p.done, p.total)),
            )
            .unwrap();
        assert_eq!((done.slot, done.rebooted), (Slot::B, true));
        // Header and image were sent: begin's length covers both.
        let total = (HEADER_LEN + 20_000) as u32;
        let chunks = (total as usize).div_ceil(CHUNK_LEN);
        assert_eq!(seen.len(), chunks + 3);
        assert_eq!(seen.last(), Some(&(chunks + 2, chunks + 3, total, total)));
        let i = d.fw_info().unwrap();
        assert_eq!(
            (i.state, i.running, i.version, i.slots[1].version),
            (
                StateCode::Trial,
                Slot::B,
                Version(V1_1),
                Some(Version(V1_1))
            )
        );
        d.fw_confirm(3, || {}).unwrap();
        assert_eq!(d.fw_info().unwrap().state, StateCode::Confirmed);
        // The installed slot is the file, header first.
        let sim = d.link().transport().fw_sim().unwrap();
        assert_eq!(sim.slot(Slot::B)[..HEADER_LEN], file[..HEADER_LEN]);
    }

    #[test]
    fn rt15_a_usb_port_that_vanishes_with_the_reboot_is_a_reboot() {
        let fake =
            rt1050()
                .with_claim(vector_seed().public_key())
                .with_script(crate::fake::Script {
                    vanish_on_reset: true,
                    ..Default::default()
                });
        let mut d = device(fake);
        let img = load_image(&rt_image(20_000, V1_1, true, 2), None).unwrap();
        let seed = vector_seed();
        let done = d
            .fw_update(
                &img,
                UpdateOptions {
                    key: Some(&seed),
                    reboot: true,
                },
                |_| {},
            )
            .unwrap();
        assert_eq!(
            (done.slot, done.rebooted, done.reboot_unanswered),
            (Slot::B, true, true),
            "the broken pipe after the reboot request is the reset"
        );
        // The port is gone until the host reopens it.
        let lost = d.fw_info().unwrap_err();
        assert!(lost.is_port_lost(), "{lost:?}");
        d.link_mut().transport_mut().reappear();
        assert_eq!(d.fw_info().unwrap().state, StateCode::Trial);
        d.fw_confirm(3, || {}).unwrap();
        assert_eq!(d.fw_info().unwrap().state, StateCode::Confirmed);
        // `fw reboot` on its own: the same rule.
        assert!(
            !Error::Timeout {
                key: "k".into(),
                attempts: 1
            }
            .is_port_lost()
        );
        assert!(
            Error::Io(std::io::Error::from_raw_os_error(6)).is_port_lost(),
            "ENXIO"
        );
        assert!(
            !Error::Io(std::io::Error::from_raw_os_error(13)).is_port_lost(),
            "EACCES is not"
        );
    }

    #[test]
    fn rt15_a_bad_header_is_refused_before_anything_is_sent() {
        let mut file = rt_image(9_000, V1_1, true, 2);
        file[HEADER_LEN + 4000] ^= 0x55;
        let err = load_image(&file, None).unwrap_err();
        assert_eq!(err, ImageError::Rt(RtImageError::HashMismatch));
        assert!(err.to_string().contains("SHA-256"), "{err}");
        // `mwctl fw update` stops at the load, so the device saw nothing;
        // and the RT updater itself refuses the same image at commit.
        let mut d = device(rt1050());
        assert_eq!(d.link().transport().requests(), 0);
        let forged = FwImage {
            bytes: file,
            kind: ImageKind::Rt(RtHeader::decode(&rt_image(9_000, V1_1, true, 2)).unwrap()),
            version: Version(V1_1),
            from_uf2: false,
        };
        let err = d
            .fw_update(&forged, UpdateOptions::default(), |_| {})
            .unwrap_err();
        assert_eq!(err.status(), Some(HandlerError::InvalidRequest), "{err}");
        // A picobin image is not an RT image: the RT updater refuses it.
        let rp = load_image(&sim::image(9_000, V1_1, true, 2), None).unwrap();
        let err = d
            .fw_update(&rp, UpdateOptions::default(), |_| {})
            .unwrap_err();
        assert_eq!(err.status(), Some(HandlerError::InvalidRequest), "{err}");
    }

    #[test]
    fn rt15_erase_waits_cover_256k_block_erases() {
        use middleware::fw_ota::keys as fw;
        // The link's per-attempt floors already cover one 4 s erase for
        // begin, commit (plus hashing) and confirm.
        let t = Timeouts::default();
        for key in [fw::BEGIN, fw::COMMIT, fw::CONFIRM] {
            assert!(t.for_key(key) > RT_BLOCK_ERASE, "{}", key.as_str());
        }
        // Chunks and abort use the plain timeout; they are kept alive.
        assert!(t.for_key(fw::CHUNK) < RT_BLOCK_ERASE);
        assert_eq!(erase_wait(1), Duration::from_secs(5));
        assert_eq!(erase_wait(2), Duration::from_secs(9));
        // The first chunk of the second 256 KiB block erases it: its reply
        // comes too late for the link's two attempts (requests 1 info, 2
        // begin, 3.. chunks; chunk 512 is request 515).
        use crate::fake::Script;
        let lost = Script {
            drop_reply: vec![515, 516, 517],
            ..Script::default()
        };
        let file = rt_image(300 * 1024, V1_1, false, 3);
        let img = load_image(&file, None).unwrap();
        let mut d = device(rt1050().with_script(lost.clone()));
        d.fw_update(&img, UpdateOptions::default(), |_| {}).unwrap();
        assert_eq!(d.fw_info().unwrap().state, StateCode::Committed);
        // info, begin, 601 chunks, the 3 resends, commit, info.
        assert_eq!(d.link().transport().requests(), 1 + 1 + 601 + 3 + 1 + 1);
        // The RP2350 path is unchanged: the same lost replies fail it.
        let mut sim = Rp2350Sim::new(512 * 1024);
        sim.program(Slot::A, &sim::image(40_000, V1_0, false, 1));
        let fake = FakeDevice::new("rp2350", "0.1.0", 4096, true)
            .with_fw(sim)
            .with_script(lost);
        let mut d = device(fake);
        let rp = load_image(&sim::image(300 * 1024, V1_1, true, 3), None).unwrap();
        let err = d
            .fw_update(&rp, UpdateOptions::default(), |_| {})
            .unwrap_err();
        assert!(err.to_string().contains("sys/fw/chunk"), "{err}");
    }

    /// A port nothing answers on.
    #[derive(Default)]
    struct Silent {
        writes: usize,
    }

    impl crate::link::Transport for Silent {
        fn write_all(&mut self, _: &[u8]) -> std::io::Result<()> {
            self.writes += 1;
            Ok(())
        }

        fn read(&mut self, _: &mut [u8], _: Duration) -> std::io::Result<usize> {
            Ok(0)
        }
    }

    #[test]
    fn rt15_a_kept_alive_request_gives_up_after_its_wait() {
        let t = Timeouts {
            request: Duration::from_millis(1),
            begin: Duration::from_millis(1),
            retries: 1,
        };
        let mut d = Device::new(Link::new(Silent::default(), t));
        // Without a wait: one request, two attempts.
        let err = d.fw_patient(&Command::Abort, Duration::ZERO).unwrap_err();
        assert!(matches!(err, Error::Timeout { attempts: 2, .. }), "{err}");
        assert_eq!(d.link().transport().writes, 2 + 1);
        // With one: sent again until the wait has passed.
        let wait = Duration::from_millis(30);
        let began = Instant::now();
        let err = d.fw_patient(&Command::Abort, wait).unwrap_err();
        assert!(matches!(err, Error::Timeout { .. }), "{err}");
        assert!(began.elapsed() >= wait);
        assert!(d.link().transport().writes > 2 + 1 + 2);
    }

    #[test]
    fn y9_abort_on_trial_rolls_back() {
        let mut d = device(pico2());
        let img = load_image(&sim::image(9_000, V1_1, true, 2), None).unwrap();
        d.fw_update(
            &img,
            UpdateOptions {
                key: None,
                reboot: true,
            },
            |_| {},
        )
        .unwrap();
        d.fw_abort().unwrap();
        let i = d.fw_info().unwrap();
        assert_eq!(
            (i.state, i.running, i.target),
            (StateCode::RolledBack, Slot::A, Some(Slot::B))
        );
    }
}
