//! Binding records: where a payload's channels come from and go to
//! (BD1–BD6).
//!
//! A payload declares named channels in its image
//! ([`crate::wasm_abi::channels`]); a binding maps each name to a
//! [`Target`]. Bindings are separate from the image and separately signed,
//! so a payload can be rebound (a different microphone rate, a key on
//! another node) without uploading it again. A binding names the image it
//! is for by SHA-256; a device ignores a binding for another image.
//!
//! # Layout
//!
//! `[version u8 = 1][image sha256 32][binding version u32 LE][n u8]`, then
//! `n` entries `[name_len u8][name][target u8][len u8][body]`:
//!
//! | target | body                                                     |
//! |--------|----------------------------------------------------------|
//! | 1 [`Target::Local`]  | [`SourceKind`] code u8, then its `route_source` config ([`route::source_cfg_len`] bytes) |
//! | 2 [`Target::Key`]    | a key (not under `sys/`)                    |
//! | 3 [`Target::Remote`] | node id (8), then a key                     |
//! | 4 [`Target::Adapted`] | [`Adapter`] code u8, parameter u16 LE, then an inner target byte and its body (Local, Key or Remote) |
//! | 5 [`Target::Driver`] (ABI 8) | a control-module point's name ([`crate::loader::control`]; channel-name rules) |
//!
//! A driver target binds a machine channel to one of the node's native
//! drivers: the runtime carries it on the node-internal key `drv/<point>`
//! ([`driver_key`]), which no link sends or delivers, and the node passes
//! what the payload publishes there to the driver and what the driver
//! reads to the payload. The node checks the point exists and that its
//! kind and direction fit the channel's before the payload starts.
//!
//! At most [`MAX_CHANNELS`] entries with distinct valid names; the record
//! is at most [`BINDING_MAX`] bytes and ends after its last entry.
//!
//! # `sys/payload/bind`
//!
//! The request body is the binding, optionally followed by
//! [`SIGNED_BINDING_LEN`] bytes `[version u32 LE][signature 64]` whose
//! version equals the binding's. The signature is over
//! [`binding_message`]: `"mw-binding-v1\0" || node id (8) || image sha256
//! || version LE || sha256(binding)`, so a binding signed for one device
//! is refused by every other. A claimed device takes a binding only with a
//! signature by its pinned key and a version above the last binding it
//! accepted, for any image, so no earlier binding can be replayed
//! ([`super::trust::TrustState::accept_binding`]); over a serial port, as
//! for `start` and `stop`, an unsigned binding is accepted too. `sys/payload/binding` reads the current binding back.

use crate::key::Key;
use crate::transport::SenderId;
use crate::wasm_abi::channels::{MAX_CHANNELS, valid_name};
use crate::wasm_abi::route::{self, RouteSource, SourceKind};

use super::trust::{SIGNATURE_LEN, Signature, Version};

/// The record format version.
pub const VERSION: u8 = 1;

/// Largest binding record.
pub const BINDING_MAX: usize = 1024;

/// Bytes before the first entry.
pub const HEADER_LEN: usize = 1 + 32 + 4 + 1;

/// Length of the optional signature trailer of `sys/payload/bind`.
pub const SIGNED_BINDING_LEN: usize = 4 + SIGNATURE_LEN;

/// Domain-separation prefix of [`binding_message`].
pub const MESSAGE_TAG: &[u8; 14] = b"mw-binding-v1\0";

/// Length of [`binding_message`].
pub const MESSAGE_LEN: usize = MESSAGE_TAG.len() + 8 + 32 + 4 + 32;

/// Target codes.
pub mod target {
    /// [`super::Target::Local`].
    pub const LOCAL: u8 = 1;
    /// [`super::Target::Key`].
    pub const KEY: u8 = 2;
    /// [`super::Target::Remote`].
    pub const REMOTE: u8 = 3;
    /// [`super::Target::Adapted`].
    pub const ADAPTED: u8 = 4;
    /// [`super::Target::Driver`] (ABI 8).
    pub const DRIVER: u8 = 5;
}

/// Why a binding did not decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingError {
    /// Ends inside an entry, bytes follow the last one, or over
    /// [`BINDING_MAX`].
    Length,
    /// Another format version.
    Version,
    /// More than [`MAX_CHANNELS`] entries.
    TooMany,
    /// An invalid or repeated channel name.
    Name,
    /// An unknown target, a bad key, or a source config `route_source`
    /// would refuse.
    Target,
}

/// A stream transformation the runtime applies before `on_message`
/// ([`crate::stream::adapt`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adapter {
    /// PCM microphone batches to RMS/peak levels per `block` samples.
    PcmToLevels {
        /// Samples per level block, `2..=MAX_LEVEL_BLOCK` (one batch of
        /// PCM then always fits one batch of levels).
        block: u16,
    },
    /// PCM microphone batches to G.711 µ-law.
    PcmToMuLaw,
    /// Every `factor`-th unit of any sample batch.
    Decimate {
        /// Keep one unit in `factor`, `2..`.
        factor: u16,
    },
    /// IMU batches to one channel: the magnitude of the acceleration.
    ImuToMagnitude,
}

impl Adapter {
    /// The adapter's code and parameter.
    pub const fn encode(&self) -> (u8, u16) {
        match *self {
            Adapter::PcmToLevels { block } => (1, block),
            Adapter::PcmToMuLaw => (2, 0),
            Adapter::Decimate { factor } => (3, factor),
            Adapter::ImuToMagnitude => (4, 0),
        }
    }

    /// Parses a code and parameter; `None` for an unknown code or a
    /// parameter out of range.
    pub const fn decode(code: u8, param: u16) -> Option<Self> {
        match (code, param) {
            (1, b) if b >= 2 && b <= route::MAX_LEVEL_BLOCK => {
                Some(Adapter::PcmToLevels { block: b })
            }
            (2, 0) => Some(Adapter::PcmToMuLaw),
            (3, f) if f >= 2 => Some(Adapter::Decimate { factor: f }),
            (4, 0) => Some(Adapter::ImuToMagnitude),
            _ => None,
        }
    }
}

/// Where an un-adapted channel binds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint<'a> {
    /// A source route on this node, started by the runtime on the
    /// channel's internal key ([`internal_key`]).
    Local(RouteSource),
    /// A key on this node's bus.
    Key(Key<'a>),
    /// A key, accepted only from `node`.
    Remote {
        /// The sending node.
        node: SenderId,
        /// The key.
        key: Key<'a>,
    },
    /// A control-module point of one of this node's drivers (ABI 8),
    /// carried on [`driver_key`].
    Driver(&'a str),
}

/// What a channel binds to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target<'a> {
    /// A source route on this node.
    Local(RouteSource),
    /// A key on this node's bus.
    Key(Key<'a>),
    /// A key accepted only from one node.
    Remote {
        /// The sending node.
        node: SenderId,
        /// The key.
        key: Key<'a>,
    },
    /// Another target, transformed by an adapter on delivery.
    Adapted {
        /// The stream before adaptation.
        inner: Endpoint<'a>,
        /// The transformation.
        adapter: Adapter,
    },
    /// A control-module point of one of this node's drivers (ABI 8).
    Driver(&'a str),
}

impl<'a> Target<'a> {
    /// The endpoint the data comes from and the adapter, if any.
    pub const fn parts(&self) -> (Endpoint<'a>, Option<Adapter>) {
        match *self {
            Target::Local(s) => (Endpoint::Local(s), None),
            Target::Key(k) => (Endpoint::Key(k), None),
            Target::Remote { node, key } => (Endpoint::Remote { node, key }, None),
            Target::Adapted { inner, adapter } => (inner, Some(adapter)),
            Target::Driver(p) => (Endpoint::Driver(p), None),
        }
    }
}

/// The key a [`Target::Local`] channel's route publishes on:
/// `ch/<name>` (at most 3 + [`MAX_NAME`](crate::wasm_abi::channels::MAX_NAME)
/// bytes, written into `buf`).
pub fn internal_key<'b>(name: &str, buf: &'b mut [u8; INTERNAL_KEY_MAX]) -> Key<'b> {
    let n = name.len().min(INTERNAL_KEY_MAX - 3);
    buf[..3].copy_from_slice(b"ch/");
    buf[3..3 + n].copy_from_slice(&name.as_bytes()[..n]);
    // A valid channel name is ASCII without '/', so the key is valid.
    let s = core::str::from_utf8(&buf[..3 + n]).unwrap_or("ch/_");
    Key::new(s).unwrap_or(Key::from_static("ch/_"))
}

/// Whether `key` is a node-internal channel key: `ch/...`, what a local
/// source bound to a channel publishes on, or `drv/...`, a driver point
/// ([`driver_key`]). A node delivers publications on these keys to its own
/// payload (or its drivers) and sends them on no link.
pub fn is_internal_key(key: Key<'_>) -> bool {
    matches!(key.segments().next(), Some("ch" | "drv"))
}

/// Longest [`driver_key`].
pub const DRIVER_KEY_MAX: usize = 4 + crate::wasm_abi::channels::MAX_NAME;

/// The node-internal key a [`Target::Driver`] point is carried on:
/// `drv/<point>` (written into `buf`).
pub fn driver_key<'b>(point: &str, buf: &'b mut [u8; DRIVER_KEY_MAX]) -> Key<'b> {
    let n = point.len().min(DRIVER_KEY_MAX - 4);
    buf[..4].copy_from_slice(b"drv/");
    buf[4..4 + n].copy_from_slice(&point.as_bytes()[..n]);
    let s = core::str::from_utf8(&buf[..4 + n]).unwrap_or("drv/_");
    Key::new(s).unwrap_or(Key::from_static("drv/_"))
}

/// The point a `drv/<point>` key names, if it is one.
pub fn driver_point(key: Key<'_>) -> Option<&str> {
    key.as_str()
        .strip_prefix("drv/")
        .filter(|p| valid_name(p.as_bytes()))
}

/// Longest [`internal_key`].
pub const INTERNAL_KEY_MAX: usize = 3 + crate::wasm_abi::channels::MAX_NAME;

/// What a device remembers of its current binding to decide the next one:
/// the image it is for and its version (a binding for the same image must
/// have a higher version, [`super::trust::TrustState::accept_binding`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindingMark {
    /// SHA-256 of the image.
    pub image_sha256: [u8; 32],
    /// The binding's version.
    pub version: Version,
}

/// A decoded binding record, borrowing names and keys from its bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding<'a> {
    /// SHA-256 of the image the binding is for.
    pub image_sha256: [u8; 32],
    /// Binding version, increasing per image.
    pub version: Version,
    entries: [Option<(&'a str, Target<'a>)>; MAX_CHANNELS],
    n: usize,
}

fn key_of(bytes: &[u8]) -> Result<Key<'_>, BindingError> {
    let s = core::str::from_utf8(bytes).map_err(|_| BindingError::Target)?;
    let k = Key::new(s).map_err(|_| BindingError::Target)?;
    if !bindable_key(k) {
        return Err(BindingError::Target);
    }
    Ok(k)
}

/// Whether a key or remote target may name `k`: not a system key, and not
/// a node-internal one (`ch/`, `drv/`). Only a local or driver target
/// reaches those, so a driver point is only ever bound by name, with its
/// kind and direction checked against the channel's.
fn bindable_key(k: Key<'_>) -> bool {
    !super::is_system_key(k) && !is_internal_key(k)
}

fn endpoint(code: u8, body: &[u8]) -> Result<Endpoint<'_>, BindingError> {
    let bad = BindingError::Target;
    match code {
        target::LOCAL => {
            let (&kind, cfg) = body.split_first().ok_or(bad)?;
            let kind = SourceKind::from_i32(i32::from(kind)).map_err(|_| bad)?;
            Ok(Endpoint::Local(
                route::parse_route_source(kind, cfg).map_err(|_| bad)?,
            ))
        }
        target::KEY => Ok(Endpoint::Key(key_of(body)?)),
        target::DRIVER => {
            let p = core::str::from_utf8(body).map_err(|_| BindingError::Target)?;
            if !valid_name(p.as_bytes()) {
                return Err(BindingError::Target);
            }
            Ok(Endpoint::Driver(p))
        }
        target::REMOTE => {
            let (node, key) = body.split_at_checked(8).ok_or(bad)?;
            let mut id = [0u8; 8];
            id.copy_from_slice(node);
            Ok(Endpoint::Remote {
                node: SenderId(id),
                key: key_of(key)?,
            })
        }
        _ => Err(bad),
    }
}

impl<'a> Binding<'a> {
    /// An empty binding for `image_sha256` at `version`.
    pub fn new(image_sha256: [u8; 32], version: Version) -> Self {
        Binding {
            image_sha256,
            version,
            entries: [None; MAX_CHANNELS],
            n: 0,
        }
    }

    /// Adds (or replaces) the target of `name`; `Err` for an invalid name
    /// or a full binding.
    pub fn bind(&mut self, name: &'a str, target: Target<'a>) -> Result<(), BindingError> {
        if !valid_name(name.as_bytes()) {
            return Err(BindingError::Name);
        }
        let (endpoint, _) = target.parts();
        match (target, endpoint) {
            (Target::Driver(p), _) if !valid_name(p.as_bytes()) => return Err(BindingError::Target),
            (Target::Adapted { .. }, Endpoint::Driver(_)) => return Err(BindingError::Target),
            (_, Endpoint::Key(k) | Endpoint::Remote { key: k, .. }) if !bindable_key(k) => {
                return Err(BindingError::Target);
            }
            _ => {}
        }
        if let Some(e) = self.entries[..self.n]
            .iter_mut()
            .flatten()
            .find(|(n, _)| *n == name)
        {
            e.1 = target;
            return Ok(());
        }
        if self.n == MAX_CHANNELS {
            return Err(BindingError::TooMany);
        }
        self.entries[self.n] = Some((name, target));
        self.n += 1;
        Ok(())
    }

    /// The binding's image and version.
    pub const fn mark(&self) -> BindingMark {
        BindingMark {
            image_sha256: self.image_sha256,
            version: self.version,
        }
    }

    /// The entries in record order.
    pub fn entries(&self) -> impl Iterator<Item = (&'a str, Target<'a>)> + '_ {
        self.entries[..self.n].iter().flatten().copied()
    }

    /// The target of `name`.
    pub fn target(&self, name: &str) -> Option<Target<'a>> {
        self.entries().find(|(n, _)| *n == name).map(|(_, t)| t)
    }

    /// Decodes a record that must end exactly at the end of `bytes`.
    pub fn decode(bytes: &'a [u8]) -> Result<Self, BindingError> {
        match Self::decode_prefix(bytes)? {
            (b, []) => Ok(b),
            _ => Err(BindingError::Length),
        }
    }

    /// Decodes a record at the start of `bytes`; returns it and the bytes
    /// after it (a `sys/payload/bind` body's signature trailer).
    pub fn decode_prefix(bytes: &'a [u8]) -> Result<(Self, &'a [u8]), BindingError> {
        let len = BindingError::Length;
        let (head, mut rest) = bytes.split_at_checked(HEADER_LEN).ok_or(len)?;
        if head[0] != VERSION {
            return Err(BindingError::Version);
        }
        let mut sha = [0u8; 32];
        sha.copy_from_slice(&head[1..33]);
        let version = Version(u32::from_le_bytes([head[33], head[34], head[35], head[36]]));
        let n = usize::from(head[37]);
        if n > MAX_CHANNELS {
            return Err(BindingError::TooMany);
        }
        let mut b = Binding::new(sha, version);
        for _ in 0..n {
            let (&nl, tail) = rest.split_first().ok_or(len)?;
            let (name, tail) = tail.split_at_checked(usize::from(nl)).ok_or(len)?;
            let (&[code, bl], tail) = tail.split_first_chunk::<2>().ok_or(len)?;
            let (body, tail) = tail.split_at_checked(usize::from(bl)).ok_or(len)?;
            rest = tail;
            let name = core::str::from_utf8(name).map_err(|_| BindingError::Name)?;
            if !valid_name(name.as_bytes()) || b.target(name).is_some() {
                return Err(BindingError::Name);
            }
            let t = match code {
                target::ADAPTED => {
                    let (&[a, p0, p1, inner], ib) =
                        body.split_first_chunk::<4>().ok_or(BindingError::Target)?;
                    let adapter = Adapter::decode(a, u16::from_le_bytes([p0, p1]))
                        .ok_or(BindingError::Target)?;
                    let inner = endpoint(inner, ib)?;
                    // Adapters take sample streams; a driver point is none.
                    if matches!(inner, Endpoint::Driver(_)) {
                        return Err(BindingError::Target);
                    }
                    Target::Adapted { inner, adapter }
                }
                c => match endpoint(c, body)? {
                    Endpoint::Local(s) => Target::Local(s),
                    Endpoint::Key(k) => Target::Key(k),
                    Endpoint::Remote { node, key } => Target::Remote { node, key },
                    Endpoint::Driver(p) => Target::Driver(p),
                },
            };
            b.bind(name, t)?;
        }
        if bytes.len() - rest.len() > BINDING_MAX {
            return Err(len);
        }
        Ok((b, rest))
    }

    /// Writes the record into `out`; returns its length, `None` if it
    /// does not fit `out` or [`BINDING_MAX`].
    pub fn encode(&self, out: &mut [u8]) -> Option<usize> {
        let mut w = Writer { out, at: 0 };
        w.put(&[VERSION])?;
        w.put(&self.image_sha256)?;
        w.put(&self.version.0.to_le_bytes())?;
        w.put(&[self.n as u8])?;
        for (name, t) in self.entries() {
            w.put(&[name.len() as u8])?;
            w.put(name.as_bytes())?;
            let mut body = [0u8; 4 + 1 + 8 + 255];
            let (code, n) = match t {
                Target::Adapted { inner, adapter } => {
                    let (a, p) = adapter.encode();
                    let p = p.to_le_bytes();
                    let (ic, n) = endpoint_body(&inner, &mut body[4..])?;
                    body[..4].copy_from_slice(&[a, p[0], p[1], ic]);
                    (target::ADAPTED, n + 4)
                }
                other => endpoint_body(&other.parts().0, &mut body)?,
            };
            w.put(&[code, u8::try_from(n).ok()?])?;
            w.put(&body[..n])?;
        }
        Some(w.at)
    }
}

fn endpoint_body(e: &Endpoint<'_>, out: &mut [u8]) -> Option<(u8, usize)> {
    let mut put = |at: usize, b: &[u8]| -> Option<usize> {
        out.get_mut(at..at + b.len())?.copy_from_slice(b);
        Some(at + b.len())
    };
    match e {
        Endpoint::Local(s) => {
            let (kind, cfg, n) = match s {
                RouteSource::Samples(ss) => {
                    let c = ss.encode();
                    let mut buf = [0u8; route::CAMERA_CFG_LEN];
                    buf[..c.len()].copy_from_slice(&c);
                    (ss.kind(), buf, c.len())
                }
                RouteSource::Frames(src, cfg) => {
                    let (buf, n) = src.encode(cfg);
                    (src.kind(), buf, n)
                }
            };
            let at = put(0, &[kind as u8])?;
            Some((target::LOCAL, put(at, &cfg[..n])?))
        }
        Endpoint::Key(k) => Some((target::KEY, put(0, k.as_bytes())?)),
        Endpoint::Remote { node, key } => {
            let at = put(0, &node.0)?;
            Some((target::REMOTE, put(at, key.as_bytes())?))
        }
        Endpoint::Driver(p) => Some((target::DRIVER, put(0, p.as_bytes())?)),
    }
}

struct Writer<'o> {
    out: &'o mut [u8],
    at: usize,
}

impl Writer<'_> {
    fn put(&mut self, bytes: &[u8]) -> Option<()> {
        let end = self.at.checked_add(bytes.len()).filter(|e| *e <= BINDING_MAX)?;
        self.out.get_mut(self.at..end)?.copy_from_slice(bytes);
        self.at = end;
        Some(())
    }
}

/// The signature trailer of a `sys/payload/bind` body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignedBinding {
    /// Must equal the binding's version.
    pub version: Version,
    /// Over [`binding_message`].
    pub signature: Signature,
}

impl SignedBinding {
    /// The trailer bytes.
    pub fn encode(&self) -> [u8; SIGNED_BINDING_LEN] {
        let mut out = [0u8; SIGNED_BINDING_LEN];
        out[..4].copy_from_slice(&self.version.0.to_le_bytes());
        out[4..].copy_from_slice(&self.signature.0);
        out
    }
}

/// A parsed `sys/payload/bind` body: the record's bytes, the record, and
/// the signature if one follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindRequest<'a> {
    /// The encoded record (what is stored and hashed).
    pub record: &'a [u8],
    /// The record.
    pub binding: Binding<'a>,
    /// The signature trailer.
    pub auth: Option<SignedBinding>,
}

impl<'a> BindRequest<'a> {
    /// Parses a body: a record, then nothing or a trailer whose version
    /// is the record's. Anything else is `None`.
    pub fn parse(body: &'a [u8]) -> Option<Self> {
        let (binding, rest) = Binding::decode_prefix(body).ok()?;
        let record = &body[..body.len() - rest.len()];
        let auth = match rest.len() {
            0 => None,
            SIGNED_BINDING_LEN => {
                let version = Version(u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]));
                if version != binding.version {
                    return None;
                }
                let mut sig = [0u8; SIGNATURE_LEN];
                sig.copy_from_slice(&rest[4..]);
                Some(SignedBinding {
                    version,
                    signature: Signature(sig),
                })
            }
            _ => return None,
        };
        Some(BindRequest {
            record,
            binding,
            auth,
        })
    }
}

/// The message a binding signature covers: [`MESSAGE_TAG`], the target
/// device's node id (as `sys/info` reports it), the image's SHA-256, the
/// binding version (LE) and the SHA-256 of the record.
pub fn binding_message(
    node: SenderId,
    image_sha256: &[u8; 32],
    version: Version,
    record_sha256: &[u8; 32],
) -> [u8; MESSAGE_LEN] {
    let mut m = [0u8; MESSAGE_LEN];
    let t = MESSAGE_TAG.len();
    m[..t].copy_from_slice(MESSAGE_TAG);
    m[t..t + 8].copy_from_slice(&node.0);
    m[t + 8..t + 40].copy_from_slice(image_sha256);
    m[t + 40..t + 44].copy_from_slice(&version.0.to_le_bytes());
    m[t + 44..].copy_from_slice(record_sha256);
    m
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::wasm_abi::route::{AudioMode, MicConfig, SampleSource};

    pub(crate) fn mic() -> RouteSource {
        RouteSource::Samples(SampleSource::Microphone(
            MicConfig::new(16_000, 20, AudioMode::Levels, 10, 160).unwrap(),
        ))
    }

    pub(crate) fn sample() -> Binding<'static> {
        let mut b = Binding::new([7; 32], Version(3));
        b.bind("audio", Target::Local(mic())).unwrap();
        b.bind("cmd", Target::Key(Key::from_static("lab/cmd"))).unwrap();
        b.bind(
            "peer",
            Target::Remote {
                node: SenderId([1, 2, 3, 4, 5, 6, 0, 0]),
                key: Key::from_static("mic/level"),
            },
        )
        .unwrap();
        b.bind(
            "pcm",
            Target::Adapted {
                inner: Endpoint::Key(Key::from_static("mic/pcm")),
                adapter: Adapter::PcmToLevels { block: 160 },
            },
        )
        .unwrap();
        b
    }

    fn encoded(b: &Binding<'_>) -> Vec<u8> {
        let mut out = [0u8; BINDING_MAX];
        let n = b.encode(&mut out).unwrap();
        out[..n].to_vec()
    }

    #[test]
    fn bd1_every_target_round_trips() {
        let b = sample();
        let bytes = encoded(&b);
        assert_eq!(Binding::decode(&bytes), Ok(b));
        assert_eq!(b.entries().count(), 4);
        assert_eq!(b.target("cmd"), Some(Target::Key(Key::from_static("lab/cmd"))));
        let (e, a) = b.target("pcm").unwrap().parts();
        assert_eq!(e, Endpoint::Key(Key::from_static("mic/pcm")));
        assert_eq!(a, Some(Adapter::PcmToLevels { block: 160 }));
        // Rebinding a name replaces its target.
        let mut c = b;
        c.bind("cmd", Target::Key(Key::from_static("x"))).unwrap();
        assert_eq!(c.entries().count(), 4);
        for a in [
            Adapter::PcmToLevels { block: 2 },
            Adapter::PcmToMuLaw,
            Adapter::Decimate { factor: 4 },
            Adapter::ImuToMagnitude,
        ] {
            let (code, p) = a.encode();
            assert_eq!(Adapter::decode(code, p), Some(a));
        }
        assert_eq!(Adapter::decode(3, 1), None);
        assert_eq!(Adapter::decode(1, 0), None);
        assert_eq!(Adapter::decode(1, 1), None);
        assert_eq!(Adapter::decode(9, 0), None);
    }

    #[test]
    fn bd1_malformed_records_are_refused() {
        let bytes = encoded(&sample());
        for cut in 0..bytes.len() {
            assert!(Binding::decode(&bytes[..cut]).is_err(), "cut {cut}");
        }
        let mut v = bytes.clone();
        v.push(0);
        assert_eq!(Binding::decode(&v), Err(BindingError::Length));
        let mut v = bytes.clone();
        v[0] = 2;
        assert_eq!(Binding::decode(&v), Err(BindingError::Version));
        let mut v = bytes.clone();
        v[HEADER_LEN - 1] = MAX_CHANNELS as u8 + 1;
        assert_eq!(Binding::decode(&v), Err(BindingError::TooMany));
        // "audio" twice, then a bad name.
        let mut dup = Binding::new([0; 32], Version(1));
        dup.bind("a", Target::Key(Key::from_static("k"))).unwrap();
        let mut v = encoded(&dup);
        let entry = v[HEADER_LEN..].to_vec();
        v.extend_from_slice(&entry);
        v[HEADER_LEN - 1] = 2;
        assert_eq!(Binding::decode(&v), Err(BindingError::Name));
        let mut v = encoded(&dup);
        v[HEADER_LEN + 1] = b'A';
        assert_eq!(Binding::decode(&v), Err(BindingError::Name));
        // A system key, an unknown target, a config route_source refuses.
        let entry = |code: u8, body: &[u8]| {
            let mut v = encoded(&Binding::new([0; 32], Version(1)));
            v[HEADER_LEN - 1] = 1;
            v.extend_from_slice(&[1, b'a', code, body.len() as u8]);
            v.extend_from_slice(body);
            Binding::decode(&v).map(drop)
        };
        assert_eq!(entry(target::KEY, b"sys/info"), Err(BindingError::Target));
        assert_eq!(entry(9, b"k"), Err(BindingError::Target));
        let mut cfg = vec![SourceKind::Microphone as u8];
        cfg.extend_from_slice(&[0x40, 0x1f, 99, 0, 10, 0, 160, 0]); // gain 99
        assert_eq!(entry(target::LOCAL, &cfg), Err(BindingError::Target));
        assert_eq!(entry(target::KEY, b"ok/key"), Ok(()));
        assert_eq!(entry(target::REMOTE, b"short"), Err(BindingError::Target));
        assert_eq!(
            entry(target::ADAPTED, &[3, 1, 0, target::KEY, b'k']),
            Err(BindingError::Target)
        );
        assert_eq!(entry(target::ADAPTED, &[3, 2, 0, target::KEY, b'k']), Ok(()));
        // Nested adaptation is refused.
        assert_eq!(
            entry(target::ADAPTED, &[3, 2, 0, target::ADAPTED, 3, 2, 0]),
            Err(BindingError::Target)
        );
    }

    #[test]
    fn bd2_bind_bodies_carry_an_optional_signature_of_the_same_version() {
        let bytes = encoded(&sample());
        let r = BindRequest::parse(&bytes).unwrap();
        assert_eq!((r.record, r.auth), (&bytes[..], None));
        let auth = SignedBinding {
            version: Version(3),
            signature: Signature([9; 64]),
        };
        let mut body = bytes.clone();
        body.extend_from_slice(&auth.encode());
        let r = BindRequest::parse(&body).unwrap();
        assert_eq!((r.record, r.auth), (&bytes[..], Some(auth)));
        let mut other = bytes.clone();
        other.extend_from_slice(&SignedBinding { version: Version(4), ..auth }.encode());
        assert_eq!(BindRequest::parse(&other), None);
        let mut short = bytes.clone();
        short.extend_from_slice(&[0; 10]);
        assert_eq!(BindRequest::parse(&short), None);
        let m = binding_message(SenderId([9; 8]), &[1; 32], Version(0x0403_0201), &[2; 32]);
        assert_eq!(&m[..14], MESSAGE_TAG);
        assert_eq!(&m[14..22], &[9; 8]);
        assert_eq!(&m[22..54], &[1; 32]);
        assert_eq!(&m[54..58], &[1, 2, 3, 4]);
        assert_eq!(&m[58..], &[2; 32]);
    }

    #[test]
    fn bd1_internal_keys_name_the_channel() {
        let mut buf = [0u8; INTERNAL_KEY_MAX];
        let k = internal_key("audio", &mut buf);
        assert_eq!(k.as_str(), "ch/audio");
        assert!(is_internal_key(k));
        assert!(!is_internal_key(Key::from_static("chat/x")));
        assert!(!is_internal_key(Key::from_static("mic/ch")));
    }

    #[test]
    fn bd8_driver_targets_round_trip_on_their_internal_key() {
        let mut b = Binding::new([7; 32], Version(3));
        b.bind("x_pos", Target::Driver("x_pos")).unwrap();
        b.bind("segments", Target::Driver("motion_queue")).unwrap();
        let mut out = [0u8; BINDING_MAX];
        let n = b.encode(&mut out).unwrap();
        let back = Binding::decode(&out[..n]).unwrap();
        assert_eq!(back.target("x_pos"), Some(Target::Driver("x_pos")));
        assert_eq!(back.target("segments"), Some(Target::Driver("motion_queue")));
        // The entry on the wire: target 5, the point's name.
        let at = HEADER_LEN + 1 + 5;
        assert_eq!(&out[at..at + 2 + 5], &[target::DRIVER, 5, b'x', b'_', b'p', b'o', b's']);

        let mut buf = [0u8; DRIVER_KEY_MAX];
        let k = driver_key("x_pos", &mut buf);
        assert_eq!(k.as_str(), "drv/x_pos");
        assert!(is_internal_key(k), "no link carries a driver point");
        assert_eq!(driver_point(k), Some("x_pos"));
        assert_eq!(driver_point(Key::from_static("drv/X")), None);
        assert_eq!(driver_point(Key::from_static("app/x_pos")), None);
        assert!(!is_internal_key(Key::from_static("drive/x")));
    }

    #[test]
    fn bd8_a_key_target_cannot_name_a_driver_or_channel_key() {
        // Only Target::Driver reaches a point, so its kind and direction
        // are always checked; a key spelled drv/... would skip that.
        let mut b = Binding::new([7; 32], Version(3));
        for k in ["drv/spindle_rpm", "ch/audio", "sys/log"] {
            let key = Key::new(k).unwrap();
            assert_eq!(b.bind("a", Target::Key(key)), Err(BindingError::Target), "{k}");
            let remote = Target::Remote { node: SenderId([1; 8]), key };
            assert_eq!(b.bind("a", remote), Err(BindingError::Target), "{k}");
        }
        // And on the wire.
        let mut ok = Binding::new([7; 32], Version(3));
        ok.bind("a", Target::Key(Key::from_static("drx/pin"))).unwrap();
        let mut out = [0u8; BINDING_MAX];
        let n = ok.encode(&mut out).unwrap();
        let mut bad = out[..n].to_vec();
        let at = bad.len() - 7;
        bad[at..at + 3].copy_from_slice(b"drv");
        assert_eq!(Binding::decode(&bad).err(), Some(BindingError::Target));
    }

    #[test]
    fn bd8_bad_driver_targets_are_refused() {
        let mut b = Binding::new([7; 32], Version(3));
        assert_eq!(b.bind("a", Target::Driver("X-Pos")), Err(BindingError::Target));
        assert_eq!(b.bind("a", Target::Driver("")), Err(BindingError::Target));
        let adapted = Target::Adapted { inner: Endpoint::Driver("x"), adapter: Adapter::PcmToMuLaw };
        assert_eq!(b.bind("a", adapted), Err(BindingError::Target));
        // The same refused on the wire.
        let mut ok = Binding::new([7; 32], Version(3));
        ok.bind("a", Target::Driver("x")).unwrap();
        let mut out = [0u8; BINDING_MAX];
        let n = ok.encode(&mut out).unwrap();
        let mut bad = out[..n].to_vec();
        *bad.last_mut().unwrap() = b'X';
        assert_eq!(Binding::decode(&bad).err(), Some(BindingError::Target));
        let mut wrapped = out[..HEADER_LEN + 2].to_vec();
        wrapped.extend_from_slice(&[target::ADAPTED, 5, 2, 0, 0, target::DRIVER, b'x']);
        assert_eq!(Binding::decode(&wrapped).err(), Some(BindingError::Target));
    }
}
