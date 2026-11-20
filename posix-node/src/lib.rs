//! `mw-posix-node`: a Linux host (a Raspberry Pi Zero 2 W or Pi 3 on a 64-bit
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
//! * [`config`] — `MW_POSIX_NODE_*`, parsed purely from the environment.
//! * [`node_id`] — the node id from `MW_POSIX_NODE_NODE_ID` or the systemd
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
pub mod driver;
pub mod espnow_gw;
pub mod flash;
pub mod layout;
pub mod mesh_gate;
pub mod node_id;
pub mod service;
pub mod shell;
pub mod zenoh_sys;

use std::io;
use std::path::Path;

use middleware::SenderId;
use wasm_host::Limits;

use flash::SectorFlash;
use flash::dir::DirSectors;

pub use config::PosixNodeConfig;
pub use shell::{DeviceParts, GatewayParts, PosixNode, Report, start};

/// The node's flash in the state directory `state` (created if missing,
/// locked while the returned flash lives).
pub fn open_state_flash(state: &Path) -> io::Result<SectorFlash<DirSectors>> {
    SectorFlash::open(DirSectors::open(state)?, layout::SECTORS)
}

/// What `sys/info` and `sys/manifest` report for node `node`, with the
/// board read from [`DEVICE_TREE_MODEL`] (none on a host without a device
/// tree).
pub fn facts(node: SenderId) -> service::Facts {
    let model = std::fs::read(DEVICE_TREE_MODEL).ok();
    service::Facts {
        chip: service::CHIP,
        firmware: env!("CARGO_PKG_VERSION"),
        node,
        board: model.as_deref().and_then(board_from_model),
        control: None,
    }
}

/// [`facts`] with `driver`'s control-module points counted in the
/// manifest.
pub fn facts_with_driver(node: SenderId, driver: &dyn driver::Driver) -> service::Facts {
    service::Facts { control: driver::control_summary(driver.points()), ..facts(node) }
}

/// Where Linux shows the device tree's `model`.
pub const DEVICE_TREE_MODEL: &str = "/proc/device-tree/model";

/// The manifest's board name from the device tree's `model` property: a
/// NUL-terminated string (`ZynqMP KD240 revA/B/1`, `Raspberry Pi 4 Model B
/// Rev 1.4`). Pure.
pub fn board_from_model(raw: &[u8]) -> Option<middleware::loader::manifest::BoardName> {
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    let text = std::str::from_utf8(&raw[..end]).ok()?.trim();
    middleware::loader::manifest::BoardName::new(text)
}

/// The gateway's control counter floor at start, as `mw-gateway`'s
/// binary computes it (`mw_gateway::state::floor_at_start`): the larger of
/// the counter persisted in `store` and the wall clock `clock_ms` (counted
/// only once it is past 2026-01-01); `None` with neither, and then signed
/// start/stop through `gw/sys` are refused until a floor exists.
pub fn control_floor(
    store: &mw_gateway::state::ControlStore,
    clock_ms: Option<u64>,
) -> Option<middleware::loader::trust::ControlCounter> {
    mw_gateway::state::floor_at_start(store.load(), clock_ms)
}

/// The wall clock in Unix milliseconds, if readable.
pub fn wall_clock_ms() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
}

/// The limits payloads run under: `memory_bytes` of linear memory,
/// [`config::DEFAULT_FUEL_PER_CALL`] per call, the default interpreter
/// stacks, and the most subscriptions an ABI 8 host gives
/// ([`middleware::wasm_abi::SLOT_MAX`]): a Linux node has the room a
/// machine payload's channels and command keys need, and the time a
/// process model's preflight and compile take in one call.
pub fn limits(memory_bytes: u32) -> Limits {
    limits_with_fuel(memory_bytes, config::DEFAULT_FUEL_PER_CALL)
}

/// [`limits`] with `fuel` per call (`MW_POSIX_NODE_FUEL_PER_CALL`).
pub fn limits_with_fuel(memory_bytes: u32, fuel: u64) -> Limits {
    Limits::bytes(memory_bytes)
        .with_fuel(fuel)
        .with_subscriptions(middleware::wasm_abi::SLOT_MAX as u8)
}
