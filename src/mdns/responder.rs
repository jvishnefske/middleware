//! A board's mDNS responder decisions (D46), pure: what a board announces
//! as its `mw/node` record ([`super::node`]) and when it announces again or
//! withdraws. Shared by the RT1050 (DHCP on Ethernet) and the RP boards
//! (link-local over USB CDC-NCM, DHCP behind a W5500); each keeps its own
//! task loop and says what it wants announced ([`Desired`]).
//!
//! * [`Board`] — what a board is: its instance prefix and TXT `chip` and
//!   `board`.
//! * [`Identity`] — the board's node id, instance label
//!   `<prefix>-<id as 16 lower-case hex digits>` and host `<label>.local`.
//! * [`Desired`] — what should be announced: an address and prefix, and the
//!   Zenoh endpoint ([`Locator`]) if one is set.
//! * [`plan`] — from what was announced and what is wanted, the
//!   responder's next [`Action`]: nothing, announce (a new address or
//!   prefix, a new endpoint) or withdraw (the address is gone).
//! * [`txt`] and [`service`] — the record itself: TXT `txtvers`, `id`,
//!   `chip`, `board`, `fw`, `sender` and, when an endpoint is set, `zenoh`;
//!   an A record of the address and SRV port [`NO_PORT`] (the boards are
//!   Zenoh clients and listen on nothing).
//!
//! Re-announcing a live instance after an address change sends the new A
//! record with the cache-flush bit at once (RFC 6762 §8.4, no probing), so
//! peers drop the old address.

use core::fmt::{self, Write as _};
use core::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use heapless::{String, Vec};

use super::node::{self, NO_PORT, NodeTxt};
use crate::discovery::directory::{MAX_HOST_LEN, MAX_INSTANCE_LEN, MAX_TXT_LEN};
use crate::discovery::{Endpoint, Service};
use crate::dns::wire::WireError;
use crate::transport::SenderId;

/// Longest [`Locator`] text, `tcp/255.255.255.255:65535`.
pub const MAX_LOCATOR: usize = 25;

/// What a board is, as its record says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Board {
    /// The instance label's prefix ([`node::label`]'s rules).
    pub prefix: &'static str,
    /// TXT `chip` (`sys/info`'s chip name).
    pub chip: &'static str,
    /// TXT `board`.
    pub board: &'static str,
}

/// A board's names on mDNS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The node id (`sys/info`'s), TXT `id`.
    pub id: SenderId,
    /// The instance label, `<prefix>-<id>`.
    pub label: String<MAX_INSTANCE_LEN>,
    /// The host name, `<label>.local`.
    pub host: String<MAX_HOST_LEN>,
}

impl Identity {
    /// The names of node `id` under `prefix`; `None` when [`node::label`]
    /// or [`node::host`] refuses them.
    pub fn new(prefix: &str, id: SenderId) -> Option<Self> {
        let label = node::label(prefix, id)?;
        let host = node::host(&label)?;
        Some(Identity { id, label, host })
    }
}

/// A Zenoh TCP endpoint, TXT `zenoh=tcp/A.B.C.D:P`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Locator {
    /// IPv4 address, most significant octet first.
    pub addr: [u8; 4],
    /// TCP port.
    pub port: u16,
}

impl Locator {
    /// Writes `tcp/A.B.C.D:P` into `out`; its length, or `None` when `out`
    /// is too small (the text is at most [`MAX_LOCATOR`] bytes).
    pub fn write_to(&self, out: &mut [u8]) -> Option<usize> {
        let mut text: String<MAX_LOCATOR> = String::new();
        write!(text, "{self}").ok()?;
        out.get_mut(..text.len())?.copy_from_slice(text.as_bytes());
        Some(text.len())
    }
}

impl fmt::Display for Locator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c, d] = self.addr;
        write!(f, "tcp/{a}.{b}.{c}.{d}:{}", self.port)
    }
}

/// What the board should be announcing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Desired {
    /// The address, most significant octet first.
    pub addr: [u8; 4],
    /// Its prefix length.
    pub prefix: u8,
    /// The Zenoh endpoint, TXT `zenoh`.
    pub zenoh: Option<Locator>,
}

/// The responder's next step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// What is announced is what is wanted.
    Nothing,
    /// (Re-)announce the record for this.
    Announce(Desired),
    /// The address is gone (a lost lease, a link down): withdraw the
    /// record.
    Withdraw,
}

/// The step from `announced` (what the last successful announcement
/// carried, `None` if nothing is announced) to `want` (`None` without an
/// address).
pub fn plan(announced: Option<Desired>, want: Option<Desired>) -> Action {
    match (announced, want) {
        (Some(a), Some(w)) if a == w => Action::Nothing,
        (_, Some(w)) => Action::Announce(w),
        (Some(_), None) => Action::Withdraw,
        (None, None) => Action::Nothing,
    }
}

/// The TXT of `board`'s record for `d`: `id`'s node id, the board's chip
/// and name, firmware version `fw`, Zenoh sender id `sender` and the
/// endpoint.
pub fn txt(
    board: &Board,
    id: &Identity,
    d: &Desired,
    fw: &str,
    sender: Option<SenderId>,
) -> Result<Vec<u8, MAX_TXT_LEN>, WireError> {
    let mut locator = [0u8; MAX_LOCATOR];
    let zenoh = match d.zenoh {
        Some(e) => {
            let n = e.write_to(&mut locator).ok_or(WireError::BufferTooSmall)?;
            let text = locator.get(..n).ok_or(WireError::BufferTooSmall)?;
            Some(core::str::from_utf8(text).map_err(|_| WireError::BadRdata)?)
        }
        None => None,
    };
    let attrs = NodeTxt {
        id: id.id,
        chip: board.chip,
        board: board.board,
        fw: Some(fw),
        sender,
        zenoh,
    };
    let mut buf = [0u8; MAX_TXT_LEN];
    let bytes = attrs.encode(&mut buf)?;
    Vec::from_slice(bytes).map_err(|_| WireError::BufferTooSmall)
}

/// The record for `d` with TXT `txt` ([`txt`]): `id`'s label, the address
/// at port [`NO_PORT`].
pub fn service<'a>(id: &'a Identity, d: &Desired, txt: &'a [u8]) -> Service<'a> {
    let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(d.addr), NO_PORT));
    node::service(&id.label, Endpoint::Socket(addr), txt, id.id)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::dns::wire::{Instance, Message, Response, ServiceType, Txt};
    use crate::mdns::announce::{AnnounceTable, Step};
    use crate::mdns::node::{KIND, TTL_S};

    const BOARD: Board = Board {
        prefix: "rt1050",
        chip: "rt1050",
        board: "mimxrt1050-evkb",
    };
    const UID: [u8; 8] = [0xE6, 0x61, 0x38, 0x52, 0x83, 0x12, 0x1F, 0x2A];
    const SENDER: SenderId = SenderId([0xA1, 0xB2, 0xC3, 0xD4, 0xE5, 0xF6, 0x07, 0x18]);

    fn identity() -> Identity {
        Identity::new(BOARD.prefix, SenderId(UID)).unwrap()
    }

    fn lease(addr: [u8; 4], prefix: u8) -> Desired {
        Desired {
            addr,
            prefix,
            zenoh: None,
        }
    }

    #[test]
    fn d46_identity_is_lower_case_and_from_the_node_id() {
        let id = identity();
        assert_eq!(id.id, SenderId(UID));
        assert_eq!(id.label.as_str(), "rt1050-e661385283121f2a");
        assert_eq!(id.host.as_str(), "rt1050-e661385283121f2a.local");
        assert!(AnnounceTable::new(&id.host).is_ok());
        let rp = Identity::new("rp", SenderId(UID)).unwrap();
        assert_eq!(rp.label.as_str(), "rp-e661385283121f2a");
        // node::label's refusals.
        assert_eq!(Identity::new("", SenderId(UID)), None);
        assert_eq!(Identity::new("RT", SenderId(UID)), None);
        // The longest prefix whose host fits 64 bytes (41 + 17 + 6).
        let long = "a2345678901234567890123456789012345678901";
        let max = Identity::new(long, SenderId(UID)).unwrap();
        assert_eq!(max.host.len(), MAX_HOST_LEN);
        assert_eq!(Identity::new(&std::format!("{long}x"), SenderId(UID)), None);
    }

    #[test]
    fn d46_locators_are_tcp_dotted_quads() {
        let mut out = [0u8; MAX_LOCATOR];
        let worst = Locator {
            addr: [255, 255, 255, 255],
            port: 65535,
        };
        assert_eq!(worst.write_to(&mut out), Some(MAX_LOCATOR));
        assert_eq!(&out[..], b"tcp/255.255.255.255:65535");
        let small = Locator {
            addr: [10, 0, 0, 2],
            port: 7447,
        };
        let n = small.write_to(&mut out).unwrap();
        assert_eq!(&out[..n], b"tcp/10.0.0.2:7447");
        assert_eq!(worst.write_to(&mut [0u8; MAX_LOCATOR - 1]), None);
        assert_eq!(small.write_to(&mut []), None);
    }

    #[test]
    fn d46_plan_announces_changes_and_withdraws_a_lost_address() {
        let a = lease([192, 168, 1, 20], 24);
        let ep = Locator {
            addr: [10, 0, 0, 2],
            port: 7447,
        };
        // Nothing to do.
        assert_eq!(plan(None, None), Action::Nothing);
        assert_eq!(plan(Some(a), Some(a)), Action::Nothing);
        // A new lease, a new address, a new prefix, a new endpoint.
        assert_eq!(plan(None, Some(a)), Action::Announce(a));
        let moved = lease([10, 1, 2, 3], 24);
        assert_eq!(plan(Some(a), Some(moved)), Action::Announce(moved));
        let wider = lease([192, 168, 1, 20], 16);
        assert_eq!(plan(Some(a), Some(wider)), Action::Announce(wider));
        let with_ep = Desired {
            zenoh: Some(ep),
            ..a
        };
        assert_eq!(plan(Some(a), Some(with_ep)), Action::Announce(with_ep));
        assert_eq!(plan(Some(with_ep), Some(a)), Action::Announce(a));
        // The address is lost.
        assert_eq!(plan(Some(a), None), Action::Withdraw);
    }

    #[test]
    fn d46_txt_carries_the_board_and_the_endpoint() {
        let id = identity();
        let d = Desired {
            zenoh: Some(Locator {
                addr: [255, 255, 255, 255],
                port: 65535,
            }),
            ..lease([192, 168, 1, 20], 24)
        };
        let t = txt(&BOARD, &id, &d, "0.1.0", Some(SENDER)).unwrap();
        let attrs = NodeTxt::parse(Txt::new(&t).unwrap()).unwrap();
        assert_eq!(
            attrs,
            NodeTxt {
                id: SenderId(UID),
                chip: "rt1050",
                board: "mimxrt1050-evkb",
                fw: Some("0.1.0"),
                sender: Some(SENDER),
                zenoh: Some("tcp/255.255.255.255:65535"),
            }
        );
        // No endpoint, no sender: the keys are left out.
        let t = txt(
            &BOARD,
            &id,
            &lease([192, 168, 1, 20], 24),
            "0.1.0+no-wasm",
            None,
        )
        .unwrap();
        let attrs = NodeTxt::parse(Txt::new(&t).unwrap()).unwrap();
        assert_eq!((attrs.zenoh, attrs.sender), (None, None));
        assert_eq!(attrs.fw, Some("0.1.0+no-wasm"));
        // Another board's names.
        let pico = Board {
            prefix: "rp",
            chip: "rp2350",
            board: "pico2-usb",
        };
        let t = txt(&pico, &id, &lease([169, 254, 1, 2], 16), "0.1", None).unwrap();
        let attrs = NodeTxt::parse(Txt::new(&t).unwrap()).unwrap();
        assert_eq!((attrs.chip, attrs.board), ("rp2350", "pico2-usb"));
    }

    #[test]
    fn d46_the_record_probes_and_announces_the_address() {
        let id = identity();
        let d = lease([192, 168, 1, 20], 24);
        let t = txt(&BOARD, &id, &d, "0.1.0", Some(SENDER)).unwrap();
        let svc = service(&id, &d, &t);
        assert_eq!((svc.kind, svc.node, svc.ttl_s), (KIND, Some(id.id), TTL_S));
        assert_eq!(svc.instance, "rt1050-e661385283121f2a");
        let (table, r) = AnnounceTable::new(&id.host).unwrap().register(&svc, 0);
        r.unwrap();
        let mut out = [0u8; 1500];
        let (table, step) = table.poll(0, &mut out);
        let Ok(Step::Send(len)) = step else {
            panic!("no probe: {step:?}")
        };
        assert!(!Message::parse(&out[..len]).unwrap().is_response());
        let (table, _) = table.poll(250, &mut out);
        let (table, _) = table.poll(500, &mut out);
        let (_, step) = table.poll(750, &mut out);
        let Ok(Step::Send(len)) = step else {
            panic!("no announcement: {step:?}")
        };
        let resp = Response::parse(&out[..len]).unwrap().unwrap();
        let ty = ServiceType::new("_mw-node._tcp.local").unwrap();
        let Some(Instance::Resolved(i)) = resp.instances(ty).next() else {
            panic!("not resolved")
        };
        assert_eq!(i.ipv4, Some(Ipv4Addr::new(192, 168, 1, 20)));
        assert_eq!(i.srv.port, NO_PORT);
        assert_eq!(i.label(), Ok("rt1050-e661385283121f2a"));
        let mut host = [0u8; 64];
        assert_eq!(
            i.srv.target.write_text(&mut host),
            Ok("rt1050-e661385283121f2a.local")
        );
    }
}
