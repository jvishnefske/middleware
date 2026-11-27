//! Instantiation and the interpreter loop.

use crate::Error;
#[cfg(feature = "alloc")]
use crate::module::Module;
use crate::module::{Index, View};
use crate::reader::Reader;
#[cfg(feature = "alloc")]
use alloc::vec::Vec;

/// An empty table slot.
const NULL: u32 = u32::MAX;

/// The imports of a module.
pub trait Host {
    /// Calls import `import` (its index among the module's function
    /// imports; see [`Module::imports`]). `vals` starts with the arguments
    /// and receives the results, from slot 0; it is as long as the longer
    /// of the two. An `i32` is in the low half of its slot, an `f32` or
    /// `f64` is its bit pattern. `mem` is the linear memory.
    fn call(&mut self, import: u32, mem: &mut [u8], vals: &mut [u64]) -> Result<(), Error>;
}

/// For modules without imports: any call fails.
impl Host for () {
    fn call(&mut self, _: u32, _: &mut [u8], _: &mut [u64]) -> Result<(), Error> {
        Err(Error::Unsupported)
    }
}

/// What an instance may use. Everything but linear memory is allocated
/// once, in [`Instance::new`].
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Value stack, in 8-byte slots: arguments, locals and operands of
    /// every active call.
    pub stack: usize,
    /// Nested calls (12 bytes each).
    pub frames: usize,
    /// Open blocks across all active calls, one per call included
    /// (12 bytes each).
    pub labels: usize,
    /// Bytes backing linear memory. Below the module's declared size, the
    /// rest of its memory traps on access.
    pub memory_bytes: usize,
    /// Table entries (4 bytes each).
    pub table: usize,
    /// Fuel for the start function; see [`Instance::set_fuel`].
    pub fuel: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            stack: 1024,
            frames: 64,
            labels: 256,
            memory_bytes: usize::MAX,
            table: 1024,
            fuel: u64::MAX,
        }
    }
}

const BLOCK: u8 = 0;
const LOOP: u8 = 1;
const FUNC: u8 = 2;

#[derive(Clone, Copy)]
struct Label {
    /// `LOOP`: the first instruction of the body. `BLOCK` (also `if`):
    /// the opening opcode, resolved through the branch table when taken.
    pc: u32,
    /// Stack height the branch unwinds to.
    sp: u32,
    /// Values a branch carries.
    arity: u16,
    kind: u8,
}

/// The storage a [`Machine`] runs in: three slices the caller owns, sized
/// with [`crate::Footprint`]. They can be statics, so nothing has to come
/// from an allocator.
pub struct Buffers<'a> {
    /// The module's index, then its table, open blocks and call frames:
    /// at least [`crate::Footprint::words`].
    pub words: &'a mut [u32],
    /// Globals and the value stack: at least [`crate::Footprint::slots`].
    pub slots: &'a mut [u64],
    /// Linear memory. The module's memory grows up to this length; shorter
    /// than its initial size, the rest traps on access as with
    /// [`Limits::memory_bytes`].
    pub memory: &'a mut [u8],
}

/// Linear memory: a fixed slice, or (feature `alloc`) a `Vec` that grows.
pub(crate) trait Mem {
    fn bytes(&mut self) -> &mut [u8];
    /// Makes the memory `len` bytes, the new ones zero. Never shrinks.
    fn grow(&mut self, len: usize) -> bool;
    /// The most [`Mem::grow`] can reach.
    fn cap(&self) -> usize;
}

/// The first `len` bytes of a caller's buffer.
struct Fixed<'a> {
    buf: &'a mut [u8],
    len: &'a mut usize,
}

impl Mem for Fixed<'_> {
    fn bytes(&mut self) -> &mut [u8] {
        self.buf.get_mut(..*self.len).unwrap_or(&mut [])
    }
    fn grow(&mut self, len: usize) -> bool {
        // The buffer may hold anything: a grown page reads as zero.
        match self.buf.get_mut(*self.len..len) {
            Some(new) => new.fill(0),
            None => return len <= *self.len,
        }
        *self.len = len;
        true
    }
    fn cap(&self) -> usize {
        self.buf.len()
    }
}

#[cfg(feature = "alloc")]
impl Mem for Vec<u8> {
    fn bytes(&mut self) -> &mut [u8] {
        self
    }
    fn grow(&mut self, len: usize) -> bool {
        let more = len.saturating_sub(self.len());
        if self.try_reserve_exact(more).is_err() {
            return false;
        }
        if more > 0 {
            self.resize(len, 0);
        }
        true
    }
    fn cap(&self) -> usize {
        usize::MAX
    }
}

/// What an instance keeps between calls, apart from its buffers.
#[derive(Clone, Copy)]
pub(crate) struct State {
    /// Current and largest size, in pages of `1 << shift` bytes.
    pages: u32,
    max_pages: u32,
    shift: u8,
    mem_limit: usize,
    fuel: u64,
    /// How the buffers are divided: words are the table, then three per
    /// open block, then three per call frame; slots are the globals, then
    /// the value stack.
    table: usize,
    labels: usize,
    frames: usize,
    globals: usize,
    stack: usize,
}

impl State {
    fn new(x: &Index, limits: &Limits) -> Result<Self, Error> {
        let table = x.table.unwrap_or(0) as usize;
        if table > limits.table {
            return Err(Error::Limit);
        }
        Ok(State {
            pages: 0,
            max_pages: 0,
            shift: 16,
            mem_limit: limits.memory_bytes,
            fuel: limits.fuel,
            table,
            labels: limits.labels,
            frames: limits.frames,
            globals: x.globals as usize,
            stack: limits.stack,
        })
    }
}

/// One run of the interpreter over an instance's state and buffers.
struct Vm<'s, M> {
    st: &'s mut State,
    mem: &'s mut M,
    table: &'s mut [u32],
    labels: &'s mut [[u32; 3]],
    frames: &'s mut [[u32; 3]],
    globals: &'s mut [u64],
    stack: &'s mut [u64],
    sp: usize,
    fp: usize,
    lp: usize,
    fc: usize,
    flabel: usize,
}

/// Divides the buffers as `st` says. Too small is [`Error::OutOfMemory`].
fn vm<'s, M: Mem>(
    st: &'s mut State,
    mem: &'s mut M,
    words: &'s mut [u32],
    slots: &'s mut [u64],
) -> Result<Vm<'s, M>, Error> {
    let oom = Error::OutOfMemory;
    let (table, words) = words.split_at_mut_checked(st.table).ok_or(oom)?;
    let (labels, words) = words.split_at_mut_checked(3 * st.labels).ok_or(oom)?;
    let (frames, _) = words.split_at_mut_checked(3 * st.frames).ok_or(oom)?;
    let (globals, slots) = slots.split_at_mut_checked(st.globals).ok_or(oom)?;
    let (stack, _) = slots.split_at_mut_checked(st.stack).ok_or(oom)?;
    Ok(Vm {
        st,
        mem,
        table,
        labels: labels.as_chunks_mut().0,
        frames: frames.as_chunks_mut().0,
        globals,
        stack,
        sp: 0,
        fp: 0,
        lp: 0,
        fc: 0,
        flabel: 0,
    })
}

/// What to do with an instance.
enum Enter<'a> {
    /// Apply the segments and run the start function.
    Init,
    Call(u32, &'a [u64], &'a mut [u64]),
}

fn enter<M: Mem>(
    st: &mut State,
    mem: &mut M,
    (words, slots): (&mut [u32], &mut [u64]),
    v: View,
    host: &mut dyn Host,
    what: Enter,
) -> Result<(), Error> {
    let mut vm = vm(st, mem, words, slots)?;
    match what {
        Enter::Init => vm.init(v, host),
        Enter::Call(func, args, results) => vm.call(v, host, func, args, results),
    }
}

// The interpreter is generic over its memory, and `Machine` and `Instance`
// over their image. These two are neither, so the interpreter is compiled
// here, once per kind of memory, with this crate's optimisation level,
// and not again in every crate that names an image type.

fn enter_fixed(
    st: &mut State,
    mem: &mut Fixed,
    bufs: (&mut [u32], &mut [u64]),
    v: View,
    host: &mut dyn Host,
    what: Enter,
) -> Result<(), Error> {
    enter(st, mem, bufs, v, host, what)
}

#[cfg(feature = "alloc")]
fn enter_vec(
    st: &mut State,
    mem: &mut Vec<u8>,
    bufs: (&mut [u32], &mut [u64]),
    v: View,
    host: &mut dyn Host,
    what: Enter,
) -> Result<(), Error> {
    enter(st, mem, bufs, v, host, what)
}

/// A module and its instance, running in [`Buffers`] the caller keeps: no
/// allocator is involved. Pass the same buffers to every call; they hold
/// the module's index and all of its state.
///
/// ```
/// use wasm_micro::{Buffers, Error, Limits, Machine};
///
/// let wasm = wat::parse_str(r#"(module (memory 1)
///     (func (export "sq") (param i32) (result i32)
///         (i32.mul (local.get 0) (local.get 0))))"#).unwrap();
/// let limits = Limits { stack: 64, frames: 8, labels: 16, ..Limits::default() };
/// let need = wasm_micro::footprint(&wasm)?;
/// assert!(need.words(&limits) <= 128 && need.slots(&limits) <= 64);
///
/// // On a device these three would be statics.
/// let (mut words, mut slots, mut memory) = ([0u32; 128], [0u64; 64], [0u8; 4096]);
/// let mut bufs = Buffers { words: &mut words, slots: &mut slots, memory: &mut memory };
/// let mut m = Machine::new(&wasm[..], &limits, &mut bufs, &mut ())?;
/// let sq = m.export_func(&bufs, "sq").unwrap();
/// let mut out = [0];
/// m.call(&mut bufs, &mut (), sq, &[12], &mut out)?;
/// assert_eq!(out[0], 144);
/// # Ok::<(), Error>(())
/// ```
pub struct Machine<B> {
    bytes: B,
    x: Index,
    st: State,
    mem_len: usize,
}

impl<B: AsRef<[u8]>> Machine<B> {
    /// Parses `bytes` into the start of `bufs.words`, lays the instance
    /// out in the rest, applies the data and element segments and runs the
    /// start function, if any, on [`Limits::fuel`]. Buffers too small for
    /// the module under `limits` are [`Error::OutOfMemory`].
    pub fn new(
        bytes: B,
        limits: &Limits,
        bufs: &mut Buffers<'_>,
        host: &mut dyn Host,
    ) -> Result<Self, Error> {
        let b = bytes.as_ref();
        let at = crate::module::footprint(b)?.index / 4;
        let split = bufs.words.split_at_mut_checked(at);
        let (index, rest) = split.ok_or(Error::OutOfMemory)?;
        let x = crate::module::parse(b, index, rest)?;
        let mut st = State::new(&x, limits)?;
        let mut mem_len = 0;
        let mut mem = Fixed {
            buf: bufs.memory,
            len: &mut mem_len,
        };
        let v = View::new(b, &x, index);
        enter_fixed(&mut st, &mut mem, (rest, bufs.slots), v, host, Enter::Init)?;
        Ok(Machine {
            bytes,
            x,
            st,
            mem_len,
        })
    }

    fn view<'a>(&'a self, bufs: &'a Buffers<'_>) -> View<'a> {
        View::new(self.bytes.as_ref(), &self.x, bufs.words)
    }

    /// Calls function `func`. `args` and `results` must have the
    /// function's arity; values are raw slots as in [`Host::call`]. After
    /// an error the machine can be called again, with whatever the failed
    /// call left in memory and globals.
    pub fn call(
        &mut self,
        bufs: &mut Buffers<'_>,
        host: &mut dyn Host,
        func: u32,
        args: &[u64],
        results: &mut [u64],
    ) -> Result<(), Error> {
        let split = bufs.words.split_at_mut_checked(self.x.words());
        let (index, rest) = split.ok_or(Error::OutOfMemory)?;
        let v = View::new(self.bytes.as_ref(), &self.x, index);
        let mut mem = Fixed {
            buf: bufs.memory,
            len: &mut self.mem_len,
        };
        let what = Enter::Call(func, args, results);
        enter_fixed(&mut self.st, &mut mem, (rest, bufs.slots), v, host, what)
    }

    /// The index of the exported function `name`.
    pub fn export_func(&self, bufs: &Buffers<'_>, name: &str) -> Option<u32> {
        self.view(bufs).export(name, crate::ExportKind::Func)
    }

    /// The parameter and result value types of function `func`.
    pub fn func_type<'a>(
        &'a self,
        bufs: &'a Buffers<'_>,
        func: u32,
    ) -> Option<(&'a [u8], &'a [u8])> {
        self.view(bufs).func_type(func)
    }

    /// The function imports, in index order.
    pub fn imports<'a>(
        &'a self,
        bufs: &'a Buffers<'_>,
    ) -> impl Iterator<Item = crate::Import<'a>> + 'a {
        self.view(bufs).imports()
    }

    /// The exports, in section order.
    pub fn exports<'a>(
        &'a self,
        bufs: &'a Buffers<'_>,
    ) -> impl Iterator<Item = crate::Export<'a>> + 'a {
        self.view(bufs).exports()
    }

    /// The contents of the first custom section called `name`.
    pub fn custom_section(&self, name: &str) -> Option<&[u8]> {
        View::new(self.bytes.as_ref(), &self.x, &[]).custom_section(name)
    }

    /// The page size of the module's memory in bytes (65536, or 1 with
    /// custom page sizes); `None` without a memory.
    pub fn page_size(&self) -> Option<u32> {
        View::new(self.bytes.as_ref(), &self.x, &[]).page_size()
    }

    /// The module's linear memory within `bufs.memory`.
    pub fn memory<'a>(&self, bufs: &'a Buffers<'_>) -> &'a [u8] {
        bufs.memory.get(..self.mem_len).unwrap_or(&[])
    }

    pub fn memory_mut<'a>(&self, bufs: &'a mut Buffers<'_>) -> &'a mut [u8] {
        bufs.memory.get_mut(..self.mem_len).unwrap_or(&mut [])
    }

    /// Fuel left: one unit per instruction executed.
    pub fn fuel(&self) -> u64 {
        self.st.fuel
    }

    pub fn set_fuel(&mut self, fuel: u64) {
        self.st.fuel = fuel;
    }
}

/// An instantiated module that owns its storage (feature `alloc`).
#[cfg(feature = "alloc")]
pub struct Instance<B> {
    module: Module<B>,
    st: State,
    words: Vec<u32>,
    slots: Vec<u64>,
    mem: Vec<u8>,
}

macro_rules! int_ops {
    ($bin:ident, $un:ident, $u:ty, $s:ty) => {
        /// add sub mul div_s div_u rem_s rem_u and or xor shl shr_s shr_u rotl rotr
        fn $bin(k: u8, a: $u, b: $u) -> Result<$u, Error> {
            Ok(match k {
                0 => a.wrapping_add(b),
                1 => a.wrapping_sub(b),
                2 => a.wrapping_mul(b),
                3 | 5 if b == 0 => return Err(Error::DivideByZero),
                3 => (a as $s)
                    .checked_div(b as $s)
                    .ok_or(Error::IntegerOverflow)? as $u,
                4 => a.checked_div(b).ok_or(Error::DivideByZero)?,
                5 => (a as $s).wrapping_rem(b as $s) as $u,
                6 => a.checked_rem(b).ok_or(Error::DivideByZero)?,
                7 => a & b,
                8 => a | b,
                9 => a ^ b,
                10 => a.wrapping_shl(b as u32),
                11 => (a as $s).wrapping_shr(b as u32) as $u,
                12 => a.wrapping_shr(b as u32),
                13 => a.rotate_left(b as u32 % <$u>::BITS),
                _ => a.rotate_right(b as u32 % <$u>::BITS),
            })
        }
        /// clz ctz popcnt
        fn $un(k: u8, a: $u) -> u32 {
            match k {
                0 => a.leading_zeros(),
                1 => a.trailing_zeros(),
                _ => a.count_ones(),
            }
        }
    };
}
int_ops!(bin32, un32, u32, i32);
int_ops!(bin64, un64, u64, i64);

/// eq ne lt_s lt_u gt_s gt_u le_s le_u ge_s ge_u
fn icmp(k: u8, a: u64, b: u64, is32: bool) -> u64 {
    let signed = k >= 2 && k & 1 == 0;
    let ext = |v: u64| match (is32, signed) {
        (false, _) => v,
        (true, false) => v as u32 as u64,
        (true, true) => v as u32 as i32 as u64,
    };
    let (a, b) = (ext(a), ext(b));
    let ord = if signed {
        (a as i64).cmp(&(b as i64))
    } else {
        a.cmp(&b)
    };
    (match k {
        0 => ord.is_eq(),
        1 => ord.is_ne(),
        2 | 3 => ord.is_lt(),
        4 | 5 => ord.is_gt(),
        6 | 7 => ord.is_le(),
        _ => ord.is_ge(),
    }) as u64
}

#[cfg(feature = "alloc")]
impl<B: AsRef<[u8]>> Instance<B> {
    /// Allocates the instance, applies the data and element segments and
    /// runs the start function, if any, on [`Limits::fuel`].
    pub fn new(module: Module<B>, limits: &Limits, host: &mut dyn Host) -> Result<Self, Error> {
        let v = module.view();
        let mut st = State::new(v.x, limits)?;
        let run = st.table + 3 * st.labels + 3 * st.frames;
        let mut words = crate::module::zeroed(run, 0)?;
        let mut slots = crate::module::zeroed(st.globals + st.stack, 0)?;
        let mut mem = Vec::new();
        enter_vec(
            &mut st,
            &mut mem,
            (&mut words, &mut slots),
            v,
            host,
            Enter::Init,
        )?;
        Ok(Instance {
            module,
            st,
            words,
            slots,
            mem,
        })
    }

    pub fn module(&self) -> &Module<B> {
        &self.module
    }

    /// The bytes backing linear memory (see [`Limits::memory_bytes`]).
    pub fn memory(&self) -> &[u8] {
        &self.mem
    }

    pub fn memory_mut(&mut self) -> &mut [u8] {
        &mut self.mem
    }

    /// The raw value of global `index`.
    pub fn global(&self, index: u32) -> Option<u64> {
        let globals = self.slots.get(..self.st.globals)?;
        globals.get(index as usize).copied()
    }

    /// Fuel left: one unit per instruction executed.
    pub fn fuel(&self) -> u64 {
        self.st.fuel
    }

    pub fn set_fuel(&mut self, fuel: u64) {
        self.st.fuel = fuel;
    }

    /// Calls function `func` (from [`Module::export_func`]). `args` and
    /// `results` must have the function's arity; values are raw slots as
    /// in [`Host::call`]. After an error the instance can be called again,
    /// with whatever the failed call left in memory and globals.
    pub fn call(
        &mut self,
        host: &mut dyn Host,
        func: u32,
        args: &[u64],
        results: &mut [u64],
    ) -> Result<(), Error> {
        let v = self.module.view();
        let bufs = (&mut self.words[..], &mut self.slots[..]);
        let what = Enter::Call(func, args, results);
        enter_vec(&mut self.st, &mut self.mem, bufs, v, host, what)
    }
}

impl<M: Mem> Vm<'_, M> {
    fn init(&mut self, v: View, host: &mut dyn Host) -> Result<(), Error> {
        // Nothing an earlier module left in these buffers is visible.
        self.stack.fill(0);
        self.table.fill(NULL);
        self.st.mem_limit = self.st.mem_limit.min(self.mem.cap());
        if let Some(m) = v.x.memory {
            self.st.shift = m.shift;
            // Addresses are 32 bits, and the limit rounds up to whole pages.
            let limit = (self.st.mem_limit as u64).div_ceil(1 << m.shift);
            let most = (1u64 << (32 - m.shift)).min(limit);
            self.st.max_pages = (m.max as u64).min(most) as u32;
            if m.min > self.st.max_pages {
                return Err(Error::Limit);
            }
            if !self.resize(m.min) {
                return Err(Error::OutOfMemory);
            }
        }

        let mut r = v.section(6);
        for i in 0..r.count().unwrap_or(0) as usize {
            r.skip(2)?;
            let g = self.const_expr(&mut r, i)?;
            *self.globals.get_mut(i).ok_or(Error::Malformed)? = g;
        }

        // Element segments. Flag bit 0: not active; bit 1: explicit table
        // index (active) ; bit 2: entries are expressions, not indices.
        let mut r = v.section(9);
        for _ in 0..r.count().unwrap_or(0) {
            let flags = r.u32()?;
            let active = flags & 1 == 0;
            let mut at = None;
            if active {
                if flags & 2 != 0 && r.u32()? != 0 {
                    return Err(Error::Unsupported);
                }
                at = Some(self.const_expr(&mut r, usize::MAX)? as u32 as usize);
            }
            if flags & 3 != 0 && matches!(r.u8()?, 0x63 | 0x64) {
                // `(ref null? ht)`: a heap type follows.
                r.leb(true)?;
            }
            for k in 0..r.count()? as usize {
                let f = if flags & 4 != 0 {
                    self.const_expr(&mut r, usize::MAX)? as u32
                } else {
                    r.u32()?
                };
                if let Some(at) = at {
                    *at.checked_add(k)
                        .and_then(|x| self.table.get_mut(x))
                        .ok_or(Error::SegmentOutOfBounds)? = f;
                }
            }
            // An active segment with no entries still checks its offset.
            if at.is_some_and(|at| at > self.table.len()) {
                return Err(Error::SegmentOutOfBounds);
            }
        }

        let mut r = v.section(11);
        for _ in 0..r.count().unwrap_or(0) {
            let flags = r.u32()?;
            if flags == 2 && r.u32()? != 0 {
                return Err(Error::Unsupported);
            }
            if flags & 1 != 0 {
                r.name()?;
                continue;
            }
            let at = self.const_expr(&mut r, usize::MAX)? as u32 as usize;
            let data = r.name()?;
            self.mem
                .bytes()
                .get_mut(at..)
                .and_then(|m| m.get_mut(..data.len()))
                .ok_or(Error::SegmentOutOfBounds)?
                .copy_from_slice(data);
        }

        if let Some(start) = v.x.start {
            self.call(v, host, start, &[], &mut [])?;
        }
        Ok(())
    }

    /// A global initialiser or segment offset. It may read the first
    /// `defined` globals: those initialised so far.
    fn const_expr(&self, r: &mut Reader, defined: usize) -> Result<u64, Error> {
        let v = match r.u8()? {
            0x41 => r.leb(true)? as u32 as u64,
            0x42 => r.leb(true)?,
            0x43 => r.le(4)?,
            0x44 => r.le(8)?,
            0x23 => {
                let i = r.u32()? as usize;
                let g = self.globals.get(i).filter(|_| i < defined);
                *g.ok_or(Error::Invalid)?
            }
            0xD2 => r.u32()? as u64,
            0xD0 => {
                r.u8()?;
                NULL as u64
            }
            _ => return Err(Error::Unsupported),
        };
        if r.u8()? != 0x0B {
            return Err(Error::Unsupported);
        }
        Ok(v)
    }

    /// Sets the memory to `pages`, backed up to the byte limit.
    fn resize(&mut self, pages: u32) -> bool {
        let bytes = usize::try_from((pages as u64) << self.st.shift).unwrap_or(usize::MAX);
        if !self.mem.grow(bytes.min(self.st.mem_limit)) {
            return false;
        }
        self.st.pages = pages;
        true
    }

    fn call(
        &mut self,
        v: View,
        host: &mut dyn Host,
        func: u32,
        args: &[u64],
        results: &mut [u64],
    ) -> Result<(), Error> {
        let (p, q) = v.func_type(func).ok_or(Error::Invalid)?;
        if p.len() != args.len() || q.len() != results.len() {
            return Err(Error::Invalid);
        }
        (self.sp, self.fp, self.lp, self.fc, self.flabel) = (0, 0, 0, 0, 0);
        for a in args {
            self.push(*a)?;
        }
        let mut r = Reader { b: v.b, pos: 0 };
        self.invoke(v, host, func, &mut r)?;
        if self.fc > 0 {
            self.run(v, host, r)?;
        }
        for (d, s) in results.iter_mut().zip(self.stack.iter()) {
            *d = *s;
        }
        Ok(())
    }

    fn label(&self, li: usize) -> Result<Label, Error> {
        let [pc, sp, meta] = *self.labels.get(li).ok_or(Error::Invalid)?;
        Ok(Label {
            pc,
            sp,
            arity: meta as u16,
            kind: (meta >> 16) as u8,
        })
    }

    fn pop(&mut self) -> Result<u64, Error> {
        self.sp = self.sp.checked_sub(1).ok_or(Error::Invalid)?;
        self.stack.get(self.sp).copied().ok_or(Error::Invalid)
    }

    fn push(&mut self, v: u64) -> Result<(), Error> {
        *self.stack.get_mut(self.sp).ok_or(Error::StackOverflow)? = v;
        self.sp += 1;
        Ok(())
    }

    fn top(&mut self) -> Result<&mut u64, Error> {
        self.sp
            .checked_sub(1)
            .and_then(|i| self.stack.get_mut(i))
            .ok_or(Error::Invalid)
    }

    fn local(&mut self, r: &mut Reader) -> Result<&mut u64, Error> {
        let i = self.fp.wrapping_add(r.u32()? as usize);
        self.stack.get_mut(i).ok_or(Error::Invalid)
    }

    fn push_label(&mut self, pc: usize, below: usize, arity: usize, kind: u8) -> Result<(), Error> {
        let sp = self.sp.checked_sub(below).ok_or(Error::Invalid)?;
        let arity = u16::try_from(arity).map_err(|_| Error::Unsupported)?;
        *self.labels.get_mut(self.lp).ok_or(Error::StackOverflow)? = [
            pc as u32,
            sp as u32,
            u32::from(arity) | u32::from(kind) << 16,
        ];
        self.lp += 1;
        Ok(())
    }

    /// `(params, results)` of a block type.
    fn blocktype(&self, v: View, r: &mut Reader) -> Result<(usize, usize), Error> {
        Ok(match r.blocktype()? {
            None => (0, 0),
            Some(u32::MAX) => (0, 1),
            Some(ty) => {
                let (p, q) = v.sig(ty)?;
                (p.len(), q.len())
            }
        })
    }

    /// Calls `f`: an import runs to completion, a module function gets a
    /// frame and `r` moves to its first instruction.
    fn invoke(
        &mut self,
        v: View,
        host: &mut dyn Host,
        f: u32,
        r: &mut Reader,
    ) -> Result<(), Error> {
        let (ty, code) = v.func(f).ok_or(Error::Invalid)?;
        let (p, q) = v.sig(ty)?;
        let base = self.sp.checked_sub(p.len()).ok_or(Error::Invalid)?;
        if f < v.x.n_imports {
            let vals = self
                .stack
                .get_mut(base..base + p.len().max(q.len()))
                .ok_or(Error::StackOverflow)?;
            host.call(f, self.mem.bytes(), vals)?;
            self.sp = base + q.len();
            return Ok(());
        }
        let mut body = Reader { b: r.b, pos: code };
        for _ in 0..body.u32()? {
            let n = body.u32()? as usize;
            body.u8()?;
            let end = self.sp.checked_add(n).ok_or(Error::StackOverflow)?;
            self.stack
                .get_mut(self.sp..end)
                .ok_or(Error::StackOverflow)?
                .fill(0);
            self.sp = end;
        }
        *self.frames.get_mut(self.fc).ok_or(Error::StackOverflow)? =
            [r.pos as u32, self.fp as u32, self.flabel as u32];
        self.fc += 1;
        self.fp = base;
        self.flabel = self.lp;
        self.push_label(0, self.sp - base, q.len(), FUNC)?;
        r.pos = body.pos;
        Ok(())
    }

    /// Moves the top `arity` values down to height `to`.
    fn unwind(&mut self, to: usize, arity: usize) -> Result<(), Error> {
        let from = self.sp.checked_sub(arity).ok_or(Error::Invalid)?;
        if to > from {
            return Err(Error::Invalid);
        }
        self.stack.copy_within(from..self.sp, to);
        self.sp = to + arity;
        Ok(())
    }

    /// Returns through function label `li`; true when the outermost call
    /// has returned.
    fn ret(&mut self, li: usize, r: &mut Reader) -> Result<bool, Error> {
        let l = self.label(li)?;
        self.unwind(l.sp as usize, l.arity as usize)?;
        self.lp = li;
        self.fc = self.fc.checked_sub(1).ok_or(Error::Invalid)?;
        let [ret, fp, flabel] = *self.frames.get(self.fc).ok_or(Error::Invalid)?;
        r.pos = ret as usize;
        self.fp = fp as usize;
        self.flabel = flabel as usize;
        Ok(self.fc == 0)
    }

    /// Branches to the label `depth` blocks out; true as for [`Self::ret`].
    fn branch(&mut self, v: View, depth: u32, r: &mut Reader) -> Result<bool, Error> {
        let li = self
            .lp
            .checked_sub(1)
            .and_then(|t| t.checked_sub(depth as usize))
            .filter(|li| *li >= self.flabel)
            .ok_or(Error::Invalid)?;
        let l = self.label(li)?;
        if l.kind == FUNC {
            return self.ret(li, r);
        }
        self.unwind(l.sp as usize, l.arity as usize)?;
        if l.kind == LOOP {
            self.lp = li + 1;
            r.pos = l.pc as usize;
        } else {
            self.lp = li;
            r.pos = v.target(l.pc as usize)?;
        }
        Ok(false)
    }

    /// The bytes an access of `n` at `addr + offset` covers.
    fn span(&mut self, r: &mut Reader, addr: u64, n: usize) -> Result<&mut [u8], Error> {
        r.u32()?;
        let at = (addr as u32 as u64) + r.u32()? as u64;
        usize::try_from(at)
            .ok()
            .and_then(|at| self.mem.bytes().get_mut(at..))
            .and_then(|m| m.get_mut(..n))
            .ok_or(Error::MemoryOutOfBounds)
    }

    fn run(&mut self, v: View, host: &mut dyn Host, mut r: Reader) -> Result<(), Error> {
        loop {
            self.st.fuel = self.st.fuel.checked_sub(1).ok_or(Error::OutOfFuel)?;
            let pc = r.pos;
            let op = r.u8()?;
            match op {
                0x00 => return Err(Error::Unreachable),
                0x01 | 0xBC..=0xBF => {} // nop; reinterpret: slots are bits
                0x02 => {
                    let (p, q) = self.blocktype(v, &mut r)?;
                    self.push_label(pc, p, q, BLOCK)?;
                }
                0x03 => {
                    let (p, _) = self.blocktype(v, &mut r)?;
                    self.push_label(r.pos, p, p, LOOP)?;
                }
                0x04 => {
                    let (p, q) = self.blocktype(v, &mut r)?;
                    let taken = self.pop()? as u32 != 0;
                    if !taken {
                        r.pos = v.target(pc + 1)?;
                    }
                    // Not taken and no `else`: the block is skipped whole.
                    if taken || r.b.get(r.pos.wrapping_sub(1)) == Some(&0x05) {
                        self.push_label(pc, p, q, BLOCK)?;
                    }
                }
                0x05 => {
                    r.pos = v.target(pc)?;
                    self.lp = self.lp.checked_sub(1).ok_or(Error::Invalid)?;
                }
                0x0B => {
                    let li = self.lp.checked_sub(1).ok_or(Error::Invalid)?;
                    if li > self.flabel {
                        self.lp = li;
                    } else if self.ret(li, &mut r)? {
                        return Ok(());
                    }
                }
                0x0C..=0x0E => {
                    let mut depth = r.u32()?;
                    if op == 0x0E {
                        // `depth` was the table length; pick the entry.
                        let i = (self.pop()? as u32).min(depth);
                        for _ in 0..=i {
                            depth = r.u32()?;
                        }
                    }
                    if (op != 0x0D || self.pop()? as u32 != 0) && self.branch(v, depth, &mut r)? {
                        return Ok(());
                    }
                }
                0x0F => {
                    if self.ret(self.flabel, &mut r)? {
                        return Ok(());
                    }
                }
                0x10 => {
                    let f = r.u32()?;
                    self.invoke(v, host, f, &mut r)?;
                }
                0x11 => {
                    let ty = r.u32()?;
                    r.u32()?;
                    let i = self.pop()? as u32 as usize;
                    let f = *self.table.get(i).ok_or(Error::UndefinedElement)?;
                    let (fty, _) = v.func(f).ok_or(Error::UninitializedElement)?;
                    if fty != ty && v.sig(fty)? != v.sig(ty)? {
                        return Err(Error::SignatureMismatch);
                    }
                    self.invoke(v, host, f, &mut r)?;
                }
                0x1A => {
                    self.pop()?;
                }
                0x1B | 0x1C => {
                    if op == 0x1C {
                        let n = r.u32()? as usize;
                        r.skip(n)?;
                    }
                    let c = self.pop()? as u32;
                    let b = self.pop()?;
                    if c == 0 {
                        *self.top()? = b;
                    }
                }
                0x20 => {
                    let v = *self.local(&mut r)?;
                    self.push(v)?;
                }
                0x21 => {
                    let v = self.pop()?;
                    *self.local(&mut r)? = v;
                }
                0x22 => {
                    let v = *self.top()?;
                    *self.local(&mut r)? = v;
                }
                0x23 => {
                    let v = *self.globals.get(r.u32()? as usize).ok_or(Error::Invalid)?;
                    self.push(v)?;
                }
                0x24 => {
                    let v = self.pop()?;
                    *self
                        .globals
                        .get_mut(r.u32()? as usize)
                        .ok_or(Error::Invalid)? = v;
                }
                0x28..=0x35 => {
                    // Size; 0x10: sign-extend; 0x20: and keep 32 bits.
                    const LOAD: [u8; 14] =
                        [4, 8, 4, 8, 0x31, 1, 0x32, 2, 0x11, 1, 0x12, 2, 0x14, 4];
                    let how = LOAD.get((op - 0x28) as usize).copied().unwrap_or(0);
                    let n = (how & 15) as usize;
                    let a = *self.top()?;
                    let mut v = 0u64;
                    for (i, b) in self.span(&mut r, a, n)?.iter().enumerate() {
                        v |= u64::from(*b) << (8 * (i & 7));
                    }
                    if how & 0x10 != 0 {
                        let sh = 64 - 8 * (n as u32);
                        v = ((v << sh) as i64 >> sh) as u64;
                    }
                    if how & 0x20 != 0 {
                        v = v as u32 as u64;
                    }
                    *self.top()? = v;
                }
                0x36..=0x3E => {
                    const STORE: [u8; 9] = [4, 8, 4, 8, 1, 2, 1, 2, 4];
                    let n = STORE.get((op - 0x36) as usize).copied().unwrap_or(0) as usize;
                    let v = self.pop()?;
                    let a = self.pop()?;
                    for (d, s) in self.span(&mut r, a, n)?.iter_mut().zip(v.to_le_bytes()) {
                        *d = s;
                    }
                }
                0x3F => {
                    r.u8()?;
                    self.push(self.st.pages as u64)?;
                }
                0x40 => {
                    r.u8()?;
                    let old = self.st.pages;
                    let t = *self.top()? as u32;
                    let fits = old.checked_add(t).filter(|n| *n <= self.st.max_pages);
                    let ok = fits.is_some_and(|n| self.resize(n));
                    *self.top()? = if ok { old } else { u32::MAX } as u64;
                }
                0x41 => {
                    let v = r.leb(true)? as u32 as u64;
                    self.push(v)?;
                }
                0x42 => {
                    let v = r.leb(true)?;
                    self.push(v)?;
                }
                0x43 | 0x44 => {
                    let v = r.le(if op == 0x43 { 4 } else { 8 })?;
                    self.push(v)?;
                }
                0x45 | 0x50 | 0x67..=0x69 | 0x79..=0x7B | 0xA7 | 0xAC | 0xAD | 0xC0..=0xC4 => {
                    let t = self.top()?;
                    let v = *t;
                    *t = match op {
                        0x45 => (v as u32 == 0) as u64,
                        0x50 => (v == 0) as u64,
                        0x67..=0x69 => un32(op - 0x67, v as u32) as u64,
                        0x79..=0x7B => un64(op - 0x79, v) as u64,
                        0xA7 | 0xAD => v as u32 as u64,
                        0xAC | 0xC4 => v as i32 as u64,
                        0xC0 => v as i8 as u32 as u64,
                        0xC1 => v as i16 as u32 as u64,
                        0xC2 => v as i8 as u64,
                        _ => v as i16 as u64,
                    };
                }
                0x46..=0x4F | 0x51..=0x5A | 0x6A..=0x78 | 0x7C..=0x8A => {
                    let b = self.pop()?;
                    let t = self.top()?;
                    let a = *t;
                    *t = match op {
                        0x46..=0x4F => icmp(op - 0x46, a, b, true),
                        0x51..=0x5A => icmp(op - 0x51, a, b, false),
                        0x6A..=0x78 => bin32(op - 0x6A, a as u32, b as u32)? as u64,
                        _ => bin64(op - 0x7C, a, b)?,
                    };
                }
                #[cfg(feature = "float")]
                0x5B..=0x66 | 0x92..=0x98 | 0xA0..=0xA6 => {
                    let b = self.pop()?;
                    let t = self.top()?;
                    *t = crate::float::binary(op, *t, b);
                }
                #[cfg(feature = "float")]
                0x8B..=0x91 | 0x99..=0x9F | 0xA8..=0xAB | 0xAE..=0xBB => {
                    let t = self.top()?;
                    *t = crate::float::unary(op, *t)?;
                }
                0xFC => match r.u32()? {
                    #[cfg(feature = "float")]
                    k @ 0..=7 => {
                        let t = self.top()?;
                        *t = crate::float::trunc(k as u8, *t, true)?;
                    }
                    k @ (10 | 11) => {
                        r.skip(12 - k as usize)?;
                        let n = self.pop()? as u32 as usize;
                        let s = self.pop()?;
                        let d = self.pop()? as u32 as usize;
                        let mem = self.mem.bytes();
                        let end = d.checked_add(n).filter(|e| *e <= mem.len());
                        let end = end.ok_or(Error::MemoryOutOfBounds)?;
                        if k == 11 {
                            mem.get_mut(d..end)
                                .ok_or(Error::MemoryOutOfBounds)?
                                .fill(s as u8);
                        } else {
                            let s = s as u32 as usize;
                            if s.checked_add(n).is_none_or(|e| e > mem.len()) {
                                return Err(Error::MemoryOutOfBounds);
                            }
                            mem.copy_within(s..s + n, d);
                        }
                    }
                    _ => return Err(Error::Unsupported),
                },
                _ => return Err(Error::Unsupported),
            }
        }
    }
}
