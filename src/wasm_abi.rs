//! The WASM payload ABI: what a payload may import from, and must export to,
//! the base firmware.
//!
//! This module is the single source of truth. The firmware runtime
//! (`wasm-host`), the desktop simulator, the Rust payload SDK and the
//! browser simulator (through `protocol-wasm`) all read these definitions.
//!
//! # Imports (module [`IMPORT_MODULE`] = `"mw"`)
//!
//! All parameters and results are `i32`. Pointers are offsets into the
//! payload's exported `memory`. A negative result is an [`AbiError`] code.
//!
//! | name           | params                                   | result            |
//! |----------------|------------------------------------------|-------------------|
//! | `log`          | `ptr, len` (UTF-8)                        | —                 |
//! | `publish`      | `key_ptr, key_len, data_ptr, data_len`    | 0 or error        |
//! | `subscribe`    | `key_ptr, key_len`                        | slot ≥ 0 or error |
//! | `now_ms`       | —                                        | ms since start (wraps) |
//! | `set_tick`     | `period_ms` (0 disables)                  | 0 or error        |
//! | `gpio_output`  | `pin`                                    | 0 or error        |
//! | `gpio_input`   | `pin, pull` ([`Pull`])                    | 0 or error        |
//! | `gpio_write`   | `pin, level` (0/1)                        | 0 or error        |
//! | `gpio_read`    | `pin`                                    | 0/1 or error      |
//!
//! Added in ABI 2 (interrupts and DMA, see [`events`] and [`periph`]):
//!
//! | name           | params                                   | result            |
//! |----------------|------------------------------------------|-------------------|
//! | `now_us`       | —                                        | µs since start (wraps) |
//! | `irq_overflow` | —                                        | events dropped since last call |
//! | `gpio_irq`     | `pin, edge` ([`periph::Edge`]; pin must be an input) | 0 or error |
//! | `timer_start`  | `id, period_us, periodic` (0/1)           | 0 or error        |
//! | `timer_stop`   | `id`                                     | 0 or error        |
//! | `spi_open`     | `bus, sck, mosi, miso, cs, freq_khz, mode` (pin -1 = unused) | 0 or error |
//! | `spi_transfer` | `bus, tx_ptr, tx_len, rx_len`             | 0 (started) or error |
//! | `dma_read`     | `channel, ptr, len`                       | bytes copied or error |
//! | `uart_open`    | `port, tx, rx, baud`                      | 0 or error        |
//! | `uart_write`   | `port, ptr, len`                          | 0 (started) or error |
//! | `uart_read`    | `port, ptr, len`                          | bytes copied (may be 0) or error |
//!
//! Added in ABI 3 (PWM, ADC, I2C; arguments validated by [`periph`]):
//!
//! | name           | params                                   | result            |
//! |----------------|------------------------------------------|-------------------|
//! | `pwm_open`     | `timer, freq_hz`                          | 0 or error        |
//! | `pwm_set`      | `timer, channel, duty` (`0..=65535`, 65535 = always on) | 0 or error |
//! | `pwm_stop`     | `timer`                                  | 0 or error        |
//! | `adc_read`     | `pin`                                    | raw `0..=4095` or error |
//! | `i2c_open`     | `bus, freq_hz`                            | 0 or error        |
//! | `i2c_transfer` | `bus, addr7, tx_ptr, tx_len, rx_ptr, rx_len` | 0 or error     |
//!
//! Added in ABI 4 (DMA routes between pub/sub keys and host data sources or
//! SPI displays; arguments and config layouts in [`route`], chunk format
//! and sink rules in [`crate::stream`]):
//!
//! | name           | params                                   | result            |
//! |----------------|------------------------------------------|-------------------|
//! | `route_source` | `source, key_ptr, key_len, cfg_ptr, cfg_len` ([`route::SourceKind`], [`route::SourceConfig`], camera pins [`route::CameraPins`]) | handle ≥ 0 or error |
//! | `route_sink`   | `key_ptr, key_len, bus, cfg_ptr, cfg_len` ([`route::SinkConfig`]) | handle ≥ 0 or error |
//! | `route_stop`   | `handle`                                 | 0 or error        |
//! | `route_stat`   | `handle, stat` ([`route::RouteStat`])     | count ≥ 0 (saturating) or error |
//!
//! A route moves frames without copying them through guest memory: a
//! source route publishes host-captured frames on a key as chunks, a sink
//! route DMAs a key's chunks to an SPI bus with a per-frame preamble.
//! Routes report through `on_irq` ([`events::EventKind::RouteFrames`],
//! [`events::EventKind::RouteDropped`], [`events::EventKind::RouteStopped`]),
//! so `route_source` and `route_sink` are event sources.
//!
//! The firmware owns the pin mapping for PWM and I2C: a PWM `(timer,
//! channel)` and an I2C `bus` drive fixed, board-documented pins, so a
//! payload never names them. `adc_read` takes a board pin number and fails
//! with `Unsupported` on a pin without an analog input. `i2c_transfer` is
//! synchronous: it writes `tx_len` bytes, issues a repeated start and reads
//! `rx_len` bytes (either length may be 0, at most
//! [`periph::MAX_I2C`] each) before returning; a missing acknowledge or a
//! bus error is `Transport`. Using a PWM timer or I2C bus before opening it
//! is `Busy`.
//!
//! # Interrupt model
//!
//! Payload code never runs inside a hardware interrupt. The firmware's ISR
//! only timestamps the event and pushes it into a bounded
//! [`events::EventQueue`]; the main loop later calls the payload's `on_irq`
//! export once per event. `timer_start` on a running timer re-arms it: the
//! new period and mode replace the old schedule, the phase restarts, and that
//! timer's pending events are purged. A full queue drops the event and counts it
//! (`irq_overflow`). Periodic timer and UART RX events coalesce while
//! pending, so a fast source cannot flood the queue.
//!
//! DMA transfers use firmware-owned buffers: `spi_transfer` and `uart_write`
//! copy from guest memory and return immediately; completion arrives as a
//! [`events::EventKind::DmaDone`] event, after which `dma_read` copies SPI
//! receive data back. Guest memory itself is never a DMA target, because
//! the interpreter may move it on `memory.grow`.
//!
//! # Exports
//!
//! | name         | signature                   | required | called                     |
//! |--------------|-----------------------------|----------|----------------------------|
//! | `memory`     | memory                      | yes      | —                          |
//! | `init`       | `() -> ()`                  | no       | once after instantiation   |
//! | `tick`       | `(now_ms: i32) -> ()`       | no       | every `set_tick` period    |
//! | `mw_alloc`   | `(len: i32) -> i32`         | if subscribing | before each delivery |
//! | `on_message` | `(slot, ptr, len) -> ()`    | if subscribing | per received sample  |
//! | `on_irq`     | `(kind, source, value, time_us) -> ()` | if using interrupts or DMA | per queued event |
//!
//! Every call into the payload is metered with [`FUEL_PER_CALL`] units of
//! fuel (the runtime's default; firmware may grant a different budget per
//! call); running out traps the payload instead of hanging the device.
//! Payloads may not publish or subscribe on `sys/...` keys.
//!
//! # Memory
//!
//! The exported `memory` may use standard 64 KiB pages or 1-byte pages
//! (the WebAssembly custom-page-sizes proposal; the Rust SDK links with
//! `--page-size=1`). The firmware caps linear memory in **bytes**, so a
//! payload with 1-byte pages can run on a device that cannot spare a whole
//! 64 KiB page. Initial memory above the cap fails to load; `memory.grow`
//! past it returns `-1`. Pointers and lengths in the imports are byte
//! offsets either way.

pub mod events;
pub mod periph;
pub mod route;

/// ABI revision. Each revision is a superset of the previous one, so a
/// payload built for revision `n` loads on firmware implementing `>= n`:
/// the runtime checks each import by name and signature against
/// [`IMPORTS`], and an older payload simply imports a subset. Revision 2
/// added interrupts, timers, SPI and UART DMA; revision 3 added PWM, ADC
/// and I2C; revision 4 added DMA routes.
pub const ABI_VERSION: u16 = 4;

/// Module name for every host import.
pub const IMPORT_MODULE: &str = "mw";

/// Maximum concurrent subscriptions per payload.
pub const MAX_SUBSCRIPTIONS: usize = 8;

/// Largest message delivered to `on_message` or accepted by `publish`.
pub const MAX_MESSAGE: usize = 512;

/// Largest `log` line, in bytes; longer lines are truncated.
pub const MAX_LOG: usize = 256;

/// Default fuel granted to each call into the payload (`init`, `tick`,
/// `mw_alloc`, `on_message`, `on_irq`), roughly one unit per executed
/// instruction.
pub const FUEL_PER_CALL: u64 = 1_000_000;

/// Shortest accepted `set_tick` period.
pub const MIN_TICK_MS: i32 = 10;

/// A host function the payload may import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Import {
    /// Function name within [`IMPORT_MODULE`].
    pub name: &'static str,
    /// Number of `i32` parameters.
    pub params: usize,
    /// Whether it returns an `i32`.
    pub returns: bool,
}

/// Every host import.
pub const IMPORTS: &[Import] = &[
    Import {
        name: "log",
        params: 2,
        returns: false,
    },
    Import {
        name: "publish",
        params: 4,
        returns: true,
    },
    Import {
        name: "subscribe",
        params: 2,
        returns: true,
    },
    Import {
        name: "now_ms",
        params: 0,
        returns: true,
    },
    Import {
        name: "set_tick",
        params: 1,
        returns: true,
    },
    Import {
        name: "gpio_output",
        params: 1,
        returns: true,
    },
    Import {
        name: "gpio_input",
        params: 2,
        returns: true,
    },
    Import {
        name: "gpio_write",
        params: 2,
        returns: true,
    },
    Import {
        name: "gpio_read",
        params: 1,
        returns: true,
    },
    // ABI 2
    Import {
        name: "now_us",
        params: 0,
        returns: true,
    },
    Import {
        name: "irq_overflow",
        params: 0,
        returns: true,
    },
    Import {
        name: "gpio_irq",
        params: 2,
        returns: true,
    },
    Import {
        name: "timer_start",
        params: 3,
        returns: true,
    },
    Import {
        name: "timer_stop",
        params: 1,
        returns: true,
    },
    Import {
        name: "spi_open",
        params: 7,
        returns: true,
    },
    Import {
        name: "spi_transfer",
        params: 4,
        returns: true,
    },
    Import {
        name: "dma_read",
        params: 3,
        returns: true,
    },
    Import {
        name: "uart_open",
        params: 4,
        returns: true,
    },
    Import {
        name: "uart_write",
        params: 3,
        returns: true,
    },
    Import {
        name: "uart_read",
        params: 3,
        returns: true,
    },
    // ABI 3
    Import {
        name: "pwm_open",
        params: 2,
        returns: true,
    },
    Import {
        name: "pwm_set",
        params: 3,
        returns: true,
    },
    Import {
        name: "pwm_stop",
        params: 1,
        returns: true,
    },
    Import {
        name: "adc_read",
        params: 1,
        returns: true,
    },
    Import {
        name: "i2c_open",
        params: 2,
        returns: true,
    },
    Import {
        name: "i2c_transfer",
        params: 6,
        returns: true,
    },
    // ABI 4
    Import {
        name: "route_source",
        params: 5,
        returns: true,
    },
    Import {
        name: "route_sink",
        params: 5,
        returns: true,
    },
    Import {
        name: "route_stop",
        params: 1,
        returns: true,
    },
    Import {
        name: "route_stat",
        params: 2,
        returns: true,
    },
];

/// Number of imports in ABI 1 (the prefix of [`IMPORTS`] up to `gpio_read`).
pub const ABI1_IMPORTS: usize = 9;

/// Number of imports in ABI 2 (the prefix of [`IMPORTS`] up to `uart_read`).
pub const ABI2_IMPORTS: usize = 20;

/// Number of imports in ABI 3 (the prefix of [`IMPORTS`] up to
/// `i2c_transfer`).
pub const ABI3_IMPORTS: usize = 26;

/// Looks up an import by name.
pub fn import(name: &str) -> Option<&'static Import> {
    IMPORTS.iter().find(|i| i.name == name)
}

/// Export names.
pub mod exports {
    /// Linear memory, with 64 KiB or 1-byte pages.
    pub const MEMORY: &str = "memory";
    /// Optional initialiser.
    pub const INIT: &str = "init";
    /// Optional periodic callback.
    pub const TICK: &str = "tick";
    /// Guest allocator used for message delivery.
    pub const ALLOC: &str = "mw_alloc";
    /// Message callback.
    pub const ON_MESSAGE: &str = "on_message";
    /// Interrupt / DMA event callback (ABI 2).
    pub const ON_IRQ: &str = "on_irq";
}

/// Error codes returned (negated) by host imports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum AbiError {
    /// An argument was out of range (bad pin, bad level, bad period).
    InvalidArgument = -1,
    /// A pointer/length pair fell outside linear memory.
    OutOfBounds = -2,
    /// A fixed table (subscriptions) is full.
    NoResources = -3,
    /// The device does not provide this feature (e.g. the pin is reserved).
    Unsupported = -4,
    /// The message could not be sent; for `i2c_transfer` (ABI 3), the
    /// target did not acknowledge or the bus failed.
    Transport = -5,
    /// The key is malformed or reserved (`sys/...`).
    InvalidKey = -6,
    /// The peripheral or DMA channel already has a transfer in flight, or
    /// was not opened (ABI 2).
    Busy = -7,
}

impl AbiError {
    /// The `i32` the import returns.
    pub const fn code(self) -> i32 {
        self as i32
    }

    /// Parses a negative result.
    pub const fn from_code(code: i32) -> Option<Self> {
        match code {
            -1 => Some(AbiError::InvalidArgument),
            -2 => Some(AbiError::OutOfBounds),
            -3 => Some(AbiError::NoResources),
            -4 => Some(AbiError::Unsupported),
            -5 => Some(AbiError::Transport),
            -6 => Some(AbiError::InvalidKey),
            -7 => Some(AbiError::Busy),
            _ => None,
        }
    }
}

/// Input pull configuration for `gpio_input`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pull {
    /// Floating.
    None = 0,
    /// Pull-up.
    Up = 1,
    /// Pull-down.
    Down = 2,
}

impl Pull {
    /// Parses the `pull` argument.
    pub const fn from_i32(v: i32) -> Option<Self> {
        match v {
            0 => Some(Pull::None),
            1 => Some(Pull::Up),
            2 => Some(Pull::Down),
            _ => None,
        }
    }
}

/// Validates a guest `(ptr, len)` pair against a memory of `mem_len` bytes
/// and returns the byte range.
pub fn guest_range(
    ptr: i32,
    len: i32,
    mem_len: usize,
) -> Result<core::ops::Range<usize>, AbiError> {
    let start = usize::try_from(ptr).map_err(|_| AbiError::OutOfBounds)?;
    let len = usize::try_from(len).map_err(|_| AbiError::OutOfBounds)?;
    let end = start.checked_add(len).ok_or(AbiError::OutOfBounds)?;
    if end > mem_len {
        return Err(AbiError::OutOfBounds);
    }
    Ok(start..end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_table_is_unique_and_searchable() {
        for (i, a) in IMPORTS.iter().enumerate() {
            assert!(IMPORTS[i + 1..].iter().all(|b| b.name != a.name));
            assert_eq!(import(a.name), Some(a));
        }
        assert_eq!(import("nope"), None);
    }

    #[test]
    fn revisions_are_prefixes_of_the_import_table() {
        assert_eq!(IMPORTS[ABI1_IMPORTS - 1].name, "gpio_read");
        assert_eq!(IMPORTS[ABI2_IMPORTS - 1].name, "uart_read");
        let abi3 = [
            ("pwm_open", 2),
            ("pwm_set", 3),
            ("pwm_stop", 1),
            ("adc_read", 1),
            ("i2c_open", 2),
            ("i2c_transfer", 6),
        ];
        assert!(
            IMPORTS[ABI2_IMPORTS..ABI3_IMPORTS]
                .iter()
                .map(|i| (i.name, i.params))
                .eq(abi3)
        );
        let abi4 = [
            ("route_source", 5),
            ("route_sink", 5),
            ("route_stop", 1),
            ("route_stat", 2),
        ];
        assert!(
            IMPORTS[ABI3_IMPORTS..]
                .iter()
                .map(|i| (i.name, i.params))
                .eq(abi4)
        );
        assert!(IMPORTS[ABI2_IMPORTS..].iter().all(|i| i.returns));
        assert_eq!(ABI_VERSION, 4);
    }

    #[test]
    fn error_codes_round_trip() {
        for e in [
            AbiError::InvalidArgument,
            AbiError::OutOfBounds,
            AbiError::NoResources,
            AbiError::Unsupported,
            AbiError::Transport,
            AbiError::InvalidKey,
            AbiError::Busy,
        ] {
            assert!(e.code() < 0);
            assert_eq!(AbiError::from_code(e.code()), Some(e));
        }
        assert_eq!(AbiError::from_code(0), None);
    }

    #[test]
    fn guest_range_checks_bounds() {
        assert_eq!(guest_range(4, 4, 8), Ok(4..8));
        assert_eq!(guest_range(5, 4, 8), Err(AbiError::OutOfBounds));
        assert_eq!(guest_range(-1, 1, 8), Err(AbiError::OutOfBounds));
        assert_eq!(guest_range(1, -1, 8), Err(AbiError::OutOfBounds));
        assert_eq!(
            guest_range(i32::MAX, i32::MAX, usize::MAX),
            Ok(i32::MAX as usize..2 * i32::MAX as usize)
        );
    }
}
