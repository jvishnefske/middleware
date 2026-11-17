//! Puts `memory.x` (the DTCM layout) where cortex-m-rt's `link.x` finds it.
fn main() {
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    std::fs::copy("memory.x", out.join("memory.x")).expect("memory.x");
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rustc-link-arg-bins=--nmagic");
    println!("cargo:rustc-link-arg-bins=-Tlink.x");
    println!("cargo:rerun-if-changed=memory.x");
}
