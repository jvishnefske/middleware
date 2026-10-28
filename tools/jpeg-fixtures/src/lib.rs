//! The fake camera's baked JPEG frames (`fixtures/cam`), and the generator
//! that writes them.
//!
//! The camera route of the ESP32-CAM streams baseline JPEG (what an OV2640
//! outputs). Until the real driver lands, the firmware and `payload-sim`
//! serve these frames instead: [`FRAMES`] frames of the moving
//! [`scene`] at each size in [`SIZES`], encoded by the integer-only
//! [`encoder`] (YCbCr 4:2:0, quality [`QUALITY`], no restart markers, no
//! APPn). Everything is deterministic, so regenerating yields identical
//! bytes; `tests/fixtures.rs` checks the committed files against a fresh
//! run.
//!
//! Layout: `fixtures/cam/jpeg<size>/frame_<nn>.jpg`, e.g.
//! `fixtures/cam/jpeg240/frame_00.jpg` … `frame_07.jpg`.

pub mod encoder;
pub mod scene;

use std::path::{Path, PathBuf};

/// Frames per loop.
pub const FRAMES: u32 = 8;

/// Square frame sizes, in pixels.
pub const SIZES: [u16; 2] = [240, 120];

/// JPEG quality (IJG scale).
pub const QUALITY: u8 = 60;

/// The fixture directory: `fixtures/cam` at the middleware crate root.
pub fn default_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/cam")
}

/// Path of frame `n` at `size`, relative to the fixture directory.
pub fn relative_path(size: u16, n: u32) -> PathBuf {
    PathBuf::from(format!("jpeg{size}/frame_{n:02}.jpg"))
}

/// Every fixture: relative path and JPEG bytes, sizes in [`SIZES`] order,
/// frames in order.
pub fn generate() -> Vec<(PathBuf, Vec<u8>)> {
    SIZES
        .iter()
        .flat_map(|&size| {
            (0..FRAMES).map(move |n| {
                let rgb = scene::render(n, FRAMES, size, size);
                // The scene always has `size * size` pixels.
                let jpg = encoder::encode(size, size, &rgb, QUALITY).unwrap_or_default();
                (relative_path(size, n), jpg)
            })
        })
        .collect()
}
