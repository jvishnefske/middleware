//! ENET MAC register values: receive and transmit control for a link, the
//! enable, the station address and the descriptor triggers.
//!
//! RCR and TCR are written while the MAC is disabled (ECR.ETHEREN clear):
//! the TCR duplex bit only takes effect then, so a link that comes up at a
//! speed or duplex other than the one programmed is handled by disabling,
//! rewriting both and enabling again.

use super::ksz8081::{Duplex, Link, Speed};

/// ECR.RESET (bit 0): MAC reset, self-clearing.
pub const ECR_RESET: u32 = 1 << 0;
/// ECR.ETHEREN (bit 1): MAC enabled.
pub const ECR_ETHEREN: u32 = 1 << 1;
/// ECR.DBSWP (bit 8): descriptor byte swap, so the descriptors read as
/// little-endian words ([`super::desc`] assumes it).
pub const ECR_DBSWP: u32 = 1 << 8;
/// The ECR the shell writes to start the MAC: enabled, little-endian
/// descriptors (legacy descriptors: EN1588 clear).
pub const ECR_ENABLE: u32 = ECR_ETHEREN | ECR_DBSWP;
/// ECR reads the shell makes waiting for [`ECR_RESET`] to clear.
pub const RESET_POLL_BOUND: u32 = 100_000;

/// RDAR / TDAR bit 24: descriptors are ready, (re)start the DMA.
pub const DESCRIPTOR_ACTIVE: u32 = 1 << 24;

/// EIR.EBERR (bit 22): the DMA hit a bus error.
pub const EIR_EBERR: u32 = 1 << 22;
/// EIR every event bit (write to clear all).
pub const EIR_ALL: u32 = 0xFFFF_FFFF;

/// TFWR.STRFWD (bit 8): transmit store and forward.
pub const TFWR_STORE_AND_FORWARD: u32 = 1 << 8;
/// MRBR: the receive buffer size in bytes, a multiple of 16 at least as
/// big as [`MAX_FRAME_LEN`], so every frame fits one buffer.
pub const MRBR: u32 = 1536;
const _: () = assert!(MRBR.is_multiple_of(16) && MRBR >= MAX_FRAME_LEN);

/// The largest frame accepted (RCR.MAX_FL), with the CRC: 1518.
pub const MAX_FRAME_LEN: u32 = 1518;
/// RCR.MAX_FL shift (bits 29:16).
pub const RCR_MAX_FL_SHIFT: u32 = 16;
/// RCR.CRCFWD (bit 14): strip the received CRC.
pub const RCR_CRCFWD: u32 = 1 << 14;
/// RCR.RMII_10T (bit 9): 10 Mbps in RMII mode.
pub const RCR_RMII_10T: u32 = 1 << 9;
/// RCR.RMII_MODE (bit 8): RMII interface.
pub const RCR_RMII_MODE: u32 = 1 << 8;
/// RCR.PROM (bit 3): promiscuous, every frame received.
pub const RCR_PROM: u32 = 1 << 3;
/// RCR.MII_MODE (bit 2): MII or RMII mode (must be set).
pub const RCR_MII_MODE: u32 = 1 << 2;
/// RCR.DRT (bit 1): disable receive on transmit (half duplex).
pub const RCR_DRT: u32 = 1 << 1;
/// TCR.FDEN (bit 2): full duplex.
pub const TCR_FDEN: u32 = 1 << 2;

/// The RCR the probe ran: MAX_FL 1518, CRC stripped, RMII at 100 Mbps,
/// promiscuous, full duplex (`rcr(Link::FAST_FULL, true)`).
pub const RCR_PROBE: u32 = 0x05EE_410C;
/// The TCR the probe ran: full duplex.
pub const TCR_PROBE: u32 = TCR_FDEN;

/// The RCR for `link`: RMII, 1518-byte frames, CRC stripped, 10 Mbps
/// (`RMII_10T`) when the link is 10BASE-T, receive disabled while
/// transmitting when half duplex, and every frame received when
/// `promiscuous` (otherwise only the station address, broadcasts and the
/// hash-matched multicasts).
pub const fn rcr(link: Link, promiscuous: bool) -> u32 {
    let mut v = (MAX_FRAME_LEN << RCR_MAX_FL_SHIFT) | RCR_CRCFWD | RCR_RMII_MODE | RCR_MII_MODE;
    if matches!(link.speed, Speed::Mbps10) {
        v |= RCR_RMII_10T;
    }
    if matches!(link.duplex, Duplex::Half) {
        v |= RCR_DRT;
    }
    if promiscuous {
        v |= RCR_PROM;
    }
    v
}

/// The TCR for `link`: FDEN when full duplex.
pub const fn tcr(link: Link) -> u32 {
    match link.duplex {
        Duplex::Full => TCR_FDEN,
        Duplex::Half => 0,
    }
}

/// PAUR's type field (bits 15:0): the PAUSE frame EtherType 0x8808.
pub const PAUR_TYPE: u32 = 0x8808;

/// The (PALR, PAUR) values for the station address `mac`: PALR the first
/// four bytes, PAUR the last two in bits 31:16 with [`PAUR_TYPE`].
pub const fn station_address(mac: [u8; 6]) -> (u32, u32) {
    let palr = u32::from_be_bytes([mac[0], mac[1], mac[2], mac[3]]);
    let paur = ((mac[4] as u32) << 24) | ((mac[5] as u32) << 16) | PAUR_TYPE;
    (palr, paur)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fl3_rcr_tcr_match_probe_for_100_full() {
        // Probe: (1518 << 16) | (1 << 14) | (1 << 8) | (1 << 2) | (1 << 3).
        assert_eq!(RCR_PROBE, (1518 << 16) | (1 << 14) | (1 << 8) | (1 << 2) | (1 << 3));
        assert_eq!(rcr(Link::FAST_FULL, true), RCR_PROBE);
        assert_eq!(tcr(Link::FAST_FULL), TCR_PROBE);
        assert_eq!(TCR_PROBE, 1 << 2);
    }

    #[test]
    fn fl3_rcr_tcr_for_each_link() {
        let base = RCR_PROBE & !RCR_PROM;
        let l = |speed, duplex| Link { speed, duplex };
        assert_eq!(rcr(l(Speed::Mbps100, Duplex::Full), false), base);
        assert_eq!(rcr(l(Speed::Mbps100, Duplex::Half), false), base | RCR_DRT);
        assert_eq!(rcr(l(Speed::Mbps10, Duplex::Full), false), base | RCR_RMII_10T);
        assert_eq!(
            rcr(l(Speed::Mbps10, Duplex::Half), false),
            base | RCR_RMII_10T | RCR_DRT
        );
        assert_eq!(tcr(l(Speed::Mbps10, Duplex::Half)), 0);
        assert_eq!(tcr(l(Speed::Mbps10, Duplex::Full)), TCR_FDEN);
    }

    #[test]
    fn fl3_mac_control_values() {
        // Probe: ECR (1 << 1) | (1 << 8), RDAR/TDAR 1 << 24, TFWR 1 << 8.
        assert_eq!(ECR_ENABLE, 0x0102);
        assert_eq!(ECR_RESET, 1);
        assert_eq!(DESCRIPTOR_ACTIVE, 0x0100_0000);
        assert_eq!(TFWR_STORE_AND_FORWARD, 0x100);
        assert_eq!(MRBR, 1536);
        assert_eq!(EIR_EBERR, 1 << 22);
    }

    #[test]
    fn fl3_station_address_matches_probe() {
        let mac = [0x02, 0x4D, 0x57, 0x52, 0x54, 0x01];
        assert_eq!(station_address(mac), (0x024D_5752, 0x5401_8808));
    }
}
