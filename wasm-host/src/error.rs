//! Errors reported by the runtime: [`LoadError`] when a module is rejected
//! or its `init` fails, [`Trap`] when a call into a loaded payload fails.
//!
//! Host imports never produce these; they return
//! [`AbiError`](middleware::wasm_abi::AbiError) codes to the payload instead.

use alloc::string::String;
use core::fmt;

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
    /// The exported memory starts larger than the page cap.
    MemoryTooLarge {
        /// Initial size declared by the module, in pages.
        pages: u64,
        /// [`Limits::max_memory_pages`](crate::Limits::max_memory_pages).
        max: u32,
    },
    /// Instantiation failed (data segment out of range, `start` trapped, ...).
    Instantiate(String),
    /// The `init` export trapped.
    Init(Trap),
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
            LoadError::MemoryTooLarge { pages, max } => {
                write!(f, "memory needs {pages} pages, limit is {max}")
            }
            LoadError::Instantiate(m) => write!(f, "instantiation failed: {m}"),
            LoadError::Init(t) => write!(f, "init failed: {t}"),
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
