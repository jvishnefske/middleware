//! The device's identity, from the RT1050's 64-bit unique id.
//!
//! The unique id is fused in OCOTP's `CFG0` (fuse word 0x410, the low 32
//! bits) and `CFG1` (0x420, the high 32 bits); the shell reads their
//! shadow registers once at boot, after enabling OCOTP's clock gate. The
//! id as a big-endian `u64` (`CFG1 << 32 | CFG0`, the order NXP's tools
//! print) becomes:
//!
//! * the node id at the end of `sys/info` ([`node_id`]), used whole, as on
//!   the RP base firmware;
//! * the boot log's and (with USB) the serial number's 16 upper-case hex
//!   digits ([`Serial`]).

use middleware::SenderId;

/// The unique id's 8 bytes from the two fuse words, most significant first.
pub const fn unique_id(cfg0: u32, cfg1: u32) -> [u8; 8] {
    let v = ((cfg1 as u64) << 32) | cfg0 as u64;
    v.to_be_bytes()
}

/// The node id `sys/info` reports for a chip with unique id `uid`.
pub const fn node_id(uid: [u8; 8]) -> SenderId {
    SenderId(uid)
}

/// The unique id as 16 upper-case hex digits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Serial([u8; 16]);

impl Serial {
    /// Formats `uid`.
    pub const fn new(uid: [u8; 8]) -> Self {
        const HEX: &[u8; 16] = b"0123456789ABCDEF";
        let mut out = [0u8; 16];
        let mut i = 0;
        while i < 8 {
            out[2 * i] = HEX[(uid[i] >> 4) as usize];
            out[2 * i + 1] = HEX[(uid[i] & 0x0F) as usize];
            i += 1;
        }
        Serial(out)
    }

    /// The digits as a string.
    pub fn as_str(&self) -> &str {
        // Always ASCII hex digits; the fallback is never taken.
        core::str::from_utf8(&self.0).unwrap_or("0000000000000000")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rt12_node_id_is_the_fused_unique_id_high_word_first() {
        let uid = unique_id(0x8312_1F2A, 0xE661_3852);
        assert_eq!(uid, [0xE6, 0x61, 0x38, 0x52, 0x83, 0x12, 0x1F, 0x2A]);
        assert_eq!(node_id(uid), SenderId(uid));
        assert_eq!(Serial::new(uid).as_str(), "E661385283121F2A");
        assert_eq!(Serial::new(unique_id(0, 0)).as_str(), "0000000000000000");
    }
}
