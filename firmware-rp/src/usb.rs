//! USB CDC-ACM framing rules and identity.
//!
//! The protocol runs over one CDC-ACM interface with 64-byte bulk packets
//! (full speed). A bulk IN transfer ends at a short packet: when a frame's
//! last packet is exactly 64 bytes, the host's read stays open until more
//! data arrives, and the frame's final `0x00` delimiter (and with it the
//! whole reply) would wait for the next frame. The link's writer therefore
//! ends a flushed write whose last packet was full with a zero-length
//! packet ([`needs_zlp`]).
//!
//! A CDC port that no host program has open is not read (Linux submits
//! bulk IN reads only while the tty is open), so every write to it blocks
//! until the link's send timeout. [`Host`] keeps the link from paying that
//! for each publication: after a send times out the host is `Away` and
//! publications are dropped at once, until a frame arrives from the host
//! (replies are always sent: a request proves someone is reading).
//!
//! Identity: Raspberry Pi's vendor id with the product id the Pico SDK uses
//! for its CDC stdio on each chip (so host rules written for Pico boards
//! match), manufacturer `middleware`, product `rp-base`, and the chip's
//! unique id as the serial number ([`crate::node::Serial`]).

/// Bulk packet size (USB full speed).
pub const MAX_PACKET: u16 = 64;

/// USB vendor id (Raspberry Pi).
pub const VID: u16 = 0x2E8A;

/// USB product id per chip: the Pico SDK's CDC stdio ids.
pub const fn pid(chip: crate::chip::Chip) -> u16 {
    match chip {
        crate::chip::Chip::Rp2040 => 0x000A,
        crate::chip::Chip::Rp2350 => 0x0009,
    }
}

/// Manufacturer string.
pub const MANUFACTURER: &str = "middleware";

/// Product string.
pub const PRODUCT: &str = "rp-base";

/// Whether a flush must send a zero-length packet: the last packet written
/// since the previous flush was a full one.
pub const fn needs_zlp(last_packet: usize, max_packet: u16) -> bool {
    last_packet == max_packet as usize
}

/// Whether the host is reading the CDC port, as the link last saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Host {
    /// Sends complete (the state at boot).
    Present,
    /// A send timed out and no frame has arrived since.
    Away,
}

impl Host {
    /// The state after a send that completed (`true`) or timed out.
    pub const fn after_send(self, completed: bool) -> Host {
        if completed { Host::Present } else { Host::Away }
    }

    /// The state after a frame arrived from the host.
    pub const fn after_receive(self) -> Host {
        Host::Present
    }

    /// Whether a publication is sent (`false`: dropped).
    pub const fn sends_publications(self) -> bool {
        matches!(self, Host::Present)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v10_a_frame_ending_on_a_full_packet_gets_a_zero_length_packet() {
        assert!(needs_zlp(64, MAX_PACKET));
        assert!(!needs_zlp(63, MAX_PACKET));
        assert!(!needs_zlp(0, MAX_PACKET), "nothing written since the last flush");
        assert_eq!(pid(crate::chip::Chip::Rp2040), 0x000A);
        assert_eq!(pid(crate::chip::Chip::Rp2350), 0x0009);
    }

    #[test]
    fn v10_publications_are_dropped_while_the_host_is_away() {
        let h = Host::Present;
        assert!(h.sends_publications());
        let away = h.after_send(false);
        assert_eq!(away, Host::Away);
        assert!(!away.sends_publications());
        assert_eq!(away.after_send(true), Host::Present);
        assert_eq!(away.after_receive(), Host::Present);
        assert!(away.after_receive().sends_publications());
    }
}
