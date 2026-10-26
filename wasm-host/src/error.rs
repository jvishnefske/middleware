//! Errors reported by the runtime: [`LoadError`] when a module is rejected
//! or its `init` fails, [`Trap`] when a call into a loaded payload fails.
//!
//! A failed `init` is reported as [`LoadError::Init`] together with an
//! [`InitLog`]: the last [`INIT_LOG_LINES`] log lines `init` emitted before
//! it trapped (requirement W8). The device is dropped with the failed
//! payload, so without this copy the lines explaining the failure would be
//! lost.
//!
//! Host imports never produce these; they return
//! [`AbiError`](middleware::wasm_abi::AbiError) codes to the payload instead.

use alloc::collections::VecDeque;
use alloc::string::String;
use core::fmt;

/// Number of log lines an [`InitLog`] keeps (the most recent ones).
///
/// Each line is at most [`wasm_abi::MAX_LOG`](middleware::wasm_abi::MAX_LOG)
/// bytes, so a log holds at most `INIT_LOG_LINES * MAX_LOG` bytes of text.
pub const INIT_LOG_LINES: usize = 8;

/// The log lines a payload's `init` emitted, bounded to the last
/// [`INIT_LOG_LINES`].
///
/// [`Payload::load`](crate::Payload::load) records every `log` call made
/// during `init` here (the lines still reach
/// [`Device::log`](crate::Device::log) as usual). When `init` traps, the
/// log is returned in [`LoadError::Init`]; when it succeeds, the log is
/// discarded. Older lines beyond the capacity are counted in
/// [`InitLog::dropped`] instead of being kept.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InitLog {
    lines: VecDeque<String>,
    dropped: u32,
}

impl InitLog {
    /// An empty log.
    pub const fn new() -> Self {
        InitLog {
            lines: VecDeque::new(),
            dropped: 0,
        }
    }

    /// Appends `line`, evicting the oldest line once [`INIT_LOG_LINES`] are
    /// held. The caller passes lines already truncated to `MAX_LOG`.
    pub(crate) fn push(&mut self, line: &str) {
        if self.lines.len() >= INIT_LOG_LINES {
            self.lines.pop_front();
            self.dropped = self.dropped.saturating_add(1);
        }
        self.lines.push_back(String::from(line));
    }

    /// The kept lines, oldest first.
    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.lines.iter().map(String::as_str)
    }

    /// Number of kept lines (at most [`INIT_LOG_LINES`]).
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// Whether `init` logged nothing.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty() && self.dropped == 0
    }

    /// Lines evicted because more than [`INIT_LOG_LINES`] were logged
    /// (saturating).
    pub fn dropped(&self) -> u32 {
        self.dropped
    }
}

impl fmt::Display for InitLog {
    /// `"<n> earlier lines dropped" | "line" | "line"`, one line of text.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut sep = "";
        if self.dropped > 0 {
            write!(f, "{} earlier lines dropped", self.dropped)?;
            sep = " | ";
        }
        for line in self.lines() {
            write!(f, "{sep}{line:?}")?;
            sep = " | ";
        }
        Ok(())
    }
}

/// Why [`Payload::load`](crate::Payload::load) rejected a module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    /// The bytes are not a valid WebAssembly module (wasmi's message).
    Invalid(String),
    /// An import outside `mw`, not in `wasm_abi::IMPORTS`, or not a function.
    UnknownImport {
        /// Import module name.
        module: String,
        /// Import field name.
        name: String,
    },
    /// A known `mw` import declared with the wrong signature.
    BadSignature {
        /// Import name.
        name: String,
    },
    /// An ABI export (`init`, `tick`, `mw_alloc`, `on_message`, `on_irq`) with the
    /// wrong signature.
    BadExport {
        /// Export name.
        name: String,
    },
    /// The module does not export a memory named `memory`.
    MissingMemory,
    /// The module's initial linear memory is larger than the byte cap, or
    /// its memory section could not be read.
    MemoryTooLarge {
        /// Initial size declared by the module, in bytes (`u64::MAX` when
        /// unreadable or overflowing).
        bytes: u64,
        /// [`Limits::max_memory_bytes`](crate::Limits::max_memory_bytes).
        max: u32,
    },
    /// Instantiation failed (data segment out of range, `start` trapped, ...).
    Instantiate(String),
    /// The `init` export trapped.
    Init {
        /// Why `init` failed.
        trap: Trap,
        /// The log lines `init` emitted before the trap.
        log: InitLog,
    },
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::Invalid(m) => write!(f, "invalid module: {m}"),
            LoadError::UnknownImport { module, name } => {
                write!(f, "unknown import {module}.{name}")
            }
            LoadError::BadSignature { name } => {
                write!(f, "import mw.{name} has the wrong signature")
            }
            LoadError::BadExport { name } => write!(f, "export {name} has the wrong signature"),
            LoadError::MissingMemory => f.write_str("module does not export `memory`"),
            LoadError::MemoryTooLarge { bytes, max } => {
                write!(f, "memory needs {bytes} bytes, limit is {max}")
            }
            LoadError::Instantiate(m) => write!(f, "instantiation failed: {m}"),
            LoadError::Init { trap, log } if log.is_empty() => {
                write!(f, "init failed: {trap}")
            }
            LoadError::Init { trap, log } => write!(f, "init failed: {trap}; init log: {log}"),
        }
    }
}

impl core::error::Error for LoadError {}

/// Why a call into the payload failed.
///
/// After any trap the payload is *poisoned*: its memory may be half updated,
/// so the runtime refuses further calls with [`Trap::Poisoned`] and
/// [`Payload::next_tick`](crate::Payload::next_tick) returns `None`. The
/// firmware reports the trap and reloads the payload to recover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trap {
    /// The call used up its fuel (e.g. an infinite loop).
    OutOfFuel,
    /// Any other WebAssembly trap or runtime error (wasmi's message).
    Wasm(String),
    /// An earlier call trapped; the payload must be reloaded.
    Poisoned,
}

impl fmt::Display for Trap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Trap::OutOfFuel => f.write_str("out of fuel"),
            Trap::Wasm(m) => write!(f, "trap: {m}"),
            Trap::Poisoned => f.write_str("payload trapped earlier and must be reloaded"),
        }
    }
}

impl core::error::Error for Trap {}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec::Vec;

    #[test]
    fn init_log_keeps_the_most_recent_lines() {
        let mut log = InitLog::new();
        assert!(log.is_empty());
        for i in 0..INIT_LOG_LINES + 3 {
            log.push(&i.to_string());
        }
        assert_eq!(log.len(), INIT_LOG_LINES);
        assert_eq!(log.dropped(), 3);
        let kept: Vec<&str> = log.lines().collect();
        assert_eq!(kept.first(), Some(&"3"));
        assert_eq!(kept.last(), Some(&"10"));
    }

    #[test]
    fn init_error_display_includes_the_log() {
        let mut log = InitLog::new();
        log.push("opening sensor");
        log.push("sensor missing");
        let e = LoadError::Init {
            trap: Trap::OutOfFuel,
            log,
        };
        assert_eq!(
            e.to_string(),
            r#"init failed: out of fuel; init log: "opening sensor" | "sensor missing""#
        );
        let quiet = LoadError::Init {
            trap: Trap::OutOfFuel,
            log: InitLog::new(),
        };
        assert_eq!(quiet.to_string(), "init failed: out of fuel");
    }
}
