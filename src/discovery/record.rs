//! What is discovered: a [`Service`] of some [`ServiceKind`], reachable at
//! an [`Endpoint`], with RFC 6763 TXT attributes.
//!
//! # Kinds and DNS-SD types
//!
//! A kind is a middleware [`Key`] naming what a service does (`cam/jpeg`,
//! `mw/bus`). Pub/sub discovery uses it verbatim; DNS-SD needs a service
//! name (RFC 6335 §5.1: 1..=15 characters, ASCII letters, digits and
//! hyphens, at least one letter, no leading, trailing or doubled hyphen).
//! [`service_name`] maps a kind onto one by a fixed, reversible rule:
//!
//! * `/` becomes `-`;
//! * lowercase ASCII letters and digits are kept;
//! * anything else — uppercase letters, `-`, `_`, `.` — cannot be mapped
//!   ([`RecordError::Unmappable`]), so the mapping is a bijection and
//!   [`kind_from_service_name`] inverts it exactly;
//! * the result must have at most [`MAX_SERVICE_NAME_LEN`] (15) characters
//!   ([`RecordError::TooLong`]) and contain a letter.
//!
//! Key segments are never empty, so the result never has a leading,
//! trailing or doubled hyphen. [`dns_sd_type`] adds the `_` and the
//! protocol, `cam/jpeg` → `_cam-jpeg._tcp`, and [`dns_sd_type_in`] the
//! domain, → `_cam-jpeg._tcp.local`. Every kind maps to `_tcp`: middleware
//! services are reached over a stream or a session, and DNS-SD uses `_tcp`
//! for everything not on UDP (RFC 6763 §7).
//!
//! # Copying into a caller buffer
//!
//! Backends receive owned or transient data and hand out a [`Service`]
//! borrowing the caller's buffer; [`service_into`] and [`event_into`] do the
//! copy without allocating, failing instead of truncating.

use core::net::SocketAddr;

use crate::dns::wire::{ServiceType, Txt, WireError};
use crate::key::Key;
use crate::transport::SenderId;

pub use crate::dns::wire::TxtWriter;

/// Longest DNS-SD service name (RFC 6335 §5.1).
pub const MAX_SERVICE_NAME_LEN: usize = 15;
/// Longest [`dns_sd_type`] result: `_`, the name, `._tcp`.
pub const MAX_DNS_SD_TYPE_LEN: usize = 1 + MAX_SERVICE_NAME_LEN + 5;

/// Why a record could not be mapped, encoded or copied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RecordError {
    /// The kind has a byte that cannot appear in a DNS-SD service name
    /// (see the module docs), or no letter.
    Unmappable,
    /// The mapped service name is over [`MAX_SERVICE_NAME_LEN`] characters.
    TooLong,
    /// The output buffer is too small.
    BufferTooSmall,
    /// A DNS-SD name does not map back to a kind.
    NotAKind,
    /// A DNS name or TXT record is malformed.
    Wire(WireError),
}

impl From<WireError> for RecordError {
    fn from(e: WireError) -> Self {
        RecordError::Wire(e)
    }
}

/// What a service does: a middleware key such as `cam/jpeg`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ServiceKind<'a>(Key<'a>);

impl<'a> ServiceKind<'a> {
    /// The kind named by `key`.
    pub const fn new(key: Key<'a>) -> Self {
        ServiceKind(key)
    }

    /// A literal kind, checked at compile time in a `const`.
    pub const fn from_static(s: &'static str) -> ServiceKind<'static> {
        ServiceKind(Key::from_static(s))
    }

    /// The key.
    pub const fn key(&self) -> Key<'a> {
        self.0
    }

    /// The key text.
    pub const fn as_str(&self) -> &'a str {
        self.0.as_str()
    }
}

impl<'a> From<Key<'a>> for ServiceKind<'a> {
    fn from(key: Key<'a>) -> Self {
        ServiceKind(key)
    }
}

impl core::fmt::Display for ServiceKind<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.0.fmt(f)
    }
}

/// Writes `parts` one after another into `buf` and returns them as text.
fn concat<'o>(buf: &'o mut [u8], parts: &[&[u8]]) -> Result<&'o str, RecordError> {
    let mut at = 0;
    for part in parts {
        let end = at + part.len();
        buf.get_mut(at..end)
            .ok_or(RecordError::BufferTooSmall)?
            .copy_from_slice(part);
        at = end;
    }
    let out: &'o [u8] = buf;
    core::str::from_utf8(out.get(..at).ok_or(RecordError::BufferTooSmall)?)
        .map_err(|_| RecordError::Unmappable)
}

/// The DNS-SD service name of `kind` (`cam/jpeg` → `cam-jpeg`), written
/// into `buf`; see the module docs for the rule.
pub fn service_name<'o>(kind: ServiceKind<'_>, buf: &'o mut [u8]) -> Result<&'o str, RecordError> {
    let bytes = kind.key().as_bytes();
    if bytes.len() > MAX_SERVICE_NAME_LEN {
        return Err(RecordError::TooLong);
    }
    let mut name = [0u8; MAX_SERVICE_NAME_LEN];
    let mut letter = false;
    for (slot, &b) in name.iter_mut().zip(bytes) {
        *slot = match b {
            b'/' => b'-',
            b'a'..=b'z' => {
                letter = true;
                b
            }
            b'0'..=b'9' => b,
            _ => return Err(RecordError::Unmappable),
        };
    }
    if !letter {
        return Err(RecordError::Unmappable);
    }
    concat(buf, &[&name[..bytes.len()]])
}

/// The DNS-SD service type of `kind` without a domain, `_cam-jpeg._tcp`.
pub fn dns_sd_type<'o>(kind: ServiceKind<'_>, buf: &'o mut [u8]) -> Result<&'o str, RecordError> {
    let mut name = [0u8; MAX_SERVICE_NAME_LEN];
    let name = service_name(kind, &mut name)?;
    concat(buf, &[b"_", name.as_bytes(), b"._tcp"])
}

/// The DNS-SD service type of `kind` in `domain` (e.g. `local`), ready to
/// browse: `_cam-jpeg._tcp.local`.
pub fn dns_sd_type_in<'o>(
    kind: ServiceKind<'_>,
    domain: &str,
    buf: &'o mut [u8],
) -> Result<ServiceType<'o>, RecordError> {
    let mut ty = [0u8; MAX_DNS_SD_TYPE_LEN];
    let ty = dns_sd_type(kind, &mut ty)?;
    let text = concat(buf, &[ty.as_bytes(), b".", domain.as_bytes()])?;
    Ok(ServiceType::new(text)?)
}

/// The kind whose [`service_name`] is `name` (`cam-jpeg` → `cam/jpeg`),
/// written into `buf`. A leading `_` (as in a DNS-SD type label) is
/// accepted and dropped.
pub fn kind_from_service_name<'o>(
    name: &str,
    buf: &'o mut [u8],
) -> Result<ServiceKind<'o>, RecordError> {
    let name = name.strip_prefix('_').unwrap_or(name);
    let ok = !name.is_empty()
        && name.len() <= MAX_SERVICE_NAME_LEN
        && name
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-'))
        && name.bytes().any(|b| b.is_ascii_lowercase());
    if !ok {
        return Err(RecordError::NotAKind);
    }
    let mut kind = [0u8; MAX_SERVICE_NAME_LEN];
    for (slot, b) in kind.iter_mut().zip(name.bytes()) {
        *slot = if b == b'-' { b'/' } else { b };
    }
    let text = concat(buf, &[&kind[..name.len()]])?;
    Key::new(text)
        .map(ServiceKind)
        .map_err(|_| RecordError::NotAKind)
}

/// Where a service is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint<'a> {
    /// An IP socket address (from an SRV + A/AAAA answer, or announced).
    Socket(SocketAddr),
    /// A host name to resolve, and a port (an SRV target without an
    /// address record, a Kubernetes service name).
    Host {
        /// The host name, dotted.
        name: &'a str,
        /// The port.
        port: u16,
    },
    /// A transport-specific locator, e.g. a Zenoh `tcp/10.0.0.2:7447`.
    Locator(&'a str),
    /// Reachable on the transport the announcement came over (a radio
    /// neighbour, a node on the same bus): address it by
    /// [`Service::node`].
    Local,
}

/// One discovered or announced service instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Service<'a> {
    /// What it does.
    pub kind: ServiceKind<'a>,
    /// Which one: unique per kind among its announcers (the DNS-SD instance
    /// label, e.g. `cam-1`). UTF-8, 1..=63 bytes on DNS-SD.
    pub instance: &'a str,
    /// The node offering it, when the transport identifies nodes; usable
    /// as a [`Target::Node`](crate::transport::Target::Node).
    pub node: Option<SenderId>,
    /// Where it is reached.
    pub endpoint: Endpoint<'a>,
    /// RFC 6763 TXT attributes: length-prefixed `key=value` strings, as
    /// built by [`TxtWriter`] and read with [`Service::txt`]. Empty for none.
    pub txt: &'a [u8],
    /// Seconds the announcement stays valid; 0 is a goodbye.
    pub ttl_s: u32,
}

impl<'a> Service<'a> {
    /// The attributes, validated.
    pub fn txt(&self) -> Result<Txt<'a>, WireError> {
        Txt::new(self.txt)
    }

    /// True for a goodbye (TTL 0).
    pub const fn is_goodbye(&self) -> bool {
        self.ttl_s == 0
    }
}

/// What a [`Browse`](crate::discovery::Browse) reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event<'a> {
    /// A service was announced or refreshed (TTL > 0).
    Up(Service<'a>),
    /// A service withdrew (a goodbye, or an announcement with TTL 0).
    Down {
        /// Its kind.
        kind: ServiceKind<'a>,
        /// Its instance name.
        instance: &'a str,
    },
}

impl<'a> Event<'a> {
    /// The event an announcement of `service` means: [`Event::Down`] when
    /// its TTL is 0, [`Event::Up`] otherwise.
    pub const fn from_service(service: Service<'a>) -> Self {
        if service.ttl_s == 0 {
            Event::Down {
                kind: service.kind,
                instance: service.instance,
            }
        } else {
            Event::Up(service)
        }
    }

    /// The kind concerned.
    pub const fn kind(&self) -> ServiceKind<'a> {
        match self {
            Event::Up(s) => s.kind,
            Event::Down { kind, .. } => *kind,
        }
    }

    /// The instance concerned.
    pub const fn instance(&self) -> &'a str {
        match self {
            Event::Up(s) => s.instance,
            Event::Down { instance, .. } => instance,
        }
    }
}

/// Hands out consecutive pieces of a buffer.
struct Bump<'b> {
    rest: &'b mut [u8],
}

impl<'b> Bump<'b> {
    fn bytes(&mut self, src: &[u8]) -> Result<&'b [u8], RecordError> {
        let (head, tail) = core::mem::take(&mut self.rest)
            .split_at_mut_checked(src.len())
            .ok_or(RecordError::BufferTooSmall)?;
        head.copy_from_slice(src);
        self.rest = tail;
        Ok(head)
    }

    fn str(&mut self, src: &str) -> Result<&'b str, RecordError> {
        core::str::from_utf8(self.bytes(src.as_bytes())?).map_err(|_| RecordError::Unmappable)
    }

    fn kind(&mut self, kind: ServiceKind<'_>) -> Result<ServiceKind<'b>, RecordError> {
        Key::new(self.str(kind.as_str())?)
            .map(ServiceKind)
            .map_err(|_| RecordError::NotAKind)
    }
}

/// Copies `service`'s strings into `buf` and returns it borrowing `buf`.
pub fn service_into<'b>(
    buf: &'b mut [u8],
    service: &Service<'_>,
) -> Result<Service<'b>, RecordError> {
    let mut bump = Bump { rest: buf };
    let kind = bump.kind(service.kind)?;
    let instance = bump.str(service.instance)?;
    let endpoint = match service.endpoint {
        Endpoint::Socket(a) => Endpoint::Socket(a),
        Endpoint::Host { name, port } => Endpoint::Host {
            name: bump.str(name)?,
            port,
        },
        Endpoint::Locator(l) => Endpoint::Locator(bump.str(l)?),
        Endpoint::Local => Endpoint::Local,
    };
    let txt = bump.bytes(service.txt)?;
    Ok(Service {
        kind,
        instance,
        node: service.node,
        endpoint,
        txt,
        ttl_s: service.ttl_s,
    })
}

/// Copies `event`'s strings into `buf` and returns it borrowing `buf`.
pub fn event_into<'b>(buf: &'b mut [u8], event: &Event<'_>) -> Result<Event<'b>, RecordError> {
    match event {
        Event::Up(s) => service_into(buf, s).map(Event::Up),
        Event::Down { kind, instance } => {
            let mut bump = Bump { rest: buf };
            Ok(Event::Down {
                kind: bump.kind(*kind)?,
                instance: bump.str(instance)?,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    const CAM: ServiceKind<'static> = ServiceKind::from_static("cam/jpeg");

    #[test]
    fn kinds_map_to_dns_sd_types_and_back() {
        let mut buf = [0u8; 32];
        assert_eq!(service_name(CAM, &mut buf), Ok("cam-jpeg"));
        assert_eq!(dns_sd_type(CAM, &mut buf), Ok("_cam-jpeg._tcp"));
        assert_eq!(
            dns_sd_type_in(ServiceKind::from_static("mw/bus"), "local", &mut buf)
                .map(|t| t.as_str()),
            Ok("_mw-bus._tcp.local")
        );
        assert_eq!(
            dns_sd_type(ServiceKind::from_static("a1/2b/c"), &mut buf),
            Ok("_a1-2b-c._tcp")
        );
        // Exactly 15 characters is the limit.
        assert_eq!(
            dns_sd_type(ServiceKind::from_static("abcdefg/hijklmn"), &mut buf),
            Ok("_abcdefg-hijklmn._tcp")
        );
        let mut back = [0u8; 16];
        assert_eq!(kind_from_service_name("cam-jpeg", &mut back), Ok(CAM));
        assert_eq!(
            kind_from_service_name("_mw-bus", &mut back).map(|k| k.as_str()),
            Ok("mw/bus")
        );
    }

    #[test]
    fn unmappable_kinds_are_refused() {
        let mut buf = [0u8; 32];
        for (kind, err) in [
            ("Cam/jpeg", RecordError::Unmappable),
            ("cam-jpeg", RecordError::Unmappable),
            ("cam_jpeg", RecordError::Unmappable),
            ("cam.jpeg", RecordError::Unmappable),
            ("12/34", RecordError::Unmappable),
            ("abcdefgh/ijklmno", RecordError::TooLong),
        ] {
            let kind = ServiceKind::new(Key::new(kind).unwrap());
            assert_eq!(dns_sd_type(kind, &mut buf), Err(err), "{kind}");
        }
        assert_eq!(
            dns_sd_type(CAM, &mut buf[..13]),
            Err(RecordError::BufferTooSmall)
        );
        let mut back = [0u8; 16];
        for bad in [
            "",
            "-a",
            "a--b",
            "a-",
            "A",
            "123",
            "a_b",
            "abcdefghijklmnop",
        ] {
            assert!(kind_from_service_name(bad, &mut back).is_err(), "{bad}");
        }
    }

    #[test]
    fn events_follow_ttl_and_copy_into_buffers() {
        let s = Service {
            kind: CAM,
            instance: "cam-1",
            node: Some(SenderId([3; 8])),
            endpoint: Endpoint::Host {
                name: "cam.local",
                port: 80,
            },
            txt: b"\x03a=b",
            ttl_s: 30,
        };
        assert_eq!(Event::from_service(s), Event::Up(s));
        let gone = Event::from_service(Service { ttl_s: 0, ..s });
        assert_eq!(
            gone,
            Event::Down {
                kind: CAM,
                instance: "cam-1"
            }
        );
        assert_eq!((gone.kind(), gone.instance()), (CAM, "cam-1"));
        assert_eq!(s.txt().unwrap().get(b"a"), Some(&b"b"[..]));
        let mut buf = [0u8; 64];
        assert_eq!(service_into(&mut buf, &s), Ok(s));
        assert_eq!(event_into(&mut buf, &gone), Ok(gone));
        let need = "cam/jpeg".len() + "cam-1".len() + "cam.local".len() + 4;
        assert_eq!(
            service_into(&mut buf[..need - 1], &s),
            Err(RecordError::BufferTooSmall)
        );
        assert!(service_into(&mut buf[..need], &s).is_ok());
    }
}
