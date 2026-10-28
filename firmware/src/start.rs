//! The heap check before a payload starts (requirement W10).
//!
//! `wasm_host::Limits::load_heap_needed` bounds the heap a payload can use
//! from compiling its image to its last call. The controller starts a
//! payload only if the free heap covers that bound plus
//! [`HEAP_MARGIN`], the allocator's own overhead (block headers,
//! alignment, the split between the reclaimed and `.bss` heap regions).
//! Otherwise the start is refused with a [`HeapShort`] report, before
//! anything is allocated, instead of the allocator failing later, which
//! halts the chip (an allocation failure is a panic).

/// Heap kept free on top of the payload's bound for the allocator's
/// per-block overhead and fragmentation, in bytes.
pub const HEAP_MARGIN: u32 = 4 * 1024;

/// Why a payload may not start: the heap free now is below what it needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeapShort {
    /// Heap free now, in bytes (saturated to `u32::MAX`).
    pub free: u32,
    /// The payload's bound plus [`HEAP_MARGIN`], in bytes.
    pub needed: u32,
}

impl core::fmt::Display for HeapShort {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "not enough heap: needs {} B ({} B bound + {} B margin), {} B free",
            self.needed,
            self.needed.saturating_sub(HEAP_MARGIN),
            HEAP_MARGIN,
            self.free
        )
    }
}

/// Whether a payload whose heap bound is `bound` bytes may start with
/// `free` bytes of heap free. Returns the bytes needed (bound plus
/// [`HEAP_MARGIN`], saturating) when it may.
pub const fn check_heap(free: usize, bound: u32) -> Result<u32, HeapShort> {
    let needed = bound.saturating_add(HEAP_MARGIN);
    let free = if free > u32::MAX as usize {
        u32::MAX
    } else {
        free as u32
    };
    if free >= needed {
        Ok(needed)
    } else {
        Err(HeapShort { free, needed })
    }
}

#[cfg(test)]
mod tests {
    extern crate alloc;
    use super::*;
    use alloc::string::ToString;

    #[test]
    fn w10_starts_only_with_bound_plus_margin_free() {
        assert_eq!(check_heap(100_000 + HEAP_MARGIN as usize, 100_000), Ok(100_000 + HEAP_MARGIN));
        assert_eq!(
            check_heap(100_000 + HEAP_MARGIN as usize - 1, 100_000),
            Err(HeapShort { free: 100_000 + HEAP_MARGIN - 1, needed: 100_000 + HEAP_MARGIN })
        );
        assert_eq!(check_heap(0, 0), Err(HeapShort { free: 0, needed: HEAP_MARGIN }));
    }

    #[test]
    fn w10_saturates() {
        assert_eq!(check_heap(usize::MAX, u32::MAX), Ok(u32::MAX));
        assert_eq!(check_heap(10, u32::MAX), Err(HeapShort { free: 10, needed: u32::MAX }));
    }

    #[test]
    fn w10_refusal_reports_free_and_needed() {
        let short = HeapShort { free: 150_000, needed: 201_000 };
        assert_eq!(
            short.to_string(),
            "not enough heap: needs 201000 B (196904 B bound + 4096 B margin), 150000 B free"
        );
    }
}
