//! The RP board's mDNS identity (D47): what it announces as its `mw/node`
//! record ([`middleware::mdns::node`]) on a network link (USB CDC-NCM or a
//! W5500), over the boards' shared decisions
//! ([`middleware::mdns::responder`], D46, re-exported here).
//!
//! * [`identity`] — instance `rp-<node id, 16 lower-case hex digits>`, the
//!   serial beacons' instance ([`crate::beacons::Identity::instance`]), so
//!   the gateway and mDNS name the board alike; host `<label>.local`.
//! * [`board`] — TXT `chip` (`rp2040`, `rp2350`) and `board`, which names
//!   the link too: `pico-usb`, `pico2-usb`, `pico-w5500`, `pico2-w5500`.
//! * [`wanted`] — what to announce: the interface address, but only while
//!   the link is up (a static link-local address is configured before any
//!   host is there; probing then would go nowhere). The RP boards have no
//!   Zenoh client yet, so no `zenoh` and no `sender`.
//! * [`txt`] — the record's TXT with the firmware version.

use heapless::Vec;
use middleware::SenderId;
use middleware::discovery::directory::MAX_TXT_LEN;
use middleware::dns::wire::WireError;
pub use middleware::mdns::responder::{Action, Board, Desired, Identity, plan, service};

use crate::chip::Chip;

/// The instance label's prefix (the beacons' `rp-`).
pub const PREFIX: &str = "rp";

/// The network link the record is announced on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    /// USB CDC-NCM beside the CDC-ACM port (`usb-ncm`).
    UsbNcm,
    /// A WIZnet W5500 on SPI0 (`w5500`).
    W5500,
}

/// The board as its record names it.
pub const fn board(chip: Chip, link: Link) -> Board {
    let (chip, board) = match (chip, link) {
        (Chip::Rp2040, Link::UsbNcm) => ("rp2040", "pico-usb"),
        (Chip::Rp2350, Link::UsbNcm) => ("rp2350", "pico2-usb"),
        (Chip::Rp2040, Link::W5500) => ("rp2040", "pico-w5500"),
        (Chip::Rp2350, Link::W5500) => ("rp2350", "pico2-w5500"),
    };
    Board {
        prefix: PREFIX,
        chip,
        board,
    }
}

/// The names of node `node`. [`PREFIX`] is short and lower case, so it
/// always fits; an empty label would make `AnnounceTable::new` refuse it.
pub fn identity(node: SenderId) -> Identity {
    Identity::new(PREFIX, node).unwrap_or(Identity {
        id: node,
        label: heapless::String::new(),
        host: heapless::String::new(),
    })
}

/// What to announce: the interface's IPv4 address and prefix `v4` while
/// the link is up, else nothing.
pub fn wanted(link_up: bool, v4: Option<([u8; 4], u8)>) -> Option<Desired> {
    let (addr, prefix) = v4.filter(|_| link_up)?;
    Some(Desired {
        addr,
        prefix,
        zenoh: None,
    })
}

/// The TXT of `board`'s record for `d` with firmware version `fw`.
pub fn txt(
    board: &Board,
    id: &Identity,
    d: &Desired,
    fw: &str,
) -> Result<Vec<u8, MAX_TXT_LEN>, WireError> {
    middleware::mdns::responder::txt(board, id, d, fw, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::beacons;
    use crate::service::Facts;
    use middleware::dns::wire::Txt;
    use middleware::mdns::announce::AnnounceTable;
    use middleware::mdns::node::NodeTxt;

    const UID: [u8; 8] = [0xE6, 0x61, 0x38, 0x52, 0x83, 0x1F, 0x2A, 0x0B];

    fn link_local() -> Desired {
        Desired {
            addr: [169, 254, 212, 138],
            prefix: 16,
            zenoh: None,
        }
    }

    #[test]
    fn d47_the_label_is_the_beacons_instance() {
        let id = identity(SenderId(UID));
        let facts = Facts {
            chip: "rp2350",
            firmware: "0.1",
            runtime: true,
            node: SenderId(UID),
            max_image: 0,
        };
        assert_eq!(id.label.as_str(), beacons::Identity::new(&facts).instance());
        assert_eq!(id.label.as_str(), "rp-e6613852831f2a0b");
        assert_eq!(id.host.as_str(), "rp-e6613852831f2a0b.local");
        assert_eq!(id.id, SenderId(UID));
        assert!(AnnounceTable::new(&id.host).is_ok());
    }

    #[test]
    fn d47_boards_name_the_chip_and_the_link() {
        let names = [
            (Chip::Rp2040, Link::UsbNcm, "rp2040", "pico-usb"),
            (Chip::Rp2350, Link::UsbNcm, "rp2350", "pico2-usb"),
            (Chip::Rp2040, Link::W5500, "rp2040", "pico-w5500"),
            (Chip::Rp2350, Link::W5500, "rp2350", "pico2-w5500"),
        ];
        for (chip, link, c, b) in names {
            let board = board(chip, link);
            assert_eq!((board.prefix, board.chip, board.board), ("rp", c, b));
        }
        // TXT chip is sys/info's chip name.
        assert_eq!(
            board(Chip::Rp2040, Link::W5500).chip,
            crate::chip::RP2040.name
        );
        assert_eq!(
            board(Chip::Rp2350, Link::UsbNcm).chip,
            crate::chip::RP2350.name
        );
    }

    #[test]
    fn d47_golden_txt() {
        let id = identity(SenderId(UID));
        let b = board(Chip::Rp2350, Link::UsbNcm);
        let t = txt(&b, &id, &link_local(), "0.1").unwrap();
        let golden: &[u8] = b"\x09txtvers=1\
            \x13id=e6613852831f2a0b\
            \x0bchip=rp2350\
            \x0fboard=pico2-usb\
            \x06fw=0.1";
        assert_eq!(t.as_slice(), golden);
        let attrs = NodeTxt::parse(Txt::new(&t).unwrap()).unwrap();
        assert_eq!(
            attrs,
            NodeTxt {
                id: SenderId(UID),
                chip: "rp2350",
                board: "pico2-usb",
                fw: Some("0.1"),
                sender: None,
                zenoh: None,
            }
        );
    }

    #[test]
    fn d47_the_longest_txt_fits() {
        let id = identity(SenderId([0xFF; 8]));
        // The longest board name and firmware string the builds report.
        let b = board(Chip::Rp2350, Link::W5500);
        let fw = "65535.65535";
        let t = txt(&b, &id, &link_local(), fw).unwrap();
        assert!(t.len() < MAX_TXT_LEN, "{} bytes", t.len());
        let t = txt(
            &board(Chip::Rp2040, Link::W5500),
            &id,
            &link_local(),
            "0.1.0+no-wasm",
        )
        .unwrap();
        assert!(t.len() < MAX_TXT_LEN, "{} bytes", t.len());
    }

    #[test]
    fn d47_nothing_is_wanted_while_the_link_is_down() {
        let v4 = Some(([169, 254, 212, 138], 16));
        assert_eq!(wanted(true, v4), Some(link_local()));
        assert_eq!(wanted(false, v4), None, "a static address before any host");
        assert_eq!(wanted(true, None), None, "no lease yet");
        assert_eq!(wanted(false, None), None);
        // The link going down withdraws what was announced.
        assert_eq!(
            plan(Some(link_local()), wanted(false, v4)),
            Action::Withdraw
        );
        assert_eq!(plan(None, wanted(true, v4)), Action::Announce(link_local()));
    }
}
