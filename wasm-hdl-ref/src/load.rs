//! The loader: parses a module in one pass, fills the tables and compiles
//! every function body into [`isa`] words.
//!
//! Sections are read in their binary order (type, import, function, table,
//! memory, global, export, element, data count, code, data; custom
//! sections are skipped). What it builds, all fixed-size:
//!
//! * the **function table**: per function its entry word address, its
//!   parameter, result and local counts, or the ABI import index for an
//!   imported function;
//! * the **globals** (i32 only, constant initialisers);
//! * **table 0** for `call_indirect`: function indices from active element
//!   segments;
//! * the **exports** the runtime calls, with their signatures checked as
//!   `wasm-host` checks them;
//! * **linear memory** with every active data segment copied in;
//! * **code**: each body compiled with resolved branch targets. A static
//!   operand-type stack gives every branch its `keep`/`drop`; forward
//!   branches are chained through their own target fields and patched at
//!   the block's `end` (no fixup list in RAM).
//!
//! Anything outside the subset is refused with [`LoadError::Unsupported`]:
//! floats, `i64` beyond `i64.const` feeding `i64.store` (what the Rust SDK
//! emits to zero 8 bytes), multi-value blocks, imported or exported
//! tables, memories and globals, a start function, passive segments, bulk
//! memory other than `memory.copy` and `memory.fill`.

use middleware::wasm_abi::{self, IMPORT_MODULE, exports};

use crate::isa::{self, NONE, word};
use crate::{
    CALL_DEPTH, CODE_WORDS, Core, Func, LoadError, MAX_CTRL, MAX_FUNCS, MAX_GLOBALS, MAX_LOCALS,
    MAX_OPERANDS, MAX_PARAMS, MAX_TABLE, MAX_TYPES,
};

/// A byte reader over the module.
struct Rd<'a> {
    b: &'a [u8],
    at: usize,
}

type R<T> = Result<T, LoadError>;

impl<'a> Rd<'a> {
    fn u8(&mut self) -> R<u8> {
        let v = *self.b.get(self.at).ok_or(LoadError::Malformed)?;
        self.at += 1;
        Ok(v)
    }

    fn bytes(&mut self, n: usize) -> R<&'a [u8]> {
        let end = self.at.checked_add(n).ok_or(LoadError::Malformed)?;
        let s = self.b.get(self.at..end).ok_or(LoadError::Malformed)?;
        self.at = end;
        Ok(s)
    }

    fn u32(&mut self) -> R<u32> {
        let mut v: u64 = 0;
        for i in 0..5 {
            let b = self.u8()?;
            v |= u64::from(b & 0x7F) << (7 * i);
            if b & 0x80 == 0 {
                return u32::try_from(v).map_err(|_| LoadError::Malformed);
            }
        }
        Err(LoadError::Malformed)
    }

    fn sleb(&mut self, bits: u32) -> R<i64> {
        let mut v: i64 = 0;
        let mut shift = 0u32;
        let max = bits.div_ceil(7);
        for _ in 0..max {
            let b = self.u8()?;
            v |= i64::from(b & 0x7F) << shift;
            shift += 7;
            if b & 0x80 == 0 {
                if shift < 64 && b & 0x40 != 0 {
                    v |= -1i64 << shift;
                }
                return Ok(v);
            }
        }
        Err(LoadError::Malformed)
    }

    fn i32(&mut self) -> R<i32> {
        i32::try_from(self.sleb(32)?).map_err(|_| LoadError::Malformed)
    }

    fn name(&mut self) -> R<&'a str> {
        let n = self.u32()? as usize;
        core::str::from_utf8(self.bytes(n)?).map_err(|_| LoadError::Malformed)
    }

    fn done(&self) -> bool {
        self.at >= self.b.len()
    }

    /// A constant expression yielding an i32: `i32.const v end`.
    fn const_i32(&mut self) -> R<i32> {
        match self.u8()? {
            0x41 => {
                let v = self.i32()?;
                (self.u8()? == 0x0B)
                    .then_some(v)
                    .ok_or(LoadError::Unsupported("constant expression"))
            }
            _ => Err(LoadError::Unsupported("constant expression")),
        }
    }
}

const I32: u8 = 0x7F;
const I64: u8 = 0x7E;

/// Module-level state while loading.
struct Module {
    /// Function types: `(params, results)`, all i32.
    types: [(u8, u8); MAX_TYPES],
    ntypes: usize,
    /// Type index of each local function, in order.
    has_table: bool,
    has_memory: bool,
    memory_exported: bool,
    /// Bodies seen so far.
    bodies: usize,
}

fn val_slots(t: u8) -> R<u8> {
    match t {
        I32 => Ok(1),
        I64 => Err(LoadError::Unsupported("i64 parameter, result or local")),
        0x7D | 0x7C => Err(LoadError::Unsupported("floats")),
        _ => Err(LoadError::Unsupported("value type")),
    }
}

impl<const MEM: usize> Core<MEM> {
    /// Loads `wasm`: resets every table and memory, then parses and
    /// compiles. On error the core holds no payload.
    pub fn load(&mut self, wasm: &[u8]) -> Result<(), LoadError> {
        self.reset();
        let r = self.load_inner(wasm);
        if r.is_err() {
            self.reset();
        } else {
            self.loaded = true;
        }
        r
    }

    fn load_inner(&mut self, wasm: &[u8]) -> R<()> {
        let mut rd = Rd { b: wasm, at: 0 };
        if rd.bytes(8)? != b"\0asm\x01\0\0\0" {
            return Err(LoadError::Malformed);
        }
        let mut m = Module {
            types: [(0, 0); MAX_TYPES],
            ntypes: 0,
            has_table: false,
            has_memory: false,
            memory_exported: false,
            bodies: 0,
        };
        let mut last = 0u8;
        while !rd.done() {
            let id = rd.u8()?;
            let len = rd.u32()? as usize;
            let body = rd.bytes(len)?;
            if id != 0 {
                // Data count (12) sits between element (9) and code (10).
                let order = |i: u8| if i == 12 { 9 } else { i };
                if order(id) <= order(last) && !(id == 12 && last == 9) {
                    return Err(LoadError::Malformed);
                }
                last = id;
            }
            let mut s = Rd { b: body, at: 0 };
            match id {
                0 | 12 => {}
                1 => self.sec_types(&mut s, &mut m)?,
                2 => self.sec_imports(&mut s, &m)?,
                3 => self.sec_functions(&mut s, &m)?,
                4 => self.sec_table(&mut s, &mut m)?,
                5 => self.sec_memory(&mut s, &mut m)?,
                6 => self.sec_globals(&mut s)?,
                7 => self.sec_exports(&mut s, &mut m)?,
                8 => return Err(LoadError::Unsupported("start function")),
                9 => self.sec_elements(&mut s, &m)?,
                10 => self.sec_code(&mut s, &mut m)?,
                11 => self.sec_data(&mut s)?,
                _ => return Err(LoadError::Malformed),
            }
            if id != 0 && id != 12 && !s.done() {
                return Err(LoadError::Malformed);
            }
        }
        if m.bodies != self.nfuncs - self.nimports {
            return Err(LoadError::Malformed);
        }
        if !m.memory_exported {
            return Err(LoadError::MissingMemory);
        }
        Ok(())
    }

    fn sec_types(&mut self, s: &mut Rd<'_>, m: &mut Module) -> R<()> {
        let n = s.u32()? as usize;
        if n > MAX_TYPES {
            return Err(LoadError::TooLarge("types"));
        }
        for i in 0..n {
            if s.u8()? != 0x60 {
                return Err(LoadError::Malformed);
            }
            let np = s.u32()? as usize;
            if np > MAX_PARAMS {
                return Err(LoadError::TooLarge("parameters"));
            }
            for _ in 0..np {
                val_slots(s.u8()?)?;
            }
            let nr = s.u32()? as usize;
            if nr > 1 {
                return Err(LoadError::Unsupported("multiple results"));
            }
            for _ in 0..nr {
                val_slots(s.u8()?)?;
            }
            *m.types.get_mut(i).ok_or(LoadError::Malformed)? = (np as u8, nr as u8);
        }
        m.ntypes = n;
        Ok(())
    }

    fn ty(m: &Module, idx: u32) -> R<(u8, u8)> {
        let i = idx as usize;
        if i >= m.ntypes {
            return Err(LoadError::Malformed);
        }
        m.types.get(i).copied().ok_or(LoadError::Malformed)
    }

    fn push_func(&mut self, f: Func) -> R<()> {
        let slot = self
            .funcs
            .get_mut(self.nfuncs)
            .ok_or(LoadError::TooLarge("functions"))?;
        *slot = f;
        self.nfuncs += 1;
        Ok(())
    }

    fn sec_imports(&mut self, s: &mut Rd<'_>, m: &Module) -> R<()> {
        let n = s.u32()?;
        for _ in 0..n {
            let module = s.name()?;
            let name = s.name()?;
            if s.u8()? != 0x00 {
                return Err(LoadError::Unsupported("imported table, memory or global"));
            }
            let (params, results) = Self::ty(m, s.u32()?)?;
            if module != IMPORT_MODULE {
                return Err(LoadError::UnknownImport);
            }
            let idx = wasm_abi::IMPORTS
                .iter()
                .position(|i| i.name == name)
                .ok_or(LoadError::UnknownImport)?;
            let imp = wasm_abi::IMPORTS.get(idx).ok_or(LoadError::UnknownImport)?;
            if usize::from(params) != imp.params || (results == 1) != imp.returns {
                return Err(LoadError::BadSignature);
            }
            self.push_func(Func {
                import: idx as u8,
                entry: NONE,
                params,
                results,
                locals: 0,
            })?;
            self.nimports += 1;
        }
        Ok(())
    }

    fn sec_functions(&mut self, s: &mut Rd<'_>, m: &Module) -> R<()> {
        let n = s.u32()?;
        for _ in 0..n {
            let (params, results) = Self::ty(m, s.u32()?)?;
            self.push_func(Func {
                import: Func::LOCAL,
                entry: NONE,
                params,
                results,
                locals: 0,
            })?;
        }
        Ok(())
    }

    fn sec_table(&mut self, s: &mut Rd<'_>, m: &mut Module) -> R<()> {
        let n = s.u32()?;
        if n > 1 {
            return Err(LoadError::Unsupported("more than one table"));
        }
        if n == 1 {
            if s.u8()? != 0x70 {
                return Err(LoadError::Unsupported("table type"));
            }
            let flags = s.u8()?;
            let min = s.u32()? as usize;
            if flags & 1 != 0 {
                s.u32()?;
            }
            if min > MAX_TABLE {
                return Err(LoadError::TooLarge("table"));
            }
            self.table_len = min;
            m.has_table = true;
        }
        Ok(())
    }

    fn sec_memory(&mut self, s: &mut Rd<'_>, m: &mut Module) -> R<()> {
        let n = s.u32()?;
        if n > 1 {
            return Err(LoadError::Unsupported("more than one memory"));
        }
        if n == 1 {
            let flags = s.u32()?;
            if flags & !0b1001 != 0 {
                return Err(LoadError::Unsupported("shared or 64-bit memory"));
            }
            let min = u64::from(s.u32()?);
            let max = if flags & 1 != 0 {
                Some(u64::from(s.u32()?))
            } else {
                None
            };
            let log2 = if flags & 8 != 0 { s.u32()? } else { 16 };
            if log2 != 0 && log2 != 16 {
                return Err(LoadError::Unsupported("page size"));
            }
            let bytes = min << log2;
            if bytes > MEM as u64 {
                return Err(LoadError::MemoryTooLarge {
                    bytes,
                    max: MEM as u32,
                });
            }
            self.page_log2 = log2;
            self.mem_len = bytes as usize;
            self.mem_max = match max {
                Some(p) => (p << log2).min(MEM as u64) as usize,
                None => MEM,
            };
            m.has_memory = true;
        }
        Ok(())
    }

    fn sec_globals(&mut self, s: &mut Rd<'_>) -> R<()> {
        let n = s.u32()? as usize;
        if n > MAX_GLOBALS {
            return Err(LoadError::TooLarge("globals"));
        }
        for i in 0..n {
            if s.u8()? != I32 {
                return Err(LoadError::Unsupported("non-i32 global"));
            }
            let _mutable = s.u8()?;
            *self.globals.get_mut(i).ok_or(LoadError::Malformed)? = s.const_i32()?;
        }
        self.nglobals = n;
        Ok(())
    }

    fn sec_exports(&mut self, s: &mut Rd<'_>, m: &mut Module) -> R<()> {
        let n = s.u32()?;
        for _ in 0..n {
            let name = s.name()?;
            let kind = s.u8()?;
            let idx = s.u32()?;
            match kind {
                0x00 => {
                    let f = self
                        .funcs
                        .get(idx as usize)
                        .filter(|_| (idx as usize) < self.nfuncs);
                    let f = *f.ok_or(LoadError::Malformed)?;
                    let shape = (f.params, f.results);
                    let slot = match name {
                        exports::INIT => Some((&mut self.exports.init, (0, 0))),
                        exports::TICK => Some((&mut self.exports.tick, (1, 0))),
                        exports::ALLOC => Some((&mut self.exports.alloc, (1, 1))),
                        exports::ON_MESSAGE => Some((&mut self.exports.on_message, (3, 0))),
                        exports::ON_IRQ => Some((&mut self.exports.on_irq, (4, 0))),
                        exports::ON_REQUEST => Some((&mut self.exports.on_request, (6, 1))),
                        _ => None,
                    };
                    if let Some((slot, want)) = slot {
                        if shape != want {
                            return Err(LoadError::BadExport);
                        }
                        *slot = Some(idx as u16);
                    }
                }
                0x01 => return Err(LoadError::Unsupported("exported table")),
                0x02 => {
                    if name == exports::MEMORY && m.has_memory {
                        m.memory_exported = true;
                    }
                }
                0x03 => {}
                _ => return Err(LoadError::Malformed),
            }
        }
        Ok(())
    }

    fn sec_elements(&mut self, s: &mut Rd<'_>, m: &Module) -> R<()> {
        let n = s.u32()?;
        for _ in 0..n {
            if s.u32()? != 0 || !m.has_table {
                return Err(LoadError::Unsupported("element segment form"));
            }
            let off = s.const_i32()? as u32 as usize;
            let k = s.u32()? as usize;
            let end = off.checked_add(k).ok_or(LoadError::Malformed)?;
            if end > self.table_len {
                return Err(LoadError::Malformed);
            }
            for i in off..end {
                let f = s.u32()?;
                if f as usize >= self.nfuncs {
                    return Err(LoadError::Malformed);
                }
                *self.table.get_mut(i).ok_or(LoadError::Malformed)? = f as u16;
            }
        }
        Ok(())
    }

    fn sec_data(&mut self, s: &mut Rd<'_>) -> R<()> {
        let n = s.u32()?;
        for _ in 0..n {
            if s.u32()? != 0 {
                return Err(LoadError::Unsupported("passive data segment"));
            }
            let off = s.const_i32()? as u32 as usize;
            let len = s.u32()? as usize;
            let bytes = s.bytes(len)?;
            let end = off.checked_add(len).ok_or(LoadError::Malformed)?;
            if end > self.mem_len {
                return Err(LoadError::Malformed);
            }
            self.mem
                .get_mut(off..end)
                .ok_or(LoadError::Malformed)?
                .copy_from_slice(bytes);
        }
        Ok(())
    }

    fn sec_code(&mut self, s: &mut Rd<'_>, m: &mut Module) -> R<()> {
        let n = s.u32()? as usize;
        if n != self.nfuncs - self.nimports {
            return Err(LoadError::Malformed);
        }
        for i in 0..n {
            let size = s.u32()? as usize;
            let body = s.bytes(size)?;
            let fi = self.nimports + i;
            let mut b = Rd { b: body, at: 0 };
            let mut locals: usize = 0;
            for _ in 0..b.u32()? {
                let count = b.u32()? as usize;
                val_slots(b.u8()?)?;
                locals = locals.checked_add(count).ok_or(LoadError::Malformed)?;
            }
            let f = *self.funcs.get(fi).ok_or(LoadError::Malformed)?;
            if usize::from(f.params) + locals > MAX_LOCALS {
                return Err(LoadError::TooLarge("locals"));
            }
            let entry = self.code_len;
            if let Some(f) = self.funcs.get_mut(fi) {
                f.entry = entry as u16;
                f.locals = locals as u16;
            }
            let mut c = Compiler {
                code: &mut self.code,
                len: self.code_len,
                ctrl: [Ctrl::EMPTY; MAX_CTRL],
                nctrl: 0,
                types: [I32; MAX_OPERANDS],
                nvals: 0,
                slots: 0,
                nlocals: usize::from(f.params) + locals,
                nglobals: self.nglobals,
                funcs: &self.funcs,
                nfuncs: self.nfuncs,
                mtypes: &m.types,
                ntypes: m.ntypes,
                has_table: m.has_table,
                has_memory: m.has_memory,
            };
            c.function(&mut b, f.results)?;
            self.code_len = c.len;
            m.bodies += 1;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Func,
    Block,
    Loop,
    If,
}

/// One open control construct during compilation.
#[derive(Clone, Copy)]
struct Ctrl {
    kind: Kind,
    /// Result slots (0 or 1).
    results: u8,
    /// Operand values and slots at entry.
    vals: usize,
    slots: usize,
    /// Loop start (the branch target of a loop label).
    start: u16,
    /// Head of the chain of forward branches to this construct's end.
    fixups: u16,
    /// The `BRUNLESS` of an `if`, until its `else` or `end`.
    unless: u16,
    /// Code after a branch, return or unreachable is dead until `end` or
    /// `else`.
    dead: bool,
    /// Opened inside dead code: everything in it is dead.
    outer_dead: bool,
}

impl Ctrl {
    const EMPTY: Ctrl = Ctrl {
        kind: Kind::Block,
        results: 0,
        vals: 0,
        slots: 0,
        start: 0,
        fixups: NONE,
        unless: NONE,
        dead: false,
        outer_dead: false,
    };
}

struct Compiler<'a> {
    code: &'a mut [u32; CODE_WORDS],
    len: usize,
    ctrl: [Ctrl; MAX_CTRL],
    nctrl: usize,
    /// Operand types (I32 or I64).
    types: [u8; MAX_OPERANDS],
    nvals: usize,
    slots: usize,
    nlocals: usize,
    nglobals: usize,
    funcs: &'a [Func; MAX_FUNCS],
    nfuncs: usize,
    mtypes: &'a [(u8, u8); MAX_TYPES],
    ntypes: usize,
    has_table: bool,
    has_memory: bool,
}

fn slots_of(t: u8) -> usize {
    if t == I64 { 2 } else { 1 }
}

impl Compiler<'_> {
    fn emit(&mut self, w: u32) -> R<u16> {
        let at = self.len;
        // Word addresses must stay below the NONE sentinel.
        if at >= CODE_WORDS || at >= usize::from(NONE) {
            return Err(LoadError::TooLarge("code"));
        }
        *self.code.get_mut(at).ok_or(LoadError::TooLarge("code"))? = w;
        self.len += 1;
        Ok(at as u16)
    }

    fn top(&self) -> R<&Ctrl> {
        self.nctrl
            .checked_sub(1)
            .and_then(|i| self.ctrl.get(i))
            .ok_or(LoadError::Malformed)
    }

    fn top_mut(&mut self) -> R<&mut Ctrl> {
        let i = self.nctrl.checked_sub(1).ok_or(LoadError::Malformed)?;
        self.ctrl.get_mut(i).ok_or(LoadError::Malformed)
    }

    fn dead(&self) -> bool {
        self.top().map(|c| c.dead).unwrap_or(true)
    }

    fn push(&mut self, t: u8) -> R<()> {
        *self
            .types
            .get_mut(self.nvals)
            .ok_or(LoadError::TooLarge("operand stack"))? = t;
        self.nvals += 1;
        self.slots += slots_of(t);
        Ok(())
    }

    fn pop(&mut self) -> R<u8> {
        let floor = self.top()?.vals;
        if self.nvals <= floor {
            return Err(LoadError::Malformed);
        }
        self.nvals -= 1;
        let t = *self.types.get(self.nvals).ok_or(LoadError::Malformed)?;
        self.slots -= slots_of(t);
        Ok(t)
    }

    fn pop_i32(&mut self) -> R<()> {
        match self.pop()? {
            I32 => Ok(()),
            _ => Err(LoadError::Unsupported("i64 operand")),
        }
    }

    fn pops(&mut self, n: usize) -> R<()> {
        for _ in 0..n {
            self.pop_i32()?;
        }
        Ok(())
    }

    fn open(&mut self, kind: Kind, results: u8) -> R<()> {
        let outer_dead = self.dead() && self.nctrl > 0;
        let c = Ctrl {
            kind,
            results,
            vals: self.nvals,
            slots: self.slots,
            start: self.len as u16,
            fixups: NONE,
            unless: NONE,
            dead: outer_dead,
            outer_dead,
        };
        *self
            .ctrl
            .get_mut(self.nctrl)
            .ok_or(LoadError::TooLarge("nesting"))? = c;
        self.nctrl += 1;
        Ok(())
    }

    fn block_type(b: &mut Rd<'_>) -> R<u8> {
        match b.u8()? {
            0x40 => Ok(0),
            I32 => Ok(1),
            I64 => Err(LoadError::Unsupported("i64 block result")),
            0x7D | 0x7C => Err(LoadError::Unsupported("floats")),
            _ => Err(LoadError::Unsupported("multi-value block")),
        }
    }

    /// The branch argument for label `depth` from the current height, and
    /// links the branch word (about to be emitted at `self.len`) into the
    /// label's fixup chain when it is a forward branch.
    fn label(&mut self, depth: u32) -> R<u32> {
        let i = self
            .nctrl
            .checked_sub(1 + depth as usize)
            .ok_or(LoadError::Malformed)?;
        let here = self.len as u16;
        let slots = self.slots;
        let c = self.ctrl.get_mut(i).ok_or(LoadError::Malformed)?;
        let keep = if c.kind == Kind::Loop {
            0
        } else {
            u32::from(c.results)
        };
        let drop = slots
            .checked_sub(c.slots + keep as usize)
            .ok_or(LoadError::Malformed)?;
        let target = if c.kind == Kind::Loop {
            c.start
        } else {
            let prev = c.fixups;
            c.fixups = here;
            prev
        };
        isa::branch(target, keep, drop as u32).ok_or(LoadError::TooLarge("branch drop"))
    }

    fn patch(&mut self, mut at: u16, to: u16) -> R<()> {
        // Each step follows one link of a chain built in code order, so
        // the walk ends within `len` steps.
        for _ in 0..=self.len {
            if at == NONE {
                return Ok(());
            }
            let w = *self.code.get(usize::from(at)).ok_or(LoadError::Malformed)?;
            let (next, keep, drop) = isa::unbranch(isa::arg(w));
            let a = isa::branch(to, keep, drop).ok_or(LoadError::Malformed)?;
            *self
                .code
                .get_mut(usize::from(at))
                .ok_or(LoadError::Malformed)? = word(isa::op(w), a);
            at = next;
        }
        Err(LoadError::Malformed)
    }

    fn set_dead(&mut self) -> R<()> {
        let floor = self.top()?.vals;
        let fslots = self.top()?.slots;
        self.nvals = floor;
        self.slots = fslots;
        self.top_mut()?.dead = true;
        Ok(())
    }

    fn memarg(b: &mut Rd<'_>) -> R<u32> {
        let _align = b.u32()?;
        b.u32()
    }

    fn func(&self, idx: u32) -> R<Func> {
        if idx as usize >= self.nfuncs {
            return Err(LoadError::Malformed);
        }
        self.funcs
            .get(idx as usize)
            .copied()
            .ok_or(LoadError::Malformed)
    }

    fn function(&mut self, b: &mut Rd<'_>, results: u8) -> R<()> {
        self.open(Kind::Func, results)?;
        while self.nctrl > 0 {
            let opc = b.u8()?;
            self.op(b, opc)?;
        }
        if !b.done() {
            return Err(LoadError::Malformed);
        }
        Ok(())
    }

    fn need_memory(&self) -> R<()> {
        self.has_memory.then_some(()).ok_or(LoadError::Malformed)
    }

    #[allow(clippy::too_many_lines)]
    fn op(&mut self, b: &mut Rd<'_>, opc: u8) -> R<()> {
        let dead = self.dead();
        match opc {
            0x00 => {
                if !dead {
                    self.emit(word(isa::UNREACHABLE, 0))?;
                    self.set_dead()?;
                }
            }
            0x01 => {}
            0x02 | 0x03 => {
                let r = Self::block_type(b)?;
                let kind = if opc == 0x02 { Kind::Block } else { Kind::Loop };
                self.open(kind, r)?;
            }
            0x04 => {
                let r = Self::block_type(b)?;
                if !dead {
                    self.pop_i32()?;
                }
                self.open(Kind::If, r)?;
                if !dead {
                    let at = self.emit(word(isa::BRUNLESS, u32::from(NONE)))?;
                    self.top_mut()?.unless = at;
                }
            }
            0x05 => {
                let c = *self.top()?;
                if c.kind != Kind::If {
                    return Err(LoadError::Malformed);
                }
                if !c.outer_dead {
                    if !c.dead {
                        let a = self.label(0)?;
                        self.emit(word(isa::BR, a))?;
                    }
                    let here = self.len as u16;
                    let unless = c.unless;
                    if unless != NONE {
                        let w = *self
                            .code
                            .get(usize::from(unless))
                            .ok_or(LoadError::Malformed)?;
                        *self
                            .code
                            .get_mut(usize::from(unless))
                            .ok_or(LoadError::Malformed)? = word(isa::op(w), u32::from(here));
                    }
                    let t = self.top_mut()?;
                    t.unless = NONE;
                    t.dead = false;
                    self.nvals = c.vals;
                    self.slots = c.slots;
                }
            }
            0x0B => {
                let c = *self.top()?;
                self.nctrl -= 1;
                if c.outer_dead {
                    return Ok(());
                }
                let here = self.len as u16;
                self.patch(c.fixups, here)?;
                if c.unless != NONE {
                    // An `if` without `else`: false skips to the end.
                    let w = *self
                        .code
                        .get(usize::from(c.unless))
                        .ok_or(LoadError::Malformed)?;
                    *self
                        .code
                        .get_mut(usize::from(c.unless))
                        .ok_or(LoadError::Malformed)? = word(isa::op(w), u32::from(here));
                }
                self.nvals = c.vals;
                self.slots = c.slots;
                if c.results == 1 {
                    // A one-result construct leaves an i32 (blocks with
                    // i64 results were refused).
                    *self
                        .types
                        .get_mut(self.nvals)
                        .ok_or(LoadError::TooLarge("operand stack"))? = I32;
                    self.nvals += 1;
                    self.slots += 1;
                }
                if c.kind == Kind::Func {
                    self.emit(word(isa::RET, u32::from(c.results)))?;
                }
            }
            0x0C => {
                let d = b.u32()?;
                if !dead {
                    let a = self.label(d)?;
                    self.emit(word(isa::BR, a))?;
                    self.set_dead()?;
                }
            }
            0x0D => {
                let d = b.u32()?;
                if !dead {
                    self.pop_i32()?;
                    let a = self.label(d)?;
                    self.emit(word(isa::BRIF, a))?;
                }
            }
            0x0E => {
                let n = b.u32()?;
                if n > 0x00FF_FFFF {
                    return Err(LoadError::TooLarge("br_table"));
                }
                if !dead {
                    self.pop_i32()?;
                    self.emit(word(isa::BRTABLE, n))?;
                }
                for _ in 0..=n {
                    let d = b.u32()?;
                    if !dead {
                        let a = self.label(d)?;
                        self.emit(word(isa::BR, a))?;
                    }
                }
                if !dead {
                    self.set_dead()?;
                }
            }
            0x0F => {
                if !dead {
                    let keep = u32::from(self.ctrl.first().ok_or(LoadError::Malformed)?.results);
                    self.emit(word(isa::RET, keep))?;
                    self.set_dead()?;
                }
            }
            0x10 => {
                let idx = b.u32()?;
                let f = self.func(idx)?;
                if !dead {
                    self.pops(usize::from(f.params))?;
                    if f.import == Func::LOCAL {
                        self.emit(word(isa::CALL, idx))?;
                    } else {
                        self.emit(word(isa::HOST, u32::from(f.import)))?;
                    }
                    if f.results == 1 {
                        self.push(I32)?;
                    }
                }
            }
            0x11 => {
                let ti = b.u32()? as usize;
                if b.u32()? != 0 || !self.has_table {
                    return Err(LoadError::Malformed);
                }
                if ti >= self.ntypes {
                    return Err(LoadError::Malformed);
                }
                let (p, r) = *self.mtypes.get(ti).ok_or(LoadError::Malformed)?;
                if !dead {
                    self.pop_i32()?;
                    self.pops(usize::from(p))?;
                    self.emit(word(isa::CALLI, (u32::from(p) << 8) | u32::from(r)))?;
                    if r == 1 {
                        self.push(I32)?;
                    }
                }
            }
            0x1A => {
                if !dead {
                    let t = self.pop()?;
                    self.emit(word(isa::DROP, slots_of(t) as u32))?;
                }
            }
            0x1B | 0x1C => {
                if opc == 0x1C {
                    let n = b.u32()?;
                    for _ in 0..n {
                        if b.u8()? != I32 {
                            return Err(LoadError::Unsupported("typed select"));
                        }
                    }
                }
                if !dead {
                    self.pops(3)?;
                    self.emit(word(isa::SELECT, 0))?;
                    self.push(I32)?;
                }
            }
            0x20..=0x22 => {
                let i = b.u32()?;
                if i as usize >= self.nlocals {
                    return Err(LoadError::Malformed);
                }
                if !dead {
                    let o = [isa::LGET, isa::LSET, isa::LTEE][usize::from(opc - 0x20)];
                    if opc != 0x20 {
                        self.pop_i32()?;
                    }
                    self.emit(word(o, i))?;
                    if opc != 0x21 {
                        self.push(I32)?;
                    }
                }
            }
            0x23 | 0x24 => {
                let i = b.u32()?;
                if i as usize >= self.nglobals {
                    return Err(LoadError::Malformed);
                }
                if !dead {
                    if opc == 0x24 {
                        self.pop_i32()?;
                        self.emit(word(isa::GSET, i))?;
                    } else {
                        self.emit(word(isa::GGET, i))?;
                        self.push(I32)?;
                    }
                }
            }
            0x28 | 0x2C..=0x2F => {
                let off = Self::memarg(b)?;
                self.need_memory()?;
                if !dead {
                    let kind = match opc {
                        0x28 => isa::load::W32,
                        0x2C => isa::load::S8,
                        0x2D => isa::load::U8,
                        0x2E => isa::load::S16,
                        _ => isa::load::U16,
                    };
                    self.pop_i32()?;
                    self.emit(word(isa::LOAD, kind))?;
                    self.emit(off)?;
                    self.push(I32)?;
                }
            }
            0x36 | 0x37 | 0x3A | 0x3B => {
                let off = Self::memarg(b)?;
                self.need_memory()?;
                if !dead {
                    let kind = match opc {
                        0x36 => isa::store::W32,
                        0x37 => isa::store::W64,
                        0x3A => isa::store::W8,
                        _ => isa::store::W16,
                    };
                    let v = self.pop()?;
                    if (v == I64) != (opc == 0x37) {
                        return Err(LoadError::Unsupported("i64 operand"));
                    }
                    self.pop_i32()?;
                    self.emit(word(isa::STORE, kind))?;
                    self.emit(off)?;
                }
            }
            0x3F | 0x40 => {
                if b.u32()? != 0 {
                    return Err(LoadError::Malformed);
                }
                self.need_memory()?;
                if !dead {
                    if opc == 0x40 {
                        self.pop_i32()?;
                        self.emit(word(isa::MGROW, 0))?;
                    } else {
                        self.emit(word(isa::MSIZE, 0))?;
                    }
                    self.push(I32)?;
                }
            }
            0x41 => {
                let v = b.i32()?;
                if !dead {
                    self.konst(v)?;
                    self.push(I32)?;
                }
            }
            0x42 => {
                let v = b.sleb(64)?;
                if !dead {
                    self.konst(v as i32)?;
                    self.konst((v >> 32) as i32)?;
                    self.push(I64)?;
                }
            }
            0x45 | 0x67..=0x69 | 0xC0 | 0xC1 => {
                if !dead {
                    let u = match opc {
                        0x45 => isa::un::EQZ,
                        0x67 => isa::un::CLZ,
                        0x68 => isa::un::CTZ,
                        0x69 => isa::un::POPCNT,
                        0xC0 => isa::un::EXT8,
                        _ => isa::un::EXT16,
                    };
                    self.pop_i32()?;
                    self.emit(word(isa::UN, u))?;
                    self.push(I32)?;
                }
            }
            0x46..=0x4F | 0x6A..=0x78 => {
                if !dead {
                    let o = if opc <= 0x4F {
                        u32::from(opc - 0x46)
                    } else {
                        u32::from(opc - 0x6A) + isa::bin::ARITH
                    };
                    self.pops(2)?;
                    self.emit(word(isa::BIN, o))?;
                    self.push(I32)?;
                }
            }
            0xFC => {
                let sub = b.u32()?;
                match sub {
                    10 => {
                        if b.u32()? != 0 || b.u32()? != 0 {
                            return Err(LoadError::Malformed);
                        }
                        self.need_memory()?;
                        if !dead {
                            self.pops(3)?;
                            self.emit(word(isa::MCOPY, 0))?;
                        }
                    }
                    11 => {
                        if b.u32()? != 0 {
                            return Err(LoadError::Malformed);
                        }
                        self.need_memory()?;
                        if !dead {
                            self.pops(3)?;
                            self.emit(word(isa::MFILL, 0))?;
                        }
                    }
                    _ => return Err(LoadError::Unsupported("0xFC instruction")),
                }
            }
            0x29..=0x35
            | 0x38
            | 0x39
            | 0x3C..=0x3E
            | 0x43
            | 0x44
            | 0x50..=0x66
            | 0x79..=0xBF
            | 0xC2..=0xC4 => return Err(LoadError::Unsupported("i64 or float instruction")),
            _ => return Err(LoadError::Unsupported("instruction")),
        }
        Ok(())
    }

    fn konst(&mut self, v: i32) -> R<()> {
        if isa::fits24(v) {
            self.emit(word(isa::CONST, v as u32))?;
        } else {
            self.emit(word(isa::CONST32, 0))?;
            self.emit(v as u32)?;
        }
        Ok(())
    }
}

/// Compile-time sanity of the table sizes against the ISA's fields.
const _: () = {
    assert!(CODE_WORDS <= NONE as usize);
    assert!(MAX_FUNCS <= 1 << 24);
    assert!(CALL_DEPTH > 0);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cl3_leb_reader_handles_edges() {
        let mut r = Rd {
            b: &[0xE5, 0x8E, 0x26, 0x7F, 0x80, 0x7F],
            at: 0,
        };
        assert_eq!(r.u32().unwrap(), 624_485);
        assert_eq!(r.i32().unwrap(), -1);
        assert_eq!(r.i32().unwrap(), -128);
        let mut bad = Rd {
            b: &[0xFF, 0xFF, 0xFF, 0xFF, 0x7F],
            at: 0,
        };
        assert_eq!(bad.u32(), Err(LoadError::Malformed));
        let mut long = Rd {
            b: &[0x80, 0x80, 0x80, 0x80, 0x80, 0x00],
            at: 0,
        };
        assert_eq!(long.u32(), Err(LoadError::Malformed));
    }
}
