//! Parsing: section offsets, the function index and the branch table.
//!
//! The index is plain `u32` words in a slice someone else owns:
//! `[type offsets][functions: type, code offset][branches: pc, target]`.

use crate::Error;
use crate::exec::Limits;
use crate::reader::Reader;

/// The declared linear memory.
#[derive(Clone, Copy)]
pub(crate) struct Mem {
    pub min: u32,
    pub max: u32,
    /// log2 of the page size: 16, or 0 with custom page sizes.
    pub shift: u8,
}

const NONE: u32 = u32::MAX;

/// What a walk over the image found. The counts size the index, and while
/// it is being filled they are the write positions.
#[derive(Clone, Copy, Default)]
pub(crate) struct Index {
    pub types: u32,
    pub funcs: u32,
    /// Branch entries: one per `block` and `else`, two per `if`. Sorted by
    /// `pc`. Keys: a `block` or `if` opcode (to just past its `end`), an
    /// `if` opcode + 1 (to just past its `else`, or its `end` when it has
    /// none), an `else` opcode (past the `end`).
    pub ctrl: u32,
    /// Deepest block nesting in any function.
    pub depth: u32,
    pub globals: u32,
    pub n_imports: u32,
    /// `(start, end)` of each section's contents, by section id.
    sec: [(u32, u32); 12],
    pub memory: Option<Mem>,
    /// Declared table size.
    pub table: Option<u32>,
    pub start: Option<u32>,
}

impl Index {
    /// Words the index takes.
    pub fn words(&self) -> usize {
        self.types as usize + 2 * self.funcs as usize + 2 * self.ctrl as usize
    }

    /// Words parsing needs on top, for its stack of open blocks.
    pub fn scratch(&self) -> usize {
        2 * self.depth as usize
    }
}

/// The storage a module needs, from [`footprint`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Footprint {
    /// Bytes of index the module keeps (4-byte words).
    pub index: usize,
    /// Bytes parsing needs on top, and gives back.
    pub scratch: usize,
    /// Globals (one 8-byte slot each).
    pub globals: usize,
    /// Table entries (one word each).
    pub table: usize,
}

impl Footprint {
    /// The `u32` words a [`crate::Machine`] needs under `limits`: the
    /// index, then the table, open blocks and call frames (parsing borrows
    /// that second part as scratch).
    pub fn words(&self, limits: &Limits) -> usize {
        let run = self.table + 3 * limits.labels + 3 * limits.frames;
        self.index / 4 + run.max(self.scratch / 4)
    }

    /// The `u64` slots a [`crate::Machine`] needs: globals and the value
    /// stack.
    pub fn slots(&self, limits: &Limits) -> usize {
        self.globals + limits.stack
    }
}

/// Measures `bytes` without storing anything. A malformed image may
/// measure and still fail to load.
pub fn footprint(bytes: &[u8]) -> Result<Footprint, Error> {
    let x = walk(bytes, None)?;
    Ok(Footprint {
        index: 4 * x.words(),
        scratch: 4 * x.scratch(),
        globals: x.globals as usize,
        table: x.table.unwrap_or(0) as usize,
    })
}

/// An upper bound on [`Footprint::index`] for any image of `len` bytes:
/// the densest entry is an `if`/`else`/`end`, 24 bytes for four.
pub const fn index_bound(len: usize) -> usize {
    len.saturating_mul(6)
}

fn valtypes(s: &[u8]) -> Result<(), Error> {
    if s.iter().all(|t| matches!(t, 0x7C..=0x7F | 0x70 | 0x6F)) {
        Ok(())
    } else {
        Err(Error::Unsupported)
    }
}

fn limits(s: &mut Reader, pages: bool) -> Result<Mem, Error> {
    let flags = s.u8()?;
    // Bit 0: a maximum follows. Bit 3: a page size follows (memories only).
    if flags & !(if pages { 9 } else { 1 }) != 0 {
        return Err(Error::Unsupported);
    }
    let min = s.u32()?;
    let max = if flags & 1 != 0 { s.u32()? } else { u32::MAX };
    let shift = if flags & 8 != 0 { s.u32()? } else { 16 };
    if shift > 16 {
        return Err(Error::Unsupported);
    }
    Ok(Mem {
        min,
        max,
        shift: shift as u8,
    })
}

/// The index being written, and the stack of open blocks.
struct Fill<'a> {
    types: &'a mut [u32],
    funcs: &'a mut [[u32; 2]],
    ctrl: &'a mut [[u32; 2]],
    /// `[branch entry of the opener or NONE, of its else or NONE]`.
    open: &'a mut [[u32; 2]],
    n_open: usize,
}

fn put<T>(s: &mut [T], at: u32, v: T) -> Result<(), Error> {
    *s.get_mut(at as usize).ok_or(Error::Malformed)? = v;
    Ok(())
}

impl Fill<'_> {
    fn is_if(&self, b: &[u8], idx: u32) -> bool {
        self.ctrl
            .get(idx as usize)
            .and_then(|e| b.get(e[0] as usize))
            == Some(&0x04)
    }

    fn target(&mut self, idx: u32, target: usize) -> Result<(), Error> {
        self.ctrl.get_mut(idx as usize).ok_or(Error::Malformed)?[1] = target as u32;
        Ok(())
    }
}

/// The three parts of an index sized by `x`.
type Parts<'a> = (&'a mut [u32], &'a mut [[u32; 2]], &'a mut [[u32; 2]]);

fn parts_mut<'a>(w: &'a mut [u32], x: &Index) -> Option<Parts<'a>> {
    let (types, w) = w.split_at_mut_checked(x.types as usize)?;
    let (funcs, w) = w.split_at_mut_checked(2 * x.funcs as usize)?;
    let (ctrl, _) = w.split_at_mut_checked(2 * x.ctrl as usize)?;
    Some((types, funcs.as_chunks_mut().0, ctrl.as_chunks_mut().0))
}

/// Parses `bytes`, writing the index to the start of `index` and using
/// `scratch` while it works. Every function body is walked twice: once to
/// size the index, once to fill it.
pub(crate) fn parse(bytes: &[u8], index: &mut [u32], scratch: &mut [u32]) -> Result<Index, Error> {
    let x = walk(bytes, None)?;
    if scratch.len() < x.scratch() {
        return Err(Error::OutOfMemory);
    }
    let (types, funcs, ctrl) = parts_mut(index, &x).ok_or(Error::OutOfMemory)?;
    let mut fill = Fill {
        types,
        funcs,
        ctrl,
        open: scratch.as_chunks_mut().0,
        n_open: 0,
    };
    walk(bytes, Some(&mut fill))
}

/// One pass over the image: counts, and with `fill` also writes.
fn walk(bytes: &[u8], mut fill: Option<&mut Fill>) -> Result<Index, Error> {
    if bytes.len() > u32::MAX as usize {
        return Err(Error::Unsupported);
    }
    let mut x = Index::default();
    let mut r = Reader { b: bytes, pos: 0 };
    if r.bytes(8)? != b"\0asm\x01\0\0\0" {
        return Err(Error::Malformed);
    }
    let mut bodies = 0u32;
    while r.pos < bytes.len() {
        let id = r.u8()?;
        let len = r.u32()? as usize;
        let end = r.pos.checked_add(len).ok_or(Error::Malformed)?;
        let mut s = Reader {
            b: bytes.get(..end).ok_or(Error::Malformed)?,
            pos: r.pos,
        };
        match id {
            1 => {
                for _ in 0..s.count()? {
                    if s.u8()? != 0x60 {
                        return Err(Error::Unsupported);
                    }
                    if let Some(f) = fill.as_deref_mut() {
                        put(f.types, x.types, s.pos as u32)?;
                    }
                    x.types += 1;
                    valtypes(s.name()?)?;
                    valtypes(s.name()?)?;
                }
            }
            2 | 3 => {
                if id == 2 {
                    x.sec[2] = (s.pos as u32, end as u32);
                    x.n_imports = s.count()?;
                }
                let n = if id == 2 { x.n_imports } else { s.count()? };
                for _ in 0..n {
                    if id == 2 {
                        s.name()?;
                        s.name()?;
                        if s.u8()? != 0 {
                            return Err(Error::Unsupported);
                        }
                    }
                    let ty = s.u32()?;
                    if let Some(f) = fill.as_deref_mut() {
                        put(f.funcs, x.funcs, [ty, 0])?;
                    }
                    x.funcs += 1;
                }
            }
            4 | 5 => match s.u32()? {
                0 => {}
                1 if id == 4 => {
                    if s.u8()? != 0x70 {
                        return Err(Error::Unsupported);
                    }
                    x.table = Some(limits(&mut s, false)?.min);
                }
                1 => x.memory = Some(limits(&mut s, true)?),
                _ => return Err(Error::Unsupported),
            },
            6 | 7 | 9 | 11 => {
                x.sec[id as usize] = (s.pos as u32, end as u32);
                if id == 6 {
                    x.globals = s.count()?;
                }
            }
            8 => x.start = Some(s.u32()?),
            10 => {
                bodies = s.count()?;
                for i in 0..bodies {
                    let size = s.u32()? as usize;
                    let fend = s.pos.checked_add(size).ok_or(Error::Malformed)?;
                    if let Some(f) = fill.as_deref_mut() {
                        let at = x.n_imports.checked_add(i).ok_or(Error::Malformed)?;
                        f.funcs.get_mut(at as usize).ok_or(Error::Malformed)?[1] = s.pos as u32;
                    }
                    let body = Reader {
                        b: s.b.get(..fend).ok_or(Error::Malformed)?,
                        pos: s.pos,
                    };
                    scan(body, &mut x, fill.as_deref_mut())?;
                    s.pos = fend;
                }
            }
            0 | 12 => {}
            _ => return Err(Error::Unsupported),
        }
        r.pos = end;
    }
    if x.funcs.checked_sub(x.n_imports) != Some(bodies) {
        return Err(Error::Malformed);
    }
    Ok(x)
}

/// Walks one function body: counts (and with `fill` records) its branch
/// targets and refuses unknown opcodes. Keys are written in address order,
/// so the branch table is sorted.
fn scan(mut r: Reader, x: &mut Index, mut fill: Option<&mut Fill>) -> Result<(), Error> {
    for _ in 0..r.count()? {
        r.u32()?;
        valtypes(&[r.u8()?])?;
    }
    if let Some(f) = fill.as_deref_mut() {
        f.n_open = 0;
    }
    let mut depth = 0u32;
    loop {
        let pc = r.pos as u32;
        let op = r.u8()?;
        match op {
            0x02..=0x04 => {
                r.blocktype()?;
                depth += 1;
                x.depth = x.depth.max(depth);
                let mut idx = NONE;
                if op != 0x03 {
                    idx = x.ctrl;
                    if let Some(f) = fill.as_deref_mut() {
                        put(f.ctrl, idx, [pc, 0])?;
                        if op == 0x04 {
                            put(f.ctrl, idx + 1, [pc + 1, 0])?;
                        }
                    }
                    x.ctrl += if op == 0x04 { 2 } else { 1 };
                }
                if let Some(f) = fill.as_deref_mut() {
                    put(f.open, f.n_open as u32, [idx, NONE])?;
                    f.n_open += 1;
                }
            }
            0x05 => {
                if let Some(f) = fill.as_deref_mut() {
                    let top = f.n_open.checked_sub(1).and_then(|i| f.open.get(i));
                    let [opener, els] = *top.ok_or(Error::Malformed)?;
                    if els != NONE || !f.is_if(r.b, opener) {
                        return Err(Error::Malformed);
                    }
                    put(f.open, f.n_open as u32 - 1, [opener, x.ctrl])?;
                    f.target(opener + 1, r.pos)?;
                    put(f.ctrl, x.ctrl, [pc, 0])?;
                }
                x.ctrl += 1;
            }
            0x0B => {
                if depth == 0 {
                    // The function's own `end` must close the body.
                    return if r.pos == r.b.len() {
                        Ok(())
                    } else {
                        Err(Error::Malformed)
                    };
                }
                depth -= 1;
                if let Some(f) = fill.as_deref_mut() {
                    f.n_open = f.n_open.checked_sub(1).ok_or(Error::Malformed)?;
                    let [idx, els] = *f.open.get(f.n_open).ok_or(Error::Malformed)?;
                    if idx != NONE {
                        f.target(idx, r.pos)?;
                        if f.is_if(r.b, idx) {
                            f.target(if els != NONE { els } else { idx + 1 }, r.pos)?;
                        }
                    }
                }
            }
            0x0C | 0x0D | 0x10 | 0x20..=0x24 => {
                r.u32()?;
            }
            0x0E => {
                for _ in 0..=r.count()? {
                    r.u32()?;
                }
            }
            0x11 => {
                r.u32()?;
                if r.u32()? != 0 {
                    return Err(Error::Unsupported);
                }
            }
            0x1C => {
                let n = r.u32()? as usize;
                r.skip(n)?;
            }
            0x28..=0x3E => {
                // Bit 6 of the alignment selects another memory.
                if r.u32()? >= 0x40 {
                    return Err(Error::Unsupported);
                }
                r.u32()?;
            }
            0x3F | 0x40 => {
                if r.u8()? != 0 {
                    return Err(Error::Unsupported);
                }
            }
            0x41 | 0x42 => {
                r.leb(true)?;
            }
            0x43 => r.skip(4)?,
            0x44 => r.skip(8)?,
            0x00 | 0x01 | 0x0F | 0x1A | 0x1B | 0x45..=0xC4 => {}
            0xFC => match r.u32()? {
                0..=7 => {}
                10 => {
                    if r.le(2)? != 0 {
                        return Err(Error::Unsupported);
                    }
                }
                11 => {
                    if r.u8()? != 0 {
                        return Err(Error::Unsupported);
                    }
                }
                _ => return Err(Error::Unsupported),
            },
            _ => return Err(Error::Unsupported),
        }
    }
}

/// One function import.
pub struct Import<'a> {
    pub module: &'a [u8],
    pub name: &'a [u8],
    /// Parameter value types (`0x7F` i32, `0x7E` i64, `0x7D` f32, `0x7C` f64).
    pub params: &'a [u8],
    pub results: &'a [u8],
}

/// What an export names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportKind {
    Func,
    Table,
    Memory,
    Global,
}

/// One export.
pub struct Export<'a> {
    pub name: &'a [u8],
    pub kind: ExportKind,
    /// Index in the kind's index space.
    pub index: u32,
}

/// The image and its index, as the interpreter and the inspection
/// functions use them.
#[derive(Clone, Copy)]
pub(crate) struct View<'a> {
    pub b: &'a [u8],
    pub x: &'a Index,
    types: &'a [u32],
    funcs: &'a [[u32; 2]],
    ctrl: &'a [[u32; 2]],
}

impl<'a> View<'a> {
    /// `words` starts with the index `x` describes. Too short (not the
    /// buffer the module was parsed into) and every lookup just fails.
    pub fn new(b: &'a [u8], x: &'a Index, words: &'a [u32]) -> Self {
        let part = |from: usize, n: usize| {
            let s = words.get(from..).unwrap_or(&[]);
            s.get(..n).unwrap_or(&[])
        };
        let (t, f) = (x.types as usize, 2 * x.funcs as usize);
        View {
            b,
            x,
            types: part(0, t),
            funcs: part(t, f).as_chunks().0,
            ctrl: part(t + f, 2 * x.ctrl as usize).as_chunks().0,
        }
    }

    /// Where the branch keyed by `pc` lands.
    pub fn target(self, pc: usize) -> Result<usize, Error> {
        let i = self.ctrl.binary_search_by_key(&(pc as u32), |e| e[0]);
        let i = i.map_err(|_| Error::Invalid)?;
        self.ctrl
            .get(i)
            .map(|e| e[1] as usize)
            .ok_or(Error::Invalid)
    }

    /// `(type index, code offset)` of function `f`.
    pub fn func(self, f: u32) -> Option<(u32, usize)> {
        self.funcs.get(f as usize).map(|e| (e[0], e[1] as usize))
    }

    /// The encoded parameter and result types of type `ty`.
    pub fn sig(self, ty: u32) -> Result<(&'a [u8], &'a [u8]), Error> {
        let pos = *self.types.get(ty as usize).ok_or(Error::Invalid)? as usize;
        let mut r = Reader { b: self.b, pos };
        Ok((r.name()?, r.name()?))
    }

    /// A reader over section `id`, empty when the module has none.
    pub fn section(self, id: usize) -> Reader<'a> {
        let (start, end) = self.x.sec.get(id).copied().unwrap_or((0, 0));
        Reader {
            b: self.b.get(..end as usize).unwrap_or(&[]),
            pos: start as usize,
        }
    }

    pub fn func_type(self, func: u32) -> Option<(&'a [u8], &'a [u8])> {
        self.sig(self.func(func)?.0).ok()
    }

    pub fn page_size(self) -> Option<u32> {
        self.x.memory.map(|m| 1 << m.shift)
    }

    pub fn imports(self) -> impl Iterator<Item = Import<'a>> + 'a {
        let mut r = self.section(2);
        let n = r.u32().unwrap_or(0);
        (0..n).map_while(move |i| {
            let module = r.name().ok()?;
            let name = r.name().ok()?;
            r.u8().ok()?;
            r.u32().ok()?;
            let (params, results) = self.func_type(i)?;
            Some(Import {
                module,
                name,
                params,
                results,
            })
        })
    }

    pub fn exports(self) -> impl Iterator<Item = Export<'a>> + 'a {
        let mut r = self.section(7);
        let n = r.u32().unwrap_or(0);
        (0..n).map_while(move |_| {
            let name = r.name().ok()?;
            let kind = match r.u8().ok()? {
                0 => ExportKind::Func,
                1 => ExportKind::Table,
                2 => ExportKind::Memory,
                3 => ExportKind::Global,
                _ => return None,
            };
            let index = r.u32().ok()?;
            Some(Export { name, kind, index })
        })
    }

    pub fn export(self, name: &str, kind: ExportKind) -> Option<u32> {
        self.exports()
            .find(|e| e.kind == kind && e.name == name.as_bytes())
            .map(|e| e.index)
    }

    pub fn custom_section(self, name: &str) -> Option<&'a [u8]> {
        let mut r = Reader { b: self.b, pos: 8 };
        while r.pos < self.b.len() {
            let id = r.u8().ok()?;
            let body = r.name().ok()?;
            if id == 0 {
                let mut s = Reader { b: body, pos: 0 };
                if s.name().ok()? == name.as_bytes() {
                    return body.get(s.pos..);
                }
            }
        }
        None
    }
}

/// A parsed module that owns its index (feature `alloc`): the image `B`
/// (borrowed, as `&[u8]` in flash, or owned, as `Vec<u8>` or `Box<[u8]>`)
/// and what cannot be read from it in constant time: one offset per type,
/// eight bytes per function and eight per branch entry (see
/// [`footprint`]).
#[cfg(feature = "alloc")]
pub struct Module<B> {
    bytes: B,
    x: Index,
    words: alloc::vec::Vec<u32>,
}

#[cfg(feature = "alloc")]
pub(crate) fn zeroed<T: Clone>(n: usize, v: T) -> Result<alloc::vec::Vec<T>, Error> {
    let mut x = alloc::vec::Vec::new();
    x.try_reserve_exact(n).map_err(|_| Error::OutOfMemory)?;
    x.resize(n, v);
    Ok(x)
}

#[cfg(feature = "alloc")]
impl<B: AsRef<[u8]>> Module<B> {
    /// Parses `bytes`. Every function body is walked twice, to size and
    /// then to build the branch table, refusing opcodes this interpreter
    /// lacks; nothing is type-checked (see the crate docs). Allocates
    /// exactly [`footprint`].
    pub fn new(bytes: B) -> Result<Self, Error> {
        let b = bytes.as_ref();
        let sized = walk(b, None)?;
        let mut words = zeroed(sized.words(), 0)?;
        let mut scratch = zeroed(sized.scratch(), 0)?;
        let x = parse(b, &mut words, &mut scratch)?;
        Ok(Module { bytes, x, words })
    }

    pub(crate) fn view(&self) -> View<'_> {
        View::new(self.bytes.as_ref(), &self.x, &self.words)
    }

    /// The image.
    pub fn bytes(&self) -> &[u8] {
        self.bytes.as_ref()
    }

    /// Number of functions, imports first.
    pub fn func_count(&self) -> u32 {
        self.x.funcs
    }

    /// Number of imported functions; they are functions `0..import_count`,
    /// and that index is what [`crate::Host::call`] receives.
    pub fn import_count(&self) -> u32 {
        self.x.n_imports
    }

    /// The parameter and result value types of function `func`.
    pub fn func_type(&self, func: u32) -> Option<(&[u8], &[u8])> {
        self.view().func_type(func)
    }

    /// The page size of the module's memory in bytes (65536, or 1 with
    /// custom page sizes); `None` without a memory.
    pub fn page_size(&self) -> Option<u32> {
        self.view().page_size()
    }

    /// The function imports, in index order.
    pub fn imports(&self) -> impl Iterator<Item = Import<'_>> + '_ {
        self.view().imports()
    }

    /// The exports, in section order.
    pub fn exports(&self) -> impl Iterator<Item = Export<'_>> + '_ {
        self.view().exports()
    }

    /// The index of the exported function `name`.
    pub fn export_func(&self, name: &str) -> Option<u32> {
        self.view().export(name, ExportKind::Func)
    }

    /// The index of the exported global `name`.
    pub fn export_global(&self, name: &str) -> Option<u32> {
        self.view().export(name, ExportKind::Global)
    }

    /// The contents of the first custom section called `name`.
    pub fn custom_section(&self, name: &str) -> Option<&[u8]> {
        self.view().custom_section(name)
    }
}
