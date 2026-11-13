//! The gateway's pure core: what to do with each frame from the device,
//! each sample from the cluster and each `gw/sys` request. No I/O; the
//! shell ([`crate::shell`]) performs what these functions decide.
//!
//! # Key mapping
//!
//! A [`KeyMap`] translates between the device's key space and the
//! cluster's. Without a namespace (the default) the keys are the same, so
//! the ESP32-CAM's `app/cam/jpeg` is `app/cam/jpeg` on Zenoh too. With the
//! namespace `<ns>` (one key segment, `MW_GATEWAY_NAMESPACE`) the device's
//! `app/<rest>` is the cluster's `app/<ns>/<rest>` and the reverse, so
//! two gateways with identical devices do not collide. Only `app` and the
//! keys below it map; everything else stays on the device.
//!
//! # Uplink (device → cluster)
//!
//! [`Bridge::uplink`] decides per publication from the device:
//!
//! * `app/**` → [`Uplink::Publish`] on the mapped key, payload unchanged
//!   (camera chunks of `middleware::stream` pass byte for byte).
//! * `disc/<kind>` → a beacon (`discovery::beacon`), bound to the device
//!   with the firmware's E21 rules: the gateway first learns the device's
//!   node id from its `sys/info` reply ([`Bridge::bind`]); until then
//!   every beacon is refused. A beacon declaring another node is refused;
//!   one without a node takes the device's. The device holds at most
//!   [`MAX_DEVICE_SERVICES`] records; a new one beyond that is refused,
//!   never evicting another. A new or changed record is
//!   [`Uplink::Announce`] (to `ZenohDiscovery`, node = the device), a
//!   goodbye [`Uplink::Withdraw`], a refresh [`Uplink::Nothing`]. Records
//!   whose TTL runs out are withdrawn by [`Bridge::expire`].
//! * `sys/log` → [`Uplink::Log`]: kept in the [`LogTail`] for `gw/sys`
//!   reads, never published (system keys do not leave the gateway).
//! * anything else → [`Uplink::Refuse`] (`sys/payload/status`, ...).
//!
//! # Downlink (cluster → device)
//!
//! The gateway subscribes on Zenoh to at most [`MAX_DOWNLINK_FILTERS`]
//! filters under `app` (cluster key space, `MW_GATEWAY_DOWNLINK`); the
//! device has no subscription frame on the serial wire, so what it takes
//! is declared in the gateway's configuration. [`Bridge::downlink`] maps a
//! matching sample's key to the device's key space; anything else is
//! dropped.
//!
//! # `gw/sys`
//!
//! The cluster reaches the device's system keys only through the served
//! kind `gw/sys`, whose request body is `[key_len u8][key][body]`
//! ([`encode_sys_request`], [`parse_sys_request`]). Only keys of the
//! [`SysAllow`] list pass ([`SysAllow::route`]); by default `sys/info`
//! (forwarded to the device) and `sys/log` (answered from the gateway's
//! [`LogTail`]: the device publishes its log, it does not serve it).
//! Everything else, `sys/loader/*` and `sys/trust/*` included, is
//! [`HandlerError::Denied`].
//!
//! # Bounds
//!
//! Everything here is bounded: [`MAX_DEVICE_SERVICES`] records,
//! [`MAX_PENDING`] outstanding device requests ([`Pending`]),
//! [`LOG_TAIL_BYTES`] of log. Queues between the shell's tasks are bounded
//! too and drop the newest item when full ([`crate::shell`]); every
//! refusal and drop is counted in [`Counters`].

use middleware::discovery::beacon::{decode, kind_of};
use middleware::discovery::{Change, Directory, Event, Service, ServiceKind, ServiceRecord};
use middleware::key::{AsKey, Key, KeyBuf, KeyFilterBuf, MAX_KEY_LEN};
use middleware::loader::{DeviceInfo, keys};
use middleware::transport::{HandlerError, SenderId};
use std::collections::VecDeque;

/// Records one device may have announced through the gateway at once.
pub const MAX_DEVICE_SERVICES: usize = 4;
/// Most downlink filters.
pub const MAX_DOWNLINK_FILTERS: usize = 8;
/// Most `gw/sys` allowlist entries.
pub const MAX_SYS_ALLOW: usize = 8;
/// Device requests outstanding at once.
pub const MAX_PENDING: usize = 4;
/// Bytes of device log the gateway keeps.
pub const LOG_TAIL_BYTES: usize = 2048;

/// The key family that crosses the gateway.
pub const APP: Key<'static> = Key::from_static("app");

/// An owned key.
pub type OwnedKey = KeyBuf<MAX_KEY_LEN>;
/// An owned filter.
pub type OwnedFilter = KeyFilterBuf<MAX_KEY_LEN>;

/// Translates keys between the device and the cluster (module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyMap {
    namespace: Option<String>,
    /// `app` or `app/<ns>`.
    prefix: String,
}

impl Default for KeyMap {
    fn default() -> Self {
        Self::identity()
    }
}

/// The rest of `key` after the segment-wise prefix `prefix`: `Some("")`
/// for `key == prefix`, `Some("x/y")` for `prefix/x/y`, `None` otherwise.
fn below<'k>(key: &'k str, prefix: &str) -> Option<&'k str> {
    let rest = key.strip_prefix(prefix)?;
    match rest.strip_prefix('/') {
        Some(r) => Some(r),
        None => rest.is_empty().then_some(""),
    }
}

fn joined(head: &str, rest: &str) -> Option<OwnedKey> {
    let text = if rest.is_empty() {
        head.to_owned()
    } else {
        format!("{head}/{rest}")
    };
    OwnedKey::new(&text).ok()
}

impl KeyMap {
    /// Identical key spaces.
    pub fn identity() -> Self {
        KeyMap {
            namespace: None,
            prefix: APP.as_str().to_owned(),
        }
    }

    /// The device's `app/<rest>` is the cluster's `app/<ns>/<rest>`;
    /// `None` unless `ns` is one key segment.
    pub fn namespaced(ns: &str) -> Option<Self> {
        let ok = Key::new(ns).is_ok() && !ns.contains('/');
        ok.then(|| KeyMap {
            namespace: Some(ns.to_owned()),
            prefix: format!("{}/{ns}", APP.as_str()),
        })
    }

    /// The namespace segment, if any.
    pub fn namespace(&self) -> Option<&str> {
        self.namespace.as_deref()
    }

    /// The cluster's prefix of the device's keys: `app` or `app/<ns>`.
    pub fn cluster_prefix(&self) -> &str {
        &self.prefix
    }

    /// The cluster key of the device key `key`; `None` if it is not `app`
    /// or below, or the result would be too long.
    pub fn to_cluster(&self, key: Key<'_>) -> Option<OwnedKey> {
        let rest = below(key.as_str(), APP.as_str())?;
        joined(&self.prefix, rest)
    }

    /// The device key of the cluster key `key`; `None` if it is not below
    /// [`KeyMap::cluster_prefix`].
    pub fn to_device(&self, key: Key<'_>) -> Option<OwnedKey> {
        let rest = below(key.as_str(), &self.prefix)?;
        joined(APP.as_str(), rest)
    }
}

/// Where an allowed `gw/sys` request is answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SysRoute {
    /// Forwarded to the device.
    Device,
    /// Answered from the gateway's [`LogTail`] (`sys/log`).
    LogTail,
}

/// The `sys/*` keys `gw/sys` passes (module docs). Exact keys only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SysAllow(Vec<OwnedKey>);

impl Default for SysAllow {
    /// `sys/info` and `sys/log`.
    fn default() -> Self {
        SysAllow(
            [keys::INFO, keys::LOG]
                .iter()
                .filter_map(|k| KeyBuf::from_key(*k).ok())
                .collect(),
        )
    }
}

impl SysAllow {
    /// Allows exactly `list`; `None` if an entry is not a `sys/...` key,
    /// or there are more than [`MAX_SYS_ALLOW`].
    pub fn new<'k>(list: impl IntoIterator<Item = Key<'k>>) -> Option<Self> {
        let mut out = Vec::new();
        for key in list {
            let sys = key.segments().next() == Some("sys") && key.as_str() != "sys";
            if !sys || out.len() >= MAX_SYS_ALLOW {
                return None;
            }
            out.push(KeyBuf::from_key(key).ok()?);
        }
        Some(SysAllow(out))
    }

    /// The allowed keys.
    pub fn keys(&self) -> impl Iterator<Item = Key<'_>> {
        self.0.iter().map(|k| k.as_key())
    }

    /// Where a request for `key` goes, or [`HandlerError::Denied`].
    pub fn route(&self, key: Key<'_>) -> Result<SysRoute, HandlerError> {
        if !self.0.iter().any(|k| k.as_str() == key.as_str()) {
            return Err(HandlerError::Denied);
        }
        Ok(if key == keys::LOG {
            SysRoute::LogTail
        } else {
            SysRoute::Device
        })
    }
}

/// Writes a `gw/sys` request body for `key` and `body` into `out`.
pub fn encode_sys_request(key: Key<'_>, body: &[u8], out: &mut [u8]) -> Option<usize> {
    let k = key.as_bytes();
    let len = u8::try_from(k.len()).ok()?;
    let total = 1 + k.len() + body.len();
    let dst = out.get_mut(..total)?;
    dst[0] = len;
    dst[1..1 + k.len()].copy_from_slice(k);
    dst[1 + k.len()..].copy_from_slice(body);
    Some(total)
}

/// Splits a `gw/sys` request body into its key and the device request's
/// body; [`HandlerError::InvalidRequest`] if malformed.
pub fn parse_sys_request(body: &[u8]) -> Result<(Key<'_>, &[u8]), HandlerError> {
    let bad = HandlerError::InvalidRequest;
    let (&len, rest) = body.split_first().ok_or(bad)?;
    let key = rest.get(..usize::from(len)).ok_or(bad)?;
    let key = core::str::from_utf8(key).map_err(|_| bad)?;
    let key = Key::new(key).map_err(|_| bad)?;
    Ok((key, rest.get(usize::from(len)..).unwrap_or(&[])))
}

/// The newest [`LOG_TAIL_BYTES`] bytes of device log (`sys/log`
/// publications, concatenated). A tail: the oldest bytes go first.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LogTail(VecDeque<u8>);

impl LogTail {
    /// The tail with `line` appended.
    pub fn push(self, line: &[u8]) -> Self {
        let mut bytes = self.0;
        let line = line
            .get(line.len().saturating_sub(LOG_TAIL_BYTES)..)
            .unwrap_or(line);
        let excess = (bytes.len() + line.len()).saturating_sub(LOG_TAIL_BYTES);
        bytes.drain(..excess.min(bytes.len()));
        bytes.extend(line);
        LogTail(bytes)
    }

    /// Copies the newest bytes that fit into `out`; returns how many.
    pub fn read_into(&self, out: &mut [u8]) -> usize {
        let n = self.0.len().min(out.len());
        let skip = self.0.len() - n;
        for (dst, src) in out.iter_mut().zip(self.0.iter().skip(skip)) {
            *dst = *src;
        }
        n
    }

    /// Bytes held.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// True when empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Outstanding device requests, by correlation id: at most
/// [`MAX_PENDING`], each with a deadline. `T` is what the shell must
/// complete when the response (or the deadline) comes.
#[derive(Debug)]
pub struct Pending<T> {
    slots: Vec<(u8, u64, T)>,
    next_id: u8,
}

impl<T> Default for Pending<T> {
    fn default() -> Self {
        Pending {
            slots: Vec::new(),
            next_id: 0,
        }
    }
}

impl<T> Pending<T> {
    /// Nothing outstanding.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records `waiter` until `deadline_ms`, under a fresh id; gives the
    /// waiter back when [`MAX_PENDING`] are outstanding.
    pub fn insert(self, waiter: T, deadline_ms: u64) -> Result<(Self, u8), (Self, T)> {
        if self.slots.len() >= MAX_PENDING {
            return Err((self, waiter));
        }
        let mut id = self.next_id;
        // At most MAX_PENDING ids are taken, so this ends within
        // MAX_PENDING + 1 steps.
        for _ in 0..=MAX_PENDING {
            if !self.slots.iter().any(|(i, _, _)| *i == id) {
                break;
            }
            id = id.wrapping_add(1);
        }
        let mut slots = self.slots;
        slots.push((id, deadline_ms, waiter));
        Ok((
            Pending {
                slots,
                next_id: id.wrapping_add(1),
            },
            id,
        ))
    }

    /// Removes and returns the waiter of `id`.
    pub fn take(self, id: u8) -> (Self, Option<T>) {
        let mut slots = self.slots;
        let found = slots
            .iter()
            .position(|(i, _, _)| *i == id)
            .map(|p| slots.remove(p).2);
        (Pending { slots, ..self }, found)
    }

    /// Removes and returns the waiters whose deadline is `now_ms` or
    /// earlier.
    pub fn expire(self, now_ms: u64) -> (Self, Vec<T>) {
        let (late, slots): (Vec<_>, Vec<_>) =
            self.slots.into_iter().partition(|(_, d, _)| *d <= now_ms);
        (
            Pending {
                slots,
                next_id: self.next_id,
            },
            late.into_iter().map(|(_, _, w)| w).collect(),
        )
    }

    /// Outstanding requests.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// True when none.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Whether a waiter matches `pred`.
    pub fn any(&self, pred: impl Fn(&T) -> bool) -> bool {
        self.slots.iter().any(|(_, _, w)| pred(w))
    }
}

/// Why a device publication does not reach the cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Not `app/**`, `disc/<kind>` or `sys/log`.
    NotBridged,
    /// The key does not map (too long with the namespace).
    Unmappable,
    /// A beacon before the device's node id is known.
    Unbound,
    /// Not a valid beacon (or a field over the directory's bounds).
    BadBeacon,
    /// A beacon declaring a node other than the device (E21).
    NodeMismatch,
    /// A new record beyond [`MAX_DEVICE_SERVICES`].
    Cap,
}

/// What to do with one device publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Uplink {
    /// Publish the payload, unchanged, on this cluster key.
    Publish(OwnedKey),
    /// Announce (or re-announce, changed) this service on Zenoh.
    Announce(Box<ServiceRecord>),
    /// Withdraw this service from Zenoh.
    Withdraw {
        /// Its kind.
        kind: OwnedKey,
        /// Its instance.
        instance: String,
    },
    /// Kept in the log tail.
    Log,
    /// Nothing to do (a refresh, a goodbye for an unknown record).
    Nothing,
    /// Refused.
    Refuse(Refusal),
}

/// What binding to a `sys/info` reply did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bound {
    /// Bound to the node; services of a previously bound node (a device
    /// swapped on the port) are withdrawn.
    Node {
        /// The device's node id.
        node: SenderId,
        /// What to withdraw.
        withdraw: Vec<(OwnedKey, String)>,
    },
    /// The reply does not decode, or names no node (a pre-0.5 device).
    NoNode,
    /// The node differs from the pinned MAC (`MW_GATEWAY_DEVICE_MAC`).
    Mismatch(SenderId),
}

/// Counters of every decision and drop (saturating).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counters {
    /// `app/**` publications relayed to the cluster.
    pub published: u32,
    /// Announcements sent to Zenoh discovery.
    pub announced: u32,
    /// Withdrawals (goodbyes, expiry, rebinding).
    pub withdrawn: u32,
    /// Beacons refused (unbound, malformed, wrong node, over the cap).
    pub beacons_refused: u32,
    /// Publications on keys that are not bridged.
    pub not_bridged: u32,
    /// `sys/log` publications kept.
    pub logs: u32,
    /// Samples relayed to the device.
    pub downlinked: u32,
    /// `gw/sys` requests denied.
    pub sys_denied: u32,
}

fn inc(n: u32) -> u32 {
    n.saturating_add(1)
}

/// The gateway's state for one device link (module docs).
#[derive(Debug, Clone)]
pub struct Bridge {
    map: KeyMap,
    downlink: Vec<OwnedFilter>,
    allow: SysAllow,
    pinned: Option<SenderId>,
    device: Option<SenderId>,
    services: Directory<MAX_DEVICE_SERVICES>,
    tail: LogTail,
    counters: Counters,
}

impl Bridge {
    /// A bridge mapping keys with `map`, relaying cluster samples matching
    /// `downlink` (at most [`MAX_DOWNLINK_FILTERS`] are kept), passing
    /// `gw/sys` requests for `allow`, and, with `pinned`, binding only to
    /// that node.
    pub fn new(
        map: KeyMap,
        downlink: Vec<OwnedFilter>,
        allow: SysAllow,
        pinned: Option<SenderId>,
    ) -> Self {
        let mut downlink = downlink;
        downlink.truncate(MAX_DOWNLINK_FILTERS);
        Bridge {
            map,
            downlink,
            allow,
            pinned,
            device: None,
            services: Directory::new(),
            tail: LogTail::default(),
            counters: Counters::default(),
        }
    }

    /// The key map.
    pub fn key_map(&self) -> &KeyMap {
        &self.map
    }

    /// The downlink filters (cluster key space).
    pub fn downlink_filters(&self) -> &[OwnedFilter] {
        &self.downlink
    }

    /// The `gw/sys` allowlist.
    pub fn sys_allow(&self) -> &SysAllow {
        &self.allow
    }

    /// The device's node, once bound.
    pub fn device(&self) -> Option<SenderId> {
        self.device
    }

    /// The services announced for the device.
    pub fn services(&self) -> impl Iterator<Item = Service<'_>> {
        self.services.iter()
    }

    /// The device log kept.
    pub fn log_tail(&self) -> &LogTail {
        &self.tail
    }

    /// The counters.
    pub fn counters(&self) -> Counters {
        self.counters
    }

    fn count(self, f: impl FnOnce(&mut Counters)) -> Self {
        let mut counters = self.counters;
        f(&mut counters);
        Bridge { counters, ..self }
    }

    /// Binds to the node in a `sys/info` reply (module docs).
    pub fn bind(self, info: &[u8]) -> (Self, Bound) {
        let Some(node) = DeviceInfo::decode(info).and_then(|i| i.node) else {
            return (self, Bound::NoNode);
        };
        if self.pinned.is_some_and(|p| p != node) {
            return (self, Bound::Mismatch(node));
        }
        let withdraw: Vec<_> = if self.device.is_some_and(|d| d != node) {
            self.services
                .iter()
                .filter_map(|s| Some((KeyBuf::from_key(s.kind.key()).ok()?, s.instance.to_owned())))
                .collect()
        } else {
            Vec::new()
        };
        let services = if withdraw.is_empty() {
            self.services
        } else {
            Directory::new()
        };
        let n = u32::try_from(withdraw.len()).unwrap_or(u32::MAX);
        let mut counters = self.counters;
        counters.withdrawn = counters.withdrawn.saturating_add(n);
        (
            Bridge {
                device: Some(node),
                services,
                counters,
                ..self
            },
            Bound::Node { node, withdraw },
        )
    }

    /// Decides what to do with the device's publication on `key` at
    /// `now_ms` (module docs).
    pub fn uplink(self, key: Key<'_>, payload: &[u8], now_ms: u64) -> (Self, Uplink) {
        if key == keys::LOG {
            let tail = self.tail.push(payload);
            let b = Bridge { tail, ..self }.count(|c| c.logs = inc(c.logs));
            return (b, Uplink::Log);
        }
        if let Some(kind) = kind_of(key) {
            return self.beacon(kind, payload, now_ms);
        }
        if below(key.as_str(), APP.as_str()).is_some() {
            return match self.map.to_cluster(key) {
                Some(k) => (
                    self.count(|c| c.published = inc(c.published)),
                    Uplink::Publish(k),
                ),
                None => (
                    self.count(|c| c.not_bridged = inc(c.not_bridged)),
                    Uplink::Refuse(Refusal::Unmappable),
                ),
            };
        }
        (
            self.count(|c| c.not_bridged = inc(c.not_bridged)),
            Uplink::Refuse(Refusal::NotBridged),
        )
    }

    fn refuse_beacon(self, why: Refusal) -> (Self, Uplink) {
        (
            self.count(|c| c.beacons_refused = inc(c.beacons_refused)),
            Uplink::Refuse(why),
        )
    }

    fn beacon(self, kind: ServiceKind<'_>, payload: &[u8], now_ms: u64) -> (Self, Uplink) {
        let Some(device) = self.device else {
            return self.refuse_beacon(Refusal::Unbound);
        };
        let Ok(service) = decode(kind, payload) else {
            return self.refuse_beacon(Refusal::BadBeacon);
        };
        if service.node.is_some_and(|n| n != device) {
            return self.refuse_beacon(Refusal::NodeMismatch);
        }
        let service = Service {
            node: Some(device),
            ..service
        };
        let known = self.services.get(service.kind, service.instance).is_some();
        if !known && service.ttl_s > 0 && self.services.len() >= MAX_DEVICE_SERVICES {
            return self.refuse_beacon(Refusal::Cap);
        }
        let Some(record) = ServiceRecord::new(&service) else {
            return self.refuse_beacon(Refusal::BadBeacon);
        };
        let (services, change) = self.services.observe(&Event::Up(service), now_ms);
        let b = Bridge { services, ..self };
        match change {
            Change::Added | Change::Updated => (
                b.count(|c| c.announced = inc(c.announced)),
                Uplink::Announce(Box::new(record)),
            ),
            Change::Removed => match KeyBuf::from_key(service.kind.key()) {
                Ok(kind) => (
                    b.count(|c| c.withdrawn = inc(c.withdrawn)),
                    Uplink::Withdraw {
                        kind,
                        instance: service.instance.to_owned(),
                    },
                ),
                Err(_) => b.refuse_beacon(Refusal::BadBeacon),
            },
            Change::Refreshed | Change::Ignored => (b, Uplink::Nothing),
            Change::Full => b.refuse_beacon(Refusal::Cap),
            Change::Rejected => b.refuse_beacon(Refusal::BadBeacon),
        }
    }

    /// Drops the records whose TTL has run out at `now_ms`; returns them
    /// (kind, instance) for withdrawal.
    pub fn expire(self, now_ms: u64) -> (Self, Vec<(OwnedKey, String)>) {
        let gone: Vec<_> = self
            .services
            .expired(now_ms)
            .filter_map(|s| Some((KeyBuf::from_key(s.kind.key()).ok()?, s.instance.to_owned())))
            .collect();
        let (services, _) = self.services.expire(now_ms);
        let n = u32::try_from(gone.len()).unwrap_or(u32::MAX);
        let b = Bridge { services, ..self }.count(|c| c.withdrawn = c.withdrawn.saturating_add(n));
        (b, gone)
    }

    /// Milliseconds from `now_ms` until the next record expires.
    pub fn next_expiry(&self, now_ms: u64) -> Option<u64> {
        self.services.next_expiry(now_ms)
    }

    /// The device key for a cluster sample on `key`, if a downlink filter
    /// takes it. Pure (counting is [`Bridge::downlinked`]).
    pub fn downlink(&self, key: Key<'_>) -> Option<OwnedKey> {
        self.downlink
            .iter()
            .any(|f| f.as_filter().matches(key))
            .then(|| self.map.to_device(key))
            .flatten()
    }

    /// Counts one sample relayed to the device.
    pub fn downlinked(self) -> Self {
        self.count(|c| c.downlinked = inc(c.downlinked))
    }

    /// Where a `gw/sys` request for `key` goes; a denial is counted.
    pub fn sys_route(self, key: Key<'_>) -> (Self, Result<SysRoute, HandlerError>) {
        match self.allow.route(key) {
            Ok(r) => (self, Ok(r)),
            Err(e) => (self.count(|c| c.sys_denied = inc(c.sys_denied)), Err(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use middleware::discovery::beacon::encode;
    use middleware::discovery::{Endpoint, beacon::beacon_key};
    use middleware::loader::PayloadStatus;

    const MAC: [u8; 6] = [0x24, 0x6F, 0x28, 0xAA, 0xBB, 0xCC];
    const DEVICE: SenderId = SenderId::from_mac(MAC);
    const OTHER: SenderId = SenderId::from_mac([2, 0, 0, 0, 0, 9]);
    const CAM: ServiceKind<'static> = ServiceKind::from_static("cam/jpeg");

    fn k(s: &str) -> Key<'_> {
        Key::new(s).unwrap()
    }

    fn info(node: Option<SenderId>) -> Vec<u8> {
        let mut out = [0u8; 128];
        let n = DeviceInfo {
            abi_version: 4,
            capacity: 1,
            status: PayloadStatus::Empty,
            chip: "esp32",
            firmware: "0.6",
            node,
        }
        .encode(&mut out)
        .unwrap();
        out[..n].to_vec()
    }

    fn beacon(instance: &str, node: Option<SenderId>, ttl_s: u32) -> Vec<u8> {
        let mut out = [0u8; 256];
        let n = encode(
            &Service {
                kind: CAM,
                instance,
                node,
                endpoint: Endpoint::Local,
                txt: b"\x10key=app/cam/jpeg",
                ttl_s,
            },
            &mut out,
        )
        .unwrap();
        out[..n].to_vec()
    }

    fn bridge() -> Bridge {
        Bridge::new(KeyMap::identity(), Vec::new(), SysAllow::default(), None)
    }

    fn bound() -> Bridge {
        let (b, r) = bridge().bind(&info(Some(DEVICE)));
        assert!(matches!(r, Bound::Node { node: DEVICE, .. }));
        b
    }

    fn disc() -> OwnedKey {
        let mut buf = [0u8; 64];
        KeyBuf::from_key(beacon_key(CAM, &mut buf).unwrap()).unwrap()
    }

    #[test]
    fn v6_key_map_identity_and_namespace() {
        let id = KeyMap::identity();
        assert_eq!(
            id.to_cluster(k("app/cam/jpeg")).unwrap().as_str(),
            "app/cam/jpeg"
        );
        assert_eq!(
            id.to_device(k("app/cam/jpeg")).unwrap().as_str(),
            "app/cam/jpeg"
        );
        assert!(id.to_cluster(k("sys/info")).is_none());
        assert!(id.to_cluster(k("apple/x")).is_none());
        let ns = KeyMap::namespaced("bench").unwrap();
        assert_eq!(ns.namespace(), Some("bench"));
        assert_eq!(ns.cluster_prefix(), "app/bench");
        assert_eq!(
            ns.to_cluster(k("app/cam/jpeg")).unwrap().as_str(),
            "app/bench/cam/jpeg"
        );
        assert_eq!(ns.to_cluster(k("app")).unwrap().as_str(), "app/bench");
        assert_eq!(
            ns.to_device(k("app/bench/cam/jpeg")).unwrap().as_str(),
            "app/cam/jpeg"
        );
        assert_eq!(ns.to_device(k("app/bench")).unwrap().as_str(), "app");
        assert!(ns.to_device(k("app/other/cam/jpeg")).is_none());
        assert!(ns.to_device(k("app/benchmark")).is_none());
        assert!(KeyMap::namespaced("a/b").is_none());
        assert!(KeyMap::namespaced("").is_none());
        // Too long once prefixed.
        let long = format!("app/{}", "x".repeat(MAX_KEY_LEN - 4));
        assert!(ns.to_cluster(k(&long)).is_none());
    }

    #[test]
    fn v7_app_publications_map_and_others_stay() {
        let (b, u) = bridge().uplink(k("app/cam/jpeg"), b"chunk", 0);
        assert_eq!(u, Uplink::Publish(OwnedKey::new("app/cam/jpeg").unwrap()));
        let (b, u) = b.uplink(k("sys/payload/status"), b"\x01", 0);
        assert_eq!(u, Uplink::Refuse(Refusal::NotBridged));
        let (b, u) = b.uplink(k("sys/log"), b"hello\n", 0);
        assert_eq!(u, Uplink::Log);
        let mut out = [0u8; 16];
        let n = b.log_tail().read_into(&mut out);
        assert_eq!(&out[..n], b"hello\n");
        let c = b.counters();
        assert_eq!((c.published, c.not_bridged, c.logs), (1, 1, 1));
    }

    #[test]
    fn v8_beacons_announce_with_the_device_node() {
        let b = bound();
        assert_eq!(b.device(), Some(DEVICE));
        // No node in the beacon: the device's.
        let (b, u) = b.uplink(disc().as_key(), &beacon("cam-aabbcc", None, 30), 0);
        let Uplink::Announce(r) = u else {
            panic!("{u:?}")
        };
        assert_eq!(r.service().node, Some(DEVICE));
        assert_eq!(r.service().instance, "cam-aabbcc");
        // The device naming itself: a refresh is nothing new.
        let (b, u) = b.uplink(
            disc().as_key(),
            &beacon("cam-aabbcc", Some(DEVICE), 30),
            1000,
        );
        assert_eq!(u, Uplink::Nothing);
        // Changed TTL: re-announced.
        let (b, u) = b.uplink(
            disc().as_key(),
            &beacon("cam-aabbcc", Some(DEVICE), 60),
            2000,
        );
        assert!(matches!(u, Uplink::Announce(_)));
        // Goodbye.
        let (b, u) = b.uplink(disc().as_key(), &beacon("cam-aabbcc", None, 0), 3000);
        assert_eq!(
            u,
            Uplink::Withdraw {
                kind: OwnedKey::new("cam/jpeg").unwrap(),
                instance: "cam-aabbcc".into()
            }
        );
        let (b, u) = b.uplink(disc().as_key(), &beacon("cam-aabbcc", None, 0), 3000);
        assert_eq!(u, Uplink::Nothing);
        let c = b.counters();
        assert_eq!((c.announced, c.withdrawn, c.beacons_refused), (2, 1, 0));
    }

    #[test]
    fn v9_beacons_are_bound_to_the_device() {
        // Before sys/info: refused.
        let (b, u) = bridge().uplink(disc().as_key(), &beacon("cam-1", None, 30), 0);
        assert_eq!(u, Uplink::Refuse(Refusal::Unbound));
        // A device without a node id never binds.
        let (b, r) = b.bind(&info(None));
        assert_eq!(r, Bound::NoNode);
        let (b, r) = b.bind(b"junk");
        assert_eq!(r, Bound::NoNode);
        let (b, _) = b.bind(&info(Some(DEVICE)));
        // Spoofed node: refused.
        let (b, u) = b.uplink(disc().as_key(), &beacon("cam-1", Some(OTHER), 30), 0);
        assert_eq!(u, Uplink::Refuse(Refusal::NodeMismatch));
        let (b, u) = b.uplink(disc().as_key(), b"\x07garbage", 0);
        assert_eq!(u, Uplink::Refuse(Refusal::BadBeacon));
        assert_eq!(b.counters().beacons_refused, 3);
        assert_eq!(b.services().count(), 0);
        // A pinned MAC refuses another device.
        let pinned = Bridge::new(
            KeyMap::identity(),
            Vec::new(),
            SysAllow::default(),
            Some(OTHER),
        );
        assert_eq!(pinned.bind(&info(Some(DEVICE))).1, Bound::Mismatch(DEVICE));
    }

    #[test]
    fn v9_device_cap_and_rebinding() {
        let mut b = bound();
        for i in 0..MAX_DEVICE_SERVICES {
            let (n, u) = b.uplink(disc().as_key(), &beacon(&format!("cam-{i}"), None, 30), 0);
            assert!(matches!(u, Uplink::Announce(_)));
            b = n;
        }
        let (b, u) = b.uplink(disc().as_key(), &beacon("cam-x", None, 30), 0);
        assert_eq!(u, Uplink::Refuse(Refusal::Cap));
        // Known records still refresh and withdraw.
        let (b, u) = b.uplink(disc().as_key(), &beacon("cam-0", None, 30), 10);
        assert_eq!(u, Uplink::Nothing);
        // Another device on the port: the old one's services go.
        let (b, r) = b.bind(&info(Some(OTHER)));
        let Bound::Node { node, withdraw } = r else {
            panic!()
        };
        assert_eq!(node, OTHER);
        assert_eq!(withdraw.len(), MAX_DEVICE_SERVICES);
        assert_eq!(b.services().count(), 0);
        // Rebinding to the same node keeps them.
        let (b, _) = b.uplink(disc().as_key(), &beacon("cam-0", None, 30), 0);
        let (b, r) = b.bind(&info(Some(OTHER)));
        assert!(matches!(r, Bound::Node { ref withdraw, .. } if withdraw.is_empty()));
        assert_eq!(b.services().count(), 1);
    }

    #[test]
    fn v10_expiry_withdraws() {
        let (b, _) = bound().uplink(disc().as_key(), &beacon("cam-1", None, 30), 1000);
        assert_eq!(b.next_expiry(1000), Some(31_000));
        let (b, gone) = b.clone().expire(30_999);
        assert!(gone.is_empty());
        let (b, gone) = b.expire(31_000);
        assert_eq!(
            gone,
            [(OwnedKey::new("cam/jpeg").unwrap(), "cam-1".to_owned())]
        );
        assert_eq!(b.services().count(), 0);
        assert_eq!(b.counters().withdrawn, 1);
    }

    #[test]
    fn v11_downlink_only_for_declared_filters() {
        let f = |s: &str| mw_service::spec::parse_filter(s).unwrap().to_buf().unwrap();
        let b = Bridge::new(
            KeyMap::namespaced("bench").unwrap(),
            vec![f("app/bench/robot/cmd/**")],
            SysAllow::default(),
            None,
        );
        assert_eq!(
            b.downlink(k("app/bench/robot/cmd/speed")).unwrap().as_str(),
            "app/robot/cmd/speed"
        );
        assert!(b.downlink(k("app/bench/cam/jpeg")).is_none());
        assert!(b.downlink(k("sys/info")).is_none());
        assert_eq!(b.downlinked().counters().downlinked, 1);
    }

    #[test]
    fn v12_sys_allowlist() {
        let b = bridge();
        let (b, r) = b.sys_route(keys::INFO);
        assert_eq!(r, Ok(SysRoute::Device));
        let (b, r) = b.sys_route(keys::LOG);
        assert_eq!(r, Ok(SysRoute::LogTail));
        let (b, r) = b.sys_route(keys::BEGIN);
        assert_eq!(r, Err(HandlerError::Denied));
        let (b, r) = b.sys_route(keys::TRUST_CLAIM);
        assert_eq!(r, Err(HandlerError::Denied));
        assert_eq!(b.counters().sys_denied, 2);
        assert!(SysAllow::new([k("app/x")]).is_none());
        assert!(SysAllow::new([k("sys")]).is_none());
        let only = SysAllow::new([keys::INFO]).unwrap();
        assert_eq!(only.route(keys::LOG), Err(HandlerError::Denied));
        assert_eq!(only.keys().count(), 1);
        let many: Vec<String> = (0..=MAX_SYS_ALLOW).map(|i| format!("sys/k{i}")).collect();
        assert!(SysAllow::new(many.iter().map(|s| k(s))).is_none());
    }

    #[test]
    fn v12_sys_request_body_round_trips() {
        let mut out = [0u8; 32];
        let n = encode_sys_request(keys::INFO, b"", &mut out).unwrap();
        assert_eq!(parse_sys_request(&out[..n]), Ok((keys::INFO, &b""[..])));
        let n = encode_sys_request(keys::BEGIN, b"\x01\x02", &mut out).unwrap();
        assert_eq!(
            parse_sys_request(&out[..n]),
            Ok((keys::BEGIN, &b"\x01\x02"[..]))
        );
        for bad in [&b""[..], b"\x09sys/info", b"\x02\xff\xfe", b"\x00"] {
            assert_eq!(
                parse_sys_request(bad),
                Err(HandlerError::InvalidRequest),
                "{bad:?}"
            );
        }
        assert!(encode_sys_request(keys::INFO, b"", &mut [0u8; 4]).is_none());
    }

    #[test]
    fn v13_log_tail_keeps_the_newest_bytes() {
        let mut t = LogTail::default();
        assert!(t.is_empty());
        for _ in 0..LOG_TAIL_BYTES {
            t = t.push(b"ab");
        }
        assert_eq!(t.len(), LOG_TAIL_BYTES);
        t = t.push(b"XYZ");
        let mut out = [0u8; 5];
        assert_eq!(t.read_into(&mut out), 5);
        assert_eq!(&out, b"abXYZ");
        let huge = vec![b'q'; LOG_TAIL_BYTES * 2];
        assert_eq!(LogTail::default().push(&huge).len(), LOG_TAIL_BYTES);
    }

    #[test]
    fn v13_pending_is_bounded_and_expires() {
        let mut p = Pending::new();
        let mut ids = Vec::new();
        for i in 0..MAX_PENDING {
            let (n, id) = p.insert(i, 100 + i as u64).map_err(|_| ()).unwrap();
            ids.push(id);
            p = n;
        }
        let Err((p, back)) = p.insert(99, 0) else {
            panic!("full")
        };
        assert_eq!(back, 99);
        assert!(p.any(|w| *w == 2));
        let (p, w) = p.take(ids[1]);
        assert_eq!(w, Some(1));
        let (p, w) = p.take(ids[1]);
        assert_eq!(w, None);
        let (p, late) = p.expire(102);
        assert_eq!(late, [0, 2]);
        assert_eq!(p.len(), 1);
        // Ids skip those still outstanding.
        let (p, id) = p.insert(7, 1).map_err(|_| ()).unwrap();
        assert!(!ids[3..].contains(&id) || ids[3] != id);
        assert_eq!(p.len(), 2);
        assert!(!p.is_empty());
    }
}
