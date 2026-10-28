//! Pure ABI decisions: everything the runtime decides about a module or a
//! host call that does not need the interpreter.
//!
//! The wasmi glue in [`crate::link`] and [`crate::Payload`] only translates
//! between interpreter types and the plain values here, so every rule of the
//! ABI in [`middleware::wasm_abi`] is unit tested without running WASM:
//!
//! * [`check_import`] / [`check_export`] — module shape (requirement W1),
//!   including the optional ABI 2 `on_irq(kind, source, value, time_us)`
//!   export.
//! * [`payload_key`], [`log_line`], [`pin`], [`level`], [`pull`],
//!   [`subscribe_slot`], [`set_tick`] — host-call argument validation (W3).
//! * [`alloc_range`] — whether `mw_alloc` accepted a delivery (W2).
//! * [`Schedule`] — `tick` scheduling with wrapping millisecond time (W2);
//!   [`tick_decision`] — whether a `tick` call runs, as a
//!   [`Ticked`], and the schedule after it (W9).
//! * [`FuelStats`] — per-payload fuel accounting: [`fuel_used`] derives
//!   one call's consumption from its budget and the fuel left, and
//!   [`FuelStats::record`] folds it into the counters (W4).
//! * [`initial_memory_bytes`], [`check_memory`], [`memory_bytes`] — the
//!   byte cap on linear memory for 64 KiB and 1-byte (custom-page-sizes)
//!   pages (W5).
//! * ABI 2 (requirement D4): [`gpio_irq`], [`dma_channel`],
//!   [`spi_transfer`], [`uart_write`] and [`read_target`] validate
//!   arguments with the shared [`wasm_abi::periph`] parsers and bound guest
//!   pointers with [`wasm_abi::guest_range`]; [`copied`] clamps what a
//!   device reports, and [`count_code`] keeps a `u32` count non-negative.
//!
//! * ABI 3: [`i2c_transfer`] validates `i2c_transfer` with
//!   [`periph::I2cTransfer::parse`] and bounds both guest buffers.
//! * ABI 4: [`route_source`] and [`route_sink`] validate the source kind or
//!   bus and the config length, then the key and config pointers, parsing
//!   with the shared [`wasm_abi::route`] parsers; [`route_handle_code`]
//!   checks the handle a device hands back.
//!
//! For the pointer-carrying ABI 2 and 3 imports, arguments are checked
//! before the pointer: a bad port, bus, address or length is
//! `InvalidArgument` even when the pointer is also out of range.

use alloc::string::{String, ToString};

use core::ops::Range;
use middleware::Key;

use middleware::wasm_abi::events::DmaChannel;
use middleware::wasm_abi::periph::{self, Edge, I2cTransfer};
use middleware::wasm_abi::route::{self, SinkConfig, SourceConfig, SourceKind};
use middleware::wasm_abi::{self, AbiError, MAX_LOG, MAX_SUBSCRIPTIONS, MIN_TICK_MS, Pull};

use crate::{LoadError, Ticked};

/// Size of one WASM page in bytes.
pub const PAGE_BYTES: usize = 65_536;

/// The shape of a function type, when every parameter and result is `i32`.
///
/// Functions using any other value type have no [`Shape`]; callers pass
/// `None` and the checks reject them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shape {
    /// Number of `i32` parameters.
    pub params: usize,
    /// Number of `i32` results.
    pub results: usize,
}

/// What kind of entity a module imports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportKind {
    /// A function; `None` when it uses a non-`i32` type.
    Func(Option<Shape>),
    /// A memory, table or global, none of which the host provides.
    Other,
}

/// Accepts an import only if it is a function in [`wasm_abi::IMPORT_MODULE`]
/// listed in [`wasm_abi::IMPORTS`] with exactly the listed signature.
pub fn check_import(module: &str, name: &str, kind: ImportKind) -> Result<(), LoadError> {
    let unknown = || LoadError::UnknownImport {
        module: module.to_string(),
        name: name.to_string(),
    };
    if module != wasm_abi::IMPORT_MODULE {
        return Err(unknown());
    }
    let spec = wasm_abi::import(name).ok_or_else(unknown)?;
    let ImportKind::Func(shape) = kind else {
        return Err(unknown());
    };
    let expected = Shape {
        params: spec.params,
        results: usize::from(spec.returns),
    };
    if shape == Some(expected) {
        Ok(())
    } else {
        Err(LoadError::BadSignature {
            name: name.to_string(),
        })
    }
}

/// The optional function exports the runtime calls, and their shapes.
const EXPORTS: &[(&str, Shape)] = &[
    (
        wasm_abi::exports::INIT,
        Shape {
            params: 0,
            results: 0,
        },
    ),
    (
        wasm_abi::exports::TICK,
        Shape {
            params: 1,
            results: 0,
        },
    ),
    (
        wasm_abi::exports::ALLOC,
        Shape {
            params: 1,
            results: 1,
        },
    ),
    (
        wasm_abi::exports::ON_MESSAGE,
        Shape {
            params: 3,
            results: 0,
        },
    ),
    (
        wasm_abi::exports::ON_IRQ,
        Shape {
            params: 4,
            results: 0,
        },
    ),
];

/// Checks an exported function. Exports the runtime does not call are
/// ignored; the ones it does call must have the ABI signature.
pub fn check_export(name: &str, shape: Option<Shape>) -> Result<(), LoadError> {
    match EXPORTS.iter().find(|(n, _)| *n == name) {
        Some((_, expected)) if shape != Some(*expected) => Err(LoadError::BadExport {
            name: name.to_string(),
        }),
        _ => Ok(()),
    }
}

/// Checks a module's initial linear memory, in bytes, against the byte cap.
pub fn check_memory(initial_bytes: u64, max_bytes: u32) -> Result<(), LoadError> {
    if initial_bytes > u64::from(max_bytes) {
        Err(LoadError::MemoryTooLarge {
            bytes: initial_bytes,
            max: max_bytes,
        })
    } else {
        Ok(())
    }
}

/// Converts the byte cap to the limit the store's resource limiter enforces
/// on `memory.grow`, saturating on targets with a small `usize`.
pub fn memory_bytes(max_bytes: u32) -> usize {
    usize::try_from(max_bytes).unwrap_or(usize::MAX)
}

/// Section id of the memory section.
const MEMORY_SECTION: u8 = 5;
/// Memory type flag: a maximum follows the minimum.
const FLAG_HAS_MAX: u32 = 1;
/// Memory type flag: a page-size exponent follows (custom-page-sizes).
const FLAG_PAGE_SIZE: u32 = 8;
/// log2 of the default 64 KiB page.
const DEFAULT_PAGE_LOG2: u32 = 16;

/// Reads an unsigned LEB128 value of at most 64 bits at `*at`, advancing it.
/// Reads at most ten bytes.
fn leb_u64(bytes: &[u8], at: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0u32..64).step_by(7) {
        let byte = *bytes.get(*at)?;
        *at += 1;
        value |= u64::from(byte & 0x7f).checked_shl(shift)?;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// Total initial size, in bytes, of the memories a module defines: each
/// memory's minimum times its page size (64 KiB, or 2^n bytes with the
/// custom-page-sizes proposal, which in practice means 1 byte). Returns
/// `Some(0)` when the module defines no memory and `None` when the section
/// headers or memory types are malformed.
///
/// wasmi validates the module but does not expose a memory's page size, so
/// the runtime reads it here. Only section headers and the memory section
/// are walked: every step consumes at least one byte, so the walk is bounded
/// by the module length. A product that overflows saturates to `u64::MAX`,
/// which any cap rejects.
pub fn initial_memory_bytes(wasm: &[u8]) -> Option<u64> {
    let mut at = 8; // magic and version
    while at < wasm.len() {
        let id = *wasm.get(at)?;
        at += 1;
        let len = usize::try_from(leb_u64(wasm, &mut at)?).ok()?;
        let end = at.checked_add(len).filter(|e| *e <= wasm.len())?;
        if id == MEMORY_SECTION {
            let section = wasm.get(at..end)?;
            return memory_section_bytes(section);
        }
        at = end;
    }
    Some(0)
}

/// Sums the initial bytes of every memory type in a memory section body.
fn memory_section_bytes(section: &[u8]) -> Option<u64> {
    let mut at = 0;
    let count = leb_u64(section, &mut at)?;
    let mut total = 0u64;
    // Each memory type takes at least two bytes, so a count beyond the
    // section length is malformed; bounding by the length keeps the loop
    // finite whatever the count says.
    for _ in 0..count.min(section.len() as u64) {
        let flags = u32::try_from(leb_u64(section, &mut at)?).ok()?;
        // `memory64` (flag 4) only widens the LEB values, which `leb_u64`
        // already reads.
        let min = leb_u64(section, &mut at)?;
        if flags & FLAG_HAS_MAX != 0 {
            leb_u64(section, &mut at)?;
        }
        let log2 = if flags & FLAG_PAGE_SIZE != 0 {
            u32::try_from(leb_u64(section, &mut at)?).ok()?
        } else {
            DEFAULT_PAGE_LOG2
        };
        let bytes = min
            .checked_shl(log2)
            .filter(|b| b >> log2 == min)
            .unwrap_or(u64::MAX);
        total = total.saturating_add(bytes);
    }
    Some(total)
}

/// Parses a key the payload passed to `publish` or `subscribe`: it must be
/// UTF-8, a valid [`Key`], and not under the reserved `sys/` namespace.
pub fn payload_key(bytes: &[u8]) -> Result<Key<'_>, AbiError> {
    let text = core::str::from_utf8(bytes).map_err(|_| AbiError::InvalidKey)?;
    let key = Key::new(text).map_err(|_| AbiError::InvalidKey)?;
    if key.segments().next() == Some("sys") {
        return Err(AbiError::InvalidKey);
    }
    Ok(key)
}

/// The text of a `log` line: at most [`MAX_LOG`] bytes, cut back to a UTF-8
/// character boundary. Invalid UTF-8 ends the line at the last valid byte.
pub fn log_line(bytes: &[u8]) -> &str {
    let bytes = bytes.get(..MAX_LOG).unwrap_or(bytes);
    match core::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => core::str::from_utf8(bytes.get(..e.valid_up_to()).unwrap_or_default())
            .unwrap_or_default(),
    }
}

/// Parses a GPIO pin number.
pub fn pin(raw: i32) -> Result<u8, AbiError> {
    u8::try_from(raw).map_err(|_| AbiError::InvalidArgument)
}

/// Parses a GPIO level, which must be exactly 0 or 1.
pub fn level(raw: i32) -> Result<bool, AbiError> {
    match raw {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(AbiError::InvalidArgument),
    }
}

/// Parses a [`Pull`] argument.
pub fn pull(raw: i32) -> Result<Pull, AbiError> {
    Pull::from_i32(raw).ok_or(AbiError::InvalidArgument)
}

/// Where a subscription lands in the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// The key was already subscribed at this slot.
    Existing(usize),
    /// The key takes this new slot (always the table length).
    New(usize),
}

impl Slot {
    /// The slot number handed back to the payload.
    pub const fn index(self) -> usize {
        match self {
            Slot::Existing(i) | Slot::New(i) => i,
        }
    }
}

/// Decides the slot for subscribing to `key`: re-subscribing returns the
/// existing slot, otherwise a new one unless [`MAX_SUBSCRIPTIONS`] are taken.
pub fn subscribe_slot(subscriptions: &[String], key: &str) -> Result<Slot, AbiError> {
    if let Some(i) = slot_for(subscriptions, key) {
        Ok(Slot::Existing(i))
    } else if subscriptions.len() >= MAX_SUBSCRIPTIONS {
        Err(AbiError::NoResources)
    } else {
        Ok(Slot::New(subscriptions.len()))
    }
}

/// The slot whose key matches `key` exactly.
pub fn slot_for(subscriptions: &[String], key: &str) -> Option<usize> {
    subscriptions.iter().position(|s| s == key)
}

/// A periodic `tick`: the period and the absolute `now_ms` it is next due.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schedule {
    /// Period in milliseconds, at least [`MIN_TICK_MS`].
    pub period: u32,
    /// Absolute time (wrapping milliseconds) of the next tick.
    pub due: u32,
}

impl Schedule {
    /// Whether the tick is due at `now`, comparing wrapping times within
    /// half the `u32` range.
    pub const fn is_due(self, now: u32) -> bool {
        (now.wrapping_sub(self.due) as i32) >= 0
    }

    /// The schedule after a tick fired at `now`: one period after the
    /// previous due time, or one period after `now` if the host fell more
    /// than a period behind (missed ticks are skipped, not replayed).
    pub const fn advance(self, now: u32) -> Schedule {
        let next = Schedule {
            period: self.period,
            due: self.due.wrapping_add(self.period),
        };
        if next.is_due(now) {
            Schedule {
                period: self.period,
                due: now.wrapping_add(self.period),
            }
        } else {
            next
        }
    }
}

/// Decides what [`Payload::tick`](crate::Payload::tick) at `now` does for a
/// module with (`has_tick`) or without a `tick` export and the current
/// `schedule`: [`Ticked::Ran`] with the advanced schedule when a tick is
/// due, otherwise the reason nothing runs and the unchanged schedule.
pub const fn tick_decision(
    has_tick: bool,
    schedule: Option<Schedule>,
    now: u32,
) -> (Ticked, Option<Schedule>) {
    match schedule {
        _ if !has_tick => (Ticked::NoTickExport, schedule),
        None => (Ticked::Disabled, None),
        Some(s) if s.is_due(now) => (Ticked::Ran, Some(s.advance(now))),
        Some(s) => (Ticked::NotDue, Some(s)),
    }
}

/// Decides the new schedule for `set_tick(period_ms)` called at `now`.
///
/// `0` disables ticking; periods below [`MIN_TICK_MS`] are rejected, and a
/// payload without a `tick` export cannot enable one.
pub fn set_tick(period_ms: i32, now: u32, has_tick: bool) -> Result<Option<Schedule>, AbiError> {
    if period_ms == 0 {
        return Ok(None);
    }
    if period_ms < MIN_TICK_MS {
        return Err(AbiError::InvalidArgument);
    }
    if !has_tick {
        return Err(AbiError::Unsupported);
    }
    let period = period_ms.unsigned_abs();
    Ok(Some(Schedule {
        period,
        due: now.wrapping_add(period),
    }))
}

/// The buffer `mw_alloc(len)` handed back, or `None` when the guest refused
/// the message (pointer 0) or returned a range outside `mem_len` bytes of
/// linear memory. Either way the message is simply not delivered.
pub fn alloc_range(ptr: i32, len: i32, mem_len: usize) -> Option<core::ops::Range<usize>> {
    if ptr == 0 {
        return None;
    }
    wasm_abi::guest_range(ptr, len, mem_len).ok()
}

/// Folds a host call outcome into the `i32` the import returns.
pub fn result_code(result: Result<i32, AbiError>) -> i32 {
    result.unwrap_or_else(AbiError::code)
}

/// Parses `gpio_irq(pin, edge)`.
pub fn gpio_irq(pin_raw: i32, edge: i32) -> Result<(u8, Edge), AbiError> {
    Ok((pin(pin_raw)?, Edge::from_i32(edge)?))
}

/// Parses the `channel` argument of `dma_read`.
pub fn dma_channel(raw: i32) -> Result<DmaChannel, AbiError> {
    DmaChannel::from_i32(raw).ok_or(AbiError::InvalidArgument)
}

/// A validated `spi_transfer` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpiTransfer {
    /// Bus number.
    pub bus: u8,
    /// Transmit bytes in guest memory (may be empty when only receiving).
    pub tx: Range<usize>,
    /// Bytes clocked: `max(tx_len, rx_len)`, `1..=MAX_DMA`.
    pub len: usize,
}

/// Plans `spi_transfer(bus, tx_ptr, tx_len, rx_len)` against `mem_len`
/// bytes of linear memory. `rx_len` may exceed `tx_len`; the device pads
/// transmit with `0x00`.
pub fn spi_transfer(
    bus: i32,
    tx_ptr: i32,
    tx_len: i32,
    rx_len: i32,
    mem_len: usize,
) -> Result<SpiTransfer, AbiError> {
    let bus = periph::spi_bus(bus)?;
    let len = periph::spi_transfer_len(tx_len, rx_len)?;
    let tx = wasm_abi::guest_range(tx_ptr, tx_len, mem_len)?;
    Ok(SpiTransfer { bus, tx, len })
}

/// Plans `uart_write(port, ptr, len)`: returns the port and the guest bytes.
pub fn uart_write(
    port: i32,
    ptr: i32,
    len: i32,
    mem_len: usize,
) -> Result<(u8, Range<usize>), AbiError> {
    let port = periph::uart_port(port)?;
    periph::uart_write_len(len)?;
    Ok((port, wasm_abi::guest_range(ptr, len, mem_len)?))
}

/// A validated `i2c_transfer` request (ABI 3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct I2cPlan {
    /// Bus, address and lengths.
    pub transfer: I2cTransfer,
    /// Bytes to write, in guest memory (empty for a read-only transfer).
    pub tx: Range<usize>,
    /// Where the read bytes go, in guest memory (empty for write-only).
    pub rx: Range<usize>,
}

/// Plans `i2c_transfer(bus, addr7, tx_ptr, tx_len, rx_ptr, rx_len)` against
/// `mem_len` bytes of linear memory: arguments first, then both pointers
/// (a zero-length buffer still needs an in-bounds pointer).
pub fn i2c_transfer(
    (bus, addr7, tx_ptr, tx_len, rx_ptr, rx_len): (i32, i32, i32, i32, i32, i32),
    mem_len: usize,
) -> Result<I2cPlan, AbiError> {
    let transfer = I2cTransfer::parse(bus, addr7, tx_len, rx_len)?;
    let tx = wasm_abi::guest_range(tx_ptr, tx_len, mem_len)?;
    let rx = wasm_abi::guest_range(rx_ptr, rx_len, mem_len)?;
    Ok(I2cPlan { transfer, tx, rx })
}

/// A validated `route_source` request (ABI 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourcePlan<'m> {
    /// The data source.
    pub source: SourceKind,
    /// The publish key (not under `sys/`).
    pub key: Key<'m>,
    /// Frame geometry, rate and chunk size.
    pub cfg: SourceConfig,
}

/// Plans `route_source(source, key_ptr, key_len, cfg_ptr, cfg_len)` against
/// linear memory `mem`: the source kind and config length first, then the
/// key and config pointers, then their contents.
pub fn route_source(
    (source, kp, kl, cp, cl): (i32, i32, i32, i32, i32),
    mem: &[u8],
) -> Result<SourcePlan<'_>, AbiError> {
    let source = SourceKind::from_i32(source)?;
    if usize::try_from(cl).ok() != Some(route::SOURCE_CFG_LEN) {
        return Err(AbiError::InvalidArgument);
    }
    let key = mem
        .get(wasm_abi::guest_range(kp, kl, mem.len())?)
        .ok_or(AbiError::OutOfBounds)?;
    let cfg = mem
        .get(wasm_abi::guest_range(cp, cl, mem.len())?)
        .ok_or(AbiError::OutOfBounds)?;
    Ok(SourcePlan {
        source,
        key: payload_key(key)?,
        cfg: SourceConfig::parse(cfg)?,
    })
}

/// A validated `route_sink` request (ABI 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SinkPlan<'m> {
    /// The subscribed key (not under `sys/`).
    pub key: Key<'m>,
    /// SPI bus number.
    pub bus: u8,
    /// D/C pin, frame length and preamble.
    pub cfg: SinkConfig,
}

/// Plans `route_sink(key_ptr, key_len, bus, cfg_ptr, cfg_len)` against
/// linear memory `mem`: the bus and config length (at most
/// [`route::MAX_SINK_CFG_LEN`]) first, then the pointers, then contents.
pub fn route_sink(
    (kp, kl, bus, cp, cl): (i32, i32, i32, i32, i32),
    mem: &[u8],
) -> Result<SinkPlan<'_>, AbiError> {
    let bus = periph::spi_bus(bus)?;
    let len = usize::try_from(cl).map_err(|_| AbiError::InvalidArgument)?;
    if !(route::SINK_HEADER_LEN..=route::MAX_SINK_CFG_LEN).contains(&len) {
        return Err(AbiError::InvalidArgument);
    }
    let key = mem
        .get(wasm_abi::guest_range(kp, kl, mem.len())?)
        .ok_or(AbiError::OutOfBounds)?;
    let cfg = mem
        .get(wasm_abi::guest_range(cp, cl, mem.len())?)
        .ok_or(AbiError::OutOfBounds)?;
    Ok(SinkPlan {
        key: payload_key(key)?,
        bus,
        cfg: SinkConfig::parse(cfg)?,
    })
}

/// The import result for a route handle a device returned: the handle, or
/// `NoResources` if the device answered outside `0..MAX_ROUTES`.
pub fn route_handle_code(handle: u8) -> Result<i32, AbiError> {
    if usize::from(handle) < route::MAX_ROUTES {
        Ok(i32::from(handle))
    } else {
        Err(AbiError::NoResources)
    }
}

/// The guest range a read import (`dma_read`, `uart_read`) may fill: the
/// whole `(ptr, len)` must lie in memory, and at most `cap` bytes (the host
/// buffer size) are written from its start.
pub fn read_target(
    ptr: i32,
    len: i32,
    mem_len: usize,
    cap: usize,
) -> Result<Range<usize>, AbiError> {
    let range = wasm_abi::guest_range(ptr, len, mem_len)?;
    let n = range.len().min(cap);
    Ok(range.start..range.start + n)
}

/// Bytes actually copied back to the guest: what the device reported,
/// clamped to the buffer it was given.
pub const fn copied(reported: usize, buffer: usize) -> usize {
    if reported < buffer { reported } else { buffer }
}

/// A `u32` count as an import result, saturating so it is never mistaken
/// for a negative error code.
pub fn count_code(count: u32) -> i32 {
    i32::try_from(count).unwrap_or(i32::MAX)
}

/// Fuel consumed by the calls into one payload (requirement W4).
///
/// Every export call (`init`, `tick`, `mw_alloc`, `on_message`, `on_irq`)
/// is recorded, including calls that trap; a call that runs out of fuel
/// counts as its whole budget. Read with [`crate::Payload::fuel_stats`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FuelStats {
    /// Number of export calls made.
    pub calls: u64,
    /// The most fuel any single call used.
    pub max_call: u64,
    /// Fuel used by all calls together, saturating at `u64::MAX`.
    pub total: u64,
}

impl FuelStats {
    /// The counters after one more call that used `used` fuel.
    #[must_use]
    pub const fn record(self, used: u64) -> Self {
        FuelStats {
            calls: self.calls.saturating_add(1),
            max_call: if used > self.max_call {
                used
            } else {
                self.max_call
            },
            total: self.total.saturating_add(used),
        }
    }
}

/// Fuel one call used: its `budget` minus the fuel `remaining` afterwards.
/// `None` (the interpreter could not report the fuel left) counts as the
/// whole budget, as does running dry.
pub const fn fuel_used(budget: u64, remaining: Option<u64>) -> u64 {
    match remaining {
        Some(left) => budget.saturating_sub(left),
        None => budget,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    const LOG: Shape = Shape {
        params: 2,
        results: 0,
    };

    #[test]
    fn imports_must_be_known_functions_with_exact_signatures() {
        assert_eq!(
            check_import("mw", "log", ImportKind::Func(Some(LOG))),
            Ok(())
        );
        assert!(matches!(
            check_import("env", "log", ImportKind::Func(Some(LOG))),
            Err(LoadError::UnknownImport { .. })
        ));
        assert!(matches!(
            check_import("mw", "fopen", ImportKind::Func(Some(LOG))),
            Err(LoadError::UnknownImport { .. })
        ));
        assert!(matches!(
            check_import("mw", "log", ImportKind::Other),
            Err(LoadError::UnknownImport { .. })
        ));
        assert!(matches!(
            check_import(
                "mw",
                "log",
                ImportKind::Func(Some(Shape {
                    params: 2,
                    results: 1
                }))
            ),
            Err(LoadError::BadSignature { .. })
        ));
        assert!(matches!(
            check_import("mw", "log", ImportKind::Func(None)),
            Err(LoadError::BadSignature { .. })
        ));
        for i in wasm_abi::IMPORTS {
            let shape = Shape {
                params: i.params,
                results: usize::from(i.returns),
            };
            assert_eq!(
                check_import("mw", i.name, ImportKind::Func(Some(shape))),
                Ok(())
            );
        }
    }

    #[test]
    fn known_exports_must_match_and_others_are_ignored() {
        assert_eq!(
            check_export(
                "tick",
                Some(Shape {
                    params: 1,
                    results: 0
                })
            ),
            Ok(())
        );
        assert!(matches!(
            check_export(
                "tick",
                Some(Shape {
                    params: 0,
                    results: 0
                })
            ),
            Err(LoadError::BadExport { .. })
        ));
        assert!(matches!(
            check_export("mw_alloc", None),
            Err(LoadError::BadExport { .. })
        ));
        assert_eq!(check_export("helper", None), Ok(()));
        let irq = Shape {
            params: 4,
            results: 0,
        };
        assert_eq!(check_export("on_irq", Some(irq)), Ok(()));
        assert!(matches!(
            check_export(
                "on_irq",
                Some(Shape {
                    params: 3,
                    results: 0
                })
            ),
            Err(LoadError::BadExport { .. })
        ));
    }

    #[test]
    fn irq_and_channel_arguments() {
        assert_eq!(gpio_irq(4, 3), Ok((4, Edge::Any)));
        assert_eq!(gpio_irq(4, 4), Err(AbiError::InvalidArgument));
        assert_eq!(gpio_irq(-1, 1), Err(AbiError::InvalidArgument));
        assert_eq!(dma_channel(1), Ok(DmaChannel::Uart1Tx));
        assert_eq!(dma_channel(2), Err(AbiError::InvalidArgument));
    }

    #[test]
    fn spi_transfer_checks_arguments_before_pointer() {
        assert_eq!(
            spi_transfer(0, 8, 2, 6, 100),
            Ok(SpiTransfer {
                bus: 0,
                tx: 8..10,
                len: 6
            })
        );
        assert_eq!(
            spi_transfer(0, 500, 0, 4, 100),
            Err(AbiError::OutOfBounds),
            "an empty tx still needs an in-memory pointer"
        );
        assert_eq!(spi_transfer(0, 98, 4, 0, 100), Err(AbiError::OutOfBounds));
        assert_eq!(
            spi_transfer(1, 0, 4, 0, 100),
            Err(AbiError::InvalidArgument)
        );
        assert_eq!(
            spi_transfer(0, 500, periph::MAX_DMA as i32 + 1, 0, 100),
            Err(AbiError::InvalidArgument)
        );
        assert_eq!(
            spi_transfer(0, 0, 0, 0, 100),
            Err(AbiError::InvalidArgument)
        );
    }

    #[test]
    fn uart_write_and_read_targets() {
        assert_eq!(uart_write(1, 4, 3, 100), Ok((1, 4..7)));
        assert_eq!(uart_write(0, 4, 3, 100), Err(AbiError::InvalidArgument));
        assert_eq!(uart_write(1, 4, 0, 100), Err(AbiError::InvalidArgument));
        assert_eq!(uart_write(1, 99, 3, 100), Err(AbiError::OutOfBounds));
        assert_eq!(read_target(10, 50, 100, 16), Ok(10..26));
        assert_eq!(read_target(10, 5, 100, 16), Ok(10..15));
        assert_eq!(read_target(90, 50, 100, 16), Err(AbiError::OutOfBounds));
        assert_eq!(read_target(0, -1, 100, 16), Err(AbiError::OutOfBounds));
        assert_eq!(copied(3, 8), 3);
        assert_eq!(copied(30, 8), 8);
        assert_eq!(count_code(7), 7);
        assert_eq!(count_code(u32::MAX), i32::MAX);
    }

    #[test]
    fn memory_cap_is_checked_in_bytes() {
        assert_eq!(
            check_memory(2 * PAGE_BYTES as u64, 2 * PAGE_BYTES as u32),
            Ok(())
        );
        assert_eq!(check_memory(4096, 4096), Ok(()));
        assert!(matches!(
            check_memory(4097, 4096),
            Err(LoadError::MemoryTooLarge {
                bytes: 4097,
                max: 4096
            })
        ));
        assert_eq!(memory_bytes(4096), 4096);
        assert_eq!(memory_bytes(u32::MAX), u32::MAX as usize);
    }

    /// A minimal module: header plus one memory section with `memtype`.
    fn with_memory(memtype: &[u8]) -> Vec<u8> {
        let mut m = b"\0asm\x01\0\0\0".to_vec();
        // An unrelated custom section first, to exercise the section walk.
        m.extend_from_slice(&[0, 2, 1, b'x']);
        m.push(MEMORY_SECTION);
        m.push(memtype.len() as u8 + 1);
        m.push(1);
        m.extend_from_slice(memtype);
        m
    }

    #[test]
    fn initial_memory_bytes_reads_both_page_sizes() {
        // 64 KiB pages: min 2.
        assert_eq!(
            initial_memory_bytes(&with_memory(&[0, 2])),
            Some(2 * PAGE_BYTES as u64)
        );
        // 64 KiB pages with a maximum: min 1, max 3.
        assert_eq!(
            initial_memory_bytes(&with_memory(&[1, 1, 3])),
            Some(PAGE_BYTES as u64)
        );
        // 1-byte pages: flags 8, min 8192 (LEB 0x80 0x40), log2 0.
        assert_eq!(
            initial_memory_bytes(&with_memory(&[8, 0x80, 0x40, 0])),
            Some(8192)
        );
        // 1-byte pages with a maximum: flags 9, min 300, max 300.
        assert_eq!(
            initial_memory_bytes(&with_memory(&[9, 0xac, 0x02, 0xac, 0x02, 0])),
            Some(300)
        );
        // No memory section.
        assert_eq!(initial_memory_bytes(b"\0asm\x01\0\0\0"), Some(0));
    }

    #[test]
    fn initial_memory_bytes_rejects_malformed_and_saturates() {
        assert_eq!(initial_memory_bytes(&with_memory(&[8, 0x80])), None);
        let mut truncated = with_memory(&[0, 2]);
        truncated.pop();
        assert_eq!(initial_memory_bytes(&truncated), None);
        let mut bad_len = b"\0asm\x01\0\0\0".to_vec();
        bad_len.extend_from_slice(&[MEMORY_SECTION, 50, 1, 0, 1]);
        assert_eq!(initial_memory_bytes(&bad_len), None);
        // A huge exponent saturates instead of wrapping.
        assert_eq!(
            initial_memory_bytes(&with_memory(&[8, 2, 63])),
            Some(u64::MAX)
        );
    }

    #[test]
    fn keys_reject_sys_and_malformed() {
        assert_eq!(payload_key(b"sensor/t").map(|k| k.as_str()), Ok("sensor/t"));
        assert_eq!(
            payload_key(b"system/ok").map(|k| k.as_str()),
            Ok("system/ok")
        );
        assert_eq!(payload_key(b"sys/log"), Err(AbiError::InvalidKey));
        assert_eq!(payload_key(b"sys"), Err(AbiError::InvalidKey));
        assert_eq!(payload_key(b"a//b"), Err(AbiError::InvalidKey));
        assert_eq!(payload_key(&[0xff]), Err(AbiError::InvalidKey));
    }

    #[test]
    fn log_lines_truncate_on_char_boundary() {
        assert_eq!(log_line(b"hello"), "hello");
        let mut long: Vec<u8> = Vec::new();
        long.extend_from_slice(&[b'a'; MAX_LOG - 1]);
        long.extend_from_slice("é".as_bytes()); // straddles the limit
        assert_eq!(log_line(&long).len(), MAX_LOG - 1);
        assert_eq!(log_line(&[b'a'; MAX_LOG + 10]).len(), MAX_LOG);
        assert_eq!(log_line(b"ok\xffbad"), "ok");
    }

    #[test]
    fn gpio_arguments_are_range_checked() {
        assert_eq!(pin(5), Ok(5));
        assert_eq!(pin(256), Err(AbiError::InvalidArgument));
        assert_eq!(pin(-1), Err(AbiError::InvalidArgument));
        assert_eq!(level(1), Ok(true));
        assert_eq!(level(2), Err(AbiError::InvalidArgument));
        assert_eq!(pull(1), Ok(Pull::Up));
        assert_eq!(pull(3), Err(AbiError::InvalidArgument));
    }

    #[test]
    fn subscriptions_dedupe_and_cap() {
        let mut subs: Vec<String> = Vec::new();
        assert_eq!(subscribe_slot(&subs, "a"), Ok(Slot::New(0)));
        subs.push("a".into());
        assert_eq!(subscribe_slot(&subs, "a"), Ok(Slot::Existing(0)));
        for i in 1..MAX_SUBSCRIPTIONS {
            subs.push(alloc::format!("k{i}"));
        }
        assert_eq!(subscribe_slot(&subs, "z"), Err(AbiError::NoResources));
        assert_eq!(subscribe_slot(&subs, "a"), Ok(Slot::Existing(0)));
        assert_eq!(slot_for(&subs, "k3"), Some(3));
        assert_eq!(slot_for(&subs, "k"), None);
    }

    #[test]
    fn w9_tick_decision_reports_why_nothing_ran() {
        let s = Schedule {
            period: 100,
            due: 50,
        };
        assert_eq!(
            tick_decision(true, Some(s), 50),
            (Ticked::Ran, Some(s.advance(50)))
        );
        assert_eq!(tick_decision(true, Some(s), 49), (Ticked::NotDue, Some(s)));
        assert_eq!(tick_decision(true, None, 50), (Ticked::Disabled, None));
        // `set_tick` refuses a schedule without the export, but the decision
        // does not depend on that.
        assert_eq!(
            tick_decision(false, Some(s), 50),
            (Ticked::NoTickExport, Some(s))
        );
        assert_eq!(tick_decision(false, None, 50), (Ticked::NoTickExport, None));
    }

    #[test]
    fn tick_schedule_wraps_and_skips_missed_ticks() {
        assert_eq!(set_tick(0, 5, true), Ok(None));
        assert_eq!(
            set_tick(MIN_TICK_MS - 1, 5, true),
            Err(AbiError::InvalidArgument)
        );
        assert_eq!(set_tick(-100, 5, true), Err(AbiError::InvalidArgument));
        assert_eq!(set_tick(100, 5, false), Err(AbiError::Unsupported));
        let s = set_tick(100, u32::MAX - 49, true).ok().flatten();
        assert_eq!(
            s,
            Some(Schedule {
                period: 100,
                due: 50
            })
        );
        let s = Schedule {
            period: 100,
            due: 50,
        };
        assert!(!s.is_due(u32::MAX));
        assert!(s.is_due(50));
        assert_eq!(
            s.advance(60),
            Schedule {
                period: 100,
                due: 150
            }
        );
        assert_eq!(
            s.advance(1000),
            Schedule {
                period: 100,
                due: 1100
            }
        );
    }

    #[test]
    fn alloc_zero_or_out_of_range_refuses() {
        assert_eq!(alloc_range(0, 4, 100), None);
        assert_eq!(alloc_range(98, 4, 100), None);
        assert_eq!(alloc_range(-8, 4, 100), None);
        assert_eq!(alloc_range(8, 4, 100), Some(8..12));
    }

    #[test]
    fn result_codes() {
        assert_eq!(result_code(Ok(3)), 3);
        assert_eq!(result_code(Err(AbiError::OutOfBounds)), -2);
    }

    #[test]
    fn route_source_checks_arguments_before_pointers() {
        let mut mem = vec![0u8; 64];
        mem[..7].copy_from_slice(b"app/cam");
        let cfg = SourceConfig::new(120, 120, 100).expect("valid").encode();
        mem[16..26].copy_from_slice(&cfg);
        let plan = route_source((0, 0, 7, 16, 10), &mem).expect("valid");
        assert_eq!(plan.source, SourceKind::TestPattern);
        assert_eq!(plan.key.as_str(), "app/cam");
        assert_eq!(plan.cfg.frame_len(), 28_800);
        let bad = Err(AbiError::InvalidArgument);
        assert_eq!(route_source((2, 0, 7, 16, 10), &mem), bad, "source");
        assert_eq!(
            route_source((2, 500, 7, 500, 10), &mem),
            bad,
            "source first"
        );
        assert_eq!(route_source((0, 0, 7, 16, 9), &mem), bad, "cfg len");
        assert_eq!(route_source((0, 0, 7, 500, 11), &mem), bad, "cfg len first");
        assert_eq!(
            route_source((0, 60, 7, 16, 10), &mem),
            Err(AbiError::OutOfBounds)
        );
        assert_eq!(
            route_source((0, 0, 7, 60, 10), &mem),
            Err(AbiError::OutOfBounds)
        );
        mem[..3].copy_from_slice(b"sys");
        assert_eq!(
            route_source((0, 0, 7, 16, 10), &mem),
            Err(AbiError::InvalidKey)
        );
        mem[..3].copy_from_slice(b"app");
        mem[20] = 2; // pixel format
        assert_eq!(route_source((1, 0, 7, 16, 10), &mem), bad, "cfg contents");
        mem[20] = 1; // JPEG
        let jpeg = route_source((0, 0, 7, 16, 10), &mem).expect("JPEG is valid");
        assert_eq!(jpeg.cfg.format, route::PixelFormat::Jpeg);
    }

    #[test]
    fn route_sink_checks_arguments_before_pointers() {
        let mut mem = vec![0u8; 128];
        mem[..7].copy_from_slice(b"app/cam");
        let cfg = SinkConfig::new(13, 28_800)
            .expect("valid")
            .with_preamble(route::Preamble::mipi_window(60, 60, 179, 179).expect("fits"));
        let mut buf = [0u8; route::MAX_SINK_CFG_LEN];
        let n = cfg.encode(&mut buf);
        mem[16..16 + n].copy_from_slice(&buf[..n]);
        let plan = route_sink((0, 7, 0, 16, n as i32), &mem).expect("valid");
        assert_eq!((plan.key.as_str(), plan.bus), ("app/cam", 0));
        assert_eq!(plan.cfg, cfg);
        let bad = Err(AbiError::InvalidArgument);
        assert_eq!(
            route_sink((500, 7, 1, 500, n as i32), &mem),
            bad,
            "bus first"
        );
        assert_eq!(route_sink((500, 7, 0, 500, 6), &mem), bad, "cfg too short");
        let long = route::MAX_SINK_CFG_LEN as i32 + 1;
        assert_eq!(
            route_sink((500, 7, 0, 500, long), &mem),
            bad,
            "cfg too long"
        );
        assert_eq!(route_sink((0, 7, 0, 16, -1), &mem), bad);
        assert_eq!(
            route_sink((0, 7, 0, 120, n as i32), &mem),
            Err(AbiError::OutOfBounds)
        );
        assert_eq!(
            route_sink((0, 7, 0, 16, n as i32 - 1), &mem),
            bad,
            "cut segment"
        );
        assert_eq!(route_handle_code(3), Ok(3));
        assert_eq!(route_handle_code(4), Err(AbiError::NoResources));
    }

    #[test]
    fn i2c_transfer_checks_arguments_before_pointers() {
        let plan = i2c_transfer((1, 0x50, 16, 2, 32, 4), 64).expect("valid");
        assert_eq!(plan.tx, 16..18);
        assert_eq!(plan.rx, 32..36);
        assert_eq!((plan.transfer.bus, plan.transfer.addr), (1, 0x50));
        assert_eq!(
            i2c_transfer((0, 0x80, -1, 1, 0, 0), 64),
            Err(AbiError::InvalidArgument),
            "bad address wins over bad pointer"
        );
        assert_eq!(
            i2c_transfer((0, 0x50, 62, 4, 0, 0), 64),
            Err(AbiError::OutOfBounds)
        );
        assert_eq!(
            i2c_transfer((0, 0x50, 0, 0, 65, 0), 64),
            Err(AbiError::OutOfBounds),
            "an empty buffer still needs an in-bounds pointer"
        );
        assert_eq!(
            i2c_transfer((0, 0x50, 0, 0, 0, 65), 64),
            Err(AbiError::InvalidArgument),
            "over MAX_I2C"
        );
    }

    #[test]
    fn fuel_stats_track_calls_max_and_saturating_total() {
        let s = FuelStats::default().record(10).record(30).record(20);
        assert_eq!(
            s,
            FuelStats {
                calls: 3,
                max_call: 30,
                total: 60
            }
        );
        assert_eq!(s.record(u64::MAX).total, u64::MAX);
        assert_eq!(fuel_used(100, Some(40)), 60);
        assert_eq!(fuel_used(100, Some(0)), 100);
        assert_eq!(fuel_used(100, None), 100);
        assert_eq!(fuel_used(100, Some(200)), 0);
    }
}
