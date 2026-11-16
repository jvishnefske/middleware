//! The internal instruction set: what the loader writes into code RAM and
//! the interpreter executes. The gateware's code BRAM holds exactly these
//! 32-bit words.
//!
//! Every word is `op[31:24] | a[23:0]`. Some instructions take one more
//! word (a literal or a memory offset); [`BR`]-format words pack a branch:
//!
//! ```text
//! a[15:0]   target word address (absolute)
//! a[16]     keep: the label's arity (0 or 1 slot carried over)
//! a[23:17]  drop: slots discarded below the kept value
//! ```
//!
//! A taken branch saves the top `keep` slots, pops `drop` more, pushes the
//! saved slots back and jumps. Every target, keep and drop is computed at
//! load from the static operand-stack height, so the interpreter needs no
//! control stack, no block-type lookups and no LEB decoding: this resolved
//! code *is* the branch side table.
//!
//! The value stack is 32-bit slots. A frame is `[params][locals][operands]`
//! with `fp` at the first parameter; `i64` values (only `i64.const` feeding
//! `i64.store` is accepted) take two slots, low word first.
//!
//! | op | name | a | extra word | stack |
//! |----|------|---|------------|-------|
//! | 00 | `UNREACHABLE` | — | — | traps |
//! | 01 | `CONST` | sign-extended 24-bit value | — | `→ v` |
//! | 02 | `CONST32` | — | the value | `→ v` |
//! | 03 | `DROP` | slots | — | `v.. →` |
//! | 04 | `SELECT` | — | — | `a b c → (c ? a : b)` |
//! | 05/06/07 | `LGET`/`LSET`/`LTEE` | slot index from `fp` | — | |
//! | 08/09 | `GGET`/`GSET` | global index | — | |
//! | 0A | `LOAD` | [`load`] kind | offset | `addr → v` |
//! | 0B | `STORE` | [`store`] kind | offset | `addr v →` (`addr lo hi →` for 64) |
//! | 0C | `MSIZE` | — | — | `→ pages` |
//! | 0D | `MGROW` | — | — | `n → old pages or -1` |
//! | 0E | `MCOPY` | — | — | `dst src n →` |
//! | 0F | `MFILL` | — | — | `dst val n →` |
//! | 10 | `UN` | [`un`] op | — | `a → r` |
//! | 11 | `BIN` | [`bin`] op | — | `a b → r` |
//! | 12 | `BR` | branch | — | |
//! | 13 | `BRIF` | branch | — | `c →` (taken when `c != 0`) |
//! | 14 | `BRUNLESS` | target | — | `c →` (jumps when `c == 0`; `if`) |
//! | 15 | `BRTABLE` | `n` | `n + 1` branch words | `i →` |
//! | 16 | `CALL` | function index | — | args → results |
//! | 17 | `CALLI` | `params << 8 \| results` | — | args `i` → results |
//! | 18 | `HOST` | ABI import index ([`middleware::wasm_abi::IMPORTS`]) | — | args → result |
//! | 19 | `RET` | keep (0 or 1) | — | |

/// `unreachable`.
pub const UNREACHABLE: u8 = 0x00;
/// Push a sign-extended 24-bit constant.
pub const CONST: u8 = 0x01;
/// Push the next word.
pub const CONST32: u8 = 0x02;
/// Drop `a` slots.
pub const DROP: u8 = 0x03;
/// `select`.
pub const SELECT: u8 = 0x04;
/// `local.get`.
pub const LGET: u8 = 0x05;
/// `local.set`.
pub const LSET: u8 = 0x06;
/// `local.tee`.
pub const LTEE: u8 = 0x07;
/// `global.get`.
pub const GGET: u8 = 0x08;
/// `global.set`.
pub const GSET: u8 = 0x09;
/// A load; `a` is a [`load`] kind, the next word the offset.
pub const LOAD: u8 = 0x0A;
/// A store; `a` is a [`store`] kind, the next word the offset.
pub const STORE: u8 = 0x0B;
/// `memory.size`.
pub const MSIZE: u8 = 0x0C;
/// `memory.grow`.
pub const MGROW: u8 = 0x0D;
/// `memory.copy`.
pub const MCOPY: u8 = 0x0E;
/// `memory.fill`.
pub const MFILL: u8 = 0x0F;
/// A unary i32 op ([`un`]).
pub const UN: u8 = 0x10;
/// A binary i32 op ([`bin`]).
pub const BIN: u8 = 0x11;
/// Unconditional branch.
pub const BR: u8 = 0x12;
/// Branch if the popped condition is non-zero.
pub const BRIF: u8 = 0x13;
/// Jump to `a[15:0]` if the popped condition is zero (the `if` entry).
pub const BRUNLESS: u8 = 0x14;
/// `br_table` with `a` labels plus the default, as branch words.
pub const BRTABLE: u8 = 0x15;
/// Call a function of the module.
pub const CALL: u8 = 0x16;
/// `call_indirect` through table 0.
pub const CALLI: u8 = 0x17;
/// Call a host import.
pub const HOST: u8 = 0x18;
/// Return from the current function.
pub const RET: u8 = 0x19;

/// Load kinds.
pub mod load {
    /// `i32.load`.
    pub const W32: u32 = 0;
    /// `i32.load8_s`.
    pub const S8: u32 = 1;
    /// `i32.load8_u`.
    pub const U8: u32 = 2;
    /// `i32.load16_s`.
    pub const S16: u32 = 3;
    /// `i32.load16_u`.
    pub const U16: u32 = 4;
}

/// Store kinds.
pub mod store {
    /// `i32.store`.
    pub const W32: u32 = 0;
    /// `i32.store8`.
    pub const W8: u32 = 1;
    /// `i32.store16`.
    pub const W16: u32 = 2;
    /// `i64.store` (two slots).
    pub const W64: u32 = 3;
}

/// Unary ops.
pub mod un {
    /// `i32.eqz`.
    pub const EQZ: u32 = 0;
    /// `i32.clz`.
    pub const CLZ: u32 = 1;
    /// `i32.ctz`.
    pub const CTZ: u32 = 2;
    /// `i32.popcnt`.
    pub const POPCNT: u32 = 3;
    /// `i32.extend8_s`.
    pub const EXT8: u32 = 4;
    /// `i32.extend16_s`.
    pub const EXT16: u32 = 5;
}

/// Binary ops: `0..=9` are the comparisons in WebAssembly order
/// (`eq ne lt_s lt_u gt_s gt_u le_s le_u ge_s ge_u`, opcode − 0x46),
/// `10..=24` the arithmetic (`add sub mul div_s div_u rem_s rem_u and or
/// xor shl shr_s shr_u rotl rotr`, opcode − 0x6A + 10).
pub mod bin {
    /// First arithmetic op (`add`).
    pub const ARITH: u32 = 10;
    /// Last op (`rotr`).
    pub const LAST: u32 = 24;
}

/// No fixup / no target.
pub const NONE: u16 = 0xFFFF;

/// Builds an instruction word.
pub const fn word(op: u8, a: u32) -> u32 {
    ((op as u32) << 24) | (a & 0x00FF_FFFF)
}

/// The op of a word.
pub const fn op(w: u32) -> u8 {
    (w >> 24) as u8
}

/// The 24-bit argument of a word.
pub const fn arg(w: u32) -> u32 {
    w & 0x00FF_FFFF
}

/// Packs a branch argument; `None` if `drop` does not fit 7 bits.
pub const fn branch(target: u16, keep: u32, drop: u32) -> Option<u32> {
    if keep > 1 || drop > 0x7F {
        return None;
    }
    Some((target as u32) | (keep << 16) | (drop << 17))
}

/// Unpacks a branch argument into `(target, keep, drop)`.
pub const fn unbranch(a: u32) -> (u16, u32, u32) {
    ((a & 0xFFFF) as u16, (a >> 16) & 1, (a >> 17) & 0x7F)
}

/// Sign-extends a 24-bit `CONST` argument.
pub const fn sext24(a: u32) -> i32 {
    ((a << 8) as i32) >> 8
}

/// Whether `v` fits a `CONST` argument.
pub const fn fits24(v: i32) -> bool {
    v >= -(1 << 23) && v < (1 << 23)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cl3_branch_words_round_trip() {
        let a = branch(0x1234, 1, 0x7F).unwrap();
        assert_eq!(unbranch(a), (0x1234, 1, 0x7F));
        assert_eq!(branch(0, 2, 0), None);
        assert_eq!(branch(0, 0, 0x80), None);
        let w = word(BR, a);
        assert_eq!((op(w), arg(w)), (BR, a));
    }

    #[test]
    fn cl3_short_constants_sign_extend() {
        for v in [0, 1, -1, (1 << 23) - 1, -(1 << 23)] {
            assert!(fits24(v));
            assert_eq!(sext24(arg(word(CONST, v as u32))), v);
        }
        assert!(!fits24(1 << 23));
        assert!(!fits24(-(1 << 23) - 1));
    }
}
