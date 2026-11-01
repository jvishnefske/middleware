//! RP2350 A/B firmware update (`mwctl fw ...`), pure parts and protocol
//! operations.
//!
//! * [`load_image`] — the image file: a **UF2** (as `picotool` or
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
//!   floor agree.
//! * [`parse_version`] / [`format_version`] — `MAJOR.MINOR` (the protocol
//!   version is `major << 16 | minor`).
//! * [`Device::fw_info`], [`Device::fw_update`], [`Device::fw_confirm`],
//!   [`Device::fw_abort`], [`Device::fw_reboot`] — the `sys/fw/*`
//!   requests. An update sends `begin` (the device answers the target
//!   slot), every chunk in order with progress, a commit signed for that
//!   slot and the device's node id (with a key), and optionally `reboot`.
//!   Chunk resends after a timeout are harmless (the device acknowledges a
//!   chunk it has without writing it again).

use crate::device::{Device, Progress};
use crate::error::Error;
use crate::link::Endpoint;
use crate::sign::{Seed, image_sha256};
use ed25519_dalek::{Signer, SigningKey};
use middleware::fw_ota::picobin::{ImageDef, SENTINEL_FROM, find_image_def_in};
use middleware::fw_ota::wire::{Command, FwInfo, StateCode, keys, signed_message};
use middleware::fw_ota::{CHUNK_LEN, MAX_IMAGE, Slot};
use middleware::loader::trust::{Signature, Version};
use middleware::transport::HandlerError;
use std::fmt;

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
            ImageError::VersionMismatch { expected, image } => write!(
                f,
                "--version {} but the image's IMAGE_DEF says {}",
                format_version(*expected),
                format_version(*image)
            ),
        }
    }
}

impl std::error::Error for ImageError {}

/// A firmware image ready to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FwImage {
    /// The flat image, as it lands at the start of a partition.
    pub bytes: Vec<u8>,
    /// Its IMAGE_DEF.
    pub def: ImageDef,
    /// Its version (`major << 16 | minor`).
    pub version: Version,
    /// Whether it came from a UF2.
    pub from_uf2: bool,
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

/// Reads an image file's bytes into a [`FwImage`], checking it as the
/// device will (see the module docs).
pub fn load_image(file: &[u8], expected: Option<Version>) -> Result<FwImage, ImageError> {
    if file.is_empty() {
        return Err(ImageError::Empty);
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
    if let Some(e) = expected.filter(|e| *e != version) {
        return Err(ImageError::VersionMismatch {
            expected: e,
            image: version,
        });
    }
    Ok(FwImage {
        bytes,
        def,
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
        let slot = self.fw(&begin).map_err(|e| wrap(step, keys::BEGIN, e))?;
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
            self.fw(&Command::Chunk { offset, data })
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
                // The reply can lose the race with the reset.
                Err(Error::Timeout { .. }) => done.reboot_unanswered = true,
                Err(e) => return Err(wrap(step, keys::REBOOT, e)),
            }
            done.rebooted = true;
            report(self, step, len);
        }
        Ok(done)
    }

    /// `sys/fw/reboot`.
    pub fn fw_reboot(&mut self) -> Result<(), Error> {
        self.fw(&Command::Reboot).map(drop)
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

    /// `sys/fw/abort`.
    pub fn fw_abort(&mut self) -> Result<(), Error> {
        self.fw(&Command::Abort).map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeDevice;
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
