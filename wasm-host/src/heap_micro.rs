//! Heap bound of a payload (requirement W10) on the `micro` backend: pure
//! arithmetic behind
//! [`Limits::load_heap_needed`](crate::Limits::load_heap_needed).
//!
//! The backend keeps a copy of the image and an index over it
//! (`wasm_micro::footprint`); nothing is translated. A payload goes
//! through two phases and the bound is the larger:
//!
//! 1. **Compile** ([`Compiled::new`](crate::Compiled::new)): the caller's
//!    image buffer, the copy, the index and the parser's scratch.
//! 2. **Run**: the copy and the index, linear memory up to what the module
//!    can reach, the three interpreter stacks (allocated once, in full),
//!    globals, the table, one handler per import and the fixed host state
//!    ([`RUNTIME_FIXED`]). The caller's image is not counted.
//!
//! With the image at hand ([`needed_for`]) the index, globals and table
//! are exact. Without it ([`needed`]) they are bounded by the densest
//! encoding any image of that length could hold, which is far more than
//! real modules use (the example payloads index at well under one byte per
//! image byte): prefer [`Limits::heap_needed`](crate::Limits::heap_needed).
//!
//! The figures are payload bytes as the allocator's callers request them;
//! a firmware allocator adds its own per-block overhead.

use middleware::wasm_abi::periph::MAX_DMA;
use middleware::wasm_abi::serve::MAX_SERVES;
use middleware::wasm_abi::{MAX_LOG, MAX_SUBSCRIPTIONS};

use crate::error::INIT_LOG_LINES;

/// Pointer width of the target, in bytes.
pub const PTR: u32 = core::mem::size_of::<usize>() as u32;

/// One value-stack slot.
pub const SLOT_BYTES: u32 = 8;
/// One call frame and one open block (`wasm-micro`'s records).
pub const FRAME_BYTES: u32 = 12;
/// Open blocks allowed per value-stack slot; see [`labels`].
pub const LABEL_BYTES: u32 = 12;

/// Heap a call may still allocate per level of call depth after the load:
/// none. Every frame is allocated at instantiation, in [`stacks_bytes`].
pub const PER_CALL_DEPTH: u32 = 0;

/// The largest table a payload may declare (4 bytes an entry).
pub const MAX_TABLE: usize = 1024;

/// Value-stack slots for [`Limits::max_stack_bytes`](crate::Limits).
pub const fn stack_slots(stack: u32) -> usize {
    (stack / SLOT_BYTES) as usize
}

/// Open blocks allowed across all active calls: one per four value-stack
/// slots, so both scale with the same knob. Compilers nest blocks deeply
/// for `match` (one block per arm), not for calls.
pub const fn labels(stack: u32) -> usize {
    stack_slots(stack) / 4
}

/// Allowance for what is allocated while the payload runs, beyond the
/// interpreter's own storage. The host state itself (keys, the DMA copy
/// buffer, `init`'s log, the channel table) is inline in
/// [`State`](crate::link::State) since it moved to fixed containers
/// ([`crate::fixed`]) and takes no heap: what remains is one handler per
/// import, a trap message, and whatever the device allocates inside a
/// call. The terms are kept at the sizes the state had on the heap, as
/// that allowance.
///
/// Measured on a 64-bit host by `tools/payload-sim/tests/heap.rs` built
/// with `--no-default-features --features micro`: the largest run peak
/// over the resident state is 11.4 KB (`cam_view` and `cam_jpeg_view`,
/// simulator allocations inside their calls included), which this covers
/// with 3.3 KB to spare. `cam_jpeg_view` (26.5 KB image) keeps a 30.0 KB
/// module (the image copy and a 3.5 KB index), against 127 KB on wasmi.
/// A resident `blink` holds 240 bytes of heap beyond its module, memory
/// and stacks. Not measured on a 32-bit host.
pub const RUNTIME_FIXED: u32 = MAX_DMA as u32
    + (MAX_SUBSCRIPTIONS as u32) * (middleware::key::MAX_KEY_LEN as u32 + 4 * PTR)
    + (MAX_SERVES as u32) * (middleware::key::MAX_KEY_LEN as u32 + 4 * PTR)
    + (INIT_LOG_LINES as u32) * (MAX_LOG as u32 + 4 * PTR)
    + (middleware::wasm_abi::channels::MAX_CHANNELS as u32)
        * (middleware::wasm_abi::channels::MAX_NAME as u32
            + middleware::key::MAX_KEY_LEN as u32
            + 12 * PTR)
    + 4096
    + 256 * PTR;

/// Bytes the interpreter's stacks take.
pub const fn stacks_bytes(stack: u32, depth: u32) -> u64 {
    let slots = stack_slots(stack) as u64;
    slots * SLOT_BYTES as u64
        + (depth as u64) * FRAME_BYTES as u64
        + (labels(stack) as u64) * LABEL_BYTES as u64
}

/// What a module of `image_len` bytes keeps beside its image, at most:
/// the index (`wasm_micro::index_bound`, which also covers globals and
/// import handlers: each takes less heap per image byte than an `if`),
/// plus a full table.
pub const fn module_bound(image_len: u32) -> u64 {
    wasm_micro::index_bound(image_len as usize) as u64 + 4 * MAX_TABLE as u64
}

const fn phases(
    image_len: u32,
    module: u64,
    scratch: u64,
    memory: u32,
    stack: u32,
    depth: u32,
) -> u32 {
    let image = image_len as u64;
    let compile = 2 * image + module + scratch;
    let run = image + module + memory as u64 + stacks_bytes(stack, depth) + RUNTIME_FIXED as u64;
    let max = if compile > run { compile } else { run };
    if max > u32::MAX as u64 {
        u32::MAX
    } else {
        max as u32
    }
}

/// The bound for any image of `image_len` bytes, saturating at `u32::MAX`.
pub const fn needed(image_len: u32, memory: u32, stack: u32, depth: u32) -> u32 {
    // The parser's scratch (8 bytes per nesting level) is at most the
    // index bound again; in the compile phase only.
    let module = module_bound(image_len);
    phases(image_len, module, module, memory, stack, depth)
}

/// The bound for this image: its measured index, globals, table and
/// imports. An image that does not parse gets [`needed`].
pub fn needed_for(wasm: &[u8], memory: u32, stack: u32, depth: u32) -> u32 {
    let len = u32::try_from(wasm.len()).unwrap_or(u32::MAX);
    let Ok(f) = wasm_micro::footprint(wasm) else {
        return needed(len, memory, stack, depth);
    };
    // Imports: one handler each; at least two bytes of image per import.
    let handlers = (len as u64 / 2).min(64) * PTR as u64;
    let module = f.index as u64 + 8 * f.globals as u64 + 4 * f.table as u64 + handlers;
    phases(len, module, f.scratch as u64, memory, stack, depth)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w10_micro_bound_grows_with_every_input_and_saturates() {
        let base = needed(10_000, 65_536, 4096, 32);
        assert!(needed(10_001, 65_536, 4096, 32) > base);
        assert_eq!(needed(10_000, 65_537, 4096, 32), base + 1);
        assert_eq!(needed(10_000, 65_536, 4096, 33), base + FRAME_BYTES);
        // 32 more bytes of stack: four slots and one open block.
        assert_eq!(
            needed(10_000, 65_536, 4096 + 32, 32),
            base + 32 + LABEL_BYTES
        );
        assert_eq!(needed(u32::MAX, u32::MAX, u32::MAX, u32::MAX), u32::MAX);
    }

    #[test]
    fn w10_micro_measured_bound_is_below_the_worst_case() {
        // No image: nothing measured, so the worst case.
        assert_eq!(
            needed_for(b"", 65_536, 4096, 32),
            needed(0, 65_536, 4096, 32)
        );
        let wasm = b"\0asm\x01\0\0\0";
        assert!(needed_for(wasm, 65_536, 4096, 32) < needed(8, 65_536, 4096, 32));
    }
}
