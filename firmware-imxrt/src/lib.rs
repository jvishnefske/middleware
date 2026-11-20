//! Pure core of the i.MX RT base firmware (NXP MIMXRT1050-EVKB).
//!
//! Everything here is deterministic decision logic, byte layouts and
//! storage effects over a generic NOR flash, with no hardware access, so it
//! is unit tested on the host (`cargo test --lib`). The request decisions
//! are the ESP base firmware's own (`esp_base::dispatch`, `store`,
//! `trust_store`), as on the RP base firmware. This crate adds what
//! differs on the RT1050-EVKB:
//!
//! * [`layout`] — the HyperFlash map: bootloader, update record, firmware
//!   slots A and B, trust record and payload store, in whole 256 KiB erase
//!   blocks.
//! * [`boot_header`] — the boot ROM's FlexSPI configuration block (FCB),
//!   image vector table (IVT) and boot data, built as bytes.
//! * [`hyperflash`] — the HyperFlash (S26KS512S) command sequences for the
//!   FlexSPI lookup table and the plans of each read, program and erase.
//! * [`ab`] — A/B firmware: the slot image header, its integrity check,
//!   the bootloader's boot decision and the `middleware::fw_ota` platform.
//! * [`chip`] — the board profile: name, heap plan, WASM caps and the
//!   EVKB's payload pins and peripherals.
//! * `shell` (feature `shell`, target only) — the imperative shell the
//!   binaries share: the FlexSPI HyperFlash driver and the LPUART1
//!   console, through embassy-nxp's PAC.
//! * [`mdns_node`] — the mDNS responder's decisions: the board's `mw/node`
//!   record and when to announce it again or withdraw it.
//! * [`service`] — the request service over the HyperFlash: `sys/fw/*` to
//!   the A/B updater, everything else through `esp_base::dispatch`, with
//!   the storage effects on the flash map's regions.
#![no_std]
#![forbid(unsafe_code)]

pub mod ab;
pub mod boot_header;
#[cfg(any(feature = "wasm", test))]
pub mod budget;
pub mod chip;
pub mod config;
pub mod enet;
pub mod hyperflash;
pub mod layout;
pub mod links;
pub mod mdns_node;
pub mod node;
pub mod periph;
pub mod pins;
#[cfg(any(feature = "wasm", test))]
pub mod requests;
pub mod sensors;
pub mod service;
#[cfg(feature = "shell")]
pub mod shell;
pub mod usb;
pub mod zenoh_mgmt;
