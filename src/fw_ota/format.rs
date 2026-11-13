//! The image-format seam: how the updater inspects a firmware image.
//!
//! The protocol, the state machine and the shell ([`super::machine`],
//! `super::service`) are the same on every board; what an image *is*
//! differs. [`ImageFormat`] is that difference:
//!
//! * [`ImageFormat::inspect`] reads what a `len`-byte image says about
//!   itself at commit (and when the shell re-checks a committed image at
//!   boot), giving an [`ImageFormat::Info`];
//! * [`ImageInfo::acceptable`] decides from that info alone (pure) whether
//!   the image may be installed as the declared version;
//! * [`ImageFormat::slot_info`] is the cheap per-slot summary
//!   `sys/fw/info` reports and the running version is taken from.
//!
//! Both readers take image offsets and fill a buffer; the shell serves the
//! first [`super::SECTOR`] from its stash while a transfer is being
//! committed and the rest from flash, so a format must keep what it needs
//! to check in the image (the first sector being written last is the
//! updater's rule, independent of the format).
//!
//! [`Picobin`] is the RP2350's format (an IMAGE_DEF in the metadata block
//! loop, [`super::picobin`]); its behaviour is exactly the updater's
//! before the seam existed. Other boards implement the trait next to their
//! platform (the i.MX RT base firmware's slot image header, for example).

use super::machine::acceptable;
use super::picobin::{ImageDef, find_image_def};
use super::wire::SlotInfo;
use crate::loader::trust::Version;

/// What [`ImageFormat::inspect`] found, enough to decide acceptance.
pub trait ImageInfo: Copy + core::fmt::Debug + PartialEq + Eq {
    /// Whether this image may be installed as `version` (pure).
    fn acceptable(&self, version: Version) -> bool;
}

/// A board's firmware image format. See the module docs.
pub trait ImageFormat {
    /// The inspection result.
    type Info: ImageInfo;

    /// Inspects the `len`-byte image `read` serves (`read(offset, buf)`
    /// fills `buf` from image offset `offset`, word-aligned): `Ok(None)`
    /// when it is not an image of this format at all. Every loop is
    /// bounded by `len`.
    fn inspect<E, R>(read: &mut R, len: u32) -> Result<Option<Self::Info>, E>
    where
        R: FnMut(u32, &mut [u8]) -> Result<(), E>;

    /// What a slot of `slot_size` bytes holds, cheaply (no full-image
    /// hash): the version of a valid image, and whether it is still on
    /// trial in the format's own terms.
    fn slot_info<E, R>(read: &mut R, slot_size: u32) -> Result<SlotInfo, E>
    where
        R: FnMut(u32, &mut [u8]) -> Result<(), E>;
}

/// The RP2350 image format: an IMAGE_DEF in the image's metadata block
/// loop ([`find_image_def`]), accepted by [`acceptable`] (an RP2350
/// executable flagged TBYB, the declared version, no rollback version, the
/// sentinel in the first sector's last page).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Picobin;

impl ImageInfo for ImageDef {
    fn acceptable(&self, version: Version) -> bool {
        acceptable(self, version)
    }
}

fn word<E, R>(read: &mut R, at: u32) -> Result<u32, E>
where
    R: FnMut(u32, &mut [u8]) -> Result<(), E>,
{
    let mut w = [0u8; 4];
    read(at, &mut w)?;
    Ok(u32::from_le_bytes(w))
}

impl ImageFormat for Picobin {
    type Info = ImageDef;

    fn inspect<E, R>(read: &mut R, len: u32) -> Result<Option<ImageDef>, E>
    where
        R: FnMut(u32, &mut [u8]) -> Result<(), E>,
    {
        find_image_def(|at| word(read, at), len)
    }

    /// A valid RP2350 executable's version and its TBYB flag; nothing for
    /// any other content.
    fn slot_info<E, R>(read: &mut R, slot_size: u32) -> Result<SlotInfo, E>
    where
        R: FnMut(u32, &mut [u8]) -> Result<(), E>,
    {
        Ok(match Self::inspect(read, slot_size)? {
            Some(d) if d.is_exe() && d.is_rp2350() => SlotInfo {
                version: Some(Version(d.version_u32())),
                tbyb: d.is_tbyb(),
            },
            _ => SlotInfo::default(),
        })
    }
}
