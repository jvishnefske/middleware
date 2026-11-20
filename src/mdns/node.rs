//! The `mw/node` record: how a middleware node announces itself on mDNS
//! (D40, D41), shared by every responder that announces one (the RT1050
//! firmware, mw-posix-node) and by browsers that read it (mwctl).
//!
//! A node is one DNS-SD instance of [`KIND`] (`_mw-node._tcp.local`):
//!
//! * instance label `<prefix>-<id>` ([`label`]), the node id as 16
//!   lower-case hex digits after a short prefix naming the platform
//!   (`rt1050-e661385283121f2a`), unique because the id is;
//! * host name `<label>.local` ([`host`]), whose A record is the node's
//!   address;
//! * SRV port [`NO_PORT`] unless the node listens on a port worth naming
//!   (the `_device-info._tcp` convention for metadata-only records): a
//!   Zenoh client listens on nothing, and its locator goes in TXT;
//! * TTL [`TTL_S`];
//! * TXT attributes [`NodeTxt`], in this order, `txtvers` first (RFC 6763
//!   §6.7):
//!
//! | key       | value                                                     | |
//! |-----------|-----------------------------------------------------------|----------|
//! | `txtvers` | `1` ([`TXT_VERSION`])                                     | required |
//! | `id`      | node id, 16 lower-case hex digits (what `sys/info` reports) | required |
//! | `chip`    | chip name, e.g. `rt1050`                                  | required |
//! | `board`   | board name, e.g. `mimxrt1050-evkb`                        | required |
//! | `fw`      | firmware version                                          | optional |
//! | `sender`  | the `@mw/node/<sender>` id, 16 lower-case hex digits      | optional |
//! | `zenoh`   | a Zenoh locator, e.g. `tcp/10.0.0.2:7447`                 | optional |
//!
//! `id` is the node's own id; `sender` the id its Zenoh session is
//! addressed by (`zenoh_lite::sender_id_of` of its Zenoh id), which
//! differs on boards whose Zenoh id is not their node id. `chip` and `fw`
//! are the names the RP beacons use. The whole TXT must fit the table's
//! [`MAX_TXT_LEN`] (192) bytes: encode into a buffer of that size.
//!
//! # Announcing
//!
//! ```ignore
//! let label = mdns::node::label("rt1050", id).unwrap();
//! let host = mdns::node::host(&label).unwrap();
//! let mut txt = [0u8; MAX_TXT_LEN];
//! let txt = NodeTxt { id, chip: "rt1050", board: "mimxrt1050-evkb", fw: Some(FW),
//!                     sender: Some(sender), zenoh: None }.encode(&mut txt)?;
//! let addr = SocketAddr::V4(SocketAddrV4::new(ip, mdns::node::NO_PORT));
//! announcer.announce(&mdns::node::service(&label, Endpoint::Socket(addr), txt, id)).await?;
//! ```
//!
//! # Browsing
//!
//! Browse [`KIND`]; for each resolved instance, [`NodeTxt::parse`] its
//! TXT. Unknown keys are ignored, so later versions may add keys; a
//! `txtvers` other than [`TXT_VERSION`] is refused.

use core::fmt::Write as _;

use heapless::String;

#[cfg(doc)]
use crate::discovery::directory::MAX_TXT_LEN;
use crate::discovery::directory::{MAX_HOST_LEN, MAX_INSTANCE_LEN};
use crate::discovery::{Endpoint, Service, ServiceKind};
use crate::dns::wire::{Txt, TxtWriter, WireError};
use crate::transport::SenderId;
use crate::zenoh_wire::zid16_hex;

/// The kind of a node record: DNS-SD type `_mw-node._tcp`.
pub const KIND: ServiceKind<'static> = ServiceKind::from_static("mw/node");
/// The SRV port of a node that names no port of its own.
pub const NO_PORT: u16 = 0;
/// Seconds a node record stays valid.
pub const TTL_S: u32 = 120;
/// The `txtvers` value this module writes and accepts.
pub const TXT_VERSION: &str = "1";
/// The domain of [`host`] names.
const DOMAIN: &str = super::DOMAIN;

/// A node's TXT attributes; see the module docs for the keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeTxt<'a> {
    /// The node id (`id`).
    pub id: SenderId,
    /// The chip name (`chip`).
    pub chip: &'a str,
    /// The board name (`board`).
    pub board: &'a str,
    /// The firmware version (`fw`), if known.
    pub fw: Option<&'a str>,
    /// The id of the node's Zenoh session (`sender`), if it has one.
    pub sender: Option<SenderId>,
    /// A Zenoh locator for the node (`zenoh`), if one is configured.
    pub zenoh: Option<&'a str>,
}

/// Why a TXT record is not a node's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeTxtError {
    /// `txtvers` is missing or not [`TXT_VERSION`].
    Version,
    /// A required key is missing.
    Missing(&'static str),
    /// A key's value is malformed: not UTF-8, or an id that is not 16
    /// lower-case hex digits.
    Malformed(&'static str),
}

impl core::fmt::Display for NodeTxtError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            NodeTxtError::Version => write!(f, "txtvers is not {TXT_VERSION}"),
            NodeTxtError::Missing(k) => write!(f, "TXT key {k} is missing"),
            NodeTxtError::Malformed(k) => write!(f, "TXT key {k} is malformed"),
        }
    }
}

impl core::error::Error for NodeTxtError {}

impl<'a> NodeTxt<'a> {
    /// Writes the TXT RDATA into `out`, keys in the module docs' order and
    /// optional ones only when set. `WireError::BufferTooSmall` when it does
    /// not fit `out` (give it [`MAX_TXT_LEN`] bytes, the most a table
    /// takes), `WireError::BadRdata` for a key and value over 255 bytes.
    pub fn encode<'o>(&self, out: &'o mut [u8]) -> Result<&'o [u8], WireError> {
        let id = zid16_hex(self.id);
        let mut w = TxtWriter::new(out)
            .entry(b"txtvers", TXT_VERSION.as_bytes())?
            .entry(b"id", &id)?
            .entry(b"chip", self.chip.as_bytes())?
            .entry(b"board", self.board.as_bytes())?;
        if let Some(fw) = self.fw {
            w = w.entry(b"fw", fw.as_bytes())?;
        }
        if let Some(sender) = self.sender {
            w = w.entry(b"sender", &zid16_hex(sender))?;
        }
        if let Some(zenoh) = self.zenoh {
            w = w.entry(b"zenoh", zenoh.as_bytes())?;
        }
        Ok(w.finish().as_bytes())
    }

    /// Reads a node's attributes from `txt`, borrowing their values.
    /// Unknown keys are ignored; of a repeated key the first counts.
    pub fn parse(txt: Txt<'a>) -> Result<Self, NodeTxtError> {
        if txt.get(b"txtvers") != Some(TXT_VERSION.as_bytes()) {
            return Err(NodeTxtError::Version);
        }
        let text = |key: &'static str| -> Result<Option<&'a str>, NodeTxtError> {
            txt.get(key.as_bytes())
                .map(|v| core::str::from_utf8(v).map_err(|_| NodeTxtError::Malformed(key)))
                .transpose()
        };
        let id_of = |key: &'static str| -> Result<Option<SenderId>, NodeTxtError> {
            txt.get(key.as_bytes())
                .map(|v| parse_hex16(v).ok_or(NodeTxtError::Malformed(key)))
                .transpose()
        };
        Ok(NodeTxt {
            id: id_of("id")?.ok_or(NodeTxtError::Missing("id"))?,
            chip: text("chip")?.ok_or(NodeTxtError::Missing("chip"))?,
            board: text("board")?.ok_or(NodeTxtError::Missing("board"))?,
            fw: text("fw")?,
            sender: id_of("sender")?,
            zenoh: text("zenoh")?,
        })
    }
}

/// 16 lower-case hex digits as an id.
fn parse_hex16(v: &[u8]) -> Option<SenderId> {
    let digit = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    if v.len() != 16 {
        return None;
    }
    let mut id = [0u8; 8];
    for (byte, pair) in id.iter_mut().zip(v.chunks_exact(2)) {
        *byte = (digit(*pair.first()?)? << 4) | digit(*pair.get(1)?)?;
    }
    Some(SenderId(id))
}

/// The instance label of node `id`: `<prefix>-<id as 16 lower-case hex
/// digits>`. `None` when it is over 63 bytes (a prefix over 46), or the
/// prefix is empty or not lower-case letters, digits and hyphens (so the
/// label is also a valid host name label, [`host`]).
pub fn label(prefix: &str, id: SenderId) -> Option<String<MAX_INSTANCE_LEN>> {
    let ok = !prefix.is_empty()
        && prefix
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-'));
    if !ok {
        return None;
    }
    let hex = zid16_hex(id);
    let mut out = String::new();
    out.push_str(prefix).ok()?;
    out.push('-').ok()?;
    out.push_str(core::str::from_utf8(&hex).ok()?).ok()?;
    Some(out)
}

/// The host name of a node whose instance label is `label`:
/// `<label>.local`, for `AnnounceTable::new`. `None` over 64 bytes.
pub fn host(label: &str) -> Option<String<MAX_HOST_LEN>> {
    let mut out = String::new();
    write!(out, "{label}.{DOMAIN}").ok()?;
    Some(out)
}

/// The node record of `id` as a [`Service`]: [`KIND`], instance `label`,
/// node `id`, [`TTL_S`], with `endpoint` (usually
/// `Endpoint::Socket(<address>:`[`NO_PORT`]`)`) and the encoded `txt`.
pub fn service<'a>(
    label: &'a str,
    endpoint: Endpoint<'a>,
    txt: &'a [u8],
    id: SenderId,
) -> Service<'a> {
    Service {
        kind: KIND,
        instance: label,
        node: Some(id),
        endpoint,
        txt,
        ttl_s: TTL_S,
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::discovery::directory::MAX_TXT_LEN;
    use crate::discovery::record::dns_sd_type;
    use crate::dns::wire::{Instance, Message, Response, ServiceType};
    use crate::mdns::announce::{AnnounceTable, Step};
    use std::vec::Vec;

    const ID: SenderId = SenderId([0xE6, 0x61, 0x38, 0x52, 0x83, 0x12, 0x1F, 0x2A]);
    const SENDER: SenderId = SenderId([0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF]);

    fn full() -> NodeTxt<'static> {
        NodeTxt {
            id: ID,
            chip: "rt1050",
            board: "mimxrt1050-evkb",
            fw: Some("0.1.0"),
            sender: Some(SENDER),
            zenoh: Some("tcp/10.0.0.2:7447"),
        }
    }

    fn minimal() -> NodeTxt<'static> {
        NodeTxt {
            fw: None,
            sender: None,
            zenoh: None,
            ..full()
        }
    }

    fn entries(txt: &[u8]) -> Vec<Vec<u8>> {
        Txt::new(txt).unwrap().entries().map(Vec::from).collect()
    }

    #[test]
    fn d41_txt_round_trips() {
        for t in [full(), minimal()] {
            let mut buf = [0u8; MAX_TXT_LEN];
            let txt = t.encode(&mut buf).unwrap();
            assert_eq!(NodeTxt::parse(Txt::new(txt).unwrap()), Ok(t));
        }
    }

    #[test]
    fn d41_txtvers_comes_first_and_keys_keep_their_order() {
        let mut buf = [0u8; MAX_TXT_LEN];
        let txt = full().encode(&mut buf).unwrap();
        let want: [&[u8]; 7] = [
            b"txtvers=1",
            b"id=e661385283121f2a",
            b"chip=rt1050",
            b"board=mimxrt1050-evkb",
            b"fw=0.1.0",
            b"sender=0123456789abcdef",
            b"zenoh=tcp/10.0.0.2:7447",
        ];
        assert_eq!(entries(txt), want.map(Vec::from));
    }

    #[test]
    fn d41_optional_keys_are_omitted() {
        let mut buf = [0u8; MAX_TXT_LEN];
        let txt = minimal().encode(&mut buf).unwrap();
        let want: [&[u8]; 4] = [
            b"txtvers=1",
            b"id=e661385283121f2a",
            b"chip=rt1050",
            b"board=mimxrt1050-evkb",
        ];
        assert_eq!(entries(txt), want.map(Vec::from));
        let t = NodeTxt::parse(Txt::new(txt).unwrap()).unwrap();
        assert_eq!((t.fw, t.sender, t.zenoh), (None, None, None));
    }

    #[test]
    fn d41_the_rt1050_worst_case_fits_the_table() {
        let t = NodeTxt {
            fw: Some("0.1.0+no-wasm-and-32-chars-long"),
            zenoh: Some("tcp/255.255.255.255:65535"),
            ..full()
        };
        let mut buf = [0u8; MAX_TXT_LEN];
        let len = t.encode(&mut buf).unwrap().len();
        assert!(len <= MAX_TXT_LEN, "{len}");
    }

    #[test]
    fn d41_over_long_values_are_errors() {
        let long = "x".repeat(300);
        let t = NodeTxt {
            fw: Some(&long),
            ..full()
        };
        let mut big = [0u8; 1024];
        assert_eq!(t.encode(&mut big), Err(WireError::BadRdata));
        // Each value fits a string, the whole does not fit the table.
        let half = "y".repeat(120);
        let t = NodeTxt {
            fw: Some(&half),
            zenoh: Some(&half),
            ..full()
        };
        let mut buf = [0u8; MAX_TXT_LEN];
        assert_eq!(t.encode(&mut buf), Err(WireError::BufferTooSmall));
    }

    #[test]
    fn d41_parse_refuses_what_is_not_a_node_record() {
        let parse = |entries: &[&[u8]]| {
            let mut buf = [0u8; 256];
            let mut w = TxtWriter::new(&mut buf);
            for e in entries {
                let (k, v) = e.split_at(e.iter().position(|&b| b == b'=').unwrap());
                w = w.entry(k, &v[1..]).unwrap();
            }
            let txt = w.finish();
            // The attributes borrow `buf`: only the verdict leaves.
            NodeTxt::parse(Txt::new(txt.as_bytes()).unwrap()).map(|_| ())
        };
        let ok: [&[u8]; 4] = [b"txtvers=1", b"id=e661385283121f2a", b"chip=c", b"board=b"];
        assert!(parse(&ok).is_ok());
        assert_eq!(parse(&ok[1..]), Err(NodeTxtError::Version));
        assert_eq!(
            parse(&[b"txtvers=2", ok[1], ok[2], ok[3]]),
            Err(NodeTxtError::Version)
        );
        assert_eq!(
            parse(&[ok[0], ok[2], ok[3]]),
            Err(NodeTxtError::Missing("id"))
        );
        assert_eq!(
            parse(&[ok[0], ok[1], ok[3]]),
            Err(NodeTxtError::Missing("chip"))
        );
        assert_eq!(
            parse(&[ok[0], ok[1], ok[2]]),
            Err(NodeTxtError::Missing("board"))
        );
        for bad_id in [
            &b"id=E661385283121F2A"[..],
            b"id=e661",
            b"id=e661385283121f2ax",
        ] {
            assert_eq!(
                parse(&[ok[0], bad_id, ok[2], ok[3]]),
                Err(NodeTxtError::Malformed("id"))
            );
        }
        assert_eq!(
            parse(&[ok[0], ok[1], ok[2], ok[3], b"sender=zz"]),
            Err(NodeTxtError::Malformed("sender"))
        );
        assert_eq!(
            parse(&[ok[0], ok[1], ok[2], ok[3], b"fw=\xff"]),
            Err(NodeTxtError::Malformed("fw"))
        );
        // Unknown keys are ignored.
        assert!(parse(&[ok[0], ok[1], ok[2], ok[3], b"later=1"]).is_ok());
    }

    #[test]
    fn d41_labels_are_lower_case_hex_and_fit() {
        let l = label("rt1050", ID).unwrap();
        assert_eq!(l.as_str(), "rt1050-e661385283121f2a");
        assert_eq!(host(&l).unwrap().as_str(), "rt1050-e661385283121f2a.local");
        let longest = "p".repeat(46);
        assert_eq!(label(&longest, ID).unwrap().len(), 63);
        assert_eq!(label(&"p".repeat(47), ID), None);
        for bad in ["", "RT1050", "rt_1050", "rt.1050"] {
            assert_eq!(label(bad, ID), None, "{bad}");
        }
        assert_eq!(host(&"h".repeat(58)).map(|h| h.len()), Some(64));
        assert_eq!(host(&"h".repeat(59)), None);
    }

    #[test]
    fn d41_kind_is_mw_node() {
        let mut buf = [0u8; 64];
        assert_eq!(dns_sd_type(KIND, &mut buf), Ok("_mw-node._tcp"));
        assert_eq!(NO_PORT, 0);
        assert_eq!(TTL_S, 120);
    }

    #[test]
    fn d41_a_node_service_registers_probes_and_announces() {
        let l = label("rt1050", ID).unwrap();
        let h = host(&l).unwrap();
        let mut buf = [0u8; MAX_TXT_LEN];
        let txt = full().encode(&mut buf).unwrap();
        let addr = "192.168.1.20:0".parse().unwrap();
        let svc = service(&l, Endpoint::Socket(addr), txt, ID);
        assert_eq!((svc.kind, svc.node, svc.ttl_s), (KIND, Some(ID), TTL_S));
        let (t, r) = AnnounceTable::new(&h).unwrap().register(&svc, 0);
        r.unwrap();
        let mut out = [0u8; 1500];
        let (t, step) = t.poll(0, &mut out);
        let Ok(Step::Send(len)) = step else {
            panic!("no probe: {step:?}")
        };
        assert!(!Message::parse(&out[..len]).unwrap().is_response());
        // The probes, then the announcement at 750 ms.
        let (t, _) = t.poll(250, &mut out);
        let (t, _) = t.poll(500, &mut out);
        let (_, step) = t.poll(750, &mut out);
        let Ok(Step::Send(len)) = step else {
            panic!("no announcement: {step:?}")
        };
        let resp = Response::parse(&out[..len]).unwrap().unwrap();
        let ty = ServiceType::new("_mw-node._tcp.local").unwrap();
        let Some(Instance::Resolved(i)) = resp.instances(ty).next() else {
            panic!("not resolved")
        };
        assert_eq!(i.srv.port, NO_PORT);
        assert_eq!(
            i.socket_addr_v4().map(core::net::SocketAddr::V4),
            Some(addr)
        );
        assert_eq!(i.label(), Ok(l.as_str()));
        assert_eq!(NodeTxt::parse(i.txt.unwrap()), Ok(full()));
    }
}
