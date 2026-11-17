//! Discovery beacons on the serial link: how a Pico behind the MCU
//! gateway (`gateway/`, RG8/RG9) announces itself and the keys its payload
//! serves, as `disc/<kind>` records with its node id.
//!
//! # What is announced
//!
//! At most [`MAX_RECORDS`] (4) services, the gateway's per-device cap
//! (`gateway::bridge::MAX_DEVICE_SERVICES`, which refuses a new record
//! beyond it and never evicts), all with instance `rp-<node id in hex>`
//! ([`Identity::instance`]), node the chip unique id (V1, the node
//! `sys/info` reports), endpoint [`Endpoint::Local`] (reach it on this
//! link) and TTL [`BEACON_TTL_S`]:
//!
//! 1. the device itself, kind [`NODE_KIND`] (`mw/node`), TXT
//!    `chip=<rp2040|rp2350>` and `fw=<firmware version>`, always;
//! 2. while a payload runs, each key it serves (ABI 5), in its slot order,
//!    as a service whose kind is the key (a request to the kind reaches
//!    the payload, V20) and no TXT; a key that cannot be a kind (longer
//!    than `MAX_KIND_LEN`, or under `disc/`), a repeat, and keys beyond the
//!    three that fit beside the node record are left out and counted
//!    ([`Wanted::omitted`]).
//!
//! [`wanted`] computes that set from the [`Identity`] and the served keys
//! (none while no payload runs). [`plan`] compares it with what is
//! announced and returns the [`Plan`] that brings one to the other,
//! performed withdrawals first (goodbyes free the gateway's slots), then
//! the new or changed announcements; an unchanged record is in neither. So when a
//! payload stops, traps or an upload ends its run, the next plan withdraws
//! its served keys with one goodbye each, and the node record stays.
//!
//! # E21 on the serial link
//!
//! The gateway binds beacons to the device whose node id `sys/info`
//! reported and refuses one naming another node; every beacon here names
//! the device's own node (goodbyes name none, which the gateway reads as
//! the device). A payload cannot add records: [`payload_may_publish`]
//! refuses `disc/...` (and `sys/...`) for its publishes, as esp-base's
//! radio does (E18), because every beacon on the link would be announced
//! as the device's.
//!
//! The gateway refuses beacons until it has bound the device, which it
//! does on a `sys/info` reply. The device answers such a reply by sending
//! every record again ([`Kick`], at most once per [`KICK_INTERVAL_MS`]),
//! after the response on the same stream, so a gateway that has just
//! bound hears the records at once instead of at the next refresh.
//!
//! # Re-announcing
//!
//! [`Announcer`] is `middleware::discovery::beacon::PubSubDiscovery` over
//! any [`Publisher`] (the shell's: the bus to the CDC link task, which
//! writes publish frames with the link's `Publisher`): it keeps the
//! announced records, re-sends each every half TTL (15 s) when the shell
//! calls [`Announcer::tick`], and [`Announcer::due`] says when the next is
//! due. The device does not browse: its subscriber half is [`NoBrowse`].
//! Each beacon's payload is under 70 bytes (its frame adds the
//! `disc/<kind>` key, at most 69 bytes), so four records cost under
//! 600 bytes per 15 s on the link.

use heapless::{String, Vec};
use middleware::Key;
use middleware::discovery::beacon::{BEACON_PREFIX, PubSubDiscovery, PubSubError};
use middleware::discovery::directory::{MAX_INSTANCE_LEN, MAX_KIND_LEN, MAX_TXT_LEN};
use middleware::discovery::{Announce, Endpoint, Service, ServiceKind};
use middleware::loader::is_system_key;
use middleware::transport::{Delivery, Limits, Overflow, Publisher, Sample, SenderId, Subscriber};
use middleware::wasm_abi::serve::MAX_SERVES;

use crate::service::Facts;

/// The kind the device announces itself as.
pub const NODE_KIND: ServiceKind<'static> = ServiceKind::from_static("mw/node");

/// TTL of every announcement: re-sent every 15 s, so one lost beacon does
/// not expire it at the gateway (esp-base's camera beacon, E19, uses the
/// same).
pub const BEACON_TTL_S: u32 = 30;

/// Records announced at most: the gateway's per-device cap.
pub const MAX_RECORDS: usize = 4;

/// Shortest interval between two re-sends caused by `sys/info`.
pub const KICK_INTERVAL_MS: u64 = 5_000;

/// Length of [`Identity::instance`]: `rp-` and 16 hex digits.
pub const INSTANCE_LEN: usize = 3 + 16;

/// Bytes of the node record's TXT ([`Identity::txt`]).
const TXT_CAP: usize = 96;

/// Whether a payload may publish on `key`: neither the firmware's
/// `sys/...` keys nor `disc/...`, whose beacons the gateway would announce
/// as the device's own (module docs). The device answers
/// `AbiError::InvalidKey` otherwise.
pub fn payload_may_publish(key: Key<'_>) -> bool {
    !is_system_key(key) && key.segments().next() != Some(BEACON_PREFIX.as_str())
}

/// What the device announces about itself: its node, instance name and
/// the node record's TXT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    node: SenderId,
    instance: [u8; INSTANCE_LEN],
    txt: Vec<u8, TXT_CAP>,
}

impl Identity {
    /// The identity of the device `facts` describe: the node id, `chip=`
    /// and `fw=` (an entry over 255 bytes, or beyond the TXT's 96 bytes,
    /// is left out).
    pub fn new(facts: &Facts) -> Self {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut instance = [0u8; INSTANCE_LEN];
        instance[..3].copy_from_slice(b"rp-");
        for (i, b) in facts.node.0.iter().enumerate() {
            if let Some(pair) = instance.get_mut(3 + 2 * i..5 + 2 * i) {
                pair[0] = HEX[usize::from(b >> 4)];
                pair[1] = HEX[usize::from(b & 0xF)];
            }
        }
        let mut txt = Vec::new();
        push_entry(&mut txt, b"chip=", facts.chip.as_bytes());
        push_entry(&mut txt, b"fw=", facts.firmware.as_bytes());
        Identity {
            node: facts.node,
            instance,
            txt,
        }
    }

    /// The node id every record names.
    pub const fn node(&self) -> SenderId {
        self.node
    }

    /// `rp-` and the node id in lower-case hex.
    pub fn instance(&self) -> &str {
        // Always ASCII; the fallback is never taken.
        core::str::from_utf8(&self.instance).unwrap_or("rp")
    }

    /// The node record's TXT RDATA.
    pub fn txt(&self) -> &[u8] {
        &self.txt
    }

    /// The device's own record.
    pub fn node_record(&self) -> Service<'_> {
        Service {
            kind: NODE_KIND,
            instance: self.instance(),
            node: Some(self.node),
            endpoint: Endpoint::Local,
            txt: &self.txt,
            ttl_s: BEACON_TTL_S,
        }
    }

    /// The record of a served `key`, `None` if it cannot be a kind (not a
    /// key, a `sys` or `disc` key, or over `MAX_KIND_LEN` bytes).
    pub fn served_record<'a>(&'a self, key: &'a str) -> Option<Service<'a>> {
        let key = Key::new(key).ok()?;
        if key.as_bytes().len() > MAX_KIND_LEN || !payload_may_publish(key) {
            return None;
        }
        Some(Service {
            kind: ServiceKind::new(key),
            instance: self.instance(),
            node: Some(self.node),
            endpoint: Endpoint::Local,
            txt: &[],
            ttl_s: BEACON_TTL_S,
        })
    }
}

/// Appends the TXT string `key` `value` if it fits.
fn push_entry<const N: usize>(txt: &mut Vec<u8, N>, key: &[u8], value: &[u8]) {
    let len = key.len() + value.len();
    let Ok(len_byte) = u8::try_from(len) else {
        return;
    };
    if txt.len() + 1 + len > N.min(MAX_TXT_LEN) {
        return;
    }
    // Fits (checked above), so none of these fail.
    let _ = txt.push(len_byte);
    let _ = txt.extend_from_slice(key);
    let _ = txt.extend_from_slice(value);
}

/// The records the device should announce now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wanted<'a> {
    records: Vec<Service<'a>, MAX_RECORDS>,
    /// Served keys left out: not a kind, repeated, or beyond the cap.
    pub omitted: usize,
}

impl<'a> Wanted<'a> {
    /// The records, the node record first.
    pub fn records(&self) -> &[Service<'a>] {
        &self.records
    }
}

/// The node record, then a record per served key (module docs). At most
/// `MAX_SERVES` keys are looked at, the number a payload can serve.
pub fn wanted<'a, 's: 'a>(id: &'a Identity, served: impl Iterator<Item = &'s str>) -> Wanted<'a> {
    let mut records: Vec<Service<'a>, MAX_RECORDS> = Vec::new();
    // Room for MAX_RECORDS >= 1 is guaranteed.
    let _ = records.push(id.node_record());
    let mut omitted = 0;
    for key in served.take(MAX_SERVES) {
        let fresh = id
            .served_record(key)
            .filter(|r| !records.iter().any(|w| w.kind == r.kind));
        match fresh {
            Some(record) if records.push(record).is_ok() => {}
            _ => omitted += 1,
        }
    }
    Wanted { records, omitted }
}

/// What changes between the announced records and the wanted ones.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    /// Indices into the announced records: withdraw these (goodbye, no
    /// more refreshes).
    pub withdraw: Vec<usize, MAX_RECORDS>,
    /// Indices into [`Wanted::records`]: announce these now (new, or
    /// changed).
    pub announce: Vec<usize, MAX_RECORDS>,
}

/// What brings `announced` to `wanted`: a record is withdrawn when no
/// wanted record has its kind and instance, and announced when no
/// announced record equals it; an unchanged record is in neither. Only
/// the first [`MAX_RECORDS`] announced records are looked at, so the
/// plan always fits. The shell performs the withdrawals first (goodbyes
/// free the gateway's slots), then the announcements.
pub fn plan(wanted: &Wanted<'_>, announced: &[Service<'_>]) -> Plan {
    let mut out = Plan::default();
    for (i, old) in announced.iter().enumerate().take(MAX_RECORDS) {
        if !wanted
            .records
            .iter()
            .any(|w| w.kind == old.kind && w.instance == old.instance)
        {
            // At most MAX_RECORDS indices.
            let _ = out.withdraw.push(i);
        }
    }
    for (i, record) in wanted.records.iter().enumerate() {
        if !announced.iter().take(MAX_RECORDS).any(|old| old == record) {
            // At most MAX_RECORDS indices.
            let _ = out.announce.push(i);
        }
    }
    out
}

/// A withdrawal's kind and instance, owned, so the record can be
/// withdrawn while its copy in the announcer is dropped.
struct Gone {
    kind: String<MAX_KIND_LEN>,
    instance: String<MAX_INSTANCE_LEN>,
}

impl Gone {
    fn of(service: &Service<'_>) -> Option<Self> {
        Some(Gone {
            kind: String::try_from(service.kind.as_str()).ok()?,
            instance: String::try_from(service.instance).ok()?,
        })
    }
}

/// When answering `sys/info` re-sends every record: a value transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Kick {
    last_ms: Option<u64>,
}

impl Kick {
    /// Never kicked.
    pub const fn new() -> Self {
        Kick { last_ms: None }
    }

    /// A `sys/info` request was answered at `now_ms`: whether to re-send,
    /// and the next state. Re-sends are at least [`KICK_INTERVAL_MS`]
    /// apart.
    pub fn after_info(self, now_ms: u64) -> (Self, bool) {
        match self.last_ms {
            Some(t) if now_ms < t.saturating_add(KICK_INTERVAL_MS) => (self, false),
            _ => (
                Kick {
                    last_ms: Some(now_ms),
                },
                true,
            ),
        }
    }
}

/// The subscriber half of a device that announces and never browses.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoBrowse;

/// Why [`NoBrowse`] yields nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotBrowsing;

impl Subscriber for NoBrowse {
    type Error = NotBrowsing;
    const DELIVERY: Delivery = Delivery::new(Overflow::DropNewest, true);
    const LIMITS: Limits = Limits::new(0, 0);

    /// Always [`NotBrowsing`].
    async fn recv<'b>(&mut self, _buf: &'b mut [u8]) -> Result<Sample<'b>, Self::Error> {
        Err(NotBrowsing)
    }

    /// Always nothing.
    fn try_recv<'b>(&mut self, _buf: &'b mut [u8]) -> Result<Option<Sample<'b>>, Self::Error> {
        Ok(None)
    }
}

/// Why an [`Announcer`] call failed.
pub type AnnounceError<E> = PubSubError<E, NotBrowsing>;

/// The device's announcer: `PubSubDiscovery` over `P` with room for
/// [`MAX_RECORDS`], and the [`Kick`] state.
pub struct Announcer<P> {
    disc: PubSubDiscovery<P, NoBrowse, MAX_RECORDS>,
    kick: Kick,
}

impl<P: Publisher> Announcer<P> {
    /// Nothing announced yet.
    pub fn new(publisher: P) -> Self {
        Announcer {
            disc: PubSubDiscovery::new(publisher, NoBrowse),
            kick: Kick::new(),
        }
    }

    /// What is announced.
    pub fn announced(&self) -> impl Iterator<Item = Service<'_>> {
        self.disc.announced()
    }

    /// Performs [`plan`]: returns the beacons sent. Stops at the first
    /// failed send; the steps after it are planned again by the next call.
    /// The record whose announcement failed counts as announced and goes
    /// out at its next refresh; a goodbye that failed is not repeated (the
    /// gateway expires the record at its TTL).
    pub async fn sync(&mut self, wanted: &Wanted<'_>) -> Result<usize, AnnounceError<P::Error>> {
        self.apply(wanted, false).await
    }

    /// A `sys/info` request was answered at `now_ms`: brings the records
    /// to `wanted` and, unless a kick re-sent them less than
    /// [`KICK_INTERVAL_MS`] ago, sends every wanted record, changed or
    /// not. Returns the beacons sent.
    pub async fn after_info(
        &mut self,
        wanted: &Wanted<'_>,
        now_ms: u64,
    ) -> Result<usize, AnnounceError<P::Error>> {
        let (kick, resend) = self.kick.after_info(now_ms);
        self.kick = kick;
        self.apply(wanted, resend).await
    }

    /// Performs [`plan`]; with `every`, announces every wanted record
    /// rather than only the new or changed ones (re-announcing a record
    /// sends it again and restarts its interval). Withdrawals go one at a
    /// time, re-planned after each (each changes what is announced), at
    /// most [`MAX_RECORDS`] of them.
    async fn apply(
        &mut self,
        wanted: &Wanted<'_>,
        every: bool,
    ) -> Result<usize, AnnounceError<P::Error>> {
        let mut sent = 0;
        for _ in 0..MAX_RECORDS {
            let gone = {
                let now: Vec<Service<'_>, MAX_RECORDS> =
                    self.disc.announced().take(MAX_RECORDS).collect();
                plan(wanted, &now)
                    .withdraw
                    .first()
                    .and_then(|&i| now.get(i))
                    .and_then(Gone::of)
            };
            let Some(gone) = gone else { break };
            let Ok(kind) = Key::new(&gone.kind) else {
                break;
            };
            self.disc
                .withdraw(ServiceKind::new(kind), &gone.instance)
                .await?;
            sent += 1;
        }
        let changed = {
            let now: Vec<Service<'_>, MAX_RECORDS> =
                self.disc.announced().take(MAX_RECORDS).collect();
            plan(wanted, &now).announce
        };
        for (i, record) in wanted.records().iter().enumerate() {
            if every || changed.contains(&i) {
                self.disc.announce(record).await?;
                sent += 1;
            }
        }
        Ok(sent)
    }

    /// Re-sends the records due at `now_ms` (every half TTL).
    pub async fn tick(&mut self, now_ms: u64) -> Result<usize, AnnounceError<P::Error>> {
        self.disc.tick(now_ms).await
    }

    /// When the next [`Announcer::tick`] has work (pure).
    pub fn due(&self, now_ms: u64) -> Option<u64> {
        self.disc.due(now_ms)
    }
}

#[cfg(test)]
pub(crate) mod tests;
