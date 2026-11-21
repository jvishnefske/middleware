//! The board's network identity (D45), from its node id: the Ethernet
//! addresses of a USB CDC-NCM link (or a W5500) and the link-local IPv4
//! address the NCM link uses, so nothing is configured and every board
//! keeps its names across boots.
//!
//! * [`mac`] — a locally administered unicast address, `02:4d:` (the
//!   board's own, [`Side::Device`]) or `06:4d:` (the host's end of the NCM
//!   link, [`Side::Host`]) and four bytes of an FNV-1a-64 hash of the node
//!   id. Linux names the host's interface `enx064d<8 hex digits>`, which a
//!   NetworkManager or systemd-networkd rule matches (`docs/usb-net.md`).
//! * [`link_local`] — `169.254.a.b` with `a` and `b` in 1..=254 from the
//!   same hash: never 169.254.0/24 or 169.254.255/24 (RFC 3927 §2.1), never
//!   a `.0` or `.255` host byte. [`PREFIX_LEN`] is 16. The board does not
//!   probe (RFC 3927 §2.2); a host that finds the address taken picks
//!   another.
//! * [`seed`] — embassy-net's random seed (ephemeral ports, DHCP
//!   transaction ids, TCP initial sequence numbers if TCP is ever on): boot
//!   entropy from the chip (the RP2350's TRNG, the RP2040's ROSC random
//!   bit) mixed with the node id. The node id is public (it is in the mDNS
//!   TXT), so it never seeds on its own: there is no seed without entropy.

use middleware::SenderId;

/// The link-local prefix length (169.254/16).
pub const PREFIX_LEN: u8 = 16;

/// Which end of the link an Ethernet address is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// The board (embassy-net's address).
    Device,
    /// The host's end of the USB NCM link (`iMACAddress`).
    Host,
}

/// FNV-1a-64 of the node id.
const fn hash(node: SenderId) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut i = 0;
    while i < node.0.len() {
        h ^= node.0[i] as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
        i += 1;
    }
    h
}

/// The Ethernet address of `side` for node `node`.
pub const fn mac(node: SenderId, side: Side) -> [u8; 6] {
    let h = hash(node).to_be_bytes();
    let first = match side {
        Side::Device => 0x02,
        Side::Host => 0x06,
    };
    [first, 0x4D, h[0], h[1], h[2], h[3]]
}

/// The board's link-local IPv4 address, most significant octet first.
pub const fn link_local(node: SenderId) -> [u8; 4] {
    // 254 * 254 values; both bytes below 254 before the + 1.
    let n = (hash(node) % (254 * 254)) as u16;
    [169, 254, (1 + n / 254) as u8, (1 + n % 254) as u8]
}

/// The boot entropy [`seed`] takes: the ROSC random bit's bytes `rosc`,
/// mixed with the TRNG's `trng` when it delivered (the RP2350's; the shell
/// waits for it a bounded time and passes `None` if it did not finish, so a
/// TRNG fault never stops or resets the board).
pub const fn boot_entropy(trng: Option<[u8; 8]>, rosc: [u8; 8]) -> [u8; 8] {
    let Some(t) = trng else {
        return rosc;
    };
    let mut out = rosc;
    let mut i = 0;
    while i < 8 {
        out[i] ^= t[i];
        i += 1;
    }
    out
}

/// embassy-net's random seed from 8 bytes of boot `entropy` (the chip's
/// TRNG or ROSC), mixed with node `node` so two boards with a weak source
/// still differ. The entropy is what makes it unpredictable.
pub const fn seed(entropy: [u8; 8], node: SenderId) -> u64 {
    // The splitmix64 finalizer is a bijection, so for one node every
    // entropy value gives a distinct seed.
    mix(u64::from_le_bytes(entropy) ^ mix(hash(node)))
}

/// The splitmix64 finalizer.
const fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    /// A xorshift64 sequence of node ids.
    fn ids(n: usize) -> impl Iterator<Item = SenderId> {
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        (0..n).map(move |_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            SenderId(x.to_be_bytes())
        })
    }

    #[test]
    fn d45_link_local_addresses_avoid_the_reserved_ranges() {
        for id in ids(100_000).chain([SenderId([0; 8]), SenderId([0xFF; 8])]) {
            let [p, q, a, b] = link_local(id);
            assert_eq!((p, q), (169, 254), "{id:?}");
            assert!((1..=254).contains(&a), "{id:?}");
            assert!((1..=254).contains(&b), "{id:?}");
        }
        assert_eq!(PREFIX_LEN, 16);
    }

    #[test]
    fn d45_macs_are_locally_administered_unicast_and_differ_by_side() {
        for id in ids(100_000) {
            let dev = mac(id, Side::Device);
            let host = mac(id, Side::Host);
            for m in [dev, host] {
                assert_eq!(m[0] & 1, 0, "unicast {m:02x?}");
                assert_eq!(m[0] & 2, 2, "locally administered {m:02x?}");
                assert_eq!(m[1], 0x4D);
            }
            assert_eq!((dev[0], host[0]), (0x02, 0x06));
            assert_ne!(dev, host);
            assert_eq!(dev[2..], host[2..]);
        }
    }

    #[test]
    fn d45_the_identity_is_deterministic_and_pinned() {
        let id = SenderId([0xE6, 0x61, 0x38, 0x52, 0x83, 0x1F, 0x2A, 0x0B]);
        assert_eq!(mac(id, Side::Device), mac(id, Side::Device));
        assert_eq!(link_local(id), link_local(id));
        // FNV-1a-64 of the id: 0x79da_7a44_7ce8_8d4f (checked with Python).
        assert_eq!(mac(id, Side::Device), [0x02, 0x4D, 0x79, 0xDA, 0x7A, 0x44]);
        assert_eq!(mac(id, Side::Host), [0x06, 0x4D, 0x79, 0xDA, 0x7A, 0x44]);
        assert_eq!(link_local(id), GOLDEN_IP);
        // Neighbouring ids get other addresses.
        let other = SenderId([0xE6, 0x61, 0x38, 0x52, 0x83, 0x1F, 0x2A, 0x0C]);
        assert_ne!(link_local(other), link_local(id));
        assert_ne!(mac(other, Side::Device), mac(id, Side::Device));
    }

    #[test]
    fn d45_boot_entropy_mixes_both_sources_and_falls_back_to_the_rosc() {
        let rosc = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];
        let trng = [0xA5, 0x5A, 0xC3, 0x3C, 0x0F, 0xF0, 0x96, 0x69];
        // Without the TRNG, the ROSC's bytes.
        assert_eq!(boot_entropy(None, rosc), rosc);
        // With it, both count: changing either changes the result.
        let both = boot_entropy(Some(trng), rosc);
        assert_ne!(both, rosc);
        assert_ne!(both, trng);
        for byte in 0..8 {
            let mut t = trng;
            t[byte] ^= 1;
            assert_ne!(boot_entropy(Some(t), rosc), both, "trng byte {byte}");
            let mut r = rosc;
            r[byte] ^= 1;
            assert_ne!(boot_entropy(Some(trng), r), both, "rosc byte {byte}");
        }
        // A TRNG that gave zeros (or a stuck ROSC) leaves the other source.
        assert_eq!(boot_entropy(Some([0; 8]), rosc), rosc);
        assert_eq!(boot_entropy(Some(trng), [0; 8]), trng);
    }

    #[test]
    fn d45_the_seed_follows_the_boot_entropy_not_the_node_id() {
        let id = SenderId([0xE6, 0x61, 0x38, 0x52, 0x83, 0x1F, 0x2A, 0x0B]);
        let other = SenderId([0xE6, 0x61, 0x38, 0x52, 0x83, 0x1F, 0x2A, 0x0C]);
        let e = [0x5A, 0x17, 0xC3, 0x08, 0x9E, 0x44, 0xB1, 0x6D];
        // Deterministic in both inputs (the host can test it) ...
        assert_eq!(seed(e, id), seed(e, id));
        // ... the node id alone does not give it: not the public hash
        // (or a value derived from the id only) ...
        assert_ne!(seed(e, id), hash(id));
        assert_ne!(seed([0; 8], id), hash(id));
        // ... and every change of entropy gives another seed for the same
        // node (one bit, and 100 000 xorshift values, all distinct).
        for byte in 0..8 {
            for bit in 0..8 {
                let mut f = e;
                f[byte] ^= 1 << bit;
                assert_ne!(seed(f, id), seed(e, id), "byte {byte} bit {bit}");
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for x in ids(100_000) {
            assert!(seen.insert(seed(x.0, id)), "a repeated seed");
        }
        // Two boards with the same (weak) entropy still differ.
        assert_ne!(seed(e, other), seed(e, id));
    }

    /// 0x79da_7a44_7ce8_8d4f % (254 * 254) = 53731 = 211 * 254 + 137.
    const GOLDEN_IP: [u8; 4] = [169, 254, 212, 138];
}
