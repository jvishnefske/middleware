//! Links the esp-hal linker scripts into the firmware binary only when
//! building for an ESP target; host builds of the pure-core library (unit
//! tests) are unaffected.

fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.ends_with("-none-elf") {
        println!("cargo:rustc-link-arg-bins=-Tlinkall.x");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
