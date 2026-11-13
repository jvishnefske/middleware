//! The beacon rules, and the announcer over a real `SerialLink` whose
//! bytes are decoded as the gateway decodes them (publish frames on
//! `disc/<kind>`, `discovery::beacon::decode`), with the gateway's E21
//! rules applied to what arrives: every record names the device or no
//! node, and the device never holds more than the gateway's cap.

extern crate std;

use std::string::{String as StdString, ToString};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::vec::Vec as StdVec;

use embassy_futures::block_on;
use embedded_io_async::{ErrorType, Read, Write};
use middleware::discovery::beacon::{beacon_key, decode, encode, kind_of};
use middleware::discovery::{Change, Directory, Event, ServiceKind};
use middleware::dns::wire::Txt;
use middleware::serial::frame::{self, Frame};
use middleware::serial::link::SerialLink;
use middleware::Key;

use super::*;
use crate::service::tests::{FACTS, NODE};

/// The device end of the cable: every written byte goes to the gateway's
/// end; nothing is ever read.
pub(crate) struct Cable(Sender<u8>);

impl ErrorType for Cable {
    type Error = core::convert::Infallible;
}

impl Write for Cable {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        for &b in buf {
            // The receiver lives as long as the test.
            let _ = self.0.send(b);
        }
        Ok(buf.len())
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl Read for Cable {
    async fn read(&mut self, _buf: &mut [u8]) -> Result<usize, Self::Error> {
        Ok(0)
    }
}

pub(crate) type Link = SerialLink<Cable, 0>;

/// The gateway end: decodes publish frames on `disc/<kind>` and keeps a
/// directory under the gateway's E21 rules (bound node, cap).
pub(crate) struct Gateway {
    rx: Receiver<u8>,
    node: SenderId,
    services: Directory<MAX_RECORDS>,
}

/// One beacon as it came off the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Heard {
    pub(crate) kind: StdString,
    pub(crate) ttl_s: u32,
    pub(crate) node: Option<SenderId>,
}

impl Gateway {
    pub(crate) fn link(node: SenderId) -> (Link, Gateway) {
        let (tx, rx) = channel();
        (SerialLink::with_inbox(Cable(tx)), Gateway { rx, node, services: Directory::new() })
    }

    /// Beacons written since the last call, folded into the directory.
    pub(crate) fn heard(&mut self) -> StdVec<Heard> {
        let bytes: StdVec<u8> = self.rx.try_iter().collect();
        let mut out = StdVec::new();
        for raw in bytes.split(|&b| b == 0).filter(|f| !f.is_empty()) {
            let mut raw = raw.to_vec();
            let Ok(Frame::Publish { key, payload }) = frame::decode_wire(&mut raw) else {
                panic!("not a publish frame");
            };
            let kind = kind_of(key).expect("a disc/<kind> key");
            let service = decode(kind, payload).expect("a version 1 beacon");
            // E21: a beacon naming another node would be refused.
            assert!(service.node.is_none_or(|n| n == self.node), "{service:?}");
            let bound = Service { node: Some(self.node), ..service };
            let (dir, change) = core::mem::replace(&mut self.services, Directory::new()).observe(&Event::from_service(bound), 0);
            self.services = dir;
            // The cap: a new record is never refused.
            assert!(!matches!(change, Change::Full | Change::Rejected), "{change:?}");
            out.push(Heard { kind: kind.as_str().to_string(), ttl_s: service.ttl_s, node: service.node });
        }
        out
    }

    pub(crate) fn kinds(&self) -> StdVec<StdString> {
        self.services.iter().map(|s| s.kind.as_str().to_string()).collect()
    }
}

pub(crate) fn up(kind: &str) -> Heard {
    Heard { kind: kind.to_string(), ttl_s: BEACON_TTL_S, node: Some(NODE) }
}

pub(crate) fn goodbye(kind: &str) -> Heard {
    Heard { kind: kind.to_string(), ttl_s: 0, node: None }
}

fn kinds<'a>(w: &Wanted<'a>) -> StdVec<&'a str> {
    w.records().iter().map(|r| r.kind.as_str()).collect()
}

#[test]
fn v21_the_node_record_names_the_chip_unique_id_chip_and_firmware() {
    let id = Identity::new(&FACTS);
    assert_eq!(id.node(), NODE);
    let hex: StdString = NODE.0.iter().map(|b| std::format!("{b:02x}")).collect();
    assert_eq!(id.instance(), std::format!("rp-{hex}"));
    assert_eq!(id.instance().len(), INSTANCE_LEN);

    let r = id.node_record();
    assert_eq!((r.kind, r.node, r.endpoint, r.ttl_s), (NODE_KIND, Some(NODE), Endpoint::Local, BEACON_TTL_S));
    let txt = Txt::new(r.txt).expect("valid TXT");
    assert_eq!(txt.get(b"chip"), Some(FACTS.chip.as_bytes()));
    assert_eq!(txt.get(b"fw"), Some(FACTS.firmware.as_bytes()));

    // It is a version 1 beacon on disc/mw/node that decodes back.
    let mut key = [0u8; 128];
    assert_eq!(beacon_key(r.kind, &mut key).unwrap().as_str(), "disc/mw/node");
    let mut out = [0u8; 128];
    let n = encode(&r, &mut out).unwrap();
    assert!(n < 70, "{n} bytes");
    assert_eq!(decode(NODE_KIND, &out[..n]), Ok(r));

    // A firmware string too long for one TXT string is left out.
    let long = std::string::String::from_utf8(std::vec![b'9'; 300]).unwrap();
    let facts = Facts { firmware: std::boxed::Box::leak(long.into_boxed_str()), ..FACTS };
    let id = Identity::new(&facts);
    let txt = Txt::new(id.txt()).expect("valid TXT");
    assert_eq!(txt.get(b"fw"), None);
    assert_eq!(txt.get(b"chip"), Some(FACTS.chip.as_bytes()));
}

#[test]
fn v21_served_keys_are_announced_as_kinds_beside_the_node_record_up_to_the_cap() {
    let id = Identity::new(&FACTS);
    // No payload: the node record alone.
    let w = wanted(&id, core::iter::empty());
    assert_eq!((kinds(&w), w.omitted), (std::vec!["mw/node"], 0));

    // Each served key is a record of that kind, same node and instance,
    // no TXT, in slot order.
    let w = wanted(&id, ["node/246f/phase/LED.Blink", "app/echo"].into_iter());
    assert_eq!(kinds(&w), ["mw/node", "node/246f/phase/LED.Blink", "app/echo"]);
    let echo = w.records()[2];
    assert_eq!((echo.instance, echo.node, echo.txt, echo.ttl_s), (id.instance(), Some(NODE), &[][..], BEACON_TTL_S));

    // Beyond the cap (node + 3), repeats, keys that cannot be kinds: left
    // out and counted, never more than MAX_RECORDS.
    let long = std::format!("app/{}", "x".repeat(MAX_KIND_LEN));
    let served = ["app/a", "app/a", "disc/app/a", long.as_str(), "not a key/", "app/b", "app/c", "app/d"];
    let w = wanted(&id, served.into_iter());
    assert_eq!(kinds(&w), ["mw/node", "app/a", "app/b", "app/c"]);
    assert_eq!(w.omitted, 5);
    assert_eq!(w.records().len(), MAX_RECORDS);
    // At most MAX_SERVES keys are looked at.
    let many = std::iter::repeat_n("app/a", 1000);
    assert_eq!(wanted(&id, many).omitted, MAX_SERVES - 1);
}

#[test]
fn v22_plan_withdraws_what_is_not_wanted_and_leaves_unchanged_records_alone() {
    let id = Identity::new(&FACTS);
    let running = wanted(&id, ["app/a", "app/b"].into_iter());
    let idle = wanted(&id, core::iter::empty());
    let ix = |v: &[usize]| v.to_vec();

    // Nothing announced: announce all, node record first.
    let p = plan(&running, &[]);
    assert_eq!((ix(&p.withdraw), ix(&p.announce)), (std::vec![], std::vec![0, 1, 2]));

    // All announced: nothing to do.
    assert_eq!(plan(&running, running.records()), Plan::default());

    // The run ended: the served keys are withdrawn, the node record stays.
    let p = plan(&idle, running.records());
    assert_eq!((ix(&p.withdraw), ix(&p.announce)), (std::vec![1, 2], std::vec![]));

    // A changed record (another firmware) is announced again; a replaced
    // key is withdrawn and the new one announced; app/b is unchanged.
    let other = Identity::new(&Facts { firmware: "9.9", ..FACTS });
    let next = wanted(&other, ["app/c", "app/b"].into_iter());
    let p = plan(&next, running.records());
    assert_eq!((ix(&p.withdraw), ix(&p.announce)), (std::vec![1], std::vec![0, 1]));
    // Only the first MAX_RECORDS announced records are looked at.
    let many = [running.records()[1]; 6];
    assert_eq!(plan(&idle, &many).withdraw.len(), MAX_RECORDS);
}

#[test]
fn v22_the_announcer_on_the_serial_link_refreshes_and_says_goodbye_when_the_run_ends() {
    block_on(async {
        let id = Identity::new(&FACTS);
        let (link, mut gw) = Gateway::link(NODE);
        let mut a = Announcer::new(link);

        // Boot: the node record.
        assert_eq!(a.sync(&wanted(&id, core::iter::empty())).await, Ok(1));
        assert_eq!(gw.heard(), [up("mw/node")]);
        assert_eq!(a.tick(0).await, Ok(0));

        // A payload serves two keys: announced; a second sync sends nothing.
        let run = wanted(&id, ["app/led", "app/echo"].into_iter());
        assert_eq!(a.sync(&run).await, Ok(2));
        assert_eq!(gw.heard(), [up("app/led"), up("app/echo")]);
        assert_eq!(a.sync(&run).await, Ok(0));
        assert_eq!(a.tick(1_000).await, Ok(0));

        // Every half TTL (15 s) each record is sent again.
        assert_eq!(a.due(1_000), Some(15_000));
        assert_eq!(a.tick(14_999).await, Ok(0));
        assert_eq!(a.tick(15_000).await, Ok(1));
        assert_eq!(gw.heard(), [up("mw/node")]);
        assert_eq!(a.tick(16_000).await, Ok(2));
        assert_eq!(gw.heard(), [up("app/led"), up("app/echo")]);
        assert_eq!(gw.kinds().len(), 3);

        // Stop, trap or upload: the run's keys say goodbye, the node stays.
        assert_eq!(a.sync(&wanted(&id, core::iter::empty())).await, Ok(2));
        assert_eq!(gw.heard(), [goodbye("app/led"), goodbye("app/echo")]);
        assert_eq!(gw.kinds(), ["mw/node"]);
        assert_eq!(a.announced().count(), 1);

        // A payload serving more than fits: three keys, and the gateway
        // never refuses one (the assertions in `heard`).
        let big = wanted(&id, ["app/1", "app/2", "app/3", "app/4", "app/5"].into_iter());
        assert_eq!(a.sync(&big).await, Ok(3));
        assert_eq!(gw.heard().len(), 3);
        assert_eq!(gw.kinds().len(), MAX_RECORDS);
        // Swapped for a payload serving others: goodbyes first, so the
        // gateway's cap is never exceeded (the assertions in `heard`).
        let swap = wanted(&id, ["app/6", "app/7", "app/8"].into_iter());
        assert_eq!(a.sync(&swap).await, Ok(6));
        let heard = gw.heard();
        assert_eq!(heard.iter().take(3).map(|h| h.ttl_s).collect::<StdVec<_>>(), [0, 0, 0]);
        assert_eq!(gw.kinds().len(), MAX_RECORDS);
    });
}

#[test]
fn v23_answering_sys_info_resends_every_record_at_most_every_five_seconds() {
    let (k, first) = Kick::new().after_info(100);
    assert!(first);
    let (k, again) = k.after_info(100 + KICK_INTERVAL_MS - 1);
    assert!(!again);
    let (_, later) = k.after_info(100 + KICK_INTERVAL_MS);
    assert!(later);

    block_on(async {
        let id = Identity::new(&FACTS);
        let (link, mut gw) = Gateway::link(NODE);
        let mut a = Announcer::new(link);
        let run = wanted(&id, ["app/led"].into_iter());
        assert_eq!(a.sync(&run).await, Ok(2));
        gw.heard();
        // The gateway's sys/info: both records again, unchanged.
        assert_eq!(a.after_info(&run, 1_000).await, Ok(2));
        assert_eq!(gw.heard(), [up("mw/node"), up("app/led")]);
        // Polled again within 5 s: only what changed.
        assert_eq!(a.after_info(&run, 2_000).await, Ok(0));
        let idle = wanted(&id, core::iter::empty());
        assert_eq!(a.after_info(&idle, 3_000).await, Ok(1));
        assert_eq!(gw.heard(), [goodbye("app/led")]);
        // A resend restarts the interval: the next refresh is 15 s on.
        assert_eq!(a.after_info(&idle, 6_000).await, Ok(1));
        assert_eq!(a.tick(6_000).await, Ok(0));
        assert_eq!(a.due(6_000), Some(21_000));
    });
}

#[test]
fn v24_a_payload_cannot_publish_beacons_or_system_keys() {
    let k = |s| Key::new(s).unwrap();
    assert!(payload_may_publish(k("app/blink/state")));
    assert!(payload_may_publish(k("node/246f/phase/LED.Blink/state")));
    assert!(payload_may_publish(k("discovery/x")));
    assert!(!payload_may_publish(k("disc/mw/node")));
    assert!(!payload_may_publish(k("disc")));
    assert!(!payload_may_publish(k("sys/log")));
    // A served `disc` key is never announced either.
    let id = Identity::new(&FACTS);
    assert_eq!(id.served_record("disc/cam/jpeg"), None);
    assert_eq!(ServiceKind::new(k("app/x")), id.served_record("app/x").unwrap().kind);
}

#[test]
fn v21_the_device_never_browses() {
    let mut s = NoBrowse;
    let mut buf = [0u8; 8];
    assert_eq!(s.try_recv(&mut buf).map(|o| o.is_none()), Ok(true));
    assert!(block_on(s.recv(&mut buf)).is_err());
}

