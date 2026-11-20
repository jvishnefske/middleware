//! The ESP-NOW gateway's pure core: which devices were heard ([`Heard`]),
//! the beacons they announce ([`Registry`], the E21 rules), the downlink
//! loop guard ([`skip_downlink`]), the system keys forwarded to devices
//! ([`forwarded`]) and the ids and error mapping of the shell
//! ([`super`]). No I/O, no clock: times are passed in.

use middleware::discovery::beacon::{decode, kind_of};
use middleware::discovery::directory::{Change, Directory, ServiceRecord};
use middleware::discovery::{Event, Service};
use middleware::espnow::Mac;
use middleware::espnow::rpc::RpcClientError;
use middleware::key::Key;
use middleware::loader::keys;
use middleware::loader::peers::{MAX_PEERS, PeerSet};
use middleware::transport::{HandlerError, SenderId};

/// A device not heard for this long is forgotten.
pub const HEARD_TTL_MS: u64 = 60_000;
/// Most devices remembered; when full the stalest is forgotten.
pub const MAX_HEARD: usize = 64;
/// Most devices given per-node queryables (bounds Zenoh declarations).
pub const MAX_SERVED: usize = 32;
/// Beacon records the gateway announces in all.
pub const MAX_RECORDS: usize = 16;
/// Records one device may hold (E21).
pub const PER_SENDER: usize = 2;

/// A device's node id: its MAC and two zero bytes ([`SenderId::from_mac`]).
pub const fn sender_id(mac: Mac) -> SenderId {
    SenderId::from_mac(mac)
}

/// The MAC of a MAC-derived id (its last two bytes are zero), else `None`.
pub fn mac_of(id: SenderId) -> Option<Mac> {
    let [a, b, c, d, e, f, 0, 0] = id.0 else { return None };
    Some([a, b, c, d, e, f])
}

/// The publication stream origin of a device's uplink: its MAC's last four
/// bytes, so streams are stable across gateway restarts.
pub const fn origin_of(mac: Mac) -> u32 {
    u32::from_be_bytes([mac[2], mac[3], mac[4], mac[5]])
}

/// The system keys forwarded to a device as ESP-NOW requests: its
/// descriptions and the signed loader and payload control keys (the device
/// checks the signatures). `sys/trust/claim` is not among them: a key is
/// pinned over the device's own serial port, never over the network.
pub const FORWARDED: [Key<'static>; 12] = [
    keys::INFO,
    keys::BOARD,
    keys::MANIFEST,
    keys::CONTROL_MODULES,
    keys::TRUST_INFO,
    keys::BEGIN,
    keys::CHUNK,
    keys::COMMIT,
    keys::START,
    keys::STOP,
    keys::BIND,
    keys::BINDING,
];

/// True if requests on `key` are forwarded to devices.
pub fn forwarded(key: Key<'_>) -> bool {
    FORWARDED.iter().any(|k| k.as_str() == key.as_str())
}

/// The Zenoh reply error for a failed forwarded request.
pub fn reply_error_for<E>(e: &RpcClientError<E>) -> HandlerError {
    match e {
        RpcClientError::Timeout => HandlerError::Unavailable,
        RpcClientError::Remote(e) => *e,
        RpcClientError::Frame(_) | RpcClientError::WrongTarget => HandlerError::InvalidRequest,
        RpcClientError::BufferTooSmall => HandlerError::BufferTooSmall,
        RpcClientError::Radio(_) => HandlerError::Internal,
    }
}

/// True if a downlink sample from `sender` must not be broadcast: it was
/// published under a heard device's id (another gateway republishing this
/// radio's uplink), so broadcasting it would loop.
pub fn skip_downlink(sender: Option<SenderId>, heard: &Heard) -> bool {
    sender.and_then(mac_of).is_some_and(|mac| heard.contains(mac))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Device {
    mac: Mac,
    first_ms: u64,
    last_ms: u64,
    frames: u32,
}

/// What [`Heard::hear`] changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Hearing {
    /// The device was not known.
    pub new: bool,
    /// The stalest device, forgotten to make room.
    pub evicted: Option<Mac>,
}

/// The devices heard, at most [`MAX_HEARD`], each forgotten
/// [`HEARD_TTL_MS`] after its last frame.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Heard {
    devices: Vec<Device>,
}

impl Heard {
    /// Nobody heard.
    pub const fn new() -> Self {
        Heard { devices: Vec::new() }
    }

    /// Records a frame from `mac` at `now_ms`.
    pub fn hear(&mut self, mac: Mac, now_ms: u64) -> Hearing {
        if let Some(d) = self.devices.iter_mut().find(|d| d.mac == mac) {
            d.last_ms = now_ms;
            d.frames = d.frames.saturating_add(1);
            return Hearing::default();
        }
        let mut evicted = None;
        if self.devices.len() >= MAX_HEARD
            && let Some(i) = self.devices.iter().enumerate().min_by_key(|(_, d)| d.last_ms).map(|(i, _)| i)
        {
            evicted = Some(self.devices.remove(i).mac);
        }
        self.devices.push(Device { mac, first_ms: now_ms, last_ms: now_ms, frames: 1 });
        Hearing { new: true, evicted }
    }

    /// Forgets the devices unheard for [`HEARD_TTL_MS`] at `now_ms`;
    /// returns them.
    pub fn expire(&mut self, now_ms: u64) -> Vec<Mac> {
        let (gone, kept) =
            self.devices.drain(..).partition(|d: &Device| now_ms.saturating_sub(d.last_ms) >= HEARD_TTL_MS);
        self.devices = kept;
        gone.into_iter().map(|d| d.mac).collect()
    }

    /// True if `mac` is heard.
    pub fn contains(&self, mac: Mac) -> bool {
        self.devices.iter().any(|d| d.mac == mac)
    }

    /// Devices heard.
    pub fn len(&self) -> usize {
        self.devices.len()
    }

    /// True when nobody is heard.
    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }

    /// Frames heard from `mac`.
    pub fn frames(&self, mac: Mac) -> Option<u32> {
        self.devices.iter().find(|d| d.mac == mac).map(|d| d.frames)
    }

    /// The devices that get per-node queryables: the [`MAX_SERVED`] heard
    /// longest, in that order.
    pub fn served(&self) -> Vec<Mac> {
        let mut v: Vec<&Device> = self.devices.iter().collect();
        v.sort_by_key(|d| (d.first_ms, d.mac));
        v.into_iter().take(MAX_SERVED).map(|d| d.mac).collect()
    }

    /// The heard list: the [`MAX_PEERS`] most recently heard ids, oldest
    /// first ([`PeerSet`], as `sys/peers` carries it).
    pub fn peers(&self) -> PeerSet {
        let mut v: Vec<&Device> = self.devices.iter().collect();
        v.sort_by_key(|d| (d.last_ms, d.mac));
        let skip = v.len().saturating_sub(MAX_PEERS);
        let mut set = PeerSet::new();
        for d in v.into_iter().skip(skip) {
            set.record(sender_id(d.mac));
        }
        set
    }
}

/// Why a beacon was refused (E21).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The payload is not a beacon of the key's kind.
    NotABeacon,
    /// It declares a node other than its sender.
    OtherNode,
    /// The record belongs to another device.
    NotOwner,
    /// The device already holds [`PER_SENDER`] records.
    PerSender,
    /// Every record slot is taken (no eviction).
    Full,
}

/// What to do on Zenoh after a beacon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Announce (or replace) this record.
    Announce(ServiceRecord),
    /// Withdraw this record (a goodbye).
    Withdraw(ServiceRecord),
    /// A refresh, or a goodbye for an unknown record.
    Nothing,
    /// Refused and counted.
    Refused(Refusal),
}

/// The beacon records heard over the radio, bound to their senders (E21,
/// as the ESP base firmware's `radio::disc::Neighbours`, with `N` records).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registry<const N: usize = MAX_RECORDS> {
    dir: Directory<N>,
    refused: u32,
}

impl<const N: usize> Default for Registry<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Registry<N> {
    /// No records.
    pub const fn new() -> Self {
        Registry { dir: Directory::new(), refused: 0 }
    }

    /// Beacons refused so far; saturates.
    pub const fn refused(&self) -> u32 {
        self.refused
    }

    fn refuse(&mut self, why: Refusal) -> Action {
        self.refused = self.refused.saturating_add(1);
        Action::Refused(why)
    }

    /// Folds a `disc/<kind>` sample from `mac` at `now_ms` in. The sender
    /// is the record's node: a beacon declaring another node is refused, a
    /// record is updated or withdrawn only by the device that created it,
    /// one device holds at most [`PER_SENDER`] records, and a full registry
    /// refuses new records rather than evict.
    pub fn observe(&mut self, mac: Mac, key: Key<'_>, payload: &[u8], now_ms: u64) -> Action {
        let node = sender_id(mac);
        let Some(service) = kind_of(key).and_then(|kind| decode(kind, payload).ok()) else {
            return self.refuse(Refusal::NotABeacon);
        };
        if service.node.is_some_and(|n| n != node) {
            return self.refuse(Refusal::OtherNode);
        }
        let service = Service { node: Some(node), ..service };
        match self.dir.get(service.kind, service.instance) {
            Some(known) if known.node != Some(node) => return self.refuse(Refusal::NotOwner),
            None if service.ttl_s > 0 && self.dir.iter().filter(|s| s.node == Some(node)).count() >= PER_SENDER => {
                return self.refuse(Refusal::PerSender);
            }
            _ => {}
        }
        let removed = self.dir.get(service.kind, service.instance).and_then(|s| ServiceRecord::new(&s));
        let dir = std::mem::replace(&mut self.dir, Directory::new());
        let (dir, change) = dir.observe(&Event::Up(service), now_ms);
        self.dir = dir;
        match change {
            Change::Added | Change::Updated => ServiceRecord::new(&service).map_or(Action::Nothing, Action::Announce),
            Change::Removed => removed.map_or(Action::Nothing, Action::Withdraw),
            Change::Full => self.refuse(Refusal::Full),
            Change::Rejected => self.refuse(Refusal::NotABeacon),
            Change::Refreshed | Change::Ignored => Action::Nothing,
        }
    }

    /// Removes the records whose TTL ran out at `now_ms`; returns them to
    /// withdraw.
    pub fn expire(&mut self, now_ms: u64) -> Vec<ServiceRecord> {
        let gone: Vec<ServiceRecord> = self.dir.expired(now_ms).filter_map(|s| ServiceRecord::new(&s)).collect();
        let dir = std::mem::replace(&mut self.dir, Directory::new());
        self.dir = dir.expire(now_ms).0;
        gone
    }

    /// Every record held.
    pub fn records(&self) -> Vec<ServiceRecord> {
        self.dir.iter().filter_map(|s| ServiceRecord::new(&s)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::discovery::beacon::encode;
    use middleware::discovery::{Endpoint, ServiceKind};

    const DEV: Mac = [0xe4, 0xb0, 0x63, 0x81, 0xd3, 0x24];
    const OTHER: Mac = [0x24, 0x6f, 0x28, 0xaa, 0xbb, 0xcc];
    const KIND: ServiceKind<'static> = ServiceKind::from_static("mw/node");
    const KEY: Key<'static> = Key::from_static("disc/mw/node");

    fn beacon(instance: &str, node: Option<SenderId>, ttl_s: u32) -> Vec<u8> {
        let s = Service { kind: KIND, instance, node, endpoint: Endpoint::Local, txt: &[], ttl_s };
        let mut out = [0u8; 256];
        let n = encode(&s, &mut out).unwrap();
        out[..n].to_vec()
    }

    fn announced(a: &Action) -> Option<(String, Option<SenderId>)> {
        match a {
            Action::Announce(r) => Some((r.service().instance.to_owned(), r.service().node)),
            _ => None,
        }
    }

    #[test]
    fn eg6_ids_are_mac_derived() {
        let id = sender_id(DEV);
        assert_eq!(id.0, [0xe4, 0xb0, 0x63, 0x81, 0xd3, 0x24, 0, 0]);
        assert_eq!(&middleware::zenoh::wire::zid16_hex(id), b"e4b06381d3240000");
        assert_eq!(mac_of(id), Some(DEV));
        assert_eq!(mac_of(SenderId([1, 2, 3, 4, 5, 6, 7, 8])), None);
        assert_eq!(origin_of(DEV), 0x6381_d324);
    }

    #[test]
    fn eg7_a_beacon_is_announced_with_the_sender_as_node() {
        let mut r: Registry = Registry::new();
        let a = r.observe(DEV, KEY, &beacon("dev-1", None, 30), 0);
        assert_eq!(announced(&a), Some(("dev-1".to_owned(), Some(sender_id(DEV)))));
        // Declaring itself is fine; a refresh does nothing.
        let a = r.observe(DEV, KEY, &beacon("dev-1", Some(sender_id(DEV)), 30), 10);
        assert_eq!(a, Action::Nothing);
        // A changed TTL is an update, announced again.
        assert!(announced(&r.observe(DEV, KEY, &beacon("dev-1", None, 60), 20)).is_some());
        // A goodbye withdraws it.
        let a = r.observe(DEV, KEY, &beacon("dev-1", None, 0), 30);
        assert!(matches!(a, Action::Withdraw(ref rec) if rec.service().instance == "dev-1"), "{a:?}");
        assert!(r.records().is_empty());
        assert_eq!(r.refused(), 0);
    }

    #[test]
    fn eg7_records_are_bound_to_the_frame_sender() {
        let mut r: Registry = Registry::new();
        let a = r.observe(DEV, KEY, &beacon("dev-1", Some(sender_id(OTHER)), 30), 0);
        assert_eq!(a, Action::Refused(Refusal::OtherNode));
        assert!(announced(&r.observe(DEV, KEY, &beacon("dev-1", None, 30), 0)).is_some());
        // Another device cannot update or withdraw it.
        assert_eq!(r.observe(OTHER, KEY, &beacon("dev-1", None, 60), 1), Action::Refused(Refusal::NotOwner));
        assert_eq!(r.observe(OTHER, KEY, &beacon("dev-1", None, 0), 1), Action::Refused(Refusal::NotOwner));
        assert_eq!(r.records().len(), 1);
        // Not a beacon.
        assert_eq!(r.observe(DEV, KEY, b"junk", 1), Action::Refused(Refusal::NotABeacon));
        assert_eq!(
            r.observe(DEV, Key::from_static("app/x"), &beacon("x", None, 30), 1),
            Action::Refused(Refusal::NotABeacon)
        );
        assert_eq!(r.refused(), 5);
    }

    #[test]
    fn eg7_one_sender_cannot_fill_the_registry() {
        let mut r: Registry<3> = Registry::new();
        assert!(announced(&r.observe(DEV, KEY, &beacon("a", None, 30), 0)).is_some());
        assert!(announced(&r.observe(DEV, KEY, &beacon("b", None, 30), 0)).is_some());
        assert_eq!(r.observe(DEV, KEY, &beacon("c", None, 30), 0), Action::Refused(Refusal::PerSender));
        assert!(announced(&r.observe(OTHER, KEY, &beacon("d", None, 30), 0)).is_some());
        // Full: refused, nothing evicted.
        let third = [2, 0, 0, 0, 0, 3];
        assert_eq!(r.observe(third, KEY, &beacon("e", None, 30), 0), Action::Refused(Refusal::Full));
        assert_eq!(r.records().len(), 3);
    }

    #[test]
    fn eg7_expired_records_are_withdrawn() {
        let mut r: Registry = Registry::new();
        r.observe(DEV, KEY, &beacon("short", None, 2), 0);
        r.observe(DEV, KEY, &beacon("long", None, 60), 0);
        assert!(r.expire(1_999).is_empty());
        let gone = r.expire(2_000);
        assert_eq!(gone.len(), 1);
        assert_eq!(gone[0].service().instance, "short");
        assert_eq!(r.records().len(), 1);
    }

    #[test]
    fn eg8_loop_guard() {
        let mut heard = Heard::new();
        heard.hear(DEV, 0);
        assert!(skip_downlink(Some(sender_id(DEV)), &heard));
        assert!(!skip_downlink(Some(sender_id(OTHER)), &heard));
        assert!(!skip_downlink(Some(SenderId([0xe4, 0xb0, 0x63, 0x81, 0xd3, 0x24, 1, 2])), &heard));
        assert!(!skip_downlink(None, &heard));
    }

    #[test]
    fn eg9_forward_allowlist() {
        for k in [
            "sys/info",
            "sys/manifest",
            "sys/loader/begin",
            "sys/loader/chunk",
            "sys/loader/commit",
            "sys/payload/start",
            "sys/payload/stop",
            "sys/payload/bind",
            "sys/trust/info",
        ] {
            assert!(forwarded(Key::new(k).unwrap()), "{k}");
        }
        for k in ["sys/trust/claim", "sys/net/addr", "sys/peers", "sys/fw/begin", "app/x", "sys/log"] {
            assert!(!forwarded(Key::new(k).unwrap()), "{k}");
        }
        assert_eq!(reply_error_for::<()>(&RpcClientError::Timeout), HandlerError::Unavailable);
        assert_eq!(reply_error_for::<()>(&RpcClientError::Remote(HandlerError::Denied)), HandlerError::Denied);
        assert_eq!(reply_error_for::<()>(&RpcClientError::BufferTooSmall), HandlerError::BufferTooSmall);
        assert_eq!(reply_error_for::<()>(&RpcClientError::WrongTarget), HandlerError::InvalidRequest);
        assert_eq!(reply_error_for(&RpcClientError::Radio(())), HandlerError::Internal);
    }

    #[test]
    fn eg10_heard_expiry_and_peerset() {
        let mut h = Heard::new();
        assert_eq!(h.hear(DEV, 0), Hearing { new: true, evicted: None });
        assert_eq!(h.hear(OTHER, 1_000), Hearing { new: true, evicted: None });
        assert_eq!(h.hear(DEV, 30_000), Hearing::default());
        assert_eq!(h.frames(DEV), Some(2));
        let ids: Vec<SenderId> = h.peers().iter().collect();
        assert_eq!(ids, [sender_id(OTHER), sender_id(DEV)]);
        assert_eq!(h.served(), [DEV, OTHER]);
        assert!(h.expire(60_999).is_empty());
        assert_eq!(h.expire(61_000), [OTHER]);
        assert_eq!(h.len(), 1);
        assert_eq!(h.expire(90_000), [DEV]);
        assert!(h.is_empty());
    }

    #[test]
    fn eg10_heard_is_bounded() {
        let mut h = Heard::new();
        for i in 0..MAX_HEARD as u8 {
            h.hear([2, 0, 0, 0, 0, i], u64::from(i) + 10);
        }
        // The stalest goes.
        assert_eq!(h.hear(DEV, 1_000).evicted, Some([2, 0, 0, 0, 0, 0]));
        assert_eq!(h.len(), MAX_HEARD);
        assert_eq!(h.served().len(), MAX_SERVED);
        assert_eq!(h.peers().iter().count(), MAX_PEERS);
        assert_eq!(h.peers().iter().last(), Some(sender_id(DEV)));
    }
}
