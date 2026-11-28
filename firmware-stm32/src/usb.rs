//! USB identity.

/// pid.codes' open-source VID.
pub const VID: u16 = 0x1209;

/// pid.codes test PID: for bench use, shared by other projects, so hosts
/// also match [`PRODUCT`].
pub const PID: u16 = 0x0001;

/// Manufacturer string.
pub const MANUFACTURER: &str = "middleware";

/// Product string; the HIL suite matches it on the shared test PID.
pub const PRODUCT: &str = "stm32-base";

/// Full-speed bulk packet size.
pub const MAX_PACKET: u16 = 64;

/// Whether a transfer whose last packet held `last` bytes needs a
/// zero-length packet to end it (it filled a whole packet).
pub const fn needs_zlp(last: usize, max_packet: u16) -> bool {
    last != 0 && last == max_packet as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_full_last_packet_needs_a_zlp() {
        assert!(needs_zlp(64, MAX_PACKET));
        assert!(!needs_zlp(63, MAX_PACKET));
        assert!(!needs_zlp(0, MAX_PACKET));
    }
}
