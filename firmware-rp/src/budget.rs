//! The payload heap budget (requirement W10 on the RP2350).
//!
//! The controller admits a payload only when the free heap covers
//! `wasm_host::Limits::heap_needed` of its image plus esp-base's
//! [`HEAP_MARGIN`](esp_base::start::HEAP_MARGIN) ([`esp_base::start`],
//! the same two-step check as the ESP base firmware: before reading the
//! image, and with it read). This module gives the limits the firmware runs
//! payloads under ([`limits`]) and, for documentation and tests, the
//! largest image a heap of a given size admits ([`largest_image`]).
//!
//! Nothing but the interpreter allocates, so the heap free at a start is
//! the whole arena ([`crate::chip::Profile::heap`]) less the allocator's
//! bookkeeping, which [`HEAP_MARGIN`](esp_base::start::HEAP_MARGIN)
//! covers.

use esp_base::start::check_heap;
use wasm_host::Limits;

use crate::chip::Profile;

/// The limits payloads run under on `profile`: its memory cap, the ABI's
/// fuel per call and wasm-host's default stacks.
pub const fn limits(profile: &Profile) -> Limits {
    Limits::bytes(profile.max_memory_bytes).with_fuel(middleware::wasm_abi::FUEL_PER_CALL)
}

/// The largest image (up to `profile.max_image`) whose worst-case bound,
/// with linear memory charged at `memory` bytes (the module's declared
/// maximum, at most the cap), passes the heap check with `free` bytes free.
/// 0 if not even an empty image fits.
pub fn largest_image(profile: &Profile, free: usize, memory: u32) -> u32 {
    let limits = Limits { max_memory_bytes: memory.min(profile.max_memory_bytes), ..limits(profile) };
    let fits = |len: u32| check_heap(free, limits.load_heap_needed(len)).is_ok();
    if !fits(0) {
        return 0;
    }
    // Binary search over 0..=max_image: at most 33 steps (the bound grows
    // with the image length).
    let (mut lo, mut hi) = (0u32, profile.max_image);
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if fits(mid) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chip::{RP2350, WASM_PAGE};

    /// The bound scales with the pointer width: the host's 64-bit bound is
    /// larger than the RP2350's 32-bit one, so what it admits the target
    /// admits too (on the host at least 40 KiB either way). On 32 bits the
    /// whole 64 KiB cap is admitted: a 64 KiB image with 128 KiB of memory
    /// needs 344 KiB while it runs, 296 KiB while it compiles.
    #[test]
    fn v7_the_rp2350_heap_admits_images_up_to_its_cap() {
        let heap = RP2350.heap;
        // A byte-page payload (16 KiB memory, the build-byte-pages default)
        // and a 64 KiB-page payload charged at the full 128 KiB cap.
        for memory in [16 * 1024, 2 * WASM_PAGE] {
            let largest = largest_image(&RP2350, heap, memory);
            assert!(largest >= 40 * 1024, "{memory}: {largest}");
            if cfg!(target_pointer_width = "32") {
                assert_eq!(largest, RP2350.max_image);
            }
        }
        // blink and echo (under 4 KiB) leave most of the heap free.
        let small = limits(&RP2350).load_heap_needed(4 * 1024);
        assert!(check_heap(heap, small).is_ok() && small < 256 * 1024, "{small}");
    }

    #[test]
    fn v7_a_smaller_heap_admits_less() {
        let heap = 240 * 1024;
        let small = largest_image(&RP2350, heap, 2 * WASM_PAGE);
        assert!(small > 0 && small < RP2350.max_image, "{small}");
        let bound = limits(&RP2350).load_heap_needed(small);
        assert!(check_heap(heap, bound).is_ok());
        assert!(check_heap(heap, limits(&RP2350).load_heap_needed(small + 1)).is_err());
        assert_eq!(largest_image(&RP2350, 1024, 16 * 1024), 0);
    }
}
