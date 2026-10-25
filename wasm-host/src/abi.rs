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
//! * [`Schedule`] — `tick` scheduling with wrapping millisecond time (W2).
//! * [`memory_bytes`] — page cap to byte limit (W5).
//! * ABI 2 (requirement D4): [`gpio_irq`], [`dma_channel`],
//!   [`spi_transfer`], [`uart_write`] and [`read_target`] validate
//!   arguments with the shared [`wasm_abi::periph`] parsers and bound guest
//!   pointers with [`wasm_abi::guest_range`]; [`copied`] clamps what a
//!   device reports, and [`count_code`] keeps a `u32` count non-negative.
//!
//! For the pointer-carrying ABI 2 imports, arguments are checked before the
//! pointer: a bad port, bus or length is `InvalidArgument` even when the
//! pointer is also out of range.

use alloc::string::{String, ToString};

use core::ops::Range;
use middleware::Key;

use middleware::wasm_abi::events::DmaChannel;
use middleware::wasm_abi::periph::{self, Edge};
use middleware::wasm_abi::{self, AbiError, MAX_LOG, MAX_SUBSCRIPTIONS, MIN_TICK_MS, Pull};

use crate::LoadError;

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

/// Checks the exported memory's initial size against the page cap.
pub fn check_memory(initial_pages: u64, max_pages: u32) -> Result<(), LoadError> {
    if initial_pages > u64::from(max_pages) {
        Err(LoadError::MemoryTooLarge {
            pages: initial_pages,
            max: max_pages,
        })
    } else {
        Ok(())
    }
}

/// Converts a page cap to the byte limit the store's resource limiter
/// enforces on `memory.grow`, saturating on targets with a small `usize`.
pub fn memory_bytes(pages: u32) -> usize {
    usize::try_from(pages)
        .ok()
        .and_then(|p| p.checked_mul(PAGE_BYTES))
        .unwrap_or(usize::MAX)
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

#[cfg(test)]
mod tests {
    use super::*;
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
    fn memory_cap_is_checked_and_converted() {
        assert_eq!(check_memory(2, 2), Ok(()));
        assert!(matches!(
            check_memory(3, 2),
            Err(LoadError::MemoryTooLarge { pages: 3, max: 2 })
        ));
        assert_eq!(memory_bytes(2), 2 * PAGE_BYTES);
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
}
