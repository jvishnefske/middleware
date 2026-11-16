//! Imperative shell shared by the binaries (feature `shell`): the hardware
//! drivers around the pure core, all register access through embassy-nxp's
//! chiptool PAC (safe register reads and writes, no `unsafe`).
//!
//! * [`flexspi`] — the HyperFlash on FlexSPI A: controller bring-up, the
//!   [`crate::hyperflash`] LUT and plans run as IP commands, and an
//!   `embedded-storage` `NorFlash` with 256 KiB erase blocks.
//! * [`console`] — a blocking writer on LPUART1 (the OpenSDA VCOM), for
//!   the bootloader's messages.
//!
//! On i.MX RT an access to a peripheral whose CCM clock gate is off stalls
//! the bus until a power cycle, so every driver enables its clock gate
//! before its first register access.

pub mod console;
pub mod flexspi;
