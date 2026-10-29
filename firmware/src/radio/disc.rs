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
//! decoded with [`decode`], the sample's sender filling in a missing node
//! id, and folded into a [`Directory`] of [`NEIGHBOURS`] records. A new
//! record, a changed one, a goodbye and a full directory return a
//! [`Heard`] whose `Display` is the `sys/log` line; refreshes are silent.
//! [`Neighbours::expired`] lists the records whose TTL has run out (their
//! [`Expired`] lines) before [`Neighbours::expire`] drops them. The lines:
//!
//! ```text
//! disc: up cam/jpeg cam-aabbcc node 246f28aabbcc0000 ttl 30 s (1 known)
//! disc: updated cam/jpeg cam-aabbcc node 246f28aabbcc0000 ttl 30 s (1 known)
//! disc: down cam/jpeg cam-aabbcc (goodbye, 0 known)
//! disc: down cam/jpeg cam-aabbcc (expired)
//! disc: full, cam/jpeg cam-aabbcc not kept (4 known)
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
    /// What the directory did: `Added`, `Updated`, `Removed` or `Full`.
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
        let (kind, instance, known) = (self.kind, self.instance, self.known);
        match self.change {
            Change::Removed => write!(f, "disc: down {kind} {instance} (goodbye, {known} known)"),
            Change::Full => write!(f, "disc: full, {kind} {instance} not kept ({known} known)"),
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
        write!(f, "disc: down {} {} (expired)", self.kind, self.instance)
    }
}

/// The services this device has heard announced on the radio.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Neighbours {
    dir: Directory<NEIGHBOURS>,
}

impl Neighbours {
    /// Nothing heard.
    pub const fn new() -> Self {
        Neighbours {
            dir: Directory::new(),
        }
    }

    /// Folds one received sample on `key` (from `sender`, at `now_ms`) into
    /// the directory. Returns what to log: `None` for a sample that is not
    /// a valid beacon, a refresh, a goodbye for an unknown record, or a
    /// record over the directory's bounds.
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
        let service = Service {
            node: service.node.or(sender),
            ..service
        };
        let (dir, change) = self.dir.observe(&Event::Up(service), now_ms);
        let known = dir.len();
        let heard = matches!(
            change,
            Change::Added | Change::Updated | Change::Removed | Change::Full
        )
        .then_some(Heard {
            change,
            kind: service.kind,
            instance: service.instance,
            node: service.node,
            ttl_s: service.ttl_s,
            known,
        });
        (Neighbours { dir }, heard)
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
        let (d, heard) = Neighbours::new().hear(key, &out[..n], Some(BADGE), 1_000);
        let heard = heard.expect("new record");
        assert_eq!(heard.change, Change::Added);
        assert_eq!(
            heard.to_string(),
            "disc: up cam/jpeg cam-aabbcc node 246f28aabbcc0000 ttl 30 s (1 known)"
        );
        // A refresh is silent and restarts the TTL.
        let (d, heard) = d.hear(key, &out[..n], Some(BADGE), 20_000);
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

    #[test]
    fn e20_a_full_directory_says_so() {
        let mut d = Neighbours::new();
        let mut out = [0u8; 64];
        let mut key = [0u8; MAX_KEY];
        let k = beacon_key(CAM_KIND, &mut key).unwrap();
        let names = ["c0", "c1", "c2", "c3", "c4"];
        let mut last = None;
        for name in names {
            let svc = Service {
                kind: CAM_KIND,
                instance: name,
                node: None,
                endpoint: Endpoint::Local,
                txt: &[],
                ttl_s: 10,
            };
            let n = encode(&svc, &mut out).unwrap();
            let (next, heard) = d.hear(k, &out[..n], None, 0);
            d = next;
            last = heard.map(|h| h.to_string());
        }
        assert_eq!(
            last.as_deref(),
            Some("disc: full, cam/jpeg c4 not kept (4 known)")
        );
    }
}
