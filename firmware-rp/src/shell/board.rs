//! The chip the binary is built for: its profile and flash driver type.

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
pub type FlashDriver = Flash<'static, FLASH, Blocking, FLASH_SIZE>;

/// The flash as the store sees it: one sector per erase call.
pub type StoreFlash = Sectored<FlashDriver>;

/// Firmware version reported in `sys/info`.
#[cfg(feature = "wasm")]
pub const FIRMWARE: &str = env!("CARGO_PKG_VERSION");
/// Firmware version reported in `sys/info`; the suffix marks a build
/// without the WASM runtime (what `mwctl` checks).
#[cfg(not(feature = "wasm"))]
pub const FIRMWARE: &str = concat!(env!("CARGO_PKG_VERSION"), "+no-wasm");

/// The firmware-update backend: the `sys/fw/*` stub, or none.
#[cfg(feature = "fw-update")]
pub type Fw = rp_base::fw_update::Unsupported;
/// The firmware-update backend: the `sys/fw/*` stub, or none.
#[cfg(not(feature = "fw-update"))]
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
