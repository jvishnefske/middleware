//! The EVKB's HyperFlash (Infineon/Cypress S26KS512S, 64 MiB, FlexSPI
//! port A1), as pure data: LUT encoding, command sequences, operation plans,
//! status decoding and the CFI query.
//!
//! Nothing here touches a register. The driver (the imperative shell)
//! loads [`s26ks512s::LUT`] into the FlexSPI LUT, then for each operation
//! executes the [`plan::Step`]s a plan returns: IP commands with a
//! sequence run, address and data length, and status waits whose every
//! read is decided by [`status::wait_step`] within an explicit bound.
//!
//! * [`lut`] — the 16-bit LUT instruction format, sequences and the LUT.
//! * [`s26ks512s`] — the flash's sequences (DDR read, status read, unlock,
//!   word program, sector erase, data write for CFI entry and exit) at
//!   their LUT indices, and the FCB's one-sequence LUT.
//! * [`plan`] — programs split at the 512-byte write buffer, erases of one
//!   256 KiB sector, reads, CFI entry, read and exit.
//! * [`status`] — the status register (ready, erase / program error) and
//!   the bounded wait.
//! * [`cfi`] — the CFI query parser that checks the 256 KiB uniform-sector
//!   assumption on hardware.

pub mod cfi;
pub mod lut;
pub mod plan;
pub mod s26ks512s;
pub mod status;

/// Where FlexSPI maps the flash for AHB reads (and the boot ROM boots).
pub const FLASH_BASE: u32 = 0x6000_0000;
/// Flash size: 64 MiB.
pub const FLASH_SIZE: u32 = 64 * 1024 * 1024;
/// Erase sector size: 256 KiB, uniform.
pub const SECTOR_SIZE: u32 = 256 * 1024;
/// Number of sectors.
pub const SECTOR_COUNT: u32 = FLASH_SIZE / SECTOR_SIZE;
/// Write buffer (one program burst at most, never crossed): 512 bytes.
pub const WRITE_BUFFER_SIZE: u32 = 512;
