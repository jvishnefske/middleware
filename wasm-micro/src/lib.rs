//! A small WebAssembly interpreter for microcontrollers.
//!
//! * **Safe Rust only** (`#![forbid(unsafe_code)]`), `no_std`, and no
//!   allocator unless you want one: [`Machine`] runs a module in three
//!   slices the caller owns (statics, say). With feature `alloc` (on by
//!   default) [`Module`] and [`Instance`] own the same storage in `Vec`s.
//! * **Runs the image in place.** [`Module`] holds the `.wasm` bytes
//!   (borrowed from flash, say, or owned) and never translates them. RAM
//!   goes to linear
//!   memory, three fixed stacks sized by [`Limits`], and a small index:
//!   four bytes per type, eight per function, eight per `block`, `if` and
//!   `else`.
//! * **No validator.** Type-checking a module is what makes interpreters
//!   that then run it unchecked large. Here every stack, local, global,
//!   table and memory access is bounds-checked as it happens, so a module
//!   that would fail validation cannot corrupt the host: it traps
//!   ([`Error::Invalid`] and friends) or computes garbage inside its own
//!   sandbox. Values are untyped 64-bit slots, so a module that confuses an
//!   `i32` with an `f64` only confuses itself. A host that wants ill-typed
//!   modules *rejected* must validate them before they reach the device.
//! * **Fixed storage.** A [`Machine`] never allocates; its sizes come from
//!   [`footprint`] and [`Limits`], and `memory.grow` succeeds while the
//!   memory buffer has room. An [`Instance`] allocates in
//!   [`Instance::new`] and afterwards only for `memory.grow`.
//! * **Bounded:** fuel (one unit per instruction), a value stack, a call
//!   depth and a block depth, all from [`Limits`]. Calls do not recurse on
//!   the native stack.
//!
//! # What it runs
//!
//! WebAssembly 1.0 plus the extensions `rustc` emits for
//! `wasm32-unknown-unknown` by default: mutable globals, sign extension,
//! saturating float-to-int, multi-value, `memory.copy` and `memory.fill`,
//! and memories with one-byte pages (the custom-page-sizes proposal).
//! Floating-point arithmetic needs the `float` feature.
//!
//! Refused at load ([`Error::Unsupported`]): imports other than functions,
//! more than one memory or table, passive data and table instructions
//! (`memory.init`, `table.*`, `ref.*`), SIMD, threads, tail calls,
//! exceptions, GC and 64-bit memories.
//!
//! # Less than a page of memory
//!
//! A module's memory is declared in 64 KiB pages, more than many small
//! parts can spare. [`Limits::memory_bytes`] caps the bytes actually
//! backing it: the module still sees its declared size, and an access
//! above the cap traps like any other out-of-bounds access. A payload
//! linked with a small stack below its data never goes there.
//!
//! # Example
//!
//! ```
//! use wasm_micro::{Error, Host, Instance, Limits, Module};
//!
//! struct Console;
//! impl Host for Console {
//!     fn call(&mut self, _import: u32, _mem: &mut [u8], vals: &mut [u64]) -> Result<(), Error> {
//!         vals[0] += 1; // one i32 in, one i32 out
//!         Ok(())
//!     }
//! }
//!
//! let wasm = wat::parse_str(r#"(module
//!     (import "env" "inc" (func $inc (param i32) (result i32)))
//!     (func (export "f") (param i32) (result i32) (call $inc (local.get 0))))"#).unwrap();
//! let module = Module::new(&wasm[..])?;
//! let f = module.export_func("f").unwrap();
//! let mut inst = Instance::new(module, &Limits::default(), &mut Console)?;
//! let mut out = [0];
//! inst.call(&mut Console, f, &[41], &mut out)?;
//! assert_eq!(out[0], 42);
//! # Ok::<(), Error>(())
//! ```
#![no_std]
#![forbid(unsafe_code)]

#[cfg(feature = "alloc")]
extern crate alloc;

mod exec;
#[cfg(feature = "float")]
mod float;
mod module;
mod reader;

#[cfg(feature = "alloc")]
pub use exec::Instance;
pub use exec::{Buffers, Host, Limits, Machine};
#[cfg(feature = "alloc")]
pub use module::Module;
pub use module::{Export, ExportKind, Footprint, Import, footprint, index_bound};

/// Why a module did not load or a call did not return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The image is not a well-formed module.
    Malformed,
    /// The module uses something this interpreter lacks (see the crate
    /// docs), or a float instruction ran without the `float` feature.
    Unsupported,
    /// The host's allocator refused.
    OutOfMemory,
    /// The module needs more memory or table than [`Limits`] allows.
    Limit,
    /// A data or element segment does not fit its memory or table.
    SegmentOutOfBounds,
    /// The module did something validation would have refused: popped an
    /// empty stack, named a missing local, called with the wrong arity.
    Invalid,
    Unreachable,
    MemoryOutOfBounds,
    DivideByZero,
    IntegerOverflow,
    /// A float with no integer value (NaN) was truncated.
    InvalidConversion,
    /// `call_indirect` past the end of the table.
    UndefinedElement,
    /// `call_indirect` through an empty table slot.
    UninitializedElement,
    /// `call_indirect` to a function of another type.
    SignatureMismatch,
    /// The value stack, call depth or block depth of [`Limits`] ran out.
    StackOverflow,
    OutOfFuel,
    /// Returned by the host from an import.
    Host(i32),
}

impl Error {
    /// A short description, in the specification's words where it has some.
    pub const fn as_str(self) -> &'static str {
        match self {
            Error::Malformed => "malformed module",
            Error::Unsupported => "unsupported feature",
            Error::OutOfMemory => "out of memory",
            Error::Limit => "module exceeds the limits",
            Error::SegmentOutOfBounds => "segment does not fit",
            Error::Invalid => "invalid module",
            Error::Unreachable => "unreachable",
            Error::MemoryOutOfBounds => "out of bounds memory access",
            Error::DivideByZero => "integer divide by zero",
            Error::IntegerOverflow => "integer overflow",
            Error::InvalidConversion => "invalid conversion to integer",
            Error::UndefinedElement => "undefined element",
            Error::UninitializedElement => "uninitialized element",
            Error::SignatureMismatch => "indirect call type mismatch",
            Error::StackOverflow => "call stack exhausted",
            Error::OutOfFuel => "out of fuel",
            Error::Host(_) => "host error",
        }
    }
}
