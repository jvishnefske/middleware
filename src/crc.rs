//! Checksums used on the wire.
//!
//! * [`crc16`] — CRC-16/CCITT-FALSE (poly `0x1021`, init `0xFFFF`), guarding
//!   each serial frame.
//! * [`Crc32`] — CRC-32/ISO-HDLC (the zlib/PNG CRC), computed incrementally
//!   over a whole payload image by the loader.
//!
//! Both are bitwise (no lookup table) to keep flash use small; the inner loop
//! runs a fixed eight iterations per byte.

/// CRC-16/CCITT-FALSE of `data`.
pub const fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    let mut i = 0;
    while i < data.len() {
        crc ^= (data[i] as u16) << 8;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
            bit += 1;
        }
        i += 1;
    }
    crc
}

/// Incremental CRC-32/ISO-HDLC. A value type: [`Crc32::update`] returns the
/// new state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Crc32(u32);

impl Crc32 {
    /// The initial state.
    pub const fn new() -> Self {
        Crc32(0xFFFF_FFFF)
    }

    /// Feeds `data` and returns the updated state.
    pub const fn update(self, data: &[u8]) -> Self {
        let mut crc = self.0;
        let mut i = 0;
        while i < data.len() {
            crc ^= data[i] as u32;
            let mut bit = 0;
            while bit < 8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
                bit += 1;
            }
            i += 1;
        }
        Crc32(crc)
    }

    /// The checksum of everything fed so far.
    pub const fn finish(self) -> u32 {
        !self.0
    }

    /// One-shot checksum of `data`.
    pub const fn checksum(data: &[u8]) -> u32 {
        Crc32::new().update(data).finish()
    }
}

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_check_values() {
        // Catalogue "check" values over the ASCII string "123456789".
        assert_eq!(crc16(b"123456789"), 0x29B1);
        assert_eq!(Crc32::checksum(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn crc32_is_incremental() {
        let whole = Crc32::checksum(b"hello world");
        let parts = Crc32::new().update(b"hello ").update(b"world").finish();
        assert_eq!(whole, parts);
    }
}
