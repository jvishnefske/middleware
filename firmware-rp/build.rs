//! Puts the chip's `memory.x` where cortex-m-rt's `link.x` finds it, and
//! on the RP2350 generates the image's boot metadata.
//!
//! `memory-rp2040.x` or `memory-rp2350.x` is copied to `$OUT_DIR/memory.x`
//! for the enabled chip feature. Host builds (no chip feature) copy nothing.
//! The FLASH lengths in both files are the firmware region of the flash map
//! in `src/layout.rs`; a unit test (`layout::tests`) keeps them equal.
//!
//! For the RP2350, `$OUT_DIR/rp2350-image.x` (included by `memory-rp2350.x`)
//! places the IMAGE_DEF and sentinel blocks of `src/ota/image.rs`, which
//! this script compiles on its own, with the version from the environment
//! variable `RP_BASE_FW_VERSION` (`MAJOR.MINOR`, what `build.sh --version`
//! sets) or else the crate's `major.minor`. The version is passed to the
//! firmware as `RP_BASE_IMAGE_VERSION` (what `sys/info` reports on the
//! RP2350). A malformed version stops the build.

use std::env;
use std::fs;
use std::path::PathBuf;

#[allow(dead_code)]
#[path = "src/ota/image.rs"]
mod image;

fn version() -> Result<image::FwVersion, String> {
    let given = env::var("RP_BASE_FW_VERSION").ok().filter(|v| !v.is_empty());
    let text = match given {
        Some(v) => v,
        None => format!(
            "{}.{}",
            env::var("CARGO_PKG_VERSION_MAJOR").unwrap_or_default(),
            env::var("CARGO_PKG_VERSION_MINOR").unwrap_or_default()
        ),
    };
    image::parse_version(&text).ok_or_else(|| format!("RP_BASE_FW_VERSION {text:?} is not MAJOR.MINOR (each 0..=65535)"))
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/ota/image.rs");
    println!("cargo:rerun-if-changed=memory-rp2040.x");
    println!("cargo:rerun-if-changed=memory-rp2350.x");
    println!("cargo:rerun-if-env-changed=RP_BASE_FW_VERSION");
    let version = match version() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    println!("cargo:rustc-env=RP_BASE_IMAGE_VERSION={version}");
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
    if rp2350 {
        let mut script = String::new();
        if image::write_linker_script(version, &mut script).is_err()
            || fs::write(out.join("rp2350-image.x"), script).is_err()
        {
            eprintln!("error: cannot write rp2350-image.x");
            std::process::exit(1);
        }
    }
    println!("cargo:rustc-link-search={}", out.display());
}
