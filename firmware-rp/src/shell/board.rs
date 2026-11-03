//! The chip the binary is built for: its profile, flash driver type and
//! firmware-update backend.

use embassy_rp::flash::{Blocking, Flash};
use embassy_rp::peripherals::FLASH;
use rp_base::chip::Profile;
use rp_base::flash::Sectored;

/// The chip's profile.
#[cfg(feature = "rp2040")]
pub const PROFILE: Profile = rp_base::chip::RP2040;
/// The chip's profile.
#[cfg(feature = "rp2350")]
pub const PROFILE: Profile = rp_base::chip::RP2350;

/// The static profile the tasks borrow.
pub static PROFILE_REF: Profile = PROFILE;

/// Flash size in bytes, from the flash map.
pub const FLASH_SIZE: usize = PROFILE.flash.flash_size as usize;

/// The QSPI flash driver.
#[cfg(feature = "rp2040")]
pub type FlashDriver = Flash<'static, FLASH, Blocking, FLASH_SIZE>;
/// The QSPI flash driver. It is told the flash is twice its size: offsets
/// below [`FLASH_SIZE`] are erased and programmed physically (the boot
/// ROM's functions), reads go through the untranslated window at
/// `FLASH_SIZE + offset` ([`rp_base::flash::Window`]).
#[cfg(feature = "rp2350")]
pub type FlashDriver = Flash<'static, FLASH, Blocking, { 2 * FLASH_SIZE }>;

/// The flash as the store and the updater see it: one sector per erase
/// call (and, on the RP2350, physical offsets for reads too).
#[cfg(feature = "rp2040")]
pub type StoreFlash = Sectored<FlashDriver>;
/// The flash as the store and the updater see it: one sector per erase
/// call (and, on the RP2350, physical offsets for reads too).
#[cfg(feature = "rp2350")]
pub type StoreFlash = Sectored<rp_base::flash::Window<FlashDriver>>;

/// The driver as [`StoreFlash`].
#[cfg(feature = "rp2040")]
pub fn store_flash(flash: FlashDriver) -> StoreFlash {
    Sectored(flash)
}

/// The driver as [`StoreFlash`], after mapping the untranslated window.
#[cfg(feature = "rp2350")]
pub fn store_flash(flash: FlashDriver) -> StoreFlash {
    super::rom::map_untranslated_window();
    Sectored(rp_base::flash::Window::new(flash, FLASH_SIZE as u32, FLASH_SIZE as u32))
}

/// Firmware version reported in `sys/info`: the image's `major.minor`
/// (`RP_BASE_FW_VERSION` at build time, else the crate's).
#[cfg(feature = "wasm")]
pub const FIRMWARE: &str = env!("RP_BASE_IMAGE_VERSION");
/// Firmware version reported in `sys/info`; the suffix marks a build
/// without the WASM runtime (what `mwctl` checks).
#[cfg(not(feature = "wasm"))]
pub const FIRMWARE: &str = concat!(env!("CARGO_PKG_VERSION"), "+no-wasm");

/// The firmware-update backend: A/B OTA on the RP2350.
#[cfg(all(feature = "fw-update", feature = "rp2350"))]
pub type Fw = rp_base::ota::AbUpdate<super::rom::RpRom>;
/// The firmware-update backend: none (`sys/fw/*` is `NotFound`).
#[cfg(not(all(feature = "fw-update", feature = "rp2350")))]
pub type Fw = rp_base::fw_update::Absent;

/// The chip's 64-bit unique id: the flash chip's unique id on the RP2040.
#[cfg(feature = "rp2040")]
pub fn unique_id(flash: &mut FlashDriver) -> [u8; 8] {
    let mut uid = [0u8; 8];
    // On failure the id stays zero; the boot log shows it.
    let _ = flash.blocking_unique_id(&mut uid);
    uid
}

/// The chip's 64-bit unique id: the OTP chip id on the RP2350.
#[cfg(feature = "rp2350")]
pub fn unique_id(_flash: &mut FlashDriver) -> [u8; 8] {
    embassy_rp::otp::get_chipid().map(u64::to_be_bytes).unwrap_or([0; 8])
}
