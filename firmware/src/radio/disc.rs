//! Service discovery on the radio, pure: the camera beacon a device sends
//! while a camera route runs, and the directory of the beacons it hears.
//!
//! # The camera beacon
//!
//! While a camera source route runs, the radio task announces one service
//! of kind [`CAM_KIND`] (`cam/jpeg`) on the beacon key `disc/cam/jpeg`:
//! instance `cam-xxxxxx` (the last three bytes of the MAC, [`instance`]),
//! node id the MAC ([`SenderId::from_mac`]), endpoint
//! [`Endpoint::Local`] (reach it on this radio), TXT `key=<route key>`
//! (where its frames stream) and TTL [`BEACON_TTL_S`]. [`CamBeacon::step`]
//! says when: as soon as the route runs, then every half TTL
//! ([`reannounce_after_ms`], 15 s), and one goodbye (TTL 0) when it stops.
//! That is one frame of under 100 bytes per 15 s.
//!
//! The beacon is written with the codec of
//! `middleware::discovery::beacon` ([`beacon_key`], [`encode`]): the same
//! wire format and re-announce interval as `PubSubDiscovery`, which the
//! radio task does not use because it would own the transport's
//! publishing half (the task also broadcasts payload publishes and route
//! chunks through it).
//!
//! # The directory
//!
//! Every `disc/...` sample the radio receives goes to [`Neighbours::hear`]:
//! decoded with [`decode`] and folded into a [`Directory`] of
//! [`NEIGHBOURS`] records. Each record is bound to the frame's sender (its
//! source MAC): a beacon declaring another node is refused, only the
//! sender that created a record may update or withdraw it, and one sender
//! holds at most [`PER_SENDER`] records, so it cannot take every slot
//! (refusals are counted, not logged). A new record, a changed one and a
//! goodbye return a [`Heard`] whose `Display` is the `sys/log` line;
//! refreshes are silent. Kinds and instances come off the air, so the lines
//! print them with every byte outside printable ASCII (and `\`) as
//! `\xNN`: a beacon cannot put escape sequences or line breaks in the log.
//! [`Neighbours::expired`] lists the records whose TTL has run out (their
//! [`Expired`] lines) before [`Neighbours::expire`] drops them. The lines:
//!
//! ```text
//! disc: up cam/jpeg cam-aabbcc node 246f28aabbcc0000 ttl 30 s (1 known)
//! disc: updated cam/jpeg cam-aabbcc node 246f28aabbcc0000 ttl 30 s (1 known)
//! disc: down cam/jpeg cam-aabbcc (goodbye, 0 known)
//! disc: down cam/jpeg cam-aabbcc (expired)
//! ```

use core::fmt;

use middleware::discovery::beacon::{
    BeaconError, beacon_key, decode, encode, kind_of, reannounce_after_ms,
};
use middleware::discovery::{Change, Directory, Endpoint, Event, Service, ServiceKind};
use middleware::espnow::{MAX_KEY, Mac};
use middleware::{Key, SenderId};

/// The kind a camera announces.
pub const CAM_KIND: ServiceKind<'static> = ServiceKind::from_static("cam/jpeg");

/// TTL of the camera beacon: re-announced every 15 s, so one lost beacon
/// does not expire it at a browser.
pub const BEACON_TTL_S: u32 = 30;

/// Records the directory holds.
pub const NEIGHBOURS: usize = 4;

/// Largest beacon payload this device sends: `cam/jpeg`, a 10-byte
/// instance, a node id and `key=` with a maximal ESP-NOW key.
pub const CAM_BEACON_LEN: usize = 3 + 4 + 8 + 1 + INSTANCE_LEN + 2 + TXT_LEN;

const INSTANCE_LEN: usize = 10;
const TXT_LEN: usize = 1 + 4 + MAX_KEY;

/// The instance name for `mac`: `cam-` and its last three bytes in hex.
pub fn instance(mac: Mac) -> [u8; INSTANCE_LEN] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = *b"cam-000000";
    for (i, b) in mac.iter().skip(3).enumerate() {
        if let Some(pair) = out.get_mut(4 + 2 * i..6 + 2 * i) {
            pair[0] = HEX[usize::from(b >> 4)];
            pair[1] = HEX[usize::from(b & 0xF)];
        }
    }
    out
}

/// Writes the camera beacon of the device `mac` streaming on `route` (an
/// announcement, TTL [`BEACON_TTL_S`]) or, with `route` `None`, its goodbye
/// (TTL 0): the beacon key into `key_out`, the payload into `out`.
/// Returns the key and the payload length.
pub fn cam_beacon<'k>(
    mac: Mac,
    route: Option<Key<'_>>,
    key_out: &'k mut [u8; MAX_KEY],
    out: &mut [u8; CAM_BEACON_LEN],
) -> Result<(Key<'k>, usize), BeaconError> {
    let name = instance(mac);
    let instance = core::str::from_utf8(&name).map_err(|_| BeaconError::Malformed)?;
    let mut txt = [0u8; TXT_LEN];
    let txt_len = match route {
        Some(key) => {
            let entry = 4 + key.as_bytes().len();
            let len = u8::try_from(entry).map_err(|_| BeaconError::Malformed)?;
            let slot = txt.get_mut(..1 + entry).ok_or(BeaconError::Malformed)?;
            let (head, body) = slot.split_at_mut(5);
            head.copy_from_slice(&[len, b'k', b'e', b'y', b'=']);
            body.copy_from_slice(key.as_bytes());
            1 + entry
        }
        None => 0,
    };
    let service = Service {
        kind: CAM_KIND,
        instance,
        node: Some(SenderId::from_mac(mac)),
        endpoint: Endpoint::Local,
        txt: txt.get(..txt_len).ok_or(BeaconError::Malformed)?,
        ttl_s: if route.is_some() { BEACON_TTL_S } else { 0 },
    };
    let n = encode(&service, out)?;
    Ok((beacon_key(CAM_KIND, key_out)?, n))
}

/// What the camera beacon must do now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeaconStep {
    /// Nothing.
    Wait,
    /// Send the announcement.
    Announce,
    /// Send the goodbye (the camera route stopped).
    Goodbye,
}

/// When the camera beacon was last sent, while it is announced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CamBeacon {
    last_ms: Option<u64>,
}

impl CamBeacon {
    /// Not announced.
    pub const fn new() -> Self {
        CamBeacon { last_ms: None }
    }

    /// Given whether a camera route runs at `now_ms`: the next state and
    /// what to send. An announcement counts as sent when decided (a failed
    /// send is not retried in a burst; the next comes after the interval).
    pub const fn step(self, camera: bool, now_ms: u64) -> (Self, BeaconStep) {
        let interval = reannounce_after_ms(BEACON_TTL_S);
        match (camera, self.last_ms) {
            (true, None) => (
                CamBeacon {
                    last_ms: Some(now_ms),
                },
                BeaconStep::Announce,
            ),
            (true, Some(t)) if now_ms >= t.saturating_add(interval) => (
                CamBeacon {
                    last_ms: Some(now_ms),
                },
                BeaconStep::Announce,
            ),
            (true, Some(_)) => (self, BeaconStep::Wait),
            (false, Some(_)) => (CamBeacon { last_ms: None }, BeaconStep::Goodbye),
            (false, None) => (self, BeaconStep::Wait),
        }
    }

    /// Milliseconds from `now_ms` until [`CamBeacon::step`] has something
    /// to do, or `None` while nothing is announced and no camera runs.
    pub const fn due_in(&self, camera: bool, now_ms: u64) -> Option<u64> {
        match (camera, self.last_ms) {
            (true, Some(t)) => Some(
                t.saturating_add(reannounce_after_ms(BEACON_TTL_S))
                    .saturating_sub(now_ms),
            ),
            (true, None) | (false, Some(_)) => Some(0),
            (false, None) => None,
        }
    }
}

/// Eight bytes of a node id in hex.
struct Hex(SenderId);

impl fmt::Display for Hex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// A beacon that changed the directory: the `sys/log` line (`Display`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Heard<'a> {
    /// What the directory did: `Added`, `Updated` or `Removed`.
    pub change: Change,
    /// The service's kind.
    pub kind: ServiceKind<'a>,
    /// Its instance.
    pub instance: &'a str,
    /// Its node (from the beacon, else the sample's sender).
    pub node: Option<SenderId>,
    /// Its TTL (0 for a goodbye).
    pub ttl_s: u32,
    /// Records held afterwards.
    pub known: usize,
}

impl fmt::Display for Heard<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (kind, instance, known) = (Safe(self.kind.as_str()), Safe(self.instance), self.known);
        match self.change {
            Change::Removed => write!(f, "disc: down {kind} {instance} (goodbye, {known} known)"),
            change => {
                let verb = if change == Change::Added {
                    "up"
                } else {
                    "updated"
                };
                write!(f, "disc: {verb} {kind} {instance}")?;
                if let Some(node) = self.node {
                    write!(f, " node {}", Hex(node))?;
                }
                write!(f, " ttl {} s ({known} known)", self.ttl_s)
            }
        }
    }
}

/// Radio-supplied text for the log: printable ASCII as is, every other
/// byte (control characters, escape sequences, non-ASCII) and `\` as
/// `\xNN`, so a beacon cannot inject terminal escapes or forge log lines.
struct Safe<'a>(&'a str);

impl fmt::Display for Safe<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for &b in self.0.as_bytes() {
            if (0x20..=0x7E).contains(&b) && b != b'\\' {
                write!(f, "{}", char::from(b))?;
            } else {
                write!(f, "\\x{b:02x}")?;
            }
        }
        Ok(())
    }
}

/// A record whose TTL ran out: its `sys/log` line (`Display`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Expired<'a> {
    /// The service's kind.
    pub kind: ServiceKind<'a>,
    /// Its instance.
    pub instance: &'a str,
}

impl fmt::Display for Expired<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "disc: down {} {} (expired)", Safe(self.kind.as_str()), Safe(self.instance))
    }
}

/// Records one sender may hold, of [`NEIGHBOURS`]: a single node (or a
/// node spoofing one MAC) cannot fill the directory.
pub const PER_SENDER: usize = 2;

/// The services this device has heard announced on the radio.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Neighbours {
    dir: Directory<NEIGHBOURS>,
    refused: u32,
}

impl Neighbours {
    /// Nothing heard.
    pub const fn new() -> Self {
        Neighbours {
            dir: Directory::new(),
            refused: 0,
        }
    }

    /// Beacons refused so far (wrong node, someone else's record, the
    /// sender's [`PER_SENDER`] cap, a full directory); saturates.
    pub const fn refused(&self) -> u32 {
        self.refused
    }

    fn refuse(self) -> Self {
        Neighbours { refused: self.refused.saturating_add(1), ..self }
    }

    /// Records held by `node`.
    fn held_by(&self, node: Option<SenderId>) -> usize {
        self.dir.iter().filter(|s| s.node == node).count()
    }

    /// Folds one received sample on `key` (from `sender`, at `now_ms`) into
    /// the directory. Returns what to log: `None` for a sample that is not
    /// a valid beacon, a refresh, a goodbye for an unknown record, or a
    /// refused beacon (counted in [`Neighbours::refused`], not logged, so
    /// a flood of them cannot flood `sys/log`).
    ///
    /// Records are bound to the frame's sender: when the transport names it
    /// (ESP-NOW always does, by source MAC), it is the record's node and a
    /// beacon declaring another node is refused; an existing record is only
    /// updated or withdrawn by the sender that created it; and one sender
    /// holds at most [`PER_SENDER`] records. The source MAC is not
    /// authenticated (a radio can forge it), so this stops one node from
    /// claiming another's identity or records in passing, not a determined
    /// spoofer.
    pub fn hear<'p>(
        self,
        key: Key<'p>,
        payload: &'p [u8],
        sender: Option<SenderId>,
        now_ms: u64,
    ) -> (Self, Option<Heard<'p>>) {
        let Some(service) = kind_of(key).and_then(|kind| decode(kind, payload).ok()) else {
            return (self, None);
        };
        let node = match (sender, service.node) {
            (Some(s), Some(n)) if n != s => return (self.refuse(), None),
            (Some(s), _) => Some(s),
            (None, n) => n,
        };
        let service = Service { node, ..service };
        match self.dir.get(service.kind, service.instance) {
            Some(known) if known.node != node => return (self.refuse(), None),
            None if service.ttl_s > 0 && self.held_by(node) >= PER_SENDER => {
                return (self.refuse(), None);
            }
            _ => {}
        }
        let refused = self.refused;
        let (dir, change) = self.dir.observe(&Event::Up(service), now_ms);
        if change == Change::Full {
            return (Neighbours { dir, refused }.refuse(), None);
        }
        let known = dir.len();
        let heard = matches!(change, Change::Added | Change::Updated | Change::Removed)
        .then_some(Heard {
            change,
            kind: service.kind,
            instance: service.instance,
            node: service.node,
            ttl_s: service.ttl_s,
            known,
        });
        (Neighbours { dir, refused }, heard)
    }

    /// The records whose TTL has run out at `now_ms`, for the log.
    pub fn expired(&self, now_ms: u64) -> impl Iterator<Item = Expired<'_>> {
        self.dir.expired(now_ms).map(|s| Expired {
            kind: s.kind,
            instance: s.instance,
        })
    }

    /// The directory without the records expired at `now_ms`.
    pub fn expire(self, now_ms: u64) -> Self {
        Neighbours {
            dir: self.dir.expire(now_ms).0,
            ..self
        }
    }

    /// Milliseconds from `now_ms` until the next record expires.
    pub fn expires_in(&self, now_ms: u64) -> Option<u64> {
        self.dir
            .next_expiry(now_ms)
            .map(|t| t.saturating_sub(now_ms))
    }

    /// The services known.
    pub fn services(&self) -> impl Iterator<Item = Service<'_>> {
        self.dir.iter()
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::string::ToString;

    const CAM_MAC: Mac = [0x24, 0x6F, 0x28, 0xAA, 0xBB, 0xCC];
    const BADGE: SenderId = SenderId::from_mac([0x02, 1, 2, 3, 4, 5]);
    const ROUTE: Key<'static> = Key::from_static("app/cam/jpeg");

    fn beacon(route: Option<Key<'_>>) -> ([u8; MAX_KEY], [u8; CAM_BEACON_LEN], usize, usize) {
        let mut key = [0u8; MAX_KEY];
        let mut out = [0u8; CAM_BEACON_LEN];
        let (k, n) = cam_beacon(CAM_MAC, route, &mut key, &mut out).expect("encodes");
        let k_len = k.as_str().len();
        (key, out, k_len, n)
    }

    fn key_of(buf: &[u8; MAX_KEY], len: usize) -> Key<'_> {
        Key::new(core::str::from_utf8(&buf[..len]).unwrap()).unwrap()
    }

    #[test]
    fn e19_cam_beacon_names_kind_instance_node_and_route() {
        assert_eq!(&instance(CAM_MAC), b"cam-aabbcc");
        let (key, out, k, n) = beacon(Some(ROUTE));
        assert_eq!(key_of(&key, k).as_str(), "disc/cam/jpeg");
        let s = decode(CAM_KIND, &out[..n]).unwrap();
        assert_eq!(s.instance, "cam-aabbcc");
        assert_eq!(s.node, Some(SenderId::from_mac(CAM_MAC)));
        assert_eq!(s.endpoint, Endpoint::Local);
        assert_eq!(s.ttl_s, BEACON_TTL_S);
        assert_eq!(s.txt().unwrap().get(b"key"), Some(&b"app/cam/jpeg"[..]));
        // Fits one ESP-NOW frame with its key, with room to spare.
        assert!(k + n <= middleware::espnow::LIMITS.max_message);
        // The largest route key still fits the buffer.
        let text = std::format!("app/{}", "a".repeat(MAX_KEY - 4));
        let long = Key::new(&text).unwrap();
        assert_eq!(long.as_str().len(), MAX_KEY);
        let mut kb = [0u8; MAX_KEY];
        let mut ob = [0u8; CAM_BEACON_LEN];
        assert!(cam_beacon(CAM_MAC, Some(long), &mut kb, &mut ob).is_ok());
        // The goodbye: TTL 0, no TXT.
        let (_, out, _, n) = beacon(None);
        let g = decode(CAM_KIND, &out[..n]).unwrap();
        assert_eq!((g.ttl_s, g.txt), (0, &[][..]));
    }

    #[test]
    fn e19_cam_beacon_runs_with_the_camera_route() {
        let half = reannounce_after_ms(BEACON_TTL_S);
        assert_eq!(half, 15_000);
        let b = CamBeacon::new();
        assert_eq!(b.step(false, 0), (b, BeaconStep::Wait));
        assert_eq!(b.due_in(false, 0), None);
        assert_eq!(b.due_in(true, 7), Some(0));
        let (b, s) = b.step(true, 1_000);
        assert_eq!(s, BeaconStep::Announce);
        assert_eq!(b.due_in(true, 1_000), Some(half));
        assert_eq!(b.step(true, 1_000 + half - 1).1, BeaconStep::Wait);
        let (b, s) = b.step(true, 1_000 + half);
        assert_eq!(s, BeaconStep::Announce);
        // The route stops: one goodbye, then nothing.
        assert_eq!(b.due_in(false, 0), Some(0));
        let (b, s) = b.step(false, 20_000);
        assert_eq!(s, BeaconStep::Goodbye);
        assert_eq!(b.step(false, 30_000).1, BeaconStep::Wait);
    }

    #[test]
    fn e20_neighbours_log_up_refresh_down_and_expiry() {
        let (key, out, k, n) = beacon(Some(ROUTE));
        let key = key_of(&key, k);
        let cam = Some(SenderId::from_mac(CAM_MAC));
        let (d, heard) = Neighbours::new().hear(key, &out[..n], cam, 1_000);
        let heard = heard.expect("new record");
        assert_eq!(heard.change, Change::Added);
        assert_eq!(
            heard.to_string(),
            "disc: up cam/jpeg cam-aabbcc node 246f28aabbcc0000 ttl 30 s (1 known)"
        );
        // A refresh is silent and restarts the TTL.
        let (d, heard) = d.hear(key, &out[..n], cam, 20_000);
        assert_eq!(heard, None);
        assert_eq!(d.expires_in(20_000), Some(30_000));
        // Expiry, 30 s after the last beacon.
        assert_eq!(d.expired(49_999).count(), 0);
        let lines: std::vec::Vec<_> = d.expired(50_000).map(|e| e.to_string()).collect();
        assert_eq!(lines, ["disc: down cam/jpeg cam-aabbcc (expired)"]);
        let d = d.expire(50_000);
        assert_eq!(d.services().count(), 0);
        // A goodbye removes the record at once.
        let (d, _) = d.hear(key, &out[..n], None, 60_000);
        let (bye_key, bye, bk, bn) = beacon(None);
        let (d, heard) = d.hear(key_of(&bye_key, bk), &bye[..bn], None, 61_000);
        assert_eq!(
            heard.expect("removed").to_string(),
            "disc: down cam/jpeg cam-aabbcc (goodbye, 0 known)"
        );
        assert_eq!(d.services().count(), 0);
    }

    #[test]
    fn e20_neighbours_fill_in_the_sender_and_ignore_non_beacons() {
        let mut key = [0u8; MAX_KEY];
        let k = beacon_key(ServiceKind::from_static("led/sync"), &mut key).unwrap();
        let mut out = [0u8; 64];
        let svc = Service {
            kind: ServiceKind::from_static("led/sync"),
            instance: "b1",
            node: None,
            endpoint: Endpoint::Local,
            txt: &[],
            ttl_s: 10,
        };
        let n = encode(&svc, &mut out).unwrap();
        let (d, heard) = Neighbours::new().hear(k, &out[..n], Some(BADGE), 0);
        let heard = heard.unwrap();
        assert_eq!(heard.node, Some(BADGE));
        assert_eq!(
            heard.to_string(),
            "disc: up led/sync b1 node 0201020304050000 ttl 10 s (1 known)"
        );
        // Not a beacon key, or a malformed beacon: nothing.
        let (d, heard) = d.hear(Key::from_static("app/x"), &out[..n], None, 0);
        assert_eq!(heard, None);
        let (d, heard) = d.hear(k, b"\x09junk", None, 0);
        assert_eq!(heard, None);
        // A changed record is logged as updated.
        let n = encode(&Service { ttl_s: 20, ..svc }, &mut out).unwrap();
        let (d, heard) = d.hear(k, &out[..n], Some(BADGE), 0);
        assert_eq!(heard.unwrap().change, Change::Updated);
        assert_eq!(d.services().count(), 1);
    }

    /// A beacon for `instance` of [`CAM_KIND`] declaring `node`.
    fn svc_beacon(instance: &str, node: Option<SenderId>, ttl_s: u32, out: &mut [u8; 64]) -> usize {
        let svc = Service { kind: CAM_KIND, instance, node, endpoint: Endpoint::Local, txt: &[], ttl_s };
        encode(&svc, out).unwrap()
    }

    #[test]
    fn e21_records_are_bound_to_the_frame_sender() {
        let mut key = [0u8; MAX_KEY];
        let k = beacon_key(CAM_KIND, &mut key).unwrap();
        let mut out = [0u8; 64];
        let cam = SenderId::from_mac(CAM_MAC);
        // A frame from the badge's MAC claiming to be the camera: refused.
        let n = svc_beacon("cam-1", Some(cam), 30, &mut out);
        let (d, heard) = Neighbours::new().hear(k, &out[..n], Some(BADGE), 0);
        assert_eq!((heard, d.refused(), d.services().count()), (None, 1, 0));
        // From the camera itself: accepted, bound to its MAC.
        let (d, heard) = d.hear(k, &out[..n], Some(cam), 0);
        assert_eq!(heard.unwrap().node, Some(cam));
        // A beacon without a node id takes the sender's.
        let n = svc_beacon("cam-2", None, 30, &mut out);
        let (d, heard) = d.hear(k, &out[..n], Some(cam), 0);
        assert_eq!(heard.unwrap().node, Some(cam));
        // Another sender can neither update nor withdraw the camera's record.
        let n = svc_beacon("cam-1", None, 99, &mut out);
        let (d, heard) = d.hear(k, &out[..n], Some(BADGE), 0);
        assert_eq!((heard, d.refused()), (None, 2));
        let n = svc_beacon("cam-1", None, 0, &mut out);
        let (d, heard) = d.hear(k, &out[..n], Some(BADGE), 0);
        assert_eq!((heard, d.refused(), d.services().count()), (None, 3, 2));
        // The owner can.
        let (d, heard) = d.hear(k, &out[..n], Some(cam), 0);
        assert_eq!(heard.unwrap().change, Change::Removed);
        assert_eq!(d.services().count(), 1);
    }

    #[test]
    fn e21_one_sender_cannot_fill_the_directory() {
        let mut key = [0u8; MAX_KEY];
        let k = beacon_key(CAM_KIND, &mut key).unwrap();
        let mut out = [0u8; 64];
        let flooder = Some(SenderId::from_mac([2, 0, 0, 0, 0, 0xEE]));
        let mut d = Neighbours::new();
        for name in ["f0", "f1", "f2", "f3", "f4"] {
            let n = svc_beacon(name, None, 30, &mut out);
            d = d.hear(k, &out[..n], flooder, 0).0;
        }
        assert_eq!((d.services().count(), d.refused()), (PER_SENDER, 3));
        // Room is left for others; once the directory is full, a new record
        // is refused silently, never by evicting someone else's.
        for (i, name) in ["a", "b", "c"].into_iter().enumerate() {
            let n = svc_beacon(name, None, 30, &mut out);
            let sender = Some(SenderId::from_mac([2, 0, 0, 0, 1, i as u8]));
            let (next, heard) = d.hear(k, &out[..n], sender, 0);
            assert_eq!(heard.is_some(), i < NEIGHBOURS - PER_SENDER, "{name}");
            d = next;
        }
        assert_eq!((d.services().count(), d.refused()), (NEIGHBOURS, 4));
    }

    #[test]
    fn e22_log_lines_escape_radio_supplied_text() {
        let mut key = [0u8; MAX_KEY];
        let k = beacon_key(CAM_KIND, &mut key).unwrap();
        let mut out = [0u8; 64];
        let n = svc_beacon("x\x1b[2J\r\ndisc: up \\ \u{e9}", None, 30, &mut out);
        let (d, heard) = Neighbours::new().hear(k, &out[..n], Some(BADGE), 0);
        let line = heard.unwrap().to_string();
        assert_eq!(
            line,
            "disc: up cam/jpeg x\\x1b[2J\\x0d\\x0adisc: up \\x5c \\xc3\\xa9 node 0201020304050000 ttl 30 s (1 known)"
        );
        assert!(line.bytes().all(|b| (0x20..=0x7E).contains(&b)));
        let expired: std::vec::Vec<_> = d.expired(30_000).map(|e| e.to_string()).collect();
        assert_eq!(expired, ["disc: down cam/jpeg x\\x1b[2J\\x0d\\x0adisc: up \\x5c \\xc3\\xa9 (expired)"]);
    }
}
