//! Incremental SHA-256 as a `Copy` value (feature `trust`).
//!
//! [`ImageHash`] hashes a payload image chunk by chunk as the loader
//! receives it, so the commit can check a signature over the image digest
//! without reading the image back. It is a plain value (eight state words,
//! one 64-byte block buffer and a byte count, 108 bytes) so
//! [`super::Loader`] stays `Copy` and `Eq` like the rest of the loader
//! state; [`ImageHash::update`] takes and returns it, as
//! [`crate::crc::Crc32::update`] does.
//!
//! The block function is the audited `sha2::compress256` (RustCrypto);
//! this module only buffers partial blocks and applies the FIPS 180-4
//! padding (`0x80`, zeros, the 64-bit big-endian bit length). The tests
//! compare it with `sha2::Sha256` at every length across several block
//! boundaries and with every way of splitting the input.

use sha2::digest::generic_array::GenericArray;

/// Bytes per SHA-256 block.
const BLOCK: usize = 64;

/// FIPS 180-4 initial hash value.
const H0: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

/// SHA-256 state over the bytes seen so far.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ImageHash {
    state: [u32; 8],
    block: [u8; BLOCK],
    /// Bytes buffered in `block` (always `< BLOCK` between calls).
    filled: usize,
    /// Total bytes hashed.
    total: u64,
}

impl core::fmt::Debug for ImageHash {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ImageHash").field("bytes", &self.total).finish_non_exhaustive()
    }
}

impl Default for ImageHash {
    fn default() -> Self {
        Self::new()
    }
}

fn compress(state: &mut [u32; 8], block: [u8; BLOCK]) {
    sha2::compress256(state, &[GenericArray::from(block)]);
}

impl ImageHash {
    /// The hash of no bytes.
    pub const fn new() -> Self {
        ImageHash { state: H0, block: [0; BLOCK], filled: 0, total: 0 }
    }

    /// Bytes hashed so far.
    pub const fn len(&self) -> u64 {
        self.total
    }

    /// Whether nothing has been hashed.
    pub const fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// The state after also hashing `data`. One pass over `data`: at most
    /// `data.len() / 64 + 1` block compressions.
    pub fn update(mut self, data: &[u8]) -> Self {
        self.total = self.total.wrapping_add(data.len() as u64);
        let mut rest = data;
        if self.filled > 0 {
            let take = (BLOCK - self.filled).min(rest.len());
            let (head, tail) = rest.split_at(take);
            if let Some(dst) = self.block.get_mut(self.filled..self.filled + take) {
                dst.copy_from_slice(head);
            }
            self.filled += take;
            rest = tail;
            if self.filled < BLOCK {
                return self;
            }
            compress(&mut self.state, self.block);
            self.filled = 0;
        }
        let mut blocks = rest.chunks_exact(BLOCK);
        for b in blocks.by_ref() {
            if let Ok(block) = <[u8; BLOCK]>::try_from(b) {
                compress(&mut self.state, block);
            }
        }
        // Bytes past `filled` are kept zero, so equal inputs give equal
        // (derived `Eq`) states however they were split.
        let tail = blocks.remainder();
        self.block = [0; BLOCK];
        if let Some(dst) = self.block.get_mut(..tail.len()) {
            dst.copy_from_slice(tail);
        }
        self.filled = tail.len();
        self
    }

    /// The SHA-256 digest of everything hashed.
    pub fn finish(self) -> [u8; 32] {
        let mut state = self.state;
        let mut block = self.block;
        let filled = self.filled.min(BLOCK - 1);
        if let Some(b) = block.get_mut(filled) {
            *b = 0x80;
        }
        if let Some(rest) = block.get_mut(filled + 1..) {
            rest.fill(0);
        }
        if filled + 1 > BLOCK - 8 {
            compress(&mut state, block);
            block = [0; BLOCK];
        }
        let bits = self.total.wrapping_mul(8).to_be_bytes();
        if let Some(len) = block.get_mut(BLOCK - 8..) {
            len.copy_from_slice(&bits);
        }
        compress(&mut state, block);
        let mut out = [0u8; 32];
        for (dst, word) in out.chunks_exact_mut(4).zip(state) {
            dst.copy_from_slice(&word.to_be_bytes());
        }
        out
    }

    /// SHA-256 of `data` in one call.
    pub fn digest(data: &[u8]) -> [u8; 32] {
        Self::new().update(data).finish()
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use sha2::{Digest, Sha256};
    use std::vec::Vec;

    fn reference(data: &[u8]) -> [u8; 32] {
        Sha256::digest(data).into()
    }

    #[test]
    fn known_answers() {
        // FIPS 180-2 examples.
        assert_eq!(
            ImageHash::digest(b""),
            reference(b""),
        );
        let abc = ImageHash::digest(b"abc");
        assert_eq!(abc[..4], [0xba, 0x78, 0x16, 0xbf]);
        assert_eq!(abc[28..], [0xf2, 0x00, 0x15, 0xad]);
        let two = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
        assert_eq!(ImageHash::digest(two), reference(two));
    }

    #[test]
    fn matches_sha2_at_every_length_and_split() {
        let data: Vec<u8> = (0..300u32).map(|i| (i * 7 + 3) as u8).collect();
        for len in 0..data.len() {
            let whole = &data[..len];
            let expect = reference(whole);
            assert_eq!(ImageHash::digest(whole), expect, "len {len}");
            for split in [1usize, 3, 55, 56, 63, 64, 65, 127, 128] {
                let h = whole.chunks(split).fold(ImageHash::new(), ImageHash::update);
                assert_eq!(h.len(), len as u64);
                assert_eq!(h.finish(), expect, "len {len} split {split}");
            }
        }
    }

    #[test]
    fn equal_inputs_give_equal_states() {
        let a = ImageHash::new().update(b"hello ").update(b"world");
        let b = ImageHash::new().update(b"hello world");
        assert_eq!(a, b);
        let long: Vec<u8> = (0..200u8).collect();
        let x = long.chunks(70).fold(ImageHash::new(), ImageHash::update);
        let y = long.chunks(13).fold(ImageHash::new(), ImageHash::update);
        assert_eq!(x, y);
        assert_ne!(a, ImageHash::new().update(b"hello worle"));
        assert!(ImageHash::new().is_empty());
        assert_eq!(ImageHash::default(), ImageHash::new());
        assert!(std::format!("{a:?}").contains("11"));
    }
}
