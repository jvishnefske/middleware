//! Pure core of the ESP base firmware.
//!
//! Everything here is deterministic decision logic with no hardware access,
//! so it is unit tested on the host (`cargo test --lib`, no chip feature):
//!
//! * [`chip`] — per-chip profiles: name, heap plan, WASM memory limit,
//!   largest payload image, and the GPIO tables that decide which pins a
//!   payload may touch (flash/PSRAM, USB D+/D-, and console UART pins are
//!   refused).
//! * [`camera`] — the R15 camera capture decisions: the ESP32 I2S
//!   camera-mode sample layout ([`camera::extract`]) and the JPEG frame
//!   assembler that finds `FF D8` … `FF D9` across DMA buffers.
//! * [`image`] — layout of the payload flash partition: two slots, each a
//!   header sector (magic, length, CRC-32, generation) followed by the
//!   image, the boot-slot choice by generation, erase spans, and a
//!   word aligner that turns in-order loader chunks into 4-byte aligned flash
//!   writes.
//! * [`store`] — [`store::PayloadStore`], the loader's storage effects
//!   (erase, ordered writes, activate) into the inactive slot and boot-time
//!   slot selection and verification over any `embedded-storage` NOR flash;
//!   tested against a RAM flash model with power-loss injection.
//! * [`dispatch`] — request dispatch: `(State, request) -> (State, reply,
//!   Action)`, built on [`middleware::loader::Loader`], plus the payload
//!   lifecycle transitions, the signed-commit and key-claim decisions
//!   (feature `trust`, on by default) and publication routing.
//! * [`trust_store`] — the pinned signing key and `min_version` on the
//!   `trust` partition: two alternately written sectors, magic, format
//!   version and CRC-32, safe against power loss at any flash operation.
//! * [`presence`] — the BOOT-button hold detector that asserts physical
//!   presence for the re-key window.
//! * [`perf`] — the payload loop's time accounting: wall time and fuel per
//!   payload operation, waits, yields and the radio task's time, folded into
//!   a reporting window and logged as two `sys/log` lines.
//! * [`periph`] — the ABI 2 peripheral decisions: pin claims, the timer
//!   deadline table, the DMA channel state machine, the UART receive ring,
//!   and the events ISRs and completions push.
//! * [`radio`] — the ESP-NOW policy: the fixed channel, the `app` key
//!   filter for what goes over the air, queue depths and drop counters,
//!   the request responder's cache size and the unicast peer table.
//! * [`ota`] — over-the-air payload updates: which requests a radio peer
//!   may make (claimed devices only, update keys only, signed commits) and
//!   which requester owns a loader transfer.
//! * [`route`] — the ABI 4 DMA routes: source-route pacing and the
//!   in-place ESP-NOW chunk frame, the sink route's bus transfers, the
//!   route table (handles, keys, owned bus and D/C pin) and the source
//!   table the radio task polls.
//! * [`start`] — the heap check before a payload starts: free heap must
//!   cover the payload's bound (`wasm_host::Limits::heap_needed`) plus
//!   a margin.
//!
//! The imperative shell (`src/main.rs` and `src/shell/`) is only compiled
//! with a chip feature (`esp32`, `esp32s3`, `esp32c3`). It owns the UARTs,
//! USB-Serial-JTAG, flash and GPIO, performs the [`dispatch::Action`]s and
//! runs the payload.
#![no_std]
#![forbid(unsafe_code)]

pub mod camera;
pub mod chip;
pub mod dispatch;
pub mod image;
pub mod ota;
pub mod perf;
pub mod periph;
pub mod presence;
pub mod radio;
#[cfg(test)]
mod ram_flash;
pub mod route;
pub mod start;
pub mod store;
pub mod trust_store;
