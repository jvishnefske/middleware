//! Heap bound of a payload (requirement W10): pure arithmetic behind
//! [`Limits::load_heap_needed`](crate::Limits::load_heap_needed).
//!
//! A payload goes through two phases, and the heap it uses in each is
//! bounded separately; the bound is the larger of the two.
//!
//! 1. **Compile** ([`Compiled::new`](crate::Compiled::new)): the caller's
//!    image buffer, wasmi's module (parsed sections, data segments and the
//!    bytecode of every function, translated eagerly) and the translator's
//!    working buffers ([`compile_bytes`]).
//! 2. **Run** ([`Payload::instantiate`](crate::Payload::instantiate) and every
//!    later call): the module, linear memory up to the cap, the value
//!    stack (allocated once, in full), the call frame list up to the depth
//!    cap, and the fixed host state: linker, store, instance, the DMA copy
//!    buffer, the subscription list and `init`'s bounded log
//!    ([`run_bytes`]). The image is not counted: the caller frees it
//!    between the phases.
//!
//! The module is modelled as `MODULE_FIXED + image * MODULE_PER_BYTE_X16 /
//! 16`. wasmi's bytecode is made of pointer-sized words, so both terms
//! scale with the target's pointer width ([`PTR`]): the constants were fitted
//! to the example payloads measured on a 64-bit host
//! (`tools/payload-sim/tests/heap.rs`, part of the test suite) and on a
//! 32-bit host (the same test built for `i686-unknown-linux-gnu`, the
//! closest runnable stand-in for the 32-bit Xtensa chips), with margin.
//! With the fixed part set to 16 KiB (32 bits) and 20 KiB (64 bits), the
//! measured modules needed at most 2.64 and 3.84 bytes per image byte
//! (`cam_view`); `cam_jpeg_view` (30.9 KB image, 27.4 KB of code)
//! measured a 94 KB module on 32 bits and 145 KB on 64 bits.
//!
//! The figures are payload bytes as the allocator's callers request them;
//! a firmware allocator adds its own per-block overhead and fragmentation,
//! which the firmware covers with its own margin.

use middleware::wasm_abi::periph::MAX_DMA;
use middleware::wasm_abi::{MAX_LOG, MAX_SUBSCRIPTIONS};

use crate::error::INIT_LOG_LINES;

/// Pointer width of the target, in bytes (4 on the ESP32 family).
pub const PTR: u32 = core::mem::size_of::<usize>() as u32;

/// Module bytes independent of the image size: function types, the
/// export and import tables, wasmi's per-module bookkeeping.
pub const MODULE_FIXED: u32 = 12 * 1024 + 1024 * PTR;

/// Module bytes per image byte, in sixteenths: 2.75 on 32-bit targets,
/// 4.25 on 64-bit ones.
pub const MODULE_PER_BYTE_X16: u32 = 20 + 6 * PTR;

/// The translator's working buffers while it compiles, on top of the
/// module: this plus half the image. Measured at most 8.4 KB (32-bit) and
/// 10.9 KB (64-bit), for `cam_jpeg_view`.
pub const TRANSLATE_FIXED: u32 = 8 * 1024;

/// One wasmi call frame record: an instruction pointer, a stack offset
/// and an optional instance handle.
pub const FRAME_BYTES: u32 = 3 * PTR;

/// Heap per allowed call depth: the frame list grows by doubling, so its
/// capacity stays under twice the depth, and while it reallocates the old
/// list (at most the depth) is still held: three frames per level.
pub const PER_CALL_DEPTH: u32 = 3 * FRAME_BYTES;

/// Fixed host state while the payload runs: the linker (one entry per
/// `mw` import), store, instance and engine, the DMA copy buffer
/// ([`MAX_DMA`] bytes), the subscription list with its keys, `init`'s
/// bounded log, and a trap message: 19.7 KB on 32-bit targets, 32.2 KB
/// on 64-bit ones. Measured (instantiation or run peak minus module,
/// memory and value stack) at most 19.6 KB and 26.8 KB, both for
/// `cam_jpeg_view`, simulator allocations inside its calls included.
pub const RUNTIME_FIXED: u32 = MAX_DMA as u32
    + (MAX_SUBSCRIPTIONS as u32) * (middleware::key::MAX_KEY_LEN as u32 + 4 * PTR)
    + (INIT_LOG_LINES as u32) * (MAX_LOG as u32 + 4 * PTR)
    + 3072 * PTR;

/// wasmi's module for an image of `image_len` bytes.
pub const fn module_bytes(image_len: u32) -> u64 {
    MODULE_FIXED as u64 + (image_len as u64) * (MODULE_PER_BYTE_X16 as u64) / 16
}

/// Heap while compiling an image of `image_len` bytes, the image included.
pub const fn compile_bytes(image_len: u32) -> u64 {
    let image = image_len as u64;
    image + module_bytes(image_len) + TRANSLATE_FIXED as u64 + image / 2
}

/// Heap while the payload is instantiated and runs, without the image.
pub const fn run_bytes(image_len: u32, memory: u32, stack: u32, depth: u32) -> u64 {
    module_bytes(image_len)
        + memory as u64
        + stack as u64
        + (depth as u64) * (PER_CALL_DEPTH as u64)
        + RUNTIME_FIXED as u64
}

/// The larger of [`compile_bytes`] and [`run_bytes`], saturating at
/// `u32::MAX`.
pub const fn needed(image_len: u32, memory: u32, stack: u32, depth: u32) -> u32 {
    let compile = compile_bytes(image_len);
    let run = run_bytes(image_len, memory, stack, depth);
    let max = if compile > run { compile } else { run };
    if max > u32::MAX as u64 {
        u32::MAX
    } else {
        max as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w10_bound_is_the_larger_phase() {
        // A tiny image: the run phase (64 KiB of memory) dominates.
        let run = run_bytes(100, 65_536, 4096, 64);
        assert_eq!(u64::from(needed(100, 65_536, 4096, 64)), run);
        assert!(run > compile_bytes(100));
        // No memory and a large image: compiling dominates.
        let compile = compile_bytes(1 << 20);
        assert_eq!(u64::from(needed(1 << 20, 0, 0, 0)), compile);
    }

    #[test]
    fn w10_bound_grows_with_every_input_and_saturates() {
        let base = needed(10_000, 65_536, 4096, 64);
        assert!(needed(10_001, 65_536, 4096, 64) >= base);
        assert_eq!(needed(10_000, 65_537, 4096, 64), base + 1);
        assert_eq!(needed(10_000, 65_536, 4097, 64), base + 1);
        assert_eq!(needed(10_000, 65_536, 4096, 65), base + PER_CALL_DEPTH);
        assert_eq!(needed(u32::MAX, u32::MAX, u32::MAX, u32::MAX), u32::MAX);
    }

    #[test]
    fn w10_module_terms_scale_with_pointer_width() {
        // 2.75 and 4.25 bytes of module per image byte on 32 / 64 bits.
        assert_eq!(20 + 6 * 4, 44);
        assert_eq!(20 + 6 * 8, 68);
        assert_eq!(MODULE_PER_BYTE_X16, 20 + 6 * PTR);
        assert_eq!(module_bytes(0), u64::from(MODULE_FIXED));
        assert_eq!(
            module_bytes(16),
            u64::from(MODULE_FIXED + MODULE_PER_BYTE_X16)
        );
    }
}
