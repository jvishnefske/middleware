//! The device's identity, from the chip's 64-bit unique id.
//!
//! The shell reads the id once at boot: on the RP2040 the QSPI flash
//! chip's unique id (`Flash::blocking_unique_id`, 8 bytes), on the RP2350
//! the chip id in OTP (`otp::get_chipid`, sent big-endian). The same 8 bytes
//! become:
//!
//! * the node id at the end of `sys/info` ([`node_id`]): a
//!   [`SenderId`] is 8 bytes, so the id is used whole (the ESP base
//!   firmware pads its 6-byte MAC with two zeros);
//! * the USB serial number string ([`Serial`]), 16 upper-case hex digits,
//!   so the CDC port shows up as
//!   `/dev/serial/by-id/usb-middleware_rp-base_<id>-if00` and two boards are
//!   told apart without probing.

use middleware::SenderId;

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
    fn v1_node_id_and_usb_serial_come_from_the_unique_id() {
        let uid = [0xE6, 0x61, 0x38, 0x52, 0x83, 0x1F, 0x2A, 0x0B];
        assert_eq!(node_id(uid), SenderId(uid));
        assert_eq!(Serial::new(uid).as_str(), "E6613852831F2A0B");
        assert_eq!(Serial::new([0; 8]).as_str(), "0000000000000000");
    }
}
