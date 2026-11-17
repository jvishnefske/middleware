//! The interpreter's heap (`wasm` only): an arena of `RT1050.heap` bytes in
//! SDRAM behind `linked_list_allocator`.
//!
//! The arena is a `.bss` static named `HEAP_ARENA`, which `memory-app.x`
//! places in SDRAM right after the image (NOLOAD; the bootloader zeroes
//! that part of the image region before it jumps, so the cell's flag reads
//! "not taken"). [`init`] hands it to the allocator through
//! `Heap::init_from_slice`, which takes a `&'static mut` slice, so no
//! `unsafe` is needed (as on the RP base firmware). Only wasm-host
//! allocates; [`free`] is what the start check (`esp_base::start`)
//! compares the payload's bound against.

use core::mem::MaybeUninit;

use imxrt_base::chip::RT1050;
use linked_list_allocator::LockedHeap;
use static_cell::ConstStaticCell;

/// Heap arena size.
pub const SIZE: usize = RT1050.heap;

#[global_allocator]
static HEAP: LockedHeap = LockedHeap::empty();

static HEAP_ARENA: ConstStaticCell<[MaybeUninit<u8>; SIZE]> =
    ConstStaticCell::new([MaybeUninit::uninit(); SIZE]);

/// Gives the arena to the allocator. Called once, first thing at boot.
pub fn init() {
    HEAP.lock().init_from_slice(HEAP_ARENA.take());
}

/// Heap bytes free now.
pub fn free() -> usize {
    HEAP.lock().free()
}
