//! The interpreter's heap (`wasm` only): a fixed `.bss` arena of
//! `PROFILE.heap` bytes behind `linked_list_allocator`.
//!
//! [`init`] hands the arena to the allocator once at boot through
//! `Heap::init_from_slice`, which takes a `&'static mut` slice, so no
//! `unsafe` is needed (`embedded-alloc`'s `init` takes a raw address and is
//! `unsafe`). Only wasm-host allocates; [`free`] is what the start check
//! (`esp_base::start`) compares the payload's bound against.

use core::mem::MaybeUninit;

use linked_list_allocator::LockedHeap;
use static_cell::ConstStaticCell;

use super::board::PROFILE;

/// Heap arena size.
pub const SIZE: usize = PROFILE.heap;

#[global_allocator]
static HEAP: LockedHeap = LockedHeap::empty();

static ARENA: ConstStaticCell<[MaybeUninit<u8>; SIZE]> = ConstStaticCell::new([MaybeUninit::uninit(); SIZE]);

/// Gives the arena to the allocator. Called once, first thing at boot.
pub fn init() {
    HEAP.lock().init_from_slice(ARENA.take());
}

/// Heap bytes free now.
pub fn free() -> usize {
    HEAP.lock().free()
}
