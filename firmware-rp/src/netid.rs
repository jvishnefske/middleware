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
//! * [`seed`] — embassy-net's random seed (TCP ports, DHCP transaction
//!   ids), from the node id: distinct per board, not secret.

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

/// embassy-net's random seed for node `node`.
pub const fn seed(node: SenderId) -> u64 {
    hash(node)
}

#[cfg(test)]
mod tests {
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
        assert_eq!(seed(id), 0x79DA_7A44_7CE8_8D4F);
        // Neighbouring ids get other addresses.
        let other = SenderId([0xE6, 0x61, 0x38, 0x52, 0x83, 0x1F, 0x2A, 0x0C]);
        assert_ne!(link_local(other), link_local(id));
        assert_ne!(mac(other, Side::Device), mac(id, Side::Device));
        assert_ne!(seed(other), seed(id));
    }

    /// 0x79da_7a44_7ce8_8d4f % (254 * 254) = 53731 = 211 * 254 + 137.
    const GOLDEN_IP: [u8; 4] = [169, 254, 212, 138];
}
