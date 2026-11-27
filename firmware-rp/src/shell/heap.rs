//! The interpreter's heap (`wasm` only): a fixed `.bss` arena of
//! `PROFILE.heap` bytes.
//!
//! On chips with CAS atomics (RP2350) the arena sits behind
//! `linked_list_allocator`: [`init`] hands it over once at boot through
//! `Heap::init_from_slice`, which takes a `&'static mut` slice, so no
//! `unsafe` is needed (`embedded-alloc`'s `init` takes a raw address and is
//! `unsafe`). Only wasm-host allocates; [`free`] is what the start check
//! (`esp_base::start`) compares the payload's bound against.
//!
//! The RP2040 has no CAS, which that allocator's lock needs. There the
//! arena is `emballoc`'s own (its lock goes through portable-atomic's
//! critical sections), also with no `unsafe` here. emballoc does not
//! report how much is free, so [`free`] answers the arena size: right
//! between payloads, when nothing is allocated, and an over-estimate while
//! one runs. A start that does not fit then fails in wasm-host
//! (`LoadError::OutOfMemory`) instead of being refused beforehand.

use super::board::PROFILE;

/// Heap arena size.
pub const SIZE: usize = PROFILE.heap;

#[cfg(target_has_atomic = "ptr")]
mod arena {
    use core::mem::MaybeUninit;

    use linked_list_allocator::LockedHeap;
    use static_cell::ConstStaticCell;

    use super::SIZE;

    #[global_allocator]
    static HEAP: LockedHeap = LockedHeap::empty();

    static ARENA: ConstStaticCell<[MaybeUninit<u8>; SIZE]> =
        ConstStaticCell::new([MaybeUninit::uninit(); SIZE]);

    pub fn init() {
        HEAP.lock().init_from_slice(ARENA.take());
    }

    pub fn free() -> usize {
        HEAP.lock().free()
    }
}

#[cfg(not(target_has_atomic = "ptr"))]
mod arena {
    use super::SIZE;

    #[global_allocator]
    static HEAP: emballoc::Allocator<SIZE> = emballoc::Allocator::new();

    pub fn init() {}

    pub fn free() -> usize {
        SIZE
    }
}

/// Gives the arena to the allocator. Called once, first thing at boot.
pub fn init() {
    arena::init();
}

/// Heap bytes free now (see the module docs for the RP2040).
pub fn free() -> usize {
    arena::free()
}
