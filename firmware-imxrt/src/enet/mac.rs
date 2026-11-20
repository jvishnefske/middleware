//! ENET MAC register values: receive and transmit control for a link, the
//! enable, the station address, the multicast group hash filter and the
//! descriptor triggers.
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

/// The station address of the chip with unique id `uid`
/// ([`crate::node::unique_id`]): a locally administered unicast address,
/// `0x02` then the unique id's last five bytes (the ones that differ most
/// between chips of a lot). Deterministic, so the board keeps its address
/// (and its DHCP lease) across boots, and the Zenoh management gate can
/// name the board by it.
pub const fn mac_from_uid(uid: [u8; 8]) -> [u8; 6] {
    [0x02, uid[3], uid[4], uid[5], uid[6], uid[7]]
}

/// Reflected CRC-32 (polynomial 0xEDB8_8320, initial value all ones) of
/// `data`, bytes taken least significant bit first, **without** the final
/// inversion: the register the ENET (and Linux's `ether_crc_le`) hashes
/// multicast addresses with. The Ethernet FCS and zlib's `crc32` are its
/// complement.
pub const fn crc32_le(data: &[u8]) -> u32 {
    let mut crc = !0u32;
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
    crc
}

/// The (GAUR, GALR) bit the ENET's group hash filter checks for frames to
/// the multicast address `mac`: the top six bits `h` of [`crc32_le`] of
/// the address in wire order; bit 5 of `h` selects GAUR (set) or GALR
/// (clear), and bit `h & 0x1F` of it is set (as Linux `fec_main.c` and
/// NXP `fsl_enet.c` do).
pub const fn group_hash(mac: [u8; 6]) -> (u32, u32) {
    let h = crc32_le(&mac) >> 26;
    // Masked: a shift by 32 or more would be lost on the Cortex-M7.
    let bit = 1u32 << (h & 0x1F);
    if h & 0x20 != 0 { (bit, 0) } else { (0, bit) }
}

/// The (GAUR, GALR) values that admit every address of `macs`: the union
/// of their [`group_hash`] bits. Other groups that hash to the same bits
/// are admitted too; smoltcp drops them.
pub const fn group_filter(macs: &[[u8; 6]]) -> (u32, u32) {
    let (mut gaur, mut galr) = (0, 0);
    let mut i = 0;
    while i < macs.len() {
        let (u, l) = group_hash(macs[i]);
        gaur |= u;
        galr |= l;
        i += 1;
    }
    (gaur, galr)
}

/// The Ethernet address of the IPv4 multicast group `g` (RFC 1112 §6.4):
/// `01:00:5e` and the group's low 23 bits.
pub const fn multicast_mac_v4(g: [u8; 4]) -> [u8; 6] {
    [0x01, 0x00, 0x5E, g[1] & 0x7F, g[2], g[3]]
}

/// 224.0.0.1, all hosts: IGMP general queries come to it, so the board
/// hears them and keeps reporting its memberships (without them an IGMP
/// snooping switch ages the mDNS membership out after a few minutes).
pub const ALL_HOSTS_MAC: [u8; 6] = multicast_mac_v4([224, 0, 0, 1]);
/// 224.0.0.251, mDNS.
pub const MDNS_GROUP_MAC: [u8; 6] = multicast_mac_v4([224, 0, 0, 251]);
/// The (GAUR, GALR) the shell writes: the all-hosts and mDNS groups.
pub const MULTICAST_FILTER: (u32, u32) = group_filter(&[ALL_HOSTS_MAC, MDNS_GROUP_MAC]);

#[cfg(test)]
mod tests {
    use super::*;

    /// A 256-entry table CRC, built here: an independent formulation of
    /// Linux `crc32_le(~0, data, len)`, byte-wise.
    fn table_crc(data: &[u8]) -> u32 {
        let mut table = [0u32; 256];
        for (i, t) in table.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    (c >> 1) ^ 0xEDB8_8320
                } else {
                    c >> 1
                };
            }
            *t = c;
        }
        data.iter().fold(!0u32, |crc, &b| {
            (crc >> 8) ^ table[((crc ^ u32::from(b)) & 0xFF) as usize]
        })
    }

    /// `n` MACs from a linear congruential generator, multicast bit set.
    fn macs(n: usize) -> impl Iterator<Item = [u8; 6]> {
        let mut x: u64 = 0x1234_5678_9ABC_DEF0;
        (0..n).map(move |_| {
            x = x
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let b = x.to_be_bytes();
            [b[0] | 1, b[1], b[2], b[3], b[4], b[5]]
        })
    }

    #[test]
    fn d40_crc32_le_engine() {
        // The standard CRC-32 check value is the complement of the register.
        assert_eq!(!crc32_le(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32_le(&[]), 0xFFFF_FFFF);
    }

    #[test]
    fn d40_group_hash_mdns_vector() {
        // Checked with zlib: ~crc32(01005e0000fb) = 0x84dcdefc, h = 33.
        assert_eq!(crc32_le(&MDNS_GROUP_MAC), 0x84DC_DEFC);
        assert_eq!(group_hash(MDNS_GROUP_MAC), (0x0000_0002, 0));
        // ~crc32(01005e000001) = 0xd9b4c5fe, h = 54.
        assert_eq!(crc32_le(&ALL_HOSTS_MAC), 0xD9B4_C5FE);
        assert_eq!(group_hash(ALL_HOSTS_MAC), (0x0040_0000, 0));
    }

    #[test]
    fn d40_multicast_mac_mapping() {
        assert_eq!(MDNS_GROUP_MAC, [0x01, 0x00, 0x5E, 0x00, 0x00, 0xFB]);
        assert_eq!(ALL_HOSTS_MAC, [0x01, 0x00, 0x5E, 0x00, 0x00, 0x01]);
        // The second octet's top bit is not mapped.
        assert_eq!(
            multicast_mac_v4([239, 129, 1, 2]),
            [0x01, 0x00, 0x5E, 0x01, 0x01, 0x02]
        );
    }

    #[test]
    fn d40_filter_is_union() {
        assert_eq!(MULTICAST_FILTER, (0x0040_0002, 0));
        assert_eq!(group_filter(&[]), (0, 0));
        assert_eq!(group_filter(&[MDNS_GROUP_MAC]), group_hash(MDNS_GROUP_MAC));
    }

    #[test]
    fn d40_one_bit_and_independent_formulation() {
        for mac in macs(512) {
            assert_eq!(crc32_le(&mac), table_crc(&mac), "{mac:02x?}");
            let (gaur, galr) = group_hash(mac);
            assert_eq!(gaur.count_ones() + galr.count_ones(), 1, "{mac:02x?}");
            let h = table_crc(&mac) >> 26;
            let want = if h >= 32 {
                (1 << (h - 32), 0)
            } else {
                (0, 1 << h)
            };
            assert_eq!((gaur, galr), want, "{mac:02x?}");
        }
    }

    #[test]
    fn d40_bit5_selects_register() {
        let with = |want: u32| macs(1 << 16).find(|m| crc32_le(m) >> 26 == want).unwrap();
        assert_eq!(group_hash(with(31)), (0, 1 << 31));
        assert_eq!(group_hash(with(32)), (1, 0));
        assert_eq!(group_hash(with(63)), (1 << 31, 0));
        assert_eq!(group_hash(with(0)), (0, 1));
    }

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

    #[test]
    fn fl3_mac_from_uid_is_local_unicast() {
        let uid = [0xE6, 0x61, 0x38, 0x52, 0x83, 0x12, 0x1F, 0x2A];
        let mac = mac_from_uid(uid);
        assert_eq!(mac, [0x02, 0x52, 0x83, 0x12, 0x1F, 0x2A]);
        // Locally administered (bit 1), unicast (bit 0 clear), any uid.
        for uid in [[0u8; 8], [0xFF; 8], uid] {
            let m = mac_from_uid(uid);
            assert_eq!(m[0] & 0b11, 0b10);
        }
        assert_ne!(mac_from_uid([0, 0, 0, 0, 0, 0, 0, 1]), mac_from_uid([0, 0, 0, 0, 0, 0, 0, 2]));
    }
}
