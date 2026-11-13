//! `mw-pi-node`: a Linux host (a Raspberry Pi Zero 2 W or Pi 3 on a 64-bit
//! OS, or any Linux machine) that runs WASM payloads itself, accepts the
//! same signed payload updates as the microcontroller base firmwares, and
//! joins the cluster through the same gateway.
//!
//! The node is a base firmware in a process. Its request decisions and
//! storage are the ESP base firmware's pure core, shared as rp-base shares
//! it (`esp-base` with default features off and `trust` on): the request
//! table, loader state machine, payload lifecycle and trust rules
//! (`esp_base::dispatch`), the two-slot payload store
//! (`esp_base::store`, `esp_base::image`), the pinned key
//! (`esp_base::trust_store`), served-key routing (`esp_base::serve`) and
//! the payload timer table (`esp_base::periph::timers`). Payloads run in
//! `wasm-host`. This crate adds what differs on Linux:
//!
//! * [`config`] — `MW_PI_NODE_*`, parsed purely from the environment.
//! * [`node_id`] — the node id from `MW_PI_NODE_NODE_ID` or the systemd
//!   machine id, hashed.
//! * [`flash`] — a NOR flash model whose sectors persist atomically and
//!   durably as files in the state directory (`flash::dir`), with an
//!   in-memory backend for tests (`flash::mem`).
//! * [`layout`] — the flash map (payload store, trust record).
//! * [`service`] — the request service over that flash (rp-base's, with
//!   no firmware updater: `sys/fw/*` is `NotFound`).
//! * [`device`] — `wasm_host::Device` on Linux: clock, timers, the
//!   outbox, `serve` and `node_id`; peripherals `Unsupported`.
//! * [`controller`] — the service and the running payload as one
//!   synchronous state machine.
//! * [`shell`] — the device thread over its serial links and the
//!   in-process gateway (`mw_gateway::start` over a `ChannelIo` pair).
//!
//! On the wire the node is a board: `sys/info` names the chip
//! [`service::CHIP`] (`linux-aarch64`), the firmware version, the node id
//! and `Caps::SERVE`; `mwctl` and the web console speak to it unchanged.
#![forbid(unsafe_code)]

pub mod config;
pub mod controller;
pub mod device;
pub mod flash;
pub mod layout;
pub mod node_id;
pub mod service;
pub mod shell;

use std::io;
use std::path::Path;

use middleware::SenderId;
use wasm_host::Limits;

use flash::SectorFlash;
use flash::dir::DirSectors;

pub use config::PiNodeConfig;
pub use shell::{PiNode, Report, start};

/// The node's flash in the state directory `state` (created if missing,
/// locked while the returned flash lives).
pub fn open_state_flash(state: &Path) -> io::Result<SectorFlash<DirSectors>> {
    SectorFlash::open(DirSectors::open(state)?, layout::SECTORS)
}

/// What `sys/info` reports for node `node`.
pub fn facts(node: SenderId) -> service::Facts {
    service::Facts { chip: service::CHIP, firmware: env!("CARGO_PKG_VERSION"), node }
}

/// The limits payloads run under: `memory_bytes` of linear memory, the
/// ABI's default fuel per call and interpreter stacks.
pub fn limits(memory_bytes: u32) -> Limits {
    Limits::bytes(memory_bytes)
}
