//! A hardware-shaped reference model of the Colorlight 5A-75B gateware's
//! WebAssembly core (requirement CL3).
//!
//! The gateware has no CPU: a loader FSM compiles the payload from SPI
//! flash into block RAM, a stack-machine interpreter runs it, and an import
//! unit implements the payload ABI. This crate is that design written as
//! plain Rust, with the same fixed-size tables and the same algorithms, so
//! it is the specification the gateware is tested against (golden vectors,
//! `tests/vectors.rs`) and is itself tested against wasmi through
//! `wasm-host` (`tests/diff.rs`): same import calls, same final memory and
//! globals.
//!
//! * [`isa`] — the 32-bit internal code in code RAM.
//! * `load` — [`Core::load`]: one-pass parse and compile ([`LoadError`]).
//! * `exec` — the interpreter: value stack, call frames, fuel, traps
//!   ([`Trap`]).
//! * `abi` — the import unit and the event entry points
//!   ([`Core::init`], [`Core::tick`], [`Core::deliver`],
//!   [`Core::request`]) over a [`Host`].
//!
//! # Subset
//!
//! i32 only, plus `i64.const` feeding `i64.store`; control flow
//! (`block`/`loop`/`if` with at most one i32 result, `br`, `br_if`,
//! `br_table`, `return`, `call`, `call_indirect`), locals, i32 globals,
//! loads and stores of 8/16/32 bits, `memory.size`/`grow`/`copy`/`fill`,
//! `select`, every i32 arithmetic, comparison and bit op, `extend8_s` and
//! `extend16_s`. One funcref table with active element segments; one
//! memory with 64 KiB or 1-byte pages, at most `MEM` bytes.
//!
//! # Imports
//!
//! `log`, `publish`, `subscribe`, `now_ms`, `set_tick`, the four GPIO
//! calls, `serve`, `node_id`, `route_bridge`, `route_stop` and
//! `route_stat` behave exactly as in `wasm-host`. Every other ABI import
//! loads (with its signature checked) and returns `Unsupported` whatever
//! its arguments: the board has no such peripheral.
//!
//! # Differences from wasmi
//!
//! Fuel is one unit per executed internal instruction (wasmi charges by
//! its own instruction set), so the point where a long call runs out
//! differs; both trap. The value stack is [`STACK_SLOTS`] 32-bit slots and
//! calls nest [`CALL_DEPTH`] deep.
#![no_std]
#![forbid(unsafe_code)]

mod abi;
mod exec;
pub mod isa;
mod load;

pub use abi::{Host, Schedule, Ticked};

use middleware::key::{KeyBuf, MAX_KEY_LEN};
use middleware::wasm_abi::{FUEL_PER_CALL, MAX_SUBSCRIPTIONS, serve::MAX_SERVES};

/// Words of code RAM (16 KiB: eight ECP5 block RAMs).
pub const CODE_WORDS: usize = 4096;
/// Functions, imports included.
pub const MAX_FUNCS: usize = 128;
/// Function types.
pub const MAX_TYPES: usize = 32;
/// Parameters of one function type.
pub const MAX_PARAMS: usize = 8;
/// Globals.
pub const MAX_GLOBALS: usize = 16;
/// Entries of table 0.
pub const MAX_TABLE: usize = 64;
/// Parameters plus locals of one function.
pub const MAX_LOCALS: usize = 256;
/// Nesting depth of blocks while compiling.
pub const MAX_CTRL: usize = 64;
/// Operand-stack depth while compiling one function.
pub const MAX_OPERANDS: usize = 256;
/// Value-stack slots (4 KiB: two block RAMs).
pub const STACK_SLOTS: usize = 1024;
/// Nested calls, the export call included (`wasm-host`'s default).
pub const CALL_DEPTH: usize = 32;

/// The 5A-75B instance: 16 KiB of linear memory.
pub const COLORLIGHT_MEMORY: usize = 16 * 1024;

/// The core as the 5A-75B gateware builds it.
pub type Colorlight = Core<COLORLIGHT_MEMORY>;

/// Why a module was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadError {
    /// Not a well-formed module (bad header, truncated, inconsistent).
    Malformed,
    /// Outside the subset the core implements.
    Unsupported(&'static str),
    /// A table of the core is too small for the module.
    TooLarge(&'static str),
    /// An import that is not in the ABI.
    UnknownImport,
    /// An ABI import with the wrong signature.
    BadSignature,
    /// An export the runtime calls has the wrong signature.
    BadExport,
    /// No exported `memory`.
    MissingMemory,
    /// The initial memory exceeds the core's.
    MemoryTooLarge {
        /// Bytes the module asks for.
        bytes: u64,
        /// Bytes the core has.
        max: u32,
    },
}

/// Why a call into the payload stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trap {
    /// `unreachable`.
    Unreachable,
    /// The call's fuel ran out.
    OutOfFuel,
    /// The value stack or the call depth overflowed.
    StackOverflow,
    /// A load, store, copy or fill outside linear memory.
    MemoryOutOfBounds,
    /// Integer division or remainder by zero.
    DivideByZero,
    /// `i32.div_s` of `i32::MIN` by -1.
    IntegerOverflow,
    /// `call_indirect` past the table's end.
    TableOutOfBounds,
    /// `call_indirect` to an empty entry.
    IndirectNull,
    /// `call_indirect` to a function of another type.
    IndirectType,
    /// The payload trapped earlier and must be reloaded.
    Poisoned,
    /// No payload is loaded.
    NotLoaded,
    /// A code word the loader never writes (a gateware fault).
    BadCode,
}

/// One entry of the function table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Func {
    /// The ABI import index, or [`Func::LOCAL`].
    pub import: u8,
    /// Entry word address (local functions).
    pub entry: u16,
    /// Parameter slots.
    pub params: u8,
    /// Result slots (0 or 1).
    pub results: u8,
    /// Declared locals (zeroed on entry).
    pub locals: u16,
}

impl Func {
    /// The `import` value of a function defined by the module.
    pub const LOCAL: u8 = 0xFF;
    const EMPTY: Func = Func {
        import: Func::LOCAL,
        entry: isa::NONE,
        params: 0,
        results: 0,
        locals: 0,
    };
}

/// The exports the runtime calls, as function indices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Exports {
    /// `init`.
    pub init: Option<u16>,
    /// `tick`.
    pub tick: Option<u16>,
    /// `mw_alloc`.
    pub alloc: Option<u16>,
    /// `on_message`.
    pub on_message: Option<u16>,
    /// `on_irq` (accepted; the board raises no events).
    pub on_irq: Option<u16>,
    /// `on_request`.
    pub on_request: Option<u16>,
}

#[derive(Debug, Clone, Copy)]
struct Frame {
    ret: u16,
    fp: u16,
}

/// The core: every RAM of the gateware design. `MEM` is the linear memory
/// size in bytes.
pub struct Core<const MEM: usize> {
    code: [u32; CODE_WORDS],
    code_len: usize,
    funcs: [Func; MAX_FUNCS],
    nfuncs: usize,
    nimports: usize,
    table: [u16; MAX_TABLE],
    table_len: usize,
    globals: [i32; MAX_GLOBALS],
    nglobals: usize,
    mem: [u8; MEM],
    mem_len: usize,
    mem_max: usize,
    page_log2: u32,
    exports: Exports,
    loaded: bool,
    stack: [i32; STACK_SLOTS],
    frames: [Frame; CALL_DEPTH],
    /// Fuel granted to each call into the payload.
    pub fuel_per_call: u64,
    subs: [Option<KeyBuf<MAX_KEY_LEN>>; MAX_SUBSCRIPTIONS],
    serves: [Option<KeyBuf<MAX_KEY_LEN>>; MAX_SERVES],
    schedule: Option<Schedule>,
    poisoned: bool,
    executed: u64,
}

impl<const MEM: usize> Default for Core<MEM> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const MEM: usize> Core<MEM> {
    /// An empty core.
    pub const fn new() -> Self {
        Core {
            code: [0; CODE_WORDS],
            code_len: 0,
            funcs: [Func::EMPTY; MAX_FUNCS],
            nfuncs: 0,
            nimports: 0,
            table: [isa::NONE; MAX_TABLE],
            table_len: 0,
            globals: [0; MAX_GLOBALS],
            nglobals: 0,
            mem: [0; MEM],
            mem_len: 0,
            mem_max: 0,
            page_log2: 16,
            exports: Exports {
                init: None,
                tick: None,
                alloc: None,
                on_message: None,
                on_irq: None,
                on_request: None,
            },
            loaded: false,
            stack: [0; STACK_SLOTS],
            frames: [Frame { ret: 0, fp: 0 }; CALL_DEPTH],
            fuel_per_call: FUEL_PER_CALL,
            subs: [None; MAX_SUBSCRIPTIONS],
            serves: [None; MAX_SERVES],
            schedule: None,
            poisoned: false,
            executed: 0,
        }
    }

    fn reset(&mut self) {
        let fuel = self.fuel_per_call;
        self.code = [0; CODE_WORDS];
        self.code_len = 0;
        self.funcs = [Func::EMPTY; MAX_FUNCS];
        self.nfuncs = 0;
        self.nimports = 0;
        self.table = [isa::NONE; MAX_TABLE];
        self.table_len = 0;
        self.globals = [0; MAX_GLOBALS];
        self.nglobals = 0;
        self.mem.fill(0);
        self.mem_len = 0;
        self.mem_max = 0;
        self.page_log2 = 16;
        self.exports = Exports::default();
        self.loaded = false;
        self.stack.fill(0);
        self.subs = [None; MAX_SUBSCRIPTIONS];
        self.serves = [None; MAX_SERVES];
        self.schedule = None;
        self.poisoned = false;
        self.executed = 0;
        self.fuel_per_call = fuel;
    }

    /// The compiled code.
    pub fn code(&self) -> &[u32] {
        self.code.get(..self.code_len).unwrap_or_default()
    }

    /// The function table (imports first).
    pub fn funcs(&self) -> &[Func] {
        self.funcs.get(..self.nfuncs).unwrap_or_default()
    }

    /// Table 0 (function indices, [`isa::NONE`] for empty entries).
    pub fn table(&self) -> &[u16] {
        self.table.get(..self.table_len).unwrap_or_default()
    }

    /// The globals.
    pub fn globals(&self) -> &[i32] {
        self.globals.get(..self.nglobals).unwrap_or_default()
    }

    /// Linear memory (its current length).
    pub fn memory(&self) -> &[u8] {
        self.mem.get(..self.mem_len).unwrap_or_default()
    }

    /// The exports.
    pub fn exports(&self) -> Exports {
        self.exports
    }

    /// The subscribed keys, in slot order.
    pub fn subscriptions(&self) -> impl Iterator<Item = &str> {
        self.subs.iter().flatten().map(|k| k.as_str())
    }

    /// The served keys, in slot order.
    pub fn serves(&self) -> impl Iterator<Item = &str> {
        self.serves.iter().flatten().map(|k| k.as_str())
    }

    /// The `tick` schedule.
    pub fn schedule(&self) -> Option<Schedule> {
        self.schedule
    }

    /// Whether a trap stopped the payload.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Internal instructions executed since load (the fuel spent).
    pub fn executed(&self) -> u64 {
        self.executed
    }
}
