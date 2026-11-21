//! The mDNS responder's pure core (D42): what the board announces as its
//! `mw/node` record ([`middleware::mdns::node`]) and when it announces
//! again or withdraws.
//!
//! * [`Identity`] — the board's node id, instance label
//!   `rt1050-<unique id as 16 lower-case hex digits>` and host name
//!   `<label>.local`.
//! * [`Desired`] — what should be announced: the DHCP address and prefix
//!   and the configured Zenoh endpoint.
//! * [`plan`] — from what was announced and what is wanted, the
//!   responder's next [`Action`]: nothing, announce (a new lease, a new
//!   address or prefix, a new endpoint) or withdraw (the lease is lost).
//! * [`txt`] and [`service`] — the record itself: TXT `txtvers`, `id`,
//!   `chip` ([`CHIP`]), `board` ([`BOARD`]), `fw`, `sender` and, when an
//!   endpoint is set, `zenoh`; an A record of the address and SRV port
//!   [`NO_PORT`] (the board is a Zenoh client and listens on nothing).
//!
//! Re-announcing a live instance after an address change sends the new A
//! record with the cache-flush bit at once (RFC 6762 §8.4, no probing), so
//! peers drop the old address.

use core::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use heapless::{String, Vec};
use middleware::SenderId;
use middleware::discovery::directory::{MAX_HOST_LEN, MAX_INSTANCE_LEN, MAX_TXT_LEN};
use middleware::discovery::{Endpoint as SrvEndpoint, Service};
use middleware::dns::wire::WireError;
use middleware::mdns::node::{self, NO_PORT, NodeTxt};

use crate::chip::RT1050;
use crate::config::Endpoint;
use crate::node::node_id;

/// The chip name in TXT `chip` (`sys/info`'s).
pub const CHIP: &str = RT1050.name;
/// The board name in TXT `board`.
pub const BOARD: &str = "mimxrt1050-evkb";
/// The instance label's prefix.
pub const PREFIX: &str = "rt1050";

/// Longest `tcp/A.B.C.D:P` locator ([`Endpoint`]'s text).
const MAX_LOCATOR: usize = 25;

/// The board's names on mDNS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The node id (`sys/info`'s), TXT `id`.
    pub id: SenderId,
    /// The instance label, `rt1050-<id>`.
    pub label: String<MAX_INSTANCE_LEN>,
    /// The host name, `<label>.local`.
    pub host: String<MAX_HOST_LEN>,
}

impl Identity {
    /// The names of the chip with unique id `uid`.
    pub fn of(uid: [u8; 8]) -> Self {
        let id = node_id(uid);
        // PREFIX is short and lower case, so both always fit.
        let label = node::label(PREFIX, id).unwrap_or_default();
        let host = node::host(&label).unwrap_or_default();
        Identity { id, label, host }
    }
}

/// What the board should be announcing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Desired {
    /// The DHCP address, most significant octet first.
    pub addr: [u8; 4],
    /// Its prefix length.
    pub prefix: u8,
    /// The Zenoh endpoint (`sys/zenoh/endpoint`), TXT `zenoh`.
    pub zenoh: Option<Endpoint>,
}

/// The responder's next step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// What is announced is what is wanted.
    Nothing,
    /// (Re-)announce the record for this.
    Announce(Desired),
    /// The lease is gone: withdraw the record.
    Withdraw,
}

/// The step from `announced` (what the last successful announcement
/// carried, `None` if nothing is announced) to `want` (`None` without a
/// lease).
pub fn plan(announced: Option<Desired>, want: Option<Desired>) -> Action {
    match (announced, want) {
        (Some(a), Some(w)) if a == w => Action::Nothing,
        (_, Some(w)) => Action::Announce(w),
        (Some(_), None) => Action::Withdraw,
        (None, None) => Action::Nothing,
    }
}

/// The TXT of the record for `d`: the board's id, [`CHIP`], [`BOARD`],
/// firmware version `fw`, Zenoh sender id `sender` and the endpoint.
pub fn txt(
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
        chip: CHIP,
        board: BOARD,
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
    node::service(&id.label, SrvEndpoint::Socket(addr), txt, id.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::dns::wire::{Instance, Message, Response, ServiceType, Txt};
    use middleware::mdns::announce::{AnnounceTable, Step};
    use middleware::mdns::node::{KIND, TTL_S};

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
        let id = Identity::of(UID);
        assert_eq!(id.id, SenderId(UID));
        assert_eq!(id.label.as_str(), "rt1050-e661385283121f2a");
        assert_eq!(id.host.as_str(), "rt1050-e661385283121f2a.local");
        assert!(AnnounceTable::new(&id.host).is_ok());
    }

    #[test]
    fn d42_plan_announces_changes_and_withdraws_a_lost_lease() {
        let a = lease([192, 168, 1, 20], 24);
        let ep = Endpoint {
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
        // The lease is lost.
        assert_eq!(plan(Some(a), None), Action::Withdraw);
    }

    #[test]
    fn d42_txt_carries_the_board_and_the_endpoint() {
        let id = Identity::of(UID);
        let d = Desired {
            zenoh: Some(Endpoint {
                addr: [255, 255, 255, 255],
                port: 65535,
            }),
            ..lease([192, 168, 1, 20], 24)
        };
        let t = txt(&id, &d, "0.1.0", Some(SENDER)).unwrap();
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
        let t = txt(&id, &lease([192, 168, 1, 20], 24), "0.1.0+no-wasm", None).unwrap();
        let attrs = NodeTxt::parse(Txt::new(&t).unwrap()).unwrap();
        assert_eq!((attrs.zenoh, attrs.sender), (None, None));
        assert_eq!(attrs.fw, Some("0.1.0+no-wasm"));
    }

    /// D46: the bytes the board announces, pinned before the pure core
    /// moved to `middleware::mdns::responder`; the move changes none.
    #[test]
    fn d46_golden_txt_and_service() {
        let id = Identity::of(UID);
        let d = Desired {
            zenoh: Some(Endpoint {
                addr: [10, 0, 0, 2],
                port: 7447,
            }),
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

    #[test]
    fn d42_the_record_probes_and_announces_the_lease_address() {
        let id = Identity::of(UID);
        let d = lease([192, 168, 1, 20], 24);
        let t = txt(&id, &d, "0.1.0", Some(SENDER)).unwrap();
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
