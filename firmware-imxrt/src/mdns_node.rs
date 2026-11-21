//! The RT1050's mDNS identity (D42): what the board announces as its
//! `mw/node` record ([`middleware::mdns::node`]). The decisions (when to
//! announce again or withdraw, the TXT and the service) are the boards'
//! shared core, [`middleware::mdns::responder`] (D46), re-exported here;
//! this module adds the board's names:
//!
//! * [`identity`] — the board's node id, instance label
//!   `rt1050-<unique id as 16 lower-case hex digits>` and host name
//!   `<label>.local`.
//! * [`txt`] — the record's TXT with `chip` [`CHIP`] and `board` [`BOARD`]
//!   ([`RT1050_EVKB`]).
//! * a [`Locator`] from the configured Zenoh [`Endpoint`], TXT `zenoh`.
//!
//! [`Desired`] is the DHCP address and prefix and the endpoint; the record
//! is an A record of the address and SRV port
//! [`NO_PORT`](middleware::mdns::node::NO_PORT) (the board is a Zenoh
//! client and listens on nothing).

use heapless::{String, Vec};
use middleware::SenderId;
use middleware::discovery::directory::MAX_TXT_LEN;
use middleware::dns::wire::WireError;
pub use middleware::mdns::responder::{Action, Board, Desired, Identity, Locator, plan, service};

use crate::chip::RT1050;
use crate::config::Endpoint;
use crate::node::node_id;

/// The chip name in TXT `chip` (`sys/info`'s).
pub const CHIP: &str = RT1050.name;
/// The board name in TXT `board`.
pub const BOARD: &str = "mimxrt1050-evkb";
/// The instance label's prefix.
pub const PREFIX: &str = "rt1050";

/// The board as its record names it.
pub const RT1050_EVKB: Board = Board {
    prefix: PREFIX,
    chip: CHIP,
    board: BOARD,
};

/// The names of the chip with unique id `uid`.
pub fn identity(uid: [u8; 8]) -> Identity {
    let id = node_id(uid);
    // PREFIX is short and lower case, so it always fits; an empty label
    // would make `AnnounceTable::new` refuse it and the task park.
    Identity::new(PREFIX, id).unwrap_or(Identity {
        id,
        label: String::new(),
        host: String::new(),
    })
}

/// The TXT of the record for `d`: the board's id, [`CHIP`], [`BOARD`],
/// firmware version `fw`, Zenoh sender id `sender` and the endpoint.
pub fn txt(
    id: &Identity,
    d: &Desired,
    fw: &str,
    sender: Option<SenderId>,
) -> Result<Vec<u8, MAX_TXT_LEN>, WireError> {
    middleware::mdns::responder::txt(&RT1050_EVKB, id, d, fw, sender)
}

impl From<Endpoint> for Locator {
    fn from(e: Endpoint) -> Self {
        Locator {
            addr: e.addr,
            port: e.port,
        }
    }
}

#[cfg(test)]
mod tests {
    use core::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

    use super::*;
    use middleware::discovery::{Endpoint as SrvEndpoint, Service};
    use middleware::dns::wire::Txt;
    use middleware::mdns::announce::AnnounceTable;
    use middleware::mdns::node::{KIND, NodeTxt};

    const UID: [u8; 8] = [0xE6, 0x61, 0x38, 0x52, 0x83, 0x12, 0x1F, 0x2A];
    const SENDER: SenderId = SenderId([0xA1, 0xB2, 0xC3, 0xD4, 0xE5, 0xF6, 0x07, 0x18]);

    fn lease(addr: [u8; 4], prefix: u8) -> Desired {
        Desired {
            addr,
            prefix,
            zenoh: None,
        }
    }

    #[test]
    fn d42_identity_is_lower_case_and_from_the_unique_id() {
        let id = identity(UID);
        assert_eq!(id.id, SenderId(UID));
        assert_eq!(id.label.as_str(), "rt1050-e661385283121f2a");
        assert_eq!(id.host.as_str(), "rt1050-e661385283121f2a.local");
        assert!(AnnounceTable::new(&id.host).is_ok());
    }

    #[test]
    fn d42_txt_carries_the_board_and_the_configured_endpoint() {
        let id = identity(UID);
        let ep = Endpoint::parse("tcp/255.255.255.255:65535").unwrap();
        let d = Desired {
            zenoh: Some(Locator::from(ep)),
            ..lease([192, 168, 1, 20], 24)
        };
        let t = txt(&id, &d, "0.1.0", Some(SENDER)).unwrap();
        let attrs = NodeTxt::parse(Txt::new(&t).unwrap()).unwrap();
        assert_eq!((attrs.chip, attrs.board), ("rt1050", "mimxrt1050-evkb"));
        assert_eq!(attrs.zenoh, Some("tcp/255.255.255.255:65535"));
    }

    /// D46: the bytes the board announces, pinned before the pure core
    /// moved to `middleware::mdns::responder`; the move changes none.
    #[test]
    fn d46_golden_txt_and_service() {
        let id = identity(UID);
        let d = Desired {
            zenoh: Some(Locator::from(Endpoint {
                addr: [10, 0, 0, 2],
                port: 7447,
            })),
            ..lease([192, 168, 1, 20], 24)
        };
        let t = txt(&id, &d, "0.1.0", Some(SENDER)).unwrap();
        let golden: &[u8] = b"\x09txtvers=1\
            \x13id=e661385283121f2a\
            \x0bchip=rt1050\
            \x15board=mimxrt1050-evkb\
            \x08fw=0.1.0\
            \x17sender=a1b2c3d4e5f60718\
            \x17zenoh=tcp/10.0.0.2:7447";
        assert_eq!(t.as_slice(), golden);
        let bare = txt(&id, &lease([192, 168, 1, 20], 24), "0.1.0+no-wasm", None).unwrap();
        let golden_bare: &[u8] = b"\x09txtvers=1\
            \x13id=e661385283121f2a\
            \x0bchip=rt1050\
            \x15board=mimxrt1050-evkb\
            \x10fw=0.1.0+no-wasm";
        assert_eq!(bare.as_slice(), golden_bare);
        let svc = service(&id, &d, &t);
        assert_eq!(
            svc,
            Service {
                kind: KIND,
                instance: "rt1050-e661385283121f2a",
                node: Some(SenderId(UID)),
                endpoint: SrvEndpoint::Socket(SocketAddr::V4(SocketAddrV4::new(
                    Ipv4Addr::new(192, 168, 1, 20),
                    0
                ))),
                txt: golden,
                ttl_s: 120,
            }
        );
    }
}
