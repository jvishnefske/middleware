//! Incremental baseline JPEG decoder for WASM payloads (`no_std`, no heap,
//! no floating point, no `unsafe`).
//!
//! Written for the conagotchi badge's camera viewer: a frame arrives over
//! ESP-NOW and is reassembled into a buffer in linear memory, and the
//! payload must turn it into RGB565 strips for the GC9A01 panel under
//! wasmi's per-call fuel budget. So the decoder works in small, bounded
//! steps and keeps all its state in one fixed-size struct:
//!
//! ```
//! # fn send(_y: u16, _h: u16, _w: u16, _px: &[u8]) {}
//! # fn frame() -> Vec<u8> { include_bytes!("../tests/fixtures/fuel.jpg").to_vec() }
//! use mw_jpeg::{Decoder, Rgb565Be, Scale, Step};
//!
//! let jpeg = frame(); // stays in the caller's buffer
//! let mut dec = Decoder::new_scaled(&jpeg, Scale::Full)?;
//! let mut strip = [0u8; 240 * 16 * 2]; // >= dec.strip_bytes::<Rgb565Be>()
//! loop {
//!     // One call per host callback: one block (plus the MCU's colour
//!     // conversion when it completes one) stays under ~42 000 fuel.
//!     match dec.decode_blocks::<Rgb565Be>(&jpeg, &mut strip, 1)? {
//!         Step::Partial => {}
//!         Step::Row(s) => send(s.y, s.height, s.width, &strip[..s.bytes(2)]),
//!         Step::Done => break,
//!     }
//! }
//! # Ok::<(), mw_jpeg::Error>(())
//! ```
//!
//! # Supported
//!
//! * Baseline sequential DCT (SOF0, and SOF1 within the same limits),
//!   Huffman coding, 8-bit samples, 8-bit quantisation tables.
//! * Grayscale (one component) and YCbCr (three components, luma first)
//!   with 4:4:4, 4:2:2 (h2v1) or 4:2:0 (h2v2) sampling: what the OV2640 and
//!   libjpeg-style encoders emit.
//! * Restart intervals (DRI / RST0..RST7), checked in sequence.
//! * Any size from 1×1 to [`MAX_DIMENSION`] square, including sizes that
//!   are not a multiple of the MCU (edge MCUs are cropped). A missing EOI is
//!   accepted.
//! * Output as [`Rgb565Be`] (high byte first, for SPI panels) or
//!   [`Rgb888`], at [`Scale::Full`] or [`Scale::Half`] (reduced 4×4 IDCT,
//!   e.g. 240×240 → 120×120).
//!
//! Progressive, lossless, hierarchical and arithmetic-coded files, 12-bit
//! samples, 16-bit quantisation tables, CMYK/four components, other
//! sampling factors and multi-scan sequential files return
//! [`Error::Unsupported`] from [`Decoder::new`]; syntax errors
//! [`Error::Malformed`]; data that ends early [`Error::Truncated`].
//!
//! # Method
//!
//! * Huffman decoding with an 8-bit lookahead table per table (module
//!   `huffman`); the bit reader (`bits`) handles byte stuffing, markers and
//!   the end of the data.
//! * Integer IDCT: libjpeg's `jidctint` LLM algorithm in 32-bit fixed point,
//!   skipping all-zero columns and rows, and a fill for DC-only blocks
//!   (`idct`); libjpeg's `jpeg_idct_4x4` for [`Scale::Half`]. Against
//!   libjpeg the full-scale output is bit-exact for 4:4:4 and grayscale.
//! * YCbCr → RGB in 16-bit fixed point ([`ycc_to_rgb`]). Chroma upsampling is
//!   replication (each chroma sample covers its 2×1 or 2×2 luma pixels), not
//!   libjpeg's "fancy" triangle filter: cheaper, and sharper at chroma edges.
//!
//! # Memory
//!
//! `size_of::<Decoder>()` is 4 352 bytes on wasm32 (checked at compile time
//! by the `fuel_probe` example): four Huffman tables of ~1 KB, four
//! quantisation tables of 64 bytes, the sample blocks of one MCU (384
//! bytes) and a few dozen bytes of state. It does not depend on the image
//! size: the input is borrowed per call and the output goes to the caller's
//! strip buffer of [`Decoder::strip_bytes`] (`width × 16 × 2` bytes for
//! 4:2:0 RGB565: 7 680 for 240 wide, 10 240 for 320; a quarter of that at
//! half scale). A decode call uses about 1 KiB of stack.
//! [`Decoder::IDLE`] is all zero bytes, so a decoder in a static costs
//! nothing in the `.wasm` file.
//!
//! # Code size
//!
//! About 14 KiB of wasm code (`opt-level = "s"`, set for this package in
//! the workspace manifest): the `fuel_probe` example is 18.2 KB of code
//! against 3.8 KB for the same payload without the decoder.
//!
//! # Work per call (fuel)
//!
//! Work is bounded per call by the `max_blocks` argument of
//! [`Decoder::decode_blocks`] (or `max_mcus` of [`Decoder::decode_mcus`]);
//! [`Decoder::decode_mcu_row`] does a whole MCU row. Every loop in the
//! decoder has a fixed bound (a block is at most 63 AC symbols, a call at
//! most its budget and never past the row, the marker walk at most the
//! input length), so a corrupt or truncated input ends in an error, never a
//! hang or a panic.
//!
//! wasmi charges about one unit of fuel per Wasm instruction. Measured on
//! the `fuel_probe` example (a 240×240 4:2:0 quality-80 frame with sensor-
//! like noise, 11 KB) with wasmi's eager translation:
//!
//! | step | full scale | half scale |
//! |---|---|---|
//! | [`Decoder::start`] (markers, tables) | 20 400 | 20 400 |
//! | one MCU (6 blocks + conversion), mean | 74 500 | 50 600 |
//! | one MCU, most detailed | 114 000 | 84 000 |
//! | one 240-wide MCU row (15 MCUs), mean / max | 1 047 000 / 1 199 000 | 711 000 / 832 000 |
//! | whole frame | 16.8 M | 11.4 M |
//! | `decode_blocks(.., 1)`, max | 41 200 | 18 800 |
//!
//! A block costs about 8 000 on average (7 600 with the 4×4 IDCT); the
//! colour conversion of a 4:2:0 MCU about 26 000 (6 500 at half scale).
//! 4:4:4 and 4:2:2 MCUs have 3 and 4 blocks and convert 64 and 128 pixels,
//! so they cost roughly half and two thirds of a 4:2:0 MCU.
//!
//! Smoother frames cost less: on the fake camera's 240×240 quality-60
//! frames (`fixtures/cam` in the parent repository) a 4:2:0 MCU takes about
//! 56 600 fuel (38 700 at half scale), a 240-wide row about 795 000
//! (544 000), and `decode_blocks(.., 1)` at most 37 000.
//!
//! wasmi's default lazy translation translates each function the first time
//! it is called and charges about 7 fuel per byte of code to that call:
//! about 36 000 extra on the first [`Decoder::start`], 33 000 on the first
//! decode call and 8 000 on the first MCU conversion. The middleware
//! runtime (`wasm-host`) translates eagerly when it loads a payload, so
//! there the first calls cost the same as later ones; a lazily translating
//! host with a 50 000-fuel budget needs a larger budget for the first
//! calls.
//!
//! # Limits
//!
//! [`MAX_DIMENSION`] caps width and height so a row is a bounded amount of
//! work and the strip size stays small; it does not affect the decoder's
//! size.
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![cfg_attr(
    not(test),
    deny(
        clippy::indexing_slicing,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic
    )
)]

mod bits;
mod color;
mod decoder;
mod error;
mod header;
mod huffman;
mod idct;

pub use color::{PixelFormat, Rgb565Be, Rgb888, ycc_to_rgb};
pub use decoder::{Decoder, Scale, Step, Strip};
pub use error::{Error, Malformed, Unsupported};
pub use header::{Header, Layout};

/// Largest accepted width and height in pixels.
pub const MAX_DIMENSION: u16 = 2048;

#[cfg(test)]
mod tests {
    #[test]
    fn decoder_size() {
        // Documented in the crate docs: 4 352 bytes on wasm32 (checked at
        // compile time by the `fuel_probe` example), 4 376 on a 64-bit host
        // (two `usize` fields and 8-byte alignment).
        let size = core::mem::size_of::<crate::Decoder>();
        if cfg!(target_pointer_width = "64") {
            assert_eq!(size, 4376);
        }
    }
}
