//! Puts the chip's `memory.x` where cortex-m-rt's `link.x` finds it.
//!
//! `memory-rp2040.x` or `memory-rp2350.x` is copied to `$OUT_DIR/memory.x`
//! for the enabled chip feature. Host builds (no chip feature) copy nothing.
//! The FLASH lengths in both files are the firmware region of the flash map
//! in `src/layout.rs`; a unit test (`layout::tests`) keeps them equal.

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=memory-rp2040.x");
    println!("cargo:rerun-if-changed=memory-rp2350.x");
    let rp2040 = env::var_os("CARGO_FEATURE_RP2040").is_some();
    let rp2350 = env::var_os("CARGO_FEATURE_RP2350").is_some();
    let source = match (rp2040, rp2350) {
        (true, false) => "memory-rp2040.x",
        (false, true) => "memory-rp2350.x",
        // No chip (host tests) or both (refused by a compile_error! in
        // main.rs): nothing to link.
        _ => return,
    };
    let Some(out) = env::var_os("OUT_DIR").map(PathBuf::from) else {
        return;
    };
    if let Err(e) = fs::copy(source, out.join("memory.x")) {
        println!("cargo:warning=copying {source}: {e}");
        return;
    }
    println!("cargo:rustc-link-search={}", out.display());
}
