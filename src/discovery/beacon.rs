//! Discovery over any pub/sub transport: announcements are published as
//! beacons on `disc/<kind>` and browsing subscribes to them.
//!
//! [`PubSubDiscovery`] implements [`Announce`] and [`Browse`] over a
//! [`Publisher`] and a [`Subscriber`] the caller created on
//! [`BEACON_PREFIX`] (`disc`, every kind) or on one kind's beacon key. It
//! works on any backend, from ESP-NOW to Zenoh, at the cost of one beacon
//! per announced service per re-announce interval.
//!
//! # Beacon wire format, version 1
//!
//! The key is `disc/<kind>` ([`beacon_key`]); the payload ([`encode`],
//! [`decode`]) is, integers little endian:
//!
//! | bytes | field |
//! |-------|-------|
//! | 1 | version, [`BEACON_VERSION`] (1) |
//! | 1 | flags: bit 0 set when a node id follows; other bits 0 |
//! | 1 | endpoint tag: 0 local, 1 IPv4 socket, 2 IPv6 socket, 3 host, 4 locator |
//! | 4 | TTL in seconds (0: goodbye) |
//! | 8 | node id, only with flag bit 0 |
//! | 1 + n | instance: length 1..=63, UTF-8 |
//! | … | endpoint: IPv4 4 address bytes + 2 port; IPv6 16 + 2 port; host 1 length + name + 2 port; locator 1 length + text; local nothing |
//! | 2 + n | TXT RDATA: length, bytes (RFC 6763 length-prefixed strings) |
//!
//! [`decode`] ignores bytes after the TXT field, so later versions may
//! append fields; a different version byte is
//! [`BeaconError::Version`]. A goodbye ([`Announce::withdraw`]) is a beacon
//! with TTL 0, endpoint local and no TXT. Beacons are at most
//! [`MAX_BEACON_LEN`] bytes.
//!
//! # Re-announcing
//!
//! Beacons are not acknowledged and browsers that subscribe late miss the
//! first one, so announced services are kept (up to `A`, in
//! [`ServiceRecord`]s) and re-published every half TTL
//! ([`reannounce_after_ms`]) by [`PubSubDiscovery::tick`], which the
//! imperative shell calls with the time; [`PubSubDiscovery::due`] (pure)
//! says when the next one is due so the shell can sleep until then. The
//! first `tick` after an `announce` starts that record's interval.

use core::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use crate::dns::wire::Txt;
use crate::key::{Key, KeyFilter, KeyFilterBuf, MAX_KEY_LEN};
use crate::transport::{Publisher, SenderId, Subscriber};

use super::directory::{MAX_INSTANCE_LEN, ServiceRecord};
use super::record::{Endpoint, Event, Service, ServiceKind, event_into};
use super::{Announce, Browse};

/// The beacon format version written and read.
pub const BEACON_VERSION: u8 = 1;
/// First segment of every beacon key.
pub const BEACON_PREFIX: Key<'static> = Key::from_static("disc");
/// Largest beacon [`encode`] writes and [`PubSubDiscovery`] receives.
pub const MAX_BEACON_LEN: usize = 512;
/// Beacons skipped (other kinds, malformed) in one receive call before it
/// gives up with [`PubSubError::SkipLimit`].
pub const MAX_SKIPPED_BEACONS: usize = 64;

const FLAG_NODE: u8 = 1;
const TAG_LOCAL: u8 = 0;
const TAG_V4: u8 = 1;
const TAG_V6: u8 = 2;
const TAG_HOST: u8 = 3;
const TAG_LOCATOR: u8 = 4;

/// Why a beacon could not be encoded or decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BeaconError {
    /// The payload ends inside a field.
    Truncated,
    /// The version byte is not [`BEACON_VERSION`].
    Version(u8),
    /// Unknown flag bits or endpoint tag.
    Malformed,
    /// A string is empty, too long for its length byte, or not UTF-8, or
    /// the instance is over [`MAX_INSTANCE_LEN`] bytes.
    BadString,
    /// The TXT field is not valid TXT RDATA.
    BadTxt,
    /// The key is not `disc/<kind>`.
    NotABeacon,
    /// The output buffer is too small.
    BufferTooSmall,
}

/// `disc/<kind>`, written into `buf`.
pub fn beacon_key<'o>(kind: ServiceKind<'_>, buf: &'o mut [u8]) -> Result<Key<'o>, BeaconError> {
    let prefix = BEACON_PREFIX.as_bytes();
    let kind = kind.as_str().as_bytes();
    let len = prefix.len() + 1 + kind.len();
    let out = buf.get_mut(..len).ok_or(BeaconError::BufferTooSmall)?;
    out[..prefix.len()].copy_from_slice(prefix);
    out[prefix.len()] = b'/';
    out[prefix.len() + 1..].copy_from_slice(kind);
    let out: &'o [u8] = out;
    let text = core::str::from_utf8(out).map_err(|_| BeaconError::NotABeacon)?;
    Key::new(text).map_err(|_| BeaconError::NotABeacon)
}

/// The kind a beacon key announces, `None` unless it is `disc/<kind>`.
pub fn kind_of(key: Key<'_>) -> Option<ServiceKind<'_>> {
    let rest = key
        .as_str()
        .strip_prefix(BEACON_PREFIX.as_str())?
        .strip_prefix('/')?;
    Key::new(rest).ok().map(ServiceKind::new)
}

struct Out<'o> {
    buf: &'o mut [u8],
    at: usize,
}

impl Out<'_> {
    fn put(&mut self, bytes: &[u8]) -> Result<(), BeaconError> {
        let end = self.at + bytes.len();
        self.buf
            .get_mut(self.at..end)
            .ok_or(BeaconError::BufferTooSmall)?
            .copy_from_slice(bytes);
        self.at = end;
        Ok(())
    }

    fn string(&mut self, s: &str) -> Result<(), BeaconError> {
        let len = u8::try_from(s.len()).map_err(|_| BeaconError::BadString)?;
        if len == 0 {
            return Err(BeaconError::BadString);
        }
        self.put(&[len])?;
        self.put(s.as_bytes())
    }
}

/// Encodes `service` as a beacon payload into `out` and returns its length.
pub fn encode(service: &Service<'_>, out: &mut [u8]) -> Result<usize, BeaconError> {
    if service.instance.len() > MAX_INSTANCE_LEN {
        return Err(BeaconError::BadString);
    }
    Txt::new(service.txt).map_err(|_| BeaconError::BadTxt)?;
    let tag = match service.endpoint {
        Endpoint::Local => TAG_LOCAL,
        Endpoint::Socket(SocketAddr::V4(_)) => TAG_V4,
        Endpoint::Socket(SocketAddr::V6(_)) => TAG_V6,
        Endpoint::Host { .. } => TAG_HOST,
        Endpoint::Locator(_) => TAG_LOCATOR,
    };
    let flags = if service.node.is_some() { FLAG_NODE } else { 0 };
    let mut w = Out { buf: out, at: 0 };
    w.put(&[BEACON_VERSION, flags, tag])?;
    w.put(&service.ttl_s.to_le_bytes())?;
    if let Some(SenderId(id)) = service.node {
        w.put(&id)?;
    }
    w.string(service.instance)?;
    match service.endpoint {
        Endpoint::Local => {}
        Endpoint::Socket(a) => {
            match a.ip() {
                IpAddr::V4(ip) => w.put(&ip.octets())?,
                IpAddr::V6(ip) => w.put(&ip.octets())?,
            }
            w.put(&a.port().to_le_bytes())?;
        }
        Endpoint::Host { name, port } => {
            w.string(name)?;
            w.put(&port.to_le_bytes())?;
        }
        Endpoint::Locator(l) => w.string(l)?,
    }
    let txt_len = u16::try_from(service.txt.len()).map_err(|_| BeaconError::BadTxt)?;
    w.put(&txt_len.to_le_bytes())?;
    w.put(service.txt)?;
    Ok(w.at)
}

struct In<'a> {
    rest: &'a [u8],
}

impl<'a> In<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], BeaconError> {
        let (head, tail) = self
            .rest
            .split_at_checked(n)
            .ok_or(BeaconError::Truncated)?;
        self.rest = tail;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], BeaconError> {
        self.take(N)?.try_into().map_err(|_| BeaconError::Truncated)
    }

    fn port(&mut self) -> Result<u16, BeaconError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    fn string(&mut self) -> Result<&'a str, BeaconError> {
        let [len] = self.array()?;
        if len == 0 {
            return Err(BeaconError::BadString);
        }
        core::str::from_utf8(self.take(usize::from(len))?).map_err(|_| BeaconError::BadString)
    }
}

/// Decodes a beacon payload of `kind` (from [`kind_of`] the key). Bytes
/// after the TXT field are ignored.
pub fn decode<'a>(kind: ServiceKind<'a>, payload: &'a [u8]) -> Result<Service<'a>, BeaconError> {
    let mut r = In { rest: payload };
    let [version, flags, tag] = r.array()?;
    if version != BEACON_VERSION {
        return Err(BeaconError::Version(version));
    }
    if flags & !FLAG_NODE != 0 {
        return Err(BeaconError::Malformed);
    }
    let ttl_s = u32::from_le_bytes(r.array()?);
    let node = if flags & FLAG_NODE != 0 {
        Some(SenderId(r.array()?))
    } else {
        None
    };
    let instance = r.string()?;
    if instance.len() > MAX_INSTANCE_LEN {
        return Err(BeaconError::BadString);
    }
    let endpoint = match tag {
        TAG_LOCAL => Endpoint::Local,
        TAG_V4 => {
            let ip = Ipv4Addr::from(r.array::<4>()?);
            Endpoint::Socket(SocketAddr::new(IpAddr::V4(ip), r.port()?))
        }
        TAG_V6 => {
            let ip = Ipv6Addr::from(r.array::<16>()?);
            Endpoint::Socket(SocketAddr::new(IpAddr::V6(ip), r.port()?))
        }
        TAG_HOST => {
            let name = r.string()?;
            Endpoint::Host {
                name,
                port: r.port()?,
            }
        }
        TAG_LOCATOR => Endpoint::Locator(r.string()?),
        _ => return Err(BeaconError::Malformed),
    };
    let txt_len = u16::from_le_bytes(r.array()?);
    let txt = r.take(usize::from(txt_len))?;
    Txt::new(txt).map_err(|_| BeaconError::BadTxt)?;
    Ok(Service {
        kind,
        instance,
        node,
        endpoint,
        txt,
        ttl_s,
    })
}

/// Milliseconds between re-announcements of a record with `ttl_s`: half
/// the TTL, so a browser sees two beacons per TTL and one lost beacon does
/// not expire the record. At least 1 ms.
pub const fn reannounce_after_ms(ttl_s: u32) -> u64 {
    let ms = ttl_s as u64 * 500;
    if ms == 0 { 1 } else { ms }
}

/// Failure of a [`PubSubDiscovery`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PubSubError<P, S> {
    /// Publishing a beacon failed.
    Publish(P),
    /// Receiving failed.
    Receive(S),
    /// A beacon could not be encoded.
    Beacon(BeaconError),
    /// The announced service has a field over the [`ServiceRecord`] bounds,
    /// or all `A` announcement slots are taken.
    Unstorable,
    /// [`MAX_SKIPPED_BEACONS`] samples passed without a matching beacon.
    SkipLimit,
    /// The event does not fit the caller's buffer.
    BufferTooSmall,
}

#[derive(Debug, Clone)]
struct Announced {
    record: ServiceRecord,
    /// When it was last published, once a `tick` has seen it.
    last_ms: Option<u64>,
}

/// [`Announce`] + [`Browse`] over a publisher and a subscriber to
/// `disc/...` beacons, keeping up to `A` announced services for
/// re-announcing; see the module docs.
pub struct PubSubDiscovery<P, S, const A: usize = 4> {
    publisher: P,
    subscriber: S,
    filter: Option<KeyFilterBuf<MAX_KEY_LEN>>,
    announced: [Option<Announced>; A],
    /// Receive buffer: key plus beacon.
    rx: [u8; MAX_KEY_LEN + MAX_BEACON_LEN],
}

impl<P, S, const A: usize> PubSubDiscovery<P, S, A> {
    /// Announces through `publisher` and browses the beacons `subscriber`
    /// receives (subscribe it to [`BEACON_PREFIX`] or a beacon key).
    pub fn new(publisher: P, subscriber: S) -> Self {
        PubSubDiscovery {
            publisher,
            subscriber,
            filter: None,
            announced: [const { None }; A],
            rx: [0; MAX_KEY_LEN + MAX_BEACON_LEN],
        }
    }

    /// Returns the publisher and subscriber.
    pub fn into_parts(self) -> (P, S) {
        (self.publisher, self.subscriber)
    }

    /// The services currently announced.
    pub fn announced(&self) -> impl Iterator<Item = Service<'_>> {
        self.announced.iter().flatten().map(|a| a.record.service())
    }

    /// When the next re-announcement is due (pure): `Some(now_ms)` if one is
    /// due now or a record has not been ticked yet, `None` when nothing is
    /// announced.
    pub fn due(&self, now_ms: u64) -> Option<u64> {
        self.announced
            .iter()
            .flatten()
            .map(|a| match a.last_ms {
                None => now_ms,
                Some(t) => t
                    .saturating_add(reannounce_after_ms(a.record.ttl_s()))
                    .max(now_ms),
            })
            .min()
    }
}

/// Publishes `service`'s beacon.
async fn send<P: Publisher, S>(
    publisher: &mut P,
    service: &Service<'_>,
) -> Result<(), PubSubError<P::Error, S>> {
    let mut key = [0u8; MAX_KEY_LEN];
    let key = beacon_key(service.kind, &mut key).map_err(PubSubError::Beacon)?;
    let mut payload = [0u8; MAX_BEACON_LEN];
    let n = encode(service, &mut payload).map_err(PubSubError::Beacon)?;
    publisher
        .publish(key, &payload[..n])
        .await
        .map_err(PubSubError::Publish)
}

impl<P: Publisher, S: Subscriber, const A: usize> PubSubDiscovery<P, S, A> {
    /// Re-publishes every announced service whose interval has elapsed at
    /// `now_ms`, starting the interval of those announced since the last
    /// tick, and returns how many beacons were sent.
    pub async fn tick(&mut self, now_ms: u64) -> Result<usize, PubSubError<P::Error, S::Error>> {
        let mut sent = 0;
        for a in self.announced.iter_mut().flatten() {
            match a.last_ms {
                None => a.last_ms = Some(now_ms),
                Some(t) if now_ms >= t.saturating_add(reannounce_after_ms(a.record.ttl_s())) => {
                    // Advance first: a failed or dropped send is not retried
                    // in a burst.
                    a.last_ms = Some(now_ms);
                    send::<P, S::Error>(&mut self.publisher, &a.record.service()).await?;
                    sent += 1;
                }
                Some(_) => {}
            }
        }
        Ok(sent)
    }

    /// The event a received sample means, if it is a beacon the filter
    /// accepts.
    fn accept<'s>(
        filter: &Option<KeyFilterBuf<MAX_KEY_LEN>>,
        key: Key<'s>,
        payload: &'s [u8],
    ) -> Option<Event<'s>> {
        let kind = kind_of(key)?;
        if !filter.as_ref()?.matches(kind.key()) {
            return None;
        }
        decode(kind, payload).ok().map(Event::from_service)
    }
}

impl<P: Publisher, S: Subscriber, const A: usize> Announce for PubSubDiscovery<P, S, A> {
    type Error = PubSubError<P::Error, S::Error>;

    /// Publishes the beacon now and keeps the service for [`tick`](Self::tick)
    /// (replacing an earlier announcement of the same kind and instance).
    /// A service that cannot be stored is [`PubSubError::Unstorable`] and is
    /// not published.
    async fn announce(&mut self, service: &Service<'_>) -> Result<(), Self::Error> {
        let record = ServiceRecord::new(service).ok_or(PubSubError::Unstorable)?;
        let slot = self
            .announced
            .iter()
            .position(|a| matches!(a, Some(a) if a.record.is(service.kind, service.instance)))
            .or_else(|| self.announced.iter().position(Option::is_none))
            .ok_or(PubSubError::Unstorable)?;
        self.announced[slot] = Some(Announced {
            record,
            last_ms: None,
        });
        send::<P, S::Error>(&mut self.publisher, service).await
    }

    /// Forgets the service and publishes its goodbye (TTL 0), whether or not
    /// it was announced here.
    async fn withdraw(&mut self, kind: ServiceKind<'_>, instance: &str) -> Result<(), Self::Error> {
        for a in self.announced.iter_mut() {
            if matches!(a, Some(x) if x.record.is(kind, instance)) {
                *a = None;
            }
        }
        let goodbye = Service {
            kind,
            instance,
            node: None,
            endpoint: Endpoint::Local,
            txt: &[],
            ttl_s: 0,
        };
        send::<P, S::Error>(&mut self.publisher, &goodbye).await
    }
}

impl<P: Publisher, S: Subscriber, const A: usize> Browse for PubSubDiscovery<P, S, A> {
    type Error = PubSubError<P::Error, S::Error>;

    /// Sets the kinds reported (replacing any earlier filter). Sends
    /// nothing: services show up at their next beacon, within half their
    /// TTL. Before the first `browse`, received beacons are discarded.
    async fn browse(&mut self, filter: KeyFilter<'_>) -> Result<(), Self::Error> {
        self.filter = Some(
            filter
                .to_buf()
                .map_err(|_| PubSubError::Beacon(BeaconError::NotABeacon))?,
        );
        Ok(())
    }

    /// Waits for the next beacon of a browsed kind; other samples and
    /// malformed beacons are skipped (at most [`MAX_SKIPPED_BEACONS`]).
    /// Cancel safe if the subscriber's `recv` is.
    async fn next<'b>(&mut self, buf: &'b mut [u8]) -> Result<Event<'b>, Self::Error> {
        for _ in 0..MAX_SKIPPED_BEACONS {
            let sample = self
                .subscriber
                .recv(&mut self.rx)
                .await
                .map_err(PubSubError::Receive)?;
            if let Some(event) = Self::accept(&self.filter, sample.key, sample.payload) {
                return event_into(buf, &event).map_err(|_| PubSubError::BufferTooSmall);
            }
        }
        Err(PubSubError::SkipLimit)
    }

    fn try_next<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Event<'b>>, Self::Error> {
        for _ in 0..MAX_SKIPPED_BEACONS {
            let Some(sample) = self
                .subscriber
                .try_recv(&mut self.rx)
                .map_err(PubSubError::Receive)?
            else {
                return Ok(None);
            };
            if let Some(event) = Self::accept(&self.filter, sample.key, sample.payload) {
                return event_into(buf, &event)
                    .map(Some)
                    .map_err(|_| PubSubError::BufferTooSmall);
            }
        }
        Err(PubSubError::SkipLimit)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::transport::{Delivery, Limits, Overflow, Sample, sample_into};
    use futures::executor::block_on;
    use std::collections::VecDeque;
    use std::vec::Vec;

    const CAM: ServiceKind<'static> = ServiceKind::from_static("cam/jpeg");

    fn cam(endpoint: Endpoint<'static>) -> Service<'static> {
        Service {
            kind: CAM,
            instance: "cam-1",
            node: Some(SenderId([9; 8])),
            endpoint,
            txt: b"\x05res=1",
            ttl_s: 30,
        }
    }

    #[test]
    fn beacons_round_trip_every_endpoint() {
        for endpoint in [
            Endpoint::Local,
            Endpoint::Socket("10.0.0.2:7447".parse().unwrap()),
            Endpoint::Socket("[fe80::1]:80".parse().unwrap()),
            Endpoint::Host {
                name: "cam.local",
                port: 8080,
            },
            Endpoint::Locator("tcp/10.0.0.2:7447"),
        ] {
            let s = cam(endpoint);
            let mut out = [0u8; 128];
            let n = encode(&s, &mut out).unwrap();
            assert_eq!(decode(CAM, &out[..n]), Ok(s));
            // Tolerant of trailing bytes (later versions append fields).
            assert_eq!(decode(CAM, &out[..n + 3]), Ok(s));
            for cut in 0..n {
                assert!(decode(CAM, &out[..cut]).is_err(), "cut {cut}");
            }
            assert_eq!(
                encode(&s, &mut out[..n - 1]),
                Err(BeaconError::BufferTooSmall)
            );
        }
        let anon = Service {
            node: None,
            txt: b"",
            ..cam(Endpoint::Local)
        };
        let mut out = [0u8; 64];
        let n = encode(&anon, &mut out).unwrap();
        assert_eq!(&out[..n], b"\x01\x00\x00\x1e\x00\x00\x00\x05cam-1\x00\x00");
        assert_eq!(decode(CAM, &out[..n]), Ok(anon));
    }

    #[test]
    fn malformed_beacons_are_refused() {
        let mut out = [0u8; 64];
        let n = encode(&cam(Endpoint::Local), &mut out).unwrap();
        let mut v2 = out;
        v2[0] = 2;
        assert_eq!(decode(CAM, &v2[..n]), Err(BeaconError::Version(2)));
        let mut flags = out;
        flags[1] = 0x81;
        assert_eq!(decode(CAM, &flags[..n]), Err(BeaconError::Malformed));
        let mut tag = out;
        tag[2] = 9;
        assert_eq!(decode(CAM, &tag[..n]), Err(BeaconError::Malformed));
        let bad_txt = Service {
            txt: b"\x09x",
            ..cam(Endpoint::Local)
        };
        assert_eq!(encode(&bad_txt, &mut out), Err(BeaconError::BadTxt));
        let empty = Service {
            instance: "",
            ..cam(Endpoint::Local)
        };
        assert_eq!(encode(&empty, &mut out), Err(BeaconError::BadString));
    }

    #[test]
    fn beacon_keys_carry_the_kind() {
        let mut buf = [0u8; 32];
        let key = beacon_key(CAM, &mut buf).unwrap();
        assert_eq!(key.as_str(), "disc/cam/jpeg");
        assert_eq!(kind_of(key), Some(CAM));
        assert_eq!(kind_of(Key::from_static("disc")), None);
        assert_eq!(kind_of(Key::from_static("discs/x")), None);
        assert_eq!(kind_of(Key::from_static("app/x")), None);
    }

    /// A loopback: published samples queue up for the subscriber half.
    #[derive(Default)]
    struct Loop(VecDeque<(std::string::String, Vec<u8>)>);

    struct LoopPub<'a>(&'a core::cell::RefCell<Loop>);
    struct LoopSub<'a>(&'a core::cell::RefCell<Loop>);

    impl Publisher for LoopPub<'_> {
        type Error = ();
        async fn publish(&mut self, key: Key<'_>, payload: &[u8]) -> Result<(), ()> {
            self.0
                .borrow_mut()
                .0
                .push_back((key.as_str().into(), payload.to_vec()));
            Ok(())
        }
    }

    impl Subscriber for LoopSub<'_> {
        type Error = ();
        const DELIVERY: Delivery = Delivery::new(Overflow::Block, true);
        const LIMITS: Limits = Limits::UNBOUNDED;
        async fn recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Sample<'b>, ()> {
            self.try_recv(buf)?.ok_or(())
        }
        fn try_recv<'b>(&mut self, buf: &'b mut [u8]) -> Result<Option<Sample<'b>>, ()> {
            match self.0.borrow_mut().0.pop_front() {
                None => Ok(None),
                Some((k, p)) => sample_into(buf, &k, &p).map(Some).ok_or(()),
            }
        }
    }

    #[test]
    fn pubsub_discovery_announces_browses_and_reannounces() {
        let bus = core::cell::RefCell::new(Loop::default());
        let mut d: PubSubDiscovery<_, _, 2> = PubSubDiscovery::new(LoopPub(&bus), LoopSub(&bus));
        let mut buf = [0u8; 256];
        // Not browsing yet: beacons are discarded.
        block_on(d.announce(&cam(Endpoint::Local))).unwrap();
        assert_eq!(d.try_next(&mut buf), Ok(None));
        block_on(d.browse(KeyFilter::Prefix(Key::from_static("cam")))).unwrap();
        // Other kinds and foreign samples are skipped.
        let other = Service {
            kind: ServiceKind::from_static("mw/bus"),
            ..cam(Endpoint::Local)
        };
        block_on(d.announce(&other)).unwrap();
        bus.borrow_mut()
            .0
            .push_back(("app/x".into(), b"junk".to_vec()));
        block_on(d.announce(&cam(Endpoint::Host { name: "h", port: 1 }))).unwrap();
        assert_eq!(
            block_on(d.next(&mut buf)),
            Ok(Event::Up(cam(Endpoint::Host { name: "h", port: 1 })))
        );
        assert_eq!(d.announced().count(), 2, "re-announcing cam-1 replaced it");
        // A third distinct service does not fit A = 2.
        let third = Service {
            instance: "cam-3",
            ..cam(Endpoint::Local)
        };
        assert_eq!(block_on(d.announce(&third)), Err(PubSubError::Unstorable));

        // Re-announce schedule: the first tick starts the interval.
        assert_eq!(d.due(1_000), Some(1_000));
        assert_eq!(block_on(d.tick(1_000)), Ok(0));
        assert_eq!(d.due(1_000), Some(16_000));
        assert_eq!(block_on(d.tick(15_999)), Ok(0));
        assert_eq!(block_on(d.tick(16_000)), Ok(2));
        assert!(matches!(d.try_next(&mut buf), Ok(Some(Event::Up(s))) if s.instance == "cam-1"));

        // Withdraw: a goodbye is a Down event and the record is forgotten.
        block_on(d.withdraw(CAM, "cam-1")).unwrap();
        assert_eq!(
            d.try_next(&mut buf),
            Ok(Some(Event::Down {
                kind: CAM,
                instance: "cam-1"
            }))
        );
        assert_eq!(d.try_next(&mut buf), Ok(None));
        assert_eq!(d.announced().count(), 1);
        assert_eq!(reannounce_after_ms(0), 1);
    }
}
