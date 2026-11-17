//! The interpreter: one internal instruction per step, with the value
//! stack, the call-frame stack and linear memory as RAMs.
//!
//! Per step: check fuel, fetch `code[pc]`, decode `op`/`a`, execute. A
//! frame is `[params][locals][operands]` from `fp`; a call pushes `(return
//! pc, caller fp)` on the frame stack, sets `fp = sp - params` and zeroes
//! the callee's locals; `RET` keeps its top `keep` slots, cuts the stack
//! back to `fp` and pops the frame. Every memory access is bounds-checked
//! against the current memory length, every stack access against the
//! frame and the stack size, so no code word can reach outside the core.

use middleware::wasm_abi::IMPORTS;

use crate::isa::{self, bin, load, store, un};
use crate::{CALL_DEPTH, Core, Frame, Func, Host, STACK_SLOTS, Trap};

type T<V> = Result<V, Trap>;

/// Return address of the export call's frame: reaching it ends the call.
const TOP: u16 = isa::NONE;

impl<const MEM: usize> Core<MEM> {
    fn spush(&mut self, sp: &mut usize, v: i32) -> T<()> {
        *self.stack.get_mut(*sp).ok_or(Trap::StackOverflow)? = v;
        *sp += 1;
        Ok(())
    }

    fn spop(&self, sp: &mut usize, floor: usize) -> T<i32> {
        if *sp <= floor {
            return Err(Trap::BadCode);
        }
        *sp -= 1;
        self.stack.get(*sp).copied().ok_or(Trap::BadCode)
    }

    fn ea(&self, addr: i32, off: u32, size: usize) -> T<usize> {
        let ea = (addr as u32 as usize)
            .checked_add(off as usize)
            .ok_or(Trap::MemoryOutOfBounds)?;
        let end = ea.checked_add(size).ok_or(Trap::MemoryOutOfBounds)?;
        if end > self.mem_len {
            return Err(Trap::MemoryOutOfBounds);
        }
        Ok(ea)
    }

    fn mem_bytes<const N: usize>(&self, at: usize) -> T<[u8; N]> {
        let mut b = [0u8; N];
        b.copy_from_slice(self.mem.get(at..at + N).ok_or(Trap::MemoryOutOfBounds)?);
        Ok(b)
    }

    fn put(&mut self, at: usize, b: &[u8]) -> T<()> {
        self.mem
            .get_mut(at..at + b.len())
            .ok_or(Trap::MemoryOutOfBounds)?
            .copy_from_slice(b);
        Ok(())
    }

    fn range(&self, at: i32, n: i32) -> T<usize> {
        let at = at as u32 as usize;
        let end = at
            .checked_add(n as u32 as usize)
            .ok_or(Trap::MemoryOutOfBounds)?;
        if end > self.mem_len {
            return Err(Trap::MemoryOutOfBounds);
        }
        Ok(at)
    }

    /// Calls function `idx` of the module with `args` and runs it to its
    /// return with a fresh fuel budget. A trap poisons the payload.
    pub(crate) fn invoke<H: Host>(
        &mut self,
        idx: u16,
        args: &[i32],
        host: &mut H,
    ) -> T<Option<i32>> {
        if !self.loaded {
            return Err(Trap::NotLoaded);
        }
        if self.poisoned {
            return Err(Trap::Poisoned);
        }
        let r = self.run(idx, args, host);
        if r.is_err() {
            self.poisoned = true;
        }
        r
    }

    fn func(&self, idx: usize) -> T<Func> {
        if idx >= self.nfuncs {
            return Err(Trap::BadCode);
        }
        self.funcs.get(idx).copied().ok_or(Trap::BadCode)
    }

    /// Enters local function `f`: pushes the frame, zeroes its locals.
    fn enter(
        &mut self,
        f: Func,
        sp: &mut usize,
        fp: &mut usize,
        depth: &mut usize,
        ret: u16,
    ) -> T<usize> {
        if *depth >= CALL_DEPTH {
            return Err(Trap::StackOverflow);
        }
        let caller_fp = u16::try_from(*fp).map_err(|_| Trap::StackOverflow)?;
        *self.frames.get_mut(*depth).ok_or(Trap::StackOverflow)? = Frame { ret, fp: caller_fp };
        *depth += 1;
        *fp = sp.checked_sub(usize::from(f.params)).ok_or(Trap::BadCode)?;
        for _ in 0..f.locals {
            self.spush(sp, 0)?;
        }
        if f.entry == isa::NONE {
            return Err(Trap::BadCode);
        }
        Ok(usize::from(f.entry))
    }

    fn run<H: Host>(&mut self, idx: u16, args: &[i32], host: &mut H) -> T<Option<i32>> {
        let f = self.func(usize::from(idx))?;
        if f.import != Func::LOCAL || usize::from(f.params) != args.len() {
            return Err(Trap::BadCode);
        }
        let (mut sp, mut fp, mut depth) = (0usize, 0usize, 0usize);
        for &a in args {
            self.spush(&mut sp, a)?;
        }
        let mut pc = self.enter(f, &mut sp, &mut fp, &mut depth, TOP)?;
        let mut fuel = self.fuel_per_call;
        loop {
            if fuel == 0 {
                return Err(Trap::OutOfFuel);
            }
            fuel -= 1;
            self.executed = self.executed.saturating_add(1);
            let w = *self
                .code
                .get(pc)
                .filter(|_| pc < self.code_len)
                .ok_or(Trap::BadCode)?;
            pc += 1;
            let a = isa::arg(w);
            // Locals and operands of this frame live at `fp..sp`.
            let floor = fp;
            match isa::op(w) {
                isa::UNREACHABLE => return Err(Trap::Unreachable),
                isa::CONST => self.spush(&mut sp, isa::sext24(a))?,
                isa::CONST32 => {
                    let v = *self.code.get(pc).ok_or(Trap::BadCode)?;
                    pc += 1;
                    self.spush(&mut sp, v as i32)?;
                }
                isa::DROP => {
                    for _ in 0..a {
                        self.spop(&mut sp, floor)?;
                    }
                }
                isa::SELECT => {
                    let c = self.spop(&mut sp, floor)?;
                    let b = self.spop(&mut sp, floor)?;
                    let x = self.spop(&mut sp, floor)?;
                    self.spush(&mut sp, if c != 0 { x } else { b })?;
                }
                isa::LGET => {
                    let v = *self.stack.get(fp + a as usize).ok_or(Trap::BadCode)?;
                    self.spush(&mut sp, v)?;
                }
                isa::LSET | isa::LTEE => {
                    let v = self.spop(&mut sp, floor)?;
                    *self.stack.get_mut(fp + a as usize).ok_or(Trap::BadCode)? = v;
                    if isa::op(w) == isa::LTEE {
                        self.spush(&mut sp, v)?;
                    }
                }
                isa::GGET => {
                    let v = *self.globals.get(a as usize).ok_or(Trap::BadCode)?;
                    self.spush(&mut sp, v)?;
                }
                isa::GSET => {
                    let v = self.spop(&mut sp, floor)?;
                    *self.globals.get_mut(a as usize).ok_or(Trap::BadCode)? = v;
                }
                isa::LOAD => {
                    let off = *self.code.get(pc).ok_or(Trap::BadCode)?;
                    pc += 1;
                    let addr = self.spop(&mut sp, floor)?;
                    let v = match a {
                        load::W32 => i32::from_le_bytes(self.mem_bytes(self.ea(addr, off, 4)?)?),
                        load::S8 => {
                            i32::from(self.mem_bytes::<1>(self.ea(addr, off, 1)?)?[0] as i8)
                        }
                        load::U8 => i32::from(self.mem_bytes::<1>(self.ea(addr, off, 1)?)?[0]),
                        load::S16 => {
                            i32::from(i16::from_le_bytes(self.mem_bytes(self.ea(addr, off, 2)?)?))
                        }
                        load::U16 => {
                            i32::from(u16::from_le_bytes(self.mem_bytes(self.ea(addr, off, 2)?)?))
                        }
                        _ => return Err(Trap::BadCode),
                    };
                    self.spush(&mut sp, v)?;
                }
                isa::STORE => {
                    let off = *self.code.get(pc).ok_or(Trap::BadCode)?;
                    pc += 1;
                    match a {
                        store::W64 => {
                            let hi = self.spop(&mut sp, floor)?;
                            let lo = self.spop(&mut sp, floor)?;
                            let addr = self.spop(&mut sp, floor)?;
                            let at = self.ea(addr, off, 8)?;
                            self.put(at, &lo.to_le_bytes())?;
                            self.put(at + 4, &hi.to_le_bytes())?;
                        }
                        _ => {
                            let v = self.spop(&mut sp, floor)?;
                            let addr = self.spop(&mut sp, floor)?;
                            let b = v.to_le_bytes();
                            let n = match a {
                                store::W32 => 4,
                                store::W16 => 2,
                                store::W8 => 1,
                                _ => return Err(Trap::BadCode),
                            };
                            let at = self.ea(addr, off, n)?;
                            self.put(at, b.get(..n).ok_or(Trap::BadCode)?)?;
                        }
                    }
                }
                isa::MSIZE => {
                    let pages = (self.mem_len >> self.page_log2) as i32;
                    self.spush(&mut sp, pages)?;
                }
                isa::MGROW => {
                    let n = self.spop(&mut sp, floor)? as u32 as usize;
                    let old = (self.mem_len >> self.page_log2) as i32;
                    let grown = n
                        .checked_shl(self.page_log2)
                        .filter(|b| b >> self.page_log2 == n)
                        .and_then(|b| self.mem_len.checked_add(b))
                        .filter(|&len| len <= self.mem_max);
                    match grown {
                        Some(len) => {
                            self.mem_len = len;
                            self.spush(&mut sp, old)?;
                        }
                        None => self.spush(&mut sp, -1)?,
                    }
                }
                isa::MCOPY => {
                    let n = self.spop(&mut sp, floor)?;
                    let src = self.spop(&mut sp, floor)?;
                    let dst = self.spop(&mut sp, floor)?;
                    let s = self.range(src, n)?;
                    let d = self.range(dst, n)?;
                    let n = n as u32 as usize;
                    self.mem.copy_within(s..s + n, d);
                }
                isa::MFILL => {
                    let n = self.spop(&mut sp, floor)?;
                    let v = self.spop(&mut sp, floor)?;
                    let dst = self.spop(&mut sp, floor)?;
                    let d = self.range(dst, n)?;
                    let n = n as u32 as usize;
                    self.mem
                        .get_mut(d..d + n)
                        .ok_or(Trap::MemoryOutOfBounds)?
                        .fill(v as u8);
                }
                isa::UN => {
                    let x = self.spop(&mut sp, floor)?;
                    let r = match a {
                        un::EQZ => i32::from(x == 0),
                        un::CLZ => x.leading_zeros() as i32,
                        un::CTZ => x.trailing_zeros() as i32,
                        un::POPCNT => x.count_ones() as i32,
                        un::EXT8 => i32::from(x as i8),
                        un::EXT16 => i32::from(x as i16),
                        _ => return Err(Trap::BadCode),
                    };
                    self.spush(&mut sp, r)?;
                }
                isa::BIN => {
                    let y = self.spop(&mut sp, floor)?;
                    let x = self.spop(&mut sp, floor)?;
                    self.spush(&mut sp, binop(a, x, y)?)?;
                }
                isa::BR => pc = self.branch(a, &mut sp, floor)?,
                isa::BRIF => {
                    if self.spop(&mut sp, floor)? != 0 {
                        pc = self.branch(a, &mut sp, floor)?;
                    }
                }
                isa::BRUNLESS => {
                    if self.spop(&mut sp, floor)? == 0 {
                        pc = (a & 0xFFFF) as usize;
                    }
                }
                isa::BRTABLE => {
                    let i = self.spop(&mut sp, floor)? as u32;
                    let pick = if i < a { i } else { a };
                    let bw = *self.code.get(pc + pick as usize).ok_or(Trap::BadCode)?;
                    if isa::op(bw) != isa::BR {
                        return Err(Trap::BadCode);
                    }
                    pc = self.branch(isa::arg(bw), &mut sp, floor)?;
                }
                isa::CALL => {
                    let f = self.func(a as usize)?;
                    if f.import != Func::LOCAL {
                        return Err(Trap::BadCode);
                    }
                    let ret = u16::try_from(pc).map_err(|_| Trap::BadCode)?;
                    pc = self.enter(f, &mut sp, &mut fp, &mut depth, ret)?;
                }
                isa::CALLI => {
                    let i = self.spop(&mut sp, floor)? as u32 as usize;
                    if i >= self.table_len {
                        return Err(Trap::TableOutOfBounds);
                    }
                    let fi = *self.table.get(i).ok_or(Trap::TableOutOfBounds)?;
                    if fi == isa::NONE {
                        return Err(Trap::IndirectNull);
                    }
                    let f = self.func(usize::from(fi))?;
                    let (p, r) = ((a >> 8) & 0xFF, a & 0xFF);
                    if (u32::from(f.params), u32::from(f.results)) != (p, r) {
                        return Err(Trap::IndirectType);
                    }
                    if f.import == Func::LOCAL {
                        let ret = u16::try_from(pc).map_err(|_| Trap::BadCode)?;
                        pc = self.enter(f, &mut sp, &mut fp, &mut depth, ret)?;
                    } else {
                        self.host_op(u32::from(f.import), &mut sp, floor, host)?;
                    }
                }
                isa::HOST => self.host_op(a, &mut sp, floor, host)?,
                isa::RET => {
                    let v = if a == 1 {
                        Some(self.spop(&mut sp, floor)?)
                    } else {
                        None
                    };
                    sp = fp;
                    if let Some(v) = v {
                        self.spush(&mut sp, v)?;
                    }
                    depth = depth.checked_sub(1).ok_or(Trap::BadCode)?;
                    let fr = *self.frames.get(depth).ok_or(Trap::BadCode)?;
                    if fr.ret == TOP {
                        return Ok(v);
                    }
                    pc = usize::from(fr.ret);
                    fp = usize::from(fr.fp);
                }
                _ => return Err(Trap::BadCode),
            }
            if sp > STACK_SLOTS {
                return Err(Trap::StackOverflow);
            }
        }
    }

    /// A taken branch: keep, drop, jump.
    fn branch(&mut self, a: u32, sp: &mut usize, floor: usize) -> T<usize> {
        let (target, keep, drop) = isa::unbranch(a);
        let kept = if keep == 1 {
            Some(self.spop(sp, floor)?)
        } else {
            None
        };
        *sp = sp
            .checked_sub(drop as usize)
            .filter(|&s| s >= floor)
            .ok_or(Trap::BadCode)?;
        if let Some(v) = kept {
            self.spush(sp, v)?;
        }
        Ok(usize::from(target))
    }

    /// Pops the import's arguments, runs it in the import unit, pushes its
    /// result.
    fn host_op<H: Host>(&mut self, idx: u32, sp: &mut usize, floor: usize, host: &mut H) -> T<()> {
        let imp = IMPORTS.get(idx as usize).ok_or(Trap::BadCode)?;
        let mut args = [0i32; 8];
        let n = imp.params;
        if n > args.len() {
            return Err(Trap::BadCode);
        }
        for i in (0..n).rev() {
            *args.get_mut(i).ok_or(Trap::BadCode)? = self.spop(sp, floor)?;
        }
        let r = self.host_call(imp.name, args.get(..n).ok_or(Trap::BadCode)?, host);
        if imp.returns {
            self.spush(sp, r)?;
        }
        Ok(())
    }
}

fn binop(o: u32, x: i32, y: i32) -> T<i32> {
    let (ux, uy) = (x as u32, y as u32);
    Ok(match o {
        0 => i32::from(x == y),
        1 => i32::from(x != y),
        2 => i32::from(x < y),
        3 => i32::from(ux < uy),
        4 => i32::from(x > y),
        5 => i32::from(ux > uy),
        6 => i32::from(x <= y),
        7 => i32::from(ux <= uy),
        8 => i32::from(x >= y),
        9 => i32::from(ux >= uy),
        10 => x.wrapping_add(y),
        11 => x.wrapping_sub(y),
        12 => x.wrapping_mul(y),
        13 => {
            if y == 0 {
                return Err(Trap::DivideByZero);
            }
            x.checked_div(y).ok_or(Trap::IntegerOverflow)?
        }
        14 => ux.checked_div(uy).ok_or(Trap::DivideByZero)? as i32,
        15 => {
            if y == 0 {
                return Err(Trap::DivideByZero);
            }
            x.wrapping_rem(y)
        }
        16 => ux.checked_rem(uy).ok_or(Trap::DivideByZero)? as i32,
        17 => x & y,
        18 => x | y,
        19 => x ^ y,
        20 => x.wrapping_shl(uy),
        21 => x.wrapping_shr(uy),
        22 => ux.wrapping_shr(uy) as i32,
        23 => ux.rotate_left(uy % 32) as i32,
        24 => ux.rotate_right(uy % 32) as i32,
        _ => return Err(Trap::BadCode),
    })
}

const _: () = assert!(bin::LAST == 24);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cl3_division_edges() {
        assert_eq!(binop(13, i32::MIN, -1), Err(Trap::IntegerOverflow));
        assert_eq!(binop(13, 1, 0), Err(Trap::DivideByZero));
        assert_eq!(binop(15, i32::MIN, -1), Ok(0));
        assert_eq!(binop(16, 7, 0), Err(Trap::DivideByZero));
        assert_eq!(binop(14, -1, 2), Ok(i32::MAX));
        assert_eq!(binop(21, -8, 33), Ok(-4));
        assert_eq!(binop(23, 0x8000_0001u32 as i32, 1), Ok(3));
    }
}
