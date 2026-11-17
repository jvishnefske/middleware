//! A/B firmware on the RT1050: the slot image header, its integrity
//! check, the bootloader's boot decision and the `middleware::fw_ota`
//! platform (design.md "Boot, A/B slots and the bootloader").
//!
//! The RT1050 boot ROM boots one image (the bootloader in block 0) and
//! knows nothing of slots. The bootloader boots one of the two firmware
//! slots of [`crate::layout`]; the running firmware updates the other with
//! `middleware::fw_ota`, protocol unchanged. The two meet in the update
//! record, which takes the place of the RP2350 boot ROM's A/B state.
//!
//! * [`header`] — the slot image header (magic, header version, image
//!   version, length, SHA-256) and [`header::check_copy`], which copies a
//!   slot's image into the SDRAM region and verifies the copy (RT5).
//! * [`record`] — [`record::BootRecord`]: the `fw_ota` record plus the
//!   confirmed slot and the trial's attempts, saved atomically in two
//!   alternate blocks.
//! * [`boot`] — [`boot::decide`], the bootloader's pure decision (RT6), and
//!   the [`boot::Handoff`] word that tells the firmware what was booted.
//! * [`platform`] — [`platform::RtImage`], the header as a
//!   `fw_ota` image format, and [`platform::RtPlatform`], `FwPlatform` over
//!   any 256 KiB-block NOR flash (RT7).
//! * [`io`] — byte-granular flash reads and writes over the HyperFlash's
//!   16-bit word.
//!
//! The bootloader's shell, in order: [`record::load`]; [`boot::decide`]
//! with [`boot::check_slot`] into the SDRAM image region; for a trial,
//! [`record::save`] of the decision's record **before** the jump (with the
//! watchdog armed); leave the [`boot::Handoff`]; jump. The jump is not in
//! this crate. The firmware's shell: [`platform::AbState::new`] from the
//! handoff; `FwService::<RtImage>::boot_for` over an
//! [`platform::RtPlatform`]; each `sys/fw/*` request through a platform
//! built for that request; then perform [`platform::AbState::take_reset`]
//! after the reply leaves, and release the trial watchdog once
//! [`platform::AbState::trial_released`].
//!
//! Power loss at any flash operation of an update, of the bootloader's
//! attempt write or of the confirm leaves a state from which the next boot
//! runs a verified slot (the tests of this module, RT8).

pub mod boot;
pub mod header;
pub mod io;
pub mod platform;
pub mod record;
#[cfg(test)]
pub(crate) mod test_flash;
#[cfg(test)]
mod tests;
pub mod watchdog;

pub use boot::{Decision, Handoff, decide};
pub use header::{HEADER_LEN, Refusal, SlotHeader};
pub use platform::{AbState, RtImage, RtPlatform};
pub use record::BootRecord;
