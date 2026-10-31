//! Checksums used on the wire.
//!
//! * [`crc16`] — CRC-16/CCITT-FALSE (poly `0x1021`, init `0xFFFF`), guarding
//!   each serial frame.
//! * [`Crc32`] — CRC-32/ISO-HDLC (the zlib/PNG CRC), computed incrementally
//!   over a whole payload image by the loader.
//! * [`crc8`] / [`crc8_update`] — CRC-8/SMBUS (poly `0x07`, init `0x00`, no
//!   reflection, no final XOR): the SMBus Packet Error Code (SMBus 2.0
//!   §7.4), guarding every block of the `smbus` transport.
//!
//! All are bitwise (no lookup table) to keep flash use small; the inner loop
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

/// Feeds `data` into a running CRC-8/SMBUS value `crc` and returns the new
/// value. Start from `0`; the result needs no final XOR. Feeding a message in
/// pieces gives the same value as feeding it whole, which is how the SMBus
/// PEC covers the address byte, the command, the count and the data without
/// copying them together.
pub const fn crc8_update(crc: u8, data: &[u8]) -> u8 {
    let mut crc = crc;
    let mut i = 0;
    while i < data.len() {
        crc ^= data[i];
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x07
            } else {
                crc << 1
            };
            bit += 1;
        }
        i += 1;
    }
    crc
}

/// CRC-8/SMBUS of `data`: the SMBus Packet Error Code.
pub const fn crc8(data: &[u8]) -> u8 {
    crc8_update(0, data)
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
        assert_eq!(crc8(b"123456789"), 0xF4);
    }

    #[test]
    fn m3_crc8_smbus_vectors() {
        // Single bytes: the polynomial itself, and an all-ones byte.
        assert_eq!(crc8(&[0x00]), 0x00);
        assert_eq!(crc8(&[0x01]), 0x07);
        assert_eq!(crc8(&[0xFF]), 0xF3);
        // A Block Write to 0x2A (address byte 0x54), command 0xC1, count 4,
        // data 01 02 03 04; and a 3-byte block to the general call address.
        // Values cross-checked with an independent implementation.
        assert_eq!(crc8(&[0x54, 0xC1, 4, 1, 2, 3, 4]), 0x3C);
        assert_eq!(crc8(&[0x00, 0xC1, 3, 0x2A, 0, 0x80]), 0x4A);
        // Incremental over pieces equals one shot.
        let pieces = crc8_update(
            crc8_update(crc8_update(0, &[0x54]), &[0xC1, 4]),
            &[1, 2, 3, 4],
        );
        assert_eq!(pieces, 0x3C);
        // Appending the PEC makes the CRC of the whole zero: how a receiver
        // can check it in one pass.
        assert_eq!(crc8(&[0x54, 0xC1, 4, 1, 2, 3, 4, 0x3C]), 0);
    }

    #[test]
    fn crc32_is_incremental() {
        let whole = Crc32::checksum(b"hello world");
        let parts = Crc32::new().update(b"hello ").update(b"world").finish();
        assert_eq!(whole, parts);
    }
}
