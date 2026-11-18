//! The installable image: what the host uploads to the 5A-75B instead of
//! the `.wasm` (CL6). The host compiles the module with [`Core::load`] and
//! writes the result with [`Core::emit_image`]; the gateware's installer
//! (`gateware-5a75b/mw5a75b/installer.py`) streams it into the core's
//! RAMs as [`Core::install`] does here, with the same checks in the same
//! order, so a refused image fails with the same [`LoadError`] kind.
//!
//! Every multi-byte field is little-endian. The header ([`HEADER`] bytes):
//!
//! | offset | size | field |
//! |-------:|-----:|-------|
//! | 0  | 4  | magic `MWI\x01` ([`MAGIC`]; the last byte is the format version) |
//! | 4  | 2  | code words (at most [`CODE_WORDS`]) |
//! | 6  | 1  | functions, imports included (at most [`MAX_FUNCS`]) |
//! | 7  | 1  | entries of table 0 (at most [`MAX_TABLE`]) |
//! | 8  | 1  | globals (at most [`MAX_GLOBALS`]) |
//! | 9  | 1  | `page_log2`: 0 (1-byte pages) or 16 |
//! | 10 | 2  | data segments |
//! | 12 | 2  | initial memory length in bytes (at most `MEM`) |
//! | 14 | 2  | maximum memory length in bytes (`mem_len..=MEM`) |
//! | 16 | 12 | exports `init`, `tick`, `mw_alloc`, `on_message`, `on_irq`, `on_request`: a function index each, `0xFFFF` for none |
//!
//! then, back to back:
//!
//! * the code words, 4 bytes each;
//! * the function table, 7 bytes each: import (`0xFF` local), entry (u16),
//!   params, results, locals (u16) — the gateware's 56-bit `funcs` word;
//! * table 0, 2 bytes each (`0xFFFF` empty);
//! * the globals, 4 bytes each;
//! * the data segments: offset (u16), length (u16), the bytes. Memory
//!   outside them is zero.
//!
//! and nothing else: the image ends exactly after the last segment.
//!
//! What the installer checks (anything else is the host's compiler's
//! business): reading past the image's end, the magic, each count against
//! its table, the page size, the memory lengths, every export and table 0
//! entry naming a function, every segment inside the initial memory, no
//! trailing bytes. So a bad image cannot write outside the core's RAMs or
//! configure the core outside its limits. Code words and function entries
//! are not validated: the core traps a word or an entry it cannot run
//! ([`crate::Trap::BadCode`], stack and memory bounds), as it would trap a
//! gateware fault.

use crate::isa::NONE;
use crate::{CODE_WORDS, Core, Exports, Func, LoadError, MAX_FUNCS, MAX_GLOBALS, MAX_TABLE};

/// The magic: `MWI` and format version 1.
pub const MAGIC: [u8; 4] = *b"MWI\x01";
/// Header bytes.
pub const HEADER: usize = 28;
/// Bytes of one function-table entry.
pub const FUNC_BYTES: usize = 7;
/// A gap of zero bytes shorter than this joins two data segments (a
/// segment header costs 4 bytes).
const JOIN_GAP: usize = 5;

/// Why an image could not be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmitError {
    /// No payload is loaded.
    NotLoaded,
    /// The output buffer is too small (`need` bytes).
    Full {
        /// Bytes the image needs.
        need: usize,
    },
    /// A value does not fit its field (memory past 64 KiB).
    Unrepresentable,
}

struct Out<'a> {
    b: &'a mut [u8],
    at: usize,
}

impl Out<'_> {
    fn put(&mut self, v: &[u8]) {
        // Past the end only counts, so `Full` can say how much is needed.
        if let Some(d) = self.b.get_mut(self.at..self.at + v.len()) {
            d.copy_from_slice(v);
        }
        self.at += v.len();
    }
}

fn u16_of(v: usize) -> Result<u16, EmitError> {
    u16::try_from(v).map_err(|_| EmitError::Unrepresentable)
}

/// A sequential reader that refuses to read past the image, as the
/// installer's byte fetch does.
struct In<'a> {
    b: &'a [u8],
    at: usize,
}

impl In<'_> {
    fn le(&mut self, n: usize) -> Result<u64, LoadError> {
        let mut v = 0u64;
        for k in 0..n {
            let byte = *self.b.get(self.at).ok_or(LoadError::Malformed)?;
            self.at += 1;
            v |= u64::from(byte) << (8 * k);
        }
        Ok(v)
    }

    fn u8(&mut self) -> Result<usize, LoadError> {
        self.le(1).map(|v| v as usize)
    }

    fn u16(&mut self) -> Result<usize, LoadError> {
        self.le(2).map(|v| v as usize)
    }
}

impl<const MEM: usize> Core<MEM> {
    /// Data segments of the initial memory: runs of non-zero bytes, joined
    /// across short gaps.
    fn segments(&self, mut f: impl FnMut(usize, &[u8])) {
        let mem = self.memory();
        let mut i = 0;
        while i < mem.len() {
            if mem.get(i) == Some(&0) {
                i += 1;
                continue;
            }
            let start = i;
            let mut end = i;
            while end < mem.len() {
                // Extend over the next run of zeros if another non-zero
                // byte follows within JOIN_GAP.
                let next = mem
                    .get(end..)
                    .and_then(|r| r.iter().position(|b| *b != 0))
                    .map(|p| end + p);
                match next {
                    Some(n) if n == end || n - end < JOIN_GAP => {
                        let run = mem
                            .get(n..)
                            .and_then(|r| r.iter().position(|b| *b == 0))
                            .map_or(mem.len(), |p| n + p);
                        end = run;
                    }
                    _ => break,
                }
            }
            f(start, mem.get(start..end).unwrap_or_default());
            i = end;
        }
    }

    /// Writes the loaded payload as an image into `out`; returns its
    /// length.
    pub fn emit_image(&self, out: &mut [u8]) -> Result<usize, EmitError> {
        if !self.loaded {
            return Err(EmitError::NotLoaded);
        }
        let mut nsegs = 0usize;
        self.segments(|_, _| nsegs += 1);
        let ex = self.exports;
        let exp = [
            ex.init,
            ex.tick,
            ex.alloc,
            ex.on_message,
            ex.on_irq,
            ex.on_request,
        ];
        let mut o = Out { b: out, at: 0 };
        o.put(&MAGIC);
        o.put(&u16_of(self.code_len)?.to_le_bytes());
        o.put(&[self.nfuncs as u8, self.table_len as u8, self.nglobals as u8]);
        o.put(&[self.page_log2 as u8]);
        o.put(&u16_of(nsegs)?.to_le_bytes());
        o.put(&u16_of(self.mem_len)?.to_le_bytes());
        o.put(&u16_of(self.mem_max)?.to_le_bytes());
        for e in exp {
            o.put(&e.unwrap_or(NONE).to_le_bytes());
        }
        for w in self.code() {
            o.put(&w.to_le_bytes());
        }
        for f in self.funcs() {
            o.put(&[f.import]);
            o.put(&f.entry.to_le_bytes());
            o.put(&[f.params, f.results]);
            o.put(&f.locals.to_le_bytes());
        }
        for t in self.table() {
            o.put(&t.to_le_bytes());
        }
        for g in self.globals() {
            o.put(&g.to_le_bytes());
        }
        let mut bad = false;
        self.segments(|off, bytes| match (u16_of(off), u16_of(bytes.len())) {
            (Ok(a), Ok(n)) => {
                o.put(&a.to_le_bytes());
                o.put(&n.to_le_bytes());
                o.put(bytes);
            }
            _ => bad = true,
        });
        if bad {
            return Err(EmitError::Unrepresentable);
        }
        if o.at > o.b.len() {
            return Err(EmitError::Full { need: o.at });
        }
        Ok(o.at)
    }

    /// Installs an image: resets every table and memory, then fills them
    /// from `img` with the installer's checks. On error the core holds no
    /// payload.
    pub fn install(&mut self, img: &[u8]) -> Result<(), LoadError> {
        self.reset();
        let r = self.install_inner(img);
        if r.is_err() {
            self.reset();
        } else {
            self.loaded = true;
        }
        r
    }

    fn install_inner(&mut self, img: &[u8]) -> Result<(), LoadError> {
        let mut r = In { b: img, at: 0 };
        if r.le(4)? != u64::from(u32::from_le_bytes(MAGIC)) {
            return Err(LoadError::Malformed);
        }
        let code_len = r.u16()?;
        if code_len > CODE_WORDS {
            return Err(LoadError::TooLarge("code"));
        }
        let nfuncs = r.u8()?;
        if nfuncs > MAX_FUNCS {
            return Err(LoadError::TooLarge("functions"));
        }
        let table_len = r.u8()?;
        if table_len > MAX_TABLE {
            return Err(LoadError::TooLarge("table"));
        }
        let nglobals = r.u8()?;
        if nglobals > MAX_GLOBALS {
            return Err(LoadError::TooLarge("globals"));
        }
        let page_log2 = r.u8()?;
        if page_log2 != 0 && page_log2 != 16 {
            return Err(LoadError::Malformed);
        }
        let nsegs = r.u16()?;
        let mem_len = r.u16()?;
        if mem_len > MEM {
            return Err(LoadError::MemoryTooLarge {
                bytes: mem_len as u64,
                max: MEM as u32,
            });
        }
        let mem_max = r.u16()?;
        if mem_max > MEM {
            return Err(LoadError::MemoryTooLarge {
                bytes: mem_max as u64,
                max: MEM as u32,
            });
        }
        if mem_max < mem_len {
            return Err(LoadError::Malformed);
        }
        let func_ref = |v: usize| -> Result<Option<u16>, LoadError> {
            match v {
                v if v == usize::from(NONE) => Ok(None),
                v if v < nfuncs => Ok(Some(v as u16)),
                _ => Err(LoadError::Malformed),
            }
        };
        let mut exp = [None; 6];
        for e in &mut exp {
            *e = func_ref(r.u16()?)?;
        }
        let [init, tick, alloc, on_message, on_irq, on_request] = exp;
        self.exports = Exports {
            init,
            tick,
            alloc,
            on_message,
            on_irq,
            on_request,
        };
        self.page_log2 = page_log2 as u32;
        self.mem_len = mem_len;
        self.mem_max = mem_max;
        for i in 0..code_len {
            let w = r.le(4)? as u32;
            *self.code.get_mut(i).ok_or(LoadError::Malformed)? = w;
        }
        self.code_len = code_len;
        for i in 0..nfuncs {
            let v = r.le(FUNC_BYTES)?;
            let f = Func {
                import: v as u8,
                entry: (v >> 8) as u16,
                params: (v >> 24) as u8,
                results: (v >> 32) as u8,
                locals: (v >> 40) as u16,
            };
            *self.funcs.get_mut(i).ok_or(LoadError::Malformed)? = f;
        }
        self.nfuncs = nfuncs;
        self.nimports = self
            .funcs()
            .iter()
            .filter(|f| f.import != Func::LOCAL)
            .count();
        for i in 0..table_len {
            let t = r.u16()?;
            if t != usize::from(NONE) {
                func_ref(t)?;
            }
            *self.table.get_mut(i).ok_or(LoadError::Malformed)? = t as u16;
        }
        self.table_len = table_len;
        for i in 0..nglobals {
            let g = r.le(4)? as u32 as i32;
            *self.globals.get_mut(i).ok_or(LoadError::Malformed)? = g;
        }
        self.nglobals = nglobals;
        for _ in 0..nsegs {
            let off = r.u16()?;
            let n = r.u16()?;
            if off + n > mem_len {
                return Err(LoadError::Malformed);
            }
            for j in 0..n {
                let b = r.u8()? as u8;
                *self.mem.get_mut(off + j).ok_or(LoadError::Malformed)? = b;
            }
        }
        if r.at != img.len() {
            return Err(LoadError::Malformed);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::boxed::Box;
    use std::vec;
    use std::vec::Vec;

    use super::*;
    use crate::Colorlight;

    fn module() -> Vec<u8> {
        wat::parse_str(
            r#"(module
  (import "mw" "log" (func $log (param i32 i32)))
  (memory (export "memory") 4096 8192 (pagesize 1))
  (table 3 funcref)
  (elem (i32.const 1) $tick)
  (global $g (mut i32) (i32.const -7))
  (global i32 (i32.const 9))
  (data (i32.const 16) "hello")
  (data (i32.const 23) "world")
  (data (i32.const 4000) "\01\00\00\00\00\02")
  (func (export "init") (call $log (i32.const 16) (i32.const 5)))
  (func $tick (export "tick") (param i32) (global.set $g (local.get 0))))"#,
        )
        .unwrap()
    }

    fn emitted(wasm: &[u8]) -> (Box<Colorlight>, Vec<u8>) {
        let mut c = Box::new(Colorlight::new());
        c.load(wasm).unwrap();
        let mut out = vec![0u8; 16 * 1024];
        let n = c.emit_image(&mut out).unwrap();
        out.truncate(n);
        (c, out)
    }

    fn same(a: &Colorlight, b: &Colorlight) {
        assert_eq!(a.code(), b.code());
        assert_eq!(a.funcs(), b.funcs());
        assert_eq!(a.table(), b.table());
        assert_eq!(a.globals(), b.globals());
        assert_eq!(a.memory(), b.memory());
        assert_eq!(a.exports(), b.exports());
        assert_eq!(a.nimports, b.nimports);
        assert_eq!((a.mem_max, a.page_log2), (b.mem_max, b.page_log2));
        assert_eq!(a.loaded, b.loaded);
    }

    #[test]
    fn cl6_an_installed_image_is_the_compiled_module() {
        let (c, img) = emitted(&module());
        assert_eq!(&img[..4], b"MWI\x01");
        let mut d = Box::new(Colorlight::new());
        d.install(&img).unwrap();
        same(&c, &d);
        assert_eq!((d.mem_max, d.page_log2), (8192, 0));
    }

    #[test]
    fn cl6_segments_cover_the_non_zero_bytes_only() {
        let (c, img) = emitted(&module());
        let mut segs = Vec::new();
        c.segments(|o, b| segs.push((o, b.len())));
        // "hello" and "world" are 2 bytes apart (joined); the 6 bytes at
        // 4000 hold 4 zeros (joined: shorter than a segment header).
        assert_eq!(segs, [(16, 12), (4000, 6)]);
        assert_eq!(u16::from_le_bytes([img[10], img[11]]), 2);
    }

    #[test]
    fn cl6_emit_reports_the_size_it_needs() {
        let (c, img) = emitted(&module());
        let mut small = vec![0u8; img.len() - 1];
        assert_eq!(
            c.emit_image(&mut small),
            Err(EmitError::Full { need: img.len() })
        );
        let empty = Box::new(Colorlight::new());
        assert_eq!(empty.emit_image(&mut small), Err(EmitError::NotLoaded));
    }

    fn refused(img: &[u8]) -> LoadError {
        let mut d = Box::new(Colorlight::new());
        let e = d.install(img).unwrap_err();
        assert!(!d.loaded && d.code().is_empty() && d.memory().is_empty());
        e
    }

    #[test]
    fn cl6_a_bad_image_is_refused_and_leaves_nothing_installed() {
        let (_, img) = emitted(&module());
        let set = |at: usize, v: &[u8]| {
            let mut b = img.clone();
            b[at..at + v.len()].copy_from_slice(v);
            b
        };
        for cut in [0, 3, 4, HEADER - 1, HEADER, img.len() - 1] {
            assert_eq!(refused(&img[..cut]), LoadError::Malformed, "cut at {cut}");
        }
        let mut long = img.clone();
        long.push(0);
        assert_eq!(refused(&long), LoadError::Malformed);
        assert_eq!(refused(&set(3, &[2])), LoadError::Malformed);
        assert_eq!(
            refused(&set(4, &4097u16.to_le_bytes())),
            LoadError::TooLarge("code")
        );
        assert_eq!(refused(&set(6, &[129])), LoadError::TooLarge("functions"));
        assert_eq!(refused(&set(7, &[65])), LoadError::TooLarge("table"));
        assert_eq!(refused(&set(8, &[17])), LoadError::TooLarge("globals"));
        assert_eq!(refused(&set(9, &[12])), LoadError::Malformed);
        assert!(matches!(
            refused(&set(12, &16385u16.to_le_bytes())),
            LoadError::MemoryTooLarge { bytes: 16385, .. }
        ));
        assert!(matches!(
            refused(&set(14, &16385u16.to_le_bytes())),
            LoadError::MemoryTooLarge { .. }
        ));
        assert_eq!(
            refused(&set(14, &4095u16.to_le_bytes())),
            LoadError::Malformed
        );
        // An export naming a function past the table (3 functions).
        assert_eq!(refused(&set(16, &3u16.to_le_bytes())), LoadError::Malformed);
        // A table entry naming no function: the table follows the code
        // and the functions.
        let code = u16::from_le_bytes([img[4], img[5]]) as usize;
        let tab = HEADER + 4 * code + 3 * FUNC_BYTES;
        assert_eq!(
            refused(&set(tab, &3u16.to_le_bytes())),
            LoadError::Malformed
        );
        // A segment past the initial memory: the first segment's offset.
        let seg = tab + 2 * 3 + 4 * 2;
        assert_eq!(
            refused(&set(seg, &4090u16.to_le_bytes())),
            LoadError::Malformed
        );
    }
}
